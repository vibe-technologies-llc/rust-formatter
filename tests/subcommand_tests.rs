//! The subcommands: shell completions, the man page, the cargo alias, and the
//! one behaviour change they cost -- a bare `completions` is now a subcommand
//! rather than a directory of that name.

use std::{fs, path::Path};

use assert_cmd::Command;
use tempfile::tempdir;

/// Every shell `completions` accepts, and the file each one is conventionally
/// installed as. Pinning the names here is the point: they are what an install
/// instruction in the README depends on.
const SHELLS: [(&str, &str); 5] = [
    ("bash", "rust-formatter.bash"),
    ("zsh", "_rust-formatter"),
    ("fish", "rust-formatter.fish"),
    ("elvish", "rust-formatter.elv"),
    ("powershell", "_rust-formatter.ps1"),
];

/// The man pages `--out-dir` writes: the tool, then one per command. `help` is
/// deliberately absent -- the subcommand is disabled, so it earns no page and
/// cannot shadow a directory called `help`.
const PAGES: [&str; 7] = [
    "rust-formatter.1",
    "rust-formatter-completions.1",
    "rust-formatter-man.1",
    "rust-formatter-hook.1",
    "rust-formatter-hook-install.1",
    "rust-formatter-hook-uninstall.1",
    "rust-formatter-hook-status.1",
];

fn formatter() -> Command {
    let mut cmd = Command::cargo_bin("rust-formatter").unwrap();
    cmd.env("RUST_FORMATTER_CACHE_DIR", cache_dir());
    cmd
}

fn cargo_alias() -> Command {
    let mut cmd = Command::cargo_bin("cargo-rust-formatter").unwrap();
    cmd.env("RUST_FORMATTER_CACHE_DIR", cache_dir());
    cmd
}

/// Every suite gets one cache directory of its own, so a run can never read an
/// answer another test wrote and the developer's real cache is left alone.
fn cache_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

fn stdout(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

#[test]
fn completions_print_a_script_for_every_shell() {
    for (shell, _) in SHELLS {
        let assert = formatter().args(["completions", shell]).assert().success();
        let script = stdout(&assert);
        assert!(!script.is_empty(), "{shell}: empty script");
        assert!(script.contains("rust-formatter"), "{shell}: {script}");
    }
}

/// The script has to describe *this* CLI, not merely be non-empty: a generator
/// pointed at the wrong `Command` would still print something.
#[test]
fn a_completion_script_describes_this_cli() {
    let bash = stdout(&formatter().args(["completions", "bash"]).assert().success());
    assert!(bash.contains("--staged"), "{bash}");
    assert!(bash.contains("--toml-max-width"), "{bash}");

    let zsh = stdout(&formatter().args(["completions", "zsh"]).assert().success());
    assert!(zsh.starts_with("#compdef rust-formatter"), "{zsh}");
}

#[test]
fn completions_out_dir_uses_the_conventional_name() {
    let dir = tempdir().unwrap();
    for (shell, name) in SHELLS {
        formatter()
            .args(["completions", shell, "--out-dir"])
            .arg(dir.path())
            .assert()
            .success();
        let written = dir.path().join(name);
        assert!(written.is_file(), "{shell}: expected {name}");
        assert!(!fs::read(&written).unwrap().is_empty(), "{shell}: empty");
    }
}

#[test]
fn completions_without_a_shell_is_refused() {
    let assert = formatter().arg("completions").assert().failure().code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("SHELL"), "{stderr}");
}

#[test]
fn the_man_page_is_roff() {
    let page = stdout(&formatter().arg("man").assert().success());
    assert!(page.contains(".TH rust-formatter 1"), "{page}");
    // roff escapes a hyphen, so the flag appears as `\-\-staged`.
    assert!(page.contains(r"\-\-staged"), "{page}");
}

#[test]
fn man_out_dir_writes_one_page_per_command() {
    let dir = tempdir().unwrap();
    formatter()
        .args(["man", "--out-dir"])
        .arg(dir.path())
        .assert()
        .success();

    let mut written: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    written.sort();
    let mut expected: Vec<String> = PAGES.iter().map(|page| (*page).to_string()).collect();
    expected.sort();
    assert_eq!(written, expected);
}

/// The one behaviour change the subcommands cost, and the escape from it. `--`
/// is *not* that escape: it opens the rustfmt pass-through.
#[test]
fn a_directory_named_completions_is_reached_through_a_prefix() {
    let dir = tempdir().unwrap();
    let nested = dir.path().join("completions");
    fs::create_dir(&nested).unwrap();
    let file = nested.join("a.toml");
    fs::write(&file, "foo={path=\"x\"}\n").unwrap();

    // A bare `completions` is the subcommand, so it never reaches the walk.
    formatter()
        .current_dir(dir.path())
        .args(["completions"])
        .assert()
        .failure()
        .code(2);

    formatter()
        .current_dir(dir.path())
        .args(["--check", "./completions"])
        .assert()
        .failure()
        .code(1);
    formatter()
        .current_dir(dir.path())
        .arg("./completions")
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&file).unwrap(), "foo.path = \"x\"\n");
}

#[test]
fn help_lists_the_commands() {
    let help = stdout(&formatter().arg("--help").assert().success());
    for expected in ["Commands:", "completions", "man", "hook"] {
        assert!(help.contains(expected), "{expected} missing from:\n{help}");
    }
}

/// `help` is disabled, so no `help` subcommand shadows a directory of that
/// name and no page is generated for it.
#[test]
fn there_is_no_help_subcommand() {
    formatter().arg("help").assert().failure();
}

// -- the cargo alias -------------------------------------------------------

/// Cargo invokes `cargo-rust-formatter` with `rust-formatter` as the first
/// argument. It is not a path, and the usage line has to name what was typed.
#[test]
fn the_cargo_alias_drops_the_subcommand_word() {
    let help = stdout(
        &cargo_alias()
            .args(["rust-formatter", "--help"])
            .assert()
            .success(),
    );
    assert!(help.contains("Usage: cargo rust-formatter"), "{help}");
}

#[test]
fn the_cargo_alias_reports_the_same_version() {
    let direct = stdout(&formatter().arg("--version").assert().success());
    let aliased = stdout(
        &cargo_alias()
            .args(["rust-formatter", "--version"])
            .assert()
            .success(),
    );
    assert_eq!(direct, aliased);
}

#[test]
fn the_cargo_alias_formats() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("a.toml");
    fs::write(&file, "foo={path=\"x\"}\n").unwrap();

    cargo_alias()
        .current_dir(dir.path())
        .args(["rust-formatter", "."])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&file).unwrap(), "foo.path = \"x\"\n");
}

/// A script generated through the alias must name the binary a shell will
/// complete for, not the alias that produced it.
#[test]
fn a_script_generated_through_the_alias_names_the_real_binary() {
    let script = stdout(
        &cargo_alias()
            .args(["rust-formatter", "completions", "bash"])
            .assert()
            .success(),
    );
    assert!(script.contains("rust-formatter"), "{script}");
    assert!(!script.contains("cargo-rust-formatter"), "{script}");
}
