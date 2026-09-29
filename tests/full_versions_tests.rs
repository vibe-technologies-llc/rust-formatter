#[path = "support/toolchain.rs"]
mod toolchain;

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use assert_cmd::Command;
use tempfile::{TempDir, tempdir};
use toolchain::{needs_nightly, nightly_available};

use self::mock_registry::{MockRegistry, Reply, Request, index_jsonl, index_lines};

const MANIFEST: &str = r#"[package]
name = "pin-me"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1"
clap = "^4.6"
ignore = "0.4.33"
local = { path = "../local" }
"#;

fn write_project(dir: &Path, manifest: &str) {
    fs::write(dir.join("Cargo.toml"), manifest).unwrap();
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("lib.rs"), "pub fn f() {}\n").unwrap();
}

fn crates_io_stub(request: &Request) -> Reply {
    match request.name.as_str() {
        "serde" => Reply::ok(index_jsonl(&[
            ("1.0.100", false),
            ("1.2.3", false),
            ("1.9.9", true),
            ("2.0.0", false),
        ])),
        "clap" => Reply::ok(index_jsonl(&[
            ("4.6.0", false),
            ("4.6.6", false),
            ("4.7.0", false),
        ])),
        _ => Reply::status(404),
    }
}

/// Every run is pointed at the loopback index and given a cache directory of its
/// own, so no test sees another's `ETag` or the developer's own cargo state.
struct Fixture {
    registry: MockRegistry,
    cache: TempDir,
    project: TempDir,
}

impl Fixture {
    fn new<F>(route: F) -> Self
    where
        F: Fn(&Request) -> Reply + Send + Sync + 'static,
    {
        Self {
            registry: MockRegistry::start(route),
            cache: tempdir().unwrap(),
            project: tempdir().unwrap(),
        }
    }

    fn with(manifest: &str) -> Self {
        let fixture = Self::new(crates_io_stub);
        write_project(fixture.path(), manifest);
        fixture
    }

    fn path(&self) -> &Path {
        self.project.path()
    }

    fn manifest(&self) -> PathBuf {
        self.project.path().join("Cargo.toml")
    }

    fn text(&self) -> String {
        fs::read_to_string(self.manifest()).unwrap()
    }

    fn command(&self) -> Command {
        let mut cmd = formatter();
        cmd.env("RUST_FORMATTER_CRATES_IO_URL", self.registry.base_url())
            .env("RUST_FORMATTER_REGISTRY_CACHE_DIR", self.cache.path())
            .env_remove("CARGO_NET_OFFLINE")
            .env_remove("CARGO_HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY");
        cmd
    }

    fn run(&self) -> Command {
        let mut cmd = self.command();
        cmd.arg("--full-versions").arg(self.path());
        cmd
    }
}

#[test]
fn full_versions_completes_partial_requirements() {
    needs_nightly!();
    let fixture = Fixture::with(MANIFEST);
    fixture.run().assert().success().code(0);

    let formatted = fixture.text();
    // `1.9.9` is yanked and `2.0.0` is out of range, so `1` lands on `1.2.3`.
    assert!(formatted.contains("serde = \"1.2.3\""), "{formatted}");
    // `^4.6` admits `4.7.0` to cargo, so completing it must not narrow to 4.6.x.
    assert!(formatted.contains("clap = \"^4.7.0\""), "{formatted}");
    assert!(formatted.contains("ignore = \"0.4.33\""), "{formatted}");
    assert!(
        formatted.contains("local.path = \"../local\""),
        "{formatted}"
    );
    assert_eq!(fixture.registry.hits(), vec!["clap", "serde"]);
}

#[test]
fn operators_are_kept_and_wildcards_completed() {
    needs_nightly!();
    let fixture = Fixture::with(
        "[dependencies]\nclap = \"~4.6\"\nserde = \"=1.0\"\nignore = \"1.*\"\nregex = \"1.2.*\"\n\
         [dev-dependencies]\ntokio = \"1.x\"\n",
    );
    let route = |request: &Request| match request.name.as_str() {
        "clap" => Reply::ok(index_jsonl(&[("4.6.6", false), ("4.7.0", false)])),
        "serde" => Reply::ok(index_jsonl(&[("1.0.100", false), ("1.2.3", false)])),
        "ignore" | "tokio" => Reply::ok(index_jsonl(&[("1.7.3", false), ("2.0.0", false)])),
        "regex" => Reply::ok(index_jsonl(&[
            ("1.2.9", false),
            ("1.7.3", false),
            ("2.0.0", false),
        ])),
        _ => Reply::status(404),
    };
    let fixture = Fixture {
        registry: MockRegistry::start(route),
        ..fixture
    };
    fixture.run().assert().success();

    let formatted = fixture.text();
    assert!(formatted.contains("clap = \"~4.6.6\""), "{formatted}");
    assert!(formatted.contains("serde = \"=1.0.100\""), "{formatted}");
    assert!(formatted.contains("ignore = \"1.7.3\""), "{formatted}");
    assert!(formatted.contains("regex = \"~1.2.9\""), "{formatted}");
    assert!(formatted.contains("tokio = \"1.7.3\""), "{formatted}");
}

#[test]
fn publish_order_does_not_decide_the_pin() {
    needs_nightly!();
    let fixture = Fixture::with("[dependencies]\nserde = \"1\"\n");
    let route = |request: &Request| match request.name.as_str() {
        // The sparse index is written in publish order, so the newest release is
        // not the last line.
        "serde" => Reply::ok(index_jsonl(&[
            ("1.10.0", false),
            ("1.2.0", false),
            ("1.9.0", false),
        ])),
        _ => Reply::status(404),
    };
    let fixture = Fixture {
        registry: MockRegistry::start(route),
        ..fixture
    };
    fixture.run().assert().success();
    assert!(fixture.text().contains("serde = \"1.10.0\""));
}

#[test]
fn build_metadata_is_dropped_from_a_pin() {
    needs_nightly!();
    let fixture = Fixture::with("[dependencies]\ntoml_edit = \"0.25\"\n");
    let route = |request: &Request| match request.name.as_str() {
        "toml_edit" => Reply::ok(index_jsonl(&[
            ("0.25.12", false),
            ("0.25.13+spec-1.1.0", false),
        ])),
        _ => Reply::status(404),
    };
    let fixture = Fixture {
        registry: MockRegistry::start(route),
        ..fixture
    };
    fixture.run().assert().success();
    assert!(fixture.text().contains("toml_edit = \"0.25.13\""));
}

#[test]
fn a_leading_underscore_is_a_legal_crate_name() {
    needs_nightly!();
    let fixture = Fixture::with("[dependencies]\n_private = \"1\"\n");
    let route = |request: &Request| match request.name.as_str() {
        "_private" => Reply::ok(index_jsonl(&[("1.0.4", false)])),
        _ => Reply::status(404),
    };
    let fixture = Fixture {
        registry: MockRegistry::start(route),
        ..fixture
    };
    fixture.run().assert().success().code(0);
    assert!(fixture.text().contains("_private = \"1.0.4\""));
    assert_eq!(fixture.registry.hits(), vec!["_private"]);
}

#[test]
fn full_versions_check_names_the_version_finding() {
    needs_nightly!();
    let fixture = Fixture::with(MANIFEST);
    let before = fixture.text();

    let output = fixture
        .run()
        .arg("--check")
        .assert()
        .code(1)
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("version:"), "{stderr}");
    assert!(stderr.contains("serde"), "{stderr}");
    assert!(stderr.contains("\"1.2.3\""), "{stderr}");
    assert!(stderr.contains("--full-versions: 2 completed"), "{stderr}");
    assert_eq!(fixture.text(), before);
}

#[test]
fn a_registry_failure_stops_the_run_before_anything_is_written() {
    needs_nightly!();
    let fixture = Fixture::with(MANIFEST);
    let fixture = Fixture {
        registry: MockRegistry::start(|_: &Request| Reply::status(500)),
        ..fixture
    };
    let before = fixture.text();

    let output = fixture.run().assert().code(2).get_output().clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("registry lookup failed for"), "{stderr}");
    assert!(stderr.contains("500"), "{stderr}");
    assert_eq!(fixture.text(), before);
}

/// A typo or a private-registry dependency must not take the whole run with it:
/// every other crate is still pinned and the Rust half still runs.
#[test]
fn an_unknown_crate_is_skipped_and_named() {
    needs_nightly!();
    let fixture = Fixture::with(
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\nserde = \"1\"\nnot-a-crate = \"3\"\n",
    );

    let output = fixture.run().assert().code(0).get_output().clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not-a-crate"), "{stderr}");
    assert!(stderr.contains("no such crate"), "{stderr}");
    assert!(fixture.text().contains("serde = \"1.2.3\""));
    assert!(fixture.text().contains("not-a-crate = \"3\""));
}

#[test]
fn a_yanked_only_crate_is_reported_and_allow_yanked_pins_it() {
    needs_nightly!();
    let route = |request: &Request| match request.name.as_str() {
        "serde" => Reply::ok(index_jsonl(&[("1.0.1", true), ("1.0.2", true)])),
        _ => Reply::status(404),
    };
    let fixture = Fixture::new(route);
    write_project(fixture.path(), "[dependencies]\nserde = \"1\"\n");

    let output = fixture.run().assert().code(0).get_output().clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("yanked"), "{stderr}");
    assert!(fixture.text().contains("serde = \"1\""));

    fixture.run().arg("--allow-yanked").assert().success();
    assert!(fixture.text().contains("serde = \"1.0.2\""));
}

#[test]
fn a_prerelease_only_crate_is_reported() {
    needs_nightly!();
    let route = |request: &Request| match request.name.as_str() {
        "serde" => Reply::ok(index_jsonl(&[("1.0.0-rc.1", false)])),
        _ => Reply::status(404),
    };
    let fixture = Fixture::new(route);
    write_project(fixture.path(), "[dependencies]\nserde = \"1\"\n");

    let output = fixture.run().assert().code(0).get_output().clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("pre-release"), "{stderr}");
    assert!(fixture.text().contains("serde = \"1\""));
}

#[test]
fn a_requirement_that_cannot_be_completed_says_so() {
    needs_nightly!();
    let fixture = Fixture::with(
        "[dependencies]\nany = \"*\"\nranged = { version = \">=1, <2\" }\nbounded = \">=1.0\"\n",
    );

    let output = fixture.run().assert().code(0).get_output().clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("`*` has no major version"), "{stderr}");
    assert!(stderr.contains("multi-comparator range"), "{stderr}");
    assert!(stderr.contains("already names its bound"), "{stderr}");
    assert_eq!(fixture.registry.hits(), Vec::<String>::new());
}

#[test]
fn other_sources_are_left_alone_and_accounted_for() {
    needs_nightly!();
    let fixture = Fixture::with(
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\n\
         serde = \"1\"\n\
         local = { path = \"../local\", version = \"0.1\" }\n\
         remote = { git = \"https://example.com/r\", version = \"1.0\" }\n\
         private = { version = \"1.0\", registry = \"corp\" }\n\
         indexed = { version = \"1.0\", registry-index = \"https://example.com/i\" }\n\
         shared = { workspace = true }\n\n\
         [patch.crates-io]\npatched = { git = \"https://example.com/p\", version = \"1.0\" }\n\n\
         [replace]\n\"old:1.0.0\" = { git = \"https://example.com/o\", version = \"1.0\" }\n",
    );

    let output = fixture
        .run()
        .arg("--verbose")
        .assert()
        .code(0)
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();

    let text = fixture.text();
    assert!(text.contains("serde = \"1.2.3\""), "{text}");
    for untouched in [
        "path = \"../local\"",
        "registry = \"corp\"",
        "registry-index",
    ] {
        assert!(text.contains(untouched), "{text}");
    }
    assert!(text.contains("version = \"0.1\""), "{text}");
    assert!(text.contains("shared.workspace = true"), "{text}");

    for reason in [
        "a path dependency",
        "a git dependency",
        "the dependency names another registry",
        "inherited from the workspace",
        "[patch] entry",
        "[replace] is deprecated",
    ] {
        assert!(stderr.contains(reason), "missing {reason:?} in {stderr}");
    }
    assert_eq!(fixture.registry.hits(), vec!["serde"]);
}

#[test]
fn a_frozen_dependency_costs_no_request() {
    needs_nightly!();
    let fixture =
        Fixture::with("[dependencies]\n# fmt: off\nserde = \"1\"\n# fmt: on\nclap = \"^4.6\"\n");
    fixture.run().assert().success();

    assert!(fixture.text().contains("serde = \"1\""));
    assert!(fixture.text().contains("clap = \"^4.7.0\""));
    assert_eq!(fixture.registry.hits(), vec!["clap"]);
}

#[test]
fn a_pin_cannot_raise_the_minimum_compiler() {
    needs_nightly!();
    let route = |request: &Request| match request.name.as_str() {
        "serde" => Reply::ok(index_lines(&[
            ("1.0.0", false, Some("1.60")),
            ("1.1.0", false, Some("1.80")),
        ])),
        _ => Reply::status(404),
    };
    let fixture = Fixture::new(route);
    write_project(
        fixture.path(),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nrust-version = \"1.63\"\n\n\
         [dependencies]\nserde = \"1\"\n",
    );

    fixture.run().assert().success();
    assert!(
        fixture.text().contains("serde = \"1.0.0\""),
        "{}",
        fixture.text()
    );

    write_project(
        fixture.path(),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nrust-version = \"1.63\"\n\n\
         [dependencies]\nserde = \"1\"\n",
    );
    fixture
        .run()
        .arg("--ignore-rust-version")
        .assert()
        .success();
    assert!(
        fixture.text().contains("serde = \"1.1.0\""),
        "{}",
        fixture.text()
    );
}

#[test]
fn a_member_inherits_the_workspace_rust_version() {
    needs_nightly!();
    let route = |request: &Request| match request.name.as_str() {
        "serde" => Reply::ok(index_lines(&[
            ("1.0.0", false, Some("1.60")),
            ("1.1.0", false, Some("1.80")),
        ])),
        _ => Reply::status(404),
    };
    let fixture = Fixture::new(route);
    let root = fixture.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\nresolver = \"2\"\n\n\
         [workspace.package]\nrust-version = \"1.63\"\n",
    )
    .unwrap();
    let member = root.join("member");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::write(member.join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         rust-version.workspace = true\n\n[dependencies]\nserde = \"1\"\n",
    )
    .unwrap();

    fixture.run().assert().success();
    let text = fs::read_to_string(member.join("Cargo.toml")).unwrap();
    assert!(text.contains("serde = \"1.0.0\""), "{text}");
}

#[test]
fn stdin_inherits_the_workspace_rust_version() {
    needs_nightly!();
    let route = |request: &Request| match request.name.as_str() {
        "serde" => Reply::ok(index_lines(&[
            ("1.0.0", false, Some("1.60")),
            ("1.1.0", false, Some("1.80")),
        ])),
        _ => Reply::status(404),
    };
    let fixture = Fixture::new(route);
    let root = fixture.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\nresolver = \"2\"\n\n\
         [workspace.package]\nrust-version = \"1.63\"\n",
    )
    .unwrap();
    let member = root.join("member");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::write(member.join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();
    let manifest = "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         rust-version.workspace = true\n\n[dependencies]\nserde = \"1\"\n";
    fs::write(member.join("Cargo.toml"), manifest).unwrap();

    let out = fixture
        .command()
        .arg("--full-versions")
        .arg("--stdin")
        .arg("--stdin-filepath")
        .arg(member.join("Cargo.toml"))
        .write_stdin(manifest)
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("serde = \"1.0.0\""), "{text}");

    let preview = fixture
        .command()
        .arg("--full-versions")
        .arg("--emit")
        .arg("stdout")
        .arg(member.join("Cargo.toml"))
        .assert()
        .success()
        .get_output()
        .clone();
    let preview = String::from_utf8(preview.stdout).unwrap();
    assert!(preview.contains("serde = \"1.0.0\""), "{preview}");
}

#[test]
fn upgrade_bumps_a_complete_requirement_only_when_asked() {
    needs_nightly!();
    let route = |request: &Request| match request.name.as_str() {
        "serde" => Reply::ok(index_jsonl(&[
            ("1.0.0", false),
            ("1.7.3", false),
            ("2.0.1", false),
        ])),
        _ => Reply::status(404),
    };
    let fixture = Fixture::new(route);
    write_project(
        fixture.path(),
        "[dependencies]\nserde = \"1.0.0\"\npinned = \"=1.0.0\"\n",
    );

    fixture.run().assert().success();
    assert!(fixture.text().contains("serde = \"1.0.0\""));

    fixture.run().arg("--upgrade").assert().success();
    let text = fixture.text();
    assert!(text.contains("serde = \"1.7.3\""), "{text}");
    assert!(text.contains("pinned = \"=1.0.0\""), "{text}");
}

#[test]
fn upgrade_flags_reach_across_a_major_and_into_a_pin() {
    needs_nightly!();
    let route = |request: &Request| match request.name.as_str() {
        "serde" => Reply::ok(index_jsonl(&[("1.7.3", false), ("2.0.1", false)])),
        "pinned" => Reply::ok(index_jsonl(&[("1.0.0", false), ("1.4.0", false)])),
        _ => Reply::status(404),
    };
    let fixture = Fixture::new(route);
    write_project(
        fixture.path(),
        "[dependencies]\nserde = \"^1.0.0\"\npinned = \"=1.0.0\"\n",
    );

    fixture
        .run()
        .args(["--upgrade", "--upgrade-incompatible", "--upgrade-pinned"])
        .assert()
        .success();

    let text = fixture.text();
    assert!(text.contains("serde = \"^2.0.1\""), "{text}");
    assert!(text.contains("pinned = \"=1.4.0\""), "{text}");
}

#[test]
fn the_disk_cache_revalidates_rather_than_refetching() {
    needs_nightly!();
    let bodies = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&bodies);
    let route = move |request: &Request| match request.name.as_str() {
        "serde" => {
            if request.if_none_match.as_deref() == Some("\"v1\"") {
                return Reply::status(304);
            }
            counter.fetch_add(1, Ordering::SeqCst);
            Reply::ok(index_jsonl(&[("1.2.3", false)])).with_etag("\"v1\"")
        }
        _ => Reply::status(404),
    };
    let fixture = Fixture::new(route);
    write_project(fixture.path(), "[dependencies]\nserde = \"1\"\n");

    fixture.run().assert().success();
    assert!(fixture.text().contains("serde = \"1.2.3\""));

    write_project(fixture.path(), "[dependencies]\nserde = \"1\"\n");
    fixture.run().assert().success();
    assert!(fixture.text().contains("serde = \"1.2.3\""));

    assert_eq!(bodies.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.registry.hits(), vec!["serde", "serde"]);
}

#[test]
fn a_rate_limited_request_is_retried() {
    needs_nightly!();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let route = move |request: &Request| match request.name.as_str() {
        "serde" => {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                return Reply::status(429).with_header("Retry-After", "0");
            }
            Reply::ok(index_jsonl(&[("1.2.3", false)]))
        }
        _ => Reply::status(404),
    };
    let fixture = Fixture::new(route);
    write_project(fixture.path(), "[dependencies]\nserde = \"1\"\n");

    fixture.run().assert().success();
    assert!(fixture.text().contains("serde = \"1.2.3\""));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn a_redirect_off_the_registry_host_is_refused() {
    needs_nightly!();
    let route = |request: &Request| match request.name.as_str() {
        "serde" => {
            Reply::status(301).with_header("Location", "https://evil.example.com/se/rd/serde")
        }
        _ => Reply::status(404),
    };
    let fixture = Fixture::new(route);
    write_project(fixture.path(), "[dependencies]\nserde = \"1\"\n");
    let before = fixture.text();

    let output = fixture.run().assert().code(2).get_output().clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("refused a redirect"), "{stderr}");
    assert_eq!(fixture.text(), before);
}

#[test]
fn a_same_host_redirect_is_followed() {
    needs_nightly!();
    // The route has to name the server's own address, which is only known once
    // the listener is bound.
    let base: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let seen = Arc::clone(&base);
    let fixture = Fixture::new(move |request: &Request| match request.name.as_str() {
        "serde" => Reply::status(307).with_header(
            "Location",
            &format!(
                "{}/se/rd/serde-moved",
                seen.get().map(String::as_str).unwrap_or_default()
            ),
        ),
        "serde-moved" => Reply::ok(index_jsonl(&[("1.2.3", false)])),
        _ => Reply::status(404),
    });
    base.set(fixture.registry.base_url()).unwrap();
    write_project(fixture.path(), "[dependencies]\nserde = \"1\"\n");

    fixture.run().assert().success();
    assert!(fixture.text().contains("serde = \"1.2.3\""));
}

#[test]
fn full_versions_refuses_a_cleartext_override_to_a_remote_host() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    write_project(temp.path(), MANIFEST);
    let before = fs::read_to_string(temp.path().join("Cargo.toml")).unwrap();

    let output = Command::cargo_bin("rust-formatter")
        .unwrap()
        .env("RUST_FORMATTER_CRATES_IO_URL", "http://192.0.2.1/index")
        .env(
            "RUST_FORMATTER_REGISTRY_CACHE_DIR",
            temp.path().join("cache"),
        )
        .env("CARGO_HTTP_TIMEOUT", "2")
        .env("CARGO_NET_RETRY", "0")
        .arg("--full-versions")
        .arg(temp.path())
        .assert()
        .code(2)
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("registry lookup failed for"), "{stderr}");
    assert_eq!(
        fs::read_to_string(temp.path().join("Cargo.toml")).unwrap(),
        before
    );
}

#[test]
fn a_replaced_crates_io_source_refuses_to_pin() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    write_project(temp.path(), MANIFEST);
    let cargo = temp.path().join(".cargo");
    fs::create_dir_all(&cargo).unwrap();
    fs::write(
        cargo.join("config.toml"),
        "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\
         [source.vendored-sources]\ndirectory = \"vendor\"\n",
    )
    .unwrap();
    let before = fs::read_to_string(temp.path().join("Cargo.toml")).unwrap();

    let output = Command::cargo_bin("rust-formatter")
        .unwrap()
        .env(
            "RUST_FORMATTER_REGISTRY_CACHE_DIR",
            temp.path().join("cache"),
        )
        .env_remove("RUST_FORMATTER_CRATES_IO_URL")
        .arg("--full-versions")
        .arg(temp.path())
        .assert()
        .code(0)
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("vendored-sources"), "{stderr}");
    let text = fs::read_to_string(temp.path().join("Cargo.toml")).unwrap();
    assert!(text.contains("serde = \"1\""), "{text}");
    assert!(text.contains("clap = \"^4.6\""), "{text}");
    let _ = before;
}

#[test]
fn a_registry_url_names_the_index_explicitly() {
    needs_nightly!();
    let fixture = Fixture::with("[dependencies]\nserde = \"1\"\n");
    fixture
        .command()
        .arg("--full-versions")
        .arg("--registry-url")
        .arg(fixture.registry.base_url())
        .arg(fixture.path())
        .env_remove("RUST_FORMATTER_CRATES_IO_URL")
        .assert()
        .success();

    assert!(fixture.text().contains("serde = \"1.2.3\""));
}

#[test]
fn offline_reads_the_local_registry_index() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    write_project(&project, "[dependencies]\nserde = \"1\"\nabsent = \"1\"\n");

    let cargo_home = temp.path().join("cargo-home");
    let cache = cargo_home
        .join("registry")
        .join("index")
        .join("index.crates.io-1949cf8c6b5b557f")
        .join(".cache");
    let entry = cache.join("se").join("rd").join("serde");
    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    fs::write(&entry, cargo_index_cache(&[("1.2.3", false)])).unwrap();

    let output = Command::cargo_bin("rust-formatter")
        .unwrap()
        .env("CARGO_HOME", &cargo_home)
        .env(
            "RUST_FORMATTER_REGISTRY_CACHE_DIR",
            temp.path().join("cache"),
        )
        .env_remove("RUST_FORMATTER_CRATES_IO_URL")
        .arg("--full-versions")
        .arg("--offline")
        .arg(&project)
        .assert()
        .code(0)
        .get_output()
        .clone();

    let text = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(text.contains("serde = \"1.2.3\""), "{text}");
    assert!(text.contains("absent = \"1\""), "{text}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("offline"), "{stderr}");
}

#[test]
fn cargo_net_offline_is_honoured_without_the_flag() {
    needs_nightly!();
    let fixture = Fixture::with("[dependencies]\nserde = \"1\"\n");
    fixture
        .run()
        .env("CARGO_NET_OFFLINE", "true")
        .assert()
        .code(0);

    assert!(fixture.text().contains("serde = \"1\""));
    assert!(fixture.registry.hits().is_empty());
}

#[test]
fn a_parent_directory_cargo_config_reaches_a_run_inside_a_member() {
    needs_nightly!();
    let fixture = Fixture::new(crates_io_stub);
    let member = fixture.path().join("member");
    fs::create_dir_all(&member).unwrap();
    write_project(
        &member,
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nserde = \"1\"\n",
    );
    let cargo = fixture.path().join(".cargo");
    fs::create_dir_all(&cargo).unwrap();
    fs::write(cargo.join("config.toml"), "[net]\noffline = true\n").unwrap();

    fixture
        .command()
        .env("CARGO_HOME", fixture.cache.path().join("cargo-home"))
        .current_dir(&member)
        .arg("--full-versions")
        .arg(".")
        .assert()
        .code(0);

    let text = fs::read_to_string(member.join("Cargo.toml")).unwrap();
    assert!(text.contains("serde = \"1\""), "{text}");
    assert!(fixture.registry.hits().is_empty());
}

#[test]
fn only_cargo_dependency_tables_are_resolved() {
    needs_nightly!();
    let fixture = Fixture::with(
        "[package]\nname = \"anchored\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [package.metadata.mytool.dependencies]\nmetadata-only = \"1\"\nserde = \"1\"\n\n\
         [dependencies]\nclap = \"^4.6\"\n\n\
         [target.'cfg(unix)'.dev-dependencies]\nserde = \"1\"\n\n\
         [workspace]\n\n\
         [workspace.dependencies]\nclap = \"4.6\"\n\n\
         [workspace.metadata.dependencies]\nworkspace-metadata-only = \"1\"\n",
    );

    let output = fixture.run().assert().code(0).get_output().clone();

    let text = fixture.text();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let doc: toml_edit::DocumentMut = text.parse().unwrap();
    let metadata = &doc["package"]["metadata"]["mytool"]["dependencies"];
    assert_eq!(metadata["metadata-only"].as_str(), Some("1"), "{text}");
    assert_eq!(metadata["serde"].as_str(), Some("1"), "{text}");
    assert_eq!(
        doc["workspace"]["metadata"]["dependencies"]["workspace-metadata-only"].as_str(),
        Some("1"),
        "{text}"
    );
    assert_eq!(
        doc["dependencies"]["clap"].as_str(),
        Some("^4.7.0"),
        "{text}"
    );
    assert_eq!(
        doc["target"]["cfg(unix)"]["dev-dependencies"]["serde"].as_str(),
        Some("1.2.3"),
        "{text}"
    );
    assert_eq!(
        doc["workspace"]["dependencies"]["clap"].as_str(),
        Some("4.7.0"),
        "{text}"
    );
    assert!(!stderr.contains("metadata-only"), "{stderr}");
    assert_eq!(fixture.registry.hits(), vec!["clap", "serde"]);
}

#[test]
fn the_json_envelope_carries_the_version_findings() {
    needs_nightly!();
    let fixture = Fixture::with(MANIFEST);
    let output = fixture
        .run()
        .args(["--check", "--message-format", "json"])
        .assert()
        .code(1)
        .get_output()
        .clone();

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("a JSON envelope");
    let versions = report["versions"].as_array().expect("a versions array");

    let serde = versions
        .iter()
        .find(|entry| entry["crate"] == "serde")
        .expect("serde is reported");
    assert_eq!(serde["outcome"], "completed");
    assert_eq!(serde["completed"], "1.2.3");
    assert_eq!(serde["section"], "dependencies");
    assert_eq!(serde["requirement"], "1");
    assert!(serde["line"].is_number(), "{serde}");

    let local = versions
        .iter()
        .find(|entry| entry["crate"] == "local")
        .expect("the path dependency is reported");
    assert_eq!(local["outcome"], "skipped");
    assert!(
        local["reason"]
            .as_str()
            .unwrap()
            .contains("path dependency"),
        "{local}"
    );
}

#[test]
fn full_versions_asks_for_each_crate_exactly_once() {
    needs_nightly!();
    let route = |request: &Request| {
        if request.name.starts_with("dep-") {
            Reply::ok(index_jsonl(&[("0.4.33", false)]))
        } else {
            Reply::status(404)
        }
    };
    let fixture = Fixture::new(route);
    let mut manifest =
        String::from("[package]\nname = \"many\"\nversion = \"0.1.0\"\n\n[dependencies]\n");
    for index in 0..12 {
        let _ = writeln!(manifest, "dep-{index} = \"0.4\"");
    }
    write_project(fixture.path(), &manifest);

    fixture.run().assert().success();

    let text = fixture.text();
    for index in 0..12 {
        assert!(
            text.contains(&format!("dep-{index} = \"0.4.33\"")),
            "{text}"
        );
    }
    let mut expected: Vec<String> = (0..12).map(|index| format!("dep-{index}")).collect();
    expected.sort();
    assert_eq!(fixture.registry.hits(), expected);
}

/// An editor formatting a manifest buffer gets the same pins a file would, so
/// `--stdin` and a write run over the same path do not disagree.
#[test]
fn full_versions_reaches_the_stdin_path() {
    let fixture = Fixture::new(crates_io_stub);

    let out = fixture
        .command()
        .args([
            "--stdin",
            "--stdin-filepath",
            "Cargo.toml",
            "--full-versions",
        ])
        .write_stdin("[dependencies]\nserde = \"1\"\n")
        .assert()
        .success()
        .get_output()
        .clone();

    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "[dependencies]\nserde = \"1.2.3\"\n"
    );
    assert_eq!(fixture.registry.hits(), vec!["serde"]);
}

/// A buffer that is not a manifest has no dependency requirements to complete,
/// so the flag must not turn every `--stdin` call into a network round trip.
#[test]
fn full_versions_leaves_a_plain_toml_buffer_alone() {
    let fixture = Fixture::new(crates_io_stub);

    fixture
        .command()
        .args([
            "--stdin",
            "--stdin-filepath",
            "other.toml",
            "--full-versions",
        ])
        .write_stdin("[dependencies]\nserde = \"1\"\n")
        .assert()
        .success()
        .stdout("[dependencies]\nserde = \"1\"\n");

    assert!(fixture.registry.hits().is_empty());
}

#[test]
fn full_versions_cannot_be_combined_with_rust_only() {
    Command::cargo_bin("rust-formatter")
        .unwrap()
        .args(["--full-versions", "--rust-only"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("--rust-only excludes"));
}

#[test]
fn the_upgrade_flags_require_their_parents() {
    for args in [
        vec!["--upgrade"],
        vec!["--upgrade-incompatible"],
        vec!["--allow-yanked"],
        vec!["--ignore-rust-version"],
    ] {
        Command::cargo_bin("rust-formatter")
            .unwrap()
            .args(&args)
            .assert()
            .failure();
    }
}

/// Cargo's own index cache, which `--offline` reads: a version byte, a
/// little-endian index-format version, a NUL-terminated index version, then
/// `version NUL json NUL` pairs.
fn cargo_index_cache(rows: &[(&str, bool)]) -> Vec<u8> {
    let mut bytes = vec![3_u8];
    bytes.extend_from_slice(&2_u32.to_le_bytes());
    bytes.extend_from_slice(b"etag: \"local\"");
    bytes.push(0);
    for (num, yanked) in rows {
        bytes.extend_from_slice(num.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(
            format!(r#"{{"name":"serde","vers":"{num}","yanked":{yanked}}}"#).as_bytes(),
        );
        bytes.push(0);
    }
    bytes
}

mod mock_registry {
    use std::{
        fmt::Write as _,
        io::{Read as _, Write as _},
        net::{SocketAddr, TcpListener, TcpStream},
        sync::{
            Arc, Mutex, MutexGuard, PoisonError,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    };

    type Route = dyn Fn(&Request) -> Reply + Send + Sync + 'static;

    pub struct Request {
        pub name: String,
        pub if_none_match: Option<String>,
    }

    pub struct Reply {
        pub status: u16,
        pub body: String,
        pub headers: Vec<(String, String)>,
    }

    impl Reply {
        pub fn ok(body: String) -> Self {
            Self {
                status: 200,
                body,
                headers: Vec::new(),
            }
        }

        pub fn status(status: u16) -> Self {
            Self {
                status,
                body: String::new(),
                headers: Vec::new(),
            }
        }

        pub fn with_etag(self, etag: &str) -> Self {
            self.with_header("ETag", etag)
        }

        pub fn with_header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_owned(), value.to_owned()));
            self
        }
    }

    /// A sparse index on loopback: one thread per connection, HTTP/1.1
    /// keep-alive, and always a correct `Content-Length`.
    pub struct MockRegistry {
        addr: SocketAddr,
        shutdown: Arc<AtomicBool>,
        acceptor: Option<JoinHandle<()>>,
        hits: Arc<Mutex<Vec<String>>>,
    }

    impl MockRegistry {
        pub fn start<F>(route: F) -> Self
        where
            F: Fn(&Request) -> Reply + Send + Sync + 'static,
        {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock registry");
            let addr = listener.local_addr().expect("mock registry address");
            let shutdown = Arc::new(AtomicBool::new(false));
            let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let route: Arc<Route> = Arc::new(route);

            let acceptor = {
                let shutdown = Arc::clone(&shutdown);
                let hits = Arc::clone(&hits);
                thread::spawn(move || {
                    for stream in listener.incoming() {
                        if shutdown.load(Ordering::SeqCst) {
                            return;
                        }
                        let Ok(stream) = stream else { continue };
                        let route = Arc::clone(&route);
                        let hits = Arc::clone(&hits);
                        // One thread per connection: prewarm opens as many as
                        // `-j` allows and would otherwise block.
                        thread::spawn(move || serve(&stream, route.as_ref(), &hits));
                    }
                })
            };

            Self {
                addr,
                shutdown,
                acceptor: Some(acceptor),
                hits,
            }
        }

        pub fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }

        /// Every crate name the binary asked for, sorted.
        pub fn hits(&self) -> Vec<String> {
            let mut hits = lock(&self.hits).clone();
            hits.sort();
            hits
        }
    }

    impl Drop for MockRegistry {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            // Wake the blocked `accept()` so the acceptor observes the flag.
            let _ = TcpStream::connect(self.addr);
            if let Some(acceptor) = self.acceptor.take() {
                let _ = acceptor.join();
            }
        }
    }

    pub fn index_jsonl(rows: &[(&str, bool)]) -> String {
        let rows: Vec<(&str, bool, Option<&str>)> = rows
            .iter()
            .map(|(num, yanked)| (*num, *yanked, None))
            .collect();
        index_lines(&rows)
    }

    pub fn index_lines(rows: &[(&str, bool, Option<&str>)]) -> String {
        rows.iter()
            .map(|(num, yanked, rust_version)| match rust_version {
                Some(rust) => format!(
                    r#"{{"name":"x","vers":"{num}","yanked":{yanked},"rust_version":"{rust}"}}"#
                ),
                None => format!(r#"{{"name":"x","vers":"{num}","yanked":{yanked}}}"#),
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn serve(mut stream: &TcpStream, route: &Route, hits: &Mutex<Vec<String>>) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
        let mut buf: Vec<u8> = Vec::with_capacity(1024);
        let mut chunk = [0_u8; 1024];

        loop {
            let head_end = loop {
                if let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end;
                }
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => buf.extend_from_slice(&chunk[..read]),
                }
            };

            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            buf.drain(..head_end + 4);

            let mut lines = head.split("\r\n");
            let request_line = lines.next().unwrap_or_default().to_owned();
            let headers: Vec<(String, String)> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                .collect();
            let wants_close = headers
                .iter()
                .any(|(name, value)| name == "connection" && value.eq_ignore_ascii_case("close"));
            let if_none_match = headers
                .iter()
                .find(|(name, _)| name == "if-none-match")
                .map(|(_, value)| value.clone());

            let path = request_line.split(' ').nth(1).unwrap_or("/").to_owned();
            let reply = match crate_name_from_path(&path) {
                Some(name) => {
                    lock(hits).push(name.clone());
                    route(&Request {
                        name,
                        if_none_match,
                    })
                }
                None => Reply::status(404),
            };

            let reason = match reply.status {
                200 => "OK",
                301 => "Moved Permanently",
                304 => "Not Modified",
                307 => "Temporary Redirect",
                404 => "Not Found",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Status",
            };
            let connection = if wants_close { "close" } else { "keep-alive" };
            let mut response = format!(
                "HTTP/1.1 {} {reason}\r\n\
                 Content-Type: text/plain\r\n\
                 Content-Length: {}\r\n\
                 Connection: {connection}\r\n",
                reply.status,
                reply.body.len()
            );
            for (name, value) in &reply.headers {
                let _ = write!(response, "{name}: {value}\r\n");
            }
            response.push_str("\r\n");

            if stream.write_all(response.as_bytes()).is_err()
                || stream.write_all(reply.body.as_bytes()).is_err()
            {
                return;
            }
            let _ = stream.flush();

            if wants_close {
                return;
            }
        }
    }

    /// The last segment of a sparse index path, whatever prefix rule produced it.
    fn crate_name_from_path(path: &str) -> Option<String> {
        let path = path.split(['?', '#']).next()?;
        let name = path.rsplit('/').next()?;
        (!name.is_empty()).then(|| name.to_owned())
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }
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
