#[path = "support/toolchain.rs"]
mod toolchain;

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;
use toolchain::{needs_nightly, needs_stable, nightly_available, stable_available};

fn assert_no_rustfmt_configs(dir: &Path) {
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy();
        assert_ne!(
            name,
            "rustfmt.toml",
            "Found rustfmt.toml in {}",
            entry.path().display()
        );
        assert_ne!(
            name,
            ".rustfmt.toml",
            "Found .rustfmt.toml in {}",
            entry.path().display()
        );
    }
}

#[test]
fn test_cargo_project_formatting_preserves_toolchain_and_no_config() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project_dir = temp.path();

    // Create Cargo.toml
    let cargo_toml = project_dir.join("Cargo.toml");
    fs::write(
        &cargo_toml,
        r#"[package]
name = "test-crate"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
    )
    .unwrap();

    // Explicitly pin to stable in rust-toolchain.toml
    let toolchain_file = project_dir.join("rust-toolchain.toml");
    fs::write(&toolchain_file, "[toolchain]\nchannel = \"stable\"\n").unwrap();

    // Create src/main.rs with unformatted/unsorted imports
    let src_dir = project_dir.join("src");
    fs::create_dir(&src_dir).unwrap();
    let main_rs = src_dir.join("main.rs");
    fs::write(
        &main_rs,
        r#"use std::path::Path;
use crate::b;
use std::collections::HashMap;
use crate::a;

mod a {}
mod b {}

fn main() {
println!("hello");
}
"#,
    )
    .unwrap();

    // Run rust-formatter on the project directory
    let mut cmd = formatter();
    cmd.arg(project_dir).assert().success();

    // Verify formatted content
    let formatted = fs::read_to_string(&main_rs).unwrap();
    let expected = r#"use std::{collections::HashMap, path::Path};

use crate::{a, b};

mod a {}
mod b {}

fn main() {
    println!("hello");
}
"#;
    assert_eq!(formatted, expected);

    // rust-toolchain.toml is formatted in place (already canonical here)
    let toolchain_content = fs::read_to_string(&toolchain_file).unwrap();
    assert_eq!(toolchain_content, "[toolchain]\nchannel = \"stable\"\n");

    // Verify no rustfmt.toml was created
    assert_no_rustfmt_configs(project_dir);
}

#[test]
fn test_three_tier_import_grouping() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("imports.rs");
    fs::write(
        &file,
        r"use crate::z;
use clap::Parser;
use std::io;
use crate::a;
use tempfile::tempdir;
use std::fs;
",
    )
    .unwrap();

    let mut cmd = formatter();
    cmd.arg(&file).assert().success();

    let formatted = fs::read_to_string(&file).unwrap();
    let expected = r"use std::{fs, io};

use clap::Parser;
use tempfile::tempdir;

use crate::{a, z};
";
    assert_eq!(formatted, expected);
}

#[test]
fn test_loose_rs_files_formatting() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let loose_dir = temp.path();

    let file1 = loose_dir.join("loose1.rs");
    fs::write(
        &file1,
        r"use std::path::Path;
use crate::b;
use std::fs;
use crate::a;
",
    )
    .unwrap();

    let sub_dir = loose_dir.join("sub");
    fs::create_dir(&sub_dir).unwrap();
    let file2 = sub_dir.join("loose2.rs");
    fs::write(
        &file2,
        r"use crate::z;
use std::io;
use crate::y;
",
    )
    .unwrap();

    // Run rust-formatter on the loose directory
    let mut cmd = formatter();
    cmd.arg(loose_dir).assert().success();

    let formatted1 = fs::read_to_string(&file1).unwrap();
    let expected1 = r"use std::{fs, path::Path};

use crate::{a, b};
";
    assert_eq!(formatted1, expected1);

    let formatted2 = fs::read_to_string(&file2).unwrap();
    let expected2 = r"use std::io;

use crate::{y, z};
";
    assert_eq!(formatted2, expected2);

    assert_no_rustfmt_configs(loose_dir);
}

#[test]
fn test_check_mode_failure_and_success() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project_dir = temp.path();

    let cargo_toml = project_dir.join("Cargo.toml");
    fs::write(
        &cargo_toml,
        r#"[package]
name = "check-test"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    let src_dir = project_dir.join("src");
    fs::create_dir(&src_dir).unwrap();
    let main_rs = src_dir.join("main.rs");
    fs::write(
        &main_rs,
        r"use std::path::Path;
use crate::foo;
mod foo {}
fn main() {}
",
    )
    .unwrap();

    // Check mode should fail because it is unformatted (missing blank line between std and crate)
    let mut check_cmd = formatter();
    check_cmd
        .arg("--check")
        .arg(project_dir)
        .assert()
        .failure()
        .code(1);

    // Run formatter to fix it
    let mut fmt_cmd = formatter();
    fmt_cmd.arg(project_dir).assert().success();

    // Now check mode should succeed
    let mut check_again = formatter();
    check_again
        .arg("--check")
        .arg(project_dir)
        .assert()
        .success();
}

#[test]
fn test_single_file_target() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("single.rs");
    fs::write(
        &file,
        r"use std::sync::Arc;
use crate::bar;
use std::sync::Mutex;
use crate::foo;
",
    )
    .unwrap();

    let mut cmd = formatter();
    cmd.arg(&file).assert().success();

    let formatted = fs::read_to_string(&file).unwrap();
    let expected = r"use std::sync::{Arc, Mutex};

use crate::{bar, foo};
";
    assert_eq!(formatted, expected);
}

#[test]
fn test_single_file_uses_package_edition() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project_dir = temp.path();
    fs::write(
        project_dir.join("Cargo.toml"),
        r#"[package]
name = "edition-infer"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    let src = project_dir.join("src");
    fs::create_dir(&src).unwrap();
    let lib_rs = src.join("lib.rs");
    fs::write(&lib_rs, "async fn f() {}\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(&lib_rs).assert().success();

    assert_eq!(fs::read_to_string(&lib_rs).unwrap(), "async fn f() {}\n");
}

#[test]
fn test_cli_edition_overrides_manifest() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project_dir = temp.path();
    fs::write(
        project_dir.join("Cargo.toml"),
        r#"[package]
name = "edition-override"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    let src = project_dir.join("src");
    fs::create_dir(&src).unwrap();
    let lib_rs = src.join("lib.rs");
    fs::write(&lib_rs, "async fn f() {}\n").unwrap();

    let mut cmd = formatter();
    // rustfmt's own diagnostic is the report, exactly as an unparseable TOML
    // file is reported: named by path, line and column, one error per file.
    cmd.arg("--edition")
        .arg("2015")
        .arg(&lib_rs)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(format!(
            "{}:1:1: error[E0670]",
            lib_rs.display()
        )))
        .stderr(predicates::str::contains("async fn"));
}

#[test]
fn test_subdirectory_in_cargo_project() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project_dir = temp.path();

    fs::write(
        project_dir.join("Cargo.toml"),
        r#"[package]
name = "sub-test"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    let src_dir = project_dir.join("src");
    fs::create_dir(&src_dir).unwrap();
    let main_rs = src_dir.join("main.rs");
    fs::write(
        &main_rs,
        r"use std::io;
use crate::b;
use crate::a;

mod a {}
mod b {}
",
    )
    .unwrap();

    // Pass the `src` subdirectory as the target
    let mut cmd = formatter();
    cmd.arg(&src_dir).assert().success();

    let formatted = fs::read_to_string(&main_rs).unwrap();
    let expected = r"use std::io;

use crate::{a, b};

mod a {}
mod b {}
";
    assert_eq!(formatted, expected);
}

#[test]
fn test_custom_config_override() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("wrap.rs");
    fs::write(
        &file,
        r"fn test_long_function_name(first_parameter_is_long: String, second_parameter_is_long: usize) {
}
",
    )
    .unwrap();

    // Pass max_width=40 to force wrap
    let mut cmd = formatter();
    cmd.arg(&file)
        .arg("--config")
        .arg("max_width=40")
        .assert()
        .success();

    let formatted = fs::read_to_string(&file).unwrap();
    assert!(formatted.contains("fn test_long_function_name(\n"));
}

#[test]
fn test_color_always_paints_success() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("color.rs");
    fs::write(&file, "fn main() {}\n").unwrap();

    let mut cmd = formatter();
    cmd.arg("--color")
        .arg("always")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicates::str::contains("Already formatted:"))
        .stderr(predicates::str::contains("\u{1b}"));
}

#[test]
fn test_color_never_is_plain() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("plain.rs");
    fs::write(&file, "fn main() {}\n").unwrap();

    let mut cmd = formatter();
    cmd.arg("--color")
        .arg("never")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicates::str::starts_with("Already formatted: "))
        .stderr(predicates::str::contains("\u{1b}").not());
}

#[test]
fn test_success_prints_summary_for_single_file() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("summary.rs");
    fs::write(
        &file,
        r"use std::sync::Arc;
use crate::bar;
use std::sync::Mutex;
use crate::foo;
",
    )
    .unwrap();

    let mut cmd = formatter();
    cmd.arg(&file)
        .assert()
        .success()
        .stderr(predicates::str::starts_with("Formatted "));
}

#[test]
fn test_quiet_suppresses_summary() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("quiet.rs");
    fs::write(&file, "fn main() {}\n").unwrap();

    let mut cmd = formatter();
    cmd.arg("-q")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicates::str::is_empty());
}

#[test]
fn test_check_success_summary() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("already.rs");
    fs::write(
        &file,
        r"use std::sync::{Arc, Mutex};

use crate::{bar, foo};
",
    )
    .unwrap();

    let mut cmd = formatter();
    cmd.arg("--check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicates::str::starts_with("Already formatted:"));
}

#[test]
fn test_check_failure_summary() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("needs.rs");
    fs::write(
        &file,
        r"use std::path::Path;
use crate::foo;
",
    )
    .unwrap();

    let mut cmd = formatter();
    cmd.arg("--check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("Check failed:"));
}

#[test]
fn test_verbose_reports_target_before_rustfmt() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("verbose.rs");
    fs::write(&file, "fn main() {}\n").unwrap();

    let mut cmd = formatter();
    cmd.arg("--verbose")
        .arg("--check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicates::str::contains("target: file"))
        .stderr(predicates::str::contains("Spent ").not());
}

#[test]
fn test_toml_formatting_dotted_and_multiline() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("deps.toml");
    fs::write(
        &file,
        r##"clap = { version = "4.6.6", features = ["derive"] }
local = { path = "../local" }
key="v"#note
"##,
    )
    .unwrap();

    let mut cmd = formatter();
    cmd.arg(&file).assert().success();

    let formatted = fs::read_to_string(&file).unwrap();
    assert_eq!(
        formatted,
        r#"clap = { version = "4.6.6", features = ["derive"] }
local.path = "../local"
key = "v" # note
"#
    );
}

#[test]
fn test_toml_check_mode() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("check.toml");
    fs::write(&file, "foo={path=\"x\"}\n").unwrap();

    let mut check_cmd = formatter();
    check_cmd
        .arg("--check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("needs formatting"));

    let mut fmt_cmd = formatter();
    fmt_cmd.arg(&file).assert().success();

    let mut check_again = formatter();
    check_again.arg("--check").arg(&file).assert().success();
}

#[test]
fn test_quiet_toml_check_has_empty_stderr() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("quiet-check.toml");
    fs::write(&file, "foo={path=\"x\"}\n").unwrap();

    let mut cmd = formatter();
    cmd.arg("-q")
        .arg("--check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicates::str::is_empty());
}

#[test]
fn test_cargo_lock_not_formatted() {
    let temp = tempdir().unwrap();
    let lock = temp.path().join("Cargo.lock");
    let original = "version = 3\n\n\n[[package]]\nname = \"foo\"\n";
    fs::write(&lock, original).unwrap();
    fs::write(temp.path().join("extra.toml"), "a=1\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(temp.path()).assert().success();

    assert_eq!(fs::read_to_string(&lock).unwrap(), original);
    assert_eq!(
        fs::read_to_string(temp.path().join("extra.toml")).unwrap(),
        "a = 1\n"
    );
}

#[test]
fn test_clippy_toml_not_formatted() {
    let temp = tempdir().unwrap();
    let clippy = temp.path().join("clippy.toml");
    let original =
        "disallowed-types = [{ path = \"std::sync::Mutex\", reason = \"use parking_lot\" }]\n";
    fs::write(&clippy, original).unwrap();
    fs::write(temp.path().join("extra.toml"), "a=1\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(temp.path()).assert().success();

    assert_eq!(fs::read_to_string(&clippy).unwrap(), original);
    assert_eq!(
        fs::read_to_string(temp.path().join("extra.toml")).unwrap(),
        "a = 1\n"
    );

    let mut single = formatter();
    single.arg(&clippy).assert().success();
    assert_eq!(fs::read_to_string(&clippy).unwrap(), original);
}

#[test]
fn test_rustfmt_toml_not_formatted() {
    let temp = tempdir().unwrap();
    let rustfmt = temp.path().join("rustfmt.toml");
    let original = "max_width=80\nfoo = { path = \"x\" }\n";
    fs::write(&rustfmt, original).unwrap();
    fs::write(temp.path().join("extra.toml"), "a=1\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(temp.path()).assert().success();

    assert_eq!(fs::read_to_string(&rustfmt).unwrap(), original);
    assert_eq!(
        fs::read_to_string(temp.path().join("extra.toml")).unwrap(),
        "a = 1\n"
    );

    let mut single = formatter();
    single.arg(&rustfmt).assert().success();
    assert_eq!(fs::read_to_string(&rustfmt).unwrap(), original);
}

#[test]
fn test_toml_only_directory() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("only.toml");
    fs::write(&file, "foo = { path = \"x\" }\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(temp.path()).assert().success();

    assert_eq!(fs::read_to_string(&file).unwrap(), "foo.path = \"x\"\n");
}

#[test]
fn test_cargo_toml_in_project_is_formatted() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project_dir = temp.path();
    fs::write(
        project_dir.join("Cargo.toml"),
        r#"[package]
name = "toml-proj"
version = "0.1.0"
edition = "2021"

[dependencies]
clap = { version = "4.6.6", features = ["derive"] }
local = { path = "../local" }
"#,
    )
    .unwrap();
    let src = project_dir.join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("lib.rs"), "pub fn f() {}\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(project_dir).assert().success();

    let cargo = fs::read_to_string(project_dir.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("clap = {"));
    assert!(cargo.contains("version = \"4.6.6\""));
    assert!(cargo.contains("features = [\"derive\"]"));
    assert!(cargo.contains("local.path = \"../local\""));
}

#[test]
fn test_full_versions_is_opt_in() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project_dir = temp.path();
    fs::write(
        project_dir.join("Cargo.toml"),
        r#"[package]
name = "pin-test"
version = "0.1.0"
edition = "2021"

[dependencies]
ignore = "0.4"
"#,
    )
    .unwrap();
    let src = project_dir.join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("lib.rs"), "").unwrap();

    let mut cmd = formatter();
    cmd.arg(project_dir).assert().success();

    let formatted = fs::read_to_string(project_dir.join("Cargo.toml")).unwrap();
    assert!(formatted.contains("ignore = \"0.4\""));
    assert!(!formatted.contains("ignore = \"0.4.33\""));
}

#[test]
fn test_check_rust_clean_toml_dirty() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project_dir = temp.path();

    let cargo_toml = project_dir.join("Cargo.toml");
    let dirty_cargo = r#"[package]
name = "check-toml"
version = "0.1.0"
edition = "2021"

[dependencies]
local = { path = "../local" }
"#;
    fs::write(&cargo_toml, dirty_cargo).unwrap();

    let src_dir = project_dir.join("src");
    fs::create_dir(&src_dir).unwrap();
    fs::write(src_dir.join("lib.rs"), "pub fn f() {}\n").unwrap();

    let mut check_cmd = formatter();
    check_cmd
        .arg("--check")
        .arg(project_dir)
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicates::str::contains("needs formatting")
                .or(predicates::str::contains("Check failed:")),
        );

    assert_eq!(fs::read_to_string(&cargo_toml).unwrap(), dirty_cargo);
}

#[test]
fn test_check_rust_dirty_toml_clean() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project_dir = temp.path();

    fs::write(
        project_dir.join("Cargo.toml"),
        r#"[package]
name = "check-rust"
version = "0.1.0"
edition = "2021"

[dependencies]
local.path = "../local"
"#,
    )
    .unwrap();

    let src_dir = project_dir.join("src");
    fs::create_dir(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        r"use std::path::Path;
use crate::foo;
mod foo {}
",
    )
    .unwrap();

    let mut check_cmd = formatter();
    check_cmd
        .arg("--check")
        .arg(project_dir)
        .assert()
        .failure()
        .code(1);
}

#[test]
fn test_parallel_toml_formats_five_files() {
    let temp = tempdir().unwrap();
    let dir = temp.path();

    for name in ["a.toml", "b.toml", "c.toml", "d.toml", "e.toml"] {
        fs::write(dir.join(name), "foo={path=\"x\"}\n").unwrap();
    }

    let mut cmd = formatter();
    cmd.arg(dir).assert().success();

    for name in ["a.toml", "b.toml", "c.toml", "d.toml", "e.toml"] {
        assert_eq!(
            fs::read_to_string(dir.join(name)).unwrap(),
            "foo.path = \"x\"\n"
        );
    }
}

#[test]
fn test_parallel_toml_check() {
    let temp = tempdir().unwrap();
    let dir = temp.path();

    for name in ["a.toml", "b.toml", "c.toml", "d.toml", "e.toml"] {
        fs::write(dir.join(name), "foo={path=\"x\"}\n").unwrap();
    }

    let mut check_cmd = formatter();
    check_cmd.arg("--check").arg(dir).assert().failure().code(1);

    let mut fmt_cmd = formatter();
    fmt_cmd.arg(dir).assert().success();

    let mut check_again = formatter();
    check_again.arg("--check").arg(dir).assert().success();
}

#[test]
fn test_missing_rustup_reports_error() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("main.rs");
    fs::write(&file, "fn main() {}\n").unwrap();

    let mut cmd = formatter();
    // An empty PATH is not enough on Windows, where CreateProcessW also searches
    // the caller's directory and the cwd; point both somewhere empty instead.
    cmd.current_dir(temp.path())
        .env("PATH", temp.path())
        .env_remove("RUSTFMT")
        .arg(&file)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("No rustfmt could be found"))
        .stderr(predicates::str::contains("$RUSTFMT"));
}

/// Naming a toolchain says rustup, so a machine without one is told that
/// rather than being told no rustfmt exists.
#[test]
fn test_a_named_toolchain_without_rustup_names_rustup() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("main.rs");
    fs::write(&file, "fn main() {}\n").unwrap();

    let mut cmd = formatter();
    cmd.current_dir(temp.path())
        .env("PATH", temp.path())
        .env_remove("RUSTFMT")
        .args(["--toolchain", "nightly"])
        .arg(&file)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("rustup was not found in PATH"));
}

/// `$RUSTFMT` is cargo's own spelling for "use this one", and it is the only
/// avenue a Nix or distro install has. It must be taken without rustup.
#[test]
fn test_the_rustfmt_variable_is_used_when_rustup_is_absent() {
    needs_nightly!();
    let Ok(resolved) = Command::new("rustup")
        .args(["which", "rustfmt", "--toolchain", "nightly"])
        .output()
    else {
        return;
    };
    let rustfmt = String::from_utf8(resolved.stdout).unwrap();

    let temp = tempdir().unwrap();
    let file = temp.path().join("main.rs");
    fs::write(&file, "fn  main(){}\n").unwrap();

    let mut cmd = formatter();
    cmd.current_dir(temp.path())
        .env("PATH", temp.path())
        .env("RUSTFMT", rustfmt.trim())
        .arg(&file)
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&file).unwrap(), "fn main() {}\n");
}

#[test]
fn test_missing_toolchain_reports_error() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("main.rs");
    fs::write(&file, "fn main() {}\n").unwrap();

    let mut cmd = formatter();
    cmd.arg("--toolchain")
        .arg("rust-formatter-does-not-exist-toolchain")
        .arg(&file)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains(
            "rustfmt component is not available for toolchain",
        ))
        .stderr(predicates::str::contains("rustup toolchain install"));
}

#[test]
fn test_unsupported_file_reports_unsupported() {
    let temp = tempdir().unwrap();
    let readme = temp.path().join("README.md");
    fs::write(&readme, "# hi\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(&readme)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("Unsupported target"))
        .stderr(predicates::str::contains("does not exist").not());
}

#[test]
fn test_cargo_lock_as_target_is_unsupported() {
    let temp = tempdir().unwrap();
    let lock = temp.path().join("Cargo.lock");
    let original = "version = 3\n\n\n[[package]]\nname = \"foo\"\n";
    fs::write(&lock, original).unwrap();

    let mut cmd = formatter();
    cmd.arg(&lock)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("Unsupported target"));

    assert_eq!(fs::read_to_string(&lock).unwrap(), original);
}

#[test]
fn test_missing_path_is_path_not_found() {
    let temp = tempdir().unwrap();
    let missing = temp.path().join("does-not-exist.rs");

    let mut cmd = formatter();
    cmd.arg(&missing)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("Target path does not exist"));
}

#[test]
fn test_empty_dir_reports_no_formattable_files() {
    let temp = tempdir().unwrap();

    let mut cmd = formatter();
    cmd.arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("No formattable"));
}

#[cfg(unix)]
#[test]
fn test_check_fails_on_unreadable_subdirectory() {
    use std::os::unix::fs::PermissionsExt;

    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    let temp = tempdir().unwrap();
    fs::write(temp.path().join("keep.rs"), "fn main() {}\n").unwrap();

    let blocked = temp.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("dirty.rs"), "fn dirty(){}\n").unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    let _guard = Restore(blocked);

    let mut cmd = formatter();
    cmd.arg("--check")
        .arg(temp.path())
        .assert()
        .failure()
        .code(2);
}

fn make_workspace_fixture(root: &Path) {
    fs::write(
        root.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/foo", "crates/bar"]

[workspace.dependencies]
local = { path = "crates/local" }
"#,
    )
    .unwrap();

    let local_dir = root.join("crates").join("local");
    fs::create_dir_all(local_dir.join("src")).unwrap();
    fs::write(
        local_dir.join("Cargo.toml"),
        r#"[package]
name = "local"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    fs::write(local_dir.join("src").join("lib.rs"), "").unwrap();

    for member in ["foo", "bar"] {
        let crate_dir = root.join("crates").join(member);
        fs::create_dir_all(crate_dir.join("src")).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            format!(
                r#"[package]
name = "{member}"
version = "0.1.0"
edition = "2021"

[dependencies]
local = {{ path = "../local" }}
"#
            ),
        )
        .unwrap();
        fs::write(crate_dir.join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();
    }
}

#[test]
fn test_workspace_member_formats_sibling_toml() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let root = temp.path();
    make_workspace_fixture(root);

    let foo_dir = root.join("crates").join("foo");
    let mut cmd = formatter();
    cmd.arg(&foo_dir).assert().success();

    let foo_cargo = fs::read_to_string(foo_dir.join("Cargo.toml")).unwrap();
    assert!(foo_cargo.contains("local.path = \"../local\""));

    let bar_cargo = fs::read_to_string(root.join("crates").join("bar").join("Cargo.toml")).unwrap();
    assert!(bar_cargo.contains("local.path = \"../local\""));

    let root_cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(root_cargo.contains("local.path = \"crates/local\""));
}

#[test]
fn test_no_all_does_not_format_sibling_toml() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let root = temp.path();
    make_workspace_fixture(root);

    let foo_dir = root.join("crates").join("foo");
    let mut cmd = formatter();
    cmd.arg("--no-all").arg(&foo_dir).assert().success();

    let foo_cargo = fs::read_to_string(foo_dir.join("Cargo.toml")).unwrap();
    assert!(foo_cargo.contains("local.path = \"../local\""));

    let bar_cargo = fs::read_to_string(root.join("crates").join("bar").join("Cargo.toml")).unwrap();
    assert!(bar_cargo.contains("local = { path = \"../local\" }"));

    let root_cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(root_cargo.contains("local = { path = \"crates/local\" }"));
}

#[test]
fn test_non_member_scratch_does_not_format_workspace_toml() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let root = temp.path();
    make_workspace_fixture(root);

    let scratch_dir = root.join("scratch");
    fs::create_dir_all(scratch_dir.join("src")).unwrap();
    fs::write(
        scratch_dir.join("Cargo.toml"),
        r#"[package]
name = "scratch"
version = "0.1.0"
edition = "2021"

[dependencies]
local = { path = "../crates/local" }
"#,
    )
    .unwrap();
    fs::write(scratch_dir.join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();

    let mut cmd = formatter();
    // Nested non-members make cargo fmt fail; TOML formatting still runs.
    let _ = cmd.arg(&scratch_dir).output().unwrap();

    let scratch_cargo = fs::read_to_string(scratch_dir.join("Cargo.toml")).unwrap();
    assert!(scratch_cargo.contains("local.path = \"../crates/local\""));

    let root_cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(root_cargo.contains("local = { path = \"crates/local\" }"));
}

#[test]
fn test_exit_code_unparseable_toml_is_tool_error() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("bad.toml");
    fs::write(&file, "key =\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(&file)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("Failed to parse TOML file"))
        .stderr(predicates::str::contains("bad.toml"));
}

/// `--fail-fast` stops *scheduling*, so at `-j 1` -- where the queue is drained
/// in order -- the second file is never reached.
#[test]
fn test_fail_fast_stops_after_first_bad_toml() {
    let temp = tempdir().unwrap();
    let a = temp.path().join("a.toml");
    let b = temp.path().join("b.toml");
    fs::write(&a, "key =\n").unwrap();
    fs::write(&b, "other =\n").unwrap();

    let mut all = formatter();
    all.arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("a.toml"))
        .stderr(predicates::str::contains("b.toml"));

    let fail_fast = Command::cargo_bin("rust-formatter")
        .unwrap()
        .args(["--fail-fast", "-j", "1"])
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(!fail_fast.status.success());
    assert_eq!(fail_fast.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&fail_fast.stderr);
    let mentioned_a = stderr.contains("a.toml");
    let mentioned_b = stderr.contains("b.toml");
    assert!(
        mentioned_a ^ mentioned_b,
        "fail-fast should report exactly one file, got {stderr}"
    );
}

/// It used to buy the early exit by running the whole TOML half serially. It
/// is an abort flag now, so a parallel run still stops early -- and a run that
/// stops early is still parallel.
#[test]
fn test_fail_fast_does_not_cost_parallelism() {
    let temp = tempdir().unwrap();
    for index in 0..400 {
        fs::write(temp.path().join(format!("{index:03}.toml")), "key =\n").unwrap();
    }

    let count = |args: &[&str]| {
        let out = Command::cargo_bin("rust-formatter")
            .unwrap()
            .args(args)
            .arg(temp.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        String::from_utf8_lossy(&out.stderr)
            .matches("Failed to parse TOML file")
            .count()
    };

    assert_eq!(count(&["-j", "8"]), 400);
    let stopped = count(&["--fail-fast", "-j", "8"]);
    assert!(stopped < 400, "fail-fast reported {stopped} of 400");
}

/// `--fail-fast` read only in the TOML half was half a flag: a workspace whose
/// first file failed to parse still spawned rustfmt over every other one.
#[test]
fn test_fail_fast_reaches_the_rust_half() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    for index in 0..400 {
        fs::write(temp.path().join(format!("{index:03}.rs")), "fn broken( {\n").unwrap();
    }

    let count = |args: &[&str]| {
        let out = Command::cargo_bin("rust-formatter")
            .unwrap()
            .args(args)
            .arg(temp.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        String::from_utf8_lossy(&out.stderr)
            .matches("unclosed delimiter")
            .count()
    };

    let all = count(&["-j", "8"]);
    let stopped = count(&["--fail-fast", "-j", "8"]);
    assert!(stopped < all, "fail-fast reported {stopped} of {all}");
}

#[test]
fn test_missing_workspace_member_does_not_dump_rustfmt_help() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"missing\"]\n",
    )
    .unwrap();

    let mut cmd = formatter();
    cmd.arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stdout(predicates::str::contains("Usage:").not())
        .stdout(predicates::str::contains("rustfmt").not())
        .stderr(predicates::str::contains("cargo metadata"));
}

#[test]
fn test_package_without_targets_does_not_dump_help() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"empty\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let mut cmd = formatter();
    cmd.arg(temp.path())
        .assert()
        .success()
        .code(0)
        .stdout(predicates::str::is_empty());
}

#[test]
fn test_vendor_tree_is_not_rewritten() {
    let temp = tempdir().unwrap();
    let keep = temp.path().join("keep.toml");
    fs::write(&keep, "foo={path=\"x\"}\n").unwrap();

    let vendor = temp.path().join("vendor").join("foo");
    fs::create_dir_all(&vendor).unwrap();
    fs::write(vendor.join(".cargo-checksum.json"), "{}").unwrap();
    let vendored = vendor.join("Cargo.toml");
    fs::write(&vendored, "x=1\n").unwrap();

    let renamed = temp.path().join("third-party").join("bar");
    fs::create_dir_all(&renamed).unwrap();
    fs::write(renamed.join(".cargo-checksum.json"), "{}").unwrap();
    let renamed_manifest = renamed.join("Cargo.toml");
    fs::write(&renamed_manifest, "y=1\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(temp.path()).assert().success();

    assert_eq!(fs::read_to_string(&keep).unwrap(), "foo.path = \"x\"\n");
    assert_eq!(fs::read_to_string(&vendored).unwrap(), "x=1\n");
    assert_eq!(fs::read_to_string(&renamed_manifest).unwrap(), "y=1\n");
}

#[test]
fn test_hidden_config_toml_is_formatted() {
    let temp = tempdir().unwrap();
    let cargo_dir = temp.path().join(".cargo");
    let config_dir = temp.path().join(".config");
    fs::create_dir(&cargo_dir).unwrap();
    fs::create_dir(&config_dir).unwrap();
    let cargo_toml = cargo_dir.join("config.toml");
    let nextest = config_dir.join("nextest.toml");
    let secret = temp.path().join(".secret.toml");
    fs::write(&cargo_toml, "foo={path=\"x\"}\n").unwrap();
    fs::write(&nextest, "foo={path=\"y\"}\n").unwrap();
    fs::write(&secret, "foo={path=\"z\"}\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(temp.path()).assert().success();

    assert_eq!(
        fs::read_to_string(&cargo_toml).unwrap(),
        "foo.path = \"x\"\n"
    );
    assert_eq!(fs::read_to_string(&nextest).unwrap(), "foo.path = \"y\"\n");
    assert_eq!(fs::read_to_string(&secret).unwrap(), "foo={path=\"z\"}\n");
}

#[test]
fn test_gitignore_without_git_is_respected() {
    let temp = tempdir().unwrap();
    fs::write(temp.path().join(".gitignore"), "skip.toml\n").unwrap();
    let keep = temp.path().join("keep.toml");
    let skip = temp.path().join("skip.toml");
    fs::write(&keep, "foo={path=\"x\"}\n").unwrap();
    fs::write(&skip, "foo={path=\"y\"}\n").unwrap();

    let mut cmd = formatter();
    cmd.arg(temp.path()).assert().success();

    assert_eq!(fs::read_to_string(&keep).unwrap(), "foo.path = \"x\"\n");
    assert_eq!(fs::read_to_string(&skip).unwrap(), "foo={path=\"y\"}\n");
}

#[test]
fn test_excluded_path_dep_toml_not_rewritten_from_workspace() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/foo"]
exclude = ["crates/local"]
"#,
    )
    .unwrap();

    let foo = root.join("crates").join("foo");
    fs::create_dir_all(foo.join("src")).unwrap();
    fs::write(
        foo.join("Cargo.toml"),
        "[package]\nname = \"foo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nlocal = { path = \"../local\" }\n",
    )
    .unwrap();
    fs::write(foo.join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();

    let local = root.join("crates").join("local");
    fs::create_dir_all(local.join("src")).unwrap();
    fs::write(
        local.join("Cargo.toml"),
        "[package]\nname = \"local\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nfoo = { path = \"../foo\" }\n",
    )
    .unwrap();
    fs::write(local.join("src").join("lib.rs"), "pub fn g() {}\n").unwrap();

    let mut from_root = formatter();
    from_root.arg(root).assert().success();

    assert!(
        fs::read_to_string(foo.join("Cargo.toml"))
            .unwrap()
            .contains("local.path = \"../local\"")
    );
    assert!(
        fs::read_to_string(local.join("Cargo.toml"))
            .unwrap()
            .contains("foo = { path = \"../foo\" }"),
        "excluded member must not be rewritten from the workspace root"
    );

    let mut from_local = formatter();
    from_local.arg(&local).assert().success();
    assert!(
        fs::read_to_string(local.join("Cargo.toml"))
            .unwrap()
            .contains("foo.path = \"../foo\"")
    );
}

#[test]
fn test_preserves_toml_bom_and_crlf() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("win.toml");
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    bytes.extend_from_slice(b"foo={path=\"x\"}\r\n");
    fs::write(&file, &bytes).unwrap();

    let mut fmt = formatter();
    fmt.arg(&file).assert().success();

    let written = fs::read(&file).unwrap();
    assert!(written.starts_with(b"\xEF\xBB\xBF"));
    let body = &written[3..];
    assert_eq!(body, b"foo.path = \"x\"\r\n");

    let mut check = formatter();
    check.arg("--check").arg(&file).assert().success().code(0);
}

#[test]
fn test_io_error_includes_path() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("bad-utf8.toml");
    fs::write(&file, [0xff, 0xfe, b'a', b'=', b'1', b'\n']).unwrap();

    let mut cmd = formatter();
    cmd.arg(&file)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("I/O error at"))
        .stderr(predicates::str::contains("bad-utf8.toml"))
        .stderr(predicates::str::contains(
            "stream did not contain valid UTF-8",
        ));
}

/// Both options this tool sets are delivered through `--config`, which stable
/// rustfmt honours even though a `rustfmt.toml` carrying them would be
/// ignored. So a stable toolchain formats, and formats the same way -- what it
/// cannot do is `--range`, which needs `--file-lines`.
#[test]
fn test_a_stable_toolchain_formats_and_says_so() {
    needs_stable!();

    let temp = tempdir().unwrap();
    let file = temp.path().join("main.rs");
    fs::write(
        &file,
        "use std::io::Write;\nuse std::io::Read;\nfn main(){}\n",
    )
    .unwrap();

    Command::cargo_bin("rust-formatter")
        .unwrap()
        .args(["--toolchain", "stable"])
        .arg(&file)
        .assert()
        .success()
        .stderr(predicates::str::contains("is not a nightly rustfmt"));

    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "use std::io::{Read, Write};\nfn main() {}\n"
    );
}

/// The two channels are two transports -- `--emit json` and `--emit stdout` --
/// and a difference between them would show up as a diff one of them does not
/// report.
#[test]
fn test_check_agrees_between_stable_and_nightly() {
    needs_stable!();
    needs_nightly!();

    let temp = tempdir().unwrap();
    let file = temp.path().join("main.rs");
    fs::write(
        &file,
        "use std::io::Write;\nuse std::io::Read;\nfn main(){ let _ = 1; }\n",
    )
    .unwrap();

    let of = |toolchain: &str| {
        let assert = Command::cargo_bin("rust-formatter")
            .unwrap()
            .args(["--check", "--toolchain", toolchain])
            .arg(&file)
            .assert()
            .failure()
            .code(1);
        String::from_utf8(assert.get_output().stdout.clone()).unwrap()
    };
    assert_eq!(of("nightly"), of("stable"));
}

/// `--file-lines` is nightly-only and has no configuration key, so the one
/// thing a stable toolchain cannot do has to be refused by name rather than
/// reaching the user as rustfmt's `Unrecognized option`.
#[test]
fn test_range_names_the_toolchain_it_needs() {
    needs_stable!();

    let temp = tempdir().unwrap();
    let file = temp.path().join("main.rs");
    fs::write(&file, "fn  main(){}\n").unwrap();

    Command::cargo_bin("rust-formatter")
        .unwrap()
        .args(["--toolchain", "stable", "--range", "1:1"])
        .arg(&file)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("needs a nightly rustfmt"));
}

#[cfg(unix)]
#[test]
fn test_unreadable_toml_is_reported_without_stopping_the_run() {
    use std::os::unix::fs::PermissionsExt;

    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o644));
        }
    }

    let temp = tempdir().unwrap();
    let locked = temp.path().join("locked.toml");
    let dirty = temp.path().join("dirty.toml");
    fs::write(&locked, "a=1\n").unwrap();
    fs::write(&dirty, "b=2\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let _guard = Restore(locked);

    Command::cargo_bin("rust-formatter")
        .unwrap()
        .arg("--check")
        .arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("locked.toml"))
        .stdout(predicates::str::contains("dirty.toml"));

    Command::cargo_bin("rust-formatter")
        .unwrap()
        .arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("locked.toml"));

    assert_eq!(fs::read_to_string(&dirty).unwrap(), "b = 2\n");
}

#[test]
fn test_non_utf8_toml_is_reported_without_stopping_the_run() {
    let temp = tempdir().unwrap();
    let broken = temp.path().join("broken.toml");
    let dirty = temp.path().join("dirty.toml");
    fs::write(&broken, [0xff, 0xfe, b'\n']).unwrap();
    fs::write(&dirty, "b=2\n").unwrap();

    Command::cargo_bin("rust-formatter")
        .unwrap()
        .arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("broken.toml"));

    assert_eq!(fs::read_to_string(&dirty).unwrap(), "b = 2\n");
}

fn cargo_project(dir: &Path, name: &str) {
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
}

/// rustfmt needs two passes to settle when `group_imports` and
/// `imports_granularity` meet a comment between imports (rustfmt#6195) — the one
/// combination this tool exists for. A write run that stopped at one pass left a
/// tree its own `--check` rejected, which breaks format-locally-then-check-in-CI.
#[test]
fn a_write_run_leaves_a_tree_its_own_check_accepts() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project = temp.path();
    cargo_project(project, "converge");
    let lib = project.join("src").join("lib.rs");
    fs::write(
        &lib,
        "use a::c;\n// foo\nuse a::b;\nuse a::d;\n\npub fn f() {}\n",
    )
    .unwrap();

    Command::cargo_bin("rust-formatter")
        .unwrap()
        .arg(project)
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(&lib).unwrap(),
        "// foo\nuse a::{b, c, d};\n\npub fn f() {}\n"
    );

    Command::cargo_bin("rust-formatter")
        .unwrap()
        .arg("--check")
        .arg(project)
        .assert()
        .success()
        .code(0);
}

/// Exit 1 means "needs formatting". A file rustfmt cannot parse is a tool error,
/// and reporting it as a diff makes CI tell the author to reformat a syntax error.
#[test]
fn a_rust_parse_error_is_a_tool_error_in_every_mode() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project = temp.path();
    cargo_project(project, "unparseable");
    fs::write(project.join("src").join("lib.rs"), "fn broken( {\n").unwrap();

    for args in [vec!["--check"], vec![], vec!["--check", "--list-different"]] {
        Command::cargo_bin("rust-formatter")
            .unwrap()
            .args(&args)
            .arg(project)
            .assert()
            .failure()
            .code(2)
            .stderr(predicates::str::contains("unclosed delimiter"))
            // The self-contradictory "0 files need formatting" verdict must be
            // gone with it.
            .stderr(predicates::str::contains("Check failed").not());
    }
}

/// A workspace's scope comes from `cargo metadata`, which needs cargo rather than
/// a nightly rustfmt. Gating it on the Rust half made `--toml-only` walk the whole
/// workspace root unscoped, rewriting packages a full run deliberately skips —
/// and made `--list-files` disagree with both.
#[test]
fn toml_only_honours_the_same_workspace_scope_as_a_full_run() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\nexclude = [\"excluded\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["member", "excluded"] {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname=\"{name}\"\nversion=\"0.1.0\"\nedition=\"2021\"\n"),
        )
        .unwrap();
        fs::write(dir.join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();
    }

    let listed = Command::cargo_bin("rust-formatter")
        .unwrap()
        .args(["--list-files", "--toml-only"])
        .arg(root)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed = String::from_utf8(listed).unwrap();
    assert!(listed.contains("member"), "{listed}");
    assert!(!listed.contains("excluded"), "{listed}");

    Command::cargo_bin("rust-formatter")
        .unwrap()
        .arg("--toml-only")
        .arg(root)
        .assert()
        .success();

    assert!(
        fs::read_to_string(root.join("member/Cargo.toml"))
            .unwrap()
            .contains("name = \"member\"")
    );
    assert_eq!(
        fs::read_to_string(root.join("excluded/Cargo.toml")).unwrap(),
        "[package]\nname=\"excluded\"\nversion=\"0.1.0\"\nedition=\"2021\"\n"
    );
}

/// An explicit file argument that is a symlink is formatted through the link.
/// A walk does not follow file symlinks, so a target outside the tree is left
/// alone.
#[test]
#[cfg(unix)]
fn a_symlinked_toml_is_formatted_through_the_link() {
    let temp = tempdir().unwrap();
    let outside = temp.path().join("outside");
    let tree = temp.path().join("tree");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&tree).unwrap();

    let real = outside.join("real.toml");
    let link = tree.join("link.toml");
    fs::write(&real, "foo={path=\"x\"}\n").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    Command::cargo_bin("rust-formatter")
        .unwrap()
        .arg(&link)
        .assert()
        .success();

    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&real).unwrap(), "foo.path = \"x\"\n");

    fs::write(&real, "foo={path=\"x\"}\n").unwrap();
    let local = tree.join("local.toml");
    fs::write(&local, "foo={path=\"x\"}\n").unwrap();
    Command::cargo_bin("rust-formatter")
        .unwrap()
        .arg(&tree)
        .assert()
        .success();

    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&real).unwrap(), "foo={path=\"x\"}\n");
    assert_eq!(fs::read_to_string(&local).unwrap(), "foo.path = \"x\"\n");
}

/// `--fail-fast` used to return the error alone, dropping the record of every
/// file it had already rewritten on disk.
#[test]
fn fail_fast_still_reports_what_it_wrote() {
    let temp = tempdir().unwrap();
    let good = temp.path().join("a_good.toml");
    let bad = temp.path().join("b_bad.toml");
    fs::write(&good, "foo={path=\"x\"}\n").unwrap();
    fs::write(&bad, "this is not = = toml\n").unwrap();

    // `-j 1` is what makes the order the assertion depends on: the queue is
    // drained in path order, so the good file is formatted before the bad one
    // stops the run.
    let out = Command::cargo_bin("rust-formatter")
        .unwrap()
        .args(["--fail-fast", "-j", "1", "--message-format", "json"])
        .arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .get_output()
        .clone();

    assert_eq!(fs::read_to_string(&good).unwrap(), "foo.path = \"x\"\n");

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["summary"]["changed"], 1);
    assert_eq!(report["summary"]["errors"], 1);
    assert_eq!(report["exit_code"], 2);
    assert_eq!(report["files"][0]["path"], good.display().to_string());
    assert_eq!(report["files"][0]["status"], "formatted");
}

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

/// A `rustfmt.toml` is the one place rustfmt's unstable options do not apply on
/// stable -- which is exactly why this tool passes its own through `--config`.
/// A project that carries such a file therefore formats differently there, and
/// that has to be said out loud rather than failing the run or passing in
/// silence.
#[test]
fn test_a_project_rustfmt_toml_reports_what_stable_dropped() {
    needs_stable!();

    let temp = tempdir().unwrap();
    cargo_project(temp.path(), "p");
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("rustfmt.toml"),
        "empty_item_single_line = false\n",
    )
    .unwrap();
    fs::write(temp.path().join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();

    formatter()
        .args(["--toolchain", "stable", "--check"])
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "warning: rustfmt: Warning: can't set `empty_item_single_line",
        ));
}

fn available_toolchains() -> Vec<&'static str> {
    [
        ("nightly", nightly_available()),
        ("stable", stable_available()),
    ]
    .into_iter()
    .filter_map(|(toolchain, available)| available.then_some(toolchain))
    .collect()
}

fn forged_comment(target: &Path, body: &str) -> String {
    format!("fn a() {{}}\n\n/{}:\n\n{body}\n", target.display())
}

#[cfg(unix)]
#[test]
fn a_comment_shaped_like_a_header_writes_no_other_file() {
    for toolchain in available_toolchains() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let tree = root.join("tree");
        let victim = root.join("victim").join("v.rs");
        fs::create_dir_all(&tree).unwrap();
        fs::create_dir_all(victim.parent().unwrap()).unwrap();

        let carrier = tree.join("c.rs");
        let sibling = tree.join("d.rs");
        fs::write(&victim, "fn keep() {}\n").unwrap();
        fs::write(&carrier, forged_comment(&victim, "fn  injected() {}")).unwrap();
        fs::write(&sibling, "fn  d(){}\n").unwrap();

        formatter()
            .args(["--toolchain", toolchain])
            .arg(&tree)
            .assert()
            .success();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "fn keep() {}\n");
        assert_eq!(
            fs::read_to_string(&carrier).unwrap(),
            forged_comment(&victim, "fn injected() {}"),
            "{toolchain}"
        );
        assert_eq!(fs::read_to_string(&sibling).unwrap(), "fn d() {}\n");
    }
}

fn forged_string(target: &Path, item: &str) -> String {
    format!(
        "{item}\nconst S: &str = r\"\n{}:\n\nfn stolen() {{}}\n\";\n",
        target.display()
    )
}

#[test]
fn a_string_naming_a_sibling_keeps_both_files_their_own_text() {
    for toolchain in available_toolchains() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();

        let carrier = root.join("a.rs");
        let sibling = root.join("b.rs");
        fs::write(&carrier, forged_string(&sibling, "fn  a(){}")).unwrap();
        fs::write(&sibling, "fn  b(){}\n").unwrap();

        formatter()
            .args(["--toolchain", toolchain])
            .arg(&root)
            .assert()
            .success();

        assert_eq!(
            fs::read_to_string(&carrier).unwrap(),
            forged_string(&sibling, "fn a() {}"),
            "{toolchain}"
        );
        assert_eq!(fs::read_to_string(&sibling).unwrap(), "fn b() {}\n");
    }
}

#[cfg(unix)]
#[test]
fn a_single_file_still_formats_its_child_modules_without_following_forged_headers() {
    for toolchain in available_toolchains() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let src = root.join("src");
        let shared = root.join("shared");
        let victim = root.join("victim.rs");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&shared).unwrap();

        let main = src.join("main.rs");
        let carrier = src.join("c.rs");
        let child = src.join("d.rs");
        let outside = shared.join("e.rs");
        fs::write(&victim, "fn keep() {}\n").unwrap();
        fs::write(
            &main,
            "mod c;\nmod d;\n#[path = \"../shared/e.rs\"]\nmod e;\nfn main(){}\n",
        )
        .unwrap();
        fs::write(&carrier, forged_comment(&victim, "fn  injected() {}")).unwrap();
        fs::write(&child, "fn  d(){}\n").unwrap();
        fs::write(&outside, "fn  e(){}\n").unwrap();

        formatter()
            .args(["--toolchain", toolchain])
            .arg(&main)
            .assert()
            .success();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "fn keep() {}\n");
        assert_eq!(
            fs::read_to_string(&main).unwrap(),
            "mod c;\nmod d;\n#[path = \"../shared/e.rs\"]\nmod e;\nfn main() {}\n",
            "{toolchain}"
        );
        assert_eq!(
            fs::read_to_string(&carrier).unwrap(),
            forged_comment(&victim, "fn injected() {}")
        );
        assert_eq!(fs::read_to_string(&child).unwrap(), "fn d() {}\n");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "fn e() {}\n");
    }
}

#[test]
fn quiet_still_rewrites_a_dirty_file() {
    for toolchain in available_toolchains() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("a.rs");
        let tree = temp.path().join("tree");
        let walked = tree.join("b.rs");
        fs::create_dir_all(&tree).unwrap();
        fs::write(&file, "fn  main(){}\n").unwrap();
        fs::write(&walked, "fn  b(){}\n").unwrap();

        formatter()
            .args(["-q", "--toolchain", toolchain])
            .arg(&file)
            .assert()
            .success();
        formatter()
            .args(["-q", "--toolchain", toolchain])
            .arg(&tree)
            .assert()
            .success();

        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "fn main() {}\n",
            "{toolchain}"
        );
        assert_eq!(fs::read_to_string(&walked).unwrap(), "fn b() {}\n");
    }
}

#[test]
fn quiet_stable_check_still_fails_a_dirty_file() {
    needs_stable!();

    let temp = tempdir().unwrap();
    let file = temp.path().join("b.rs");
    fs::write(&file, "fn  main(){}\n").unwrap();

    formatter()
        .args(["-q", "--check", "--toolchain", "stable"])
        .arg(&file)
        .assert()
        .failure()
        .code(1);
    formatter()
        .args(["-q", "--check", "--toolchain", "stable"])
        .arg(temp.path())
        .assert()
        .failure()
        .code(1);

    assert_eq!(fs::read_to_string(&file).unwrap(), "fn  main(){}\n");
}

#[test]
fn stable_check_of_a_single_file_reports_its_dirty_child_module() {
    needs_stable!();

    let temp = tempdir().unwrap();
    let src = temp.path().join("src");
    fs::create_dir_all(&src).unwrap();

    let main = src.join("main.rs");
    let child = src.join("foo.rs");
    fs::write(&main, "mod foo;\nfn main() {}\n").unwrap();
    fs::write(&child, "fn  x(){}\n").unwrap();

    formatter()
        .args(["--check", "--toolchain", "stable"])
        .arg(&main)
        .assert()
        .failure()
        .code(1)
        .stdout(predicates::str::contains("foo.rs"));

    assert_eq!(fs::read_to_string(&child).unwrap(), "fn  x(){}\n");
}

#[cfg(unix)]
#[test]
fn a_failed_write_still_formats_and_reports_the_rest_of_the_chunk() {
    use std::os::unix::fs::PermissionsExt;

    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    for toolchain in available_toolchains() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let locked_dir = root.join("b_locked");
        fs::create_dir(&locked_dir).unwrap();

        let before = root.join("a.rs");
        let locked = locked_dir.join("b.rs");
        let after = root.join("c.rs");
        for path in [&before, &locked, &after] {
            fs::write(path, "fn  x(){}\n").unwrap();
        }
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let _guard = Restore(locked_dir.clone());

        if fs::write(locked_dir.join("probe"), "").is_ok() {
            eprintln!("SKIP: directory permissions are not enforced for this user");
            return;
        }

        let out = formatter()
            .args([
                "-j",
                "1",
                "--message-format",
                "json",
                "--toolchain",
                toolchain,
            ])
            .arg(&root)
            .assert()
            .failure()
            .code(2)
            .get_output()
            .clone();

        assert_eq!(fs::read_to_string(&before).unwrap(), "fn x() {}\n");
        assert_eq!(fs::read_to_string(&after).unwrap(), "fn x() {}\n");
        assert_eq!(fs::read_to_string(&locked).unwrap(), "fn  x(){}\n");

        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let status_of = |path: &Path| {
            report["files"]
                .as_array()
                .unwrap()
                .iter()
                .find(|file| file["path"] == path.display().to_string())
                .map(|file| file["status"].clone())
        };
        assert_eq!(report["exit_code"], 2, "{toolchain}: {report}");
        assert_eq!(report["summary"]["errors"], 1, "{report}");
        assert_eq!(report["errors"][0]["path"], locked.display().to_string());
        assert_eq!(status_of(&before), Some("formatted".into()), "{report}");
        assert_eq!(status_of(&after), Some("formatted".into()), "{report}");
    }
}

#[test]
fn newline_style_decides_the_line_endings_written() {
    for toolchain in available_toolchains() {
        let temp = tempdir().unwrap();
        let crlf = temp.path().join("crlf.rs");
        let lf_tree = temp.path().join("windows");
        let lf = lf_tree.join("lf.rs");
        fs::create_dir_all(&lf_tree).unwrap();
        fs::write(&crlf, "fn main() {}\r\n").unwrap();
        fs::write(&lf, "fn main() {}\n").unwrap();
        fs::write(
            lf_tree.join("rustfmt.toml"),
            "newline_style = \"Windows\"\n",
        )
        .unwrap();

        let run = |args: &[&str]| {
            let mut cmd = formatter();
            cmd.args(["--toolchain", toolchain]).args(args);
            cmd
        };

        run(&["--check"]).arg(&crlf).assert().success();
        run(&["--check", "--config", "newline_style=Unix"])
            .arg(&crlf)
            .assert()
            .failure()
            .code(1);
        run(&["--check"]).arg(&lf_tree).assert().failure().code(1);
        run(&[
            "--stdin",
            "--stdin-filepath",
            "crlf.rs",
            "--config",
            "newline_style=Unix",
        ])
        .write_stdin("fn main() {}\r\n")
        .assert()
        .success()
        .stdout("fn main() {}\n");

        run(&["--config", "newline_style=Unix"])
            .arg(&crlf)
            .assert()
            .success();
        run(&[]).arg(&lf_tree).assert().success();

        assert_eq!(fs::read(&crlf).unwrap(), b"fn main() {}\n", "{toolchain}");
        assert_eq!(fs::read(&lf).unwrap(), b"fn main() {}\r\n", "{toolchain}");
    }
}
