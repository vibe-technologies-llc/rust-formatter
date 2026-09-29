#[path = "support/toolchain.rs"]
mod toolchain;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command as StdCommand,
};

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use tempfile::tempdir;
use toolchain::{needs_nightly, nightly_available};

fn formatter() -> Command {
    let mut cmd = Command::cargo_bin("rust-formatter").unwrap();
    cmd.env("RUST_FORMATTER_CACHE_DIR", cache_dir());
    cmd
}

/// Every suite gets one cache directory of its own, so a run can never read an
/// answer another test wrote and the developer's real cache is left alone.
fn cache_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

const DIRTY: &str = "foo={path=\"x\"}\n";
const CLEAN: &str = "foo.path = \"x\"\n";

fn write(path: &Path, body: &str) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    path.to_path_buf()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

/// Sets up a repository with one commit, or reports why it could not.
fn git_repo(dir: &Path) -> bool {
    let git = |args: &[&str]| {
        StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .is_ok_and(|status| status.success())
    };
    if !git(&["init", "--quiet", "."]) {
        eprintln!("SKIP: git is unavailable");
        return false;
    }
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["config", "user.name", "test"]);
    write(&dir.join("seed.txt"), "seed\n");
    git(&["add", "-A"]) && git(&["commit", "--quiet", "-m", "seed"])
}

fn git(dir: &Path, args: &[&str]) {
    assert!(
        StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed"
    );
}

// ------------------------------------------------------------ multiple paths

#[test]
fn every_named_path_is_formatted() {
    let temp = tempdir().unwrap();
    let a = write(&temp.path().join("a/one.toml"), DIRTY);
    let b = write(&temp.path().join("b/two.toml"), DIRTY);
    let untouched = write(&temp.path().join("c/three.toml"), DIRTY);

    formatter()
        .arg(temp.path().join("a"))
        .arg(temp.path().join("b"))
        .assert()
        .success();

    assert_eq!(read(&a), CLEAN);
    assert_eq!(read(&b), CLEAN);
    assert_eq!(
        read(&untouched),
        DIRTY,
        "an unnamed path must be left alone"
    );
}

/// Naming a directory and a file inside it must not run the file through
/// rustfmt twice, which under `--check` would print its diff twice.
#[test]
fn an_overlapping_path_is_collapsed_into_its_parent() {
    let temp = tempdir().unwrap();
    let nested = write(&temp.path().join("nested/inner.toml"), DIRTY);

    formatter()
        .arg(temp.path())
        .arg(&nested)
        .arg("-v")
        .assert()
        .success()
        .stderr(predicates::str::contains("target: loose directory").count(1));

    assert_eq!(read(&nested), CLEAN);
}

/// A variadic positional in front of `#[arg(last = true)]` is the risky half of
/// clap's grammar: the paths must not swallow the pass-through arguments.
#[test]
fn pass_through_arguments_survive_multiple_paths() {
    let temp = tempdir().unwrap();
    let one = write(&temp.path().join("one.rs"), "fn one() {}\n");
    let two = write(&temp.path().join("two.rs"), "fn two() {}\n");

    formatter()
        .arg(&one)
        .arg(&two)
        .arg("--")
        .arg("--not-a-rustfmt-flag")
        .assert()
        .failure()
        .code(2);
}

#[test]
fn several_targets_collapse_into_one_summary_line() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a/one.toml"), DIRTY);
    write(&temp.path().join("b/two.toml"), DIRTY);

    formatter()
        .arg(temp.path().join("a"))
        .arg(temp.path().join("b"))
        .assert()
        .success()
        .stderr(predicates::str::contains("Formatted 2 files"));
}

/// With nothing written there is no count to report, so the summary falls back
/// to naming what was covered.
#[test]
fn several_clean_targets_are_named_rather_than_counted() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a/one.toml"), CLEAN);
    write(&temp.path().join("b/two.toml"), CLEAN);

    formatter()
        .arg(temp.path().join("a"))
        .arg(temp.path().join("b"))
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Already formatted: 2 targets, 2 files",
        ));
}

// ---------------------------------------------------------------- file lists

#[test]
fn a_file_list_selects_exactly_its_entries() {
    let temp = tempdir().unwrap();
    let listed = write(&temp.path().join("listed.toml"), DIRTY);
    let unlisted = write(&temp.path().join("unlisted.toml"), DIRTY);
    let list = write(
        &temp.path().join("list.txt"),
        &format!("{}\n", listed.display()),
    );

    formatter()
        .arg("--files-from")
        .arg(&list)
        .assert()
        .success();

    assert_eq!(read(&listed), CLEAN);
    assert_eq!(read(&unlisted), DIRTY);
}

/// `git diff -z | rust-formatter --files-from -` is the shape that makes paths
/// with newlines or non-ASCII bytes survive, so both separators have to work
/// without a second flag.
#[test]
fn a_file_list_on_stdin_accepts_either_separator() {
    for separator in ["\n", "\0"] {
        let temp = tempdir().unwrap();
        let one = write(&temp.path().join("one.toml"), DIRTY);
        let two = write(&temp.path().join("two.toml"), DIRTY);

        formatter()
            .arg("--files-from")
            .arg("-")
            .write_stdin(format!(
                "{}{separator}{}{separator}",
                one.display(),
                two.display()
            ))
            .assert()
            .success();

        assert_eq!(read(&one), CLEAN, "separator {separator:?}");
        assert_eq!(read(&two), CLEAN, "separator {separator:?}");
    }
}

/// pre-commit hands over whatever git listed a moment ago; a path deleted since
/// then must not fail the whole run.
#[test]
fn a_file_list_skips_blank_and_vanished_entries() {
    let temp = tempdir().unwrap();
    let kept = write(&temp.path().join("kept.toml"), DIRTY);
    let list = write(
        &temp.path().join("list.txt"),
        &format!(
            "\n{}\n\n{}\n",
            kept.display(),
            temp.path().join("gone.toml").display()
        ),
    );

    formatter()
        .arg("--files-from")
        .arg(&list)
        .assert()
        .success();

    assert_eq!(read(&kept), CLEAN);
}

#[test]
fn an_empty_file_list_is_a_successful_no_op() {
    formatter()
        .arg("--check")
        .arg("--files-from")
        .arg("-")
        .write_stdin("")
        .assert()
        .success()
        .code(0)
        .stderr("");
}

#[test]
fn positional_paths_narrow_a_file_list() {
    let temp = tempdir().unwrap();
    let inside = write(&temp.path().join("keep/one.toml"), DIRTY);
    let outside = write(&temp.path().join("drop/two.toml"), DIRTY);
    let list = write(
        &temp.path().join("list.txt"),
        &format!("{}\n{}\n", inside.display(), outside.display()),
    );

    formatter()
        .arg("--files-from")
        .arg(&list)
        .arg(temp.path().join("keep"))
        .assert()
        .success();

    assert_eq!(read(&inside), CLEAN);
    assert_eq!(read(&outside), DIRTY);
}

// --------------------------------------------------------------- git scoping

#[test]
fn since_selects_committed_and_untracked_changes() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    let committed = write(&temp.path().join("committed.toml"), DIRTY);
    git(temp.path(), &["add", "-A"]);
    git(temp.path(), &["commit", "--quiet", "-m", "change"]);
    let untracked = write(&temp.path().join("untracked.toml"), DIRTY);
    let unchanged = write(&temp.path().join("unchanged.toml"), DIRTY);
    git(temp.path(), &["add", "unchanged.toml"]);
    git(temp.path(), &["commit", "--quiet", "-m", "base"]);

    formatter()
        .current_dir(temp.path())
        .arg("--since")
        .arg("HEAD~2")
        .assert()
        .success();

    assert_eq!(read(&committed), CLEAN);
    assert_eq!(read(&untracked), CLEAN, "a new file is a change too");
    let _ = unchanged;
}

#[test]
fn since_with_no_changes_exits_zero_and_says_nothing() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }

    formatter()
        .current_dir(temp.path())
        .arg("--check")
        .arg("--since")
        .arg("HEAD")
        .assert()
        .success()
        .code(0)
        .stderr("");
}

#[test]
fn since_outside_a_repository_is_a_tool_error() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY);

    formatter()
        .current_dir(temp.path())
        .arg("--since")
        .arg("HEAD")
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("Not inside a git repository"));
}

#[test]
fn an_unknown_ref_is_a_tool_error() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }

    formatter()
        .current_dir(temp.path())
        .arg("--since")
        .arg("no-such-ref")
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("Unknown git revision"));
}

/// `--all` is a git switch. Without `--end-of-options` it would list every
/// object instead of looking up a ref of that name.
#[test]
fn a_since_ref_that_looks_like_a_git_switch_is_a_ref() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }

    formatter()
        .current_dir(temp.path())
        .arg("--since")
        .arg("--all")
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("Unknown git revision"));
}

#[test]
fn staged_formats_only_staged_paths() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    let staged = write(&temp.path().join("staged.toml"), DIRTY);
    let unstaged = write(&temp.path().join("unstaged.toml"), DIRTY);
    git(temp.path(), &["add", "staged.toml"]);

    formatter()
        .current_dir(temp.path())
        .arg("--staged")
        .assert()
        .success();

    assert_eq!(read(&staged), CLEAN);
    assert_eq!(read(&unstaged), DIRTY);
}

/// The formatter rewrites the working tree, so for a partially staged file the
/// bytes it formats are not the bytes that would be committed. Saying so is the
/// whole contract of `--staged`.
#[test]
fn staged_warns_when_the_working_tree_has_drifted() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    let path = write(&temp.path().join("drift.toml"), DIRTY);
    git(temp.path(), &["add", "drift.toml"]);
    write(&path, &format!("{DIRTY}bar={{path=\"y\"}}\n"));

    formatter()
        .current_dir(temp.path())
        .arg("--check")
        .arg("--staged")
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains(
            "has unstaged changes; formatted the working tree",
        ));
}

/// `--staged` selects paths; it must never write to the index on the caller's
/// behalf, or a hook silently rewrites the commit it was asked to inspect.
/// The index fallback must still cover the case it was written for.
#[test]
fn staged_works_before_the_first_commit() {
    let temp = tempdir().unwrap();
    let git_init = StdCommand::new("git")
        .args(["init", "--quiet", "."])
        .current_dir(temp.path())
        .status()
        .is_ok_and(|status| status.success());
    if !git_init {
        eprintln!("SKIP: git is unavailable");
        return;
    }
    git(
        temp.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(temp.path(), &["config", "user.name", "test"]);
    let staged = write(&temp.path().join("staged.toml"), DIRTY);
    let untracked = write(&temp.path().join("loose.toml"), DIRTY);
    git(temp.path(), &["add", "staged.toml"]);

    formatter()
        .current_dir(temp.path())
        .arg("--staged")
        .assert()
        .success();

    assert_eq!(read(&staged), CLEAN);
    assert_eq!(read(&untracked), DIRTY);
}

/// A HEAD that exists but cannot be diffed is a git failure, not an unborn
/// branch. Taking the index fallback there would rewrite every tracked file.
#[test]
fn staged_reports_a_git_failure_rather_than_widening() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    let tracked = write(&temp.path().join("tracked.toml"), DIRTY);
    git(temp.path(), &["add", "-A"]);
    git(temp.path(), &["commit", "--quiet", "-m", "tracked"]);
    let staged = write(&temp.path().join("staged.toml"), DIRTY);
    git(temp.path(), &["add", "staged.toml"]);

    let blob = StdCommand::new("git")
        .args(["hash-object", "-w", "seed.txt"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(blob.status.success());
    git(temp.path(), &["symbolic-ref", "HEAD", "refs/heads/broken"]);
    write(
        &temp.path().join(".git/refs/heads/broken"),
        &String::from_utf8(blob.stdout).unwrap(),
    );

    formatter()
        .current_dir(temp.path())
        .arg("--staged")
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("git command failed"));

    assert_eq!(read(&tracked), DIRTY);
    assert_eq!(read(&staged), DIRTY);
}

#[test]
fn staged_never_touches_the_index() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    write(&temp.path().join("indexed.toml"), DIRTY);
    git(temp.path(), &["add", "indexed.toml"]);
    let before = StdCommand::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(temp.path())
        .output()
        .unwrap();

    formatter()
        .current_dir(temp.path())
        .arg("--staged")
        .assert()
        .success();

    let staged_blob = StdCommand::new("git")
        .args(["show", ":indexed.toml"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(staged_blob.stdout).unwrap(), DIRTY);
    assert!(before.status.success());
}

fn show(dir: &Path, spec: &str) -> String {
    let output = StdCommand::new("git")
        .args(["show", spec])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git show {spec} failed");
    String::from_utf8(output.stdout).unwrap()
}

/// The whole point of `--staged`: without `--restage` the commit carries the
/// bytes the formatter replaced.
#[test]
fn restage_puts_the_formatted_bytes_in_the_index() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    let indexed = write(&temp.path().join("indexed.toml"), DIRTY);
    git(temp.path(), &["add", "indexed.toml"]);

    formatter()
        .current_dir(temp.path())
        .args(["--staged", "--restage"])
        .assert()
        .success();

    assert_eq!(read(&indexed), CLEAN);
    assert_eq!(show(temp.path(), ":indexed.toml"), CLEAN);
}

/// Staging a partially staged file would also stage the edits the user kept out
/// of the commit, so it is left alone and named.
#[test]
fn restage_leaves_a_partially_staged_path_alone() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    let drifted = write(&temp.path().join("drifted.toml"), DIRTY);
    let clean = write(&temp.path().join("clean.toml"), DIRTY);
    git(temp.path(), &["add", "-A"]);
    write(&drifted, &format!("{DIRTY}bar={{path=\"y\"}}\n"));

    formatter()
        .current_dir(temp.path())
        .args(["--staged", "--restage"])
        .assert()
        .success()
        .stderr(predicates::str::contains("was not re-added to the index"));

    assert_eq!(show(temp.path(), ":clean.toml"), CLEAN);
    assert_eq!(show(temp.path(), ":drifted.toml"), DIRTY);
    let _ = clean;
}

#[test]
fn restage_reaches_the_index_of_each_submodule() {
    let temp = tempdir().unwrap();
    let inner = temp.path().join("sub");
    fs::create_dir_all(&inner).unwrap();
    if !git_repo(&inner) {
        return;
    }
    let outer = temp.path().join("super");
    fs::create_dir_all(&outer).unwrap();
    if !git_repo(&outer) {
        return;
    }
    let added = StdCommand::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            "../sub",
            "sub",
        ])
        .current_dir(&outer)
        .status()
        .unwrap();
    if !added.success() {
        eprintln!("SKIP: this git refuses a local submodule");
        return;
    }
    git(&outer, &["commit", "--quiet", "-m", "add submodule"]);
    let nested = write(&outer.join("sub/nested.toml"), DIRTY);
    git(&outer.join("sub"), &["add", "nested.toml"]);

    formatter()
        .current_dir(&outer)
        .args(["--staged", "--list-files"])
        .assert()
        .success()
        .stdout("");

    formatter()
        .current_dir(&outer)
        .args(["--staged", "--recurse-submodules", "--restage"])
        .assert()
        .success();

    assert_eq!(read(&nested), CLEAN);
    assert_eq!(show(&outer.join("sub"), ":nested.toml"), CLEAN);
}

#[test]
fn restage_needs_staged_and_refuses_a_read_only_run() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY);

    formatter()
        .current_dir(temp.path())
        .arg("--restage")
        .assert()
        .code(2);
    formatter()
        .current_dir(temp.path())
        .args(["--staged", "--restage", "--check"])
        .assert()
        .code(2);
    formatter()
        .current_dir(temp.path())
        .args(["--staged", "--restage", "--emit", "stdout", "a.toml"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("--emit stdout"));
}

/// An untracked file differs from every ref, so the ref does not select it; the
/// flag is what says whether that counts as a change.
#[test]
fn no_untracked_drops_the_files_no_ref_can_describe() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    let untracked = write(&temp.path().join("untracked.toml"), DIRTY);

    formatter()
        .current_dir(temp.path())
        .args(["--since", "HEAD", "--no-untracked"])
        .assert()
        .success();
    assert_eq!(read(&untracked), DIRTY);

    formatter()
        .current_dir(temp.path())
        .args(["--since", "HEAD"])
        .assert()
        .success();
    assert_eq!(read(&untracked), CLEAN);
}

#[test]
fn recurse_submodules_needs_a_git_scope_to_narrow() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY);

    formatter()
        .current_dir(temp.path())
        .arg("--recurse-submodules")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("--recurse-submodules"));
}

#[test]
fn no_untracked_only_applies_to_since() {
    let temp = tempdir().unwrap();
    formatter()
        .current_dir(temp.path())
        .args(["--staged", "--no-untracked"])
        .assert()
        .code(2);
}

/// A shallow clone has no fork point, so the comparison quietly became one
/// against the ref tip. Saying so is the difference between a narrowed run and
/// a wrong one.
#[test]
fn a_missing_merge_base_is_reported_rather_than_assumed() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    let first = StdCommand::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    let first = String::from_utf8(first.stdout).unwrap().trim().to_string();

    git(
        temp.path(),
        &["checkout", "--quiet", "--orphan", "unrelated"],
    );
    git(temp.path(), &["rm", "-q", "-rf", "."]);
    write(&temp.path().join("other.toml"), DIRTY);
    git(temp.path(), &["add", "-A"]);
    git(temp.path(), &["commit", "--quiet", "-m", "unrelated"]);

    formatter()
        .current_dir(temp.path())
        .args(["--since", &first])
        .assert()
        .success()
        .stderr(predicates::str::contains("no merge base"));
}

/// `--diff-filter` omitted `T`, so a path that swapped between a symlink and a
/// regular file was never selected.
#[cfg(unix)]
#[test]
fn a_type_change_is_a_change() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    write(&temp.path().join("real.toml"), CLEAN);
    let swapped = temp.path().join("swapped.toml");
    std::os::unix::fs::symlink("real.toml", &swapped).unwrap();
    git(temp.path(), &["add", "-A"]);
    git(temp.path(), &["commit", "--quiet", "-m", "link"]);

    fs::remove_file(&swapped).unwrap();
    write(&swapped, DIRTY);
    git(temp.path(), &["add", "swapped.toml"]);

    formatter()
        .current_dir(temp.path())
        .arg("--staged")
        .assert()
        .success();
    assert_eq!(read(&swapped), CLEAN);
}

/// The hazard `--staged` warns about applies to any scope that formats the
/// working tree while something else is staged.
#[test]
fn since_warns_when_a_selected_path_is_partially_staged() {
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    let drifted = write(&temp.path().join("drifted.toml"), DIRTY);
    git(temp.path(), &["add", "-A"]);
    write(&drifted, &format!("{DIRTY}bar={{path=\"y\"}}\n"));

    formatter()
        .current_dir(temp.path())
        .args(["--since", "HEAD"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "has unstaged changes; formatted the working tree",
        ));
}

// ------------------------------------------------------------ workspace scope

fn workspace(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\nresolver = \"2\"\n",
    );
    for name in ["a", "b"] {
        let dir = root.join("crates").join(name);
        write(
            &dir.join("Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        );
        write(&dir.join("src/lib.rs"), "pub fn f() {}\n");
        write(&dir.join("data.toml"), DIRTY);
        write(&dir.join("src/nested.toml"), DIRTY);
    }
}

/// Naming a directory inside a member rewrote the whole workspace, because the
/// run worked from the manifest the search walked up to rather than the path
/// that was typed.
#[test]
fn a_named_subdirectory_does_not_widen_to_the_workspace() {
    let temp = tempdir().unwrap();
    workspace(temp.path());

    formatter()
        .current_dir(temp.path())
        .args(["--toml-only", "crates/a/src"])
        .assert()
        .success();

    assert_eq!(read(&temp.path().join("crates/a/src/nested.toml")), CLEAN);
    assert_eq!(read(&temp.path().join("crates/a/data.toml")), DIRTY);
    assert_eq!(read(&temp.path().join("crates/b/data.toml")), DIRTY);
}

/// A directory that holds members but is not one itself is still just that
/// directory.
#[test]
fn a_named_directory_above_the_members_covers_only_what_it_holds() {
    let temp = tempdir().unwrap();
    workspace(temp.path());
    write(&temp.path().join("loose.toml"), DIRTY);

    formatter()
        .current_dir(temp.path())
        .args(["--toml-only", "crates"])
        .assert()
        .success();

    assert_eq!(read(&temp.path().join("crates/a/data.toml")), CLEAN);
    assert_eq!(read(&temp.path().join("crates/b/data.toml")), CLEAN);
    assert_eq!(read(&temp.path().join("loose.toml")), DIRTY);
}

/// Two members of one workspace are one cargo target under `--all`, keyed by
/// cargo's workspace root rather than each package directory.
#[test]
fn two_workspace_members_collapse_to_one_target() {
    let temp = tempdir().unwrap();
    workspace(temp.path());

    formatter()
        .current_dir(temp.path())
        .args(["--toml-only", "-v", "crates/a", "crates/b"])
        .assert()
        .success()
        .stderr(predicates::str::contains("target: cargo project").count(1));
}

/// Naming the package itself is what `--all` is about, so that stays wide.
#[test]
fn a_named_package_still_covers_the_workspace_under_all() {
    let temp = tempdir().unwrap();
    workspace(temp.path());

    formatter()
        .current_dir(temp.path())
        .args(["--toml-only", "crates/a"])
        .assert()
        .success();
    assert_eq!(read(&temp.path().join("crates/b/data.toml")), CLEAN);

    let temp = tempdir().unwrap();
    workspace(temp.path());
    formatter()
        .current_dir(temp.path())
        .args(["--toml-only", "--no-all", "crates/a"])
        .assert()
        .success();
    assert_eq!(read(&temp.path().join("crates/b/data.toml")), DIRTY);
}

#[test]
fn list_files_union_of_language_flags_matches_the_default() {
    let temp = tempdir().unwrap();
    workspace(temp.path());

    let listed = |args: &[&str]| {
        let stdout = formatter()
            .current_dir(temp.path())
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let mut lines: Vec<String> = String::from_utf8(stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines.sort();
        lines
    };

    let both = listed(&["--list-files"]);
    let rust = listed(&["--list-files", "--rust-only"]);
    let toml = listed(&["--list-files", "--toml-only"]);
    let mut union = rust.clone();
    union.extend(toml.iter().cloned());
    union.sort();
    union.dedup();
    assert_eq!(both, union);
    assert!(!rust.is_empty(), "{rust:?}");
    assert!(!toml.is_empty(), "{toml:?}");
}

#[test]
fn a_nested_non_member_is_out_of_a_package_run() {
    let temp = tempdir().unwrap();
    workspace(temp.path());
    let nested = temp.path().join("crates/a/nested");
    write(
        &nested.join("Cargo.toml"),
        "[package]\nname = \"nested\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&nested.join("data.toml"), DIRTY);
    write(&nested.join("src/lib.rs"), "pub fn f(  ){}\n");

    formatter()
        .current_dir(temp.path())
        .args(["--toml-only", "--no-all", "crates/a"])
        .assert()
        .success();

    assert_eq!(read(&temp.path().join("crates/a/data.toml")), CLEAN);
    assert_eq!(read(&nested.join("data.toml")), DIRTY);
}

#[test]
fn all_without_cargo_uses_one_member_scope() {
    let temp = tempdir().unwrap();
    workspace(temp.path());
    let local = temp.path().join("crates").join("local");
    write(
        &local.join("Cargo.toml"),
        "[package]\nname = \"local\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&local.join("data.toml"), DIRTY);
    write(&local.join("src/lib.rs"), "pub fn f(  ){}\n");
    write(&temp.path().join("crates/b/src/lib.rs"), "pub fn f(  ){}\n");

    let empty = tempdir().unwrap();
    formatter()
        .env("PATH", empty.path())
        .env_remove("RUSTFMT")
        .current_dir(temp.path())
        .args(["--toml-only"])
        .assert()
        .success();

    assert_eq!(read(&temp.path().join("crates/a/data.toml")), CLEAN);
    assert_eq!(read(&temp.path().join("crates/b/data.toml")), CLEAN);
    assert_eq!(read(&local.join("data.toml")), DIRTY);

    if !nightly_available() {
        return;
    }
    let rustfmt = StdCommand::new("rustup")
        .args(["which", "rustfmt", "--toolchain", "nightly"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_owned());
    let Some(rustfmt) = rustfmt else {
        return;
    };
    let bin = empty.path().join("rustfmt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&rustfmt, &bin).unwrap();
    #[cfg(not(unix))]
    fs::copy(&rustfmt, &bin).unwrap();

    formatter()
        .env("PATH", empty.path())
        .env("RUSTFMT", &bin)
        .current_dir(temp.path())
        .args(["--rust-only"])
        .assert()
        .success();

    assert_eq!(
        read(&temp.path().join("crates/b/src/lib.rs")),
        "pub fn f() {}\n"
    );
    assert_eq!(read(&local.join("src/lib.rs")), "pub fn f(  ){}\n");
}

/// Two members of one workspace are one run: the selection-time dedup cannot
/// see that on its own, because it resolves the workspace differently from the
/// run.
#[test]
fn two_named_members_are_walked_once() {
    let temp = tempdir().unwrap();
    workspace(temp.path());

    formatter()
        .current_dir(temp.path())
        .args(["--toml-only", "crates/a", "crates/b"])
        .assert()
        .success();

    // One target, and every file counted once: a second walk would report the
    // same workspace twice over.
    formatter()
        .current_dir(temp.path())
        .args(["--toml-only", "crates/a", "crates/b"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Already formatted: Cargo project",
        ))
        .stderr(predicates::str::contains("2 targets").not());
}

fn package(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&root.join("src/main.rs"), "fn main() {}\n");
    write(&root.join("src/clean.toml"), CLEAN);
    write(&root.join("tests/dirty.toml"), DIRTY);
}

#[test]
fn two_named_subdirectories_of_one_package_are_both_checked() {
    let temp = tempdir().unwrap();
    package(temp.path());

    for order in [["src", "tests"], ["tests", "src"]] {
        formatter()
            .current_dir(temp.path())
            .args(["--toml-only", "--check"])
            .args(order)
            .assert()
            .code(1);
    }
}

#[test]
fn two_named_subdirectories_of_workspace_members_are_both_formatted() {
    let temp = tempdir().unwrap();
    workspace(temp.path());

    formatter()
        .current_dir(temp.path())
        .args(["--toml-only", "crates/a/src", "crates/b/src"])
        .assert()
        .success();

    assert_eq!(read(&temp.path().join("crates/a/src/nested.toml")), CLEAN);
    assert_eq!(read(&temp.path().join("crates/b/src/nested.toml")), CLEAN);
    assert_eq!(read(&temp.path().join("crates/a/data.toml")), DIRTY);
    assert_eq!(read(&temp.path().join("crates/b/data.toml")), DIRTY);
}

#[test]
fn a_named_package_absorbs_a_named_subdirectory_in_either_order() {
    for order in [["crates/a/src", "crates/a"], ["crates/a", "crates/a/src"]] {
        let temp = tempdir().unwrap();
        workspace(temp.path());

        formatter()
            .current_dir(temp.path())
            .args(["--toml-only", "--no-all", "-v"])
            .args(order)
            .assert()
            .success()
            .stderr(predicates::str::contains("target: cargo project").count(1));

        assert_eq!(read(&temp.path().join("crates/a/data.toml")), CLEAN);
        assert_eq!(read(&temp.path().join("crates/a/src/nested.toml")), CLEAN);
        assert_eq!(read(&temp.path().join("crates/b/data.toml")), DIRTY);
    }
}

// ------------------------------------------------------------ include/exclude

#[test]
fn exclude_drops_a_subtree_and_include_narrows_to_one() {
    let temp = tempdir().unwrap();
    let kept = write(&temp.path().join("src/keep.toml"), DIRTY);
    let dropped = write(&temp.path().join("vendored/drop.toml"), DIRTY);

    formatter()
        .arg(temp.path())
        .arg("--exclude")
        .arg("vendored/**")
        .assert()
        .success();
    assert_eq!(read(&kept), CLEAN);
    assert_eq!(read(&dropped), DIRTY);

    let only = write(&temp.path().join("vendored/drop.toml"), DIRTY);
    formatter()
        .arg(temp.path())
        .arg("--include")
        .arg("vendored/**")
        .assert()
        .success();
    assert_eq!(read(&only), CLEAN);
}

#[test]
fn a_glob_that_matches_nothing_exits_zero_and_says_nothing() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY);

    formatter()
        .arg("--check")
        .arg(temp.path())
        .arg("--exclude")
        .arg("**")
        .assert()
        .success()
        .code(0)
        .stderr("");
}

#[test]
fn excluding_a_named_file_selects_nothing_rather_than_failing() {
    let temp = tempdir().unwrap();
    let path = write(&temp.path().join("a.toml"), DIRTY);

    formatter()
        .arg(&path)
        .arg("--exclude")
        .arg("a.toml")
        .assert()
        .success()
        .code(0);
    assert_eq!(read(&path), DIRTY);
}

#[test]
fn an_invalid_glob_is_reported_rather_than_ignored() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY);

    formatter()
        .arg(temp.path())
        .arg("--exclude")
        .arg("a[")
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("Invalid glob pattern"));
}

// ------------------------------------------------------- language selection

#[test]
fn rust_only_leaves_toml_alone() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let toml = write(&temp.path().join("a.toml"), DIRTY);
    let rust = write(&temp.path().join("a.rs"), "fn  a() {}\n");

    formatter()
        .arg(temp.path())
        .arg("--rust-only")
        .assert()
        .success();

    assert_eq!(read(&toml), DIRTY);
    assert_eq!(read(&rust), "fn a() {}\n");
}

/// `--toml-only` has to short-circuit the toolchain preflight entirely, or it
/// is useless in an image with no rustup.
#[test]
fn toml_only_never_resolves_a_toolchain() {
    let temp = tempdir().unwrap();
    let toml = write(&temp.path().join("a.toml"), DIRTY);
    let rust = write(&temp.path().join("a.rs"), "fn  a() {}\n");

    formatter()
        .arg(temp.path())
        .arg("--toml-only")
        .arg("--toolchain")
        .arg("no-such-toolchain")
        .assert()
        .success();

    assert_eq!(read(&toml), CLEAN);
    assert_eq!(read(&rust), "fn  a() {}\n");
}

#[test]
fn rust_only_and_toml_only_conflict() {
    formatter()
        .arg("--rust-only")
        .arg("--toml-only")
        .assert()
        .failure()
        .code(2);
}

// ---------------------------------------------------------- walker toggles

#[test]
fn hidden_walks_dot_directories_but_never_git() {
    let temp = tempdir().unwrap();
    let visible = write(&temp.path().join("visible.toml"), DIRTY);
    let hidden = write(&temp.path().join(".secret/a.toml"), DIRTY);
    let in_git = write(&temp.path().join(".git/config.toml"), DIRTY);

    formatter().arg(temp.path()).assert().success();
    assert_eq!(read(&visible), CLEAN);
    assert_eq!(read(&hidden), DIRTY, "hidden dirs are skipped by default");

    formatter()
        .arg(temp.path())
        .arg("--hidden")
        .assert()
        .success();
    assert_eq!(read(&hidden), CLEAN);
    assert_eq!(read(&in_git), DIRTY, ".git stays pruned under --hidden");
}

#[test]
fn no_ignore_reaches_gitignored_files() {
    let temp = tempdir().unwrap();
    write(&temp.path().join(".gitignore"), "ignored.toml\n");
    let ignored = write(&temp.path().join("ignored.toml"), DIRTY);
    let tracked = write(&temp.path().join("tracked.toml"), DIRTY);

    formatter().arg(temp.path()).assert().success();
    assert_eq!(read(&tracked), CLEAN);
    assert_eq!(read(&ignored), DIRTY);

    formatter()
        .arg(temp.path())
        .arg("--no-ignore")
        .assert()
        .success();
    assert_eq!(read(&ignored), CLEAN);
}

#[test]
fn ignore_path_applies_an_extra_pattern_file() {
    let temp = tempdir().unwrap();
    let skipped = write(&temp.path().join("skip.toml"), DIRTY);
    let kept = write(&temp.path().join("keep.toml"), DIRTY);
    let patterns = write(&temp.path().join("patterns"), "skip.toml\n");

    formatter()
        .arg(temp.path())
        .arg("--ignore-path")
        .arg(&patterns)
        .assert()
        .success();

    assert_eq!(read(&skipped), DIRTY);
    assert_eq!(read(&kept), CLEAN);
}

#[test]
fn a_missing_ignore_path_is_a_tool_error() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY);

    formatter()
        .arg(temp.path())
        .arg("--ignore-path")
        .arg(temp.path().join("nope"))
        .assert()
        .failure()
        .code(2);
}

#[test]
fn max_depth_bounds_the_walk_below_each_named_directory() {
    let temp = tempdir().unwrap();
    let shallow = write(&temp.path().join("top.toml"), DIRTY);
    let deep = write(&temp.path().join("one/two.toml"), DIRTY);

    formatter()
        .arg(temp.path())
        .arg("--max-depth")
        .arg("1")
        .assert()
        .success();

    assert_eq!(read(&shallow), CLEAN);
    assert_eq!(read(&deep), DIRTY);
}

// -------------------------------------------------------------- toml skips

#[test]
fn the_default_toml_skip_list_can_be_extended_and_cleared() {
    let temp = tempdir().unwrap();
    let clippy = write(&temp.path().join("clippy.toml"), DIRTY);
    let plain = write(&temp.path().join("plain.toml"), DIRTY);

    formatter().arg(temp.path()).assert().success();
    assert_eq!(read(&clippy), DIRTY);
    assert_eq!(read(&plain), CLEAN);

    formatter()
        .arg(temp.path())
        .arg("--no-default-toml-skips")
        .assert()
        .success();
    assert_eq!(read(&clippy), CLEAN);

    let extra = write(&temp.path().join("generated.toml"), DIRTY);
    formatter()
        .arg(temp.path())
        .arg("--skip-toml")
        .arg("generated.toml")
        .assert()
        .success();
    assert_eq!(read(&extra), DIRTY);
}

// --------------------------------------------------- bounded ancestor search

/// A `Cargo.toml` above the repository root belongs to another project; picking
/// it up would drag a whole unrelated workspace into the run.
#[test]
fn the_manifest_search_stops_at_the_repository_root() {
    let temp = tempdir().unwrap();
    write(
        &temp.path().join("Cargo.toml"),
        "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
    );
    let inner = temp.path().join("inner");
    fs::create_dir_all(&inner).unwrap();
    if !git_repo(&inner) {
        return;
    }
    write(&inner.join("loose.toml"), DIRTY);

    formatter()
        .arg(&inner)
        .arg("-v")
        .assert()
        .success()
        .stderr(predicates::str::contains("target: loose directory"));
}

#[test]
fn a_manifest_inside_the_repository_is_still_found() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    if !git_repo(temp.path()) {
        return;
    }
    write(
        &temp.path().join("Cargo.toml"),
        "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&temp.path().join("src/lib.rs"), "pub fn a() {}\n");

    formatter()
        .arg(temp.path().join("src"))
        .arg("-v")
        .assert()
        .success()
        .stderr(predicates::str::contains("target: cargo project"));
}
