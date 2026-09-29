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

const DIRTY_RS: &str = "fn  main( ){let x=1;}\n";
const CLEAN_RS: &str = "fn main() {\n    let x = 1;\n}\n";
const DIRTY_TOML: &str = "foo={path=\"x\"}\n";
const CLEAN_TOML: &str = "foo.path = \"x\"\n";

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

fn write(path: &Path, body: &str) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    path.to_path_buf()
}

// --------------------------------------------------------------- stream split

/// stdout carries what a caller pipes; stderr carries what a human reads.
#[test]
fn check_puts_diffs_on_stdout_and_the_verdict_on_stderr() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY_TOML);

    let out = formatter()
        .arg("--check")
        .arg(temp.path())
        .assert()
        .failure()
        .code(1)
        .stdout(predicates::str::contains("a.toml"))
        .stdout(predicates::str::contains("-foo={path=\"x\"}"))
        .stdout(predicates::str::contains("+foo.path = \"x\""))
        .stderr(predicates::str::contains("Check failed:"))
        .get_output()
        .clone();

    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!stderr.contains("a.toml"), "{stderr}");
}

#[test]
fn a_write_run_writes_nothing_to_stdout() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY_TOML);

    formatter()
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicates::str::is_empty());
}

// ---------------------------------------------------------------------- stdin

#[test]
fn stdin_formats_to_stdout() {
    needs_nightly!();

    formatter()
        .arg("--stdin")
        .write_stdin(DIRTY_RS)
        .assert()
        .success()
        .stdout(predicates::str::contains("fn main() {"))
        .stderr(predicates::str::is_empty());
}

/// rustfmt exits 0 for `--check` on stdin even after printing a diff, so the
/// verdict has to be decided by comparing the text.
#[test]
fn stdin_check_exits_one_on_a_difference() {
    needs_nightly!();

    formatter()
        .arg("--stdin")
        .arg("--check")
        .write_stdin(DIRTY_RS)
        .assert()
        .failure()
        .code(1);

    formatter()
        .arg("--stdin")
        .arg("--check")
        .write_stdin(CLEAN_RS)
        .assert()
        .success();
}

#[test]
fn stdin_filepath_selects_toml() {
    formatter()
        .arg("--stdin")
        .arg("--stdin-filepath")
        .arg("Cargo.toml")
        .write_stdin(DIRTY_TOML)
        .assert()
        .success()
        .stdout(predicates::str::diff(CLEAN_TOML.to_string()));
}

#[test]
fn stdin_filepath_requires_stdin() {
    formatter()
        .arg("--stdin-filepath")
        .arg("Cargo.toml")
        .assert()
        .failure()
        .code(2);
}

// ----------------------------------------------------------------- list modes

#[test]
fn list_files_prints_the_selection_without_formatting() {
    let temp = tempdir().unwrap();
    let dirty = write(&temp.path().join("a.toml"), DIRTY_TOML);

    formatter()
        .arg("--list-files")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("a.toml"));

    assert_eq!(fs::read_to_string(&dirty).unwrap(), DIRTY_TOML);
}

#[test]
fn list_different_prints_only_mismatched_paths() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("dirty.toml"), DIRTY_TOML);
    write(&temp.path().join("clean.toml"), CLEAN_TOML);

    let output = formatter()
        .arg("--list-different")
        .arg(temp.path())
        .assert()
        .failure()
        .code(1)
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("dirty.toml"), "{stdout}");
    assert!(!stdout.contains("clean.toml"), "{stdout}");
    // Names, never a diff: the point is that the list is pipeable.
    assert!(!stdout.contains("+foo.path"), "{stdout}");
}

#[test]
fn list_different_on_a_clean_tree_prints_nothing_and_exits_zero() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("clean.toml"), CLEAN_TOML);

    formatter()
        .arg("--list-different")
        .arg(temp.path())
        .assert()
        .success()
        .stdout(predicates::str::is_empty());
}

#[test]
fn list_different_leaves_the_file_alone() {
    let temp = tempdir().unwrap();
    let dirty = write(&temp.path().join("dirty.toml"), DIRTY_TOML);

    formatter()
        .arg("--list-different")
        .arg(temp.path())
        .assert()
        .failure();

    assert_eq!(fs::read_to_string(&dirty).unwrap(), DIRTY_TOML);
}

// --------------------------------------------------------------- message-format

#[test]
fn json_reports_every_verdict_on_stdout() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY_TOML);

    let output = formatter()
        .arg("--check")
        .arg("--message-format")
        .arg("json")
        .arg(temp.path())
        .assert()
        .failure()
        .code(1)
        .get_output()
        .clone();

    let text = String::from_utf8(output.stdout).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).expect("stdout is JSON");

    assert_eq!(value["version"], 3);
    assert_eq!(value["mode"], "check");
    assert_eq!(value["exit_code"], 1);
    assert_eq!(value["summary"]["changed"], 1);
    assert_eq!(value["files"][0]["language"], "toml");
    assert_eq!(value["files"][0]["status"], "needs-formatting");

    let diff = value["files"][0]["diff"].as_str().expect("a diff");
    assert!(diff.contains("+foo.path"), "{diff}");
    // A JSON-encoded escape survives stream-level stripping, so the diff
    // embedded here must never have been painted.
    assert!(!diff.contains('\u{1b}'), "{diff}");
}

#[test]
fn json_suppresses_the_human_summary() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), CLEAN_TOML);

    formatter()
        .arg("--message-format")
        .arg("json")
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicates::str::is_empty());
}

/// `-q` silences commentary, not the product. rustfmt's own `--quiet` still
/// prints its diff, so suppressing the TOML one would split the two apart.
#[test]
fn quiet_silences_the_summary_but_not_the_diff() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY_TOML);

    formatter()
        .arg("-q")
        .arg("--check")
        .arg(temp.path())
        .assert()
        .failure()
        .code(1)
        .stdout(predicates::str::contains("+foo.path"))
        .stderr(predicates::str::is_empty());
}

// -------------------------------------------------------------------- colour

#[test]
fn color_never_reaches_the_rustfmt_diff() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    write(&temp.path().join("a.rs"), DIRTY_RS);

    formatter()
        .arg("--check")
        .arg("--color")
        .arg("never")
        .arg(temp.path())
        .assert()
        .failure()
        .stdout(predicates::str::contains("@@ "))
        .stdout(predicates::str::contains("\u{1b}").not());
}

#[test]
fn color_always_paints_the_rustfmt_diff_through_a_pipe() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    write(&temp.path().join("a.rs"), DIRTY_RS);

    formatter()
        .arg("--check")
        .arg("--color")
        .arg("always")
        .arg(temp.path())
        .assert()
        .failure()
        .stdout(predicates::str::contains("\u{1b}"));
}

#[test]
fn color_never_leaves_the_toml_diff_plain() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY_TOML);

    formatter()
        .arg("--check")
        .arg("--color")
        .arg("never")
        .arg(temp.path())
        .assert()
        .failure()
        .stdout(predicates::str::contains("\u{1b}").not());
}

// ------------------------------------------------------------- honest counts

#[test]
fn a_clean_tree_is_never_reported_as_formatted() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), CLEAN_TOML);
    write(&temp.path().join("b.toml"), CLEAN_TOML);

    formatter()
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("Already formatted:"))
        .stderr(predicates::str::contains("Formatted 2").not());
}

#[test]
fn the_summary_counts_written_files_not_selected_ones() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("dirty.toml"), DIRTY_TOML);
    write(&temp.path().join("clean1.toml"), CLEAN_TOML);
    write(&temp.path().join("clean2.toml"), CLEAN_TOML);

    formatter()
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("Formatted 1 file\n"));
}

// ------------------------------------------------------------------- config

#[test]
fn print_config_reports_the_effective_settings() {
    needs_nightly!();

    formatter()
        .arg("--print-config")
        .assert()
        .success()
        .stdout(predicates::str::contains("imports_granularity = \"Crate\""))
        .stdout(predicates::str::contains(
            "group_imports = \"StdExternalCrate\"",
        ));
}

/// `--config` can overwrite a default but never drop it, which is what
/// `--unset-config` exists for.
#[test]
fn unset_config_restores_the_rustfmt_default() {
    needs_nightly!();

    formatter()
        .arg("--print-config")
        .arg("--unset-config")
        .arg("imports_granularity")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "imports_granularity = \"Preserve\"",
        ))
        .stdout(predicates::str::contains(
            "group_imports = \"StdExternalCrate\"",
        ));
}

#[test]
fn unset_config_changes_how_files_are_formatted() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let file = write(
        &temp.path().join("a.rs"),
        "use std::fs;\nuse std::io;\n\nfn main() {\n    let _ = (fs::metadata(\".\"), io::stdin());\n}\n",
    );

    formatter().arg(&file).assert().success();
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("use std::{fs, io};")
    );

    let other = write(
        &temp.path().join("b.rs"),
        "use std::fs;\nuse std::io;\n\nfn main() {\n    let _ = (fs::metadata(\".\"), io::stdin());\n}\n",
    );
    formatter()
        .arg("--unset-config")
        .arg("imports_granularity")
        .arg(&other)
        .assert()
        .success();
    let body = fs::read_to_string(&other).unwrap();
    assert!(body.contains("use std::fs;"), "{body}");
    assert!(body.contains("use std::io;"), "{body}");
}

// ------------------------------------------------------------------- edition

#[test]
fn an_unknown_edition_is_rejected_by_the_parser() {
    formatter()
        .arg("--edition")
        .arg("2020")
        .arg(".")
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("invalid value '2020'"))
        .stderr(predicates::str::contains("2015"));
}

#[test]
fn every_rustfmt_edition_is_accepted() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let file = write(&temp.path().join("a.rs"), DIRTY_RS);

    for edition in ["2015", "2018", "2021", "2024"] {
        formatter()
            .arg("--edition")
            .arg(edition)
            .arg(&file)
            .assert()
            .success();
    }
}

// ---------------------------------------------------------------------- emit

#[test]
fn emit_stdout_leaves_the_file_alone() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let file = write(&temp.path().join("a.rs"), DIRTY_RS);

    formatter()
        .arg("--emit")
        .arg("stdout")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicates::str::contains("fn main() {"));

    assert_eq!(fs::read_to_string(&file).unwrap(), DIRTY_RS);
}

#[test]
fn emit_stdout_does_not_call_a_file_it_would_change_formatted() {
    let temp = tempdir().unwrap();
    let file = write(&temp.path().join("a.toml"), DIRTY_TOML);

    formatter()
        .arg("--emit")
        .arg("stdout")
        .arg(&file)
        .assert()
        .success()
        .stdout(CLEAN_TOML)
        .stderr(predicates::str::contains("Previewed:"))
        .stderr(predicates::str::contains("Already formatted").not());

    assert_eq!(fs::read_to_string(&file).unwrap(), DIRTY_TOML);
}

#[test]
fn emit_stdout_needs_a_single_target() {
    let temp = tempdir().unwrap();
    let one = write(&temp.path().join("a.toml"), DIRTY_TOML);
    let two = write(&temp.path().join("b.toml"), DIRTY_TOML);

    formatter()
        .arg("--emit")
        .arg("stdout")
        .arg(&one)
        .arg(&two)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("exactly one PATH"));
}

// ------------------------------------------------------------------- verbose

#[test]
fn verbose_reports_what_only_the_run_can_know() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    write(&temp.path().join("a.rs"), CLEAN_RS);

    formatter()
        .arg("-v")
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("rustfmt: "))
        .stderr(predicates::str::contains("config: group_imports="))
        .stderr(predicates::str::contains("jobs: "))
        .stderr(predicates::str::contains("edition: "))
        .stderr(predicates::str::contains("strategy: "));
}

// ------------------------------------------------------- workspace reporting

#[test]
fn a_workspace_run_names_the_workspace_root() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    write(
        &temp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/member\"]\nresolver = \"3\"\n",
    );
    write(
        &temp.path().join("crates/member/Cargo.toml"),
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &temp.path().join("crates/member/src/lib.rs"),
        "pub fn ok() {}\n",
    );

    let output = formatter()
        .arg(temp.path().join("crates/member"))
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Cargo project"), "{stderr}");
    assert!(!stderr.contains("crates/member"), "{stderr}");
}

// ------------------------------------------------------ the two-language diff

/// rustfmt's own `--check` prints `Diff in <path>:<line>:` and bare `-`/`+`
/// lines with no `---`/`+++`/`@@` headers, so it cannot be applied. The TOML
/// half already produced a real unified diff; both halves now do.
#[test]
fn a_rust_check_diff_is_unified_and_appliable() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let file = write(&temp.path().join("a.rs"), DIRTY_RS);

    let assert = formatter()
        .arg("--check")
        .arg("--color")
        .arg("never")
        .arg(temp.path())
        .assert()
        .failure()
        .code(1);
    let diff = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(diff.contains(&format!("--- {}", file.display())), "{diff}");
    assert!(diff.contains(&format!("+++ {}", file.display())), "{diff}");
    assert!(diff.contains("@@ "), "{diff}");
    assert!(!diff.contains("Diff in "), "{diff}");

    let patch = temp.path().join("d.patch");
    fs::write(&patch, &diff).unwrap();
    let applied = StdCommand::new("git")
        .args(["apply", "--check", "--unsafe-paths", "-p0"])
        .arg(&patch)
        .current_dir("/")
        .output()
        .unwrap();
    assert!(
        applied.status.success(),
        "git apply rejected the diff: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
}

/// The diff a `--check` prints and the diff the JSON report carries have to be
/// the same text, or a machine consumer and a human are reading different runs.
#[test]
fn the_json_report_carries_the_same_rust_diff() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    write(&temp.path().join("a.rs"), DIRTY_RS);

    let human = formatter()
        .arg("--check")
        .arg("--color")
        .arg("never")
        .arg(temp.path())
        .assert()
        .failure();
    let printed = String::from_utf8(human.get_output().stdout.clone()).unwrap();

    let machine = formatter()
        .arg("--check")
        .arg("--message-format")
        .arg("json")
        .arg(temp.path())
        .assert()
        .failure();
    let report: serde_json::Value = serde_json::from_slice(&machine.get_output().stdout).unwrap();
    let diff = report["files"][0]["diff"].as_str().unwrap();

    assert_eq!(report["files"][0]["language"], "rust");
    assert_eq!(printed.trim_end(), diff.trim_end());
}

// --------------------------------------------------------- per-file reporting

/// One error per file, the way an unparseable TOML file is already reported,
/// rather than one per rustfmt invocation carrying the whole captured stderr.
#[test]
fn a_rust_parse_error_is_reported_against_its_own_file() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let first = write(&temp.path().join("a.rs"), "fn main() {\n    let x = ;\n}\n");
    let second = write(&temp.path().join("b.rs"), "fn f( {\n");
    write(&temp.path().join("ok.rs"), CLEAN_RS);

    formatter()
        .arg("--check")
        .arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(format!(
            "{}:2:13: error",
            first.display()
        )))
        .stderr(predicates::str::contains(format!(
            "{}:1:",
            second.display()
        )));
}

// ---------------------------------------------------------- converged preview

/// A write run formats to a fixed point, so a preview built from one pass could
/// differ from the bytes the same command would have written.
#[test]
fn the_preview_is_what_a_write_run_would_leave() {
    needs_nightly!();

    let dirty = "use std::io::Write;\nuse b::B;\nuse std::io::Read;\nfn  main( ){let x=1;}\n";
    let temp = tempdir().unwrap();
    let previewed = write(&temp.path().join("a.rs"), dirty);

    let assert = formatter()
        .arg("--emit")
        .arg("stdout")
        .arg(&previewed)
        .assert()
        .success();
    let preview = assert.get_output().stdout.clone();

    // Framed like the TOML half: the text, and nothing else.
    assert!(
        !String::from_utf8_lossy(&preview).contains(&previewed.display().to_string()),
        "the preview should carry no path header"
    );
    assert_eq!(
        fs::read_to_string(&previewed).unwrap(),
        dirty,
        "a preview writes nothing"
    );

    let written = write(&temp.path().join("b.rs"), dirty);
    formatter().arg(&written).assert().success();
    assert_eq!(preview, fs::read(&written).unwrap());
}

// ------------------------------------------------------------ config validity

/// rustfmt splits every `--config` occurrence on `,` and rejects an array
/// outright, so the two array-valued options had no command-line spelling.
#[test]
fn an_array_valued_config_option_reaches_rustfmt() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    write(&temp.path().join("a.rs"), CLEAN_RS);

    formatter()
        .arg("--check")
        .arg("--verbose")
        .arg("--config")
        .arg(r#"skip_macro_invocations=["a","b"]"#)
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicates::str::contains(
            r#"config file: skip_macro_invocations=["a","b"]"#,
        ));
}

/// `ignore` is rustfmt's file selection, resolved against the directory of the
/// configuration file it was read from -- which this tool writes nowhere near
/// the project. The selection layer carries the same patterns, and reaches TOML.
#[test]
fn an_ignore_list_selects_rather_than_reaching_rustfmt() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    write(&temp.path().join("keep.rs"), DIRTY_RS);
    write(&temp.path().join("skip.rs"), DIRTY_RS);
    write(&temp.path().join("skip.toml"), DIRTY_TOML);

    let assert = formatter()
        .arg("--check")
        .arg("--color")
        .arg("never")
        .arg("--config")
        .arg(r#"ignore=["skip.rs","skip.toml"]"#)
        .arg(temp.path())
        .assert()
        .failure()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("keep.rs"), "{out}");
    assert!(!out.contains("skip.rs"), "{out}");
    assert!(!out.contains("skip.toml"), "{out}");
}

/// A bare key used to become `key=true` and die inside rustfmt as
/// `invalid key=val pair: max_width=true`, well after argument parsing.
#[test]
fn a_bare_config_key_fails_at_argument_parse_time() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.rs"), CLEAN_RS);

    formatter()
        .arg("--config")
        .arg("max_width")
        .arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("expected KEY=VALUE"));
}

/// An option name rustfmt does not have is a typo, and a typo that exits 0 is
/// the failure mode `--unset-config` had.
#[test]
fn unsetting_an_option_rustfmt_does_not_have_is_an_error() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    write(&temp.path().join("a.rs"), CLEAN_RS);

    formatter()
        .arg("--check")
        .arg("--unset-config")
        .arg("group_imprts")
        .arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("has no option `group_imprts`"));

    // A real option this run never set is a no-op, but a named one.
    formatter()
        .arg("--check")
        .arg("--unset-config")
        .arg("max_width")
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("nothing to drop"));
}

// ------------------------------------------------------------- rustfmt surface

/// `--style-edition` pins formatting behaviour across rustfmt releases, which is
/// what `--edition` does not do.
#[test]
fn style_edition_reaches_rustfmt() {
    needs_nightly!();

    formatter()
        .arg("--print-config")
        .arg("--style-edition")
        .arg("2015")
        .assert()
        .success()
        .stdout(predicates::str::contains("style_edition = \"2015\""));

    formatter()
        .arg("--print-config")
        .arg("--style-edition")
        .arg("2024")
        .arg("--config")
        .arg("style_edition=2021")
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("conflicts with --config"));
}

/// What an editor's format-selection request needs: everything outside the
/// named lines is left exactly as it was.
#[test]
fn a_range_formats_only_the_lines_it_names() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let file = write(
        &temp.path().join("a.rs"),
        "fn  a( ){}\nfn  b( ){}\nfn  c( ){}\n",
    );

    formatter()
        .arg("--range")
        .arg("2")
        .arg(&file)
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "fn  a( ){}\nfn b() {}\nfn  c( ){}\n"
    );

    // A column is accepted and widened, because rustfmt works in whole lines.
    formatter()
        .arg("--range")
        .arg("3:1-3:11")
        .arg(&file)
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "fn  a( ){}\nfn b() {}\nfn c() {}\n"
    );

    formatter()
        .arg("--range")
        .arg("1")
        .arg(temp.path().join("a.toml"))
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("--range formats Rust only"));
}

#[test]
fn a_range_never_reaches_the_lines_its_first_pass_moved() {
    needs_nightly!();

    let source = "fn main() {\n    foo(\n        a,\n        b,\n    );\n    let   x   =   1;\n}\n";
    let expected = "fn main() {\n    foo(a, b);\n    let   x   =   1;\n}\n";
    let temp = tempdir().unwrap();
    let written = write(&temp.path().join("written.rs"), source);
    let previewed = write(&temp.path().join("previewed.rs"), source);

    formatter()
        .args(["--range", "2-5"])
        .arg(&written)
        .assert()
        .success();
    let preview = formatter()
        .args(["--emit", "stdout", "--range", "2-5"])
        .arg(&previewed)
        .assert()
        .success();
    let stdin = formatter()
        .args(["--stdin", "--stdin-filepath", "a.rs", "--range", "2-5"])
        .write_stdin(source)
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&written).unwrap(), expected);
    assert_eq!(
        String::from_utf8_lossy(&preview.get_output().stdout),
        expected
    );
    assert_eq!(
        String::from_utf8_lossy(&stdin.get_output().stdout),
        expected
    );
}

#[test]
fn a_range_reaches_the_stdin_path_too() {
    needs_nightly!();

    formatter()
        .args(["--stdin", "--stdin-filepath", "a.rs", "--range", "1"])
        .write_stdin("fn  a( ){}\nfn  b( ){}\n")
        .assert()
        .success()
        .stdout("fn a() {}\nfn  b( ){}\n");
}

/// The presets are the point: one name instead of a dozen `--config` strings,
/// and an explicit flag still wins over the preset.
#[test]
fn a_rust_style_preset_sets_a_group_of_options() {
    needs_nightly!();

    formatter()
        .args(["--print-config", "--rust-style", "strict"])
        .assert()
        .success()
        .stdout(predicates::str::contains("wrap_comments = true"))
        .stdout(predicates::str::contains("hex_literal_case = \"Upper\""))
        .stdout(predicates::str::contains("format_strings = true"));

    formatter()
        .args(["--print-config", "--rust-style", "literals"])
        .assert()
        .success()
        .stdout(predicates::str::contains("hex_literal_case = \"Upper\""))
        .stdout(predicates::str::contains("wrap_comments = false"));

    formatter()
        .args([
            "--print-config",
            "--rust-style",
            "comments",
            "--config",
            "wrap_comments=false",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("wrap_comments = false"))
        .stdout(predicates::str::contains("normalize_comments = true"));
}

// -------------------------------------------------- a total machine interface

/// Every mode either produces the envelope or refuses the combination. Four of
/// them used to return before the JSON branch and print human output instead.
fn json_of(command: &mut Command) -> serde_json::Value {
    let output = command.assert().get_output().clone();
    let text = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{err}: {text}"))
}

#[test]
fn list_files_reports_json_when_asked() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), CLEAN_TOML);

    let value = json_of(
        formatter()
            .arg("--list-files")
            .arg("--message-format")
            .arg("json")
            .arg(temp.path()),
    );

    assert_eq!(value["version"], 3);
    assert_eq!(value["mode"], "list-files");
    assert_eq!(value["summary"]["selected"], 1);
    assert_eq!(value["files"][0]["language"], "toml");
    // A listing names files; it does not judge them.
    assert!(value["files"][0].get("status").is_none(), "{value}");
    assert_eq!(value["exit_code"], 0);
}

#[test]
fn list_different_json_names_files_without_diffs() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("dirty.toml"), DIRTY_TOML);
    write(&temp.path().join("clean.toml"), CLEAN_TOML);

    let value = json_of(
        formatter()
            .arg("--list-different")
            .arg("--message-format")
            .arg("json")
            .arg(temp.path()),
    );

    assert_eq!(value["mode"], "list-different");
    assert_eq!(value["summary"]["changed"], 1);
    assert_eq!(value["exit_code"], 1);
    assert_eq!(value["files"][0]["status"], "needs-formatting");
    assert!(
        value["files"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("dirty.toml")
    );
    // The point of a list is that it is not a diff.
    assert!(value["files"][0].get("diff").is_none(), "{value}");
}

#[test]
fn print_config_reports_json_when_asked() {
    needs_nightly!();
    let value = json_of(
        formatter()
            .arg("--print-config")
            .arg("--message-format")
            .arg("json"),
    );

    assert_eq!(value["mode"], "print-config");
    // rustfmt reports TOML; a JSON consumer gets typed values, not a blob.
    assert!(value["config"]["max_width"].is_number(), "{value}");
    assert_eq!(value["config"]["imports_granularity"], "Crate");
    assert_eq!(value["config"]["group_imports"], "StdExternalCrate");
}

#[test]
fn stdin_reports_json_when_asked() {
    let value = json_of(
        formatter()
            .arg("--stdin")
            .arg("--stdin-filepath")
            .arg("x.toml")
            .arg("--message-format")
            .arg("json")
            .write_stdin(DIRTY_TOML),
    );

    assert_eq!(value["mode"], "stdin");
    assert_eq!(value["content"], CLEAN_TOML);
    assert_eq!(value["files"][0]["path"], "x.toml");
    assert_eq!(value["files"][0]["language"], "toml");
    assert_eq!(value["exit_code"], 0);
}

#[test]
fn stdin_check_json_carries_a_diff() {
    let value = json_of(
        formatter()
            .arg("--stdin")
            .arg("--stdin-filepath")
            .arg("x.toml")
            .arg("--check")
            .arg("--message-format")
            .arg("json")
            .write_stdin(DIRTY_TOML),
    );

    assert_eq!(value["files"][0]["status"], "needs-formatting");
    assert_eq!(value["exit_code"], 1);
    let diff = value["files"][0]["diff"].as_str().expect("a diff");
    assert!(diff.contains("+foo.path"), "{diff}");
    // A check reports on the buffer; it does not hand it back.
    assert!(value.get("content").is_none(), "{value}");
}

#[test]
fn emit_stdout_reports_json_when_asked() {
    let temp = tempdir().unwrap();
    let file = write(&temp.path().join("a.toml"), DIRTY_TOML);

    let value = json_of(
        formatter()
            .arg("--emit")
            .arg("stdout")
            .arg("--message-format")
            .arg("json")
            .arg(&file),
    );

    assert_eq!(value["mode"], "preview");
    assert_eq!(value["content"], CLEAN_TOML);
    assert_eq!(fs::read_to_string(&file).unwrap(), DIRTY_TOML);
}

// ------------------------------------------------------- errors as data

#[test]
fn a_toml_parse_error_is_structured_in_json() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("bad.toml"), "a = 1\nb = = 2\n");

    let value = json_of(
        formatter()
            .arg("--check")
            .arg("--message-format")
            .arg("json")
            .arg(temp.path()),
    );

    assert_eq!(value["exit_code"], 2);
    let error = &value["errors"][0];
    assert_eq!(error["code"], "toml-parse");
    assert!(error["path"].as_str().unwrap().ends_with("bad.toml"));
    assert_eq!(error["line"], 2);
    assert_eq!(error["column"], 5);
    // The message is the reason alone: the location is carried as data.
    let message = error["message"].as_str().unwrap();
    assert!(!message.contains("bad.toml"), "{message}");
    assert!(!message.contains("TOML parse error at line"), "{message}");
}

#[test]
fn a_failed_file_is_listed_with_an_error_status() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("bad.toml"), "a = = 1\n");

    let value = json_of(
        formatter()
            .arg("--message-format")
            .arg("json")
            .arg(temp.path()),
    );

    assert_eq!(value["summary"]["errors"], 1);
    assert_eq!(value["files"][0]["status"], "error");
    assert_eq!(value["files"][0]["language"], "toml");
}

#[test]
fn a_rust_parse_error_is_structured_in_json() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    write(&temp.path().join("bad.rs"), "fn main() { let x = ; }\n");

    let value = json_of(
        formatter()
            .arg("--check")
            .arg("--message-format")
            .arg("json")
            .arg(temp.path()),
    );

    let error = &value["errors"][0];
    assert_eq!(error["code"], "rustfmt-diagnostic");
    assert!(error["path"].as_str().unwrap().ends_with("bad.rs"));
    assert!(error["line"].is_number(), "{value}");
    assert!(error["column"].is_number(), "{value}");
}

// ------------------------------------------------- a preview is a single file

#[test]
fn emit_stdout_refuses_a_directory_of_many_files() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY_TOML);
    write(&temp.path().join("b.toml"), DIRTY_TOML);

    formatter()
        .arg("--emit")
        .arg("stdout")
        .arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("resolves to 2 files"));

    // Refusing is the point: neither file may be rewritten either.
    assert_eq!(
        fs::read_to_string(temp.path().join("a.toml")).unwrap(),
        DIRTY_TOML
    );
}

#[test]
fn emit_stdout_refuses_a_listing() {
    let temp = tempdir().unwrap();
    let file = write(&temp.path().join("a.toml"), DIRTY_TOML);

    formatter()
        .arg("--emit")
        .arg("stdout")
        .arg("--list-different")
        .arg(&file)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "cannot be combined with a listing",
        ));
}

/// A preview is what a write run would leave, byte for byte -- which includes
/// the byte-order mark and the line endings the file arrived with.
#[test]
fn a_preview_keeps_the_bom_and_crlf_a_write_run_would() {
    let temp = tempdir().unwrap();
    let source = "\u{feff}foo={path=\"x\"}\r\n";
    let previewed = temp.path().join("preview.toml");
    let written = temp.path().join("written.toml");
    fs::write(&previewed, source).unwrap();
    fs::write(&written, source).unwrap();

    let out = formatter()
        .arg("--emit")
        .arg("stdout")
        .arg(&previewed)
        .assert()
        .success()
        .get_output()
        .clone();

    formatter().arg(&written).assert().success();
    assert_eq!(out.stdout, fs::read(&written).unwrap());
    assert_eq!(out.stdout, b"\xef\xbb\xbffoo.path = \"x\"\r\n");
}

// ------------------------------------------------------ stdin carries its bytes

#[test]
fn stdin_check_accepts_a_crlf_buffer_that_needs_nothing() {
    formatter()
        .arg("--stdin")
        .arg("--stdin-filepath")
        .arg("x.toml")
        .arg("--check")
        .write_stdin("foo.path = \"x\"\r\n")
        .assert()
        .success()
        .code(0);
}

#[test]
fn stdin_restores_the_bom_and_crlf_it_was_given() {
    let out = formatter()
        .arg("--stdin")
        .arg("--stdin-filepath")
        .arg("x.toml")
        .write_stdin("\u{feff}foo={path=\"x\"}\r\n")
        .assert()
        .success()
        .get_output()
        .clone();

    assert_eq!(out.stdout, b"\xef\xbb\xbffoo.path = \"x\"\r\n");
}

#[test]
fn rust_stdin_check_accepts_a_crlf_buffer_that_needs_nothing() {
    needs_nightly!();
    formatter()
        .arg("--stdin")
        .arg("--stdin-filepath")
        .arg("x.rs")
        .arg("--check")
        .write_stdin("fn main() {}\r\n")
        .assert()
        .success()
        .code(0);
}

#[test]
fn rust_stdin_restores_the_bom_and_crlf_it_was_given() {
    needs_nightly!();
    let out = formatter()
        .arg("--stdin")
        .arg("--stdin-filepath")
        .arg("x.rs")
        .write_stdin("\u{feff}fn  main() {}\r\n")
        .assert()
        .success()
        .get_output()
        .clone();

    assert_eq!(out.stdout, b"\xef\xbb\xbffn main() {}\r\n");
}

#[test]
fn rust_preview_and_write_keep_the_bom_and_crlf() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let source = "\u{feff}fn  main() {}\r\n";
    let previewed = temp.path().join("preview.rs");
    let written = temp.path().join("written.rs");
    fs::write(&previewed, source).unwrap();
    fs::write(&written, source).unwrap();

    let out = formatter()
        .arg("--emit")
        .arg("stdout")
        .arg(&previewed)
        .assert()
        .success()
        .get_output()
        .clone();

    formatter().arg(&written).assert().success();
    assert_eq!(out.stdout, fs::read(&written).unwrap());
    assert_eq!(out.stdout, b"\xef\xbb\xbffn main() {}\r\n");
}

#[test]
fn rust_check_accepts_a_clean_crlf_file() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("main.rs");
    fs::write(&file, "fn main() {}\r\n").unwrap();
    formatter()
        .arg("--check")
        .arg(&file)
        .assert()
        .success()
        .code(0);
}

// ------------------------------------------------------------- diff and lists

#[test]
fn diff_context_sets_the_number_of_context_lines() {
    let temp = tempdir().unwrap();
    let body = "a = 1\nb = 2\nc = 3\nd = 4\ne={f=\"g\"}\n";
    write(&temp.path().join("a.toml"), body);

    let tight = formatter()
        .arg("--check")
        .arg("-U")
        .arg("1")
        .arg(temp.path())
        .assert()
        .failure()
        .get_output()
        .clone();
    let tight = String::from_utf8(tight.stdout).unwrap();

    assert!(tight.contains(" d = 4\n"), "{tight}");
    assert!(!tight.contains(" a = 1\n"), "{tight}");

    let none = formatter()
        .arg("--check")
        .arg("--diff-context")
        .arg("0")
        .arg(temp.path())
        .assert()
        .failure()
        .get_output()
        .clone();
    let none = String::from_utf8(none.stdout).unwrap();
    assert!(!none.contains(" d = 4\n"), "{none}");
}

#[test]
fn dash_l_is_list_different() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("dirty.toml"), DIRTY_TOML);
    write(&temp.path().join("clean.toml"), CLEAN_TOML);

    formatter()
        .arg("-l")
        .arg(temp.path())
        .assert()
        .failure()
        .code(1)
        .stdout(predicates::str::contains("dirty.toml"))
        .stdout(predicates::str::contains("clean.toml").not());
}

#[test]
fn help_groups_the_flags() {
    let out = formatter()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8(out.stdout).unwrap();

    for heading in [
        "Selecting files:",
        "Git scoping:",
        "Output:",
        "Standard input:",
        "Rust formatting:",
        "TOML layout:",
        "TOML ordering:",
        "Dependency versions:",
        "Execution:",
        "Commands:",
    ] {
        assert!(text.contains(heading), "missing {heading}\n{text}");
    }
    assert!(text.contains("Examples:"), "{text}");
}

/// Naming `Cargo.toml` means the cargo project everywhere else, which is what a
/// write run wants. A preview is one buffer, so the named file wins.
#[test]
fn a_named_manifest_is_previewed_alone() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    write(
        &temp.path().join("Cargo.toml"),
        "[package]\nname=\"a\"\nversion  =  \"1\"\n",
    );
    write(&temp.path().join("src/lib.rs"), DIRTY_RS);

    let out = formatter()
        .arg("--emit")
        .arg("stdout")
        .arg(temp.path().join("Cargo.toml"))
        .assert()
        .success()
        .get_output()
        .clone();

    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "[package]\nname = \"a\"\nversion = \"1\"\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("src/lib.rs")).unwrap(),
        DIRTY_RS
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("Cargo.toml"), "{stderr}");
}

/// A caller that asked for JSON gets JSON for the failure too. A fatal error
/// used to leave prose on stderr and an unparseable empty stdout.
#[test]
fn a_fatal_error_is_reported_as_json_too() {
    let value = json_of(
        formatter()
            .arg("--message-format")
            .arg("json")
            .arg("--stdin")
            .arg("--stdin-filepath")
            .arg("x.toml")
            .write_stdin("a = = 1\n"),
    );

    assert_eq!(value["mode"], "stdin");
    assert_eq!(value["exit_code"], 2);
    assert_eq!(value["errors"][0]["code"], "toml-parse");
    assert_eq!(value["errors"][0]["line"], 1);
    assert_eq!(value["errors"][0]["column"], 5);
}

#[test]
fn a_refused_preview_is_reported_as_json_too() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), DIRTY_TOML);
    write(&temp.path().join("b.toml"), DIRTY_TOML);

    let value = json_of(
        formatter()
            .arg("--emit")
            .arg("stdout")
            .arg("--message-format")
            .arg("json")
            .arg(temp.path()),
    );

    assert_eq!(value["mode"], "preview");
    assert_eq!(value["exit_code"], 2);
    assert_eq!(value["errors"][0]["code"], "preview-not-single-file");
}

#[track_caller]
fn json_failure(command: &mut Command) -> serde_json::Value {
    let assert = command.assert().code(2).stderr(predicates::str::is_empty());
    let text = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout is UTF-8");
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{err}: {text}"))
}

#[test]
fn a_configuration_failure_is_reported_as_json_too() {
    let temp = tempdir().unwrap();
    write(
        &temp.path().join("rust-formatter.toml"),
        "sort_deps = true\n",
    );
    write(&temp.path().join("a.toml"), CLEAN_TOML);

    let value = json_failure(formatter().current_dir(temp.path()).args([
        "--check",
        "--message-format",
        "json",
    ]));

    assert_eq!(value["mode"], "check");
    assert_eq!(value["exit_code"], 2);
    assert_eq!(value["errors"][0]["code"], "config");
    assert!(
        value["errors"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("rust-formatter.toml")
    );
}

#[test]
fn a_configuration_failure_honours_json_from_the_environment() {
    let temp = tempdir().unwrap();
    write(
        &temp.path().join("rust-formatter.toml"),
        "sort_deps = true\n",
    );

    let value = json_failure(
        formatter()
            .current_dir(temp.path())
            .env("RUST_FORMATTER_MESSAGE_FORMAT", "json")
            .arg("--list-files"),
    );

    assert_eq!(value["mode"], "list-files");
    assert_eq!(value["errors"][0]["code"], "config");
}

#[test]
fn a_refused_combination_is_reported_as_json_too() {
    let temp = tempdir().unwrap();
    write(&temp.path().join("a.toml"), CLEAN_TOML);

    for (args, message) in [
        (
            &["--sort-grouped", "--toml-max-blank-lines", "0"][..],
            "--sort-grouped cannot be combined with --toml-max-blank-lines 0",
        ),
        (
            &["--config", "max_width"][..],
            "expected KEY=VALUE in --config, found `max_width`",
        ),
        (
            &["--edition", "2021", "--config", "edition=2018"][..],
            "--edition 2021 conflicts with --config edition=2018",
        ),
    ] {
        let value = json_failure(
            formatter()
                .current_dir(temp.path())
                .args(["--message-format", "json"])
                .args(args),
        );

        assert_eq!(value["mode"], "write", "{args:?}");
        assert_eq!(value["exit_code"], 2, "{args:?}");
        assert_eq!(value["errors"][0]["code"], "usage", "{args:?}");
        assert_eq!(value["errors"][0]["message"], message, "{args:?}");
    }
}

#[test]
fn a_configuration_failure_stays_plain_without_json() {
    let temp = tempdir().unwrap();
    write(
        &temp.path().join("rust-formatter.toml"),
        "sort_deps = true\n",
    );

    formatter()
        .current_dir(temp.path())
        .arg("--check")
        .assert()
        .code(2)
        .stdout(predicates::str::is_empty())
        .stderr(predicates::str::starts_with("error: "));
}
