mod support;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
};

use rust_formatter::{
    FormatterOptions, TargetKind, TomlStyle, detect_target, format_toml, run_format,
};
use support::{
    Profile, comments,
    corpus::{self, corpus_files},
    golden, parse_spellings, parse_tree, profiles, regions, regions_survive, round_trip,
};
use toml_edit::DocumentMut;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn expected_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/expected/toml")
}

/// Every `<name>.toml.in` directly under `tests/fixtures`, sorted. The
/// subdirectories hold fixtures for other suites: `rust/` needs a toolchain and
/// `encoding/` is not valid UTF-8 text by design.
fn fixture_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(fixtures_dir())
        .expect("fixtures directory")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "in"))
        .collect();
    files.sort();
    files
}

#[derive(Debug)]
struct Failure {
    path: PathBuf,
    profile: String,
    kind: &'static str,
    detail: String,
    source: String,
}

/// Reads the file once and puts it through every option combination, because a
/// guarantee that only holds for the defaults is not a guarantee.
fn check_all(path: &Path, profiles: &[Profile]) -> Option<Vec<Failure>> {
    let source = fs::read_to_string(path).ok()?;
    // A file that does not parse is reported per-file by the runner; the
    // formatter makes no promises about it.
    let baseline = round_trip(&source).ok()?;
    let before = parse_tree(&source).expect("input parses");
    let spelled = parse_spellings(&source).expect("input parses");

    let failures: Vec<Failure> = profiles
        .iter()
        .filter_map(|profile| check_one(path, &source, &baseline, &before, &spelled, profile))
        .collect();
    (!failures.is_empty()).then_some(failures)
}

/// Everything the formatter promises about a single document, in the order
/// `docs/toml-style.md` states them: it succeeds, its output parses, no data
/// moves, every value keeps its spelling, it is a fixed point, no comment is
/// lost or displaced, and every `fmt: off` region comes out byte for byte.
fn check_one(
    path: &Path,
    source: &str,
    baseline: &str,
    before: &support::Tree,
    spelled: &[(String, String)],
    profile: &Profile,
) -> Option<Failure> {
    let fail = |kind: &'static str, detail: String| {
        Some(Failure {
            path: path.to_owned(),
            profile: profile.name.clone(),
            kind,
            detail,
            source: source.to_owned(),
        })
    };

    let once = match format_toml(source, &profile.style) {
        Ok(once) => once,
        Err(err) => return fail("format failed on a parseable document", err.to_string()),
    };

    let after = match parse_tree(&once) {
        Ok(after) => after,
        Err(err) => return fail("output does not parse", err.to_string()),
    };
    let (before, after) = (profile.normalize(before), profile.normalize(&after));
    if before != after {
        return fail("value tree changed", format!("{before:?}\n  ->\n{after:?}"));
    }

    let expected = profile.spelling_view(spelled);
    let actual = profile.spelling_view(&parse_spellings(&once).expect("output parses"));
    if expected != actual {
        return fail(
            "value spelling changed",
            format!("expected {expected:?}\n  actual {actual:?}"),
        );
    }

    let twice = format_toml(&once, &profile.style).expect("output parses, so it formats");
    if twice != once {
        return fail("not idempotent", first_difference(&once, &twice));
    }

    // Against the round-trip baseline, not the raw source: `toml_edit` regroups
    // interleaved dotted keys on its own and strands their comments.
    let expected = profile.comment_view(baseline);
    let actual = profile.comment_view(&once);
    if expected != actual {
        return fail(
            "comments changed",
            format!("expected {expected:?}\n  actual {actual:?}"),
        );
    }

    if !regions_survive(baseline, &once) {
        return fail(
            "directive region changed",
            format!(
                "expected {:?}\n  actual {:?}",
                regions(baseline),
                regions(&once)
            ),
        );
    }

    None
}

fn first_difference(left: &str, right: &str) -> String {
    let left: Vec<&str> = left.lines().collect();
    let right: Vec<&str> = right.lines().collect();
    let at = left
        .iter()
        .zip(&right)
        .position(|(a, b)| a != b)
        .unwrap_or(left.len().min(right.len()));
    format!("line {}: {:?} -> {:?}", at + 1, left.get(at), right.get(at))
}

fn report(failures: &[Failure]) {
    let mut by_kind: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for failure in failures {
        *by_kind
            .entry((failure.profile.as_str(), failure.kind))
            .or_default() += 1;
    }
    eprintln!("\n{} failing files:", failures.len());
    for ((profile, kind), count) in &by_kind {
        eprintln!("  {count:>5}  [{profile}] {kind}");
    }
    for failure in failures.iter().take(20) {
        eprintln!(
            "\n=== {} [{}] ===\n{}\n{}",
            failure.path.display(),
            failure.profile,
            failure.kind,
            failure.detail
        );
    }
    if failures.len() > 20 {
        eprintln!("\n... and {} more", failures.len() - 20);
    }

    let mut written = BTreeSet::new();
    for failure in failures {
        match corpus::record_failure(&failure.source, &failure.profile, failure.kind) {
            Ok(path) => {
                written.insert(path);
            }
            Err(err) => eprintln!("could not record {}: {err}", failure.path.display()),
        }
    }
    if !written.is_empty() {
        eprintln!(
            "\n{} inputs written to {}\npromote the interesting ones into tests/fixtures/ so they \
             are pinned independently of this machine's registry",
            written.len(),
            corpus::failure_dir().display(),
        );
    }
}

/// `check_all` skips a file that does not parse, because the formatter makes no
/// promise about one and a registry is full of them. A *committed* fixture is
/// different: we wrote it, so one that does not parse is exercising nothing at
/// all and the skip hides that.
#[test]
fn every_committed_fixture_parses() {
    let broken: Vec<String> = fixture_files()
        .iter()
        .filter_map(|path| {
            let source = fs::read_to_string(path).ok()?;
            let err = source.parse::<DocumentMut>().err()?;
            Some(format!("{}: {err}", path.display()))
        })
        .collect();
    assert!(
        broken.is_empty(),
        "committed fixtures that do not parse are checked by nothing:\n{}",
        broken.join("\n")
    );
}

/// A small adversarial set committed to the repo, so the guarantees are checked
/// on a machine whose registry is empty.
#[test]
fn committed_fixtures_are_idempotent_and_lossless() {
    let profiles = profiles();
    let files = fixture_files();
    assert!(
        files.len() >= 10,
        "expected a real fixture set, found {}",
        files.len()
    );

    let failures: Vec<Failure> = files
        .iter()
        .filter_map(|path| check_all(path, &profiles))
        .flatten()
        .collect();
    if !failures.is_empty() {
        report(&failures);
        panic!("{} fixtures failed", failures.len());
    }
}

/// The invariants above are all shape-preserving: a change that alters the
/// output while staying idempotent and lossless passes every one of them. Only
/// a byte comparison sees it.
///
/// Regenerate with `UPDATE_EXPECT=1 cargo test --test corpus_tests`, and read
/// the resulting diff -- it is the whole point of the file.
#[test]
fn committed_fixtures_match_their_goldens() {
    let profiles = profiles();
    let root = expected_dir();
    let mut produced = BTreeSet::new();
    let mut failures = Vec::new();

    for path in fixture_files() {
        let source = fs::read_to_string(&path).expect("fixture is UTF-8");
        let stem = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".toml.in"))
            .expect("fixture is <name>.toml.in");

        for profile in &profiles {
            // `.out`, never `.toml`: the repo's own `--check .` step formats
            // every `.toml` it finds with the *default* style, which would
            // rewrite the golden of every other profile.
            let expected = root.join(&profile.name).join(format!("{stem}.out"));
            let formatted = format_toml(&source, &profile.style).expect("fixture formats");
            if let Some(detail) = golden::check(&expected, formatted.as_bytes()) {
                failures.push(format!("{} [{}]\n{detail}", path.display(), profile.name));
            }
            produced.insert(expected);
        }
    }

    if !failures.is_empty() {
        for failure in &failures {
            eprintln!("\n{failure}");
        }
        panic!(
            "{} goldens differ; rerun with UPDATE_EXPECT=1 to accept",
            failures.len()
        );
    }
    golden::assert_no_orphans(&root, &produced);
}

#[test]
#[ignore = "sweeps the local crate registry; run with --ignored"]
fn registry_corpus_is_idempotent_and_lossless() {
    let files = corpus_files();
    if files.is_empty() {
        corpus::skip("no crate registry found; set RUST_FORMATTER_CORPUS_DIR");
        return;
    }
    let profiles = profiles();
    eprintln!(
        "corpus: {} files x {} profiles",
        files.len(),
        profiles.len()
    );

    let failures: Vec<Failure> = corpus::map_parallel(&files, |path| check_all(path, &profiles))
        .into_iter()
        .flatten()
        .collect();
    if !failures.is_empty() {
        report(&failures);
        panic!("{} corpus files failed", failures.len());
    }
}

/// The oracles above are only worth running if they can fail. Each of these
/// pairs differs in exactly the way one guarantee forbids, so an oracle that
/// stopped observing its property would show up here rather than as a suite
/// that passes vacuously -- which is what `BTreeMap` keys and sorted comment
/// lists were doing before.
mod the_oracles_can_fail {
    use super::{Profile, comments as comment_bodies, parse_spellings, parse_tree, profiles};

    fn profile(name: &str) -> Profile {
        profiles()
            .into_iter()
            .find(|profile| profile.name == name)
            .expect("preset exists")
    }

    #[test]
    fn the_value_tree_sees_a_key_swap() {
        let one = parse_tree("[t]\na = 1\nb = 2\n").unwrap();
        let other = parse_tree("[t]\nb = 2\na = 1\n").unwrap();
        assert_ne!(one, other, "key order is erased");

        let default = profile("default");
        assert_ne!(default.normalize(&one), default.normalize(&other));
    }

    /// A profile that sorts moves keys on purpose, so its view has to be the
    /// one that cannot see the swap.
    #[test]
    fn a_sorting_profile_forgives_a_key_swap() {
        let one = parse_tree("[t]\na = 1\nb = 2\n").unwrap();
        let other = parse_tree("[t]\nb = 2\na = 1\n").unwrap();
        let sorting = profile("everything");
        assert!(sorting.reorders());
        assert_eq!(sorting.normalize(&one), sorting.normalize(&other));
    }

    /// The three interchangeable spellings guarantee 2 names have to stay
    /// interchangeable, or the oracle would report the formatter's own rules.
    #[test]
    fn the_value_tree_does_not_see_a_legal_respelling() {
        let section = parse_tree("[a]\nb = 1\n").unwrap();
        let dotted = parse_tree("a.b = 1\n").unwrap();
        let inline = parse_tree("a = { b = 1 }\n").unwrap();
        assert_eq!(section, dotted);
        assert_eq!(section, inline);
    }

    #[test]
    fn the_comment_view_sees_a_permutation() {
        let default = profile("default");
        assert!(!default.reorders());
        assert_ne!(
            default.comment_view("# one\na = 1\n# two\nb = 2\n"),
            default.comment_view("# two\na = 1\n# one\nb = 2\n"),
        );
    }

    #[test]
    fn a_sorting_profile_forgives_a_comment_permutation() {
        let sorting = profile("everything");
        assert_eq!(
            sorting.comment_view("# one\na = 1\n# two\nb = 2\n"),
            sorting.comment_view("# two\na = 1\n# one\nb = 2\n"),
        );
    }

    /// Losing a comment must be visible under every profile, sorting or not.
    #[test]
    fn every_profile_sees_a_lost_comment() {
        for profile in profiles() {
            assert_ne!(
                profile.comment_view("# one\na = 1\n"),
                profile.comment_view("a = 1\n"),
                "{}",
                profile.name
            );
        }
    }

    /// Exactly the cases guarantee 5 names, each of which the value tree
    /// canonicalises away.
    #[test]
    fn the_spelling_view_sees_a_respelling_the_value_tree_cannot() {
        for (before, after) in [
            ("a = 0x1F\n", "a = 31\n"),
            ("a = 1.50\n", "a = 1.5\n"),
            ("a = 1e0\n", "a = 1.0\n"),
            ("a = 1_000\n", "a = 1000\n"),
            ("a = 0o17\n", "a = 15\n"),
            ("a = 0b101\n", "a = 5\n"),
            ("a = 'win\\path'\n", "a = \"win\\\\path\"\n"),
        ] {
            assert_eq!(
                parse_tree(before).unwrap(),
                parse_tree(after).unwrap(),
                "the value tree was expected to canonicalise {before:?}"
            );
            for profile in profiles() {
                assert_ne!(
                    profile.spelling_view(&parse_spellings(before).unwrap()),
                    profile.spelling_view(&parse_spellings(after).unwrap()),
                    "{}: {before:?} vs {after:?}",
                    profile.name
                );
            }
        }
    }

    /// The spelling oracle is keyed on the path, so the moves guarantee 2 does
    /// allow must not read as a changed spelling.
    #[test]
    fn the_spelling_view_does_not_see_a_legal_respelling() {
        let default = profile("default");
        let section = parse_spellings("[a]\nb = 0x1F\n").unwrap();
        let dotted = parse_spellings("a.b = 0x1F\n").unwrap();
        let inline = parse_spellings("a = { b = 0x1F }\n").unwrap();
        assert_eq!(
            default.spelling_view(&section),
            default.spelling_view(&dotted)
        );
        assert_eq!(
            default.spelling_view(&section),
            default.spelling_view(&inline)
        );
    }

    /// The marker padding the formatter is documented to rewrite is forgiven,
    /// but the marker run itself is not: `## note` and `# note` are different
    /// comments and gaining a `#` is a change.
    #[test]
    fn the_comment_view_forgives_padding_but_not_the_marker() {
        let default = profile("default");
        assert_eq!(
            default.comment_view("#note\n"),
            default.comment_view("#   note\n")
        );
        assert_eq!(
            default.comment_view("##note\n"),
            default.comment_view("## note\n")
        );
        assert_ne!(
            default.comment_view("#note\n"),
            default.comment_view("##note\n")
        );
        assert_ne!(
            default.comment_view("# note\n"),
            default.comment_view("# other\n")
        );
    }

    /// The scanner has to know where a string ends, or a `#` inside one is read
    /// as a comment and the oracle reports a comment the document never had.
    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        assert!(comment_bodies("a = \"not # a comment\"\n").is_empty());
        assert!(comment_bodies("a = 'not # a comment'\n").is_empty());
        assert!(comment_bodies("a = \"\"\"not # a comment\"\"\"\n").is_empty());
        assert_eq!(
            comment_bodies("a = \"x\" # real\n"),
            vec!["real".to_owned()]
        );
    }
}

/// `--check` claims a file needs formatting; a write run rewrites it. Those two
/// answers are decided from the same text comparison, but only the write path
/// runs the result back through the BOM/CRLF encoder, so they can still diverge
/// on bytes. Assert set equality over a real tree.
#[test]
#[ignore = "copies the crate registry into a tempdir; run with --ignored"]
fn check_reports_exactly_what_a_write_would_change() {
    let files = corpus_files();
    if files.is_empty() {
        corpus::skip("no crate registry found; set RUST_FORMATTER_CORPUS_DIR");
        return;
    }

    let parseable: Vec<PathBuf> = corpus::map_parallel(&files, |path| {
        let source = fs::read_to_string(path).ok()?;
        source.parse::<DocumentMut>().ok()?;
        Some(path.clone())
    });
    eprintln!("{} of {} files parse", parseable.len(), files.len());

    let temp = tempfile::tempdir().expect("tempdir");
    let mapping = corpus::flat_copy(&parseable, temp.path());
    let origin: HashMap<PathBuf, PathBuf> = mapping
        .iter()
        .map(|(source, dest)| (dest.clone(), source.clone()))
        .collect();

    let target = detect_target(temp.path()).expect("temp dir is formattable");
    // `find_cargo_manifest` walks to the filesystem root uncapped, so a stray
    // Cargo.toml above the temp dir would silently make this a CargoProject and
    // test something else entirely.
    if !matches!(target, TargetKind::LooseDirectory { .. }) {
        corpus::skip("temp dir was detected as a Cargo project; set TMPDIR somewhere clean");
        return;
    }

    let base = FormatterOptions {
        quiet: true,
        toml_style: TomlStyle::default(),
        ..FormatterOptions::for_path(temp.path())
    };

    let checked = run_format(
        detect_target(temp.path()).unwrap(),
        &FormatterOptions {
            check: true,
            ..base.clone()
        },
    )
    .expect("check run");
    assert!(
        checked.file_errors.is_empty(),
        "unexpected file errors: {:?}",
        checked.file_errors
    );
    let reported: BTreeSet<PathBuf> = checked.mismatched_toml().map(Path::to_path_buf).collect();
    assert_eq!(checked.exit_code, i32::from(!reported.is_empty()));

    let before = read_tree(temp.path());
    let written = run_format(detect_target(temp.path()).unwrap(), &base).expect("write run");
    assert!(written.file_errors.is_empty());
    assert_eq!(written.exit_code, 0, "a write run must never exit 1");
    let after = read_tree(temp.path());

    let changed: BTreeSet<PathBuf> = before
        .iter()
        .filter(|(path, bytes)| after.get(*path).is_some_and(|new| new != *bytes))
        .map(|(path, _)| path.clone())
        .collect();

    let name = |path: &PathBuf| {
        origin
            .get(path)
            .map_or_else(|| path.display().to_string(), |o| o.display().to_string())
    };
    let stale: Vec<String> = reported.difference(&changed).map(name).collect();
    let silent: Vec<String> = changed.difference(&reported).map(name).collect();
    assert!(
        stale.is_empty() && silent.is_empty(),
        "--check disagrees with a write run\n\
         reported but unchanged ({}): {:?}\n\
         changed but unreported ({}): {:?}",
        stale.len(),
        &stale[..stale.len().min(10)],
        silent.len(),
        &silent[..silent.len().min(10)],
    );

    let recheck = run_format(
        detect_target(temp.path()).unwrap(),
        &FormatterOptions {
            check: true,
            ..base
        },
    )
    .expect("recheck run");
    assert!(
        recheck.mismatched_toml().next().is_none(),
        "{} files still need formatting after a write run",
        recheck.mismatched_toml().count()
    );
    assert_eq!(recheck.exit_code, 0);
}

fn read_tree(dir: &Path) -> HashMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let bytes = fs::read(entry.path()).ok()?;
            Some((entry.into_path(), bytes))
        })
        .collect()
}
