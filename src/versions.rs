use std::{
    fmt,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
};

use ahash::{AHashMap, AHashSet};

use crate::{
    registry::{IndexEntry, LookupError, Registry, RegistryOptions},
    semver::{PartialVersion, Requirement, Unpinnable},
};

/// A dependency requirement found in a manifest, with the crate name already
/// resolved through a `package = "…"` rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepRequest<'a> {
    pub name: &'a str,
    pub req: &'a str,
}

/// Why a dependency was left as it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    WorkspaceInherited,
    PathSource,
    GitSource,
    AlternateRegistry,
    PatchSection,
    ReplaceSection,
    Frozen,
    AlreadyComplete,
    Pinned,
    Unpinnable(Unpinnable),
    UnknownCrate,
    InvalidName,
    NoMatchingRelease,
    AllYanked,
    AllPrerelease,
    BelowRustVersion(PartialVersion),
    Offline,
    SourceReplaced(String),
    LookupFailed,
}

impl SkipReason {
    /// Whether the skip means the run could not do what it was asked. Those are
    /// warned about; the rest are ordinary and reported only on request.
    pub fn is_notable(&self) -> bool {
        matches!(
            self,
            Self::Unpinnable(_)
                | Self::UnknownCrate
                | Self::InvalidName
                | Self::NoMatchingRelease
                | Self::AllYanked
                | Self::AllPrerelease
                | Self::BelowRustVersion(_)
                | Self::Offline
                | Self::SourceReplaced(_)
        )
    }

    /// The word this skip is counted under in the run summary.
    pub fn label(&self) -> &'static str {
        match self {
            Self::WorkspaceInherited => "workspace-inherited",
            Self::PathSource => "path",
            Self::GitSource => "git",
            Self::AlternateRegistry => "other registry",
            Self::PatchSection => "patch",
            Self::ReplaceSection => "replace",
            Self::Frozen => "frozen",
            Self::AlreadyComplete => "already complete",
            Self::Pinned => "pinned",
            Self::Unpinnable(_) => "not completable",
            Self::UnknownCrate => "unknown crate",
            Self::InvalidName => "illegal name",
            Self::NoMatchingRelease => "no matching release",
            Self::AllYanked => "yanked",
            Self::AllPrerelease => "pre-release only",
            Self::BelowRustVersion(_) => "rust-version",
            Self::Offline => "offline",
            Self::SourceReplaced(_) => "source replaced",
            Self::LookupFailed => "lookup failed",
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceInherited => f.write_str("the version is inherited from the workspace"),
            Self::PathSource => f.write_str("a path dependency does not come from a registry"),
            Self::GitSource => f.write_str("a git dependency does not come from a registry"),
            Self::AlternateRegistry => f.write_str("the dependency names another registry"),
            Self::PatchSection => {
                f.write_str("a [patch] entry constrains the source it patches, not crates.io")
            }
            Self::ReplaceSection => f.write_str("[replace] is deprecated and is left alone"),
            Self::Frozen => f.write_str("the entry is inside a `# fmt: off` region"),
            Self::AlreadyComplete => f.write_str("the requirement already names x.y.z"),
            Self::Pinned => f.write_str("an `=` requirement is pinned; pass --upgrade-pinned"),
            Self::Unpinnable(why) => write!(f, "{why}"),
            Self::UnknownCrate => f.write_str("no such crate on the registry"),
            Self::InvalidName => f.write_str("not a legal crate name"),
            Self::NoMatchingRelease => f.write_str("no release satisfies the requirement"),
            Self::AllYanked => {
                f.write_str("every matching release is yanked; pass --allow-yanked to use one")
            }
            Self::AllPrerelease => f.write_str("every matching release is a pre-release"),
            Self::BelowRustVersion(msrv) => write!(
                f,
                "every matching release needs a newer compiler than rust-version {msrv}"
            ),
            Self::Offline => f.write_str("offline and not in the local registry index"),
            Self::SourceReplaced(source) => {
                write!(f, "crates.io is replaced by source `{source}`")
            }
            Self::LookupFailed => f.write_str("the registry lookup failed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Pinned(String),
    Unchanged,
    Skipped(SkipReason),
}

/// What one dependency's requirement did during a run, for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRecord {
    pub crate_name: String,
    pub section: String,
    pub requirement: String,
    /// 1-based position of the dependency key, when the layout did not move it
    /// out from under the parse the position was read from.
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub outcome: Resolution,
}

pub trait VersionLookup: Sync {
    fn resolve(&self, request: DepRequest<'_>, rust_version: Option<PartialVersion>) -> Resolution;

    /// Offered every request in the document before formatting starts, so
    /// implementations backed by network I/O can resolve them concurrently.
    fn prewarm(&self, _requests: &[DepRequest<'_>]) {}

    fn take_error(&self) -> Option<(String, String)> {
        None
    }
}

/// cargo-edit's vocabulary: an upgrade is compatible unless told otherwise, and
/// an `=` requirement is left alone unless named.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpgradePolicy {
    pub enabled: bool,
    pub incompatible: bool,
    pub pinned: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LookupPolicy {
    pub upgrade: UpgradePolicy,
    pub allow_yanked: bool,
    pub ignore_rust_version: bool,
}

type Entries = Result<Arc<[IndexEntry]>, Arc<LookupError>>;
type EntryCells = AHashMap<Box<str>, Arc<OnceLock<Entries>>>;

pub struct RegistryLookup {
    registry: Registry,
    policy: LookupPolicy,
    concurrency: usize,
    cells: Mutex<EntryCells>,
    error: Mutex<Option<(String, String)>>,
    warnings: Vec<String>,
}

impl RegistryLookup {
    pub fn new(
        options: RegistryOptions,
        policy: LookupPolicy,
        concurrency: usize,
    ) -> Result<Self, LookupError> {
        let warnings = options.warnings.clone();
        Ok(Self {
            registry: Registry::new(options)?,
            policy,
            concurrency: concurrency.max(1),
            cells: Mutex::new(EntryCells::default()),
            error: Mutex::new(None),
            warnings,
        })
    }

    /// Settings this run could not honour, reported once rather than per crate.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn record_error(&self, crate_name: &str, details: String) {
        let mut error = lock(&self.error);
        if error.is_none() {
            *error = Some((crate_name.to_owned(), details));
        }
    }

    fn entries(&self, crate_name: &str) -> Entries {
        let cell = lock(&self.cells)
            .entry(crate_name.into())
            .or_insert_with(|| Arc::new(OnceLock::new()))
            .clone();
        cell.get_or_init(|| {
            self.registry
                .lookup(crate_name)
                .map(Arc::from)
                .map_err(Arc::new)
        })
        .clone()
    }

    /// Whether this run would rewrite a requirement of this shape at all, which
    /// is what decides if the crate is worth a request.
    fn wants(&self, req: &Requirement) -> bool {
        if !req.is_complete() {
            return true;
        }
        self.policy.upgrade.enabled && (!req.is_pinned() || self.policy.upgrade.pinned)
    }

    fn select(
        &self,
        req: &Requirement,
        entries: &[IndexEntry],
        rust_version: Option<PartialVersion>,
    ) -> Resolution {
        let incompatible = self.policy.upgrade.enabled && self.policy.upgrade.incompatible;
        let considered: Vec<&IndexEntry> = entries
            .iter()
            .filter(|entry| {
                if incompatible {
                    entry.version.pre.is_empty()
                } else {
                    req.matches(&entry.version)
                }
            })
            .collect();
        if considered.is_empty() {
            let all_prerelease =
                !entries.is_empty() && entries.iter().all(|e| !e.version.pre.is_empty());
            return Resolution::Skipped(if all_prerelease {
                SkipReason::AllPrerelease
            } else {
                SkipReason::NoMatchingRelease
            });
        }

        let released: Vec<&IndexEntry> = if self.policy.allow_yanked {
            considered
        } else {
            considered.into_iter().filter(|e| !e.yanked).collect()
        };
        if released.is_empty() {
            return Resolution::Skipped(SkipReason::AllYanked);
        }

        let msrv = rust_version.filter(|_| !self.policy.ignore_rust_version);
        let usable: Vec<&IndexEntry> = match msrv {
            Some(msrv) => released
                .into_iter()
                .filter(|e| e.rust_version.is_none_or(|needs| needs <= msrv))
                .collect(),
            None => released,
        };
        // `released` was not empty, so only the rust-version filter can have
        // emptied `usable`, which means `msrv` is set.
        let Some(best) = usable.iter().map(|entry| &entry.version).max() else {
            return Resolution::Skipped(SkipReason::BelowRustVersion(
                msrv.unwrap_or(PartialVersion::ZERO),
            ));
        };
        Resolution::Pinned(req.render(best))
    }

    fn classify(&self, crate_name: &str, err: &LookupError) -> Resolution {
        if err.is_fatal() {
            self.record_error(crate_name, err.to_string());
            return Resolution::Skipped(SkipReason::LookupFailed);
        }
        Resolution::Skipped(match err {
            LookupError::NotFound => SkipReason::UnknownCrate,
            LookupError::InvalidName => SkipReason::InvalidName,
            LookupError::Offline => SkipReason::Offline,
            LookupError::SourceReplaced(source) => SkipReason::SourceReplaced(source.clone()),
            _ => SkipReason::LookupFailed,
        })
    }
}

impl VersionLookup for RegistryLookup {
    fn resolve(&self, request: DepRequest<'_>, rust_version: Option<PartialVersion>) -> Resolution {
        let req = match Requirement::parse(request.req) {
            Ok(req) => req,
            Err(why) => return Resolution::Skipped(SkipReason::Unpinnable(why)),
        };
        if !self.wants(&req) {
            return Resolution::Skipped(if req.is_complete() && req.is_pinned() {
                SkipReason::Pinned
            } else {
                SkipReason::AlreadyComplete
            });
        }

        let entries = match self.entries(request.name) {
            Ok(entries) => entries,
            Err(err) => return self.classify(request.name, &err),
        };

        match self.select(&req, &entries, rust_version) {
            Resolution::Pinned(rendered) if rendered == request.req.trim() => Resolution::Unchanged,
            other => other,
        }
    }

    /// One request per crate, run on a pool sized by `-j` so a large workspace
    /// is not serialised behind the registry.
    fn prewarm(&self, requests: &[DepRequest<'_>]) {
        let pending = self.pending(requests);
        if pending.len() < 2 {
            for name in &pending {
                self.warm_one(name);
            }
            return;
        }

        let next = AtomicUsize::new(0);
        let workers = pending.len().min(self.concurrency);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(name) = pending.get(index) else {
                            return;
                        };
                        self.warm_one(name);
                    }
                });
            }
        });
    }

    fn take_error(&self) -> Option<(String, String)> {
        lock(&self.error).take()
    }
}

impl RegistryLookup {
    /// Fetch one crate, recording a failure of this tool so that the caller can
    /// stop the run before any file is written.
    fn warm_one(&self, crate_name: &str) {
        if let Err(err) = self.entries(crate_name)
            && err.is_fatal()
        {
            self.record_error(crate_name, err.to_string());
        }
    }

    /// Crate names this run would act on that have no entry list yet.
    fn pending<'a>(&self, requests: &[DepRequest<'a>]) -> Vec<&'a str> {
        let cells = lock(&self.cells);
        let mut seen = AHashSet::with_capacity(requests.len());
        let mut pending = Vec::new();
        for request in requests {
            if !Requirement::parse(request.req).is_ok_and(|req| self.wants(&req)) {
                continue;
            }
            if !seen.insert(request.name) {
                continue;
            }
            if cells
                .get(request.name)
                .and_then(|cell| cell.get())
                .is_some()
            {
                continue;
            }
            pending.push(request.name);
        }
        pending
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::*;
    use crate::registry::{RegistryCli, RegistrySource};

    fn entry(version: &str, yanked: bool, rust_version: Option<&str>) -> IndexEntry {
        IndexEntry {
            version: Version::parse(version).unwrap(),
            yanked,
            rust_version: rust_version.and_then(PartialVersion::parse),
        }
    }

    fn make_lookup(policy: LookupPolicy) -> RegistryLookup {
        let map: std::collections::HashMap<String, std::ffi::OsString> =
            std::collections::HashMap::new();
        let env = |key: &str| map.get(key).cloned();
        let options = RegistryOptions::resolve(
            std::path::Path::new("/nonexistent"),
            &env,
            &RegistryCli {
                registry_url: Some("http://127.0.0.1:1".to_owned()),
                offline: true,
                concurrency: 1,
            },
        );
        RegistryLookup::new(options, policy, 1).unwrap()
    }

    fn select(policy: LookupPolicy, current: &str, entries: &[IndexEntry]) -> Resolution {
        let lookup = make_lookup(policy);
        let req = Requirement::parse(current).unwrap();
        lookup.select(&req, entries, None)
    }

    #[test]
    fn the_highest_matching_release_wins_whatever_the_publish_order() {
        let entries = [
            entry("1.10.0", false, None),
            entry("1.2.0", false, None),
            entry("1.9.0", false, None),
        ];
        assert_eq!(
            select(LookupPolicy::default(), "1", &entries),
            Resolution::Pinned("1.10.0".to_owned())
        );
    }

    #[test]
    fn yanked_releases_are_reported_rather_than_ignored() {
        let entries = [entry("1.0.0", true, None), entry("1.1.0", true, None)];
        assert_eq!(
            select(LookupPolicy::default(), "1", &entries),
            Resolution::Skipped(SkipReason::AllYanked)
        );

        let allow = LookupPolicy {
            allow_yanked: true,
            ..LookupPolicy::default()
        };
        assert_eq!(
            select(allow, "1", &entries),
            Resolution::Pinned("1.1.0".to_owned())
        );
    }

    #[test]
    fn a_prerelease_only_crate_is_reported() {
        let entries = [entry("1.0.0-rc.1", false, None)];
        assert_eq!(
            select(LookupPolicy::default(), "1", &entries),
            Resolution::Skipped(SkipReason::AllPrerelease)
        );
    }

    #[test]
    fn a_requirement_no_release_satisfies_is_reported() {
        let entries = [entry("1.0.0", false, None)];
        assert_eq!(
            select(LookupPolicy::default(), "2", &entries),
            Resolution::Skipped(SkipReason::NoMatchingRelease)
        );
    }

    #[test]
    fn a_pin_cannot_raise_the_minimum_compiler() {
        let entries = [
            entry("1.0.0", false, Some("1.60")),
            entry("1.1.0", false, Some("1.80")),
        ];
        let lookup = make_lookup(LookupPolicy::default());
        let req = Requirement::parse("1").unwrap();

        assert_eq!(
            lookup.select(&req, &entries, PartialVersion::parse("1.63")),
            Resolution::Pinned("1.0.0".to_owned())
        );
        assert_eq!(
            lookup.select(&req, &entries, PartialVersion::parse("1.90")),
            Resolution::Pinned("1.1.0".to_owned())
        );
        assert_eq!(
            lookup.select(&req, &entries, None),
            Resolution::Pinned("1.1.0".to_owned())
        );

        let entries = [entry("1.0.0", false, Some("1.80"))];
        assert!(matches!(
            lookup.select(&req, &entries, PartialVersion::parse("1.63")),
            Resolution::Skipped(SkipReason::BelowRustVersion(_))
        ));

        let ignoring = make_lookup(LookupPolicy {
            ignore_rust_version: true,
            ..LookupPolicy::default()
        });
        assert_eq!(
            ignoring.select(&req, &entries, PartialVersion::parse("1.63")),
            Resolution::Pinned("1.0.0".to_owned())
        );
    }

    #[test]
    fn a_complete_requirement_is_untouched_until_upgrade_is_asked_for() {
        let entries = [entry("1.0.0", false, None), entry("1.7.3", false, None)];
        let request = DepRequest {
            name: "serde",
            req: "1.0.0",
        };

        let plain = make_lookup(LookupPolicy::default());
        assert_eq!(
            plain.resolve(request, None),
            Resolution::Skipped(SkipReason::AlreadyComplete)
        );

        let upgrading = make_lookup(LookupPolicy {
            upgrade: UpgradePolicy {
                enabled: true,
                ..UpgradePolicy::default()
            },
            ..LookupPolicy::default()
        });
        let req = Requirement::parse("1.0.0").unwrap();
        assert_eq!(
            upgrading.select(&req, &entries, None),
            Resolution::Pinned("1.7.3".to_owned())
        );
    }

    #[test]
    fn an_incompatible_upgrade_crosses_a_major_and_a_compatible_one_does_not() {
        let entries = [
            entry("1.7.3", false, None),
            entry("2.0.1", false, None),
            entry("3.0.0-rc.1", false, None),
        ];
        let compatible = LookupPolicy {
            upgrade: UpgradePolicy {
                enabled: true,
                ..UpgradePolicy::default()
            },
            ..LookupPolicy::default()
        };
        assert_eq!(
            select(compatible, "^1.0.0", &entries),
            Resolution::Pinned("^1.7.3".to_owned())
        );

        let incompatible = LookupPolicy {
            upgrade: UpgradePolicy {
                enabled: true,
                incompatible: true,
                ..UpgradePolicy::default()
            },
            ..LookupPolicy::default()
        };
        assert_eq!(
            select(incompatible, "^1.0.0", &entries),
            Resolution::Pinned("^2.0.1".to_owned())
        );
    }

    #[test]
    fn a_pinned_requirement_needs_its_own_flag_to_be_upgraded() {
        let request = DepRequest {
            name: "serde",
            req: "=1.0.0",
        };
        let upgrading = make_lookup(LookupPolicy {
            upgrade: UpgradePolicy {
                enabled: true,
                ..UpgradePolicy::default()
            },
            ..LookupPolicy::default()
        });
        assert_eq!(
            upgrading.resolve(request, None),
            Resolution::Skipped(SkipReason::Pinned)
        );

        let with_pinned = make_lookup(LookupPolicy {
            upgrade: UpgradePolicy {
                enabled: true,
                pinned: true,
                ..UpgradePolicy::default()
            },
            ..LookupPolicy::default()
        });
        assert!(with_pinned.wants(&Requirement::parse("=1.0.0").unwrap()));
    }

    #[test]
    fn an_incomplete_pinned_requirement_is_completed_without_any_upgrade_flag() {
        let plain = make_lookup(LookupPolicy::default());
        assert!(plain.wants(&Requirement::parse("=4.6").unwrap()));
        assert!(!plain.wants(&Requirement::parse("=4.6.6").unwrap()));
    }

    #[test]
    fn an_unpinnable_requirement_says_why_instead_of_doing_nothing() {
        let plain = make_lookup(LookupPolicy::default());
        for (raw, expected) in [
            ("*", Unpinnable::BareWildcard),
            (">=1, <2", Unpinnable::Range),
            (">=1.0", Unpinnable::Inequality),
        ] {
            assert_eq!(
                plain.resolve(
                    DepRequest {
                        name: "serde",
                        req: raw
                    },
                    None
                ),
                Resolution::Skipped(SkipReason::Unpinnable(expected)),
                "{raw}"
            );
        }
    }

    #[test]
    fn offline_without_a_local_index_is_a_skip_not_a_failure() {
        let plain = make_lookup(LookupPolicy::default());
        let resolution = plain.resolve(
            DepRequest {
                name: "serde",
                req: "1.0",
            },
            None,
        );
        assert_eq!(resolution, Resolution::Skipped(SkipReason::Offline));
        assert_eq!(plain.take_error(), None);
    }

    #[test]
    fn prewarm_asks_for_each_crate_once_and_skips_what_it_would_not_rewrite() {
        let plain = make_lookup(LookupPolicy::default());
        let pending = plain.pending(&[
            DepRequest {
                name: "serde",
                req: "1.0",
            },
            DepRequest {
                name: "serde",
                req: "1",
            },
            DepRequest {
                name: "clap",
                req: "4.6",
            },
            DepRequest {
                name: "ignore",
                req: "0.4.33",
            },
            DepRequest {
                name: "tokio",
                req: "*",
            },
        ]);
        assert_eq!(pending, vec!["serde", "clap"]);
    }

    #[test]
    fn notable_skips_are_the_ones_that_could_not_do_what_was_asked() {
        for reason in [
            SkipReason::UnknownCrate,
            SkipReason::AllYanked,
            SkipReason::Offline,
            SkipReason::Unpinnable(Unpinnable::BareWildcard),
        ] {
            assert!(reason.is_notable(), "{reason}");
        }
        for reason in [
            SkipReason::PathSource,
            SkipReason::GitSource,
            SkipReason::WorkspaceInherited,
            SkipReason::AlreadyComplete,
            SkipReason::PatchSection,
        ] {
            assert!(!reason.is_notable(), "{reason}");
        }
    }

    #[test]
    fn a_replaced_source_reports_itself_once_and_skips_every_crate() {
        let map: std::collections::HashMap<String, std::ffi::OsString> =
            std::collections::HashMap::new();
        let env = |key: &str| map.get(key).cloned();
        let mut options = RegistryOptions::resolve(
            std::path::Path::new("/nonexistent"),
            &env,
            &RegistryCli {
                registry_url: None,
                offline: false,
                concurrency: 1,
            },
        );
        options.source = RegistrySource::Replaced("vendored".to_owned());
        let lookup = RegistryLookup::new(options, LookupPolicy::default(), 1).unwrap();

        assert_eq!(
            lookup.resolve(
                DepRequest {
                    name: "serde",
                    req: "1.0"
                },
                None
            ),
            Resolution::Skipped(SkipReason::SourceReplaced("vendored".to_owned()))
        );
        assert_eq!(lookup.take_error(), None);
    }
}
