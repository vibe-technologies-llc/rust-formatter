use std::{fs, path::Path};

use rust_formatter::{FormatterOptions, Streams, detect_target, format, run, run_format};

/// The documented contract (`README.md`): 0 clean, 1 `--check` found work, 2
/// tool/environment/parse/I/O error.
fn run_capture(options: &FormatterOptions) -> (i32, String) {
    let (code, _, err) = run_streams(options);
    (code, err)
}

/// stdout is the product (diffs, lists, JSON); stderr is the commentary.
fn run_streams(options: &FormatterOptions) -> (i32, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = {
        let streams = Streams::plain(&mut out, &mut err);
        run(options, &streams)
    };
    (
        code,
        String::from_utf8(out).expect("stdout is UTF-8"),
        String::from_utf8(err).expect("stderr is UTF-8"),
    )
}

fn options(target: &Path) -> FormatterOptions {
    FormatterOptions::for_path(target)
}

fn toml_dir(contents: &[(&str, &str)]) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    for (name, body) in contents {
        fs::write(temp.path().join(name), body).unwrap();
    }
    temp
}

/// A run that rewrote nothing must not claim it formatted anything.
#[test]
fn clean_tree_exits_zero() {
    let temp = toml_dir(&[("a.toml", "foo.path = \"x\"\n")]);
    let (code, err) = run_capture(&options(temp.path()));

    assert_eq!(code, 0);
    assert!(err.contains("Already formatted:"), "{err}");
}

#[test]
fn a_write_run_counts_only_what_it_wrote() {
    let temp = toml_dir(&[
        ("a.toml", "foo={path=\"x\"}\n"),
        ("b.toml", "foo.path = \"x\"\n"),
    ]);
    let (code, _, err) = run_streams(&options(temp.path()));

    assert_eq!(code, 0);
    assert!(err.contains("Formatted 1 file"), "{err}");
}

/// The diff is the product and belongs on stdout; the verdict is commentary.
#[test]
fn check_with_differences_exits_one() {
    let temp = toml_dir(&[("a.toml", "foo={path=\"x\"}\n")]);
    let (code, out, err) = run_streams(&FormatterOptions {
        check: true,
        ..options(temp.path())
    });

    assert_eq!(code, 1);
    assert!(err.contains("needs formatting"), "{err}");
    assert!(!err.contains("a.toml"), "{err}");
    assert!(out.contains("a.toml"), "{out}");
    assert!(out.contains("-foo={path=\"x\"}"), "{out}");
    assert!(out.contains("+foo.path = \"x\""), "{out}");
}

#[test]
fn check_on_a_clean_tree_exits_zero() {
    let temp = toml_dir(&[("a.toml", "foo.path = \"x\"\n")]);
    let (code, err) = run_capture(&FormatterOptions {
        check: true,
        ..options(temp.path())
    });

    assert_eq!(code, 0);
    assert!(!err.contains("needs formatting"), "{err}");
}

#[test]
fn a_write_run_never_exits_one() {
    let temp = toml_dir(&[("a.toml", "foo={path=\"x\"}\n")]);
    let (code, _) = run_capture(&options(temp.path()));

    assert_eq!(code, 0);
    assert_eq!(
        fs::read_to_string(temp.path().join("a.toml")).unwrap(),
        "foo.path = \"x\"\n"
    );
}

#[test]
fn unparseable_toml_exits_two_even_in_check_mode() {
    let temp = toml_dir(&[("bad.toml", "this is not = = toml\n")]);

    for check in [false, true] {
        let (code, err) = run_capture(&FormatterOptions {
            check,
            ..options(temp.path())
        });
        assert_eq!(code, 2, "check={check}: {err}");
        assert!(err.contains("bad.toml"), "{err}");
    }
}

/// A parse error in one file must outrank a `--check` mismatch in another:
/// exit 1 would tell CI "reformat me", when the real answer is "this is broken".
#[test]
fn a_file_error_outranks_a_check_mismatch() {
    let temp = toml_dir(&[
        ("bad.toml", "this is not = = toml\n"),
        ("dirty.toml", "foo={path=\"x\"}\n"),
    ]);

    let (code, _) = run_capture(&FormatterOptions {
        check: true,
        ..options(temp.path())
    });
    assert_eq!(code, 2);
}

#[test]
fn missing_path_exits_two() {
    let temp = tempfile::tempdir().unwrap();
    let (code, err) = run_capture(&options(&temp.path().join("nope")));

    assert_eq!(code, 2);
    assert!(err.contains("does not exist"), "{err}");
}

#[test]
fn unsupported_target_exits_two() {
    let temp = tempfile::tempdir().unwrap();
    let readme = temp.path().join("README.md");
    fs::write(&readme, "hi\n").unwrap();

    let (code, err) = run_capture(&options(&readme));
    assert_eq!(code, 2);
    assert!(err.contains("Unsupported target"), "{err}");
}

#[test]
fn an_empty_directory_exits_two() {
    let temp = tempfile::tempdir().unwrap();
    let (code, err) = run_capture(&options(temp.path()));

    assert_eq!(code, 2);
    assert!(err.contains("No formattable"), "{err}");
}

#[test]
fn quiet_silences_stderr_without_changing_the_code() {
    let temp = toml_dir(&[("a.toml", "foo={path=\"x\"}\n")]);
    let (code, err) = run_capture(&FormatterOptions {
        check: true,
        quiet: true,
        ..options(temp.path())
    });

    assert_eq!(code, 1);
    assert!(err.is_empty(), "{err}");
}

#[test]
fn verbose_precedes_the_summary() {
    let temp = toml_dir(&[("a.toml", "foo.path = \"x\"\n")]);
    let (code, err) = run_capture(&FormatterOptions {
        verbose: true,
        ..options(temp.path())
    });

    assert_eq!(code, 0);
    let target = err.find("target:").expect("verbose preamble");
    let summary = err.find("Already formatted:").expect("summary");
    assert!(target < summary, "preamble must come first:\n{err}");
}

#[test]
fn process_exit_code_matches_what_run_returns() {
    for (body, check) in [
        ("foo.path = \"x\"\n", false),
        ("foo.path = \"x\"\n", true),
        ("foo={path=\"x\"}\n", true),
        ("not = = toml\n", true),
    ] {
        let temp = toml_dir(&[("a.toml", body)]);
        let options = FormatterOptions {
            check,
            ..options(temp.path())
        };

        let result = run_format(detect_target(&options.targets[0]).unwrap(), &options).unwrap();
        let expected = result.process_exit_code();

        let temp = toml_dir(&[("a.toml", body)]);
        let (code, _) = run_capture(&FormatterOptions {
            check,
            ..options.clone()
        });
        let _ = temp;

        assert_eq!(code, expected, "body={body:?} check={check}");
    }
}

/// `lib::format` is the library entry point and `run` is the binary's. They must
/// not drift: same input, same bytes on disk, same exit code.
#[test]
fn the_library_entry_point_agrees_with_the_binary_path() {
    let bodies: &[(&str, &str)] = &[
        ("a.toml", "foo={path=\"x\"}\n"),
        ("b.toml", "[t]\nx = { y = 1 }\n"),
        ("c.toml", "already.formatted = true\n"),
    ];

    let via_format = toml_dir(bodies);
    let via_run = toml_dir(bodies);

    let result = format(&options(via_format.path())).expect("format");
    let (code, _) = run_capture(&options(via_run.path()));

    assert_eq!(result.process_exit_code(), code);
    for (name, _) in bodies {
        assert_eq!(
            fs::read_to_string(via_format.path().join(name)).unwrap(),
            fs::read_to_string(via_run.path().join(name)).unwrap(),
            "{name} differs between format() and run()"
        );
    }
}

#[test]
fn format_reports_the_detected_target_and_mismatches() {
    let temp = toml_dir(&[("a.toml", "foo={path=\"x\"}\n")]);
    let result = format(&FormatterOptions {
        check: true,
        ..options(temp.path())
    })
    .expect("format");

    assert_eq!(result.exit_code, 1);
    assert_eq!(result.mismatched_toml().count(), 1);
    assert!(result.file_errors.is_empty());
    assert!(
        result.mismatched_toml().next().unwrap().ends_with("a.toml"),
        "{:?}",
        result.mismatched_toml().collect::<Vec<_>>()
    );
}

#[test]
fn format_surfaces_detection_errors() {
    let temp = tempfile::tempdir().unwrap();
    let err = format(&options(&temp.path().join("missing"))).unwrap_err();

    assert!(err.to_string().contains("does not exist"), "{err}");
}
