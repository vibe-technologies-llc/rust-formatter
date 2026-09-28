#[path = "support/toolchain.rs"]
mod toolchain;

use std::{fs, path::Path, process::Command as StdCommand};

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

fn write_package(dir: &Path, name: &str, edition: &str, lib: &str) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n"),
    )
    .unwrap();
    fs::write(dir.join("src").join("lib.rs"), lib).unwrap();
}

// --------------------------------------------------------------- -j / --jobs

/// `-j 1` short-circuits before the round-robin deal while `-j >= 2` takes the
/// sharded path, so both have to land on the same bytes.
#[test]
fn jobs_settings_agree_on_the_result() {
    let bodies: Vec<String> = (0..40)
        .map(|index| format!("dep{index}={{path=\"p{index}\"}}\n"))
        .collect();

    let mut outputs = Vec::new();
    for jobs in ["1", "2", "7"] {
        let temp = tempdir().unwrap();
        for (index, body) in bodies.iter().enumerate() {
            fs::write(temp.path().join(format!("f{index}.toml")), body).unwrap();
        }

        formatter()
            .arg("-j")
            .arg(jobs)
            .arg(temp.path())
            .assert()
            .success()
            .code(0);

        let mut formatted: Vec<String> = (0..bodies.len())
            .map(|index| fs::read_to_string(temp.path().join(format!("f{index}.toml"))).unwrap())
            .collect();
        formatted.sort();
        outputs.push(formatted);
    }

    assert_eq!(outputs[0], outputs[1], "-j 1 and -j 2 disagree");
    assert_eq!(outputs[1], outputs[2], "-j 2 and -j 7 disagree");
    assert!(outputs[0][0].contains(" = "), "{:?}", outputs[0][0]);
}

/// Files are dealt round-robin into one bucket per worker; a bucket that never
/// ran would leave its share of the tree untouched.
#[test]
fn every_shard_is_formatted() {
    let temp = tempdir().unwrap();
    for index in 0..20 {
        fs::write(
            temp.path().join(format!("f{index}.toml")),
            format!("k{index}={{path=\"x\"}}\n"),
        )
        .unwrap();
    }

    formatter()
        .arg("-j")
        .arg("3")
        .arg(temp.path())
        .assert()
        .success()
        .code(0);

    for index in 0..20 {
        let body = fs::read_to_string(temp.path().join(format!("f{index}.toml"))).unwrap();
        assert_eq!(body, format!("k{index}.path = \"x\"\n"), "shard {index}");
    }
}

#[test]
fn jobs_rejects_zero() {
    let temp = tempdir().unwrap();
    fs::write(temp.path().join("a.toml"), "a = 1\n").unwrap();

    formatter()
        .arg("-j")
        .arg("0")
        .arg(temp.path())
        .assert()
        .failure()
        .code(2);
}

// ------------------------------------------------------------------ chunking

/// rustfmt is invoked in chunks of 500 paths. Only `-j 1` reaches that loop:
/// at any higher job count the round-robin deal leaves each bucket under the
/// threshold. 1001 files means three chunks, so both boundaries are crossed.
#[test]
fn rustfmt_chunk_boundaries_format_every_file() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    for index in 0..1001 {
        fs::write(
            temp.path().join(format!("f{index}.rs")),
            "pub fn  f( ) {  }\n",
        )
        .unwrap();
    }

    formatter()
        .arg("-j")
        .arg("1")
        .arg(temp.path())
        .assert()
        .success()
        .code(0);

    for index in 0..1001 {
        let body = fs::read_to_string(temp.path().join(format!("f{index}.rs"))).unwrap();
        assert_eq!(body, "pub fn f() {}\n", "file {index} was not formatted");
    }
}

// ------------------------------------------------------------ -- extra args

#[test]
fn extra_args_reach_rustfmt() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let file = temp.path().join("wide.rs");
    let source = "pub fn f(first_parameter: String, second_parameter: usize, third: u8) -> usize {\n    0\n}\n";
    fs::write(&file, source).unwrap();

    formatter().arg(&file).assert().success().code(0);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        source,
        "fits inside the default max_width"
    );

    formatter()
        .arg(&file)
        .arg("--")
        .arg("--config")
        .arg("max_width=40")
        .assert()
        .success()
        .code(0);

    let narrow = fs::read_to_string(&file).unwrap();
    assert!(narrow.contains("pub fn f(\n"), "{narrow}");
}

#[test]
fn a_rejected_extra_arg_is_a_tool_error() {
    let temp = tempdir().unwrap();
    let file = temp.path().join("a.rs");
    fs::write(&file, "fn main() {}\n").unwrap();

    formatter()
        .arg(&file)
        .arg("--")
        .arg("--this-flag-does-not-exist")
        .assert()
        .failure()
        .code(2);
}

// -------------------------------------------------------- multi-edition work

/// `let dyn = 1;` only parses before 2018 and `async fn` only from 2018 on, so
/// a run that exits 0 proves each member was formatted at its own edition
/// rather than one edition being applied to the whole workspace.
#[test]
fn workspace_members_are_formatted_at_their_own_edition() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"old\", \"new\"]\nresolver = \"2\"\n",
    )
    .unwrap();

    let old_lib = "pub fn f() {\n    let dyn = 1;\n    let _ = dyn;\n}\n";
    let new_lib = "pub async fn g() {}\n";
    write_package(&temp.path().join("old"), "old", "2015", old_lib);
    write_package(&temp.path().join("new"), "new", "2021", new_lib);

    formatter().arg(temp.path()).assert().success().code(0);

    assert_eq!(
        fs::read_to_string(temp.path().join("old").join("src").join("lib.rs")).unwrap(),
        old_lib
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("new").join("src").join("lib.rs")).unwrap(),
        new_lib
    );
}

#[test]
fn cli_edition_overrides_every_member() {
    let temp = tempdir().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"new\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    write_package(
        &temp.path().join("new"),
        "new",
        "2021",
        "pub async fn g() {}\n",
    );

    // 2015 has no `async fn`, so forcing it must fail rather than be ignored.
    formatter()
        .arg("--edition")
        .arg("2015")
        .arg(temp.path())
        .assert()
        .failure()
        .code(2);
}

// ----------------------------------------------------- cargo metadata fallback

/// When `cargo metadata` fails on a standalone package the run falls back to
/// walking the directory and driving rustfmt directly, instead of erroring out.
#[test]
fn a_standalone_package_survives_cargo_metadata_failing() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"standalone\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nmissing = { path = \"does-not-exist\" }\n",
    )
    .unwrap();
    let lib = temp.path().join("src").join("lib.rs");
    fs::write(&lib, "pub fn  f( ) {  }\n").unwrap();

    formatter().arg(temp.path()).assert().success().code(0);

    assert_eq!(fs::read_to_string(&lib).unwrap(), "pub fn f() {}\n");
}

// ------------------------------------------------------------------ ignoring

#[test]
fn gitignore_is_respected_inside_a_real_repository() {
    let temp = tempdir().unwrap();
    let ok = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(temp.path())
        .status()
        .is_ok_and(|status| status.success());
    if !ok {
        eprintln!("SKIP: git is unavailable");
        return;
    }

    fs::write(temp.path().join(".gitignore"), "ignored/\ntop.toml\n").unwrap();
    fs::create_dir_all(temp.path().join("ignored")).unwrap();
    fs::create_dir_all(temp.path().join("nested")).unwrap();
    fs::write(temp.path().join("nested").join(".gitignore"), "deep.toml\n").unwrap();

    let dirty = "foo={path=\"x\"}\n";
    for path in [
        "top.toml",
        "ignored/inside.toml",
        "nested/deep.toml",
        "nested/kept.toml",
    ] {
        fs::write(temp.path().join(path), dirty).unwrap();
    }

    formatter().arg(temp.path()).assert().success().code(0);

    for path in ["top.toml", "ignored/inside.toml", "nested/deep.toml"] {
        assert_eq!(
            fs::read_to_string(temp.path().join(path)).unwrap(),
            dirty,
            "{path} should have been ignored"
        );
    }
    assert_eq!(
        fs::read_to_string(temp.path().join("nested").join("kept.toml")).unwrap(),
        "foo.path = \"x\"\n"
    );
}

#[test]
fn cargo_config_files_are_formatted() {
    let temp = tempdir().unwrap();
    let cargo_dir = temp.path().join(".cargo");
    fs::create_dir_all(&cargo_dir).unwrap();
    fs::write(
        cargo_dir.join("config.toml"),
        "[build]\nrustflags=[\"-C\",\"target-cpu=native\"]\n",
    )
    .unwrap();
    // Cargo still honours the extension-less spelling.
    fs::write(cargo_dir.join("config"), "alias={b=\"build\"}\n").unwrap();

    formatter().arg(temp.path()).assert().success().code(0);

    let with_extension = fs::read_to_string(cargo_dir.join("config.toml")).unwrap();
    assert!(
        with_extension.contains("rustflags = [\"-C\", \"target-cpu=native\"]"),
        "{with_extension}"
    );
    assert_eq!(
        fs::read_to_string(cargo_dir.join("config")).unwrap(),
        "alias.b = \"build\"\n"
    );
}

// --------------------------------------------------------------- edition scope

/// rustfmt's own default is 2015, where `async` is not a keyword, so a file with
/// no manifest above it has to be given a modern edition to parse at all.
#[test]
fn a_loose_file_with_no_manifest_is_formatted_at_a_modern_edition() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let script = temp.path().join("script.rs");
    fs::write(&script, "async fn f() {}\n").unwrap();

    formatter()
        .arg("--check")
        .arg("--verbose")
        .arg(&script)
        .assert()
        .success()
        .code(0)
        .stderr(predicates::str::contains("edition 2024"));
}

/// `cargo metadata` reports `package.edition` and an edition per target;
/// reading `targets[0]` answered for whichever target happened to be first and
/// applied it to the whole package. `dyn` is an identifier before 2018 and a
/// keyword from 2018 on, `async fn` is the other way round, so a package that
/// mixes the two only parses when each file is formatted at its own edition.
#[test]
fn each_target_is_formatted_at_its_own_edition() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"pt\"\nversion = \"0.1.0\"\nedition = \"2015\"\n\n\
         [[bin]]\nname = \"tool\"\npath = \"src/bin/tool.rs\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(temp.path().join("src").join("bin")).unwrap();
    let lib = "pub fn f() {\n    let dyn = 1;\n    let _ = dyn;\n}\n";
    let bin = "pub async fn g() {}\nfn main() {}\n";
    fs::write(temp.path().join("src").join("lib.rs"), lib).unwrap();
    fs::write(temp.path().join("src").join("bin").join("tool.rs"), bin).unwrap();

    formatter()
        .arg("--check")
        .arg(temp.path())
        .assert()
        .success()
        .code(0);
    assert_eq!(
        fs::read_to_string(temp.path().join("src").join("lib.rs")).unwrap(),
        lib
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("src").join("bin").join("tool.rs")).unwrap(),
        bin
    );
}

/// A project's own `rustfmt.toml` outranks an inferred edition, because rustfmt
/// ranks the command line above the file and the inference is this tool's guess.
#[test]
fn a_project_rustfmt_toml_keeps_the_last_word_on_edition() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    write_package(temp.path(), "p", "2015", "pub fn f() {}\n");
    fs::write(temp.path().join("rustfmt.toml"), "edition = \"2021\"\n").unwrap();
    fs::write(
        temp.path().join("src").join("lib.rs"),
        "pub async fn f() {}\n",
    )
    .unwrap();

    formatter()
        .arg("--check")
        .arg("--verbose")
        .arg(temp.path())
        .assert()
        .success()
        .code(0)
        .stderr(predicates::str::contains("edition per rustfmt.toml"));
}

/// An `edition` smuggled in through `--config` reaches rustfmt's command line,
/// where it silently outranks `--edition`, while the batching that has to group
/// files by edition never sees it.
#[test]
fn an_edition_in_config_is_reconciled_with_the_flag() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    fs::write(temp.path().join("a.rs"), "fn main() {}\n").unwrap();

    formatter()
        .arg("--edition")
        .arg("2021")
        .arg("--config")
        .arg("edition=2018")
        .arg(temp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("conflicts with --config"));

    formatter()
        .arg("--check")
        .arg("--verbose")
        .arg("--config")
        .arg("edition=2018")
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("edition: 2018"));
}

// ------------------------------------------------------------ workspace scope

/// A virtual manifest is not a package, so a walk over `packages` could not see
/// the root's own files at all. TOML at the same location was always collected.
#[test]
fn a_virtual_workspace_root_formats_its_loose_files() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    write_package(
        &temp.path().join("member"),
        "member",
        "2021",
        "pub fn f() {}\n",
    );
    let loose = temp.path().join("build.rs");
    fs::write(&loose, "fn  main( ){}\n").unwrap();

    formatter().arg(temp.path()).assert().success();
    assert_eq!(fs::read_to_string(&loose).unwrap(), "fn main() {}\n");
}

/// `[workspace] exclude` and a nested non-member are outside the workspace, and
/// were being formatted at the root package's edition. TOML already refused
/// them; Rust now uses the same rule.
#[test]
fn a_workspace_run_leaves_non_member_packages_alone() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [workspace]\nexclude = [\"excluded\"]\n",
    )
    .unwrap();
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();

    // Deliberately unformatted: a run that reached either package would report
    // it under `--check` and rewrite it without one.
    let outside = "pub fn  f( ){}\n";
    for name in ["excluded", "nested"] {
        write_package(&temp.path().join(name), name, "2015", outside);
    }

    formatter()
        .arg("--check")
        .arg(temp.path())
        .assert()
        .success()
        .code(0);
    formatter().arg(temp.path()).assert().success();
    for name in ["excluded", "nested"] {
        assert_eq!(
            fs::read_to_string(temp.path().join(name).join("src").join("lib.rs")).unwrap(),
            outside
        );
    }
}

// ------------------------------------------------------------ #[path] modules

/// `--skip-children` keeps rustfmt from rewriting files the selection left out,
/// but it also means a module outside every package directory is never reached
/// and `--check` calls the tree clean.
#[test]
fn a_path_attribute_module_outside_the_package_is_reached() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let package = temp.path().join("package");
    write_package(&package, "p", "2021", "");
    fs::write(
        package.join("src").join("lib.rs"),
        "#[path = \"../../shared/out.rs\"]\nmod out;\npub fn f() {}\n",
    )
    .unwrap();
    let outside = temp.path().join("shared").join("out.rs");
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    fs::write(&outside, "pub fn  outside( ){}\n").unwrap();

    formatter()
        .arg("--check")
        .arg(&package)
        .assert()
        .failure()
        .code(1)
        .stdout(predicates::str::contains("out.rs"));

    // The selection still decides: a discovered module goes through the same
    // filter as a walked one.
    formatter()
        .arg("--check")
        .arg("--exclude")
        .arg("out.rs")
        .arg(&package)
        .assert()
        .success()
        .code(0);

    formatter().arg(&package).assert().success();
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        "pub fn outside() {}\n"
    );
}

#[test]
fn a_path_attribute_with_space_after_the_hash_is_reached() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let package = temp.path().join("package");
    write_package(&package, "p", "2021", "");
    fs::write(
        package.join("src").join("lib.rs"),
        "#[ path = \"../../shared/out.rs\"]\nmod out;\npub fn f() {}\n",
    )
    .unwrap();
    let outside = temp.path().join("shared").join("out.rs");
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    fs::write(&outside, "pub fn  outside( ){}\n").unwrap();

    formatter()
        .arg("--check")
        .arg(&package)
        .assert()
        .failure()
        .code(1)
        .stdout(predicates::str::contains("out.rs"));
}

/// The discovery pass reparses the crate, so a tree that never spells the
/// attribute must not pay for it.
#[test]
fn a_tree_without_the_attribute_runs_no_discovery_pass() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    write_package(temp.path(), "p", "2021", "pub fn f() {}\n");

    formatter()
        .arg("--check")
        .arg("--verbose")
        .arg(temp.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("#[path] module").not());
}

// ------------------------------------------------------- toolchain resolution

/// A shim on `PATH` is the only way to observe what the tool asks rustup for
/// without installing the toolchain it names, and a date-pinned nightly is
/// exactly the case nobody can install on demand.
#[cfg(unix)]
fn shim(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
fn real_rustfmt(toolchain: &str) -> Option<String> {
    let out = StdCommand::new("rustup")
        .args(["which", "rustfmt", "--toolchain", toolchain])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
}

/// `--toolchain nightly-2026-06-01` is documented and cannot be tested against
/// a real toolchain, so the question the shim answers is the only one that
/// matters: is the pinned name the one rustup is asked for, verbatim?
#[cfg(unix)]
#[test]
fn a_pinned_toolchain_is_asked_for_by_name() {
    let Some(rustfmt) = real_rustfmt("nightly") else {
        eprintln!("SKIP: no nightly rustfmt");
        return;
    };
    let temp = tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = temp.path().join("asked.txt");
    shim(
        &bin,
        "rustup",
        &format!(
            "printf '%s\\n' \"$*\" >> {log}\n\
             case \"$1\" in which) echo {rustfmt};; *) exit 1;; esac",
            log = log.display(),
            rustfmt = rustfmt,
        ),
    );

    let file = temp.path().join("main.rs");
    fs::write(&file, "fn  main(){}\n").unwrap();

    formatter()
        .env("PATH", &bin)
        .env_remove("RUSTFMT")
        .args(["--toolchain", "nightly-2026-06-01"])
        .arg(&file)
        .assert()
        .success();

    let asked = fs::read_to_string(&log).unwrap();
    assert!(
        asked.contains("which rustfmt --toolchain nightly-2026-06-01"),
        "{asked}"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "fn main() {}\n");
}

/// `auto` prefers nightly, so a machine that has one keeps formatting exactly
/// as it did before the fallback existed.
#[cfg(unix)]
#[test]
fn auto_asks_for_nightly_first() {
    let Some(rustfmt) = real_rustfmt("nightly") else {
        eprintln!("SKIP: no nightly rustfmt");
        return;
    };
    let temp = tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = temp.path().join("asked.txt");
    shim(
        &bin,
        "rustup",
        &format!(
            "printf '%s\\n' \"$*\" >> {log}\n\
             case \"$1\" in which) echo {rustfmt};; *) exit 1;; esac",
            log = log.display(),
            rustfmt = rustfmt,
        ),
    );

    let file = temp.path().join("main.rs");
    fs::write(&file, "fn  main(){}\n").unwrap();
    formatter()
        .env("PATH", &bin)
        .env_remove("RUSTFMT")
        .arg(&file)
        .assert()
        .success();

    let asked = fs::read_to_string(&log).unwrap();
    assert_eq!(
        asked.lines().next(),
        Some("which rustfmt --toolchain nightly")
    );
}

/// With no rustup at all, `rustfmt` on `PATH` is the whole answer -- the case
/// a Nix profile, a distro package and `rust:alpine` all present.
#[cfg(unix)]
#[test]
fn a_rustfmt_on_path_is_used_without_rustup() {
    let Some(rustfmt) = real_rustfmt("nightly") else {
        eprintln!("SKIP: no nightly rustfmt");
        return;
    };
    let temp = tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    shim(&bin, "rustfmt", &format!("exec {rustfmt} \"$@\""));

    let file = temp.path().join("main.rs");
    fs::write(&file, "fn  main(){}\n").unwrap();
    formatter()
        .env("PATH", &bin)
        .env_remove("RUSTFMT")
        .arg(&file)
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&file).unwrap(), "fn main() {}\n");
}

/// The channel is detected by behaviour, not by a version string, so a rustfmt
/// that refuses `--unstable-features` must take the text transport even when
/// its version says nightly. This is what makes the stable path testable on a
/// machine that has no stable toolchain.
#[cfg(unix)]
#[test]
fn a_rustfmt_without_unstable_features_still_formats() {
    let Some(rustfmt) = real_rustfmt("nightly") else {
        eprintln!("SKIP: no nightly rustfmt");
        return;
    };
    let temp = tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    shim(
        &bin,
        "rustfmt",
        &format!(
            "for arg in \"$@\"; do\n\
             \x20 case \"$arg\" in --unstable-features) echo \"Unrecognized option\" >&2; exit 1;; \
             json) echo \"Invalid value for --emit\" >&2; exit 1;; esac\n\
             done\n\
             exec {rustfmt} \"$@\""
        ),
    );

    let file = temp.path().join("main.rs");
    fs::write(
        &file,
        "use std::io::Write;\nuse std::io::Read;\nfn main(){}\n",
    )
    .unwrap();

    formatter()
        .env("PATH", &bin)
        .env_remove("RUSTFMT")
        .args(["--check"])
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(predicates::str::contains("+use std::io::{Read, Write};"));

    formatter()
        .env("PATH", &bin)
        .env_remove("RUSTFMT")
        .arg(&file)
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "use std::io::{Read, Write};\nfn main() {}\n"
    );
}

// ------------------------------------------------------------------- caching

/// A run that reads an answer from a previous run has to give the same answer,
/// or the cache is a way of hiding work rather than of skipping it.
#[test]
fn a_warm_run_says_exactly_what_a_cold_one_says() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let store = temp.path().join("store");
    write_package(temp.path(), "p", "2021", "pub fn f() {}\n");
    fs::write(temp.path().join("dirty.toml"), "a=1\nb   =   2\n").unwrap();
    fs::write(temp.path().join("src").join("dirty.rs"), "pub fn g(  ){}\n").unwrap();

    let run = || {
        let assert = formatter()
            .env("RUST_FORMATTER_CACHE_DIR", &store)
            .args(["--check", temp.path().to_str().unwrap()])
            .assert()
            .failure()
            .code(1);
        String::from_utf8(assert.get_output().stdout.clone()).unwrap()
    };

    let cold = run();
    let warm = run();
    assert_eq!(cold, warm);
    assert!(warm.contains("dirty.toml"), "{warm}");
    assert!(warm.contains("dirty.rs"), "{warm}");
}

/// The one failure a cache must not have. A file that was clean when it was
/// remembered and is not clean now has to be reported.
#[test]
fn an_edit_is_never_hidden_by_the_cache() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let store = temp.path().join("store");
    write_package(temp.path(), "p", "2021", "pub fn f() {}\n");
    let toml = temp.path().join("clean.toml");
    let rust = temp.path().join("src").join("clean.rs");
    fs::write(&toml, "a = 1\n").unwrap();
    fs::write(&rust, "pub fn g() {}\n").unwrap();

    let check = || {
        formatter()
            .env("RUST_FORMATTER_CACHE_DIR", &store)
            .args(["--check", temp.path().to_str().unwrap()])
            .assert()
    };
    check().success();
    check().success();

    fs::write(&toml, "a   =   1\n").unwrap();
    fs::write(&rust, "pub fn g(  ){}\n").unwrap();
    check()
        .failure()
        .code(1)
        .stdout(predicates::str::contains("clean.toml"))
        .stdout(predicates::str::contains("clean.rs"));
}

/// Formatting is a function of the bytes *and* the settings, so an entry
/// written under one configuration must not answer for another.
#[test]
fn a_changed_setting_is_a_miss() {
    let temp = tempdir().unwrap();
    let store = temp.path().join("store");
    let file = temp.path().join("wide.toml");
    fs::write(
        &file,
        "k = [\"aaaaaaaaaaaaaaaa\", \"bbbbbbbbbbbbbbbb\", \"cccccccccccc\"]\n",
    )
    .unwrap();

    let check = |args: &[&str]| {
        formatter()
            .env("RUST_FORMATTER_CACHE_DIR", &store)
            .args(["--check", "--toml-only"])
            .args(args)
            .arg(&file)
            .assert()
    };
    check(&[]).success();
    check(&[]).success();
    check(&["--toml-max-width", "20"]).failure().code(1);
}

/// Nothing on disk, and the same answer.
#[test]
fn no_cache_writes_nothing() {
    let temp = tempdir().unwrap();
    let store = temp.path().join("store");
    let file = temp.path().join("a.toml");
    fs::write(&file, "a = 1\n").unwrap();

    for args in [
        vec!["--no-cache"],
        vec!["--cache=false"],
        vec!["--cache", "--no-cache"],
    ] {
        formatter()
            .env("RUST_FORMATTER_CACHE_DIR", &store)
            .args(["--check", "--toml-only"])
            .args(&args)
            .arg(&file)
            .assert()
            .success();
        assert!(!store.exists(), "{args:?} wrote {}", store.display());
    }

    formatter()
        .env("RUST_FORMATTER_CACHE", "0")
        .env("RUST_FORMATTER_CACHE_DIR", &store)
        .args(["--check", "--toml-only"])
        .arg(&file)
        .assert()
        .success();
    assert!(!store.exists());
}

/// A write run leaves a fixed point behind, and remembering it is what makes
/// the `--check` a pre-commit hook runs straight afterwards cheap.
#[test]
fn a_write_run_remembers_what_it_wrote() {
    let temp = tempdir().unwrap();
    let store = temp.path().join("store");
    let file = temp.path().join("a.toml");
    fs::write(&file, "a   =   1\n").unwrap();

    formatter()
        .env("RUST_FORMATTER_CACHE_DIR", &store)
        .args(["--toml-only"])
        .arg(&file)
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&file).unwrap(), "a = 1\n");

    formatter()
        .env("RUST_FORMATTER_CACHE_DIR", &store)
        .args(["--check", "--toml-only"])
        .arg(&file)
        .assert()
        .success();
    assert!(store.exists());
}

/// A cache is never a source of errors: a file this crate did not write, or
/// wrote in a format it no longer uses, is a miss.
#[test]
fn a_damaged_cache_file_is_only_a_slower_run() {
    let temp = tempdir().unwrap();
    let store = temp.path().join("store");
    let file = temp.path().join("a.toml");
    fs::write(&file, "a = 1\n").unwrap();

    let check = || {
        formatter()
            .env("RUST_FORMATTER_CACHE_DIR", &store)
            .args(["--check", "--toml-only"])
            .arg(&file)
            .assert()
    };
    check().success();

    for entry in fs::read_dir(&store).unwrap() {
        fs::write(entry.unwrap().path(), b"not a cache at all").unwrap();
    }
    check().success();
}

#[test]
fn a_project_rustfmt_toml_is_part_of_the_cache_key() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let store = temp.path().join("store");
    write_package(
        temp.path(),
        "p",
        "2021",
        "pub fn f() { let _x = 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10; }\n",
    );

    formatter()
        .env("RUST_FORMATTER_CACHE_DIR", &store)
        .args(["--rust-only", temp.path().to_str().unwrap()])
        .assert()
        .success();

    let check = || {
        formatter()
            .env("RUST_FORMATTER_CACHE_DIR", &store)
            .args(["--check", "--rust-only", temp.path().to_str().unwrap()])
            .assert()
    };
    check().success();

    fs::write(temp.path().join("rustfmt.toml"), "max_width = 20\n").unwrap();
    check().failure().code(1);
}

#[test]
fn a_range_run_is_not_cached_as_a_full_file() {
    needs_nightly!();

    let temp = tempdir().unwrap();
    let store = temp.path().join("store");
    write_package(temp.path(), "p", "2021", "fn a() {}\nfn  b(){}\n");

    formatter()
        .env("RUST_FORMATTER_CACHE_DIR", &store)
        .args([
            "--range",
            "1",
            temp.path().join("src/lib.rs").to_str().unwrap(),
        ])
        .assert()
        .success();

    formatter()
        .env("RUST_FORMATTER_CACHE_DIR", &store)
        .args(["--check", temp.path().to_str().unwrap()])
        .assert()
        .failure()
        .code(1)
        .stdout(predicates::str::contains("fn  b()"));

    formatter()
        .env("RUST_FORMATTER_CACHE_DIR", &store)
        .args(["--no-cache", "--check", temp.path().to_str().unwrap()])
        .assert()
        .failure()
        .code(1);
}

// ------------------------------------------------------- installing a rustfmt

/// Printing an install command and leaving the user to paste it is one step
/// more than the tool needs to ask for. `--install-toolchain` runs it, and a
/// non-interactive session -- an editor, a hook, CI -- never prompts.
#[cfg(unix)]
#[test]
fn install_toolchain_runs_the_command_it_would_have_printed() {
    let Some(rustfmt) = real_rustfmt("nightly") else {
        eprintln!("SKIP: no nightly rustfmt");
        return;
    };
    let temp = tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = temp.path().join("asked.txt");
    let installed = temp.path().join("installed");

    // Refuses until something creates the marker, which is what the install
    // command does. That is the whole contract: run it, then try again.
    shim(
        &bin,
        "rustup",
        &format!(
            "printf '%s\\n' \"$*\" >> {log}\n\
             case \"$1\" in\n\
             \x20 which) [ -f {installed} ] && echo {rustfmt} || {{ echo 'no rustfmt' >&2; exit 1; }};;\n\
             \x20 toolchain|component) : > {installed};;\n\
             \x20 *) exit 1;;\n\
             esac",
            log = log.display(),
            installed = installed.display(),
            rustfmt = rustfmt,
        ),
    );

    let file = temp.path().join("main.rs");
    fs::write(&file, "fn  main(){}\n").unwrap();

    // Without the flag it is refused, and the command is named rather than run.
    formatter()
        .env("PATH", &bin)
        .env_remove("RUSTFMT")
        .args(["--toolchain", "nightly-2026-06-01"])
        .arg(&file)
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("rustup toolchain install"));
    assert!(!installed.exists());

    formatter()
        .env("PATH", &bin)
        .env_remove("RUSTFMT")
        .args(["--install-toolchain", "--toolchain", "nightly-2026-06-01"])
        .arg(&file)
        .assert()
        .success();
    assert!(installed.exists());
    assert_eq!(fs::read_to_string(&file).unwrap(), "fn main() {}\n");

    let asked = fs::read_to_string(&log).unwrap();
    assert!(
        asked.contains("toolchain install nightly-2026-06-01 --component rustfmt"),
        "{asked}"
    );
}
