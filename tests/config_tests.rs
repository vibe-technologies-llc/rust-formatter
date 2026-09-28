#[path = "support/toolchain.rs"]
mod toolchain;

use std::{fs, path::Path};

use assert_cmd::Command;
use tempfile::{TempDir, tempdir};
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

/// A workspace with one member, both able to carry a metadata table. Every
/// configuration test runs against a real manifest, because the metadata tables
/// are the source the walk has to find on its own.
fn workspace(root_metadata: &str, member_metadata: &str) -> TempDir {
    let temp = tempdir().unwrap();
    let member = temp.path().join("member");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        format!("[workspace]\nmembers = [\"member\"]\n{root_metadata}"),
    )
    .unwrap();
    fs::write(
        member.join("Cargo.toml"),
        format!(
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             {member_metadata}\n[dependencies]\nzzz = \"1\"\naaa = \"1\"\n"
        ),
    )
    .unwrap();
    fs::write(member.join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();
    temp
}

/// The settings a run resolves to, as the lines `--print-settings` prints
/// without its provenance comments.
fn settings(dir: &Path, args: &[&str]) -> String {
    let assert = formatter()
        .current_dir(dir)
        .args(["--print-settings", "--toml-only", "."])
        .args(args)
        .assert()
        .success();
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn formatted(dir: &Path, file: &str, args: &[&str]) -> String {
    let assert = formatter()
        .current_dir(dir)
        .args(["--toml-only", "--emit", "stdout", file])
        .args(args)
        .assert()
        .success();
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn value(rendered: &str, key: &str) -> Option<String> {
    rendered
        .lines()
        .find(|line| line.starts_with(&format!("{key} = ")))
        .map(|line| line[key.len() + 3..].to_string())
}

#[test]
fn a_workspace_metadata_table_reaches_every_member() {
    let temp = workspace(
        "\n[workspace.metadata.rust-formatter]\nsort-deps = true\n",
        "",
    );
    let rendered = settings(&temp.path().join("member"), &[]);
    assert_eq!(value(&rendered, "sort-deps").as_deref(), Some("true"));

    let out = formatted(&temp.path().join("member"), "Cargo.toml", &[]);
    let deps: Vec<&str> = out
        .lines()
        .skip_while(|line| !line.starts_with("[dependencies]"))
        .skip(1)
        .take(2)
        .collect();
    assert_eq!(deps, vec!["aaa = \"1\"", "zzz = \"1\""]);
}

#[test]
fn a_package_table_layers_over_the_workspace_one() {
    let temp = workspace(
        "\n[workspace.metadata.rust-formatter]\ntoml-max-width = 80\nexclude = [\"vendor/**\"]\n",
        "\n[package.metadata.rust-formatter]\ntoml-max-width = 120\nexclude = [\"generated/**\"]\n",
    );
    let rendered = settings(&temp.path().join("member"), &[]);

    assert_eq!(value(&rendered, "toml-max-width").as_deref(), Some("120"));
    assert_eq!(
        value(&rendered, "exclude").as_deref(),
        Some(r#"["vendor/**", "generated/**"]"#)
    );
}

#[test]
fn a_discovered_file_outranks_the_manifest_and_a_named_one_outranks_it() {
    let temp = workspace(
        "",
        "\n[package.metadata.rust-formatter]\ntoml-max-width = 80\n",
    );
    let member = temp.path().join("member");
    fs::write(member.join("rust-formatter.toml"), "toml-max-width = 70\n").unwrap();
    assert_eq!(
        value(&settings(&member, &[]), "toml-max-width").as_deref(),
        Some("70")
    );

    fs::write(member.join("named.toml"), "toml-max-width = 60\n").unwrap();
    assert_eq!(
        value(
            &settings(&member, &["--config-file", "named.toml"]),
            "toml-max-width"
        )
        .as_deref(),
        Some("60")
    );
}

#[test]
fn a_dotted_file_name_is_found_too() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(member.join(".rust-formatter.toml"), "toml-max-width = 70\n").unwrap();
    assert_eq!(
        value(&settings(&member, &[]), "toml-max-width").as_deref(),
        Some("70")
    );
}

#[test]
fn no_config_ignores_every_file_source() {
    let temp = workspace(
        "\n[workspace.metadata.rust-formatter]\nsort-deps = true\n",
        "",
    );
    let member = temp.path().join("member");
    fs::write(member.join("rust-formatter.toml"), "toml-max-width = 70\n").unwrap();

    let rendered = settings(&member, &["--no-config"]);
    assert_eq!(value(&rendered, "sort-deps"), None);
    assert_eq!(value(&rendered, "toml-max-width"), None);
}

#[test]
fn a_named_config_file_cannot_be_combined_with_no_config() {
    formatter()
        .args(["--config-file", "x.toml", "--no-config", "."])
        .assert()
        .failure();
}

#[test]
fn the_environment_outranks_a_file_and_loses_to_a_flag() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(member.join("rust-formatter.toml"), "toml-max-width = 70\n").unwrap();

    let assert = formatter()
        .current_dir(&member)
        .env("RUST_FORMATTER_TOML_MAX_WIDTH", "55")
        .args(["--print-settings", "--toml-only", "."])
        .assert()
        .success();
    let rendered = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(value(&rendered, "toml-max-width").as_deref(), Some("55"));
    assert!(
        rendered.contains("# from: $RUST_FORMATTER_TOML_MAX_WIDTH"),
        "{rendered}"
    );

    let assert = formatter()
        .current_dir(&member)
        .env("RUST_FORMATTER_TOML_MAX_WIDTH", "55")
        .args([
            "--print-settings",
            "--toml-only",
            "--toml-max-width",
            "44",
            ".",
        ])
        .assert()
        .success();
    let rendered = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(value(&rendered, "toml-max-width").as_deref(), Some("44"));
}

/// Once a repository can turn a rewrite on, one run has to be able to turn it
/// off again -- in both spellings.
#[test]
fn a_flag_can_switch_off_what_a_file_switched_on() {
    let temp = workspace(
        "\n[workspace.metadata.rust-formatter]\nsort-deps = true\n",
        "",
    );
    let member = temp.path().join("member");

    for args in [
        vec!["--sort-deps=false"],
        vec!["--no-sort-deps"],
        vec!["--sort-deps=0"],
        vec!["--sort-deps", "--no-sort-deps"],
    ] {
        let out = formatted(&member, "Cargo.toml", &args);
        assert!(
            out.contains("zzz = \"1\"\naaa = \"1\""),
            "{args:?} did not switch sorting off:\n{out}"
        );
    }

    // The last spelling on the line wins, so a wrapper may append either.
    let out = formatted(&member, "Cargo.toml", &["--no-sort-deps", "--sort-deps"]);
    assert!(out.contains("aaa = \"1\"\nzzz = \"1\""), "{out}");
}

#[test]
fn an_environment_variable_can_switch_off_what_a_file_switched_on() {
    let temp = workspace(
        "\n[workspace.metadata.rust-formatter]\nsort-deps = true\n",
        "",
    );
    let assert = formatter()
        .current_dir(temp.path().join("member"))
        .env("RUST_FORMATTER_SORT_DEPS", "off")
        .args(["--toml-only", "--emit", "stdout", "Cargo.toml"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("zzz = \"1\"\naaa = \"1\""), "{out}");
}

#[test]
fn a_preset_fills_in_under_the_layer_that_named_it() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(
        member.join("rust-formatter.toml"),
        "preset = [\"cargo\"]\ntoml-max-width = 70\n",
    )
    .unwrap();

    let rendered = settings(&member, &[]);
    assert_eq!(value(&rendered, "toml-max-width").as_deref(), Some("70"));
    assert_eq!(value(&rendered, "sort-keys").as_deref(), Some("true"));
    assert!(rendered.contains("# from: preset cargo"), "{rendered}");
}

#[test]
fn style_guide_and_the_cargo_preset_are_the_same_settings() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    let strip = |text: String| {
        text.lines()
            .filter(|line| !line.starts_with('#') && !line.starts_with("preset = "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        strip(settings(&member, &["--style-guide"])),
        strip(settings(&member, &["--preset", "cargo"]))
    );
}

#[test]
fn a_file_may_define_a_preset_of_its_own() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(
        member.join("rust-formatter.toml"),
        "[presets.house]\nsort-keys = true\ntoml-align-entries = true\n",
    )
    .unwrap();

    let rendered = settings(&member, &["--preset", "house"]);
    assert_eq!(value(&rendered, "sort-keys").as_deref(), Some("true"));
    assert_eq!(
        value(&rendered, "toml-align-entries").as_deref(),
        Some("true")
    );
}

#[test]
fn presets_layer_in_the_order_they_are_named() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    let rendered = settings(&member, &["--preset", "narrow-tabs", "--preset", "cargo"]);
    assert_eq!(value(&rendered, "toml-max-width").as_deref(), Some("100"));

    let rendered = settings(&member, &["--preset", "cargo", "--preset", "narrow-tabs"]);
    assert_eq!(value(&rendered, "toml-max-width").as_deref(), Some("60"));
}

#[test]
fn a_misspelled_setting_names_the_one_it_meant_and_exits_two() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(member.join("rust-formatter.toml"), "sort_deps = true\n").unwrap();

    let assert = formatter()
        .current_dir(&member)
        .args(["--toml-only", "."])
        .assert()
        .code(2);
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("did you mean `sort-deps`"), "{err}");
    assert!(err.contains("rust-formatter.toml"), "{err}");
}

#[test]
fn a_misspelled_setting_in_a_manifest_names_its_table() {
    let temp = workspace(
        "\n[workspace.metadata.rust-formatter]\ntoml-maxwidth = 90\n",
        "",
    );
    let assert = formatter()
        .current_dir(temp.path().join("member"))
        .args(["--toml-only", "."])
        .assert()
        .code(2);
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("[workspace.metadata.rust-formatter]"), "{err}");
    assert!(err.contains("did you mean `toml-max-width`"), "{err}");
}

#[test]
fn an_unknown_preset_exits_two_and_lists_the_ones_there_are() {
    let temp = workspace("", "");
    let assert = formatter()
        .current_dir(temp.path().join("member"))
        .args(["--toml-only", "--preset", "nope", "."])
        .assert()
        .code(2);
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("unknown preset `nope`"), "{err}");
    assert!(err.contains("aligned-indented"), "{err}");
}

#[test]
fn a_flag_combination_a_file_asked_for_is_refused_the_same_way_a_typed_one_is() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(
        member.join("rust-formatter.toml"),
        "sort-grouped = true\ntoml-max-blank-lines = 0\n",
    )
    .unwrap();

    let assert = formatter()
        .current_dir(&member)
        .args(["--toml-only", "."])
        .assert()
        .code(2);
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("--sort-grouped"), "{err}");
}

/// The settings dump has to work on a configuration that would not run, because
/// that is exactly when it is needed.
#[test]
fn the_settings_dump_still_prints_a_configuration_that_would_be_refused() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(
        member.join("rust-formatter.toml"),
        "sort-grouped = true\ntoml-max-blank-lines = 0\n",
    )
    .unwrap();

    let rendered = settings(&member, &[]);
    assert_eq!(value(&rendered, "sort-grouped").as_deref(), Some("true"));
}

#[test]
fn the_settings_dump_has_a_machine_readable_form() {
    let temp = workspace(
        "\n[workspace.metadata.rust-formatter]\ntoml-max-width = 80\n",
        "",
    );
    let assert = formatter()
        .current_dir(temp.path().join("member"))
        .args([
            "--print-settings",
            "--message-format",
            "json",
            "--toml-only",
            ".",
        ])
        .assert()
        .success();
    let text = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(parsed["settings"]["toml-max-width"], 80);
    assert!(
        parsed["sources"]["toml-max-width"]
            .as_str()
            .unwrap()
            .contains("workspace.metadata"),
        "{text}"
    );
}

/// A repository's own excludes describe its tree, not this run, so they must not
/// turn "there is nothing formattable here" into a silent success -- which is
/// what a typed `--exclude` does mean.
#[test]
fn a_configured_exclude_does_not_make_an_empty_run_a_no_op() {
    needs_nightly!();
    let temp = tempdir().unwrap();
    let empty = temp.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    fs::write(
        temp.path().join("rust-formatter.toml"),
        "exclude = [\"nothing/**\"]\n",
    )
    .unwrap();
    fs::write(empty.join("skip-me.txt"), "not formattable\n").unwrap();

    formatter().current_dir(&empty).arg(".").assert().code(2);

    formatter()
        .current_dir(&empty)
        .args(["--exclude", "nothing/**", "."])
        .assert()
        .success();
}

/// The rustfmt options a repository names reach rustfmt, and the ones it
/// misspells are dropped with a warning rather than failing everyone's run.
#[test]
fn a_rustfmt_option_from_a_file_reaches_the_command_line() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(
        member.join("rust-formatter.toml"),
        "[config]\nmax_width = 120\n",
    )
    .unwrap();

    let rendered = settings(&member, &[]);
    assert!(rendered.contains("max_width = 120"), "{rendered}");
}

#[test]
fn a_setting_reaches_the_formatter_through_every_source() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    let unsorted = "zzz = \"1\"\naaa = \"1\"";
    let sorted = "aaa = \"1\"\nzzz = \"1\"";

    assert!(formatted(&member, "Cargo.toml", &[]).contains(unsorted));

    fs::write(member.join("rust-formatter.toml"), "sort-deps = true\n").unwrap();
    assert!(formatted(&member, "Cargo.toml", &[]).contains(sorted));

    fs::remove_file(member.join("rust-formatter.toml")).unwrap();
    assert!(formatted(&member, "Cargo.toml", &["--preset", "cargo"]).contains(sorted));
}

/// A rustfmt option a repository names is read by everyone who checks it out, on
/// whatever rustfmt they have, so an option this one lacks is dropped with a
/// warning. The same option typed on the command line is still a typo, and fatal.
#[test]
fn an_unknown_rustfmt_option_is_fatal_only_when_it_was_typed() {
    needs_nightly!();
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(
        member.join("rust-formatter.toml"),
        "[config]\nno_such_rustfmt_option = true\n",
    )
    .unwrap();

    let assert = formatter()
        .current_dir(&member)
        .args(["--check", "."])
        .assert()
        .success();
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("no_such_rustfmt_option"), "{err}");
    assert!(err.contains("formatting without it"), "{err}");

    fs::remove_file(member.join("rust-formatter.toml")).unwrap();
    formatter()
        .current_dir(&member)
        .args(["--check", "--config", "no_such_rustfmt_option=true", "."])
        .assert()
        .code(2);
}

/// An edition a configuration source names describes a tree rather than a run,
/// so it ranks below each package's own `edition` and below the project's
/// `rustfmt.toml` -- where a typed `--edition` outranks both.
#[test]
fn a_configured_edition_ranks_below_the_manifest_and_the_project_config() {
    needs_nightly!();
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(member.join("rust-formatter.toml"), "edition = \"2015\"\n").unwrap();

    let assert = formatter()
        .current_dir(&member)
        .args(["-v", "--check", "."])
        .assert();
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("edition: 2015"), "{err}");

    // `package.edition` is more specific, so the file only replaces the built-in
    // default: a `.rs` file that needs 2021 still formats.
    fs::write(
        member.join("src").join("lib.rs"),
        "pub async fn f() {\n    let dyn_ = 1;\n    let _ = dyn_;\n}\n",
    )
    .unwrap();
    formatter().current_dir(&member).arg(".").assert().success();

    let assert = formatter()
        .current_dir(&member)
        .args(["-v", "--check", "--edition", "2021", "."])
        .assert();
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("edition: 2021"), "{err}");
}

// ------------------------------------------------------------ toolchain pins

/// rustup and cargo both read `rust-toolchain.toml`, so a repository that
/// carries one has already said which toolchain its tooling should use.
#[test]
fn a_rust_toolchain_file_sets_the_toolchain() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(
        temp.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly-2026-06-01\"\ncomponents = [\"rustfmt\"]\n",
    )
    .unwrap();

    let rendered = settings(&member, &[]);
    assert_eq!(
        value(&rendered, "toolchain").as_deref(),
        Some("\"nightly-2026-06-01\"")
    );
    assert!(rendered.contains("rust-toolchain.toml"), "{rendered}");
}

/// The legacy spelling is a bare channel name and is still what many
/// repositories carry.
#[test]
fn a_legacy_rust_toolchain_file_is_read_too() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(temp.path().join("rust-toolchain"), "stable\n").unwrap();
    assert_eq!(
        value(&settings(&member, &[]), "toolchain").as_deref(),
        Some("\"stable\"")
    );
}

/// The pin is the lowest layer of the chain: it outranks the built-in default
/// and loses to every source this tool owns.
#[test]
fn every_configuration_source_outranks_the_toolchain_file() {
    let temp = workspace(
        "",
        "\n[package.metadata.rust-formatter]\ntoolchain = \"beta\"\n",
    );
    let member = temp.path().join("member");
    fs::write(
        temp.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    assert_eq!(
        value(&settings(&member, &[]), "toolchain").as_deref(),
        Some("\"beta\"")
    );

    fs::write(
        member.join("rust-formatter.toml"),
        "toolchain = \"nightly\"\n",
    )
    .unwrap();
    assert_eq!(
        value(&settings(&member, &[]), "toolchain").as_deref(),
        Some("\"nightly\"")
    );

    assert_eq!(
        value(&settings(&member, &["--toolchain", "auto"]), "toolchain").as_deref(),
        Some("\"auto\"")
    );
}

/// `--no-config` names the file sources, and the pin is one of them.
#[test]
fn no_config_ignores_the_toolchain_file() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    fs::write(
        temp.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    // Nothing set it, so nothing is rendered: the built-in default applies.
    assert_eq!(
        value(&settings(&member, &["--no-config"]), "toolchain"),
        None
    );
}

/// A file rustup would report on itself is not a second failure here.
#[test]
fn a_toolchain_file_with_no_channel_is_ignored() {
    let temp = workspace("", "");
    let member = temp.path().join("member");
    for body in ["[toolchain]\ncomponents = [\"rustfmt\"]\n", "= = =\n", "\n"] {
        fs::write(temp.path().join("rust-toolchain.toml"), body).unwrap();
        assert_eq!(value(&settings(&member, &[]), "toolchain"), None, "{body}");
    }
}
