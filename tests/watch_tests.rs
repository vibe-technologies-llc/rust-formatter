//! `--watch`, through the real binary and a real filesystem watcher.
//!
//! Almost everything about watch mode -- which directories it watches, which
//! events wake it, how the loop settles -- is asserted as unit tests in
//! `src/watch.rs`, where there is no watcher and no timing to be flaky about.
//! What is left here are the two things those cannot prove: that a watcher can
//! actually be installed and delivers a save, and that a watch which can never
//! start says so and exits.

use std::{
    fs,
    path::{Path, PathBuf},
    process, thread,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use tempfile::tempdir;

/// Long enough that a loaded machine is not mistaken for a broken watcher.
const DEADLINE: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(25);

/// Whether this machine can start a filesystem watcher at all.
///
/// Skipping rather than failing, for the same reason the suites gate on a
/// rustfmt: an exhausted inotify limit, or a sandbox without inotify, says
/// nothing about the code under test.
fn watcher_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(rust_formatter::watch::available)
}

macro_rules! needs_watcher {
    () => {
        if !watcher_available() {
            eprintln!("SKIP: no filesystem watcher is available");
            return;
        }
    };
}

/// Reaps the watcher even when an assertion panics: a leaked `--watch` never
/// exits on its own.
struct Watcher(process::Child);

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(dir: &Path, cache: &Path) -> Watcher {
    let child = process::Command::new(assert_cmd::cargo::cargo_bin("rust-formatter"))
        .args(["--watch", "--toml-only", "-q"])
        .arg(dir)
        .env("RUST_FORMATTER_CACHE_DIR", cache)
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .spawn()
        .expect("spawn the watcher");
    Watcher(child)
}

/// Polls for a state rather than sleeping for a duration, which is what keeps
/// this from being flaky.
fn becomes(path: &PathBuf, expected: &str) -> bool {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if fs::read_to_string(path).is_ok_and(|body| body == expected) {
            return true;
        }
        thread::sleep(POLL);
    }
    false
}

/// TOML only, so no rustfmt is needed and the suite still passes with no
/// toolchain installed.
#[test]
fn a_save_is_reformatted() {
    needs_watcher!();
    let dir = tempdir().unwrap();
    let cache = tempdir().unwrap();

    let existing = dir.path().join("before.toml");
    fs::write(&existing, "a={b=\"c\"}\n").unwrap();

    let _watcher = spawn(dir.path(), cache.path());

    // The first pass formats what was already there.
    assert!(
        becomes(&existing, "a.b = \"c\"\n"),
        "the first pass did not format an existing file"
    );

    // Then a save the watcher had to notice for itself.
    let saved = dir.path().join("after.toml");
    fs::write(&saved, "d={e=\"f\"}\n").unwrap();
    assert!(
        becomes(&saved, "d.e = \"f\"\n"),
        "a file written while watching was not formatted"
    );
}

/// A file the run excludes must not be rewritten, however much it changes --
/// the watcher re-runs the same run, so the same selection applies.
#[test]
fn an_excluded_save_is_left_alone() {
    needs_watcher!();
    let dir = tempdir().unwrap();
    let cache = tempdir().unwrap();

    let watched = dir.path().join("watched.toml");
    fs::write(&watched, "a={b=\"c\"}\n").unwrap();
    let generated = dir.path().join("gen/x.toml");
    fs::create_dir_all(generated.parent().unwrap()).unwrap();

    let child = process::Command::new(assert_cmd::cargo::cargo_bin("rust-formatter"))
        .args(["--watch", "--toml-only", "-q", "--exclude", "gen/**"])
        .arg(dir.path())
        .env("RUST_FORMATTER_CACHE_DIR", cache.path())
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .spawn()
        .expect("spawn the watcher");
    let _watcher = Watcher(child);

    assert!(becomes(&watched, "a.b = \"c\"\n"), "the first pass ran");

    let untouched = "g={h=\"i\"}\n";
    fs::write(&generated, untouched).unwrap();
    // Give the watcher every chance to get it wrong: one more save it *does*
    // care about has to be handled before the excluded one is judged.
    fs::write(&watched, "j={k=\"l\"}\n").unwrap();
    assert!(becomes(&watched, "j.k = \"l\"\n"), "the later save ran");
    assert_eq!(
        fs::read_to_string(&generated).unwrap(),
        untouched,
        "an excluded file must not be rewritten"
    );
}

/// A watch that could never start reports it and exits, rather than sitting
/// there watching nothing. This one terminates, so a blocking assert is safe.
#[test]
fn a_missing_path_exits_two() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("nowhere");
    let assert = Command::cargo_bin("rust-formatter")
        .unwrap()
        .env("RUST_FORMATTER_CACHE_DIR", dir.path())
        .arg("--watch")
        .arg(&missing)
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("does not exist"), "{stderr}");
    assert!(
        assert.get_output().stdout.is_empty(),
        "stdout is the product stream and must stay empty"
    );
}
