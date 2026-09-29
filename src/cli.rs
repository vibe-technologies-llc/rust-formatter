//! The command line: parsing, the settings overlay, and the subcommands that
//! only exist to be invoked by something else.
//!
//! Everything here is private but two entry points, one per binary. `main` is
//! `rust-formatter`; `cargo_main` is `cargo rust-formatter`, which cargo invokes
//! with its own name as the first argument.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process,
};

use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum, parser::ValueSource};

use crate::{
    ArrayStyle, ColorChoice, DEFAULT_DIFF_CONTEXT, Emit, FileSource, FormatterOptions, GitScope,
    GitSelection, IndentSpec, InlineTableStyle, Languages, LineRange, ListMode, MessageFormat,
    PackageOrder, ReportMode, RustStyle, RustfmtConfig, SelectionOptions, Spacing, Streams,
    TomlVersion, TrailingComma, UpgradePolicy,
    error::{Error, Usage},
    hook,
    settings::{self, ConfigValue, Provenance, Resolved, Settings, SettingsCli, Source, Toggle},
};

const EDITIONS: [&str; 4] = ["2015", "2018", "2021", "2024"];

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ColorArg {
    Auto,
    Always,
    Never,
}

impl From<ColorArg> for ColorChoice {
    fn from(arg: ColorArg) -> Self {
        match arg {
            ColorArg::Auto => Self::Auto,
            ColorArg::Always => Self::Always,
            ColorArg::Never => Self::Never,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum EmitArg {
    Files,
    Stdout,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum MessageFormatArg {
    Human,
    Json,
}

impl From<MessageFormatArg> for MessageFormat {
    fn from(arg: MessageFormatArg) -> Self {
        match arg {
            MessageFormatArg::Human => Self::Human,
            MessageFormatArg::Json => Self::Json,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ArrayStyleArg {
    Preserve,
    Auto,
    Expand,
}

impl From<ArrayStyleArg> for ArrayStyle {
    fn from(arg: ArrayStyleArg) -> Self {
        match arg {
            ArrayStyleArg::Preserve => Self::Preserve,
            ArrayStyleArg::Auto => Self::Auto,
            ArrayStyleArg::Expand => Self::Expand,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum SpacingArg {
    Compact,
    Spaced,
}

impl From<SpacingArg> for Spacing {
    fn from(arg: SpacingArg) -> Self {
        match arg {
            SpacingArg::Compact => Self::Compact,
            SpacingArg::Spaced => Self::Spaced,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum InlineTableStyleArg {
    Auto,
    Compact,
    Expand,
    Section,
}

impl From<InlineTableStyleArg> for InlineTableStyle {
    fn from(arg: InlineTableStyleArg) -> Self {
        match arg {
            InlineTableStyleArg::Auto => Self::Auto,
            InlineTableStyleArg::Compact => Self::Compact,
            InlineTableStyleArg::Expand => Self::Expand,
            InlineTableStyleArg::Section => Self::Section,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum PackageOrderArg {
    Book,
    StyleGuide,
}

impl From<PackageOrderArg> for PackageOrder {
    fn from(arg: PackageOrderArg) -> Self {
        match arg {
            PackageOrderArg::Book => Self::Book,
            PackageOrderArg::StyleGuide => Self::StyleGuide,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum TomlVersionArg {
    #[value(name = "1.0")]
    V1_0,
    #[value(name = "1.1")]
    V1_1,
}

impl From<TomlVersionArg> for TomlVersion {
    fn from(arg: TomlVersionArg) -> Self {
        match arg {
            TomlVersionArg::V1_0 => Self::V1_0,
            TomlVersionArg::V1_1 => Self::V1_1,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum TrailingCommaArg {
    Never,
    Multiline,
}

impl From<TrailingCommaArg> for TrailingComma {
    fn from(arg: TrailingCommaArg) -> Self {
        match arg {
            TrailingCommaArg::Never => Self::Never,
            TrailingCommaArg::Multiline => Self::Multiline,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum DirectivesArg {
    On,
    Off,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum RustStyleArg {
    Comments,
    Literals,
    Strict,
}

impl From<RustStyleArg> for RustStyle {
    fn from(arg: RustStyleArg) -> Self {
        match arg {
            RustStyleArg::Comments => Self::Comments,
            RustStyleArg::Literals => Self::Literals,
            RustStyleArg::Strict => Self::Strict,
        }
    }
}

/// `L-L`, `L:C-L:C` or a single `L`. rustfmt's `--file-lines` works in whole
/// lines, so a column is accepted -- an editor's format-selection request comes
/// with one -- and widened rather than refused.
fn parse_range(value: &str) -> Result<LineRange, String> {
    let position = |text: &str| -> Result<usize, String> {
        let line = text.split_once(':').map_or(text, |(line, _)| line);
        match line.trim().parse::<usize>() {
            Ok(0) => Err("line numbers start at 1".to_string()),
            Ok(line) => Ok(line),
            Err(_) => Err(format!("expected a line number, found `{line}`")),
        }
    };

    let (start, end) = if let Some((start, end)) = value.split_once('-') {
        (position(start)?, position(end)?)
    } else {
        let only = position(value)?;
        (only, only)
    };
    if end < start {
        return Err(format!("range {start}-{end} ends before it starts"));
    }
    Ok(LineRange { start, end })
}

fn parse_toml_indent(value: &str) -> Result<IndentSpec, String> {
    value.parse()
}

/// The value parser every boolean flag shares, so `--sort-deps=0` and
/// `--sort-deps=off` mean what a configuration file means by them.
fn boolish() -> clap::builder::BoolishValueParser {
    clap::builder::BoolishValueParser::new()
}

const EXAMPLES: &str = "\
Examples:
  rust-formatter                                   format the current directory in place
  rust-formatter --check src/                      exit 1 and print a unified diff
  rust-formatter -l . | xargs $EDITOR              only the paths that differ
  rust-formatter --check --message-format json .   one JSON document on stdout
  rust-formatter --staged --restage                a pre-commit hook
  cat x.rs | rust-formatter --stdin --stdin-filepath x.rs
";

#[derive(Parser, Debug)]
#[command(
    name = "rust-formatter",
    about = "Format Rust and TOML without writing rustfmt.toml or changing the project's toolchain",
    after_help = EXAMPLES,
    version,
    // `--help` already exists, and a `help` subcommand would shadow a directory
    // of that name and earn a man page nobody wants.
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Directories, cargo projects, .rs files or .toml files to format. Defaults to the current directory.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Apply a named settings profile (repeatable, layered). Anything set alongside it still wins.
    #[arg(help_heading = "Configuration", long = "preset", value_name = "NAME")]
    preset: Vec<String>,

    /// Read settings from FILE, which outranks every discovered configuration source.
    #[arg(
        help_heading = "Configuration",
        long = "config-file",
        value_name = "FILE",
        conflicts_with = "no_config"
    )]
    config_file: Option<PathBuf>,

    /// Ignore every configuration file: the rust-toolchain.toml pin, the Cargo.toml metadata tables and any rust-formatter.toml.
    #[arg(help_heading = "Configuration", long = "no-config")]
    no_config: bool,

    /// Print the settings this run resolves to, with the source of each, then exit.
    #[arg(help_heading = "Configuration", long = "print-settings")]
    print_settings: bool,

    /// Read the paths to format from FILE, one per line or NUL-separated. `-` reads standard input.
    #[arg(help_heading = "Selecting files", 
        long = "files-from",
        value_name = "FILE",
        conflicts_with_all = ["since", "staged", "stdin"]
    )]
    files_from: Vec<PathBuf>,

    /// Format only files that changed since REF (compared against the merge base with HEAD).
    #[arg(
        help_heading = "Git scoping",
        long,
        value_name = "REF",
        allow_hyphen_values = true,
        conflicts_with_all = ["staged", "stdin"]
    )]
    since: Option<String>,

    /// Format only files with staged changes. Rewrites the working tree, and the index only with --restage.
    #[arg(help_heading = "Git scoping", long, conflicts_with = "stdin")]
    staged: bool,

    /// Re-add the formatted paths to the index, so a pre-commit hook commits what was formatted.
    #[arg(help_heading = "Git scoping", 
        long,
        requires = "staged",
        conflicts_with_all = ["check", "list_files", "list_different"]
    )]
    restage: bool,

    /// Leave untracked files out of --since, which otherwise selects them because they differ from every ref.
    #[arg(
        help_heading = "Git scoping",
        long = "no-untracked",
        requires = "since"
    )]
    no_untracked: bool,

    /// Reach into initialized submodules as well, applying the same --since or --staged scope inside each.
    #[arg(help_heading = "Git scoping", long = "recurse-submodules")]
    recurse_submodules: bool,

    /// Only format files matching GLOB (gitignore syntax, repeatable).
    #[arg(
        help_heading = "Selecting files",
        long = "include",
        value_name = "GLOB"
    )]
    includes: Vec<String>,

    /// Skip files matching GLOB (gitignore syntax, repeatable). Also prunes matching directories.
    #[arg(
        help_heading = "Selecting files",
        long = "exclude",
        value_name = "GLOB"
    )]
    excludes: Vec<String>,

    /// Only format Rust files.
    #[arg(
        help_heading = "Selecting files",
        long = "rust-only",
        conflicts_with = "toml_only"
    )]
    rust_only: bool,

    /// Only format TOML files. Needs no rustfmt toolchain.
    #[arg(help_heading = "Selecting files", long = "toml-only")]
    toml_only: bool,

    /// Walk hidden files and directories.
    #[arg(help_heading = "Selecting files",
        long = "hidden",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["hidden", "no_hidden"]
    )]
    hidden: Option<bool>,

    #[arg(long = "no-hidden",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["hidden", "no_hidden"]
    )]
    no_hidden: bool,

    /// Ignore .gitignore, .ignore, git excludes and parent ignore files.
    #[arg(help_heading = "Selecting files",
        long = "no-ignore",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["no_ignore"]
    )]
    no_ignore: Option<bool>,

    /// Read extra ignore patterns from FILE (gitignore syntax, repeatable).
    #[arg(
        help_heading = "Selecting files",
        long = "ignore-path",
        value_name = "FILE"
    )]
    ignore_paths: Vec<PathBuf>,

    /// Descend at most N directory levels below each named directory.
    #[arg(help_heading = "Selecting files", long = "max-depth", value_name = "N")]
    max_depth: Option<usize>,

    /// Skip TOML files matching GLOB (repeatable). Adds to the default skip list.
    #[arg(
        help_heading = "Selecting files",
        long = "skip-toml",
        value_name = "GLOB"
    )]
    skip_toml: Vec<String>,

    /// Do not skip Cargo.lock, clippy.toml, rustfmt.toml and .rustfmt.toml.
    #[arg(help_heading = "Selecting files",
        long = "no-default-toml-skips",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["no_default_toml_skips"]
    )]
    no_default_toml_skips: Option<bool>,

    /// Indent wrapped TOML containers by WIDTH spaces, or by `tab`.
    #[arg(help_heading = "TOML layout", 
        long = "toml-indent",
        value_name = "WIDTH",
        default_value = "4",
        value_parser = parse_toml_indent
    )]
    toml_indent: IndentSpec,

    /// Columns a tab advances to in the TOML width budget.
    #[arg(help_heading = "TOML layout", 
        long = "toml-tab-width",
        value_name = "N",
        default_value_t = 4,
        value_parser = clap::value_parser!(u8).range(settings::TOML_TAB_WIDTH)
    )]
    toml_tab_width: u8,

    /// Keep a TOML value on one line while it fits in N display columns.
    #[arg(help_heading = "TOML layout", 
        long = "toml-max-width",
        value_name = "N",
        default_value_t = 100,
        value_parser = clap::value_parser!(u16).range(settings::TOML_MAX_WIDTH)
    )]
    toml_max_width: u16,

    /// How a TOML array chooses between one line and many. Overridden only
    /// inside a container `--toml-inline-tables compact` has pinned to one line.
    #[arg(help_heading = "TOML layout", long = "toml-arrays", value_enum, value_name = "STYLE", default_value_t = ArrayStyleArg::Preserve)]
    toml_arrays: ArrayStyleArg,

    /// How a TOML inline table chooses between one line and many. `compact` never wraps one, so the formatter writes no TOML 1.1 construct of its own.
    #[arg(help_heading = "TOML layout", long = "toml-inline-tables", value_enum, value_name = "STYLE", default_value_t = InlineTableStyleArg::Auto)]
    toml_inline_tables: InlineTableStyleArg,

    /// TOML revision the output must satisfy. `1.0` never wraps an inline table or gives one a trailing comma, and reports every 1.1-only spelling it cannot remove without rewriting a value.
    #[arg(help_heading = "TOML layout", long = "toml-version", value_enum, value_name = "VERSION", default_value_t = TomlVersionArg::V1_1)]
    toml_version: TomlVersionArg,

    /// Whether a wrapped TOML array or inline table ends with a comma.
    #[arg(help_heading = "TOML layout", long = "toml-trailing-comma", value_enum, value_name = "WHEN", default_value_t = TrailingCommaArg::Never)]
    toml_trailing_comma: TrailingCommaArg,

    /// Put exactly one blank line before every TOML table header except the first.
    #[arg(help_heading = "TOML layout",
        long = "toml-blank-line-before-tables",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["toml_blank_line_before_tables", "no_toml_blank_line_before_tables"]
    )]
    toml_blank_line_before_tables: Option<bool>,

    #[arg(long = "no-toml-blank-line-before-tables",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["toml_blank_line_before_tables", "no_toml_blank_line_before_tables"]
    )]
    no_toml_blank_line_before_tables: bool,

    /// Keep at most N consecutive blank lines in a TOML file.
    #[arg(help_heading = "TOML layout", 
        long = "toml-max-blank-lines",
        value_name = "N",
        default_value_t = 1,
        value_parser = clap::value_parser!(u8).range(settings::TOML_MAX_BLANK_LINES)
    )]
    toml_max_blank_lines: u8,

    /// Whether a one-line TOML array pads the inside of its brackets.
    #[arg(help_heading = "TOML layout", long = "toml-array-spacing", value_enum, value_name = "STYLE", default_value_t = SpacingArg::Compact)]
    toml_array_spacing: SpacingArg,

    /// Whether a one-line TOML inline table pads the inside of its braces.
    #[arg(help_heading = "TOML layout", long = "toml-inline-table-spacing", value_enum, value_name = "STYLE", default_value_t = SpacingArg::Spaced)]
    toml_inline_table_spacing: SpacingArg,

    /// Pad TOML keys so the `=` of neighbouring entries lines up.
    #[arg(help_heading = "TOML layout",
        long = "toml-align-entries",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["toml_align_entries", "no_toml_align_entries"]
    )]
    toml_align_entries: Option<bool>,

    #[arg(long = "no-toml-align-entries",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["toml_align_entries", "no_toml_align_entries"]
    )]
    no_toml_align_entries: bool,

    /// Pad TOML lines so the `#` of neighbouring same-line comments lines up.
    #[arg(help_heading = "TOML layout",
        long = "toml-align-comments",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["toml_align_comments", "no_toml_align_comments"]
    )]
    toml_align_comments: Option<bool>,

    #[arg(long = "no-toml-align-comments",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["toml_align_comments", "no_toml_align_comments"]
    )]
    no_toml_align_comments: bool,

    /// Indent a TOML table header by the number of headers above it.
    #[arg(help_heading = "TOML layout",
        long = "toml-indent-tables",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["toml_indent_tables", "no_toml_indent_tables"]
    )]
    toml_indent_tables: Option<bool>,

    #[arg(long = "no-toml-indent-tables",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["toml_indent_tables", "no_toml_indent_tables"]
    )]
    no_toml_indent_tables: bool,

    /// Indent the entries under a TOML table header by one level.
    #[arg(help_heading = "TOML layout",
        long = "toml-indent-entries",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["toml_indent_entries", "no_toml_indent_entries"]
    )]
    toml_indent_entries: Option<bool>,

    #[arg(long = "no-toml-indent-entries",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["toml_indent_entries", "no_toml_indent_entries"]
    )]
    no_toml_indent_entries: bool,

    /// Drop the quotes from TOML keys the spec allows to be bare.
    #[arg(help_heading = "TOML layout",
        long = "toml-normalize-keys",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["toml_normalize_keys", "no_toml_normalize_keys"]
    )]
    toml_normalize_keys: Option<bool>,

    #[arg(long = "no-toml-normalize-keys",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["toml_normalize_keys", "no_toml_normalize_keys"]
    )]
    no_toml_normalize_keys: bool,

    /// Whether `# fmt: off` exempts the TOML lines up to the matching `# fmt: on`.
    #[arg(help_heading = "TOML layout", long = "toml-directives", value_enum, value_name = "WHEN", default_value_t = DirectivesArg::On)]
    toml_directives: DirectivesArg,

    /// Sort dependency tables alphabetically.
    #[arg(help_heading = "TOML ordering",
        long = "sort-deps",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["sort_deps", "no_sort_deps"]
    )]
    sort_deps: Option<bool>,

    #[arg(long = "no-sort-deps",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["sort_deps", "no_sort_deps"]
    )]
    no_sort_deps: bool,

    /// Sort [package] and [workspace.package] into cargo's canonical field order, not alphabetically.
    #[arg(help_heading = "TOML ordering",
        long = "sort-package",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["sort_package", "no_sort_package"]
    )]
    sort_package: Option<bool>,

    #[arg(long = "no-sort-package",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["sort_package", "no_sort_package"]
    )]
    no_sort_package: bool,

    /// Which published [package] order --sort-package applies.
    #[arg(help_heading = "TOML ordering", long = "package-order", value_enum, value_name = "ORDER", default_value_t = PackageOrderArg::Book)]
    package_order: PackageOrderArg,

    /// Put the fields of one dependency entry into cargo's canonical order: source first, then what is built from it.
    #[arg(help_heading = "TOML ordering",
        long = "sort-dep-fields",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["sort_dep_fields", "no_sort_dep_fields"]
    )]
    sort_dep_fields: Option<bool>,

    #[arg(long = "no-sort-dep-fields",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["sort_dep_fields", "no_sort_dep_fields"]
    )]
    no_sort_dep_fields: bool,

    /// Sort the keys of [features].
    #[arg(help_heading = "TOML ordering",
        long = "sort-features",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["sort_features", "no_sort_features"]
    )]
    sort_features: Option<bool>,

    #[arg(long = "no-sort-features",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["sort_features", "no_sort_features"]
    )]
    no_sort_features: bool,

    /// Sort the cargo arrays whose order carries no meaning: feature lists, workspace members, keywords, categories, includes and excludes.
    #[arg(help_heading = "TOML ordering",
        long = "sort-arrays",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["sort_arrays", "no_sort_arrays"]
    )]
    sort_arrays: Option<bool>,

    #[arg(long = "no-sort-arrays",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["sort_arrays", "no_sort_arrays"]
    )]
    no_sort_arrays: bool,

    /// Sort [[bin]], [[example]], [[test]] and [[bench]] by their `name`.
    #[arg(help_heading = "TOML ordering",
        long = "sort-targets",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["sort_targets", "no_sort_targets"]
    )]
    sort_targets: Option<bool>,

    #[arg(long = "no-sort-targets",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["sort_targets", "no_sort_targets"]
    )]
    no_sort_targets: bool,

    /// Put the top-level tables of a manifest into the Cargo Book's document order.
    #[arg(help_heading = "TOML ordering",
        long = "sort-tables",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["sort_tables", "no_sort_tables"]
    )]
    sort_tables: Option<bool>,

    #[arg(long = "no-sort-tables",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["sort_tables", "no_sort_tables"]
    )]
    no_sort_tables: bool,

    /// Version-sort the keys of every table, which is the Rust Style Guide's rule. [package] and the document sequence keep their own flags.
    #[arg(help_heading = "TOML ordering",
        long = "sort-keys",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["sort_keys", "no_sort_keys"]
    )]
    sort_keys: Option<bool>,

    #[arg(long = "no-sort-keys",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["sort_keys", "no_sort_keys"]
    )]
    no_sort_keys: bool,

    /// Sort within each blank-line-separated group rather than across them.
    #[arg(help_heading = "TOML ordering",
        long = "sort-grouped",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["sort_grouped", "no_sort_grouped"]
    )]
    sort_grouped: Option<bool>,

    #[arg(long = "no-sort-grouped",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["sort_grouped", "no_sort_grouped"]
    )]
    no_sort_grouped: bool,

    /// Rewrite `dep = { version = "1" }` as `dep = "1"`, the spelling cargo itself writes. This is the one option that changes a TOML value rather than its layout.
    #[arg(help_heading = "TOML ordering",
        long = "cargo-conventions",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["cargo_conventions", "no_cargo_conventions"]
    )]
    cargo_conventions: Option<bool>,

    #[arg(long = "no-cargo-conventions",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["cargo_conventions", "no_cargo_conventions"]
    )]
    no_cargo_conventions: bool,

    /// Format Cargo.toml the way the Rust Style Guide's `Cargo.toml` chapter specifies. The same as `--preset style-guide`.
    #[arg(help_heading = "TOML ordering", long = "style-guide")]
    style_guide: bool,

    /// Run in check mode. Exit 0 if already formatted, 1 if not. Diffs go to stdout, the summary to stderr.
    #[arg(help_heading = "Output", long)]
    check: bool,

    /// Format standard input and write the result to standard output.
    #[arg(help_heading = "Standard input", long, conflicts_with_all = ["list_files", "list_different", "print_config", "print_settings"])]
    stdin: bool,

    /// Treat standard input as this path: picks Rust or TOML and infers the edition.
    #[arg(
        help_heading = "Standard input",
        long = "stdin-filepath",
        value_name = "PATH",
        requires = "stdin"
    )]
    stdin_filepath: Option<PathBuf>,

    /// Where formatted output goes.
    #[arg(
        help_heading = "Output",
        long,
        value_enum,
        value_name = "WHAT",
        conflicts_with = "check"
    )]
    emit: Option<EmitArg>,

    /// Print every file the selection resolves to and exit without formatting.
    #[arg(
        help_heading = "Output",
        long = "list-files",
        conflicts_with = "list_different"
    )]
    list_files: bool,

    /// Print only the files whose formatting differs. Exits 1 if any do.
    #[arg(help_heading = "Output", short = 'l', long = "list-different")]
    list_different: bool,

    /// Output format for the run.
    #[arg(
        help_heading = "Output",
        long = "message-format",
        value_enum,
        value_name = "FORMAT"
    )]
    message_format: Option<MessageFormatArg>,

    /// Lines of context around each hunk of a --check diff.
    #[arg(help_heading = "Output", 
        short = 'U',
        long = "diff-context",
        value_name = "N",
        default_value_t = DEFAULT_DIFF_CONTEXT as u16,
        value_parser = clap::value_parser!(u16).range(settings::DIFF_CONTEXT)
    )]
    diff_context: u16,

    /// Print the rustfmt configuration this run would apply, then exit.
    #[arg(help_heading = "Rust formatting", long = "print-config")]
    print_config: bool,

    /// Rust edition to format with.
    #[arg(help_heading = "Rust formatting", long, value_parser = EDITIONS)]
    edition: Option<String>,

    /// Edition of the Rust Style Guide to format by. Pins output across rustfmt releases, which --edition does not.
    #[arg(help_heading = "Rust formatting", long = "style-edition", value_parser = EDITIONS)]
    style_edition: Option<String>,

    /// Restrict formatting to LINE, LINE-LINE or LINE:COL-LINE:COL (repeatable). Rust only; a .rs file or --stdin; columns widen to whole lines.
    #[arg(help_heading = "Rust formatting", 
        long = "range",
        value_name = "RANGE",
        value_parser = parse_range,
        conflicts_with_all = ["files_from", "since", "staged"]
    )]
    ranges: Vec<LineRange>,

    /// Rustup toolchain to use, or "auto" to prefer nightly and fall back.
    #[arg(help_heading = "Rust formatting", long, default_value = crate::toolchain::AUTO)]
    toolchain: String,

    /// Format only the package for the detected manifest, not every workspace member.
    #[arg(help_heading = "Rust formatting",
        long = "no-all",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["no_all"]
    )]
    no_all: Option<bool>,

    /// Apply a named group of rustfmt options (repeatable, layered). --config still wins over it.
    #[arg(
        help_heading = "Rust formatting",
        long = "rust-style",
        value_enum,
        value_name = "NAME"
    )]
    rust_style: Vec<RustStyleArg>,

    /// Set additional rustfmt options (e.g. --config `max_width=120,tab_spaces=4`).
    #[arg(
        help_heading = "Rust formatting",
        long = "config",
        value_name = "KEY=VALUE"
    )]
    configs: Vec<String>,

    /// Drop a rustfmt option so its own default applies (repeatable).
    #[arg(
        help_heading = "Rust formatting",
        long = "unset-config",
        value_name = "KEY"
    )]
    unset_configs: Vec<String>,

    /// Print verbose output.
    #[arg(help_heading = "Output",
        short = 'v',
        long = "verbose",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["verbose", "no_verbose"]
    )]
    verbose: Option<bool>,

    #[arg(long = "no-verbose",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["verbose", "no_verbose"]
    )]
    no_verbose: bool,

    /// Print less output.
    #[arg(help_heading = "Output",
        short = 'q',
        long = "quiet",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["quiet", "no_quiet"]
    )]
    quiet: Option<bool>,

    #[arg(long = "no-quiet",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["quiet", "no_quiet"]
    )]
    no_quiet: bool,

    /// Look up the registry and rewrite Cargo.toml dependency versions to full x.y.z.
    #[arg(help_heading = "Dependency versions", long)]
    full_versions: bool,

    /// Also rewrite requirements that already name x.y.z, to the newest compatible release.
    #[arg(help_heading = "Dependency versions", long, requires = "full_versions")]
    upgrade: bool,

    /// Allow an upgrade that breaks the existing requirement, such as 1.x to 2.0.0.
    #[arg(help_heading = "Dependency versions", long, requires = "upgrade")]
    upgrade_incompatible: bool,

    /// Include `=` requirements when upgrading.
    #[arg(help_heading = "Dependency versions", long, requires = "upgrade")]
    upgrade_pinned: bool,

    /// Consider yanked releases when choosing a version.
    #[arg(help_heading = "Dependency versions", long, requires = "full_versions")]
    allow_yanked: bool,

    /// Choose versions without regard for the manifest's rust-version.
    #[arg(help_heading = "Dependency versions", long, requires = "full_versions")]
    ignore_rust_version: bool,

    /// Sparse index to resolve against instead of <https://index.crates.io>.
    #[arg(
        help_heading = "Dependency versions",
        long,
        value_name = "URL",
        requires = "full_versions"
    )]
    registry_url: Option<String>,

    /// Worker threads. Defaults to the number of available CPUs.
    #[arg(
        help_heading = "Execution",
        short = 'j',
        long = "jobs",
        value_name = "N"
    )]
    jobs: Option<NonZeroUsize>,

    /// When to use colored output.
    #[arg(help_heading = "Output", long, value_enum, value_name = "WHEN", default_value_t = ColorArg::Auto)]
    color: ColorArg,

    /// Keep running: reformat each file as it changes on disk. Ends on Ctrl-C.
    ///
    /// The two listings are treated differently below on purpose:
    /// `--list-files` returns before any formatting, so a watch would reprint
    /// the same names for ever, while `--list-different` is `--check` with
    /// names instead of diffs -- it writes nothing, so watching it is a
    /// standing report of what is unformatted.
    #[arg(
        help_heading = "Execution",
        long = "watch",
        conflicts_with_all = [
            "stdin",
            "print_config",
            "print_settings",
            "list_files",
            "restage",
            "ranges",
            "files_from",
            "since",
            "staged",
        ]
    )]
    watch: bool,

    /// Stop scheduling work after the first file fails, instead of collecting every error.
    #[arg(help_heading = "Execution",
        long = "fail-fast",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["fail_fast", "no_fail_fast"]
    )]
    fail_fast: Option<bool>,

    #[arg(long = "no-fail-fast",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["fail_fast", "no_fail_fast"]
    )]
    no_fail_fast: bool,

    /// Never use the network: read the local registry index and run cargo offline.
    #[arg(help_heading = "Execution",
        long = "offline",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["offline", "no_offline"]
    )]
    offline: Option<bool>,

    #[arg(long = "no-offline",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["offline", "no_offline"]
    )]
    no_offline: bool,

    /// Skip files a previous run already proved were formatted. On by default.
    #[arg(help_heading = "Execution",
        long = "cache",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["cache", "no_cache"]
    )]
    cache: Option<bool>,

    #[arg(long = "no-cache",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["cache", "no_cache"]
    )]
    no_cache: bool,

    /// Install a missing rustfmt component instead of only printing the command.
    #[arg(help_heading = "Execution",
        long = "install-toolchain",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL",
        value_parser = boolish(),
        overrides_with_all = ["install_toolchain", "no_install_toolchain"]
    )]
    install_toolchain: Option<bool>,

    #[arg(long = "no-install-toolchain",
        hide = true,
        action = clap::ArgAction::SetTrue,
        overrides_with_all = ["install_toolchain", "no_install_toolchain"]
    )]
    no_install_toolchain: bool,

    /// Extra arguments passed directly to rustfmt.
    #[arg(last = true)]
    extra_args: Vec<String>,
}

/// What a run does *instead* of formatting. None of these reads a configuration
/// source: they are wiring for something else -- a shell, a manpage reader, a
/// git repository -- and a repository with a typo in its own settings should
/// still be able to ask for them.
#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Print a shell completion script for SHELL on standard output.
    Completions {
        #[arg(value_enum, value_name = "SHELL")]
        shell: clap_complete::aot::Shell,

        /// Write the conventionally named file into DIR instead of stdout.
        #[arg(long, value_name = "DIR")]
        out_dir: Option<PathBuf>,
    },

    /// Print this tool's man page, as roff, on standard output.
    Man {
        /// Write `rust-formatter.1` and one page per command into DIR.
        #[arg(long, value_name = "DIR")]
        out_dir: Option<PathBuf>,
    },

    /// Install, remove or inspect this repository's git `pre-commit` hook.
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
}

#[derive(clap::Subcommand, Debug)]
enum HookAction {
    /// Write the `pre-commit` hook into this repository's hooks directory.
    Install {
        /// Abort the commit instead of formatting it: the hook runs --staged --check.
        #[arg(long)]
        check: bool,

        /// Replace a hook this tool did not write, saving the old one beside it.
        #[arg(long)]
        force: bool,
    },

    /// Remove the `pre-commit` hook, if this tool installed it.
    Uninstall,

    /// Report the hooks directory and what is installed in it.
    Status,
}

impl Commands {
    fn run(&self) -> i32 {
        match self {
            Self::Completions { shell, out_dir } => completions(*shell, out_dir.as_deref()),
            Self::Man { out_dir } => man(out_dir.as_deref()),
            Self::Hook { action } => action.run(),
        }
    }
}

/// The command a completion script and a man page are generated from is built
/// fresh here, so the program they name is `rust-formatter` even when this ran
/// as `cargo rust-formatter`.
fn generated_from() -> clap::Command {
    Cli::command()
        .name("rust-formatter")
        .bin_name("rust-formatter")
}

fn completions(shell: clap_complete::aot::Shell, out_dir: Option<&Path>) -> i32 {
    let mut command = generated_from();
    if let Some(directory) = out_dir {
        return match clap_complete::aot::generate_to(
            shell,
            &mut command,
            "rust-formatter",
            directory,
        ) {
            Ok(path) => {
                eprintln!("wrote {}", path.display());
                0
            }
            Err(err) => fail(format!(
                "could not write into {}: {err}",
                directory.display()
            )),
        };
    }
    // `clap_complete` panics rather than reporting a write failure, and this
    // build aborts on a panic -- so it is handed a buffer and the pipe is dealt
    // with here.
    let mut script = Vec::new();
    clap_complete::aot::generate(shell, &mut command, "rust-formatter", &mut script);
    emit(&script)
}

fn man(out_dir: Option<&Path>) -> i32 {
    if let Some(directory) = out_dir {
        // `clap_mangen` walks the subcommands itself, naming each page after the
        // display name clap builds (`rust-formatter-hook-install.1`).
        return match clap_mangen::generate_to(generated_from(), directory) {
            Ok(()) => 0,
            Err(err) => fail(format!(
                "could not write into {}: {err}",
                directory.display()
            )),
        };
    }
    let mut page = Vec::new();
    match clap_mangen::Man::new(generated_from())
        .manual("General Commands Manual")
        .render(&mut page)
    {
        Ok(()) => emit(&page),
        Err(err) => fail(err),
    }
}

/// A generated script or man page is a product, so it goes to stdout whole --
/// and a reader that closed the pipe (`completions bash | head`) asked for that.
fn emit(bytes: &[u8]) -> i32 {
    use std::io::Write as _;

    let mut out = io::stdout();
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => 0,
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(err) => fail(err),
    }
}

impl HookAction {
    fn run(&self) -> i32 {
        let cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(err) => fail(format!("could not read the working directory: {err}")),
        };
        match self {
            Self::Install { check, force } => install(&cwd, *check, *force),
            Self::Uninstall => uninstall(&cwd),
            Self::Status => status(&cwd),
        }
    }
}

/// The binary a hook falls back on when nothing answers to `rust-formatter` on
/// `PATH`.
///
/// Under the cargo alias this process *is* `cargo-rust-formatter`, which eats a
/// leading `rust-formatter` argument -- so a sibling of the right name is
/// preferred, and the alias itself is only recorded when there is none.
fn hook_fallback() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rust-formatter"));
    if exe
        .file_stem()
        .is_some_and(|stem| stem == "cargo-rust-formatter")
    {
        let sibling = exe.with_file_name(format!("rust-formatter{}", std::env::consts::EXE_SUFFIX));
        if sibling.exists() {
            return sibling;
        }
    }
    exe
}

fn install(cwd: &Path, check: bool, force: bool) -> i32 {
    let mode = if check {
        hook::Mode::Check
    } else {
        hook::Mode::Restage
    };
    let fallback = hook_fallback();
    let (located, outcome) = match hook::install(cwd, mode, force, &fallback) {
        Ok(result) => result,
        Err(err) => fail(err),
    };
    let path = located.path.display();
    match outcome {
        Ok(hook::Installed::Unchanged) => {
            println!(
                "{path} is already installed and current (mode {})",
                mode.name()
            );
        }
        Ok(hook::Installed::Forced) => {
            println!("saved the existing hook as {}", hook::BACKUP);
            println!("installed {path} (mode {})", mode.name());
            println!("falls back on {}", fallback.display());
        }
        Ok(hook::Installed::Upgraded { version, mode: had }) => {
            println!(
                "upgraded {path} (was version {version}, mode {}; now mode {})",
                had.name(),
                mode.name()
            );
            println!("falls back on {}", fallback.display());
        }
        Ok(hook::Installed::Fresh) => {
            println!("installed {path} (mode {})", mode.name());
            println!("falls back on {}", fallback.display());
        }
        Err(hook::Refused::Foreign) => fail(format!(
            "{path} already exists and was not installed by rust-formatter.\n\
             Add this line to it instead:\n    \
             rust-formatter --staged --restage || exit 1\n\
             or re-run with --force, which saves the existing hook as {}.",
            hook::BACKUP
        )),
        Err(hook::Refused::Backup) => fail(format!(
            "{} already exists, and --force would overwrite it.\n\
             It is the only copy of a hook this tool moved aside before; move or\n\
             delete it yourself, then re-run.",
            located.directory.join(hook::BACKUP).display()
        )),
    }
    0
}

fn uninstall(cwd: &Path) -> i32 {
    let (located, outcome) = match hook::uninstall(cwd) {
        Ok(result) => result,
        Err(err) => fail(err),
    };
    let path = located.path.display();
    match outcome {
        Ok(hook::Uninstalled::Removed(mode)) => println!("removed {path} (mode {})", mode.name()),
        Ok(hook::Uninstalled::Restored(mode)) => {
            println!("removed {path} (mode {})", mode.name());
            println!("restored the hook saved as {}", hook::BACKUP);
        }
        Ok(hook::Uninstalled::Absent) => println!("no rust-formatter hook is installed at {path}"),
        Err(hook::Refused::Foreign | hook::Refused::Backup) => fail(format!(
            "{path} was not installed by rust-formatter, so it was left alone."
        )),
    }
    0
}

/// The report is what this subcommand was asked for, so it goes to stdout -- and
/// the exit code follows the tool's own contract, where `1` means the state is
/// not the one you wanted. `rust-formatter hook status >/dev/null || rust-formatter hook install`
/// is the point of that.
fn status(cwd: &Path) -> i32 {
    let located = match hook::locate(cwd) {
        Ok(located) => located,
        Err(err) => fail(err),
    };
    println!("hooks-dir: {}", located.directory.display());
    match located.state {
        hook::State::Ours { version, mode } => {
            println!(
                "pre-commit: installed by rust-formatter (version {version}, mode {})",
                mode.name()
            );
            0
        }
        hook::State::Absent => {
            println!("pre-commit: absent");
            1
        }
        hook::State::Foreign => {
            println!("pre-commit: present, but not installed by rust-formatter");
            1
        }
    }
}

fn parse_string_list(raw: &str) -> Result<Vec<String>, Usage> {
    let not_a_list = || Usage::PatternListExpected {
        raw: raw.to_owned(),
    };
    let value = raw.parse::<toml_edit::Value>().map_err(|_| not_a_list())?;
    match &value {
        toml_edit::Value::String(one) => Ok(vec![one.value().clone()]),
        toml_edit::Value::Array(entries) => entries
            .iter()
            .map(|entry| entry.as_str().map(str::to_owned).ok_or_else(not_a_list))
            .collect(),
        _ => Err(not_a_list()),
    }
}

fn reconcile(
    flag: Option<String>,
    from_config: Option<String>,
    typed: bool,
    flag_name: &'static str,
    key: &'static str,
) -> Result<Option<String>, Usage> {
    match (flag, from_config) {
        (Some(flag), Some(config)) if typed && flag != config => Err(Usage::EditionConflict {
            flag: flag_name,
            value: flag,
            key,
            configured: config,
        }),
        (Some(value), _) | (None, Some(value)) => {
            if !EDITIONS.contains(&value.as_str()) {
                return Err(Usage::InvalidEdition {
                    key,
                    value,
                    expected: &EDITIONS,
                });
            }
            Ok(Some(value))
        }
        (None, None) => Ok(None),
    }
}

/// A boolean the command line carries, from the flag and the hidden negative
/// spelling that shares its argument group.
fn typed_flag(value: Option<bool>, negated: bool) -> Option<bool> {
    if negated { Some(false) } else { value }
}

fn toggle(value: Option<bool>) -> Option<Toggle> {
    value.map(Toggle)
}

fn is(value: Option<Toggle>, fallback: bool) -> bool {
    value.map_or(fallback, bool::from)
}

fn config_overlay(entries: &[String]) -> Result<BTreeMap<String, ConfigValue>, Usage> {
    let mut parsed = RustfmtConfig::empty();
    parsed.extend_from_slice(entries)?;
    Ok(parsed
        .options()
        .map(|(key, value)| (key.to_owned(), ConfigValue::String(value.to_owned())))
        .collect())
}

impl Cli {
    #[expect(
        clippy::too_many_lines,
        reason = "one line per flag the command line can set, and there are that many \
                  flags. A helper per group would only move the list."
    )]
    fn overlay(&self, matches: &clap::ArgMatches) -> Result<Settings, Usage> {
        let typed = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);
        let mut out = Settings::default();

        if typed("toolchain") {
            out.toolchain = Some(self.toolchain.clone());
        }
        out.edition.clone_from(&self.edition);
        out.style_edition.clone_from(&self.style_edition);
        if !self.rust_style.is_empty() {
            out.rust_style = Some(
                self.rust_style
                    .iter()
                    .copied()
                    .map(RustStyle::from)
                    .collect(),
            );
        }
        if !self.configs.is_empty() {
            out.config = Some(config_overlay(&self.configs)?);
        }
        if !self.unset_configs.is_empty() {
            out.unset_config = Some(self.unset_configs.clone());
        }
        out.no_all = toggle(self.no_all);

        if !self.includes.is_empty() {
            out.include = Some(self.includes.clone());
        }
        if !self.excludes.is_empty() {
            out.exclude = Some(self.excludes.clone());
        }
        if !self.ignore_paths.is_empty() {
            out.ignore_path = Some(self.ignore_paths.clone());
        }
        out.max_depth = self.max_depth;
        out.hidden = toggle(typed_flag(self.hidden, self.no_hidden));
        out.no_ignore = toggle(self.no_ignore);
        if !self.skip_toml.is_empty() {
            out.skip_toml = Some(self.skip_toml.clone());
        }
        out.no_default_toml_skips = toggle(self.no_default_toml_skips);
        if self.rust_only || self.toml_only {
            out.languages = Some(if self.rust_only {
                Languages::Rust
            } else {
                Languages::Toml
            });
        }

        if typed("toml_indent") {
            out.toml_indent = Some(self.toml_indent);
        }
        if typed("toml_tab_width") {
            out.toml_tab_width = Some(self.toml_tab_width);
        }
        if typed("toml_max_width") {
            out.toml_max_width = Some(self.toml_max_width);
        }
        if typed("toml_arrays") {
            out.toml_arrays = Some(self.toml_arrays.into());
        }
        if typed("toml_inline_tables") {
            out.toml_inline_tables = Some(self.toml_inline_tables.into());
        }
        if typed("toml_version") {
            out.toml_version = Some(self.toml_version.into());
        }
        if typed("toml_trailing_comma") {
            out.toml_trailing_comma = Some(self.toml_trailing_comma.into());
        }
        if typed("toml_max_blank_lines") {
            out.toml_max_blank_lines = Some(self.toml_max_blank_lines);
        }
        if typed("toml_array_spacing") {
            out.toml_array_spacing = Some(self.toml_array_spacing.into());
        }
        if typed("toml_inline_table_spacing") {
            out.toml_inline_table_spacing = Some(self.toml_inline_table_spacing.into());
        }
        if typed("toml_directives") {
            out.toml_directives = Some(Toggle(self.toml_directives == DirectivesArg::On));
        }
        if typed("package_order") {
            out.package_order = Some(self.package_order.into());
        }

        out.toml_blank_line_before_tables = toggle(typed_flag(
            self.toml_blank_line_before_tables,
            self.no_toml_blank_line_before_tables,
        ));
        out.toml_align_entries = toggle(typed_flag(
            self.toml_align_entries,
            self.no_toml_align_entries,
        ));
        out.toml_align_comments = toggle(typed_flag(
            self.toml_align_comments,
            self.no_toml_align_comments,
        ));
        out.toml_indent_tables = toggle(typed_flag(
            self.toml_indent_tables,
            self.no_toml_indent_tables,
        ));
        out.toml_indent_entries = toggle(typed_flag(
            self.toml_indent_entries,
            self.no_toml_indent_entries,
        ));
        out.toml_normalize_keys = toggle(typed_flag(
            self.toml_normalize_keys,
            self.no_toml_normalize_keys,
        ));
        out.sort_deps = toggle(typed_flag(self.sort_deps, self.no_sort_deps));
        out.sort_package = toggle(typed_flag(self.sort_package, self.no_sort_package));
        out.sort_dep_fields = toggle(typed_flag(self.sort_dep_fields, self.no_sort_dep_fields));
        out.sort_features = toggle(typed_flag(self.sort_features, self.no_sort_features));
        out.sort_arrays = toggle(typed_flag(self.sort_arrays, self.no_sort_arrays));
        out.sort_targets = toggle(typed_flag(self.sort_targets, self.no_sort_targets));
        out.sort_tables = toggle(typed_flag(self.sort_tables, self.no_sort_tables));
        out.sort_keys = toggle(typed_flag(self.sort_keys, self.no_sort_keys));
        out.sort_grouped = toggle(typed_flag(self.sort_grouped, self.no_sort_grouped));
        out.cargo_conventions = toggle(typed_flag(
            self.cargo_conventions,
            self.no_cargo_conventions,
        ));

        out.jobs = self.jobs;
        out.fail_fast = toggle(typed_flag(self.fail_fast, self.no_fail_fast));
        out.offline = toggle(typed_flag(self.offline, self.no_offline));
        out.cache = toggle(typed_flag(self.cache, self.no_cache));
        if typed("color") {
            out.color = Some(self.color.into());
        }
        if let Some(format) = self.message_format {
            out.message_format = Some(format.into());
        }
        if typed("diff_context") {
            out.diff_context = Some(self.diff_context);
        }
        out.verbose = toggle(typed_flag(self.verbose, self.no_verbose));
        out.quiet = toggle(typed_flag(self.quiet, self.no_quiet));

        Ok(out)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the whole projection from the resolved settings onto FormatterOptions, \
                  field by field. Splitting it hides which fields are covered."
    )]
    fn into_options(
        self,
        settings: &Settings,
        run: &RunContext,
    ) -> Result<FormatterOptions, Usage> {
        let mut config = RustfmtConfig::default();
        for style in settings.rust_style.clone().unwrap_or_default() {
            config.apply_style(style);
        }
        if let Some(table) = &settings.config {
            for (key, value) in table {
                config.set(key.clone(), value.to_string());
            }
        }
        let unset_configs = settings.unset_config.clone().unwrap_or_default();
        let unset_misses = config.unset_all(&unset_configs);

        // rustfmt resolves `ignore` against the directory of the configuration
        // file it was read from, and this tool writes no such file into the
        // project. The selection layer already carries the same gitignore
        // patterns, anchored at the directory being walked, and applies them to
        // TOML as well -- so that is where the list goes.
        let mut excludes = settings.exclude.clone().unwrap_or_default();
        if let Some(list) = config.take("ignore") {
            excludes.extend(parse_string_list(&list)?);
        }

        // An edition a configuration file names describes a tree, not this run,
        // so it ranks below the project's own `rustfmt.toml` and below each
        // package's `edition` -- where a typed `--edition` outranks both.
        let from_config = config.take("edition");
        let (edition, edition_fallback) = if run.edition_typed || from_config.is_some() {
            let value = reconcile(
                settings.edition.clone(),
                from_config,
                run.edition_typed,
                "--edition",
                "edition",
            )?;
            (value, None)
        } else {
            let value = reconcile(
                settings.edition.clone(),
                None,
                false,
                "--edition",
                "edition",
            )?;
            (None, value)
        };
        let style_edition = reconcile(
            settings.style_edition.clone(),
            config.take("style_edition"),
            run.style_edition_typed,
            "--style-edition",
            "style_edition",
        )?;

        let scope = if let Some(reference) = self.since {
            Some(GitScope::Since(reference))
        } else {
            self.staged.then_some(GitScope::Staged)
        };
        let source = match scope {
            Some(scope) => FileSource::Git(GitSelection {
                scope,
                untracked: !self.no_untracked,
                recurse_submodules: self.recurse_submodules,
            }),
            None if self.files_from.is_empty() => FileSource::Paths,
            None => FileSource::FilesFrom(self.files_from),
        };

        let list = match (self.list_files, self.list_different) {
            (true, _) => ListMode::Files,
            (_, true) => ListMode::Different,
            _ => ListMode::None,
        };

        let toml_style = settings.toml_style();

        Ok(FormatterOptions {
            targets: self.paths,
            source,
            selection: SelectionOptions {
                include: settings.include.clone().unwrap_or_default(),
                exclude: excludes,
                languages: settings.languages.unwrap_or_default(),
                hidden: is(settings.hidden, false),
                no_ignore: is(settings.no_ignore, false),
                ignore_paths: settings.ignore_path.clone().unwrap_or_default(),
                max_depth: settings.max_depth,
                toml_skips: settings.skip_toml.clone().unwrap_or_default(),
                default_toml_skips: !is(settings.no_default_toml_skips, false),
                narrowed_by_user: run.narrowed_by_user,
            },
            // Listing what differs is a check that prints names instead of diffs.
            check: self.check || self.list_different,
            toolchain: settings
                .toolchain
                .clone()
                .unwrap_or_else(|| crate::toolchain::AUTO.to_string()),
            edition,
            edition_fallback,
            style_edition,
            ranges: self.ranges,
            all: !is(settings.no_all, false),
            restage: self.restage,
            verbose: is(settings.verbose, false),
            quiet: is(settings.quiet, false),
            config,
            unset_configs,
            unset_misses,
            config_sources: run.config_sources.clone(),
            lenient_keys: run.lenient.clone(),
            toml_style,
            extra_args: self.extra_args,
            full_versions: self.full_versions,
            upgrade: UpgradePolicy {
                enabled: self.upgrade,
                incompatible: self.upgrade_incompatible,
                pinned: self.upgrade_pinned,
            },
            allow_yanked: self.allow_yanked,
            ignore_rust_version: self.ignore_rust_version,
            registry_url: self.registry_url,
            offline: is(settings.offline, false),
            jobs: settings.jobs,
            cache: is(settings.cache, true),
            fail_fast: is(settings.fail_fast, false),
            install_toolchain: typed_flag(self.install_toolchain, self.no_install_toolchain),
            color: settings.color.unwrap_or_default(),
            diff_context: settings
                .diff_context
                .map_or(DEFAULT_DIFF_CONTEXT, usize::from),
            emit: match self.emit {
                Some(EmitArg::Stdout) => Emit::Stdout,
                _ => Emit::Files,
            },
            list,
            message_format: settings.message_format.unwrap_or_default(),
            print_config: self.print_config,
            stdin: self.stdin,
            stdin_filepath: self.stdin_filepath,
        })
    }
}

/// Where configuration is resolved from: the tree the run actually names, so a
/// run inside a workspace member reads that member's settings.
fn settings_root(cli: &Cli) -> PathBuf {
    let named = cli
        .paths
        .first()
        .cloned()
        .or_else(|| cli.stdin_filepath.clone());
    match named {
        Some(path) if path.is_dir() => path,
        Some(path) => match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        },
        None => PathBuf::from("."),
    }
}

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("error: {message}");
    process::exit(2);
}

/// The `rust-formatter` binary.
pub fn main() -> ! {
    dispatch(std::env::args_os(), "rust-formatter")
}

/// The `cargo rust-formatter` binary. Cargo invokes a `cargo-<name>` executable
/// with `<name>` as its first argument, so that argument is dropped rather than
/// parsed as a path -- and the usage line has to say `cargo rust-formatter`, or
/// every error message names a binary the caller did not type.
pub fn cargo_main() -> ! {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|first| first == "rust-formatter") {
        args.remove(1);
    }
    dispatch(args, "cargo rust-formatter")
}

fn dispatch(args: impl IntoIterator<Item = std::ffi::OsString>, bin_name: &'static str) -> ! {
    let matches = Cli::command().bin_name(bin_name).get_matches_from(args);
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };

    // A subcommand is self-contained: it reads no settings, so a repository with
    // a typo in its own configuration can still be asked for a completion
    // script or told to install a hook.
    if let Some(command) = &cli.command {
        process::exit(command.run());
    }

    let mut names = cli.preset.clone();
    if cli.style_guide {
        names.insert(0, "style-guide".to_string());
    }
    let request = SettingsCli {
        preset: names,
        config_file: cli.config_file.clone(),
        no_config: cli.no_config,
    };

    let resolved = match settings::resolve(&settings_root(&cli), &cargo_env, &request) {
        Ok(resolved) => resolved,
        Err(err) => fail_run(err, &cli, settings::env_message_format(&cargo_env)),
    };
    let overlay = match cli.overlay(&matches) {
        Ok(overlay) => overlay,
        Err(usage) => fail_run(usage, &cli, resolved.settings.message_format),
    };
    let typed = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);
    let run = RunContext {
        lenient: lenient_keys(&resolved.settings, &overlay),
        config_sources: resolved.sources.iter().map(Source::to_string).collect(),
        narrowed_by_user: NARROWING_ARGS.iter().any(|id| typed(id)),
        edition_typed: typed("edition"),
        style_edition_typed: typed("style_edition"),
    };
    let (settings, provenance) = merge_command_line(resolved, overlay);

    if cli.print_settings {
        let json = settings.message_format == Some(MessageFormat::Json);
        let text = settings::render(&settings, &provenance, json);
        print!("{text}");
        process::exit(0);
    }

    if let Err(usage) = validate(&cli, &settings) {
        fail_run(usage, &cli, settings.message_format);
    }

    let choice = settings.color.unwrap_or_default();
    let watch = cli.watch;
    let message_format = settings.message_format;
    let failure_mode = failure_mode(&cli);
    let options = match cli.into_options(&settings, &run) {
        Ok(options) => options,
        Err(usage) => report_run_failure(usage.into(), failure_mode, message_format),
    };

    let streams = Streams::new(
        io::stdout(),
        choice.resolve(&io::stdout()),
        io::stderr(),
        choice.resolve(&io::stderr()),
        choice,
    );

    process::exit(if watch {
        // A watcher that could never start is reported like any other run that
        // could not begin -- as JSON when that is what was asked for.
        crate::watch::run(&options, &streams)
            .unwrap_or_else(|err| crate::report_fatal(err, &options, &streams))
    } else {
        crate::run(&options, &streams)
    });
}

fn fail_run(err: impl Into<Error>, cli: &Cli, configured: Option<MessageFormat>) -> ! {
    let requested = cli.message_format.map(MessageFormat::from).or(configured);
    report_run_failure(err.into(), failure_mode(cli), requested)
}

fn report_run_failure(err: Error, mode: ReportMode, format: Option<MessageFormat>) -> ! {
    if format != Some(MessageFormat::Json) {
        fail(err);
    }
    let streams = Streams::new(io::stdout(), false, io::stderr(), false, ColorChoice::Never);
    process::exit(crate::report_failure(err, mode, &streams));
}

fn failure_mode(cli: &Cli) -> ReportMode {
    if cli.print_config {
        ReportMode::PrintConfig
    } else if cli.stdin {
        ReportMode::Stdin
    } else if cli.list_files {
        ReportMode::ListFiles
    } else if cli.list_different {
        ReportMode::ListDifferent
    } else if cli.emit == Some(EmitArg::Stdout) {
        ReportMode::Preview
    } else if cli.check {
        ReportMode::Check
    } else {
        ReportMode::Write
    }
}

fn cargo_env(key: &str) -> Option<std::ffi::OsString> {
    crate::cargo_config::process_env(key)
}

/// The rustfmt options a configuration source named rather than the caller. A
/// key this rustfmt does not have is a typo when it was typed, and a
/// portability problem in a repository's own file -- so only the first is fatal.
fn lenient_keys(resolved: &Settings, overlay: &Settings) -> BTreeSet<String> {
    let typed_config = overlay.config.clone().unwrap_or_default();
    let typed_unset = overlay.unset_config.clone().unwrap_or_default();
    let mut keys: BTreeSet<String> = resolved
        .config
        .iter()
        .flat_map(BTreeMap::keys)
        .filter(|key| !typed_config.contains_key(*key))
        .cloned()
        .collect();
    keys.extend(
        resolved
            .unset_config
            .iter()
            .flatten()
            .filter(|key| !typed_unset.contains(*key))
            .cloned(),
    );
    keys
}

fn merge_command_line(resolved: Resolved, overlay: Settings) -> (Settings, Provenance) {
    let Resolved {
        mut settings,
        mut provenance,
        ..
    } = resolved;
    settings.merge_from(overlay, &Source::CommandLine, &mut provenance);
    (settings, provenance)
}

/// What only the command line can say about a run: which keys the caller named
/// itself, and whether the caller narrowed the selection.
struct RunContext {
    lenient: BTreeSet<String>,
    config_sources: Vec<String>,
    narrowed_by_user: bool,
    edition_typed: bool,
    style_edition_typed: bool,
}

/// Narrowing the selection is what makes an empty result a no-op rather than a
/// failure, and only the caller can narrow: a repository's own excludes describe
/// the tree, not this run.
const NARROWING_ARGS: [&str; 7] = [
    "includes",
    "excludes",
    "ignore_paths",
    "max_depth",
    "skip_toml",
    "rust_only",
    "toml_only",
];

fn validate(cli: &Cli, settings: &Settings) -> Result<(), Usage> {
    if cli.stdin && !cli.paths.is_empty() {
        return Err(Usage::StdinWithPath);
    }
    if cli.emit == Some(EmitArg::Stdout) && !cli.stdin && cli.paths.len() != 1 {
        return Err(Usage::PreviewNeedsOnePath);
    }
    if cli.watch && cli.emit == Some(EmitArg::Stdout) {
        return Err(Usage::WatchWithPreview);
    }
    if cli.restage && cli.emit == Some(EmitArg::Stdout) {
        return Err(Usage::RestageWithPreview);
    }
    if cli.emit == Some(EmitArg::Stdout) && (cli.list_files || cli.list_different) {
        return Err(Usage::PreviewWithListing);
    }
    if cli.full_versions && settings.languages == Some(Languages::Rust) {
        return Err(Usage::FullVersionsWithRustOnly);
    }
    if cli.registry_url.is_some() && is(settings.offline, false) {
        return Err(Usage::RegistryUrlWhileOffline);
    }
    if cli.recurse_submodules && cli.since.is_none() && !cli.staged {
        return Err(Usage::RecurseSubmodulesWithoutGitScope);
    }
    if settings.toml_version == Some(TomlVersion::V1_0)
        && settings.toml_inline_tables == Some(InlineTableStyle::Expand)
    {
        return Err(Usage::Toml10WithExpandedInlineTables);
    }
    if is(settings.sort_grouped, false) && settings.toml_max_blank_lines == Some(0) {
        return Err(Usage::SortGroupedWithoutBlankLines);
    }
    if !cli.ranges.is_empty() {
        if !cli.stdin && cli.paths.len() != 1 {
            return Err(Usage::RangeNeedsOnePath);
        }
        let named = if cli.stdin {
            cli.stdin_filepath.as_deref()
        } else {
            cli.paths.first().map(PathBuf::as_path)
        };
        if named.is_some_and(crate::detector::is_toml_path) {
            return Err(Usage::RangeOnToml);
        }
        if !cli.stdin && named.is_some_and(|path| !crate::detector::is_rust_path(path)) {
            return Err(Usage::RangeNeedsRustFile);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// clap only checks a conflict, a heading or a short it was given at parse
    /// time, so a `conflicts_with` naming a renamed argument is otherwise found
    /// by whoever first types the combination.
    #[test]
    fn the_command_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// Every `conflicts_with` the CLI declares, with an invocation that trips
    /// it. clap enforces these before `validate` ever runs, so they have their
    /// own table -- and `every_declared_conflict_is_covered` below fails if a
    /// new one is added without a row here.
    const CONFLICTS: &[(&str, &str, &[&str])] = &[
        (
            "config_file",
            "no_config",
            &["--config-file", "x.toml", "--no-config"],
        ),
        (
            "files_from",
            "since",
            &["--files-from", "list.txt", "--since", "HEAD"],
        ),
        (
            "files_from",
            "staged",
            &["--files-from", "list.txt", "--staged"],
        ),
        (
            "files_from",
            "stdin",
            &["--files-from", "list.txt", "--stdin"],
        ),
        ("since", "staged", &["--since", "HEAD", "--staged"]),
        ("since", "stdin", &["--since", "HEAD", "--stdin"]),
        ("staged", "stdin", &["--staged", "--stdin"]),
        ("restage", "check", &["--restage", "--staged", "--check"]),
        (
            "restage",
            "list_files",
            &["--restage", "--staged", "--list-files"],
        ),
        (
            "restage",
            "list_different",
            &["--restage", "--staged", "--list-different"],
        ),
        ("rust_only", "toml_only", &["--rust-only", "--toml-only"]),
        ("stdin", "list_files", &["--stdin", "--list-files"]),
        ("stdin", "list_different", &["--stdin", "--list-different"]),
        ("stdin", "print_config", &["--stdin", "--print-config"]),
        ("stdin", "print_settings", &["--stdin", "--print-settings"]),
        (
            "list_files",
            "list_different",
            &["--list-files", "--list-different"],
        ),
        ("emit", "check", &["--emit", "stdout", "--check"]),
        (
            "ranges",
            "files_from",
            &["--range", "1:2", "--files-from", "list.txt"],
        ),
        ("ranges", "since", &["--range", "1:2", "--since", "HEAD"]),
        ("ranges", "staged", &["--range", "1:2", "--staged"]),
        ("watch", "stdin", &["--watch", "--stdin"]),
        ("watch", "print_config", &["--watch", "--print-config"]),
        ("watch", "print_settings", &["--watch", "--print-settings"]),
        ("watch", "list_files", &["--watch", "--list-files"]),
        ("watch", "restage", &["--watch", "--restage", "--staged"]),
        ("watch", "ranges", &["--watch", "--range", "1:2"]),
        (
            "watch",
            "files_from",
            &["--watch", "--files-from", "list.txt"],
        ),
        ("watch", "since", &["--watch", "--since", "HEAD"]),
        ("watch", "staged", &["--watch", "--staged"]),
    ];

    #[test]
    fn every_declared_conflict_is_refused() {
        for (left, right, args) in CONFLICTS {
            let err = Cli::command()
                .try_get_matches_from(std::iter::once("rust-formatter").chain(args.iter().copied()))
                .expect_err(&format!("{left} + {right} was accepted: {args:?}"));
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{left} + {right}: {err}"
            );
        }
    }

    /// The table above is only as good as its coverage. Reading the conflicts
    /// back off the built `Command` means a new `conflicts_with` cannot be
    /// added without a row that actually trips it.
    #[test]
    fn every_declared_conflict_is_covered() {
        let command = Cli::command();
        let covered: std::collections::HashSet<(&str, &str)> = CONFLICTS
            .iter()
            .flat_map(|(left, right, _)| [(*left, *right), (*right, *left)])
            .collect();

        let mut missing = Vec::new();
        for arg in command.get_arguments() {
            let left = arg.get_id().as_str();
            for other in command.get_arg_conflicts_with(arg) {
                let right = other.get_id().as_str();
                if !covered.contains(&(left, right)) {
                    missing.push(format!("{left} + {right}"));
                }
            }
        }
        missing.sort_unstable();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "conflicts with no row in CONFLICTS: {missing:?}"
        );
    }

    /// `validate` against the settings the command line alone produces. The
    /// configuration sources are deliberately left out: a file or a metadata
    /// table can only ask for a subset of these combinations, and
    /// `tests/config_tests.rs` covers that subset. The rest are reachable by
    /// flag and by nothing else.
    fn check(args: &[&str]) -> Result<(), String> {
        let matches = Cli::command()
            .try_get_matches_from(std::iter::once("rust-formatter").chain(args.iter().copied()))
            .map_err(|err| format!("clap rejected the arguments: {err}"))?;
        let cli = Cli::from_arg_matches(&matches).map_err(|err| err.to_string())?;
        let settings = cli.overlay(&matches).map_err(|usage| usage.to_string())?;
        validate(&cli, &settings).map_err(|usage| usage.to_string())
    }

    #[track_caller]
    fn rejected(args: &[&str], expected: &str) {
        let message = check(args).expect_err(&format!("{args:?} was accepted"));
        assert_eq!(message, expected, "{args:?}");
    }

    #[track_caller]
    fn accepted(args: &[&str]) {
        assert_eq!(check(args), Ok(()), "{args:?}");
    }

    #[test]
    fn a_plain_run_is_accepted() {
        accepted(&[]);
        accepted(&["."]);
        accepted(&["src", "tests"]);
    }

    #[test]
    fn emit_stdout_needs_exactly_one_file_to_print() {
        let message = "--emit stdout needs --stdin or exactly one PATH";
        rejected(&["--emit", "stdout"], message);
        rejected(&["--emit", "stdout", "a.rs", "b.rs"], message);
        accepted(&["--emit", "stdout", "a.rs"]);
        accepted(&["--emit", "stdout", "--stdin", "--stdin-filepath", "a.rs"]);
    }

    #[test]
    fn stdin_takes_no_path() {
        let message = "--stdin formats standard input and takes no PATH; name the input with --stdin-filepath";
        rejected(&["--stdin", "x/Cargo.toml"], message);
        rejected(&["--stdin", "--stdin-filepath", "a.rs", "b.rs"], message);
        accepted(&["--stdin"]);
        accepted(&["--stdin", "--stdin-filepath", "x/Cargo.toml"]);
    }

    /// A watch cannot preview: `--emit stdout` prints one buffer and returns.
    /// Keyed on the value, so `--watch --emit files` -- the default, spelled
    /// out -- is still a watch.
    #[test]
    fn watch_cannot_preview() {
        let message =
            "--watch rewrites files as they change, so it cannot be combined with --emit stdout";
        rejected(&["--watch", "--emit", "stdout", "a.rs"], message);
        accepted(&["--watch", "--emit", "files"]);
        accepted(&["--watch"]);
        accepted(&["--watch", "--check"]);
        accepted(&["--watch", "--list-different"]);
    }

    /// A subcommand name is a subcommand only until the first path. `--` is not
    /// the escape from that -- it opens the rustfmt pass-through.
    #[test]
    fn a_subcommand_name_is_a_subcommand_only_as_the_first_path() {
        let parse = |args: &[&str]| {
            let matches = Cli::command()
                .try_get_matches_from(std::iter::once("rust-formatter").chain(args.iter().copied()))
                .expect("clap accepted the arguments");
            Cli::from_arg_matches(&matches).expect("cli")
        };

        let subcommand = parse(&["completions", "bash"]);
        assert!(subcommand.command.is_some());
        assert!(subcommand.paths.is_empty());

        for spelling in ["./completions", "completions/"] {
            let path = parse(&[spelling]);
            assert!(path.command.is_none(), "{spelling}");
            assert_eq!(path.paths, vec![PathBuf::from(spelling)], "{spelling}");
        }

        let after_a_path = parse(&["src", "completions"]);
        assert!(after_a_path.command.is_none());
        assert_eq!(after_a_path.paths.len(), 2);

        let passed_through = parse(&["--", "completions"]);
        assert!(passed_through.command.is_none());
        assert_eq!(passed_through.extra_args, vec!["completions".to_string()]);
    }

    /// The sharp edge of the rule above, pinned rather than left to be
    /// discovered: a flag leaves the parser between positionals, so the next
    /// word is still read as a subcommand.
    #[test]
    fn a_flag_before_a_subcommand_name_still_finds_the_subcommand() {
        let err = Cli::command()
            .try_get_matches_from(["rust-formatter", "--check", "completions"])
            .expect_err("`completions` is taken as the subcommand");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// The man page set is derived from these names, and `help` must not be
    /// among them: it would shadow a directory called `help` and earn a page.
    #[test]
    fn the_command_names_are_stable() {
        let command = Cli::command();
        let names: Vec<&str> = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(names, ["completions", "man", "hook"]);

        let hook = command
            .find_subcommand("hook")
            .expect("the hook subcommand");
        let actions: Vec<&str> = hook
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(actions, ["install", "uninstall", "status"]);
    }

    /// `--emit files` is the default and constrains nothing, so every rule
    /// above has to be keyed on the value and not merely on the flag.
    #[test]
    fn emit_files_constrains_nothing() {
        accepted(&["--emit", "files"]);
        accepted(&["--emit", "files", "a.rs", "b.rs"]);
        accepted(&["--emit", "files", "--list-different"]);
        accepted(&["--emit", "files", "--restage", "--staged"]);
    }

    #[test]
    fn restage_has_nothing_to_stage_under_emit_stdout() {
        rejected(
            &["--emit", "stdout", "--restage", "--staged", "a.rs"],
            "--restage rewrites files, so it cannot be combined with --emit stdout",
        );
    }

    #[test]
    fn a_preview_is_not_a_listing() {
        let message =
            "--emit stdout prints a formatted file, so it cannot be combined with a listing";
        rejected(&["--emit", "stdout", "--list-files", "a.rs"], message);
        rejected(&["--emit", "stdout", "--list-different", "a.rs"], message);
    }

    #[test]
    fn full_versions_cannot_be_asked_to_skip_the_manifest() {
        rejected(
            &["--full-versions", "--rust-only"],
            "--full-versions rewrites Cargo.toml, which --rust-only excludes",
        );
        accepted(&["--full-versions", "--toml-only"]);
        accepted(&["--full-versions"]);
    }

    #[test]
    fn a_registry_url_cannot_be_reached_offline() {
        rejected(
            &[
                "--full-versions",
                "--registry-url",
                "https://example.test",
                "--offline",
            ],
            "--registry-url names an index to fetch from, which --offline forbids",
        );
        accepted(&["--full-versions", "--registry-url", "https://example.test"]);
    }

    #[test]
    fn recursing_submodules_needs_a_git_scope() {
        let message =
            "--recurse-submodules narrows --since or --staged, which is what asks git for a scope";
        rejected(&["--recurse-submodules"], message);
        accepted(&["--recurse-submodules", "--staged"]);
        accepted(&["--recurse-submodules", "--since", "HEAD"]);
    }

    #[test]
    fn toml_1_0_cannot_expand_an_inline_table() {
        rejected(
            &["--toml-version", "1.0", "--toml-inline-tables", "expand"],
            "--toml-version 1.0 cannot be combined with --toml-inline-tables expand",
        );
        accepted(&["--toml-version", "1.0", "--toml-inline-tables", "compact"]);
        accepted(&["--toml-version", "1.0", "--toml-inline-tables", "auto"]);
        accepted(&["--toml-version", "1.1", "--toml-inline-tables", "expand"]);
    }

    #[test]
    fn grouping_needs_a_blank_line_to_read() {
        rejected(
            &["--sort-grouped", "--toml-max-blank-lines", "0"],
            "--sort-grouped cannot be combined with --toml-max-blank-lines 0",
        );
        accepted(&["--sort-grouped", "--toml-max-blank-lines", "1"]);
        accepted(&["--toml-max-blank-lines", "0"]);
    }

    #[test]
    fn a_range_names_lines_of_one_file() {
        let message = "--range needs --stdin or exactly one PATH";
        rejected(&["--range", "1:2"], message);
        rejected(&["--range", "1:2", "a.rs", "b.rs"], message);
        accepted(&["--range", "1:2", "a.rs"]);
        accepted(&["--range", "1:2", "--stdin", "--stdin-filepath", "a.rs"]);
    }

    /// A range is lines of one file. Applying the same span to every file in a
    /// directory is not that, so a directory (or any non-`.rs` path) is refused
    /// by name the way TOML is.
    #[test]
    fn a_range_over_a_directory_is_refused() {
        let message = "--range needs a .rs file or --stdin";
        rejected(&["--range", "1:2", "."], message);
        rejected(&["--range", "1:2", "src"], message);
        rejected(&["--range", "1:2", "lib.rs.bak"], message);
        accepted(&["--range", "1:2", "src/lib.rs"]);
        accepted(&["--range", "1:2", "--stdin"]);
    }

    /// The TOML formatter rewrites a whole document, so a line range over one
    /// has no meaning and must be refused by name rather than silently ignored.
    #[test]
    fn a_range_over_toml_is_refused_by_name() {
        let message = "--range formats Rust only; TOML is formatted as a whole document";
        rejected(&["--range", "1:2", "Cargo.toml"], message);
        rejected(
            &[
                "--range",
                "1:2",
                "--stdin",
                "--stdin-filepath",
                "Cargo.toml",
            ],
            message,
        );
    }

    /// A `--range` run with `--stdin` and no `--stdin-filepath` names no file
    /// at all, so the language cannot be read off a path; rustfmt is the right
    /// default and the run must not be refused here.
    #[test]
    fn a_range_over_unnamed_stdin_is_accepted() {
        accepted(&["--range", "1:2", "--stdin"]);
    }

    #[test]
    fn a_since_ref_may_look_like_a_flag() {
        accepted(&["--since", "--all"]);
        accepted(&["--since=-bar"]);
    }

    /// A pinned toolchain reaches `validate` as a plain string and constrains
    /// nothing, but it is the one CLI value the suites never passed at all.
    #[test]
    fn a_pinned_toolchain_is_accepted_everywhere() {
        accepted(&["--toolchain", "nightly"]);
        accepted(&["--toolchain", "stable", "--check"]);
        accepted(&["--toolchain", "1.85", "--toml-only"]);
    }

    #[test]
    fn every_colour_choice_is_accepted() {
        for choice in ["auto", "always", "never"] {
            accepted(&["--color", choice]);
            accepted(&["--color", choice, "--check", "."]);
        }
    }
}
