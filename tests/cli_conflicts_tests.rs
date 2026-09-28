#[path = "support/toolchain.rs"]
mod toolchain;

use std::{fs, path::Path};

use assert_cmd::Command;
use tempfile::tempdir;
use toolchain::{needs_nightly, nightly_available};

fn formatter() -> Command {
    let mut cmd = Command::cargo_bin("rust-formatter").unwrap();
    cmd.env("RUST_FORMATTER_CACHE_DIR", cache_dir());
    cmd
}

/// Every suite gets one cache directory of its own, so a run can never read an
/// answer another test wrote and the developer's real cache is left alone.
fn cache_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

/// The exit-code half of `validate`. The rules themselves are asserted against
/// the function in `src/main.rs`; what the binary adds is that a rejection is
/// an exit 2 with the message on stderr, and not a panic or a silent exit 0.
#[track_caller]
fn rejected(args: &[&str], fragment: &str) {
    let assert = formatter().args(args).assert().failure().code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains(fragment),
        "{args:?}\nexpected {fragment:?} in:\n{stderr}"
    );
    assert!(
        stderr.starts_with("error: "),
        "{args:?}: a rejection has to say `error:` first:\n{stderr}"
    );
    assert!(
        assert.get_output().stdout.is_empty(),
        "{args:?}: stdout is the product stream and must stay empty"
    );
}

#[test]
fn a_flag_combination_validate_refuses_exits_two() {
    rejected(
        &["--emit", "stdout"],
        "--emit stdout needs --stdin or exactly one PATH",
    );
    rejected(
        &["--emit", "stdout", "--restage", "--staged", "a.rs"],
        "--restage rewrites files, so it cannot be combined with --emit stdout",
    );
    rejected(
        &["--full-versions", "--rust-only"],
        "--full-versions rewrites Cargo.toml, which --rust-only excludes",
    );
    rejected(
        &["--recurse-submodules"],
        "--recurse-submodules narrows --since or --staged",
    );
    rejected(
        &["--toml-version", "1.0", "--toml-inline-tables", "expand"],
        "--toml-version 1.0 cannot be combined with --toml-inline-tables expand",
    );
    rejected(
        &["--sort-grouped", "--toml-max-blank-lines", "0"],
        "--sort-grouped cannot be combined with --toml-max-blank-lines 0",
    );
    rejected(
        &["--range", "1:2", "Cargo.toml"],
        "--range formats Rust only",
    );
    // The path is load-bearing: without one, the `--emit stdout needs one
    // PATH` rule fires first and this rule is never reached.
    rejected(
        &["--watch", "--emit", "stdout", "a.rs"],
        "--watch rewrites files as they change, so it cannot be combined with --emit stdout",
    );
}

#[test]
fn a_clap_conflict_exits_two() {
    for args in [
        ["--rust-only", "--toml-only"].as_slice(),
        ["--list-files", "--list-different"].as_slice(),
        ["--staged", "--stdin"].as_slice(),
        ["--emit", "stdout", "--check"].as_slice(),
    ] {
        formatter().args(args).assert().failure().code(2);
    }
}

/// `--emit files` is the default spelled out. It was declared and never passed,
/// so nothing proved it still meant "write the files".
#[test]
fn emit_files_writes_the_files() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("a.toml");
    fs::write(&file, "foo={path=\"x\"}\n").unwrap();

    formatter()
        .args(["--emit", "files", "--toml-only"])
        .arg(temp.path())
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&file).unwrap(), "foo.path = \"x\"\n");
}

#[test]
fn emit_stdout_prints_the_file_and_leaves_it_alone() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("a.toml");
    fs::write(&file, "foo={path=\"x\"}\n").unwrap();

    formatter()
        .args(["--emit", "stdout", "--toml-only"])
        .arg(&file)
        .assert()
        .success()
        .stdout("foo.path = \"x\"\n");

    assert_eq!(fs::read_to_string(&file).unwrap(), "foo={path=\"x\"}\n");
}

/// `--color auto` against a captured stdout is a pipe, so it has to resolve to
/// off. Getting it backwards paints escapes into whatever reads the output.
#[test]
fn color_auto_is_plain_when_the_output_is_captured() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("a.toml");
    fs::write(&file, "foo={path=\"x\"}\n").unwrap();

    let assert = formatter()
        .args(["--color", "auto", "--check", "--toml-only"])
        .arg(temp.path())
        .assert()
        .failure()
        .code(1);
    let output = assert.get_output();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains('\u{1b}'), "auto painted a pipe:\n{text}");
}

#[test]
fn color_always_paints_and_never_does_not() {
    let temp = tempdir().unwrap();
    fs::write(temp.path().join("a.toml"), "foo={path=\"x\"}\n").unwrap();

    for (choice, painted) in [("always", true), ("never", false)] {
        let assert = formatter()
            .args(["--color", choice, "--check", "--toml-only"])
            .arg(temp.path())
            .assert()
            .failure()
            .code(1);
        let output = assert.get_output();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            text.contains('\u{1b}'),
            painted,
            "--color {choice}:\n{text}"
        );
    }
}

/// A pinned `--toolchain` was declared and never passed either. It has to reach
/// rustup by name rather than falling back to `auto`'s search.
#[test]
fn a_pinned_toolchain_is_used_by_name() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"pinned\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(temp.path().join("src").join("lib.rs"), "pub fn  f( ) {}\n").unwrap();

    formatter()
        .args(["--toolchain", "nightly", "-v"])
        .arg(temp.path())
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(temp.path().join("src").join("lib.rs")).unwrap(),
        "pub fn f() {}\n"
    );
}

#[test]
fn an_unknown_toolchain_names_itself_rather_than_falling_back() {
    let temp = tempdir().unwrap();
    fs::write(temp.path().join("a.rs"), "fn  f( ){}\n").unwrap();

    let assert = formatter()
        .args(["--toolchain", "rust-formatter-no-such-toolchain"])
        .arg(temp.path())
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("rust-formatter-no-such-toolchain"),
        "{stderr}"
    );
}
