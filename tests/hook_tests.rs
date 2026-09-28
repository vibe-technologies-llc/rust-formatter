//! The git hook installer.
//!
//! Every git and formatter invocation here runs with the developer's own git
//! configuration switched off. A global `core.hooksPath` would otherwise fail
//! every hooks-directory assertion, and a global `commit.gpgsign` would fail
//! the two tests that make a commit -- neither of which says anything about
//! the code under test.

use std::{fs, path::Path, process};

use assert_cmd::Command;
use tempfile::TempDir;

/// The marker the installed hook carries. It is a compatibility promise, so the
/// test names it literally rather than reading it back off the source.
const MARKER: &str = "# rust-formatter-hook:";
const BACKUP: &str = "pre-commit.rust-formatter.bak";
const FOREIGN: &str = "#!/bin/sh\nexec cargo fmt --check\n";

/// A repository with no template hooks, no user configuration and no identity
/// but its own.
struct Repo {
    dir: TempDir,
}

impl Repo {
    /// `None` when git is unavailable, which is a skip rather than a failure.
    fn new() -> Option<Self> {
        let dir = tempfile::tempdir().unwrap();
        let repo = Self { dir };
        // An empty template, so the stock `*.sample` hooks are absent and a
        // user's `init.templateDir` cannot install one of their own.
        repo.git(&["init", "--quiet", "--template=", "."])?;
        repo.git(&["config", "user.email", "test@example.invalid"])?;
        repo.git(&["config", "user.name", "Test"])?;
        repo.git(&["config", "commit.gpgsign", "false"])?;
        Some(repo)
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `None` when git could not be run at all, or reported a failure.
    fn git(&self, args: &[&str]) -> Option<String> {
        let output = self.git_in(self.path(), args)?;
        output.status.success().then(|| stdout(&output))
    }

    fn git_in(&self, cwd: &Path, args: &[&str]) -> Option<process::Output> {
        let mut command = process::Command::new("git");
        self.isolate(&mut command);
        command.current_dir(cwd).args(args).output().ok()
    }

    /// The environment both git and the formatter are given.
    fn isolate(&self, command: &mut process::Command) {
        let nowhere = self.path().join("no-such-config");
        command
            .env("HOME", self.path())
            .env("XDG_CONFIG_HOME", self.path().join("xdg"))
            .env("GIT_CONFIG_GLOBAL", &nowhere)
            .env("GIT_CONFIG_SYSTEM", &nowhere)
            .env("GIT_CONFIG_NOSYSTEM", "1");
    }

    fn formatter(&self) -> Command {
        self.formatter_in(self.path())
    }

    fn formatter_in(&self, cwd: &Path) -> Command {
        let mut cmd = Command::cargo_bin("rust-formatter").unwrap();
        let nowhere = self.path().join("no-such-config");
        cmd.current_dir(cwd)
            .env("HOME", self.path())
            .env("XDG_CONFIG_HOME", self.path().join("xdg"))
            .env("GIT_CONFIG_GLOBAL", &nowhere)
            .env("GIT_CONFIG_SYSTEM", &nowhere)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("RUST_FORMATTER_CACHE_DIR", self.path().join("cache"));
        cmd
    }

    fn hook(&self) -> std::path::PathBuf {
        self.path().join(".git/hooks/pre-commit")
    }

    /// `git init --template=` leaves no `hooks` directory at all, so a test
    /// that plants a hook has to make one -- exactly as `install` does.
    fn plant(&self, body: &str) {
        let hook = self.hook();
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(hook, body).unwrap();
    }
}

fn stdout(output: &process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Every test opens with this: without git there is nothing to install into.
macro_rules! repo {
    () => {
        match Repo::new() {
            Some(repo) => repo,
            None => {
                eprintln!("SKIP: git is unavailable");
                return;
            }
        }
    };
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

#[test]
fn install_writes_an_executable_hook() {
    let repo = repo!();
    repo.formatter()
        .args(["hook", "install"])
        .assert()
        .success();

    let hook = repo.hook();
    let body = fs::read_to_string(&hook).unwrap();
    assert!(body.contains(MARKER), "{body}");
    assert!(body.contains("mode=restage"), "{body}");
    assert!(body.contains("--staged --restage"), "{body}");
    assert!(body.starts_with("#!/bin/sh\n"), "{body}");
    assert!(is_executable(&hook), "the hook has to be executable");
}

#[test]
fn install_check_writes_the_other_mode() {
    let repo = repo!();
    repo.formatter()
        .args(["hook", "install", "--check"])
        .assert()
        .success();

    let body = fs::read_to_string(repo.hook()).unwrap();
    assert!(body.contains("mode=check"), "{body}");
    assert!(body.contains("--staged --check"), "{body}");
}

/// Re-installing has to be an upgrade, not a refusal: that is what makes the
/// marker a compatibility promise rather than decoration.
#[test]
fn installing_twice_leaves_the_same_bytes() {
    let repo = repo!();
    repo.formatter()
        .args(["hook", "install"])
        .assert()
        .success();
    let first = fs::read(repo.hook()).unwrap();

    let assert = repo
        .formatter()
        .args(["hook", "install"])
        .assert()
        .success();
    let said = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(said.contains("already installed"), "{said}");
    assert_eq!(fs::read(repo.hook()).unwrap(), first);
}

#[test]
fn an_older_hook_is_upgraded_in_place() {
    let repo = repo!();
    repo.formatter()
        .args(["hook", "install"])
        .assert()
        .success();
    let current = fs::read_to_string(repo.hook()).unwrap();
    let stale = current.replace("version=1", "version=0");
    fs::write(repo.hook(), &stale).unwrap();

    let assert = repo
        .formatter()
        .args(["hook", "install"])
        .assert()
        .success();
    let said = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(said.contains("upgraded"), "{said}");
    assert_eq!(fs::read_to_string(repo.hook()).unwrap(), current);
}

/// Somebody else's hook is their only copy of it.
#[test]
fn a_foreign_hook_is_not_clobbered() {
    let repo = repo!();
    repo.plant(FOREIGN);

    let assert = repo
        .formatter()
        .args(["hook", "install"])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.starts_with("error: "), "{stderr}");
    assert!(stderr.contains("--staged --restage"), "{stderr}");
    assert!(stderr.contains("--force"), "{stderr}");
    assert_eq!(fs::read_to_string(repo.hook()).unwrap(), FOREIGN);
}

#[test]
fn force_saves_the_foreign_hook_but_never_a_backup() {
    let repo = repo!();
    repo.plant(FOREIGN);

    repo.formatter()
        .args(["hook", "install", "--force"])
        .assert()
        .success();
    let backup = repo.path().join(".git/hooks").join(BACKUP);
    assert_eq!(fs::read_to_string(&backup).unwrap(), FOREIGN);
    assert!(fs::read_to_string(repo.hook()).unwrap().contains(MARKER));

    // A second forced install over another foreign hook would destroy the only
    // copy of the first one.
    repo.plant("#!/bin/sh\nexit 0\n");
    let assert = repo
        .formatter()
        .args(["hook", "install", "--force"])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains(BACKUP), "{stderr}");
    assert_eq!(fs::read_to_string(&backup).unwrap(), FOREIGN);
}

#[test]
fn uninstall_removes_only_our_hook() {
    let repo = repo!();

    repo.formatter()
        .args(["hook", "uninstall"])
        .assert()
        .success();

    repo.plant(FOREIGN);
    repo.formatter()
        .args(["hook", "uninstall"])
        .assert()
        .failure()
        .code(2);
    assert_eq!(fs::read_to_string(repo.hook()).unwrap(), FOREIGN);

    fs::remove_file(repo.hook()).unwrap();
    repo.formatter()
        .args(["hook", "install"])
        .assert()
        .success();
    repo.formatter()
        .args(["hook", "uninstall"])
        .assert()
        .success();
    assert!(!repo.hook().exists());
}

#[test]
fn uninstall_puts_back_what_force_moved_aside() {
    let repo = repo!();
    repo.plant(FOREIGN);
    repo.formatter()
        .args(["hook", "install", "--force"])
        .assert()
        .success();
    repo.formatter()
        .args(["hook", "uninstall"])
        .assert()
        .success();

    assert_eq!(fs::read_to_string(repo.hook()).unwrap(), FOREIGN);
    assert!(!repo.path().join(".git/hooks").join(BACKUP).exists());
}

/// `1` is "the state you asked about is not the one you want", so
/// `hook status >/dev/null || hook install` is a one-liner.
#[test]
fn status_reports_each_state() {
    let repo = repo!();

    let assert = repo
        .formatter()
        .args(["hook", "status"])
        .assert()
        .failure()
        .code(1);
    let said = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(said.contains("hooks-dir:"), "{said}");
    assert!(said.contains("absent"), "{said}");

    repo.formatter()
        .args(["hook", "install"])
        .assert()
        .success();
    let assert = repo.formatter().args(["hook", "status"]).assert().success();
    let said = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(said.contains("mode restage"), "{said}");

    repo.plant(FOREIGN);
    let assert = repo
        .formatter()
        .args(["hook", "status"])
        .assert()
        .failure()
        .code(1);
    let said = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(said.contains("not installed by rust-formatter"), "{said}");
}

#[test]
fn the_hooks_directory_honours_core_hookspath() {
    let repo = repo!();
    repo.git(&["config", "core.hooksPath", "myhooks"]).unwrap();

    repo.formatter()
        .args(["hook", "install"])
        .assert()
        .success();
    let hook = repo.path().join("myhooks/pre-commit");
    assert!(hook.is_file(), "install has to create the directory too");
    assert!(fs::read_to_string(&hook).unwrap().contains(MARKER));
    assert!(!repo.hook().exists());
}

/// `rev-parse --git-path` answers relative to the directory git ran in, so a
/// run from a subdirectory is the regression test for resolving it against the
/// wrong base.
#[test]
fn a_subdirectory_resolves_the_same_hooks_directory() {
    let repo = repo!();
    let nested = repo.path().join("src/deep");
    fs::create_dir_all(&nested).unwrap();

    repo.formatter_in(&nested)
        .args(["hook", "install"])
        .assert()
        .success();
    assert!(fs::read_to_string(repo.hook()).unwrap().contains(MARKER));

    let assert = repo
        .formatter_in(&nested)
        .args(["hook", "status"])
        .assert()
        .success();
    let said = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(!said.contains("src/deep"), "{said}");
}

/// A linked worktree has a git directory of its own, but hooks live in the
/// common one -- which is why `--git-path` is the right question to ask.
#[test]
fn a_linked_worktree_shares_the_hooks_directory() {
    let repo = repo!();
    fs::write(repo.path().join("a.toml"), "a = 1\n").unwrap();
    if repo.git(&["add", "a.toml"]).is_none() || repo.git(&["commit", "-qm", "one"]).is_none() {
        eprintln!("SKIP: git could not make a commit");
        return;
    }
    let tree = repo.path().join("linked");
    if repo
        .git(&["worktree", "add", "--quiet", "linked"])
        .is_none()
    {
        eprintln!("SKIP: git worktree is unavailable");
        return;
    }

    repo.formatter_in(&tree)
        .args(["hook", "install"])
        .assert()
        .success();
    assert!(
        fs::read_to_string(repo.hook()).unwrap().contains(MARKER),
        "a worktree's hooks are the repository's hooks"
    );
    assert!(!tree.join(".git/hooks/pre-commit").exists());
}

#[test]
fn outside_a_repository_it_says_so() {
    let repo = repo!();
    let outside = tempfile::tempdir().unwrap();
    let assert = repo
        .formatter_in(outside.path())
        .args(["hook", "status"])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.starts_with("error: "), "{stderr}");
}

/// The whole point, end to end: the hook is what git runs, and the commit
/// carries formatted bytes. TOML only, so no rustfmt is needed -- and
/// `rust-formatter` is deliberately *not* on the hook's PATH, so this also
/// proves the recorded fallback path works.
#[test]
fn the_installed_hook_formats_the_commit() {
    let repo = repo!();
    repo.formatter()
        .args(["hook", "install"])
        .assert()
        .success();

    fs::write(repo.path().join("a.toml"), "foo={path=\"x\"}\n").unwrap();
    if repo.git(&["add", "a.toml"]).is_none() {
        eprintln!("SKIP: git could not stage");
        return;
    }
    let output = repo
        .git_in(repo.path(), &["commit", "-qm", "formatted"])
        .unwrap();
    assert!(
        output.status.success(),
        "the commit failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let committed = repo.git(&["show", "HEAD:a.toml"]).unwrap();
    assert_eq!(committed, "foo.path = \"x\"\n");
    assert_eq!(
        fs::read_to_string(repo.path().join("a.toml")).unwrap(),
        "foo.path = \"x\"\n"
    );
}

#[test]
fn the_checking_hook_aborts_the_commit() {
    let repo = repo!();
    repo.formatter()
        .args(["hook", "install", "--check"])
        .assert()
        .success();

    let unformatted = "foo={path=\"x\"}\n";
    fs::write(repo.path().join("a.toml"), unformatted).unwrap();
    if repo.git(&["add", "a.toml"]).is_none() {
        eprintln!("SKIP: git could not stage");
        return;
    }
    let output = repo
        .git_in(repo.path(), &["commit", "-qm", "refused"])
        .unwrap();
    assert!(
        !output.status.success(),
        "the commit should have been refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--staged --restage"), "{stderr}");
    assert_eq!(
        fs::read_to_string(repo.path().join("a.toml")).unwrap(),
        unformatted,
        "a checking hook must not rewrite anything"
    );
}
