use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::OnceLock,
};

use crate::{
    detector::{decode_path, normalize_lexically, simplify_path},
    error::{Error, Result},
};

/// Deep enough for any real superproject, shallow enough that a submodule that
/// somehow points at an ancestor cannot spin.
const MAX_SUBMODULE_DEPTH: usize = 32;

/// `git add` is chunked by both counts, because a command line is bounded in
/// bytes and a staged set is bounded in entries.
const MAX_PATHSPECS: usize = 500;
const MAX_PATHSPEC_BYTES: usize = 48 * 1024;

/// Only Added/Copied/Modified/Renamed/Typechanged can be formatted. `T` matters
/// because a path that swapped between a symlink and a regular file is a
/// content change like any other; `D` and `U` cannot be.
const DIFF_FILTER: &str = "--diff-filter=ACMRT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitScope {
    /// Everything that differs from the merge base of `<ref>` and `HEAD`.
    Since(String),
    /// Everything staged in the index.
    Staged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSelection {
    pub scope: GitScope,
    /// `--since` only. Untracked files differ from every ref, so they are not
    /// selected by the ref at all; `--no-untracked` leaves them alone.
    pub untracked: bool,
    pub recurse_submodules: bool,
}

impl GitSelection {
    pub fn new(scope: GitScope) -> Self {
        Self {
            scope,
            untracked: true,
            recurse_submodules: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct Changed {
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
    /// Selected paths that are staged and whose working tree differs from what
    /// is staged. The formatter rewrites the working tree, so these are the
    /// paths where the bytes it formats are not the bytes that would be
    /// committed.
    pub drifted: Vec<PathBuf>,
    pub warnings: Vec<String>,
    /// Repository roots owning `files`: the superproject first, then every
    /// submodule that was recursed into. `restage` needs this to write to the
    /// index that actually holds each path.
    pub repos: Vec<PathBuf>,
}

/// What `--restage` needs after the run: which index owns each formatted path,
/// and which paths must be left out of it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GitPlan {
    pub repos: Vec<PathBuf>,
    pub drifted: Vec<PathBuf>,
}

impl From<&Changed> for GitPlan {
    fn from(changed: &Changed) -> Self {
        Self {
            repos: changed.repos.clone(),
            drifted: changed.drifted.clone(),
        }
    }
}

/// The base a repository is compared against. `Since` collapses to a resolved
/// commit before recursion, because a submodule's fork point is the commit the
/// superproject recorded, not a merge base of its own.
#[derive(Debug, Clone)]
enum RepoScope {
    Staged,
    Diff(String),
}

pub fn changed_files(selection: &GitSelection, cwd: &Path) -> Result<Changed> {
    let root = repository_root(cwd)?;
    let git = Git::inherited(&root);
    let mut changed = Changed {
        root: root.clone(),
        ..Changed::default()
    };

    if !inside(cwd, &root) {
        changed.warnings.push(format!(
            "warning: git reports the repository root as {}, which does not contain the current \
             directory; GIT_DIR or GIT_WORK_TREE is set in the environment",
            root.display()
        ));
    }

    let scope = match &selection.scope {
        GitScope::Staged => RepoScope::Staged,
        GitScope::Since(reference) => RepoScope::Diff(base_for(git, reference, &mut changed)?),
    };

    let mut visited = HashSet::new();
    visited.insert(root.clone());
    collect_repo(git, &scope, selection, 0, &mut visited, &mut changed)?;

    changed.files.sort_unstable();
    changed.files.dedup();
    changed.drifted.sort_unstable();
    changed.drifted.dedup();
    Ok(changed)
}

pub fn repository_root(cwd: &Path) -> Result<PathBuf> {
    let output = Git::inherited(cwd).run(&["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        return Err(Error::NotAGitRepository(cwd.to_path_buf()));
    }
    let Some(root) = decode_path(trim_ascii(&output.stdout)) else {
        return Err(Error::NotAGitRepository(cwd.to_path_buf()));
    };
    if root.as_os_str().is_empty() {
        return Err(Error::NotAGitRepository(cwd.to_path_buf()));
    }
    Ok(simplify_path(root.canonicalize().unwrap_or(root)))
}

/// Where this repository keeps its hooks.
///
/// `rev-parse --git-path hooks` is the only spelling that answers for every
/// layout at once: it honours `core.hooksPath`, and it resolves to the *common*
/// git directory, so a linked worktree gets the hooks the whole repository
/// shares rather than a directory of its own that git would never run. The path
/// comes back relative to the directory git was asked in, so it is joined back
/// onto it.
pub fn hooks_dir(cwd: &Path) -> Result<PathBuf> {
    let output = Git::inherited(cwd).run(&["rev-parse", "--git-path", "hooks"])?;
    if !output.status.success() {
        return Err(Error::NotAGitRepository(cwd.to_path_buf()));
    }
    let Some(hooks) = decode_path(trim_ascii(&output.stdout)) else {
        return Err(Error::NotAGitRepository(cwd.to_path_buf()));
    };
    if hooks.as_os_str().is_empty() {
        return Err(Error::NotAGitRepository(cwd.to_path_buf()));
    }
    if hooks.is_absolute() {
        return Ok(simplify_path(hooks));
    }
    // The hooks directory need not exist yet -- `core.hooksPath` can name one
    // this call is about to create -- so only `cwd` is canonicalized and the
    // `..` a subdirectory prints is resolved lexically.
    let base = cwd.canonicalize().map_err(|err| Error::io(cwd, err))?;
    Ok(simplify_path(normalize_lexically(&base.join(hooks))))
}

/// Re-add the formatted paths to the index they came from, so a pre-commit hook
/// commits what was formatted. A path whose working tree already differed from
/// the index is left alone: staging it would also stage the edits the user
/// deliberately kept out of the commit.
pub fn restage<'a>(plan: &GitPlan, paths: impl Iterator<Item = &'a Path>) -> Result<Vec<String>> {
    let drifted: HashSet<&Path> = plan.drifted.iter().map(PathBuf::as_path).collect();
    let mut warnings = Vec::new();
    let mut by_repo: HashMap<&Path, Vec<OsString>> = HashMap::new();

    for path in paths {
        if drifted.contains(path) {
            warnings.push(format!(
                "warning: {} was not re-added to the index; staging it would also stage its \
                 unstaged changes",
                path.display()
            ));
            continue;
        }
        let Some(root) = owning_repo(plan, path) else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        by_repo.entry(root).or_default().push(pathspec(relative));
    }

    let superproject = plan.repos.first().map(PathBuf::as_path);
    let mut roots: Vec<&Path> = by_repo.keys().copied().collect();
    roots.sort_unstable();
    for root in roots {
        let git = if superproject == Some(root) {
            Git::inherited(root)
        } else {
            Git::isolated(root)?
        };
        let mut specs = by_repo.remove(root).unwrap_or_default();
        specs.sort_unstable();
        for chunk in chunked(&specs) {
            let mut args: Vec<OsString> = vec![OsString::from("add"), OsString::from("--")];
            args.extend(chunk.iter().cloned());
            git.checked(&args)?;
        }
    }

    warnings.sort_unstable();
    Ok(warnings)
}

fn owning_repo<'p>(plan: &'p GitPlan, path: &Path) -> Option<&'p Path> {
    plan.repos
        .iter()
        .map(PathBuf::as_path)
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.as_os_str().len())
}

/// `:(literal)` is what keeps a file really named `a[1].rs` from being read as a
/// glob, whatever `GIT_GLOB_PATHSPECS` the surrounding hook set.
fn pathspec(relative: &Path) -> OsString {
    let mut spec = OsString::from(":(literal)");
    spec.push(relative.as_os_str());
    spec
}

fn chunked(specs: &[OsString]) -> Vec<&[OsString]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (index, spec) in specs.iter().enumerate() {
        let len = spec.len() + 1;
        if index > start && (index - start >= MAX_PATHSPECS || bytes + len > MAX_PATHSPEC_BYTES) {
            chunks.push(&specs[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += len;
    }
    if start < specs.len() {
        chunks.push(&specs[start..]);
    }
    chunks
}

fn base_for(git: Git<'_>, reference: &str, changed: &mut Changed) -> Result<String> {
    let verified = git.run(&[
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        reference,
    ])?;
    let Some(object) = verified
        .status
        .success()
        .then(|| text(&verified.stdout))
        .flatten()
        .filter(|object| !object.is_empty())
    else {
        return Err(Error::UnknownGitRef(reference.to_string()));
    };

    let merge_base = git.run(&["merge-base", "--end-of-options", &object, "HEAD"])?;
    if merge_base.status.success()
        && let Some(base) = text(&merge_base.stdout)
        && !base.is_empty()
    {
        return Ok(base);
    }

    changed.warnings.push(format!(
        "warning: no merge base between {reference} and HEAD, so the comparison is against \
         {reference} itself; a shallow clone has no fork point to find"
    ));
    Ok(object)
}

fn collect_repo(
    git: Git<'_>,
    scope: &RepoScope,
    selection: &GitSelection,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    changed: &mut Changed,
) -> Result<()> {
    changed.repos.push(git.cwd.to_path_buf());

    let mut files = match scope {
        RepoScope::Staged => staged_names(git, &mut changed.warnings)?,
        RepoScope::Diff(base) => {
            let mut files = paths_from(
                git,
                &[
                    "diff",
                    "--name-only",
                    "-z",
                    DIFF_FILTER,
                    "--end-of-options",
                    base,
                ],
                &mut changed.warnings,
            )?;
            if selection.untracked {
                files.extend(paths_from(
                    git,
                    &["ls-files", "--others", "--exclude-standard", "-z"],
                    &mut changed.warnings,
                )?);
            }
            files
        }
    };
    files.sort_unstable();
    files.dedup();

    if !files.is_empty() {
        let unstaged: HashSet<PathBuf> = paths_from(
            git,
            &["diff", "--name-only", "-z", DIFF_FILTER],
            &mut changed.warnings,
        )?
        .into_iter()
        .collect();
        if !unstaged.is_empty() {
            // Under `--staged` the selection already is the staged set, so
            // there is nothing more to ask git for.
            let staged: HashSet<PathBuf> = match scope {
                RepoScope::Staged => files.iter().cloned().collect(),
                RepoScope::Diff(_) => staged_names(git, &mut changed.warnings)?
                    .into_iter()
                    .collect(),
            };
            changed.drifted.extend(
                files
                    .iter()
                    .filter(|path| unstaged.contains(*path) && staged.contains(*path))
                    .cloned(),
            );
        }
    }

    changed.files.extend(files);

    if selection.recurse_submodules && depth < MAX_SUBMODULE_DEPTH {
        recurse_submodules(git, scope, selection, depth, visited, changed)?;
    }
    Ok(())
}

fn recurse_submodules(
    git: Git<'_>,
    scope: &RepoScope,
    selection: &GitSelection,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    changed: &mut Changed,
) -> Result<()> {
    for relative in gitlinks(git, &mut changed.warnings)? {
        let path = git.cwd.join(&relative);
        if !path.join(".git").exists() {
            continue;
        }
        let Ok(sub_root) = path.canonicalize().map(simplify_path) else {
            continue;
        };
        if !visited.insert(sub_root.clone()) {
            continue;
        }
        let submodule = Git::isolated(&sub_root)?;

        let sub_scope = match scope {
            RepoScope::Staged => RepoScope::Staged,
            RepoScope::Diff(base) => {
                let mut spec = OsString::from(base);
                spec.push(":");
                spec.push(relative.as_os_str());
                if let Some(commit) = resolve_in(git, submodule, &spec)? {
                    RepoScope::Diff(commit)
                } else {
                    changed.warnings.push(format!(
                        "warning: skipped submodule {}: the commit it is compared against is \
                         not available",
                        sub_root.display()
                    ));
                    continue;
                }
            }
        };

        collect_repo(
            submodule,
            &sub_scope,
            selection,
            depth + 1,
            visited,
            changed,
        )?;
    }
    Ok(())
}

fn resolve_in(superproject: Git<'_>, submodule: Git<'_>, spec: &OsStr) -> Result<Option<String>> {
    let recorded = superproject.run(&[
        OsStr::new("rev-parse"),
        OsStr::new("--verify"),
        OsStr::new("--quiet"),
        spec,
    ])?;
    if !recorded.status.success() {
        return Ok(None);
    }
    let Some(commit) = text(&recorded.stdout).filter(|commit| !commit.is_empty()) else {
        return Ok(None);
    };
    let present = submodule.run(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{commit}^{{commit}}"),
    ])?;
    Ok(present.status.success().then_some(commit))
}

fn gitlinks(git: Git<'_>, warnings: &mut Vec<String>) -> Result<Vec<PathBuf>> {
    let stdout = git.checked(&["ls-files", "--stage", "-z"])?;
    let mut links = Vec::new();
    for entry in stdout.split(|byte| *byte == 0).filter(|e| !e.is_empty()) {
        if !entry.starts_with(b"160000 ") {
            continue;
        }
        let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        match decode_path(&entry[tab + 1..]) {
            Some(path) => links.push(path),
            None => warnings.push(undecodable(&entry[tab + 1..])),
        }
    }
    Ok(links)
}

fn staged_names(git: Git<'_>, warnings: &mut Vec<String>) -> Result<Vec<PathBuf>> {
    let mut files = if head_exists(git)? {
        paths_from(
            git,
            &["diff", "--name-only", "-z", "--cached", DIFF_FILTER],
            warnings,
        )?
    } else {
        paths_from(git, &["ls-files", "--cached", "-z"], warnings)?
    };
    files.sort_unstable();
    files.dedup();
    Ok(files)
}

fn head_exists(git: Git<'_>) -> Result<bool> {
    Ok(git
        .run(&["rev-parse", "--verify", "--quiet", "HEAD"])?
        .status
        .success())
}

fn paths_from<S: AsRef<OsStr>>(
    git: Git<'_>,
    args: &[S],
    warnings: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    let root = git.cwd;
    let stdout = git.checked(args)?;
    let mut files = Vec::new();
    for entry in stdout.split(|byte| *byte == 0).filter(|e| !e.is_empty()) {
        let Some(relative) = decode_path(entry) else {
            warnings.push(undecodable(entry));
            continue;
        };
        let path = root.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {}
            Ok(metadata) if metadata.is_file() => match path.canonicalize() {
                Ok(canonical) => files.push(simplify_path(canonical)),
                Err(err) => warnings.push(skipped(&path, &err)),
            },
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => warnings.push(skipped(&path, &err)),
        }
    }
    Ok(files)
}

fn skipped(path: &Path, err: &io::Error) -> String {
    format!("warning: skipped {}: {err}", path.display())
}

fn undecodable(bytes: &[u8]) -> String {
    format!(
        "warning: skipped {}: git reported a path that is not valid UTF-8",
        String::from_utf8_lossy(bytes)
    )
}

fn inside(cwd: &Path, root: &Path) -> bool {
    cwd.canonicalize()
        .map(simplify_path)
        .is_ok_and(|cwd| cwd.starts_with(root))
}

fn text(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(trim_ascii(bytes))
        .ok()
        .map(str::to_string)
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace());
    let end = bytes.iter().rposition(|b| !b.is_ascii_whitespace());
    match (start, end) {
        (Some(start), Some(end)) => &bytes[start..=end],
        _ => &[],
    }
}

#[derive(Debug, Clone, Copy)]
struct Git<'a> {
    cwd: &'a Path,
    removed_env: &'a [OsString],
}

impl<'a> Git<'a> {
    fn inherited(cwd: &'a Path) -> Self {
        Self {
            cwd,
            removed_env: &[],
        }
    }

    fn isolated(cwd: &'a Path) -> Result<Self> {
        Ok(Self {
            cwd,
            removed_env: repository_local_env(cwd)?,
        })
    }

    fn checked<S: AsRef<OsStr>>(self, args: &[S]) -> Result<Vec<u8>> {
        let output = self.run(args)?;
        if !output.status.success() {
            return Err(Error::GitCommandFailed {
                command: describe(args),
                details: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(output.stdout)
    }

    fn run<S: AsRef<OsStr>>(self, args: &[S]) -> Result<Output> {
        let mut command = Command::new("git");
        for name in self.removed_env {
            command.env_remove(name);
        }
        command
            .arg("--no-optional-locks")
            .args(args)
            .current_dir(self.cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => Error::GitNotFound,
                _ => Error::CommandExecutionFailed {
                    command: describe(args),
                    source: err,
                },
            })
    }
}

const SHARED_WITH_SUBMODULES: [&str; 2] = ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"];

fn repository_local_env(cwd: &Path) -> Result<&'static [OsString]> {
    static NAMES: OnceLock<Vec<OsString>> = OnceLock::new();

    if let Some(names) = NAMES.get() {
        return Ok(names);
    }

    let listed = Git::inherited(cwd).checked(&["rev-parse", "--local-env-vars"])?;
    let names = listed
        .split(u8::is_ascii_whitespace)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .filter(|name| !SHARED_WITH_SUBMODULES.contains(&name.as_str()))
        .map(OsString::from)
        .collect();
    Ok(NAMES.get_or_init(|| names))
}

fn describe<S: AsRef<OsStr>>(args: &[S]) -> String {
    let mut command = String::from("git");
    for arg in args {
        command.push(' ');
        command.push_str(&arg.as_ref().to_string_lossy());
    }
    command
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command as StdCommand};

    use super::*;

    fn git(dir: &Path, args: &[&str]) -> bool {
        StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn seeded(dir: &Path) -> bool {
        if !git(dir, &["init", "--quiet", "."]) {
            eprintln!("SKIP: git is unavailable");
            return false;
        }
        git(dir, &["config", "user.email", "test@example.invalid"]);
        git(dir, &["config", "user.name", "test"]);
        true
    }

    #[test]
    fn head_exists_only_after_the_first_commit() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }

        assert!(!head_exists(Git::inherited(dir)).unwrap());

        fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        assert!(git(dir, &["add", "-A"]));
        assert!(git(dir, &["commit", "--quiet", "-m", "seed"]));

        assert!(head_exists(Git::inherited(dir)).unwrap());
    }

    /// A ref that looks like a git switch must still be a ref: without
    /// `--end-of-options`, `rev-parse --verify --quiet --all` lists every object
    /// and `merge-base --dashed HEAD` is `unknown option`.
    #[test]
    fn a_ref_that_looks_like_an_option_is_still_a_ref() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }
        fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        assert!(git(dir, &["add", "-A"]));
        assert!(git(dir, &["commit", "--quiet", "-m", "seed"]));
        let sha = text(
            &Git::inherited(dir)
                .run(&["rev-parse", "--verify", "--quiet", "HEAD"])
                .unwrap()
                .stdout,
        )
        .expect("HEAD");
        assert!(git(dir, &["update-ref", "refs/heads/--foo", &sha]));
        assert!(git(dir, &["update-ref", "refs/heads/-bar", &sha]));

        let mut changed = Changed::default();
        assert_eq!(
            base_for(Git::inherited(dir), "--foo", &mut changed).unwrap(),
            sha
        );
        assert_eq!(
            base_for(Git::inherited(dir), "-bar", &mut changed).unwrap(),
            sha
        );
        assert!(
            matches!(
                base_for(Git::inherited(dir), "--all", &mut changed),
                Err(Error::UnknownGitRef(name)) if name == "--all"
            ),
            "a git switch must not be taken as a revision"
        );
    }

    #[test]
    fn a_ref_that_looks_like_an_option_and_shares_no_history_is_still_diffed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }
        fs::write(dir.join("seed.toml"), "a = 1\n").unwrap();
        commit(dir, "seed");

        let tree = StdCommand::new("git")
            .args(["write-tree"])
            .current_dir(dir)
            .output()
            .unwrap();
        let tree = text(&tree.stdout).expect("a tree id");
        let orphan = StdCommand::new("git")
            .args(["commit-tree", &tree, "-m", "orphan"])
            .current_dir(dir)
            .output()
            .unwrap();
        let orphan = text(&orphan.stdout).expect("a commit id");
        assert!(git(dir, &["update-ref", "refs/heads/--foo", &orphan]));

        fs::write(dir.join("changed.toml"), "b = 2\n").unwrap();
        commit(dir, "change");

        let selection = GitSelection {
            untracked: false,
            ..GitSelection::new(GitScope::Since("--foo".to_string()))
        };
        let changed = changed_files(&selection, dir).unwrap();
        let names: Vec<_> = changed
            .files
            .iter()
            .filter_map(|path| path.file_name())
            .collect();
        let base = base_for(Git::inherited(dir), "--foo", &mut Changed::default()).unwrap();

        assert_eq!(names, ["changed.toml"]);
        assert_eq!(base, orphan);
        assert!(
            changed
                .warnings
                .iter()
                .any(|warning| warning.contains("no merge base")),
            "{:?}",
            changed.warnings
        );
    }

    #[test]
    fn the_repository_local_variables_include_the_index_and_the_git_dir() {
        let temp = tempfile::tempdir().unwrap();

        let Ok(names) = repository_local_env(temp.path()) else {
            eprintln!("SKIP: git is unavailable");
            return;
        };

        assert!(
            names.iter().any(|name| name == "GIT_INDEX_FILE"),
            "{names:?}"
        );
        assert!(names.iter().any(|name| name == "GIT_DIR"), "{names:?}");
        assert!(
            !names
                .iter()
                .any(|name| SHARED_WITH_SUBMODULES.iter().any(|shared| name == *shared)),
            "{names:?}"
        );
    }

    /// The index fallback exists for the unborn-HEAD case alone. Any other git
    /// failure must surface, because taking it would select every tracked file.
    #[test]
    fn a_git_failure_is_not_mistaken_for_an_unborn_head() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }
        fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        fs::write(dir.join("tracked.toml"), "a = 1\n").unwrap();
        assert!(git(dir, &["add", "-A"]));
        assert!(git(dir, &["commit", "--quiet", "-m", "seed"]));

        // A blob is a valid object, so `rev-parse --verify HEAD` succeeds while
        // `diff --cached` rejects it: a real HEAD that cannot be diffed.
        let blob = StdCommand::new("git")
            .args(["hash-object", "-w", "seed.txt"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(blob.status.success());
        assert!(git(dir, &["symbolic-ref", "HEAD", "refs/heads/broken"]));
        fs::write(
            dir.join(".git/refs/heads/broken"),
            String::from_utf8(blob.stdout).unwrap(),
        )
        .unwrap();

        assert!(head_exists(Git::inherited(dir)).unwrap());
        let err = staged_names(Git::inherited(dir), &mut Vec::new()).unwrap_err();
        assert!(
            matches!(err, Error::GitCommandFailed { .. }),
            "expected a git failure, got {err:?}"
        );
    }

    #[test]
    fn a_path_that_cannot_be_read_is_reported_rather_than_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }
        fs::write(dir.join("kept.toml"), "a = 1\n").unwrap();
        fs::write(dir.join("gone.toml"), "b = 2\n").unwrap();
        assert!(git(dir, &["add", "-A"]));

        // A staged path deleted before the walk is a race, not a failure, so it
        // is dropped without a word; everything else is named.
        fs::remove_file(dir.join("gone.toml")).unwrap();
        let mut warnings = Vec::new();
        let files = paths_from(
            Git::inherited(dir),
            &["ls-files", "--cached", "-z"],
            &mut warnings,
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_tracked_symlink_is_not_resolved_to_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }
        fs::write(dir.join("real.toml"), "a = 1\n").unwrap();
        std::os::unix::fs::symlink("real.toml", dir.join("link.toml")).unwrap();
        assert!(git(dir, &["add", "-A"]));

        let mut warnings = Vec::new();
        let files = paths_from(
            Git::inherited(dir),
            &["ls-files", "--cached", "-z"],
            &mut warnings,
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].ends_with("real.toml"), "{}", files[0].display());
    }

    #[test]
    fn pathspecs_chunk_by_count_and_by_bytes() {
        let many: Vec<OsString> = (0..1200)
            .map(|n| pathspec(Path::new(&n.to_string())))
            .collect();
        assert_eq!(chunked(&many).len(), 3);
        assert_eq!(chunked(&many).concat().len(), many.len());

        let long: Vec<OsString> = (0..8)
            .map(|n| pathspec(Path::new(&format!("{}{n}", "x".repeat(10_000)))))
            .collect();
        assert_eq!(chunked(&long).len(), 2);
        assert_eq!(chunked(&long).concat().len(), long.len());
    }

    fn commit(dir: &Path, message: &str) {
        assert!(git(dir, &["add", "-A"]));
        assert!(git(dir, &["commit", "--quiet", "-m", message]));
    }

    fn staged_names_in(dir: &Path) -> Vec<String> {
        let out = StdCommand::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(dir)
            .output()
            .expect("git diff --cached");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    // --------------------------------------------------------------- restage

    #[test]
    fn restage_re_adds_a_formatted_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }
        fs::write(dir.join("a.toml"), "a = 1\n").unwrap();
        commit(dir, "seed");

        fs::write(dir.join("a.toml"), "a = 2\n").unwrap();
        assert!(git(dir, &["add", "-A"]));
        // The formatter rewrote the working tree after it was staged.
        fs::write(dir.join("a.toml"), "a = 3\n").unwrap();

        let root = repository_root(dir).unwrap();
        let plan = GitPlan {
            repos: vec![root.clone()],
            drifted: Vec::new(),
        };
        let file = root.join("a.toml");
        let warnings = restage(&plan, std::iter::once(file.as_path())).unwrap();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(staged_names_in(dir), ["a.toml"]);
        let staged = StdCommand::new("git")
            .args(["show", ":a.toml"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&staged.stdout), "a = 3\n");
    }

    /// The whole point of tracking drift: re-adding a path the user had
    /// deliberately left half-staged would sweep their unstaged edits into the
    /// commit.
    #[test]
    fn restage_skips_a_drifted_path_and_says_why() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }
        fs::write(dir.join("a.toml"), "a = 1\n").unwrap();
        commit(dir, "seed");
        fs::write(dir.join("a.toml"), "a = 2\n").unwrap();

        let root = repository_root(dir).unwrap();
        let file = root.join("a.toml");
        let plan = GitPlan {
            repos: vec![root.clone()],
            drifted: vec![file.clone()],
        };
        let warnings = restage(&plan, std::iter::once(file.as_path())).unwrap();

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("was not re-added to the index"),
            "{warnings:?}"
        );
        assert!(staged_names_in(dir).is_empty());
    }

    #[test]
    fn restage_ignores_a_path_outside_every_repository() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }
        fs::write(dir.join("a.toml"), "a = 1\n").unwrap();
        commit(dir, "seed");

        let root = repository_root(dir).unwrap();
        let plan = GitPlan {
            repos: vec![root],
            drifted: Vec::new(),
        };
        let outside = PathBuf::from("/definitely/not/in/the/repo.toml");
        let warnings = restage(&plan, std::iter::once(outside.as_path())).unwrap();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(staged_names_in(dir).is_empty());
    }

    /// A path under a nested repository belongs to the innermost one. Handing
    /// it to the outer `git add` would fail, because the inner tree is a
    /// gitlink there and not a file.
    #[test]
    fn restage_groups_a_path_under_its_innermost_repository() {
        let temp = tempfile::tempdir().unwrap();
        let outer = temp.path();
        if !seeded(outer) {
            return;
        }
        fs::write(outer.join("outer.toml"), "a = 1\n").unwrap();
        commit(outer, "seed");

        let inner = outer.join("inner");
        fs::create_dir_all(&inner).unwrap();
        if !seeded(&inner) {
            return;
        }
        fs::write(inner.join("inner.toml"), "b = 1\n").unwrap();
        commit(&inner, "seed");

        let outer_root = repository_root(outer).unwrap();
        let inner_root = repository_root(&inner).unwrap();
        let plan = GitPlan {
            repos: vec![outer_root.clone(), inner_root.clone()],
            drifted: Vec::new(),
        };

        fs::write(outer_root.join("outer.toml"), "a = 2\n").unwrap();
        fs::write(inner_root.join("inner.toml"), "b = 2\n").unwrap();
        let outer_file = outer_root.join("outer.toml");
        let inner_file = inner_root.join("inner.toml");
        let warnings = restage(
            &plan,
            [outer_file.as_path(), inner_file.as_path()].into_iter(),
        )
        .unwrap();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(staged_names_in(outer), ["outer.toml"]);
        assert_eq!(staged_names_in(&inner), ["inner.toml"]);
    }

    /// The chunking is exercised at its real boundary rather than only through
    /// `chunked`, so a `git add` argv that grew past the limit would show up
    /// here as a failed command and not as a passing unit test.
    #[test]
    fn restage_crosses_the_pathspec_chunk_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }
        let count = MAX_PATHSPECS + 20;
        for index in 0..count {
            fs::write(dir.join(format!("f{index:04}.toml")), "a = 1\n").unwrap();
        }
        commit(dir, "seed");
        for index in 0..count {
            fs::write(dir.join(format!("f{index:04}.toml")), "a = 2\n").unwrap();
        }

        let root = repository_root(dir).unwrap();
        let files: Vec<PathBuf> = (0..count)
            .map(|index| root.join(format!("f{index:04}.toml")))
            .collect();
        let warnings = restage(&plan_for(&root), files.iter().map(PathBuf::as_path)).unwrap();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(staged_names_in(dir).len(), count);
    }

    fn plan_for(root: &Path) -> GitPlan {
        GitPlan {
            repos: vec![root.to_path_buf()],
            drifted: Vec::new(),
        }
    }

    #[test]
    fn the_owning_repository_is_the_innermost_one() {
        let plan = GitPlan {
            repos: vec![PathBuf::from("/a"), PathBuf::from("/a/b")],
            drifted: Vec::new(),
        };
        assert_eq!(
            owning_repo(&plan, Path::new("/a/b/c.rs")),
            Some(Path::new("/a/b"))
        );
        assert_eq!(
            owning_repo(&plan, Path::new("/a/x.rs")),
            Some(Path::new("/a"))
        );
        assert_eq!(owning_repo(&plan, Path::new("/elsewhere/x.rs")), None);
    }

    // ------------------------------------------------------------ submodules

    /// Adds `name` as a submodule of `super_root`, or reports that this git
    /// refuses local submodule URLs (`protocol.file.allow` defaults to `user`).
    fn add_submodule(super_root: &Path, sub: &Path, name: &str) -> bool {
        git(
            super_root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                sub.to_str().unwrap(),
                name,
            ],
        )
    }

    fn submodule_fixture() -> Option<(tempfile::TempDir, PathBuf, PathBuf)> {
        let temp = tempfile::tempdir().unwrap();
        let sub = temp.path().join("sub-origin");
        let super_dir = temp.path().join("super");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(&super_dir).unwrap();
        if !seeded(&sub) || !seeded(&super_dir) {
            return None;
        }
        fs::write(sub.join("sub.toml"), "a = 1\n").unwrap();
        commit(&sub, "sub seed");
        fs::write(super_dir.join("top.toml"), "a = 1\n").unwrap();
        commit(&super_dir, "super seed");

        if !add_submodule(&super_dir, &sub, "vendored") {
            eprintln!("SKIP: this git refuses a local submodule URL");
            return None;
        }
        commit(&super_dir, "add submodule");
        Some((temp, super_dir, sub))
    }

    #[test]
    fn a_submodule_is_reached_only_when_asked_for() {
        let Some((_temp, super_dir, _sub)) = submodule_fixture() else {
            return;
        };
        let inner = super_dir.join("vendored").join("sub.toml");
        fs::write(&inner, "a = 2\n").unwrap();
        assert!(git(&super_dir.join("vendored"), &["add", "-A"]));

        let without = changed_files(&GitSelection::new(GitScope::Staged), &super_dir).unwrap();
        assert!(
            !without.files.iter().any(|path| path.ends_with("sub.toml")),
            "{:?}",
            without.files
        );
        assert_eq!(without.repos.len(), 1);

        let with = changed_files(
            &GitSelection {
                recurse_submodules: true,
                ..GitSelection::new(GitScope::Staged)
            },
            &super_dir,
        )
        .unwrap();
        assert!(
            with.files.iter().any(|path| path.ends_with("sub.toml")),
            "{:?}",
            with.files
        );
        assert_eq!(with.repos.len(), 2, "{:?}", with.repos);
    }

    /// An uninitialized submodule is an empty directory: there is nothing
    /// checked out to format, and descending into it would fail the run.
    #[test]
    fn an_uninitialized_submodule_is_skipped() {
        let Some((_temp, super_dir, _sub)) = submodule_fixture() else {
            return;
        };
        fs::remove_dir_all(super_dir.join("vendored")).unwrap();
        fs::create_dir_all(super_dir.join("vendored")).unwrap();

        let changed = changed_files(
            &GitSelection {
                recurse_submodules: true,
                ..GitSelection::new(GitScope::Staged)
            },
            &super_dir,
        )
        .unwrap();
        assert_eq!(changed.repos.len(), 1, "{:?}", changed.repos);
    }

    #[test]
    fn a_gitlink_is_read_off_the_index() {
        let Some((_temp, super_dir, _sub)) = submodule_fixture() else {
            return;
        };
        let mut warnings = Vec::new();
        let links = gitlinks(Git::inherited(&super_dir), &mut warnings).unwrap();
        assert_eq!(links, vec![PathBuf::from("vendored")]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_repository_with_no_submodule_has_no_gitlink() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        if !seeded(dir) {
            return;
        }
        fs::write(dir.join("a.toml"), "a = 1\n").unwrap();
        commit(dir, "seed");

        let mut warnings = Vec::new();
        assert!(
            gitlinks(Git::inherited(dir), &mut warnings)
                .unwrap()
                .is_empty()
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// The visited set is what stops a submodule graph that points back at
    /// itself from recursing until the depth cap, and the cap is the backstop.
    #[test]
    fn a_submodule_is_visited_once() {
        let Some((_temp, super_dir, _sub)) = submodule_fixture() else {
            return;
        };
        // A second gitlink to the same checkout: two entries, one directory.
        if !add_submodule(&super_dir, &super_dir.join("vendored"), "again") {
            return;
        }
        commit(&super_dir, "add the same submodule twice");

        let changed = changed_files(
            &GitSelection {
                recurse_submodules: true,
                ..GitSelection::new(GitScope::Staged)
            },
            &super_dir,
        )
        .unwrap();
        let mut roots = changed.repos.clone();
        roots.sort_unstable();
        roots.dedup();
        assert_eq!(roots.len(), changed.repos.len(), "{:?}", changed.repos);
    }

    #[test]
    fn the_submodule_depth_is_capped() {
        assert_eq!(MAX_SUBMODULE_DEPTH, 32);
    }

    #[test]
    fn a_pathspec_is_literal_so_a_glob_in_a_name_is_not_expanded() {
        assert_eq!(
            pathspec(Path::new("a[1].rs")),
            OsString::from(":(literal)a[1].rs")
        );
    }
}
