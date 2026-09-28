mod support;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use support::{
    golden, needs_nightly, nightly_available,
    rust_comments::{comments, fingerprint, skip_regions},
    snippets::{self, PRESETS, Snippet},
};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust")
}

fn expected_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/expected/rust")
}

fn cache_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

fn load() -> Vec<Snippet> {
    snippets::discover(&fixtures_dir()).unwrap_or_else(|err| panic!("{err}"))
}

fn report(failures: &[String], what: &str) {
    if failures.is_empty() {
        return;
    }
    for failure in failures {
        eprintln!("\n{failure}");
    }
    panic!(
        "{} {what}; rerun with UPDATE_EXPECT=1 to accept golden drift",
        failures.len()
    );
}

/// Formats through the binary rather than the library, so what is pinned is the
/// flag set `rust-formatter` builds and hands to rustfmt -- a lost `--config`
/// key is invisible to any in-process check.
///
/// These bytes are rustfmt's, so a toolchain update will move them. That is the
/// point -- nightly output drift is otherwise invisible. Read the diff, then
/// `UPDATE_EXPECT=1 cargo test --test snippet_tests` to accept it.
#[test]
fn snippets_match_their_goldens() {
    needs_nightly!();
    let root = expected_dir();
    let mut produced = BTreeSet::new();
    let mut failures = Vec::new();

    for snippet in load() {
        for preset in &snippet.presets {
            let expected = snippet.golden(&root, preset);
            match snippets::format(&snippet, preset, cache_dir()) {
                Ok(formatted) => {
                    if let Some(detail) = golden::check(&expected, formatted.as_bytes()) {
                        failures.push(format!("{} [{preset}]\n{detail}", snippet.stem));
                    }
                }
                Err(err) => failures.push(err),
            }
            produced.insert(expected);
        }
    }

    report(&failures, "Rust goldens differ");
    golden::assert_no_orphans(&root, &produced);
}

/// A second pass over the output must change nothing. rustfmt alone does not
/// guarantee this once `group_imports` meets a comment between imports, which
/// is why the runner makes more than one pass; the fixture set includes that
/// case on purpose.
#[test]
fn snippets_are_fixed_points() {
    needs_nightly!();
    let mut failures = Vec::new();
    for snippet in load() {
        if !snippet.fixed_point {
            continue;
        }
        for preset in &snippet.presets {
            let once = match snippets::format(&snippet, preset, cache_dir()) {
                Ok(once) => once,
                Err(err) => {
                    failures.push(err);
                    continue;
                }
            };
            let mut again = snippet.clone();
            again.source = once.clone();
            match snippets::format(&again, preset, cache_dir()) {
                Ok(twice) if twice == once => {}
                Ok(twice) => failures.push(format!(
                    "{} [{preset}] is not a fixed point\n{}",
                    snippet.stem,
                    rust_formatter::unified_diff(
                        Path::new(&snippet.stem),
                        &once,
                        &twice,
                        rust_formatter::DEFAULT_DIFF_CONTEXT,
                    )
                )),
                Err(err) => failures.push(err),
            }
        }
    }
    report(&failures, "snippets are not fixed points");
}

#[test]
fn snippets_keep_comment_content() {
    needs_nightly!();
    let mut failures = Vec::new();
    for snippet in load() {
        if !snippet.comments_kept {
            continue;
        }
        for preset in &snippet.presets {
            match snippets::format(&snippet, preset, cache_dir()) {
                Ok(formatted) => {
                    if let Some(detail) = snippets::comments_lost(&snippet.source, &formatted) {
                        failures.push(format!("{} [{preset}]\n{detail}", snippet.stem));
                    }
                }
                Err(err) => failures.push(err),
            }
        }
    }
    report(&failures, "snippets lost comment content");
}

/// `#[rustfmt::skip]` is the Rust half of the `# fmt: off` escape hatch. Its
/// region has to come out byte for byte under every listed preset.
#[test]
fn skip_regions_survive_listed_presets() {
    needs_nightly!();
    let mut failures = Vec::new();
    let mut saw_skip = false;
    for snippet in load() {
        if !snippet.skip_intact {
            continue;
        }
        saw_skip = true;
        let regions = skip_regions(&snippet.source);
        assert!(
            !regions.is_empty(),
            "{} enables skip-intact but has no #[rustfmt::skip] item",
            snippet.stem
        );
        for preset in &snippet.presets {
            match snippets::format(&snippet, preset, cache_dir()) {
                Ok(formatted) => {
                    if let Some(detail) = snippets::skip_lost(&snippet.source, &formatted) {
                        failures.push(format!("{} [{preset}]\n{detail}", snippet.stem));
                    }
                }
                Err(err) => failures.push(err),
            }
        }
    }
    assert!(
        saw_skip,
        "the suite needs at least one #[rustfmt::skip] snippet"
    );
    report(&failures, "skip regions were reformatted");
}

#[test]
fn every_snippet_has_a_feature_comment() {
    let snippets = load();
    assert!(
        snippets.len() >= 50,
        "expected a real snippet set, found {}",
        snippets.len()
    );
    let mute: Vec<String> = snippets
        .iter()
        .filter(|snippet| comments(&snippet.source).is_empty())
        .map(|snippet| snippet.stem.clone())
        .collect();
    assert!(
        mute.is_empty(),
        "snippets with no comments (the suite documents features in comments): {mute:?}"
    );
}

#[test]
fn every_listed_preset_is_a_rust_preset() {
    for snippet in load() {
        for preset in &snippet.presets {
            assert!(
                PRESETS.contains(&preset.as_str()),
                "{} names unknown preset {preset}",
                snippet.stem
            );
        }
    }
}

#[test]
fn directives_are_stripped_and_unknown_keys_fail() {
    let root = Path::new("/tmp/snippets");
    let parsed = Snippet::from_text(
        root.join("imports/three_tier.rs.in"),
        root,
        "// @presets: default, comments\n\
         // @config: max_width=80\n\
         // @edition: 2024\n\
         // @range: 2\n\
         // @range: 4-5\n\
         // Three-tier grouping.\n\
         use crate::a;\n",
    )
    .unwrap();
    assert_eq!(parsed.stem, "imports/three_tier");
    assert_eq!(parsed.presets, ["default", "comments"]);
    assert_eq!(parsed.config.as_deref(), Some("max_width=80"));
    assert_eq!(parsed.edition.as_deref(), Some("2024"));
    assert_eq!(parsed.ranges, ["2", "4-5"]);
    assert!(parsed.source.starts_with("// Three-tier grouping."));
    assert!(!parsed.source.contains("@presets"));

    let err = Snippet::from_text(root.join("bad.rs.in"), root, "// @nope: true\nfn f() {}\n")
        .unwrap_err();
    assert!(err.contains("unknown directive `@nope`"), "{err}");
}

#[test]
fn comment_scanner_skips_strings_and_nests_blocks() {
    let src = r##"
        let a = "not // a comment";
        let b = r#"still // not"#;
        let c = 'x';
        let d = '\'';
        let e = c"c-string // no";
        // keep me
        /* outer /* inner */ still */
        crate::foo();
        br#"raw // no"#;
    "##;
    let bodies = comments(src);
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(bodies[0].contains("keep me"), "{bodies:?}");
    assert!(bodies[1].contains("outer"), "{bodies:?}");
    assert!(
        !bodies.iter().any(|body| body.contains("not // a comment")),
        "{bodies:?}"
    );
}

#[test]
fn fingerprint_survives_block_to_line_normalize() {
    let block = "/* an old-style block comment that is long enough to need wrapping */\n";
    let lines = "// an old-style block comment that is long\n// enough to need wrapping\n";
    assert_eq!(fingerprint(block), fingerprint(lines));
}

#[test]
fn skip_scanner_captures_the_item_after_the_attribute() {
    let src = r"
#[rustfmt::skip]
pub const MATRIX: [[u8; 3]; 2] = [
    [1,   2,   3],
    [40,  50,  60],
];
pub fn reformatted() {}
";
    let regions = skip_regions(src);
    assert_eq!(regions.len(), 1, "{regions:?}");
    assert!(regions[0].contains("[1,   2,   3]"), "{}", regions[0]);
}
