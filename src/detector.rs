use std::{
    borrow::Cow,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use globset::GlobBuilder;
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState};

use crate::{
    cargo_config,
    error::{Error, Result},
    selection::{Filter, Kind, Languages, Selector},
};

const ALLOWED_HIDDEN_DIRS: [&str; 2] = [".cargo", ".config"];

/// Backstop for the ancestor walks below. Deep enough for any real source tree,
/// shallow enough that a symlink loop or an odd mount cannot spin.
const MAX_ANCESTOR_DEPTH: usize = 32;

/// Represents the detected type of target to format.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TargetKind {
    /// A Cargo project or workspace member with a `Cargo.toml`.
    CargoProject {
        manifest_path: PathBuf,
        root_dir: PathBuf,
        /// Cargo's workspace when metadata has answered, otherwise the parser's.
        workspace_root: PathBuf,
    },
    /// A single source file (`.rs` or `.toml`).
    SingleFile(PathBuf),
    /// A directory containing loose `.rs` / `.toml` files (no root `Cargo.toml` found).
    LooseDirectory {
        root_dir: PathBuf,
        files: Vec<PathBuf>,
        toml_files: Vec<PathBuf>,
    },
    /// An explicit set of files, from `--files-from`, `--since` or `--staged`.
    FileList {
        rust_files: Vec<PathBuf>,
        toml_files: Vec<PathBuf>,
    },
}

/// Decode a path exactly as the operating system spells it. `-z` output from
/// git and NUL-separated file lists are raw bytes, so a lossy conversion would
/// invent a path that does not exist; `None` lets the caller say so instead.
#[cfg(unix)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `Option` is the cross-platform contract; the non-unix arm below does return None"
)]
pub(crate) fn decode_path(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    Some(PathBuf::from(OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
pub(crate) fn decode_path(bytes: &[u8]) -> Option<PathBuf> {
    std::str::from_utf8(bytes).ok().map(PathBuf::from)
}

/// Resolve `.` and `..` textually. Both sides of every comparison below are
/// already canonical, so there is no symlink for a lexical `..` to cross.
pub(crate) fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path`, made absolute against the process working directory, without
/// requiring it to exist.
///
/// An editor hands over the buffer's path, which may be relative to the
/// editor's own working directory -- and a relative path has no ancestors for
/// the manifest walk to climb, because [`bounded_ancestors`] stops at the empty
/// path that `Path::ancestors` ends on. A buffer may also have no file yet, so
/// this cannot go through `canonicalize`.
pub fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return normalize_lexically(path);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    normalize_lexically(&cwd.join(path))
}

/// `fs::canonicalize` yields `\\?\C:\...` verbatim paths on Windows. Cargo, and
/// every path this tool prints, want the plain `C:\...` form, so drop the prefix
/// whenever the remainder is an ordinary path.
pub(crate) fn simplify_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};

        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next()
            && let Prefix::VerbatimDisk(letter) = prefix.kind()
        {
            let rest = components.as_path();
            if rest
                .components()
                .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
            {
                let mut simplified = PathBuf::from(format!("{}:\\", letter as char));
                simplified.push(rest);
                return simplified;
            }
        }
    }
    path
}

/// Analyzes the given path and detects whether it is a Cargo project, a single file, or loose files.
pub fn detect_target(target_path: &Path) -> Result<TargetKind> {
    let mut errors = Vec::new();
    let target = detect_target_with(target_path, &Selector::permissive(), &mut errors)?;
    match errors.into_iter().next() {
        Some(err) => Err(err),
        None => Ok(target),
    }
}

/// `errors` collects the paths the walk could not read. They are reported per
/// path rather than returned, so the files it did read are still formatted.
pub fn detect_target_with(
    target_path: &Path,
    selector: &Selector,
    errors: &mut Vec<Error>,
) -> Result<TargetKind> {
    let canonical = simplify_path(target_path.canonicalize().map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => Error::PathNotFound(target_path.to_path_buf()),
        _ => Error::io(target_path, err),
    })?);
    let metadata = fs::metadata(&canonical).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => Error::PathNotFound(target_path.to_path_buf()),
        _ => Error::io(&canonical, err),
    })?;

    if metadata.is_file() {
        return single_file_target(canonical)
            .ok_or_else(|| Error::UnsupportedTarget(target_path.to_path_buf()));
    }

    if !metadata.is_dir() {
        return Err(Error::UnsupportedTarget(target_path.to_path_buf()));
    }

    if let Some(manifest_path) = find_cargo_manifest(&canonical) {
        let workspace_root = workspace_root(&manifest_path);
        return Ok(TargetKind::CargoProject {
            manifest_path,
            root_dir: canonical,
            workspace_root,
        });
    }

    let walk = collect(&canonical, selector, selector.languages(), &[], true)?;
    if walk.rust_files.is_empty() && walk.toml_files.is_empty() {
        return match walk.errors.into_iter().next() {
            Some(err) => Err(err),
            None => Err(Error::NoFormattableFilesFound(canonical)),
        };
    }
    errors.extend(walk.errors);

    Ok(TargetKind::LooseDirectory {
        root_dir: canonical,
        files: walk.rust_files,
        toml_files: walk.toml_files,
    })
}

fn single_file_target(path: PathBuf) -> Option<TargetKind> {
    if is_cargo_manifest(&path) {
        let root_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let workspace_root = workspace_root(&path);
        return Some(TargetKind::CargoProject {
            manifest_path: path,
            root_dir,
            workspace_root,
        });
    }
    (is_rust_path(&path) || is_toml_path(&path)).then_some(TargetKind::SingleFile(path))
}

/// How far an ancestor walk may travel before it leaves the project.
pub(crate) struct Boundary {
    home: Option<PathBuf>,
    stop_at_git: bool,
}

impl Boundary {
    /// For finding the manifest that owns a path: a repository boundary is the
    /// outermost thing that can still be the same project.
    pub(crate) fn project() -> Self {
        Self {
            home: cargo_config::home_dir(&cargo_config::process_env),
            stop_at_git: true,
        }
    }

    /// For resolving a workspace from a manifest already known to be real. A
    /// git boundary is wrong here: a workspace member can be a submodule, and
    /// its `.git` sits below the workspace root.
    pub(crate) fn workspace() -> Self {
        Self {
            home: cargo_config::home_dir(&cargo_config::process_env),
            stop_at_git: false,
        }
    }

    /// `std::env::set_var` is unsafe and process-global in edition 2024, so a
    /// test cannot fake `$HOME`; it has to be injected.
    #[cfg(test)]
    pub(crate) fn with_home(home: Option<PathBuf>, stop_at_git: bool) -> Self {
        Self { home, stop_at_git }
    }
}

/// Ancestors of `start` that can still belong to the same project: the walk
/// stops at a repository boundary and never reaches `$HOME`.
pub fn project_ancestors(start: &Path) -> Vec<&Path> {
    bounded_ancestors(start, &Boundary::project()).collect()
}

pub(crate) fn bounded_ancestors<'a, 'b>(
    start: &'a Path,
    boundary: &'b Boundary,
) -> impl Iterator<Item = &'a Path> + use<'a, 'b> {
    let mut past_start = false;
    let mut done = false;

    start
        .ancestors()
        .take(MAX_ANCESTOR_DEPTH)
        .take_while(move |dir| {
            if done {
                return false;
            }
            // `$HOME` and its ancestors are never a project root, or a stray
            // `~/Cargo.toml` promotes every scratch directory below it. The
            // start directory itself is exempt so `cd ~ && rust-formatter` works.
            if past_start
                && boundary
                    .home
                    .as_deref()
                    .is_some_and(|home| home.starts_with(dir))
            {
                return false;
            }
            past_start = true;
            if boundary.stop_at_git && dir.join(".git").exists() {
                done = true;
            }
            true
        })
}

/// Traverse ancestors of `start_path` to find the nearest `Cargo.toml`, without
/// leaving the enclosing repository or reaching `$HOME`.
pub fn find_cargo_manifest(start_path: &Path) -> Option<PathBuf> {
    find_cargo_manifest_within(start_path, &Boundary::project())
}

pub(crate) fn find_cargo_manifest_within(
    start_path: &Path,
    boundary: &Boundary,
) -> Option<PathBuf> {
    let start = if start_path.is_file() {
        start_path.parent()?
    } else {
        start_path
    };

    bounded_ancestors(start, boundary)
        .map(|dir| dir.join("Cargo.toml"))
        .find(|manifest| manifest.is_file())
}

/// Collect all `.rs` files recursively in `dir`, respecting `.gitignore` and skipping hidden/.git/build dirs.
pub fn collect_rust_files(dir: &Path) -> Result<Vec<PathBuf>> {
    collect(dir, &Selector::permissive(), Languages::Rust, &[], true)?
        .into_result()
        .map(|(files, _)| files)
}

/// Like [`collect_rust_files`], but does not descend into `prune_dirs`.
pub fn collect_rust_files_pruning(dir: &Path, prune_dirs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    collect(
        dir,
        &Selector::permissive(),
        Languages::Rust,
        prune_dirs,
        true,
    )?
    .into_result()
    .map(|(files, _)| files)
}

/// Collect `.toml` files recursively, respecting `.gitignore` and the TOML skip list.
pub fn collect_toml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    collect(dir, &Selector::permissive(), Languages::Toml, &[], true)?
        .into_result()
        .map(|(_, files)| files)
}

pub fn workspace_root(manifest_path: &Path) -> PathBuf {
    let start = manifest_path.parent().unwrap_or(Path::new("."));

    if let Some(explicit) = package_workspace_dir(manifest_path) {
        let workspace_manifest = explicit.join("Cargo.toml");
        if workspace_manifest.is_file()
            && let Ok(source) = fs::read_to_string(&workspace_manifest)
            && let Ok(doc) = source.parse::<toml_edit::DocumentMut>()
            && doc
                .get("workspace")
                .and_then(toml_edit::Item::as_table)
                .is_some()
        {
            return simplify_path(fs::canonicalize(&explicit).unwrap_or(explicit));
        }
    }

    let boundary = Boundary::workspace();
    for dir in bounded_ancestors(start, &boundary) {
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        if let Ok(source) = fs::read_to_string(&manifest)
            && let Ok(doc) = source.parse::<toml_edit::DocumentMut>()
            && let Some(workspace) = doc.get("workspace").and_then(toml_edit::Item::as_table)
        {
            if dir == start || is_workspace_member(dir, start, workspace) {
                return canonical_dir(dir);
            }
            return canonical_dir(start);
        }
    }

    canonical_dir(start)
}

pub fn workspace_member_dirs(workspace_root: &Path) -> Vec<PathBuf> {
    let manifest = workspace_root.join("Cargo.toml");
    let Ok(source) = fs::read_to_string(&manifest) else {
        return vec![canonical_dir(workspace_root)];
    };
    let Ok(doc) = source.parse::<toml_edit::DocumentMut>() else {
        return vec![canonical_dir(workspace_root)];
    };
    let Some(workspace) = doc.get("workspace").and_then(toml_edit::Item::as_table) else {
        return vec![canonical_dir(workspace_root)];
    };

    let mut members = Vec::new();
    if doc.get("package").is_some() {
        members.push(canonical_dir(workspace_root));
    }

    let mut builder = WalkBuilder::new(workspace_root);
    builder
        .hidden(false)
        .require_git(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(|entry| {
            let name = entry.file_name();
            name != ".git" && name != "target"
        });

    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        if entry.file_name() != "Cargo.toml" {
            continue;
        }
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() || !kind.is_file() {
            continue;
        }
        let Some(dir) = entry.path().parent() else {
            continue;
        };
        if dir == workspace_root {
            continue;
        }
        if is_workspace_member(workspace_root, dir, workspace) {
            members.push(canonical_dir(dir));
        }
    }

    members.sort();
    members.dedup();
    members
}

/// `cargo metadata` always reports a canonical workspace root. Matching it here
/// is what lets the two resolvers produce comparable keys.
pub fn canonical_dir(dir: &Path) -> PathBuf {
    simplify_path(fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
}

fn package_workspace_dir(manifest_path: &Path) -> Option<PathBuf> {
    let source = fs::read_to_string(manifest_path).ok()?;
    let doc = source.parse::<toml_edit::DocumentMut>().ok()?;
    let rel = doc.get("package")?.get("workspace")?.as_str()?;
    let parent = manifest_path.parent()?;
    Some(parent.join(rel))
}

fn is_workspace_member(workspace_dir: &Path, pkg_dir: &Path, workspace: &toml_edit::Table) -> bool {
    let members = toml_string_array(workspace.get("members"));
    let exclude = toml_string_array(workspace.get("exclude"));

    if matches_workspace_patterns(workspace_dir, pkg_dir, &exclude) {
        return false;
    }
    if members.is_empty() {
        return pkg_dir == workspace_dir;
    }
    matches_workspace_patterns(workspace_dir, pkg_dir, &members)
}

fn toml_string_array(item: Option<&toml_edit::Item>) -> Vec<&str> {
    let Some(array) = item.and_then(toml_edit::Item::as_array) else {
        return Vec::new();
    };
    array.iter().filter_map(toml_edit::Value::as_str).collect()
}

fn matches_workspace_patterns(workspace_dir: &Path, pkg_dir: &Path, patterns: &[&str]) -> bool {
    patterns
        .iter()
        .any(|pattern| member_pattern_matches(workspace_dir, pkg_dir, pattern))
}

fn member_pattern_matches(workspace_dir: &Path, pkg_dir: &Path, pattern: &str) -> bool {
    expand_braces(pattern)
        .iter()
        .any(|pat| one_member_pattern_matches(workspace_dir, pkg_dir, pat))
}

fn one_member_pattern_matches(workspace_dir: &Path, pkg_dir: &Path, pattern: &str) -> bool {
    if pattern == "." {
        return pkg_dir == workspace_dir;
    }
    let candidate = normalize_lexically(&workspace_dir.join(pattern));
    let target = normalize_lexically(pkg_dir);
    if candidate == target {
        return true;
    }
    // `members = ["../pkg"]` leaves the root entirely, so the pattern is matched
    // against the absolute path rather than a relative one that cannot exist.
    // Only the pattern's own segments may glob: a directory really named `a*`
    // higher up the tree is escaped back into a literal.
    let mut absolute = String::new();
    for component in normalize_lexically(workspace_dir).components() {
        let text = component.as_os_str().to_string_lossy().replace('\\', "/");
        push_glob_segment(&mut absolute, &globset::escape(&text));
    }
    for component in Path::new(pattern).components() {
        let text = component.as_os_str().to_string_lossy().replace('\\', "/");
        match text.as_str() {
            "." => {}
            ".." => pop_glob_segment(&mut absolute),
            segment => push_glob_segment(&mut absolute, segment),
        }
    }
    let target = target.to_string_lossy().replace('\\', "/");
    let Ok(glob) = GlobBuilder::new(&absolute).literal_separator(true).build() else {
        return false;
    };
    glob.compile_matcher().is_match(target.as_str())
}

fn push_glob_segment(out: &mut String, segment: &str) {
    if segment == "/" {
        out.push('/');
        return;
    }
    if !out.is_empty() && !out.ends_with('/') {
        out.push('/');
    }
    out.push_str(segment);
}

fn pop_glob_segment(out: &mut String) {
    match out.rfind('/') {
        Some(0) => out.truncate(1),
        Some(index) => out.truncate(index),
        None => out.clear(),
    }
}

fn expand_braces(pattern: &str) -> Vec<Cow<'_, str>> {
    let Some(start) = pattern.find('{') else {
        return vec![Cow::Borrowed(pattern)];
    };
    let Some(end_rel) = pattern[start + 1..].find('}') else {
        return vec![Cow::Borrowed(pattern)];
    };
    let end = start + 1 + end_rel;
    let prefix = &pattern[..start];
    let suffix = &pattern[end + 1..];
    let mut expanded = Vec::new();
    for alt in pattern[start + 1..end].split(',') {
        let combined = format!("{prefix}{alt}{suffix}");
        expanded.extend(
            expand_braces(&combined)
                .into_iter()
                .map(|piece| Cow::Owned(piece.into_owned())),
        );
    }
    expanded
}

/// Whether a file found under a workspace root belongs to the run. Walking up
/// from it, the workspace root means "loose at the root, in scope"; a nearer
/// `Cargo.toml` means the file belongs to that package, and only a selected
/// member is in scope. An excluded or nested non-member package is therefore
/// left alone rather than formatted under the root package's settings.
pub fn file_in_workspace_scope(
    path: &Path,
    workspace_root: &Path,
    member_dirs: &[PathBuf],
) -> bool {
    let Some(parent) = path.parent() else {
        return true;
    };
    for dir in parent.ancestors() {
        if dir == workspace_root {
            return true;
        }
        if dir.join("Cargo.toml").is_file() {
            return member_dirs.iter().any(|member| member == dir);
        }
    }
    true
}

pub fn is_rust_path(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "rs")
}

pub fn is_toml_path(path: &Path) -> bool {
    if path.extension().is_some_and(|ext| ext == "toml") {
        return true;
    }
    path.file_name().is_some_and(|name| name == "config")
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == ".cargo")
}

fn is_cargo_manifest(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "Cargo.toml")
}

struct WalkScope {
    root: PathBuf,
    filter: Filter,
    prune_dirs: Vec<PathBuf>,
    build_dirs: Vec<PathBuf>,
    hidden: bool,
    wanted: Languages,
}

/// Whether the walk descends into a directory, and whether it looks at an entry
/// at all.
///
/// Lifted out of the walk so `--watch` can ask the same questions of an event's
/// ancestors: an ignored subtree must never wake a run that would then ignore
/// it, and two implementations of this would drift.
pub struct DirGate<'a> {
    pub filter: &'a Filter,
    pub prune_dirs: &'a [PathBuf],
    pub build_dirs: &'a [PathBuf],
    pub hidden: bool,
}

impl DirGate<'_> {
    /// Whether the walk should skip this directory and everything under it.
    ///
    /// Ordered by what each test costs. Two of them read the directory's
    /// contents -- a `CACHEDIR.TAG` and the `.cargo-checksum.json` cargo
    /// writes into a vendored crate -- and those are the only two syscalls
    /// here, so nothing that can be decided from a name or from the glob
    /// filter is decided after them. The order is otherwise the one it always
    /// was: an allowed hidden directory such as `.cargo` is still pruned when
    /// it holds build output, which is why the two reads come last rather than
    /// being skipped for it.
    pub fn prunes(&self, path: &Path, name: &OsStr) -> bool {
        // `.git` and build output stay pruned under every flag: rewriting
        // either is never what the caller meant.
        if name == ".git" {
            return true;
        }
        if self.prune_dirs.iter().any(|pruned| pruned == path) {
            return true;
        }
        if cargo_config::is_named_build_dir(path, name, self.build_dirs)
            || cargo_config::is_vendor_dir(path, name)
        {
            return true;
        }
        if !self.hidden && is_dot_name(name) {
            if !ALLOWED_HIDDEN_DIRS.iter().any(|allowed| name == *allowed) {
                return true;
            }
        } else if !self.filter.allows_dir(path) {
            return true;
        }
        cargo_config::has_cachedir_tag(path) || path.join(".cargo-checksum.json").is_file()
    }
}

/// Whether the walk skips an entry for its name alone. Separate from
/// [`DirGate::prunes`] because `.cargo` is a hidden directory the walk is
/// allowed into while a hidden *file* is still skipped.
pub fn hides_entry(name: &OsStr) -> bool {
    is_dot_name(name)
}

impl WalkScope {
    fn gate(&self) -> DirGate<'_> {
        DirGate {
            filter: &self.filter,
            prune_dirs: &self.prune_dirs,
            build_dirs: &self.build_dirs,
            hidden: self.hidden,
        }
    }

    fn prunes_dir(&self, path: &Path, name: &OsStr) -> bool {
        self.gate().prunes(path, name)
    }

    fn classify(&self, entry: &DirEntry) -> Option<Kind> {
        if !self.hidden && is_dot_name(entry.file_name()) {
            return None;
        }
        match self.filter.classify(entry.path()) {
            Some(Kind::Rust) if self.wanted.rust() => Some(Kind::Rust),
            Some(Kind::Toml) if self.wanted.toml() => Some(Kind::Toml),
            _ => None,
        }
    }
}

/// What one walk found, and what it could not read. A directory the walk cannot
/// enter is reported per path rather than aborting the run, so one
/// permission-denied subtree does not make the whole repository unformattable.
#[derive(Debug, Default)]
pub struct Walk {
    pub rust_files: Vec<PathBuf>,
    pub toml_files: Vec<PathBuf>,
    pub errors: Vec<Error>,
    /// Findings that leave the walk usable but worth saying out loud, such as a
    /// cargo configuration file that did not parse and so could not name the
    /// build directory this walk should have pruned.
    pub warnings: Vec<String>,
}

impl Walk {
    /// The first failure, for the callers that have nowhere to report the rest.
    pub fn into_result(mut self) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        if self.errors.is_empty() {
            Ok((self.rust_files, self.toml_files))
        } else {
            Err(self.errors.remove(0))
        }
    }
}

/// Walk `root` and gather the files that survive `selector`, further narrowed to
/// `wanted`. `apply_depth` is false for the internal per-package walks: a
/// `--max-depth` typed against the tree the user named must not be re-measured
/// from a directory they never mentioned.
pub fn collect(
    root: &Path,
    selector: &Selector,
    wanted: Languages,
    prune_dirs: &[PathBuf],
    apply_depth: bool,
) -> Result<Walk> {
    let filter = selector.filter_for(root)?;
    let overrides = filter.overrides();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let build = cargo_config::target_dir(root, &cwd, &cargo_config::process_env);
    let warnings = build
        .unreadable
        .iter()
        .map(|path| {
            format!(
                "warning: {} did not parse; the build directory it names was not pruned",
                path.display()
            )
        })
        .collect();
    let scope = WalkScope {
        root: root.to_path_buf(),
        filter,
        prune_dirs: prune_dirs.to_vec(),
        build_dirs: build.path.into_iter().collect(),
        hidden: selector.hidden(),
        wanted,
    };

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .require_git(false)
        .overrides(overrides);
    if selector.no_ignore() {
        builder
            .ignore(false)
            .parents(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
    } else {
        builder.git_ignore(true).git_global(true).git_exclude(true);
    }
    for path in selector.ignore_paths() {
        if let Some(err) = builder.add_ignore(path) {
            return Err(Error::io(path, io::Error::other(err.to_string())));
        }
    }
    if apply_depth {
        builder.max_depth(selector.max_depth());
    }
    builder.threads(selector.threads());

    let collected = Mutex::new(Vec::new());
    let failures = Mutex::new(Vec::new());
    builder.build_parallel().visit(&mut Collector {
        scope: &scope,
        collected: &collected,
        failures: &failures,
    });

    let mut errors = failures
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    // Threads fail independently, so sorting is what makes the report stable.
    errors.sort_by_key(ToString::to_string);
    errors.dedup_by_key(|err| err.to_string());

    let parts = collected
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    let mut rust_files = Vec::with_capacity(parts.iter().map(|(rust, _)| rust.len()).sum());
    let mut toml_files = Vec::with_capacity(parts.iter().map(|(_, toml)| toml.len()).sum());
    for (rust, toml) in parts {
        rust_files.extend(rust);
        toml_files.extend(toml);
    }

    // `WalkBuilder::sort_by_file_path` is ignored by `build_parallel`, so this
    // is the only thing making the result deterministic.
    rust_files.sort_unstable();
    rust_files.dedup();
    toml_files.sort_unstable();
    toml_files.dedup();
    Ok(Walk {
        rust_files,
        toml_files,
        errors,
        warnings,
    })
}

struct Collector<'s> {
    scope: &'s WalkScope,
    collected: &'s Mutex<Vec<(Vec<PathBuf>, Vec<PathBuf>)>>,
    failures: &'s Mutex<Vec<Error>>,
}

impl<'s> ParallelVisitorBuilder<'s> for Collector<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Visitor {
            scope: self.scope,
            collected: self.collected,
            failures: self.failures,
            rust_files: Vec::new(),
            toml_files: Vec::new(),
        })
    }
}

struct Visitor<'s> {
    scope: &'s WalkScope,
    collected: &'s Mutex<Vec<(Vec<PathBuf>, Vec<PathBuf>)>>,
    failures: &'s Mutex<Vec<Error>>,
    rust_files: Vec<PathBuf>,
    toml_files: Vec<PathBuf>,
}

impl ParallelVisitor for Visitor<'_> {
    fn visit(&mut self, entry: std::result::Result<DirEntry, ignore::Error>) -> WalkState {
        let entry = match entry {
            Ok(entry) => entry,
            // A partial error is one unusable line in an otherwise valid ignore
            // file. Aborting over it would make a typo in someone's .gitignore
            // render the whole tree unformattable.
            Err(err) if err.is_partial() => return WalkState::Continue,
            // One unreadable subtree is reported and skipped. Aborting the walk
            // would make a single permission-denied directory hide every file
            // in the repository.
            Err(err) => {
                let path = walk_error_path(&err)
                    .map_or_else(|| self.scope.root.clone(), Path::to_path_buf);
                let source = walk_io_error(&err);
                self.failures
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Error::io(path, source));
                return WalkState::Continue;
            }
        };

        let kind = entry.file_type();
        if kind.is_some_and(|kind| kind.is_dir()) {
            if entry.depth() > 0 && self.scope.prunes_dir(entry.path(), entry.file_name()) {
                return WalkState::Skip;
            }
            return WalkState::Continue;
        }
        if kind.is_some_and(|kind| kind.is_symlink()) || !kind.is_some_and(|kind| kind.is_file()) {
            return WalkState::Continue;
        }

        match self.scope.classify(&entry) {
            Some(Kind::Rust) => self.rust_files.push(entry.into_path()),
            Some(Kind::Toml) => self.toml_files.push(entry.into_path()),
            None => {}
        }
        WalkState::Continue
    }
}

impl Drop for Visitor<'_> {
    fn drop(&mut self) {
        if self.rust_files.is_empty() && self.toml_files.is_empty() {
            return;
        }
        // A panicking visitor would poison this lock; recovering keeps the
        // panic itself as the reported failure instead of a double panic.
        self.collected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((
                std::mem::take(&mut self.rust_files),
                std::mem::take(&mut self.toml_files),
            ));
    }
}

/// The failing path is buried under whichever wrappers `ignore` added; without
/// it the report names the walk root and says nothing about what went wrong.
fn walk_error_path(err: &ignore::Error) -> Option<&Path> {
    match err {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::Loop { child, .. } => Some(child),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            walk_error_path(err)
        }
        _ => None,
    }
}

/// `ignore` wraps the original `io::Error` when it has one; the kind is what
/// tells a permission failure from a vanished directory downstream.
fn walk_io_error(err: &ignore::Error) -> io::Error {
    match err.io_error() {
        Some(source) => io::Error::new(source.kind(), err.to_string()),
        None => io::Error::other(err.to_string()),
    }
}

fn is_dot_name(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    /// `detect_target` simplifies the paths it returns, so expectations have to
    /// be built the same way or every assertion fails on Windows.
    fn canonical(path: &Path) -> PathBuf {
        simplify_path(path.canonicalize().unwrap())
    }

    #[cfg(windows)]
    #[test]
    fn simplify_path_strips_the_verbatim_disk_prefix() {
        assert_eq!(
            simplify_path(PathBuf::from(r"\\?\C:\Users\runner\p\Cargo.toml")),
            PathBuf::from(r"C:\Users\runner\p\Cargo.toml")
        );
        // A UNC share has no plain form, so it keeps the verbatim prefix.
        assert_eq!(
            simplify_path(PathBuf::from(r"\\?\UNC\server\share\x")),
            PathBuf::from(r"\\?\UNC\server\share\x")
        );
    }

    #[test]
    fn test_detect_cargo_project() {
        let temp = tempdir().unwrap();
        let cargo_toml = temp.path().join("Cargo.toml");
        fs::write(
            &cargo_toml,
            "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let src = temp.path().join("src");
        fs::create_dir(&src).unwrap();
        let main_rs = src.join("main.rs");
        fs::write(&main_rs, "fn main() {}\n").unwrap();

        let detected = detect_target(temp.path()).unwrap();
        match detected {
            TargetKind::CargoProject { manifest_path, .. } => {
                assert_eq!(manifest_path, canonical(&cargo_toml));
            }
            _ => panic!("Expected CargoProject"),
        }
    }

    #[test]
    fn test_detect_loose_files() {
        let temp = tempdir().unwrap();
        let file1 = temp.path().join("script.rs");
        fs::write(&file1, "fn main() {}\n").unwrap();

        let detected = detect_target(temp.path()).unwrap();
        match detected {
            TargetKind::LooseDirectory { files, .. } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0], canonical(&file1));
            }
            _ => panic!("Expected LooseDirectory"),
        }
    }

    #[test]
    fn test_detect_loose_rs_and_toml() {
        let temp = tempdir().unwrap();
        let rust = temp.path().join("script.rs");
        let toml = temp.path().join("extra.toml");
        fs::write(&rust, "fn main() {}\n").unwrap();
        fs::write(&toml, "a = 1\n").unwrap();

        let detected = detect_target(temp.path()).unwrap();
        match detected {
            TargetKind::LooseDirectory {
                files, toml_files, ..
            } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0], canonical(&rust));
                assert_eq!(toml_files.len(), 1);
                assert_eq!(toml_files[0], canonical(&toml));
            }
            _ => panic!("Expected LooseDirectory"),
        }
    }

    #[test]
    fn test_detect_single_file() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();

        let detected = detect_target(&file).unwrap();
        match detected {
            TargetKind::SingleFile(path) => {
                assert_eq!(path, canonical(&file));
            }
            _ => panic!("Expected SingleFile"),
        }
    }

    #[test]
    fn test_collect_rust_files_pruning_skips_nested_package() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("root.rs"), "fn root() {}\n").unwrap();
        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("child.rs"), "fn child() {}\n").unwrap();

        let files = collect_rust_files_pruning(root, &[nested]).unwrap();
        assert_eq!(files, vec![root.join("root.rs")]);
    }

    /// Cargo stamps a build directory with `CACHEDIR.TAG`, and that tag is what
    /// distinguishes it from a source directory that happens to be named
    /// `target`.
    #[test]
    fn test_target_dir_is_pruned_even_when_not_ignored() {
        let temp = tempdir().unwrap();
        let kept = temp.path().join("keep.rs");
        fs::write(&kept, "fn main() {}\n").unwrap();
        let target = temp.path().join("target");
        let build_dir = target.join("debug");
        fs::create_dir_all(&build_dir).unwrap();
        fs::write(
            target.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        fs::write(build_dir.join("generated.rs"), "fn gen() {}\n").unwrap();

        let files = collect_rust_files(temp.path()).unwrap();
        assert_eq!(files, vec![kept]);
    }

    #[test]
    fn a_target_dir_beside_a_manifest_is_pruned_without_a_tag() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let build_dir = temp.path().join("target").join("debug");
        fs::create_dir_all(&build_dir).unwrap();
        fs::write(build_dir.join("generated.rs"), "fn gen() {}\n").unwrap();

        assert!(collect_rust_files(temp.path()).unwrap().is_empty());
    }

    /// The whole point of judging a build directory by evidence: a module named
    /// `target` is ordinary source and used to be skipped.
    #[test]
    fn a_source_module_named_target_is_formatted() {
        let temp = tempdir().unwrap();
        let module = temp.path().join("src").join("target");
        fs::create_dir_all(&module).unwrap();
        let source = module.join("mod.rs");
        fs::write(&source, "pub fn t() {}\n").unwrap();

        assert_eq!(collect_rust_files(temp.path()).unwrap(), vec![source]);
    }

    #[test]
    fn a_renamed_build_dir_is_pruned_by_its_tag() {
        let temp = tempdir().unwrap();
        let kept = temp.path().join("keep.rs");
        fs::write(&kept, "fn main() {}\n").unwrap();
        let build = temp.path().join("build-output");
        fs::create_dir_all(&build).unwrap();
        fs::write(
            build.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        fs::write(build.join("generated.rs"), "fn gen() {}\n").unwrap();

        assert_eq!(collect_rust_files(temp.path()).unwrap(), vec![kept]);
    }

    #[test]
    fn test_collect_skips_cargo_lock_and_unwanted_kinds() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("Cargo.lock"), "version = 4\n").unwrap();
        fs::write(
            temp.path().join("clippy.toml"),
            "avoid-breaking-exported-api = false\n",
        )
        .unwrap();
        fs::write(temp.path().join("rustfmt.toml"), "max_width=80\n").unwrap();
        fs::write(temp.path().join("a.toml"), "a = 1\n").unwrap();
        fs::write(temp.path().join("a.rs"), "fn main() {}\n").unwrap();

        let toml_files = collect_toml_files(temp.path()).unwrap();
        assert_eq!(toml_files, vec![temp.path().join("a.toml")]);

        let rust_files = collect_rust_files(temp.path()).unwrap();
        assert_eq!(rust_files, vec![temp.path().join("a.rs")]);
    }

    #[test]
    fn the_manifest_search_stops_below_home() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let scratch = home.join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(home.join("Cargo.toml"), "[package]\nname = \"stray\"\n").unwrap();

        let bounded = Boundary::with_home(Some(home.clone()), false);
        assert_eq!(find_cargo_manifest_within(&scratch, &bounded), None);

        // The start directory itself is exempt, so working in `$HOME` still works.
        assert_eq!(
            find_cargo_manifest_within(&home, &bounded),
            Some(home.join("Cargo.toml"))
        );
    }

    #[test]
    fn the_manifest_search_stops_at_a_git_root() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"outer\"\n",
        )
        .unwrap();
        let inner = temp.path().join("inner").join("src");
        fs::create_dir_all(&inner).unwrap();
        fs::create_dir_all(temp.path().join("inner").join(".git")).unwrap();

        assert_eq!(
            find_cargo_manifest_within(&inner, &Boundary::with_home(None, true)),
            None
        );
        // Without the git boundary the same layout still resolves upward, which
        // is what workspace resolution needs.
        assert_eq!(
            find_cargo_manifest_within(&inner, &Boundary::with_home(None, false)),
            Some(temp.path().join("Cargo.toml"))
        );
    }

    /// A submodule spells `.git` as a file, and the manifest inside it must
    /// still be found before the boundary stops the walk.
    #[test]
    fn a_manifest_beside_a_git_file_is_found() {
        let temp = tempdir().unwrap();
        let sub = temp.path().join("sub");
        fs::create_dir_all(sub.join("src")).unwrap();
        fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();
        let manifest = sub.join("Cargo.toml");
        fs::write(&manifest, "[package]\nname = \"sub\"\n").unwrap();

        assert_eq!(
            find_cargo_manifest_within(&sub.join("src"), &Boundary::with_home(None, true)),
            Some(manifest)
        );
    }

    #[test]
    fn the_ancestor_walk_is_depth_capped() {
        let mut deep = PathBuf::from("/");
        for level in 0..MAX_ANCESTOR_DEPTH + 8 {
            deep.push(format!("d{level}"));
        }
        let boundary = Boundary::with_home(None, false);
        let visited: Vec<_> = bounded_ancestors(&deep, &boundary).collect();
        assert_eq!(visited.len(), MAX_ANCESTOR_DEPTH);
    }

    /// `build_parallel` visits in an unspecified order, so the sort afterwards
    /// is the only thing making a run reproducible.
    #[test]
    fn a_parallel_walk_agrees_with_a_serial_one() {
        let temp = tempdir().unwrap();
        for dir in 0..12 {
            let nested = temp.path().join(format!("d{dir}")).join("inner");
            fs::create_dir_all(&nested).unwrap();
            for file in 0..12 {
                fs::write(nested.join(format!("f{file}.rs")), "fn f() {}\n").unwrap();
            }
        }

        let serial = collect(
            temp.path(),
            &Selector::permissive().with_threads(1),
            Languages::Rust,
            &[],
            true,
        )
        .unwrap();
        let parallel = collect(
            temp.path(),
            &Selector::permissive().with_threads(8),
            Languages::Rust,
            &[],
            true,
        )
        .unwrap();

        assert_eq!(serial.rust_files.len(), 144);
        assert_eq!(serial.rust_files, parallel.rust_files);
    }

    #[test]
    fn test_skipped_toml_names() {
        let filter = Selector::permissive()
            .filter_for(Path::new("/tmp"))
            .unwrap();
        for name in crate::selection::DEFAULT_TOML_SKIPS {
            assert!(
                filter.skips_toml(Path::new("/tmp").join(name).as_path()),
                "expected skip for {name}"
            );
        }
        assert!(!filter.skips_toml(Path::new("/tmp/deny.toml")));
    }

    #[test]
    fn test_workspace_root_standalone_package() {
        let temp = tempdir().unwrap();
        let manifest = temp.path().join("Cargo.toml");
        fs::write(
            &manifest,
            r#"[package]
name = "solo"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        assert_eq!(workspace_root(&manifest), temp.path());
    }

    #[test]
    fn test_workspace_root_from_virtual_member() {
        let temp = tempdir().unwrap();
        let root_manifest = temp.path().join("Cargo.toml");
        fs::write(
            &root_manifest,
            r#"[workspace]
members = ["crates/foo", "crates/bar"]
"#,
        )
        .unwrap();

        let foo_dir = temp.path().join("crates").join("foo");
        fs::create_dir_all(&foo_dir).unwrap();
        let foo_manifest = foo_dir.join("Cargo.toml");
        fs::write(
            &foo_manifest,
            r#"[package]
name = "foo"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

        assert_eq!(workspace_root(&foo_manifest), temp.path());
        assert_eq!(workspace_root(&root_manifest), temp.path());
    }

    fn write_package(dir: &Path, name: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let manifest = dir.join("Cargo.toml");
        fs::write(
            &manifest,
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        manifest
    }

    #[test]
    fn test_workspace_root_for_member() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            r#"[workspace]
members = ["crates/foo"]
"#,
        )
        .unwrap();
        let foo_manifest = write_package(&temp.path().join("crates").join("foo"), "foo");

        assert_eq!(workspace_root(&foo_manifest), temp.path());
    }

    #[test]
    fn test_workspace_root_for_non_member_is_the_package() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            r#"[workspace]
members = ["crates/foo"]
"#,
        )
        .unwrap();
        write_package(&temp.path().join("crates").join("foo"), "foo");
        let scratch_manifest = write_package(&temp.path().join("scratch"), "scratch");

        assert_eq!(
            workspace_root(&scratch_manifest),
            temp.path().join("scratch"),
            "a non-member must not resolve to the ancestor workspace"
        );
    }

    #[test]
    fn test_workspace_root_for_glob_member() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            r#"[workspace]
members = ["crates/*"]
"#,
        )
        .unwrap();
        let foo_manifest = write_package(&temp.path().join("crates").join("foo"), "foo");

        assert_eq!(workspace_root(&foo_manifest), temp.path());
    }

    #[cfg(unix)]
    #[test]
    fn test_walk_error_is_not_swallowed() {
        use std::os::unix::fs::PermissionsExt;

        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }

        let temp = tempdir().unwrap();
        let kept = temp.path().join("keep.rs");
        fs::write(&kept, "fn main() {}\n").unwrap();

        let blocked = temp.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("dirty.rs"), "fn dirty(){}\n").unwrap();

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        let _guard = Restore(blocked.clone());

        let result = collect_rust_files(temp.path());
        assert!(
            result.is_err(),
            "expected walk error, got Ok({:?})",
            result.ok()
        );

        // The failure names the directory that could not be read, and the walk
        // keeps going: one locked subtree must not hide the rest of the tree.
        let walk = collect(
            temp.path(),
            &Selector::permissive(),
            Languages::Rust,
            &[],
            true,
        )
        .unwrap();
        assert_eq!(walk.rust_files, vec![kept]);
        assert_eq!(walk.errors.len(), 1);
        assert!(
            walk.errors[0].to_string().contains("blocked"),
            "expected the blocked directory to be named, got {}",
            walk.errors[0]
        );
    }

    /// `members = ["../pkg"]` is legal cargo, and `Path::join` leaves the `..`
    /// in place, so an unnormalized comparison can never match.
    #[test]
    fn a_member_outside_the_workspace_root_is_recognized() {
        let root = Path::new("/ws/inner");
        assert!(member_pattern_matches(root, Path::new("/ws/pkg"), "../pkg"));
        assert!(member_pattern_matches(
            root,
            Path::new("/ws/pkgs/one"),
            "../pkgs/*"
        ));
        assert!(!member_pattern_matches(
            root,
            Path::new("/ws/pkgs/one/two"),
            "../pkgs/*"
        ));
        assert!(!member_pattern_matches(
            root,
            Path::new("/ws/other"),
            "../pkg"
        ));
        assert!(member_pattern_matches(
            root,
            Path::new("/ws/inner/here"),
            "../inner/here"
        ));
    }

    /// A directory really named `a*` is a literal path segment, not a glob.
    #[test]
    fn a_glob_character_in_the_workspace_path_is_not_reinterpreted() {
        let root = Path::new("/ws/a*");
        assert!(member_pattern_matches(root, Path::new("/ws/a*/one"), "one"));
        assert!(!member_pattern_matches(
            root,
            Path::new("/ws/ab/one"),
            "one"
        ));
    }

    #[test]
    fn a_lexical_normalization_never_climbs_past_the_root() {
        assert_eq!(
            normalize_lexically(Path::new("/a/../..")),
            PathBuf::from("/")
        );
        assert_eq!(
            normalize_lexically(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(
            normalize_lexically(Path::new("../../x")),
            PathBuf::from("../../x")
        );
    }

    #[test]
    fn member_globs_match_nested_and_braces() {
        let root = Path::new("/ws");
        assert!(member_pattern_matches(
            root,
            &root.join("crates/foo"),
            "crates/*"
        ));
        assert!(member_pattern_matches(
            root,
            &root.join("crates/foo/bar"),
            "crates/**"
        ));
        assert!(!member_pattern_matches(
            root,
            &root.join("crates/foo/bar"),
            "crates/*"
        ));
        assert!(member_pattern_matches(root, &root.join("a/b"), "*/*"));
        assert!(member_pattern_matches(
            root,
            &root.join("crates/foo"),
            "crates/{foo,bar}"
        ));
        assert!(member_pattern_matches(
            root,
            &root.join("crates/bar"),
            "crates/{foo,bar}"
        ));
        assert!(!member_pattern_matches(
            root,
            &root.join("crates/baz"),
            "crates/{foo,bar}"
        ));
    }

    #[test]
    fn workspace_root_follows_package_workspace_path() {
        let temp = tempdir().unwrap();
        let ws = temp.path().join("ws");
        let pkg = temp.path().join("pkg");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            ws.join("Cargo.toml"),
            "[workspace]\nmembers = [\"../pkg\"]\n",
        )
        .unwrap();
        let manifest = pkg.join("Cargo.toml");
        fs::write(
            &manifest,
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\nworkspace = \"../ws\"\n",
        )
        .unwrap();
        assert_eq!(workspace_root(&manifest), canonical(&ws));
    }

    #[test]
    fn package_workspace_true_is_not_a_path() {
        let temp = tempdir().unwrap();
        let manifest = temp.path().join("Cargo.toml");
        fs::write(
            &manifest,
            "[package]\nname = \"pkg\"\nversion.workspace = true\n",
        )
        .unwrap();
        assert_eq!(workspace_root(&manifest), temp.path());
    }

    #[test]
    fn vendor_and_checksum_dirs_are_pruned() {
        let temp = tempdir().unwrap();
        let keep = temp.path().join("keep.toml");
        fs::write(&keep, "a = 1\n").unwrap();

        let vendor = temp.path().join("vendor").join("foo");
        fs::create_dir_all(&vendor).unwrap();
        fs::write(vendor.join(".cargo-checksum.json"), "{}").unwrap();
        fs::write(vendor.join("Cargo.toml"), "x=1\n").unwrap();

        let renamed = temp.path().join("third-party").join("bar");
        fs::create_dir_all(&renamed).unwrap();
        fs::write(renamed.join(".cargo-checksum.json"), "{}").unwrap();
        fs::write(renamed.join("Cargo.toml"), "y=1\n").unwrap();

        let files = collect_toml_files(temp.path()).unwrap();
        assert_eq!(files, vec![keep]);
    }

    #[test]
    fn hidden_config_dirs_are_walked() {
        let temp = tempdir().unwrap();
        let cargo_dir = temp.path().join(".cargo");
        let config_dir = temp.path().join(".config");
        fs::create_dir(&cargo_dir).unwrap();
        fs::create_dir(&config_dir).unwrap();
        let cargo_toml = cargo_dir.join("config.toml");
        let nextest = config_dir.join("nextest.toml");
        let secret = temp.path().join(".secret.toml");
        fs::write(&cargo_toml, "x = 1\n").unwrap();
        fs::write(&nextest, "y = 1\n").unwrap();
        fs::write(&secret, "z = 1\n").unwrap();

        let files = collect_toml_files(temp.path()).unwrap();
        assert!(files.contains(&cargo_toml));
        assert!(files.contains(&nextest));
        assert!(!files.contains(&secret));
    }

    #[test]
    fn nested_cargo_config_is_found() {
        let temp = tempdir().unwrap();
        let nested = temp.path().join("crates").join("foo").join(".cargo");
        fs::create_dir_all(&nested).unwrap();
        let config = nested.join("config.toml");
        fs::write(&config, "x = 1\n").unwrap();
        let files = collect_toml_files(temp.path()).unwrap();
        assert_eq!(files, vec![config]);
    }

    #[test]
    fn gitignore_applies_without_git_dir() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join(".gitignore"), "skip.toml\n").unwrap();
        let keep = temp.path().join("keep.toml");
        let skip = temp.path().join("skip.toml");
        fs::write(&keep, "a = 1\n").unwrap();
        fs::write(&skip, "b = 1\n").unwrap();
        let files = collect_toml_files(temp.path()).unwrap();
        assert_eq!(files, vec![keep]);
    }

    #[test]
    fn toml_scope_skips_non_member_package() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let member = root.join("crates").join("foo");
        let excluded = root.join("crates").join("local");
        fs::create_dir_all(&member).unwrap();
        fs::create_dir_all(&excluded).unwrap();
        let root_toml = root.join("deny.toml");
        let member_toml = member.join("Cargo.toml");
        let excluded_toml = excluded.join("Cargo.toml");
        fs::write(&root_toml, "a = 1\n").unwrap();
        fs::write(&member_toml, "[package]\nname = \"foo\"\n").unwrap();
        fs::write(&excluded_toml, "[package]\nname = \"local\"\n").unwrap();

        let members = vec![member.clone()];
        assert!(file_in_workspace_scope(&root_toml, root, &members));
        assert!(file_in_workspace_scope(&member_toml, root, &members));
        assert!(!file_in_workspace_scope(&excluded_toml, root, &members));
    }

    #[test]
    fn workspace_member_dirs_skips_excluded_and_nested_packages() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/local\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        let member = root.join("crates").join("foo");
        let excluded = root.join("crates").join("local");
        let nested = root.join("crates").join("foo").join("nested");
        fs::create_dir_all(&member).unwrap();
        fs::create_dir_all(&excluded).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::write(member.join("Cargo.toml"), "[package]\nname = \"foo\"\n").unwrap();
        fs::write(excluded.join("Cargo.toml"), "[package]\nname = \"local\"\n").unwrap();
        fs::write(nested.join("Cargo.toml"), "[package]\nname = \"nested\"\n").unwrap();

        let members = workspace_member_dirs(root);
        assert!(members.contains(&canonical(&member)), "{members:?}");
        assert!(!members.contains(&canonical(&excluded)), "{members:?}");
        assert!(!members.contains(&canonical(&nested)), "{members:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_walk_does_not_follow_a_file_symlink() {
        let temp = tempdir().unwrap();
        let outside = temp.path().join("outside");
        let tree = temp.path().join("tree");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&tree).unwrap();
        let real = outside.join("real.toml");
        let link = tree.join("link.toml");
        let kept = tree.join("kept.toml");
        fs::write(&real, "a = 1\n").unwrap();
        fs::write(&kept, "b = 2\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let files = collect_toml_files(&tree).unwrap();
        assert_eq!(files, vec![kept]);
        assert!(!files.contains(&link));
    }
}
