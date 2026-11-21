#[path = "support/toolchain.rs"]
mod toolchain;

use std::{fs, path::Path, sync::OnceLock};

use assert_cmd::Command;
use tempfile::{TempDir, tempdir};
use toolchain::{needs_nightly, needs_stable, nightly_available, stable_available};

const GROUPED: &str = "fn main() {
    let x = 1;
    let y = 2;
    let z = x + y;
    println!(\"{z}\");
}
";

const PARAGRAPHED: &str = "fn main() {
    let x = 1;
    let y = 2;

    let z = x + y;

    println!(\"{z}\");
}
";

fn cache_dir() -> &'static Path {
    static DIR: OnceLock<TempDir> = OnceLock::new();
    DIR.get_or_init(|| tempdir().unwrap()).path()
}

fn formatter() -> Command {
    let mut cmd = Command::cargo_bin("rust-formatter").unwrap();

    cmd.env("RUST_FORMATTER_CACHE_DIR", cache_dir());
    cmd
}

fn loose_file(contents: &str) -> (TempDir, std::path::PathBuf) {
    let temp = tempdir().unwrap();
    let file = temp.path().join("main.rs");

    fs::write(&file, contents).unwrap();
    (temp, file)
}

#[test]
fn writing_inserts_paragraphs_and_check_then_passes() {
    needs_nightly!();

    let (_temp, file) = loose_file(GROUPED);

    formatter().arg(&file).assert().success();

    assert_eq!(fs::read_to_string(&file).unwrap(), PARAGRAPHED);

    formatter().arg("--check").arg(&file).assert().success();
}

#[test]
fn check_flags_a_file_rustfmt_alone_calls_clean() {
    needs_nightly!();

    let (_temp, file) = loose_file(GROUPED);

    formatter()
        .arg("--check")
        .arg(&file)
        .assert()
        .failure()
        .code(1);

    assert_eq!(fs::read_to_string(&file).unwrap(), GROUPED);
}

#[test]
fn check_on_stable_flags_missing_paragraphs() {
    needs_stable!();

    let (_temp, file) = loose_file(GROUPED);

    formatter()
        .args(["--toolchain", "stable", "--check"])
        .arg(&file)
        .assert()
        .failure()
        .code(1);

    formatter()
        .args(["--toolchain", "stable"])
        .arg(&file)
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&file).unwrap(), PARAGRAPHED);
}

#[test]
fn no_paragraphs_leaves_statements_as_rustfmt_wrote_them() {
    needs_nightly!();

    let (_temp, file) = loose_file(GROUPED);

    formatter()
        .args(["--no-paragraphs", "--check"])
        .arg(&file)
        .assert()
        .success();
    formatter()
        .arg("--no-paragraphs")
        .arg(&file)
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&file).unwrap(), GROUPED);
}

#[test]
fn a_config_file_can_turn_paragraphs_off() {
    needs_nightly!();

    let (temp, file) = loose_file(GROUPED);

    fs::write(
        temp.path().join("rust-formatter.toml"),
        "paragraphs = false\n",
    )
    .unwrap();

    formatter()
        .current_dir(temp.path())
        .arg("main.rs")
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&file).unwrap(), GROUPED);
}

#[test]
fn the_environment_can_turn_paragraphs_off() {
    needs_nightly!();

    let (_temp, file) = loose_file(GROUPED);

    formatter()
        .env("RUST_FORMATTER_PARAGRAPHS", "false")
        .arg(&file)
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&file).unwrap(), GROUPED);
}

#[test]
fn stdin_gets_paragraphs() {
    needs_nightly!();

    let output = formatter()
        .arg("--stdin")
        .write_stdin(GROUPED)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(String::from_utf8(output).unwrap(), PARAGRAPHED);
}

#[test]
fn a_range_request_does_not_add_paragraphs() {
    needs_nightly!();

    let output = formatter()
        .args(["--stdin", "--range", "1:6"])
        .write_stdin(GROUPED)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(String::from_utf8(output).unwrap(), GROUPED);
}

#[test]
fn a_zero_blank_line_bound_turns_paragraphs_off() {
    needs_nightly!();

    let (_temp, file) = loose_file(GROUPED);

    formatter()
        .args(["--config", "blank_lines_upper_bound=0"])
        .arg(&file)
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&file).unwrap(), GROUPED);
}

#[test]
fn runs_of_blank_lines_collapse_where_rustfmt_leaves_them() {
    needs_nightly!();

    let (temp, file) = loose_file(
        "macro_rules! m {\n    () => {\n        a();\n\n\n\n        b();\n    };\n}\n\n\n\nfn main() {\n    let x = 1;\n\n\n    go(x);\n}\n",
    );

    fs::write(
        temp.path().join("rustfmt.toml"),
        "blank_lines_upper_bound = 3\n",
    )
    .unwrap();

    formatter().arg(&file).assert().success();

    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "macro_rules! m {\n    () => {\n        a();\n\n        b();\n    };\n}\n\nfn main() {\n    let x = 1;\n\n    go(x);\n}\n"
    );

    formatter().arg("--check").arg(&file).assert().success();
}

const UNSORTED_ATTRIBUTES: &str = "#[derive(Debug, PartialEq, Copy, Eq, Clone)]
#[must_use]
#[allow(unused, dead_code)]
pub struct S;
";

#[test]
fn attributes_are_sorted_and_check_then_passes() {
    needs_nightly!();

    let (_temp, file) = loose_file(UNSORTED_ATTRIBUTES);

    formatter()
        .arg("--check")
        .arg(&file)
        .assert()
        .failure()
        .code(1);
    formatter().arg(&file).assert().success();

    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
#[allow(dead_code, unused)]
pub struct S;
"
    );

    formatter().arg("--check").arg(&file).assert().success();
}

#[test]
fn no_sort_attributes_leaves_them_as_written() {
    needs_nightly!();

    let (_temp, file) = loose_file(UNSORTED_ATTRIBUTES);

    formatter()
        .args(["--no-sort-attributes", "--check"])
        .arg(&file)
        .assert()
        .success();

    formatter()
        .env("RUST_FORMATTER_SORT_ATTRIBUTES", "false")
        .arg(&file)
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&file).unwrap(), UNSORTED_ATTRIBUTES);
}
