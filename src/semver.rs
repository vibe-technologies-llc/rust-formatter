use std::fmt;

use semver::{Op, Prerelease, Version, VersionReq};

/// The operator as it was written, which `semver::Op` cannot express: cargo's
/// `1.2` and `^1.2` are the same requirement but must be rewritten differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Implicit,
    Caret,
    Tilde,
    Exact,
    Wildcard,
}

impl Operator {
    fn as_str(self) -> &'static str {
        match self {
            Self::Implicit | Self::Wildcard => "",
            Self::Caret => "^",
            Self::Tilde => "~",
            Self::Exact => "=",
        }
    }
}

/// Why a requirement cannot be completed to `x.y.z`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unpinnable {
    Empty,
    Malformed,
    Range,
    Inequality,
    BareWildcard,
    Prerelease,
}

impl fmt::Display for Unpinnable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "the requirement is empty",
            Self::Malformed => "not a valid version requirement",
            Self::Range => "a multi-comparator range already names its bounds",
            Self::Inequality => "a `<`/`>` comparator already names its bound",
            Self::BareWildcard => "`*` has no major version to complete",
            Self::Prerelease => "the requirement names a pre-release",
        };
        f.write_str(text)
    }
}

/// A cargo dependency requirement this tool may rewrite: exactly one comparator,
/// an operator that anchors rather than bounds, and no pre-release.
#[derive(Debug, Clone)]
pub struct Requirement {
    req: VersionReq,
    operator: Operator,
    complete: bool,
    patch_wildcard: bool,
}

impl Requirement {
    pub fn parse(raw: &str) -> Result<Self, Unpinnable> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(Unpinnable::Empty);
        }

        let req = VersionReq::parse(trimmed).map_err(|_| Unpinnable::Malformed)?;
        let [comparator] = req.comparators.as_slice() else {
            return Err(if req.comparators.is_empty() {
                Unpinnable::BareWildcard
            } else {
                Unpinnable::Range
            });
        };

        let operator = match comparator.op {
            Op::Caret if trimmed.starts_with('^') => Operator::Caret,
            Op::Caret => Operator::Implicit,
            Op::Tilde => Operator::Tilde,
            Op::Exact => Operator::Exact,
            Op::Wildcard => Operator::Wildcard,
            _ => return Err(Unpinnable::Inequality),
        };
        if comparator.pre != Prerelease::EMPTY {
            return Err(Unpinnable::Prerelease);
        }

        // `1.*` spells a minor without pinning one, so a wildcard is never
        // complete however many components it carries.
        let complete = comparator.patch.is_some() && operator != Operator::Wildcard;
        let patch_wildcard = operator == Operator::Wildcard && comparator.minor.is_some();

        Ok(Self {
            req,
            operator,
            complete,
            patch_wildcard,
        })
    }

    /// Whether the requirement already spells out `x.y.z`, which is what
    /// separates completing a requirement from upgrading one.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// An `=` requirement, which cargo-edit's vocabulary calls pinned.
    pub fn is_pinned(&self) -> bool {
        self.operator == Operator::Exact
    }

    /// Cargo's own matching, including the pre-release rule: a pre-release
    /// version satisfies a requirement only when the requirement names one.
    pub fn matches(&self, version: &Version) -> bool {
        self.req.matches(version)
    }

    /// The requirement written out against `version`, keeping the operator and
    /// dropping the pre-release and build metadata cargo ignores when matching.
    pub fn render(&self, version: &Version) -> String {
        let prefix = if self.patch_wildcard {
            "~"
        } else {
            self.operator.as_str()
        };
        format!(
            "{}{}.{}.{}",
            prefix, version.major, version.minor, version.patch
        )
    }
}

/// Highest release satisfying `req`.
pub fn best_match<'a>(
    req: &Requirement,
    versions: impl IntoIterator<Item = &'a Version>,
) -> Option<&'a Version> {
    versions
        .into_iter()
        .filter(|version| req.matches(version))
        .max()
}

/// A `rust-version` as cargo writes it: `1`, `1.63` or `1.63.0`, where an absent
/// component means zero rather than "any".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PartialVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl PartialVersion {
    pub const ZERO: Self = Self {
        major: 0,
        minor: 0,
        patch: 0,
    };

    pub fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.trim().split('.');
        let major = parse_component(parts.next()?)?;
        let minor = parts.next().map_or(Some(0), parse_component)?;
        let patch = parts.next().map_or(Some(0), parse_component)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for PartialVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn parse_component(part: &str) -> Option<u64> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(list: &[&str]) -> Vec<Version> {
        list.iter()
            .map(|raw| Version::parse(raw).unwrap())
            .collect()
    }

    fn complete(current: &str, available: &[&str]) -> Option<String> {
        let req = Requirement::parse(current).ok()?;
        if req.is_complete() {
            return None;
        }
        let list = versions(available);
        best_match(&req, &list).map(|version| req.render(version))
    }

    #[test]
    fn completes_a_partial_requirement() {
        let avail = ["4.5.0", "4.6.1", "4.6.6", "5.0.0"];
        assert_eq!(complete("4.6", &avail).as_deref(), Some("4.6.6"));
        assert_eq!(complete("4.6.6", &avail), None);
        assert_eq!(complete("4", &avail).as_deref(), Some("4.6.6"));
    }

    #[test]
    fn caret_admits_a_higher_minor() {
        let avail = ["1.0.100", "1.0.230", "1.1.0"];
        assert_eq!(complete("^1.0", &avail).as_deref(), Some("^1.1.0"));
        assert_eq!(complete("1.0", &avail).as_deref(), Some("1.1.0"));
        assert_eq!(complete("1", &avail).as_deref(), Some("1.1.0"));
    }

    #[test]
    fn tilde_stays_inside_its_minor() {
        let avail = ["1.0.100", "1.0.230", "1.1.0"];
        assert_eq!(complete("~1.0", &avail).as_deref(), Some("~1.0.230"));
        assert_eq!(complete("~1", &avail).as_deref(), Some("~1.1.0"));
    }

    #[test]
    fn exact_keeps_its_operator_and_narrows() {
        let avail = ["4.6.1", "4.6.6", "4.7.0"];
        assert_eq!(complete("=4.6", &avail).as_deref(), Some("=4.6.6"));
        assert_eq!(complete("=4", &avail).as_deref(), Some("=4.7.0"));
    }

    #[test]
    fn wildcards_complete_like_a_bare_requirement() {
        let avail = ["1.0.100", "1.2.9", "1.7.3", "2.0.0"];
        assert_eq!(complete("1.*", &avail).as_deref(), Some("1.7.3"));
        assert_eq!(complete("1.x", &avail).as_deref(), Some("1.7.3"));
        assert_eq!(complete("1.2.*", &avail).as_deref(), Some("~1.2.9"));
        assert_eq!(complete("1.2.x", &avail).as_deref(), Some("~1.2.9"));
    }

    #[test]
    fn build_metadata_is_dropped_from_the_completion() {
        let avail = ["0.25.12", "0.25.13+spec-1.1.0"];
        assert_eq!(complete("0.25", &avail).as_deref(), Some("0.25.13"));
    }

    #[test]
    fn prereleases_are_not_selected() {
        let avail = ["1.0.0", "1.1.0-rc.1"];
        assert_eq!(complete("1", &avail).as_deref(), Some("1.0.0"));
    }

    #[test]
    fn zero_major_caret_stays_inside_its_minor() {
        let avail = ["0.4.33", "0.5.0"];
        assert_eq!(complete("0.4", &avail).as_deref(), Some("0.4.33"));
        assert_eq!(complete("0", &avail).as_deref(), Some("0.5.0"));
    }

    #[test]
    fn unpinnable_requirements_report_why() {
        for (raw, expected) in [
            ("", Unpinnable::Empty),
            ("   ", Unpinnable::Empty),
            ("a.b", Unpinnable::Malformed),
            (">=1.0", Unpinnable::Inequality),
            ("<2", Unpinnable::Inequality),
            (">=1, <2", Unpinnable::Range),
            ("*", Unpinnable::BareWildcard),
            ("^1.0.0-rc.1", Unpinnable::Prerelease),
        ] {
            assert_eq!(
                Requirement::parse(raw).map(|_| ()).unwrap_err(),
                expected,
                "{raw}"
            );
        }
    }

    #[test]
    fn completeness_separates_completion_from_upgrade() {
        assert!(Requirement::parse("1.2.3").unwrap().is_complete());
        assert!(Requirement::parse("=1.2.3").unwrap().is_complete());
        assert!(!Requirement::parse("1.2").unwrap().is_complete());
        assert!(!Requirement::parse("=1").unwrap().is_complete());
        assert!(!Requirement::parse("1.*").unwrap().is_complete());
        assert!(!Requirement::parse("1.2.*").unwrap().is_complete());
    }

    #[test]
    fn pinned_requirements_are_recognised() {
        assert!(Requirement::parse("=1.2.3").unwrap().is_pinned());
        assert!(!Requirement::parse("1.2.3").unwrap().is_pinned());
        assert!(!Requirement::parse("^1.2.3").unwrap().is_pinned());
    }

    #[test]
    fn an_upgrade_moves_the_operator_with_the_version() {
        let target = Version::parse("2.0.1").unwrap();
        assert_eq!(
            Requirement::parse("^1.2").unwrap().render(&target),
            "^2.0.1"
        );
        assert_eq!(
            Requirement::parse("1.2.3").unwrap().render(&target),
            "2.0.1"
        );
        assert_eq!(
            Requirement::parse("=1.2.3").unwrap().render(&target),
            "=2.0.1"
        );
        assert_eq!(
            Requirement::parse("1.2.*").unwrap().render(&target),
            "~2.0.1"
        );
    }

    #[test]
    fn a_partial_rust_version_fills_missing_components_with_zero() {
        assert_eq!(PartialVersion::parse("1"), PartialVersion::parse("1.0.0"));
        assert_eq!(
            PartialVersion::parse("1.63"),
            PartialVersion::parse("1.63.0")
        );
        assert!(PartialVersion::parse("1.63") < PartialVersion::parse("1.70"));
        assert!(PartialVersion::parse("1.9") < PartialVersion::parse("1.10"));
        assert_eq!(PartialVersion::parse("1.2.3.4"), None);
        assert_eq!(PartialVersion::parse("nightly"), None);
        assert_eq!(PartialVersion::parse(""), None);
    }
}
