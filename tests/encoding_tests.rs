mod support;

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use rust_formatter::{FormatterOptions, TargetKind, TomlStyle, detect_target, run_format};
use support::golden;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/encoding")
}

fn expected_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/expected/encoding")
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(fixtures_dir())
        .expect("encoding fixtures directory")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter_map(|path| {
            let stem = path
                .file_name()?
                .to_str()?
                .strip_suffix(".toml.in")?
                .to_owned();
            Some((stem, fs::read(&path).ok()?))
        })
        .collect();
    out.sort();
    out
}

/// Writes `bytes` into a directory of its own and formats it, returning what
/// ended up on disk.
///
/// A write run rather than `format_toml`: the BOM and the line ending never
/// reach the formatter at all. `decode_source` strips them on the way in and
/// `encode_source` puts them back on the way out, so a `&str` API cannot
/// observe either one, and this is the only path that can.
fn format_bytes(bytes: &[u8], check_first: bool) -> Option<Vec<u8>> {
    let temp = tempfile::tempdir().expect("tempdir");
    let file = temp.path().join("fixture.toml");
    fs::write(&file, bytes).expect("write fixture");

    if !matches!(
        detect_target(temp.path()).expect("temp dir is formattable"),
        TargetKind::LooseDirectory { .. }
    ) {
        return None;
    }

    let options = FormatterOptions {
        quiet: true,
        toml_style: TomlStyle::default(),
        ..FormatterOptions::for_path(temp.path())
    };

    if check_first {
        let checked = run_format(
            detect_target(temp.path()).unwrap(),
            &FormatterOptions {
                check: true,
                ..options.clone()
            },
        )
        .expect("check run");
        assert!(
            checked.file_errors.is_empty(),
            "unexpected file errors: {:?}",
            checked.file_errors
        );
        assert_eq!(
            fs::read(&file).expect("read back"),
            bytes,
            "a --check run rewrote the file"
        );
    }

    run_format(detect_target(temp.path()).unwrap(), &options).expect("write run");
    Some(fs::read(&file).expect("read back"))
}

/// `.gitattributes` forces `eol=lf` on everything else in the tree, so these
/// fixtures are the one place a CRLF, a lone CR or a BOM can be committed. If
/// git ever normalises them the inputs stop testing anything, and this is the
/// assertion that notices.
#[test]
fn encoding_fixtures_survive_checkout() {
    let mut seen = BTreeSet::new();
    for (stem, bytes) in fixtures() {
        seen.insert(stem.clone());
        match stem.as_str() {
            "crlf"
            | "bom_crlf"
            | "crlf_no_final_newline"
            | "mixed_newlines"
            | "fmt_off_mixed_lf"
            | "fmt_off_mixed_crlf" => assert!(
                bytes.windows(2).any(|pair| pair == b"\r\n"),
                "{stem}: no CRLF survived checkout"
            ),
            "lone_cr" => {
                assert!(bytes.contains(&b'\r'), "{stem}: no CR survived checkout");
                assert!(
                    !bytes.windows(2).any(|pair| pair == b"\r\n"),
                    "{stem}: the lone CRs became CRLF"
                );
            }
            "no_final_newline" => {
                assert!(!bytes.ends_with(b"\n"), "{stem}: a final newline was added");
            }
            _ => {}
        }
        if stem.starts_with("bom") {
            assert!(
                bytes.starts_with(b"\xef\xbb\xbf"),
                "{stem}: the BOM was stripped"
            );
        }
    }
    for required in [
        "bom",
        "bom_crlf",
        "crlf",
        "crlf_no_final_newline",
        "fmt_off_mixed_crlf",
        "fmt_off_mixed_lf",
        "lone_cr",
        "mixed_newlines",
        "no_final_newline",
    ] {
        assert!(seen.contains(required), "missing fixture {required}");
    }
}

#[test]
fn encoding_fixtures_match_their_goldens() {
    let root = expected_dir();
    let mut produced = BTreeSet::new();
    let mut failures = Vec::new();

    for (stem, bytes) in fixtures() {
        let Some(written) = format_bytes(&bytes, true) else {
            eprintln!("SKIP: temp dir was detected as a Cargo project");
            return;
        };
        let expected = root.join(format!("{stem}.out"));
        if let Some(detail) = golden::check(&expected, &written) {
            failures.push(format!("{stem}\n{detail}"));
        }
        produced.insert(expected);
    }

    if !failures.is_empty() {
        for failure in &failures {
            eprintln!("\n{failure}");
        }
        panic!(
            "{} encoding goldens differ; rerun with UPDATE_EXPECT=1 to accept",
            failures.len()
        );
    }
    golden::assert_no_orphans(&root, &produced);
}

/// Idempotence at the byte level, which the `format_toml` suites cannot state:
/// a codec that re-added a BOM it had not stripped, or flipped the line ending,
/// would grow the file on every run while the text stayed a fixed point.
#[test]
fn encoding_is_a_byte_level_fixed_point() {
    for (stem, bytes) in fixtures() {
        let Some(once) = format_bytes(&bytes, false) else {
            eprintln!("SKIP: temp dir was detected as a Cargo project");
            return;
        };
        let twice = format_bytes(&once, false).expect("second pass");
        assert_eq!(once, twice, "{stem} is not a byte-level fixed point");
    }
}
