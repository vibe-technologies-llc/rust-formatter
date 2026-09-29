use assert_cmd::Command;
use rust_formatter::toml_1_0_issues;

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

/// Every style flag is exercised through the binary, because a knob that never
/// reaches the formatter would still pass every unit test.
fn styled(source: &str, args: &[&str]) -> String {
    let assert = formatter()
        .args(["--stdin", "--stdin-filepath", "style.toml"])
        .args(args)
        .write_stdin(source)
        .assert()
        .success();
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

const WIDE_TABLE: &str = "clap = { version = \"4.6.6\", features = [\"derive\", \"cargo\", \"env\", \"unicode\", \"wrap_help\", \"suggestions\"] }\n";

// ---------------------------------------------------------- inline tables

#[test]
fn a_table_that_fits_stays_on_one_line_by_default() {
    let source = "clap = { version = \"4.6.6\", features = [\"derive\"] }\n";
    assert_eq!(styled(source, &[]), source);
}

#[test]
fn a_table_past_the_budget_wraps_by_default() {
    assert!(styled(WIDE_TABLE, &[]).contains("clap = {\n    version"));
}

#[test]
fn compact_adds_no_toml_1_1_construct_of_its_own() {
    let out = styled(WIDE_TABLE, &["--toml-inline-tables", "compact"]);
    assert_eq!(out, WIDE_TABLE);
    assert_eq!(toml_1_0_issues(&out), []);
}

#[test]
fn compact_still_passes_a_1_1_spelling_through() {
    let source = "esc = \"\\x41\"\n";
    let out = styled(source, &["--toml-inline-tables", "compact"]);
    assert_eq!(out, source);
    assert_eq!(toml_1_0_issues(&out).len(), 1);
}

#[test]
fn expand_restores_the_two_key_rule() {
    let source = "clap = { version = \"4.6.6\", features = [\"derive\"] }\n";
    assert_eq!(
        styled(source, &["--toml-inline-tables", "expand"]),
        "clap = {\n    version = \"4.6.6\",\n    features = [\"derive\"]\n}\n"
    );
}

// ----------------------------------------------------------------- width

#[test]
fn the_width_budget_is_configurable() {
    let source = "features = [\"one\", \"two\"]\n";
    assert_eq!(styled(source, &["--toml-max-width", "100"]), source);
    assert_eq!(
        styled(source, &["--toml-max-width", "20"]),
        "features = [\n    \"one\",\n    \"two\"\n]\n"
    );
}

#[test]
fn a_zero_width_is_rejected_by_the_parser() {
    formatter()
        .args(["--toml-max-width", "0", "."])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--toml-max-width"));
}

// ---------------------------------------------------------------- indent

#[test]
fn the_indent_is_configurable() {
    let source = "features = [\"one\", \"two\"]\n";
    assert_eq!(
        styled(source, &["--toml-max-width", "20", "--toml-indent", "2"]),
        "features = [\n  \"one\",\n  \"two\"\n]\n"
    );
    assert_eq!(
        styled(source, &["--toml-max-width", "20", "--toml-indent", "tab"]),
        "features = [\n\t\"one\",\n\t\"two\"\n]\n"
    );
}

#[test]
fn an_unparseable_indent_is_rejected_by_the_parser() {
    formatter()
        .args(["--toml-indent", "wide", "."])
        .assert()
        .failure()
        .stderr(predicates::str::contains("`tab`"));
}

// ---------------------------------------------------------------- arrays

#[test]
fn a_deliberate_multiline_array_is_kept_by_default() {
    let source = "features = [\n    \"one\",\n    \"two\"\n]\n";
    assert_eq!(styled(source, &[]), source);
}

#[test]
fn auto_arrays_reflow_it() {
    assert_eq!(
        styled(
            "features = [\n    \"one\",\n    \"two\"\n]\n",
            &["--toml-arrays", "auto"]
        ),
        "features = [\"one\", \"two\"]\n"
    );
}

#[test]
fn the_trailing_comma_policy_is_configurable() {
    let source = "a = [\n    1,\n    2\n]\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--toml-trailing-comma", "multiline"]),
        "a = [\n    1,\n    2,\n]\n"
    );
}

// ----------------------------------------------------------- blank lines

#[test]
fn blank_lines_before_tables_are_only_inserted_on_request() {
    let source = "[a]\nx = 1\n[b]\ny = 2\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--toml-blank-line-before-tables"]),
        "[a]\nx = 1\n\n[b]\ny = 2\n"
    );
}

// ---------------------------------------------------------- toml version

/// The stderr of a `--stdin` run, which is where a finding that changes no
/// bytes has to appear.
fn styled_notes(source: &str, args: &[&str]) -> String {
    let assert = formatter()
        .args(["--stdin", "--stdin-filepath", "style.toml"])
        .args(args)
        .write_stdin(source)
        .assert()
        .success();
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

#[test]
fn targeting_toml_1_0_overrides_the_inline_table_style() {
    let out = styled(WIDE_TABLE, &["--toml-version", "1.0"]);
    assert_eq!(out, WIDE_TABLE);
    assert_eq!(toml_1_0_issues(&out), []);
}

#[test]
fn targeting_toml_1_0_reports_a_spelling_it_cannot_fix() {
    let source = "esc = \"\\e[0m\"\nwhen = 07:32\n";
    let notes = styled_notes(source, &["--toml-version", "1.0"]);
    assert!(
        notes.contains("style.toml:1:8: \\e escape is TOML 1.1 only"),
        "{notes}"
    );
    assert!(
        notes.contains("style.toml:2:8: a time without seconds is TOML 1.1 only"),
        "{notes}"
    );
    assert!(styled_notes(source, &[]).is_empty());
}

#[test]
fn a_toml_1_0_finding_leaves_the_exit_code_alone() {
    formatter()
        .args(["--stdin", "--stdin-filepath", "style.toml"])
        .args(["--check", "--toml-version", "1.0"])
        .write_stdin("esc = \"\\e[0m\"\n")
        .assert()
        .code(0);
}

#[test]
fn targeting_toml_1_0_conflicts_with_expanding_inline_tables() {
    formatter()
        .args(["--stdin", "--stdin-filepath", "style.toml"])
        .args(["--toml-version", "1.0", "--toml-inline-tables", "expand"])
        .write_stdin(WIDE_TABLE)
        .assert()
        .code(2);
}

// ------------------------------------------------------------------ keys

#[test]
fn a_key_path_always_loses_the_padding_around_its_dots() {
    assert_eq!(styled("a  .  b  .  c = 1\n", &[]), "a.b.c = 1\n");
    assert_eq!(styled("[  a  .  b  ]\nx = 1\n", &[]), "[a.b]\nx = 1\n");
    assert_eq!(styled("[[  c  ]]\nx = 1\n", &[]), "[[c]]\nx = 1\n");
}

#[test]
fn keys_are_only_unquoted_on_request() {
    let source = "\"quoted\" = 1\n'lit key' = 2\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--toml-normalize-keys"]),
        "quoted = 1\n'lit key' = 2\n"
    );
}

// --------------------------------------------------------------- sorting

#[test]
fn dependencies_are_only_sorted_on_request() {
    let source = "[dependencies]\nzzz = \"1\"\naaa = \"2\"\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--sort-deps"]),
        "[dependencies]\naaa = \"2\"\nzzz = \"1\"\n"
    );
}

#[test]
fn the_package_table_is_only_ordered_on_request() {
    let source = "[package]\nversion = \"0.1.0\"\nname = \"demo\"\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--sort-package"]),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n"
    );
}

#[test]
fn dependency_fields_are_only_ordered_on_request() {
    let source = "[dependencies.serde]\nfeatures = [\"derive\"]\nversion = \"1\"\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--sort-dep-fields"]),
        "[dependencies.serde]\nversion = \"1\"\nfeatures = [\"derive\"]\n"
    );
}

#[test]
fn features_are_only_ordered_on_request() {
    let source = "[features]\nzzz = []\naaa = []\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--sort-features"]),
        "[features]\naaa = []\nzzz = []\n"
    );
}

#[test]
fn arrays_are_only_ordered_on_request() {
    let source = "[package]\nkeywords = [\"z\", \"a\"]\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--sort-arrays"]),
        "[package]\nkeywords = [\"a\", \"z\"]\n"
    );
}

#[test]
fn target_sections_are_only_ordered_on_request() {
    let source = "[[bin]]\nname = \"zzz\"\n\n[[bin]]\nname = \"aaa\"\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--sort-targets"]),
        "[[bin]]\nname = \"aaa\"\n\n[[bin]]\nname = \"zzz\"\n"
    );
}

#[test]
fn the_document_sequence_is_only_ordered_on_request() {
    let source = "[dependencies]\na = \"1\"\n\n[package]\nname = \"n\"\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--sort-tables"]),
        "[package]\nname = \"n\"\n\n[dependencies]\na = \"1\"\n"
    );
}

#[test]
fn plain_keys_are_only_ordered_on_request() {
    let source = "[profile.release]\nzzz = 1\naaa = 2\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--sort-keys"]),
        "[profile.release]\naaa = 2\nzzz = 1\n"
    );
}

#[test]
fn grouping_is_only_honoured_on_request() {
    let source = "[dependencies]\nzzz = \"1\"\n\naaa = \"2\"\n";
    assert_eq!(
        styled(source, &["--sort-deps"]),
        "[dependencies]\naaa = \"2\"\n\nzzz = \"1\"\n"
    );
    assert_eq!(styled(source, &["--sort-deps", "--sort-grouped"]), source);
}

#[test]
fn grouping_refuses_the_cap_that_would_erase_its_own_input() {
    formatter()
        .args(["--sort-grouped", "--toml-max-blank-lines", "0", "--stdin"])
        .args(["--stdin-filepath", "style.toml"])
        .write_stdin("a = 1\n")
        .assert()
        .failure();
}

#[test]
fn the_package_order_is_selectable() {
    let source = "[package]\ndescription = \"d\"\nedition = \"2024\"\nname = \"n\"\n";
    assert_eq!(
        styled(source, &["--sort-package"]),
        "[package]\nname = \"n\"\nedition = \"2024\"\ndescription = \"d\"\n"
    );
    assert_eq!(
        styled(
            source,
            &["--sort-package", "--package-order", "style-guide"]
        ),
        "[package]\nname = \"n\"\nedition = \"2024\"\ndescription = \"d\"\n"
    );
    assert_eq!(
        styled(
            "[package]\ndescription = \"d\"\nzed = 1\nname = \"n\"\n",
            &["--sort-package", "--package-order", "style-guide"]
        ),
        "[package]\nname = \"n\"\nzed = 1\ndescription = \"d\"\n"
    );
}

#[test]
fn a_version_only_table_is_only_collapsed_on_request() {
    let source = "[dependencies]\nserde = { version = \"1.0\" }\n";
    assert_eq!(
        styled(source, &[]),
        "[dependencies]\nserde.version = \"1.0\"\n"
    );
    assert_eq!(
        styled(source, &["--cargo-conventions"]),
        "[dependencies]\nserde = \"1.0\"\n"
    );
}

// --------------------------------------------------------- the style guide

#[test]
fn the_style_guide_preset_breaks_out_an_over_wide_dependency() {
    let source = "[dependencies]\nshort = \"1\"\nexplicit = { version = \"2\" }\nextremely_long_crate_name_goes_here = { path = \"extremely_long_path_name_goes_right_here\", version = \"4.5.6\" }\n";
    assert_eq!(
        styled(source, &["--style-guide"]),
        "[dependencies]\nexplicit.version = \"2\"\nshort = \"1\"\n\n[dependencies.extremely_long_crate_name_goes_here]\npath = \"extremely_long_path_name_goes_right_here\"\nversion = \"4.5.6\"\n"
    );
}

#[test]
fn an_explicit_flag_beats_the_style_guide_preset() {
    let source =
        "k = [\"aaaaaaaaaaaaaaaaaaaa\", \"bbbbbbbbbbbbbbbbbbbb\", \"cccccccccccccccccccc\"]\n";
    assert_eq!(styled(source, &["--style-guide"]), source);
    assert!(
        styled(source, &["--style-guide", "--toml-max-width", "40"])
            .contains("k = [\n    \"aaaaaaaaaaaaaaaaaaaa\",")
    );
}

#[test]
fn the_style_guide_preset_is_a_fixed_point() {
    let source = "[dependencies]\nzzz = \"1\"\n# note\naaa = { features = [\"z\", \"a\"], version = \"2\" }\n[features]\nzed = [\"b\", \"a\"]\n[package]\nversion = \"0.1.0\"\n\"name\" = \"demo\"\ndescription = \"d\"\nkeywords = [\"z\", \"a\"]\n[[bin]]\nname = \"zzz\"\n[[bin]]\nname = \"aaa\"\n";
    let once = styled(source, &["--style-guide"]);
    assert_eq!(
        styled(&once, &["--style-guide"]),
        once,
        "not a fixed point:\n{once}"
    );
}

#[test]
fn every_style_flag_survives_a_second_pass() {
    let args = [
        "--sort-deps",
        "--sort-package",
        "--toml-normalize-keys",
        "--toml-blank-line-before-tables",
        "--toml-trailing-comma",
        "multiline",
        "--toml-arrays",
        "expand",
        "--toml-indent",
        "2",
        "--toml-tab-width",
        "8",
        "--toml-max-width",
        "60",
        "--toml-max-blank-lines",
        "2",
        "--toml-array-spacing",
        "spaced",
        "--toml-inline-table-spacing",
        "compact",
        "--toml-align-entries",
        "--toml-align-comments",
        "--toml-indent-tables",
        "--toml-indent-entries",
        "--toml-version",
        "1.1",
        "--toml-directives",
        "on",
        "--sort-dep-fields",
        "--sort-features",
        "--sort-arrays",
        "--sort-targets",
        "--sort-tables",
        "--sort-keys",
        "--sort-grouped",
        "--package-order",
        "style-guide",
        "--cargo-conventions",
    ];
    let source = "[package]\nversion = \"0.1.0\"\n\"name\" = \"demo\"\n[dependencies]\nzzz = \"1\" # pinned\n# note\naaa = { version = \"2\", features = [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\"] }\n[  dependencies  .  mmm  ]\nversion = \"3\"\n";

    let once = styled(source, &args);
    assert_eq!(styled(&once, &args), once, "not a fixed point:\n{once}");
}

// ------------------------------------------------------------ width and layout

#[test]
fn the_width_budget_is_measured_in_display_columns() {
    let source = "k = [\"日本語日本語日本語日本語\"]\n";
    assert_eq!(
        styled(source, &["--toml-max-width", "25"]),
        "k = [\n    \"日本語日本語日本語日本語\"\n]\n"
    );
    assert_eq!(styled(source, &["--toml-max-width", "32"]), source);
}

#[test]
fn the_tab_width_is_configurable() {
    let source = "k = [[1, 2, 3, 4]]\n";
    let args = ["--toml-indent", "tab", "--toml-max-width", "15"];
    assert_eq!(
        styled(source, &args),
        "k = [\n\t[\n\t\t1,\n\t\t2,\n\t\t3,\n\t\t4\n\t]\n]\n"
    );
    assert_eq!(
        styled(
            source,
            &[args.as_slice(), &["--toml-tab-width", "1"]].concat()
        ),
        "k = [\n\t[1, 2, 3, 4]\n]\n"
    );
}

#[test]
fn a_tab_indent_leaves_the_closing_bracket_flush() {
    assert_eq!(
        styled(
            "k = [\n  1\n  # why\n]\n",
            &["--toml-indent", "tab", "--toml-max-width", "5"]
        ),
        "k = [\n\t1\n\t# why\n]\n"
    );
}

#[test]
fn expand_arrays_are_selectable() {
    assert_eq!(
        styled("a = [1, 2]\nb = [1]\n", &["--toml-arrays", "expand"]),
        "a = [\n    1,\n    2\n]\nb = [1]\n"
    );
}

#[test]
fn the_blank_line_maximum_is_selectable() {
    let source = "a = 1\n\n\n\nb = 2\n";
    assert_eq!(styled(source, &[]), "a = 1\n\nb = 2\n");
    assert_eq!(
        styled(source, &["--toml-max-blank-lines", "2"]),
        "a = 1\n\n\nb = 2\n"
    );
    assert_eq!(
        styled(source, &["--toml-max-blank-lines", "0"]),
        "a = 1\nb = 2\n"
    );
}

#[test]
fn container_spacing_is_selectable() {
    let source = "a = [1, 2]\nb = { x = 1, y = 2 }\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(
            source,
            &[
                "--toml-array-spacing",
                "spaced",
                "--toml-inline-table-spacing",
                "compact"
            ]
        ),
        "a = [ 1, 2 ]\nb = {x = 1, y = 2}\n"
    );
}

#[test]
fn alignment_is_selectable() {
    let source = "a = 1 # one\nbbb = 22 # two\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--toml-align-entries"]),
        "a   = 1 # one\nbbb = 22 # two\n"
    );
    assert_eq!(
        styled(source, &["--toml-align-comments"]),
        "a = 1    # one\nbbb = 22 # two\n"
    );
    assert_eq!(
        styled(source, &["--toml-align-entries", "--toml-align-comments"]),
        "a   = 1  # one\nbbb = 22 # two\n"
    );
}

#[test]
fn table_indentation_is_selectable() {
    let source = "[t]\nk = 1\n\n[t.u]\nm = 2\n";
    assert_eq!(styled(source, &[]), source);
    assert_eq!(
        styled(source, &["--toml-indent-entries"]),
        "[t]\n    k = 1\n\n[t.u]\n    m = 2\n"
    );
    assert_eq!(
        styled(source, &["--toml-indent-tables"]),
        "[t]\nk = 1\n\n    [t.u]\n    m = 2\n"
    );
    assert_eq!(
        styled(source, &["--toml-indent-tables", "--toml-indent-entries"]),
        "[t]\n    k = 1\n\n    [t.u]\n        m = 2\n"
    );
}

#[test]
fn an_out_of_range_tab_width_is_rejected_by_the_parser() {
    formatter()
        .args(["--stdin", "--stdin-filepath", "style.toml"])
        .args(["--toml-tab-width", "0"])
        .write_stdin("a = 1\n")
        .assert()
        .failure()
        .stderr(predicates::str::contains("--toml-tab-width"));
}

/// `expand_over_lines` used to overwrite the value's decor prefix, which is the
/// only slot a comment between `=` and its value can live in — and a comment
/// there is precisely what forces the expand branch in the first place.
#[test]
fn a_comment_between_the_equals_and_the_value_survives() {
    let out = styled("a = { b = # why\n1 }\n", &[]);
    assert_eq!(out, "a = {\n    # why\n    b = 1\n}\n");

    let nested = styled("x = [{ p = # inner\n2 }]\n", &[]);
    assert!(nested.contains("# inner"), "{nested}");

    let later = styled("y = { m = 1, n = # second\n2 }\n", &[]);
    assert_eq!(later, "y = {\n    m = 1,\n    # second\n    n = 2\n}\n");

    // The hoisted comment must not displace one already above the key, nor
    // introduce a blank line between the two.
    let stacked = styled("z = {\n    # above\n    k = # after\n    3,\n}\n", &[]);
    assert_eq!(stacked, "z = {\n    # above\n    # after\n    k = 3\n}\n");
}

// ------------------------------------------------------------------ directives

const HAND_ALIGNED: &str = "\
[package]
name = \"demo\"
# fmt: off
matrix = [
  1,   2,   3,
  40,  50,  60,
]
#   a   hand   aligned   note
# fmt: on
version = \"1\"
";

#[test]
fn a_directive_region_survives_the_style_guide() {
    let once = styled(HAND_ALIGNED, &["--style-guide"]);
    assert!(once.contains("  40,  50,  60,\n"), "{once}");
    assert!(once.contains("#   a   hand   aligned   note\n"), "{once}");
    assert_eq!(
        styled(&once, &["--style-guide"]),
        once,
        "not a fixed point:\n{once}"
    );
}

#[test]
fn directives_can_be_turned_off() {
    let once = styled(HAND_ALIGNED, &["--style-guide", "--toml-directives", "off"]);
    assert!(once.contains("matrix = [1, 2, 3, 40, 50, 60]\n"), "{once}");
    assert!(once.contains("# a   hand   aligned   note\n"), "{once}");
}

#[test]
fn every_directive_spelling_is_honoured() {
    for (off, on) in [
        ("# fmt: off", "# fmt: on"),
        ("# taplo: fmt-off", "# taplo: fmt-on"),
        ("# rust-formatter: fmt-off", "# rust-formatter: fmt-on"),
    ] {
        let source = format!("{off}\na   =   1\n{on}\nb   =   2\n");
        assert_eq!(
            styled(&source, &[]),
            format!("{off}\na   =   1\n{on}\nb = 2\n"),
            "{off}"
        );
    }
}

/// A marker counts wherever it is written, so that a comment the formatter has
/// to move cannot change what the document asked for.
#[test]
fn a_marker_sharing_a_line_with_a_value_still_opens_a_region() {
    assert_eq!(
        styled("a   =   1 # fmt: off\nb   =   2\n", &[]),
        "a   =   1 # fmt: off\nb   =   2\n"
    );
    assert_eq!(
        styled("a   =   1 # fmt: off is only a comment\nb   =   2\n", &[]),
        "a = 1 # fmt: off is only a comment\nb = 2\n"
    );
}

#[test]
fn a_frozen_region_keeps_its_own_line_endings_in_an_lf_file() {
    assert_eq!(
        styled("a=1\n# fmt: off\nb   =  2\r\n# fmt: on\nc=3\n", &[]),
        "a = 1\n# fmt: off\nb   =  2\r\n# fmt: on\nc = 3\n"
    );
}

#[test]
fn a_frozen_region_keeps_its_own_line_endings_in_a_crlf_file() {
    assert_eq!(
        styled("a=1\r\n# fmt: off\r\nb   =  2\n# fmt: on\r\nc=3\r\n", &[]),
        "a = 1\r\n# fmt: off\r\nb   =  2\n# fmt: on\r\nc = 3\r\n"
    );
}

#[test]
fn a_frozen_region_is_reported_as_clean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("frozen.toml");
    std::fs::write(&path, "# fmt: off\na   =   1\n# fmt: on\n").unwrap();

    formatter()
        .args(["--check", "--toml-align-entries"])
        .arg(&path)
        .assert()
        .success();
}
