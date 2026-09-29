use std::{
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
};

use ahash::HashMap;
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
    overrides::{Override, OverrideBuilder},
};
use serde::{Deserialize, Serialize};

use crate::{
    detector::{self, TargetKind, decode_path, is_rust_path, is_toml_path, simplify_path},
    error::{Error, Result},
    git::{self, GitPlan, GitSelection},
    runner::named_scope,
};

pub const DEFAULT_TOML_SKIPS: [&str; 4] =
    ["Cargo.lock", "clippy.toml", "rustfmt.toml", ".rustfmt.toml"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Languages {
    #[default]
    Both,
    Rust,
    Toml,
}

impl Languages {
    pub fn rust(self) -> bool {
        matches!(self, Self::Rust | Self::Both)
    }

    pub fn toml(self) -> bool {
        matches!(self, Self::Toml | Self::Both)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Rust,
    Toml,
}

/// Where the list of files to format comes from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FileSource {
    #[default]
    Paths,
    /// `--files-from`; a `-` entry reads stdin.
    FilesFrom(Vec<PathBuf>),
    Git(GitSelection),
}

impl FileSource {
    pub fn is_explicit(&self) -> bool {
        !matches!(self, Self::Paths)
    }
}

/// The uncompiled, cloneable half of file selection; lives in `FormatterOptions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionOptions {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub languages: Languages,
    pub hidden: bool,
    pub no_ignore: bool,
    pub ignore_paths: Vec<PathBuf>,
    pub max_depth: Option<usize>,
    pub toml_skips: Vec<String>,
    pub default_toml_skips: bool,
    /// Whether the caller narrowed the selection on this run. A repository's own
    /// excludes describe its tree rather than this invocation, so they must not
    /// turn "there is nothing formattable here" into a silent success.
    pub narrowed_by_user: bool,
}

impl Default for SelectionOptions {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            languages: Languages::default(),
            hidden: false,
            no_ignore: false,
            ignore_paths: Vec::new(),
            max_depth: None,
            toml_skips: Vec::new(),
            default_toml_skips: true,
            narrowed_by_user: false,
        }
    }
}

impl SelectionOptions {
    /// Whether the user narrowed the selection themselves. An explicit
    /// selection that matches nothing is a successful no-op, not "there is
    /// nothing formattable here".
    pub fn narrows(&self) -> bool {
        self.narrowed_by_user
    }
}

/// Compiled selection rules. Built once per run and shared by every walk.
#[derive(Debug, Clone)]
pub struct Selector {
    options: SelectionOptions,
    threads: usize,
}

impl Selector {
    pub fn new(options: &SelectionOptions) -> Result<Self> {
        let selector = Self {
            options: options.clone(),
            threads: 1,
        };
        // Compile once up front so a bad glob fails before any filesystem work.
        selector.filter_for(Path::new("."))?;
        Ok(selector)
    }

    pub fn permissive() -> Self {
        Self {
            options: SelectionOptions::default(),
            threads: 1,
        }
    }

    #[must_use]
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    pub fn options(&self) -> &SelectionOptions {
        &self.options
    }

    pub fn languages(&self) -> Languages {
        self.options.languages
    }

    pub fn hidden(&self) -> bool {
        self.options.hidden
    }

    pub fn no_ignore(&self) -> bool {
        self.options.no_ignore
    }

    pub fn ignore_paths(&self) -> &[PathBuf] {
        &self.options.ignore_paths
    }

    pub fn max_depth(&self) -> Option<usize> {
        self.options.max_depth
    }

    /// Glob matching is anchored at the root being walked, so `--exclude
    /// 'tests/**'` means "under the path you named".
    pub fn filter_for(&self, root: &Path) -> Result<Filter> {
        let mut overrides = OverrideBuilder::new(root);
        for pattern in &self.options.include {
            add_override(&mut overrides, pattern)?;
        }
        for pattern in &self.options.exclude {
            add_override(&mut overrides, &format!("!{pattern}"))?;
        }
        let overrides = overrides
            .build()
            .map_err(|err| Error::InvalidGlob(err.to_string()))?;

        let mut skips = GitignoreBuilder::new(root);
        if self.options.default_toml_skips {
            for pattern in DEFAULT_TOML_SKIPS {
                add_skip(&mut skips, pattern)?;
            }
        }
        for pattern in &self.options.toml_skips {
            add_skip(&mut skips, pattern)?;
        }
        let skips = skips
            .build()
            .map_err(|err| Error::InvalidGlob(err.to_string()))?;

        Ok(Filter {
            overrides,
            skips,
            languages: self.options.languages,
        })
    }
}

fn add_override(builder: &mut OverrideBuilder, pattern: &str) -> Result<()> {
    builder
        .add(pattern)
        .map(|_| ())
        .map_err(|err| Error::InvalidGlob(format!("{pattern}: {err}")))
}

fn add_skip(builder: &mut GitignoreBuilder, pattern: &str) -> Result<()> {
    builder
        .add_line(None, pattern)
        .map(|_| ())
        .map_err(|err| Error::InvalidGlob(format!("{pattern}: {err}")))
}

/// Selection rules compiled against one root.
#[derive(Debug, Clone)]
pub struct Filter {
    overrides: Override,
    skips: Gitignore,
    languages: Languages,
}

impl Filter {
    pub fn overrides(&self) -> Override {
        self.overrides.clone()
    }

    pub fn allows_dir(&self, path: &Path) -> bool {
        !self.overrides.matched(path, true).is_ignore()
    }

    pub fn skips_toml(&self, path: &Path) -> bool {
        matches!(self.skips.matched(path, false), Match::Ignore(_))
    }

    /// The one place that decides whether a path is formatted and as what.
    /// Walked entries and explicitly named files both come through here so the
    /// two can never disagree.
    pub fn classify(&self, path: &Path) -> Option<Kind> {
        if self.overrides.matched(path, false).is_ignore() {
            return None;
        }
        if is_rust_path(path) {
            return self.languages.rust().then_some(Kind::Rust);
        }
        if is_toml_path(path) && !self.skips_toml(path) {
            return self.languages.toml().then_some(Kind::Toml);
        }
        None
    }
}

#[derive(Debug, Default)]
pub struct Plan {
    pub targets: Vec<TargetKind>,
    pub warnings: Vec<String>,
    /// Paths the selection could not read. They are carried rather than
    /// returned so the files it did read are still formatted.
    pub errors: Vec<Error>,
    /// Which index owns each selected path, for `--restage`. Only a git scope
    /// sets it.
    pub git: Option<GitPlan>,
    /// The user narrowed the selection, so an empty result is a no-op.
    pub explicit: bool,
}

/// Turn the paths and file-list sources the user gave into concrete targets.
pub fn resolve(
    paths: &[PathBuf],
    source: &FileSource,
    all: bool,
    selector: &Selector,
) -> Result<Plan> {
    let explicit = source.is_explicit() || selector.options().narrows() || paths.len() > 1;

    if let FileSource::Paths = source {
        let mut errors = Vec::new();
        let targets = resolve_paths(paths, all, selector, &mut errors)?;
        return Ok(Plan {
            targets,
            errors,
            explicit,
            ..Plan::default()
        });
    }

    let cwd = std::env::current_dir().map_err(|err| Error::io(".", err))?;
    let mut warnings = Vec::new();
    let mut git_plan = None;
    let (listed, root) = match source {
        FileSource::FilesFrom(sources) => {
            let mut listed = Vec::new();
            for entry in sources {
                listed.extend(read_file_list(entry, &mut warnings)?);
            }
            (listed, cwd)
        }
        FileSource::Git(selection) => {
            let changed = git::changed_files(selection, &cwd)?;
            warnings.extend(changed.warnings.iter().cloned());
            warnings.extend(changed.drifted.iter().map(|path| {
                format!(
                    "warning: {} has unstaged changes; formatted the working tree",
                    path.display()
                )
            }));
            git_plan = Some(GitPlan::from(&changed));
            (changed.files, changed.root)
        }
        FileSource::Paths => unreachable!("handled above"),
    };
    warnings.sort_unstable();
    warnings.dedup();

    let scope = canonical_paths(paths)?;
    let filter = selector.filter_for(&root)?;
    let mut rust_files = Vec::new();
    let mut toml_files = Vec::new();
    let mut errors = Vec::new();

    for path in listed {
        if !scope.is_empty() && !scope.iter().any(|allowed| path.starts_with(allowed)) {
            continue;
        }
        if path.is_dir() {
            let walk = detector::collect(&path, selector, selector.languages(), &[], true)?;
            rust_files.extend(walk.rust_files);
            toml_files.extend(walk.toml_files);
            errors.extend(walk.errors);
            warnings.extend(walk.warnings);
            continue;
        }
        match filter.classify(&path) {
            Some(Kind::Rust) => rust_files.push(path),
            Some(Kind::Toml) => toml_files.push(path),
            None => {}
        }
    }

    rust_files.sort_unstable();
    rust_files.dedup();
    toml_files.sort_unstable();
    toml_files.dedup();

    Ok(Plan {
        targets: vec![TargetKind::FileList {
            rust_files,
            toml_files,
        }],
        warnings,
        errors,
        git: git_plan,
        explicit,
    })
}

fn resolve_paths(
    paths: &[PathBuf],
    all: bool,
    selector: &Selector,
    errors: &mut Vec<Error>,
) -> Result<Vec<TargetKind>> {
    let default = [PathBuf::from(".")];
    let paths = if paths.is_empty() {
        &default[..]
    } else {
        paths
    };

    let narrows = selector.options().narrows();
    // Every override set is rooted at the directory it is built for, so a
    // thousand named files in one directory must not compile it a thousand
    // times.
    let mut filters: HashMap<PathBuf, Filter> = HashMap::default();
    let mut resolved: Vec<TargetKind> = Vec::with_capacity(paths.len());
    for path in paths {
        let target = match detector::detect_target_with(path, selector, errors) {
            Ok(target) => target,
            // A directory the user narrowed down to nothing is a no-op, not a
            // directory with nothing formattable in it.
            Err(Error::NoFormattableFilesFound(_)) if narrows => continue,
            Err(err) => return Err(err),
        };
        if let TargetKind::SingleFile(file) = &target {
            let root = file.parent().unwrap_or(Path::new("."));
            let filter = match filters.get(root) {
                Some(filter) => filter,
                None => filters
                    .entry(root.to_path_buf())
                    .or_insert(selector.filter_for(root)?),
            };
            if filter.classify(file).is_none() {
                continue;
            }
        }
        resolved.push(target);
    }

    Ok(collapse_targets(resolved, all))
}

struct Keyed {
    key: PathBuf,
    scope: Option<PathBuf>,
    target: TargetKind,
}

pub(crate) fn collapse_targets(targets: Vec<TargetKind>, all: bool) -> Vec<TargetKind> {
    let mut resolved: Vec<Keyed> = targets
        .into_iter()
        .map(|target| Keyed {
            key: target_key(&target, all),
            scope: named_scope(&target).map(Path::to_path_buf),
            target,
        })
        .collect();
    resolved.sort_by(|left, right| (&left.key, &left.scope).cmp(&(&right.key, &right.scope)));

    let mut kept: Vec<Keyed> = Vec::with_capacity(resolved.len());
    for candidate in resolved {
        if kept.iter().any(|seen| subsumes(seen, &candidate, all)) {
            continue;
        }
        kept.push(candidate);
    }

    kept.into_iter().map(|keyed| keyed.target).collect()
}

fn target_key(target: &TargetKind, all: bool) -> PathBuf {
    match target {
        TargetKind::CargoProject {
            manifest_path,
            workspace_root,
            ..
        } => {
            if all {
                workspace_root.clone()
            } else {
                manifest_path.clone()
            }
        }
        TargetKind::SingleFile(path) => path.clone(),
        TargetKind::LooseDirectory { root_dir, .. } => root_dir.clone(),
        TargetKind::FileList { .. } => PathBuf::new(),
    }
}

fn subsumes(kept: &Keyed, candidate: &Keyed, all: bool) -> bool {
    if kept.key == candidate.key {
        return match (&kept.scope, &candidate.scope) {
            (None, _) => true,
            (Some(kept_scope), Some(candidate_scope)) => candidate_scope.starts_with(kept_scope),
            (Some(_), None) => false,
        };
    }
    if !candidate.key.starts_with(&kept.key) {
        return false;
    }
    match &kept.target {
        TargetKind::LooseDirectory { .. } => true,
        TargetKind::CargoProject { workspace_root, .. } if all && kept.scope.is_none() => {
            detector::find_cargo_manifest(&candidate.key)
                .is_some_and(|manifest| detector::workspace_root(&manifest) == *workspace_root)
        }
        _ => false,
    }
}

fn canonical_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    paths
        .iter()
        .map(|path| {
            path.canonicalize()
                .map(simplify_path)
                .map_err(|_| Error::PathNotFound(path.clone()))
        })
        .collect()
}

/// Read a NUL- or newline-separated path list. Entries that no longer exist are
/// skipped rather than reported: a hook is handed whatever git listed a moment
/// ago, and a race there must not fail the whole run.
pub fn read_file_list(source: &Path, warnings: &mut Vec<String>) -> Result<Vec<PathBuf>> {
    let bytes = if source == Path::new("-") {
        let mut stdin = std::io::stdin();
        if stdin.is_terminal() {
            return Err(Error::StdinIsATerminal);
        }
        let mut bytes = Vec::new();
        stdin
            .read_to_end(&mut bytes)
            .map_err(|err| Error::io(source, err))?;
        bytes
    } else {
        std::fs::read(source).map_err(|err| Error::io(source, err))?
    };

    // No supported platform allows NUL in a path, so its presence identifies
    // the separator without needing a second flag.
    let separator = if bytes.contains(&0) { 0 } else { b'\n' };

    let mut listed = Vec::new();
    for entry in bytes
        .split(|byte| *byte == separator)
        .map(strip_carriage_return)
        .filter(|entry| !entry.is_empty())
    {
        let Some(path) = decode_path(entry) else {
            warnings.push(format!(
                "warning: skipped {}: the list holds a path that is not valid UTF-8",
                String::from_utf8_lossy(entry)
            ));
            continue;
        };
        if !path.exists() {
            continue;
        }
        match path.canonicalize() {
            Ok(canonical) => listed.push(simplify_path(canonical)),
            Err(err) => warnings.push(format!("warning: skipped {}: {err}", path.display())),
        }
    }
    Ok(listed)
}

fn strip_carriage_return(entry: &[u8]) -> &[u8] {
    entry.strip_suffix(b"\r").unwrap_or(entry)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn selector(options: &SelectionOptions) -> Selector {
        Selector::new(options).unwrap()
    }

    #[test]
    fn a_basename_pattern_matches_at_any_depth() {
        let filter = selector(&SelectionOptions {
            exclude: vec!["build.rs".to_string()],
            ..Default::default()
        })
        .filter_for(Path::new("/ws"))
        .unwrap();
        assert!(
            filter
                .classify(Path::new("/ws/crates/a/build.rs"))
                .is_none()
        );
        assert_eq!(
            filter.classify(Path::new("/ws/crates/a/lib.rs")),
            Some(Kind::Rust)
        );
    }

    #[test]
    fn a_slashed_pattern_is_anchored_at_the_root() {
        let filter = selector(&SelectionOptions {
            exclude: vec!["tests/**".to_string()],
            ..Default::default()
        })
        .filter_for(Path::new("/ws"))
        .unwrap();
        assert!(filter.classify(Path::new("/ws/tests/a.rs")).is_none());
        assert_eq!(
            filter.classify(Path::new("/ws/src/tests/a.rs")),
            Some(Kind::Rust)
        );
    }

    #[test]
    fn include_narrows_files_but_never_gates_directories() {
        let filter = selector(&SelectionOptions {
            include: vec!["src/**".to_string()],
            ..Default::default()
        })
        .filter_for(Path::new("/ws"))
        .unwrap();
        assert_eq!(filter.classify(Path::new("/ws/src/a.rs")), Some(Kind::Rust));
        assert!(filter.classify(Path::new("/ws/other/a.rs")).is_none());
        // An include that pruned directories would stop the walk before it ever
        // reached the files it is meant to select.
        assert!(filter.allows_dir(Path::new("/ws/other")));
    }

    #[test]
    fn the_last_matching_pattern_wins() {
        let filter = selector(&SelectionOptions {
            exclude: vec!["*.rs".to_string()],
            include: vec![],
            ..Default::default()
        })
        .filter_for(Path::new("/ws"))
        .unwrap();
        assert!(filter.classify(Path::new("/ws/a.rs")).is_none());
    }

    #[test]
    fn language_selection_drops_the_other_kind() {
        let rust = selector(&SelectionOptions {
            languages: Languages::Rust,
            ..Default::default()
        })
        .filter_for(Path::new("/ws"))
        .unwrap();
        assert_eq!(rust.classify(Path::new("/ws/a.rs")), Some(Kind::Rust));
        assert!(rust.classify(Path::new("/ws/a.toml")).is_none());

        let toml = selector(&SelectionOptions {
            languages: Languages::Toml,
            ..Default::default()
        })
        .filter_for(Path::new("/ws"))
        .unwrap();
        assert!(toml.classify(Path::new("/ws/a.rs")).is_none());
        assert_eq!(toml.classify(Path::new("/ws/a.toml")), Some(Kind::Toml));
    }

    #[test]
    fn the_default_toml_skips_apply_and_can_be_cleared() {
        let default = selector(&SelectionOptions::default())
            .filter_for(Path::new("/ws"))
            .unwrap();
        for name in DEFAULT_TOML_SKIPS {
            assert!(
                default.classify(&Path::new("/ws").join(name)).is_none(),
                "expected skip for {name}"
            );
        }
        assert_eq!(
            default.classify(Path::new("/ws/deny.toml")),
            Some(Kind::Toml)
        );

        let cleared = selector(&SelectionOptions {
            default_toml_skips: false,
            ..Default::default()
        })
        .filter_for(Path::new("/ws"))
        .unwrap();
        assert_eq!(
            cleared.classify(Path::new("/ws/clippy.toml")),
            Some(Kind::Toml)
        );
    }

    #[test]
    fn skip_toml_globs_add_to_the_defaults_and_bang_removes_one() {
        let filter = selector(&SelectionOptions {
            toml_skips: vec!["fixtures/*.toml".to_string(), "!clippy.toml".to_string()],
            ..Default::default()
        })
        .filter_for(Path::new("/ws"))
        .unwrap();
        assert!(filter.classify(Path::new("/ws/fixtures/a.toml")).is_none());
        assert_eq!(
            filter.classify(Path::new("/ws/clippy.toml")),
            Some(Kind::Toml)
        );
    }

    #[test]
    fn a_bad_glob_is_reported_rather_than_ignored() {
        let err = Selector::new(&SelectionOptions {
            exclude: vec!["a[".to_string()],
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, Error::InvalidGlob(_)), "{err}");
    }

    #[test]
    fn a_file_list_accepts_both_separators_and_skips_blanks() {
        let temp = tempdir().unwrap();
        let a = temp.path().join("a.rs");
        let b = temp.path().join("b.rs");
        fs::write(&a, "fn a() {}\n").unwrap();
        fs::write(&b, "fn b() {}\n").unwrap();

        let newline = temp.path().join("list.txt");
        fs::write(&newline, format!("{}\n\n{}\n", a.display(), b.display())).unwrap();
        assert_eq!(read_file_list(&newline, &mut Vec::new()).unwrap().len(), 2);

        let nul = temp.path().join("list.nul");
        fs::write(
            &nul,
            format!("{}\0{}\0", a.display(), b.display()).as_bytes(),
        )
        .unwrap();
        assert_eq!(read_file_list(&nul, &mut Vec::new()).unwrap().len(), 2);
    }

    #[test]
    fn a_file_list_entry_that_vanished_is_skipped() {
        let temp = tempdir().unwrap();
        let list = temp.path().join("list.txt");
        fs::write(
            &list,
            format!("{}\n", temp.path().join("gone.rs").display()),
        )
        .unwrap();
        assert!(read_file_list(&list, &mut Vec::new()).unwrap().is_empty());
    }
}
