use std::{
    collections::BTreeMap,
    fmt,
    num::NonZeroUsize,
    ops::RangeInclusive,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    cargo_config::EnvLookup,
    config::RustStyle,
    detector,
    error::{Error, Result},
    output::{ColorChoice, MessageFormat},
    selection::Languages,
    toml_style::{
        ArrayStyle, IndentSpec, InlineTableStyle, PackageOrder, Spacing, TomlVersion, TrailingComma,
    },
};

/// Every setting also reads one environment variable, named from the setting.
pub const ENV_PREFIX: &str = "RUST_FORMATTER_";

/// The names a standalone configuration file may have, in discovery order.
pub const CONFIG_NAMES: [&str; 2] = ["rust-formatter.toml", ".rust-formatter.toml"];

/// The table a manifest carries settings under.
const METADATA_KEY: &str = "rust-formatter";

/// Settings that name other settings rather than carrying a value, so an
/// environment variable for them would have nothing to say.
const NO_ENV: [&str; 1] = ["presets"];

const RUSTUP_TOOLCHAIN: &str = "RUSTUP_TOOLCHAIN";
const RUSTUP_TOOLCHAIN_SOURCE: &str = "RUSTUP_TOOLCHAIN_SOURCE";
const EXPLICIT_RUSTUP_TOOLCHAIN_SOURCES: [&str; 2] = ["cli", "env"];

pub const TOML_TAB_WIDTH: RangeInclusive<i64> = 1..=16;
pub const TOML_MAX_WIDTH: RangeInclusive<i64> = 1..=4096;
pub const TOML_MAX_BLANK_LINES: RangeInclusive<i64> = 0..=32;
pub const DIFF_CONTEXT: RangeInclusive<i64> = 0..=4096;

struct OutOfRange {
    field: &'static str,
    value: i64,
    allowed: RangeInclusive<i64>,
}

impl fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not in {}..={}",
            self.value,
            self.allowed.start(),
            self.allowed.end()
        )
    }
}

/// The environment variable a setting reads, derived from its own name so the
/// two can never drift.
pub fn env_var(field: &str) -> String {
    format!("{ENV_PREFIX}{}", field.to_uppercase())
}

/// The configuration-file spelling of a setting: its command-line name without
/// the dashes.
pub fn key(field: &str) -> String {
    field.replace('_', "-")
}

/// A boolean as a configuration file or an environment variable may spell it.
/// The command line accepts `1`, `yes` and `off` for these too, so the two
/// surfaces agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Toggle(pub bool);

impl From<Toggle> for bool {
    fn from(toggle: Toggle) -> Self {
        toggle.0
    }
}

impl From<bool> for Toggle {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

impl<'de> Deserialize<'de> for Toggle {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = Toggle;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a boolean, or one of `true`, `false`, `yes`, `no`, `on`, `off`, `1`, `0`",
                )
            }

            fn visit_bool<E: serde::de::Error>(
                self,
                value: bool,
            ) -> std::result::Result<Toggle, E> {
                Ok(Toggle(value))
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> std::result::Result<Toggle, E> {
                match value {
                    0 => Ok(Toggle(false)),
                    1 => Ok(Toggle(true)),
                    other => Err(E::custom(format!("expected a boolean, found `{other}`"))),
                }
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> std::result::Result<Toggle, E> {
                u64::try_from(value)
                    .map_err(|_| E::custom(format!("expected a boolean, found `{value}`")))
                    .and_then(|value| self.visit_u64(value))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> std::result::Result<Toggle, E> {
                match value.to_ascii_lowercase().as_str() {
                    "true" | "yes" | "on" | "1" => Ok(Toggle(true)),
                    "false" | "no" | "off" | "0" => Ok(Toggle(false)),
                    other => Err(E::custom(format!("expected a boolean, found `{other}`"))),
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// One rustfmt option as a configuration file writes it. rustfmt's own command
/// line takes every value as text, so each spelling is rendered back to the
/// string that `--config` would have carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigValue {
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    List(Vec<ConfigValue>),
}

impl fmt::Display for ConfigValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(value) => write!(f, "{value}"),
            Self::Integer(value) => write!(f, "{value}"),
            Self::Float(value) => write!(f, "{value}"),
            Self::String(value) => f.write_str(value),
            Self::List(values) => {
                f.write_str("[")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    match value {
                        Self::String(text) => write!(f, "{text:?}")?,
                        other => write!(f, "{other}")?,
                    }
                }
                f.write_str("]")
            }
        }
    }
}

/// The layer a setting's value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Default,
    Preset(String),
    WorkspaceMetadata(PathBuf),
    PackageMetadata(PathBuf),
    File(PathBuf),
    /// The `rust-toolchain.toml` rustup and cargo already read. It sets one
    /// key and ranks below every source this tool owns, so naming a toolchain
    /// in a `rust-formatter.toml` still wins.
    ToolchainFile(PathBuf),
    Environment(String),
    CommandLine,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Preset(name) => write!(f, "preset {name}"),
            Self::WorkspaceMetadata(path) => {
                write!(
                    f,
                    "[workspace.metadata.rust-formatter] in {}",
                    path.display()
                )
            }
            Self::PackageMetadata(path) => {
                write!(f, "[package.metadata.rust-formatter] in {}", path.display())
            }
            Self::File(path) | Self::ToolchainFile(path) => write!(f, "{}", path.display()),
            Self::Environment(name) => write!(f, "${name}"),
            Self::CommandLine => f.write_str("command line"),
        }
    }
}

/// Which layer last set each setting, keyed by the setting's field name.
#[derive(Debug, Clone, Default)]
pub struct Provenance(BTreeMap<&'static str, Source>);

impl Provenance {
    pub fn set(&mut self, field: &'static str, source: Source) {
        self.0.insert(field, source);
    }

    pub fn get(&self, field: &str) -> Option<&Source> {
        self.0.get(field)
    }

    /// The source of a setting nothing above the defaults touched.
    pub fn source_of(&self, field: &str) -> Source {
        self.get(field).cloned().unwrap_or(Source::Default)
    }
}

/// Generates the settings struct together with everything that has to walk its
/// fields, so a new setting cannot be added without its merge, its environment
/// variable and its provenance entry coming with it.
macro_rules! settings {
    ($($field:ident : $ty:ty),* $(,)?) => {
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "kebab-case")]
        pub struct Settings {
            $(
                #[serde(default, skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )*
        }

        impl Settings {
            pub const FIELDS: &'static [&'static str] = &[$(stringify!($field)),*];

            fn merge_replacing(
                &mut self,
                higher: Settings,
                source_of: &dyn Fn(&'static str) -> Source,
                provenance: &mut Provenance,
            ) {
                $(
                    if higher.$field.is_some() {
                        self.$field = higher.$field;
                        provenance.set(stringify!($field), source_of(stringify!($field)));
                    }
                )*
            }

            fn set_fields(&self) -> Vec<&'static str> {
                let mut fields = Vec::new();
                $(
                    if self.$field.is_some() {
                        fields.push(stringify!($field));
                    }
                )*
                fields
            }

            fn from_env(env: EnvLookup<'_>) -> Result<Self> {
                let mut out = Self::default();
                $(
                    if !NO_ENV.contains(&stringify!($field)) {
                        let name = env_var(stringify!($field));
                        if let Some(raw) = env(&name) {
                            let raw = raw.to_string_lossy().into_owned();
                            out.$field = Some(parse_env_value(&name, &raw)?);
                        }
                    }
                )*
                if let Some(found) = out.out_of_range() {
                    return Err(Error::Config {
                        path: PathBuf::from(format!("${}", env_var(found.field))),
                        message: found.to_string(),
                    });
                }
                Ok(out)
            }
        }
    };
}

settings! {
    preset: Vec<String>,
    presets: BTreeMap<String, Settings>,
    toolchain: String,
    edition: String,
    style_edition: String,
    rust_style: Vec<RustStyle>,
    config: BTreeMap<String, ConfigValue>,
    unset_config: Vec<String>,
    no_all: Toggle,
    include: Vec<String>,
    exclude: Vec<String>,
    ignore_path: Vec<PathBuf>,
    max_depth: usize,
    hidden: Toggle,
    no_ignore: Toggle,
    skip_toml: Vec<String>,
    no_default_toml_skips: Toggle,
    languages: Languages,
    toml_indent: IndentSpec,
    toml_tab_width: u8,
    toml_max_width: u16,
    toml_arrays: ArrayStyle,
    toml_inline_tables: InlineTableStyle,
    toml_version: TomlVersion,
    toml_trailing_comma: TrailingComma,
    toml_blank_line_before_tables: Toggle,
    toml_max_blank_lines: u8,
    toml_array_spacing: Spacing,
    toml_inline_table_spacing: Spacing,
    toml_align_entries: Toggle,
    toml_align_comments: Toggle,
    toml_indent_tables: Toggle,
    toml_indent_entries: Toggle,
    toml_normalize_keys: Toggle,
    toml_directives: Toggle,
    sort_deps: Toggle,
    sort_package: Toggle,
    package_order: PackageOrder,
    sort_dep_fields: Toggle,
    sort_features: Toggle,
    sort_arrays: Toggle,
    sort_targets: Toggle,
    sort_tables: Toggle,
    sort_keys: Toggle,
    sort_grouped: Toggle,
    cargo_conventions: Toggle,
    jobs: NonZeroUsize,
    fail_fast: Toggle,
    offline: Toggle,
    cache: Toggle,
    color: ColorChoice,
    message_format: MessageFormat,
    diff_context: u16,
    verbose: Toggle,
    quiet: Toggle,
}

impl Settings {
    pub fn merge_from(&mut self, higher: Settings, source: &Source, provenance: &mut Provenance) {
        self.merge_layer(higher, &|_| source.clone(), provenance);
    }

    fn merge_env(&mut self, higher: Settings, provenance: &mut Provenance) {
        self.merge_layer(
            higher,
            &|field| Source::Environment(env_var(field)),
            provenance,
        );
    }

    fn env_sources(&self) -> impl Iterator<Item = Source> {
        self.set_fields()
            .into_iter()
            .map(|field| Source::Environment(env_var(field)))
    }

    fn merge_layer(
        &mut self,
        mut higher: Settings,
        source_of: &dyn Fn(&'static str) -> Source,
        provenance: &mut Provenance,
    ) {
        if let Some(config) = higher.config.take() {
            let merged = self.config.get_or_insert_with(BTreeMap::new);
            merged.extend(config);
            provenance.set("config", source_of("config"));
        }
        if let Some(presets) = higher.presets.take() {
            let merged = self.presets.get_or_insert_with(BTreeMap::new);
            merged.extend(presets);
            provenance.set("presets", source_of("presets"));
        }
        macro_rules! append {
            ($field:ident) => {
                if let Some(values) = higher.$field.take() {
                    self.$field.get_or_insert_with(Vec::new).extend(values);
                    provenance.set(stringify!($field), source_of(stringify!($field)));
                }
            };
        }
        append!(rust_style);
        append!(unset_config);
        append!(include);
        append!(exclude);
        append!(ignore_path);
        append!(skip_toml);

        self.merge_replacing(higher, source_of, provenance);
    }

    /// The TOML style these settings resolve to. Everything they leave unset
    /// keeps [`TomlStyle::default`], which is the one place those defaults live.
    pub fn toml_style(&self) -> crate::toml_style::TomlStyle {
        use crate::toml_style::TomlStyle;

        let defaults = TomlStyle::default();
        let tab_width = usize::from(self.toml_tab_width.unwrap_or(4));
        TomlStyle {
            indent: self.toml_indent.unwrap_or_default().resolve(tab_width),
            max_width: self.toml_max_width.map_or(defaults.max_width, usize::from),
            arrays: self.toml_arrays.unwrap_or(defaults.arrays),
            inline_tables: self.toml_inline_tables.unwrap_or(defaults.inline_tables),
            toml_version: self.toml_version.unwrap_or(defaults.toml_version),
            trailing_comma: self.toml_trailing_comma.unwrap_or(defaults.trailing_comma),
            directives: is(self.toml_directives, defaults.directives),
            blank_line_before_tables: is(
                self.toml_blank_line_before_tables,
                defaults.blank_line_before_tables,
            ),
            max_blank_lines: self
                .toml_max_blank_lines
                .map_or(defaults.max_blank_lines, usize::from),
            array_spacing: self.toml_array_spacing.unwrap_or(defaults.array_spacing),
            inline_table_spacing: self
                .toml_inline_table_spacing
                .unwrap_or(defaults.inline_table_spacing),
            align_entries: is(self.toml_align_entries, defaults.align_entries),
            align_comments: is(self.toml_align_comments, defaults.align_comments),
            indent_tables: is(self.toml_indent_tables, defaults.indent_tables),
            indent_entries: is(self.toml_indent_entries, defaults.indent_entries),
            normalize_keys: is(self.toml_normalize_keys, defaults.normalize_keys),
            sort_deps: is(self.sort_deps, defaults.sort_deps),
            sort_package: is(self.sort_package, defaults.sort_package),
            sort_dep_fields: is(self.sort_dep_fields, defaults.sort_dep_fields),
            sort_features: is(self.sort_features, defaults.sort_features),
            sort_arrays: is(self.sort_arrays, defaults.sort_arrays),
            sort_targets: is(self.sort_targets, defaults.sort_targets),
            sort_tables: is(self.sort_tables, defaults.sort_tables),
            sort_keys: is(self.sort_keys, defaults.sort_keys),
            sort_grouped: is(self.sort_grouped, defaults.sort_grouped),
            package_order: self.package_order.unwrap_or(defaults.package_order),
            cargo_conventions: is(self.cargo_conventions, defaults.cargo_conventions),
        }
    }

    fn out_of_range(&self) -> Option<OutOfRange> {
        let ranged = [
            (
                "toml_tab_width",
                self.toml_tab_width.map(i64::from),
                TOML_TAB_WIDTH,
            ),
            (
                "toml_max_width",
                self.toml_max_width.map(i64::from),
                TOML_MAX_WIDTH,
            ),
            (
                "toml_max_blank_lines",
                self.toml_max_blank_lines.map(i64::from),
                TOML_MAX_BLANK_LINES,
            ),
            (
                "diff_context",
                self.diff_context.map(i64::from),
                DIFF_CONTEXT,
            ),
        ];
        ranged.into_iter().find_map(|(field, value, allowed)| {
            let value = value.filter(|value| !allowed.contains(value))?;
            Some(OutOfRange {
                field,
                value,
                allowed,
            })
        })
    }

    fn range_violation(&self) -> Option<String> {
        let describe = |found: OutOfRange| format!("{}: {found}", key(found.field));
        self.out_of_range().map(describe).or_else(|| {
            self.presets.iter().flatten().find_map(|(name, preset)| {
                preset
                    .out_of_range()
                    .map(|found| format!("preset `{name}`: {}", describe(found)))
            })
        })
    }

    /// A preset that named other presets would need a cycle check to mean
    /// anything, so it is refused instead.
    fn reject_nesting(&self, path: &Path, name: &str) -> Result<()> {
        if self.preset.is_some() || self.presets.is_some() {
            return Err(Error::Config {
                path: path.to_path_buf(),
                message: format!("preset `{name}` names another preset, which is not allowed"),
            });
        }
        Ok(())
    }
}

fn is(value: Option<Toggle>, fallback: bool) -> bool {
    value.map_or(fallback, bool::from)
}

/// An environment variable's value, read as TOML and then as the bare string it
/// looks like. `RUST_FORMATTER_EXCLUDE='["vendor"]'` is an array,
/// `RUST_FORMATTER_EDITION=2024` is the string `2024` rather than the integer.
pub fn env_message_format(env: EnvLookup<'_>) -> Option<MessageFormat> {
    let name = env_var("message_format");
    let raw = env(&name)?;
    parse_env_value(&name, &raw.to_string_lossy()).ok()
}

fn parse_env_value<T: serde::de::DeserializeOwned>(name: &str, raw: &str) -> Result<T> {
    #[derive(Deserialize)]
    struct Wrap<T> {
        value: T,
    }

    let trimmed = raw.trim();
    let quoted = toml_edit::Value::from(trimmed).to_string();
    for candidate in [trimmed.to_string(), quoted] {
        if let Ok(wrapped) = toml_edit::de::from_str::<Wrap<T>>(&format!("value = {candidate}")) {
            return Ok(wrapped.value);
        }
    }
    Err(Error::Config {
        path: PathBuf::from(format!("${name}")),
        message: format!("`{raw}` is not a valid value for this setting"),
    })
}

fn deserialize_file(path: &Path, source: &str) -> Result<Settings> {
    let settings = toml_edit::de::from_str::<Settings>(source)
        .map_err(|err| config_error(path, source, &err))?;
    match settings.range_violation() {
        Some(message) => Err(Error::Config {
            path: path.to_path_buf(),
            message,
        }),
        None => Ok(settings),
    }
}

/// A metadata table, deserialized without a position: `DocumentMut` has already
/// dropped its spans, so any offset reported here would point into a rendering
/// of the table rather than into the manifest. The table's name locates it
/// instead.
fn deserialize_table(path: &Path, label: &str, table: &toml_edit::Table) -> Result<Settings> {
    let mut doc = toml_edit::DocumentMut::new();
    *doc.as_table_mut() = table.clone();
    let table_error = |message: String| Error::Config {
        path: path.to_path_buf(),
        message: format!("{label}: {message}"),
    };
    let settings = toml_edit::de::from_document::<Settings>(doc)
        .map_err(|err| table_error(shorten(err.message())))?;
    match settings.range_violation() {
        Some(message) => Err(table_error(message)),
        None => Ok(settings),
    }
}

fn config_error(path: &Path, source: &str, err: &toml_edit::de::Error) -> Error {
    let position = err
        .span()
        .and_then(|span| crate::error::line_column(source, span.start));
    let message = match position {
        Some((line, column)) => format!("{line}:{column}: {}", shorten(err.message())),
        None => shorten(err.message()),
    };
    Error::Config {
        path: path.to_path_buf(),
        message,
    }
}

/// serde answers an unknown key with all fifty-odd it would have accepted, which
/// buries the one that was meant. Name the nearest instead.
fn shorten(message: &str) -> String {
    let Some(rest) = message.strip_prefix("unknown field `") else {
        return message.to_string();
    };
    let Some((found, _)) = rest.split_once('`') else {
        return message.to_string();
    };
    match nearest(found) {
        Some(suggestion) => format!("unknown setting `{found}`; did you mean `{suggestion}`?"),
        None => format!(
            "unknown setting `{found}`. Run `rust-formatter --print-settings` to see the settings there are."
        ),
    }
}

/// The closest known setting, when one is close enough to be a typo rather than
/// a different word.
fn nearest(found: &str) -> Option<String> {
    let budget = 1 + found.len() / 4;
    Settings::FIELDS
        .iter()
        .map(|field| key(field))
        .map(|candidate| (distance(found, &candidate), candidate))
        .filter(|(distance, _)| *distance <= budget)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

fn distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut row: Vec<usize> = (0..=right.len()).collect();
    for (i, a) in left.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, b) in right.iter().enumerate() {
            let next = if a == *b {
                diagonal
            } else {
                1 + diagonal.min(row[j]).min(row[j + 1])
            };
            diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[right.len()]
}

/// The settings a named preset stands for. Every preset states its values
/// outright, so a change to a default cannot move one.
pub fn builtin_preset(name: &str) -> Option<Settings> {
    let on = || Some(Toggle(true));
    let mut settings = Settings::default();
    match name {
        "default" => {}
        "compact" => settings.toml_inline_tables = Some(InlineTableStyle::Compact),
        "expand" => settings.toml_inline_tables = Some(InlineTableStyle::Expand),
        "everything" => {
            settings.toml_arrays = Some(ArrayStyle::Auto);
            settings.toml_trailing_comma = Some(TrailingComma::Multiline);
            settings.toml_blank_line_before_tables = on();
            settings.toml_normalize_keys = on();
            settings.sort_deps = on();
            settings.sort_package = on();
            settings.sort_dep_fields = on();
            settings.sort_features = on();
            settings.sort_tables = on();
            settings.sort_keys = on();
            settings.sort_grouped = on();
        }
        "cargo" | "style-guide" => {
            settings.toml_indent = Some(IndentSpec::Spaces(4));
            settings.toml_max_width = Some(100);
            settings.toml_arrays = Some(ArrayStyle::Auto);
            settings.toml_inline_tables = Some(InlineTableStyle::Section);
            settings.toml_trailing_comma = Some(TrailingComma::Multiline);
            settings.toml_max_blank_lines = Some(0);
            settings.toml_blank_line_before_tables = on();
            settings.toml_normalize_keys = on();
            settings.sort_deps = on();
            settings.sort_package = on();
            settings.package_order = Some(PackageOrder::StyleGuide);
            settings.sort_features = on();
            settings.sort_arrays = on();
            settings.sort_targets = on();
            settings.sort_tables = on();
            settings.sort_keys = on();
        }
        "narrow-tabs" => {
            settings.toml_indent = Some(IndentSpec::Tab);
            settings.toml_max_width = Some(60);
        }
        "aligned-indented" => {
            settings.toml_arrays = Some(ArrayStyle::Expand);
            settings.toml_array_spacing = Some(Spacing::Spaced);
            settings.toml_inline_table_spacing = Some(Spacing::Compact);
            settings.toml_max_blank_lines = Some(2);
            settings.toml_align_entries = on();
            settings.toml_align_comments = on();
            settings.toml_indent_tables = on();
            settings.toml_indent_entries = on();
        }
        "comments" => settings.rust_style = Some(vec![RustStyle::Comments]),
        "literals" => settings.rust_style = Some(vec![RustStyle::Literals]),
        "strict" => settings.rust_style = Some(vec![RustStyle::Strict]),
        _ => return None,
    }
    Some(settings)
}

/// Every built-in preset name, `style-guide` included as the alias of `cargo`.
pub const BUILTIN_PRESETS: &[&str] = &[
    "default",
    "compact",
    "expand",
    "everything",
    "cargo",
    "style-guide",
    "narrow-tabs",
    "aligned-indented",
    "comments",
    "literals",
    "strict",
];

/// The command-line half of the resolution, which no configuration source can
/// set because each entry names where settings come from.
#[derive(Debug, Clone, Default)]
pub struct SettingsCli {
    pub preset: Vec<String>,
    pub config_file: Option<PathBuf>,
    pub no_config: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Resolved {
    pub settings: Settings,
    pub provenance: Provenance,
    /// The layers that were actually read, in the order they applied.
    pub sources: Vec<Source>,
}

/// One configuration layer before it is merged.
struct Layer {
    source: Source,
    settings: Settings,
}

/// One step of the resolution, in the order it applies.
enum Step {
    Preset(String),
    Layer(Layer),
    Environment(Settings),
}

/// Resolve every configuration source for a run rooted at `start`.
///
/// Layers apply lowest first: the manifest's workspace and package metadata
/// tables, a discovered configuration file, `--config-file`, then the
/// environment. A preset applies immediately below the layer that named it, so
/// naming one never discards the keys that layer also set, and `--preset` on the
/// command line outranks every file. The command line itself is applied by the
/// caller, which is the only place that knows what was actually typed.
pub fn resolve(start: &Path, env: EnvLookup<'_>, cli: &SettingsCli) -> Result<Resolved> {
    // A relative start has no ancestors to walk, so `.` inside a workspace member
    // would never reach the workspace manifest above it.
    let start = detector::canonical_dir(start);
    let start = start.as_path();

    let mut layers = Vec::new();
    if !cli.no_config {
        layers.extend(toolchain_layer(start, env));
        layers.extend(manifest_layers(start)?);
        if let Some(found) = discover(start) {
            layers.push(read_file(&found)?);
        }
    }
    if let Some(path) = &cli.config_file {
        layers.push(read_file(path)?);
    }
    let from_env = Settings::from_env(env)?;

    let defined = collect_presets(&layers, &from_env)?;

    let mut steps = Vec::new();
    for layer in layers {
        for name in layer.settings.preset.clone().unwrap_or_default() {
            steps.push(Step::Preset(name));
        }
        steps.push(Step::Layer(layer));
    }
    for name in from_env.preset.clone().unwrap_or_default() {
        steps.push(Step::Preset(name));
    }
    steps.push(Step::Environment(from_env));
    for name in &cli.preset {
        steps.push(Step::Preset(name.clone()));
    }

    let mut settings = Settings::default();
    let mut provenance = Provenance::default();
    let mut sources = Vec::new();
    let mut applied = Vec::new();

    for step in steps {
        match step {
            Step::Preset(name) => {
                let preset = preset_settings(&name, &defined)?;
                let source = Source::Preset(name.clone());
                settings.merge_from(preset, &source, &mut provenance);
                sources.push(source);
                applied.push(name);
            }
            Step::Layer(layer) => {
                settings.merge_from(strip(layer.settings), &layer.source, &mut provenance);
                sources.push(layer.source);
            }
            Step::Environment(from_env) => {
                let from_env = strip(from_env);
                sources.extend(from_env.env_sources());
                settings.merge_env(from_env, &mut provenance);
            }
        }
    }

    settings.preset = Some(applied);
    settings.presets = None;
    Ok(Resolved {
        settings,
        provenance,
        sources,
    })
}

/// A layer with the two keys that name other settings removed, so they cannot
/// also be merged as ordinary values.
fn strip(mut settings: Settings) -> Settings {
    settings.preset = None;
    settings.presets = None;
    settings
}

fn preset_settings(name: &str, defined: &BTreeMap<String, Settings>) -> Result<Settings> {
    if let Some(preset) = builtin_preset(name) {
        return Ok(preset);
    }
    match defined.get(name) {
        Some(preset) => Ok(preset.clone()),
        None => Err(Error::UnknownPreset {
            name: name.to_string(),
            known: known_presets(defined),
        }),
    }
}

/// Every `[presets]` table, gathered before any layer is merged so a preset
/// named by a lower layer can still be defined by a higher one.
fn collect_presets(layers: &[Layer], from_env: &Settings) -> Result<BTreeMap<String, Settings>> {
    let mut defined: BTreeMap<String, Settings> = BTreeMap::new();
    let sources = layers
        .iter()
        .map(|layer| (&layer.source, &layer.settings))
        .chain(std::iter::once((&Source::CommandLine, from_env)));
    for (source, settings) in sources {
        let Some(presets) = &settings.presets else {
            continue;
        };
        for (name, body) in presets {
            if BUILTIN_PRESETS.contains(&name.as_str()) {
                return Err(Error::Config {
                    path: source_path(source),
                    message: format!("`{name}` is a built-in preset and cannot be redefined"),
                });
            }
            body.reject_nesting(&source_path(source), name)?;
            defined.insert(name.clone(), body.clone());
        }
    }
    Ok(defined)
}

fn source_path(source: &Source) -> PathBuf {
    match source {
        Source::WorkspaceMetadata(path)
        | Source::PackageMetadata(path)
        | Source::File(path)
        | Source::ToolchainFile(path) => path.clone(),
        other => PathBuf::from(other.to_string()),
    }
}

fn known_presets(defined: &BTreeMap<String, Settings>) -> String {
    let mut names: Vec<&str> = BUILTIN_PRESETS.to_vec();
    names.extend(defined.keys().map(String::as_str));
    names.sort_unstable();
    names.dedup();
    names.join(", ")
}

fn toolchain_layer(start: &Path, env: EnvLookup<'_>) -> Option<Layer> {
    let pin = crate::toolchain_file::discover(start)?;
    let (source, toolchain) = match explicit_rustup_toolchain(env) {
        Some(chosen) => (Source::Environment(RUSTUP_TOOLCHAIN.to_string()), chosen),
        None => (Source::ToolchainFile(pin.path), pin.channel),
    };
    Some(Layer {
        source,
        settings: Settings {
            toolchain: Some(toolchain),
            ..Settings::default()
        },
    })
}

fn explicit_rustup_toolchain(env: EnvLookup<'_>) -> Option<String> {
    let toolchain = env(RUSTUP_TOOLCHAIN)?
        .into_string()
        .ok()
        .filter(|toolchain| !toolchain.is_empty())?;
    let chosen_explicitly = env(RUSTUP_TOOLCHAIN_SOURCE).is_none_or(|source| {
        EXPLICIT_RUSTUP_TOOLCHAIN_SOURCES
            .iter()
            .any(|explicit| source == *explicit)
    });
    chosen_explicitly.then_some(toolchain)
}

fn manifest_layers(start: &Path) -> Result<Vec<Layer>> {
    let Some(manifest) = detector::find_cargo_manifest(start) else {
        return Ok(Vec::new());
    };
    let workspace = detector::workspace_root(&manifest).join("Cargo.toml");

    let mut layers = Vec::new();
    if let Some(settings) = metadata_table(&workspace, &["workspace", "metadata"])? {
        layers.push(Layer {
            source: Source::WorkspaceMetadata(workspace.clone()),
            settings,
        });
    }
    if let Some(settings) = metadata_table(&manifest, &["package", "metadata"])? {
        layers.push(Layer {
            source: Source::PackageMetadata(manifest),
            settings,
        });
    }
    Ok(layers)
}

fn metadata_table(manifest: &Path, path: &[&str]) -> Result<Option<Settings>> {
    let Ok(source) = std::fs::read_to_string(manifest) else {
        return Ok(None);
    };
    let Ok(doc) = source.parse::<toml_edit::DocumentMut>() else {
        return Ok(None);
    };
    let mut item = doc.as_item();
    for segment in path.iter().chain(std::iter::once(&METADATA_KEY)) {
        match item.get(*segment) {
            Some(next) => item = next,
            None => return Ok(None),
        }
    }
    let label = format!("[{}.{METADATA_KEY}]", path.join("."));
    let Some(table) = item.as_table() else {
        return Err(Error::Config {
            path: manifest.to_path_buf(),
            message: format!("{label} is not a table"),
        });
    };
    deserialize_table(manifest, &label, table).map(Some)
}

/// The configuration file in force for `start`.
///
/// The search stops at the workspace root when there is one, and otherwise at
/// the repository boundary -- never at the filesystem root. A checkout has to be
/// able to carry its own settings without inheriting whatever sits above the
/// directory it happens to have been unpacked into.
pub fn discover(start: &Path) -> Option<PathBuf> {
    let stop =
        detector::find_cargo_manifest(start).map(|manifest| detector::workspace_root(&manifest));
    for dir in detector::project_ancestors(start) {
        for name in CONFIG_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if stop.as_deref() == Some(dir) {
            break;
        }
    }
    None
}

fn read_file(path: &Path) -> Result<Layer> {
    let source = std::fs::read_to_string(path).map_err(|err| Error::io(path, err))?;
    Ok(Layer {
        source: Source::File(path.to_path_buf()),
        settings: deserialize_file(path, &source)?,
    })
}

/// The settings this run resolves to, with the layer each value came from.
/// Anything absent is at its built-in default.
pub fn render(settings: &Settings, provenance: &Provenance, json: bool) -> String {
    if json {
        return render_json(settings, provenance);
    }

    let mut doc = match toml_edit::ser::to_document(settings) {
        Ok(doc) => doc,
        Err(err) => return format!("# settings could not be rendered: {err}\n"),
    };

    let annotate = |field: &str| {
        if field == "preset" {
            return "# the presets this run applied, in order\n".to_string();
        }
        let source = provenance.source_of(field);
        match source {
            Source::Environment(_) => format!("# from: {source}\n"),
            _ => format!("# from: {source}   (${})\n", env_var(field)),
        }
    };

    let fields: BTreeMap<String, &'static str> = Settings::FIELDS
        .iter()
        .map(|field| (key(field), *field))
        .collect();

    for (mut name, entry) in doc.iter_mut() {
        let Some(field) = fields.get(name.get()).copied() else {
            continue;
        };
        let prefix = annotate(field);
        if let Some(table) = entry.as_table_mut() {
            table.decor_mut().set_prefix(format!("\n{prefix}"));
        } else {
            name.leaf_decor_mut().set_prefix(prefix);
        }
    }

    format!("# rust-formatter settings\n{doc}")
}

fn render_json(settings: &Settings, provenance: &Provenance) -> String {
    let sources: BTreeMap<String, String> = Settings::FIELDS
        .iter()
        .filter(|field| provenance.get(field).is_some())
        .map(|field| (key(field), provenance.source_of(field).to_string()))
        .collect();
    let document = serde_json::json!({
        "settings": settings,
        "sources": sources,
    });
    format!("{document}\n")
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::OsString, fs};

    use tempfile::tempdir;

    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> HashMap<String, OsString> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(*value)))
            .collect()
    }

    fn lookup(map: &HashMap<String, OsString>) -> impl Fn(&str) -> Option<OsString> + '_ {
        move |key| map.get(key).cloned()
    }

    fn resolve_in(dir: &Path, env: &HashMap<String, OsString>, cli: &SettingsCli) -> Resolved {
        resolve(dir, &lookup(env), cli).expect("these settings resolve")
    }

    fn parse(source: &str) -> Settings {
        deserialize_file(Path::new("<test>"), source).expect("this configuration parses")
    }

    #[test]
    fn an_environment_variable_is_named_after_its_setting() {
        assert_eq!(env_var("sort_deps"), "RUST_FORMATTER_SORT_DEPS");
        assert_eq!(key("toml_max_width"), "toml-max-width");
    }

    /// The key a file writes and the variable the environment reads are both
    /// derived from the flag, so a new setting cannot arrive with one missing.
    #[test]
    fn every_setting_has_a_key_and_a_variable() {
        for field in Settings::FIELDS {
            assert!(!key(field).contains('_'), "{field}");
            assert!(env_var(field).starts_with(ENV_PREFIX), "{field}");
        }
    }

    #[test]
    fn a_scalar_replaces_and_a_list_accumulates() {
        let mut lower = parse("toml-max-width = 80\nexclude = [\"a\"]\n");
        let higher = parse("toml-max-width = 120\nexclude = [\"b\"]\n");
        let mut provenance = Provenance::default();
        lower.merge_from(higher, &Source::CommandLine, &mut provenance);

        assert_eq!(lower.toml_max_width, Some(120));
        assert_eq!(lower.exclude, Some(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(provenance.source_of("exclude"), Source::CommandLine);
    }

    #[test]
    fn a_table_merges_key_by_key() {
        let mut lower = parse("[config]\nmax_width = 120\nhard_tabs = true\n");
        let higher = parse("[config]\nmax_width = 100\n");
        lower.merge_from(higher, &Source::CommandLine, &mut Provenance::default());

        let config = lower.config.unwrap();
        assert_eq!(config["max_width"], ConfigValue::Integer(100));
        assert_eq!(config["hard_tabs"], ConfigValue::Bool(true));
    }

    #[test]
    fn a_rustfmt_option_keeps_the_spelling_the_command_line_would_have_used() {
        let config = parse(
            "[config]\ngroup_imports = \"StdExternalCrate\"\nmax_width = 120\n\
             hard_tabs = false\nignore = [\"a\", \"b\"]\n",
        )
        .config
        .unwrap();
        assert_eq!(config["group_imports"].to_string(), "StdExternalCrate");
        assert_eq!(config["max_width"].to_string(), "120");
        assert_eq!(config["hard_tabs"].to_string(), "false");
        assert_eq!(config["ignore"].to_string(), r#"["a", "b"]"#);
    }

    #[test]
    fn a_boolean_is_read_the_way_the_command_line_reads_one() {
        for (spelling, expected) in [
            ("true", true),
            ("\"on\"", true),
            ("\"yes\"", true),
            ("1", true),
            ("false", false),
            ("\"off\"", false),
            ("0", false),
        ] {
            let parsed = parse(&format!("sort-deps = {spelling}\n"));
            assert_eq!(parsed.sort_deps, Some(Toggle(expected)), "{spelling}");
        }
    }

    /// An environment variable carries no type, so each is read as the type its
    /// own setting has: `2024` is the string `"2024"` for `edition` and the
    /// number 120 for a width.
    #[test]
    fn an_environment_value_is_read_as_the_type_of_its_setting() {
        let map = env_from(&[
            ("RUST_FORMATTER_EDITION", "2024"),
            ("RUST_FORMATTER_TOOLCHAIN", "nightly-2026-06-01"),
            ("RUST_FORMATTER_TOML_MAX_WIDTH", "120"),
            ("RUST_FORMATTER_TOML_INDENT", "tab"),
            ("RUST_FORMATTER_EXCLUDE", r#"["vendor", "generated/**"]"#),
            ("RUST_FORMATTER_SORT_DEPS", "1"),
            ("RUST_FORMATTER_TOML_VERSION", "1.0"),
        ]);
        let found = Settings::from_env(&lookup(&map)).unwrap();

        assert_eq!(found.edition.as_deref(), Some("2024"));
        assert_eq!(found.toolchain.as_deref(), Some("nightly-2026-06-01"));
        assert_eq!(found.toml_max_width, Some(120));
        assert_eq!(found.toml_indent, Some(IndentSpec::Tab));
        assert_eq!(
            found.exclude,
            Some(vec!["vendor".to_string(), "generated/**".to_string()])
        );
        assert_eq!(found.sort_deps, Some(Toggle(true)));
        assert_eq!(found.toml_version, Some(TomlVersion::V1_0));
    }

    #[test]
    fn an_environment_value_that_cannot_be_read_names_its_variable() {
        let map = env_from(&[("RUST_FORMATTER_TOML_MAX_WIDTH", "wide")]);
        let err = Settings::from_env(&lookup(&map)).unwrap_err().to_string();
        assert!(err.contains("RUST_FORMATTER_TOML_MAX_WIDTH"), "{err}");
    }

    #[test]
    fn a_file_value_outside_the_command_line_range_names_its_file() {
        for source in [
            "toml-tab-width = 0\n",
            "toml-max-width = 0\n",
            "toml-max-width = 65535\n",
            "toml-max-blank-lines = 200\n",
            "diff-context = 9000\n",
            "[presets.house]\ntoml-max-width = 0\n",
        ] {
            let err = deserialize_file(Path::new("<test>"), source)
                .unwrap_err()
                .to_string();
            assert!(err.starts_with("<test>: "), "{err}");
            assert!(err.contains("is not in"), "{err}");
        }

        let edges = parse(
            "toml-tab-width = 16\ntoml-max-width = 4096\ntoml-max-blank-lines = 0\ndiff-context = 4096\n",
        );
        assert_eq!(edges.toml_max_width, Some(4096));
    }

    #[test]
    fn an_environment_value_outside_the_command_line_range_names_its_variable() {
        let map = env_from(&[("RUST_FORMATTER_TOML_MAX_WIDTH", "0")]);
        let err = Settings::from_env(&lookup(&map)).unwrap_err().to_string();
        assert_eq!(
            err,
            "$RUST_FORMATTER_TOML_MAX_WIDTH: `0` is not in 1..=4096"
        );
    }

    #[test]
    fn a_metadata_value_outside_the_command_line_range_names_its_table() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[package.metadata.rust-formatter]\ntoml-tab-width = 17\n",
        )
        .unwrap();
        let err = resolve(
            temp.path(),
            &lookup(&env_from(&[])),
            &SettingsCli::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(
                "[package.metadata.rust-formatter]: toml-tab-width: `17` is not in 1..=16"
            ),
            "{err}"
        );
    }

    /// The metadata tables are the whole point of the zero-config promise: a
    /// repository states its style without a file of its own.
    #[test]
    fn the_manifest_tables_layer_workspace_then_package() {
        let temp = tempdir().unwrap();
        let member = temp.path().join("member");
        fs::create_dir_all(&member).unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"member\"]\n\n\
             [workspace.metadata.rust-formatter]\n\
             sort-deps = true\ntoml-max-width = 80\nexclude = [\"vendor/**\"]\n",
        )
        .unwrap();
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\n\n\
             [package.metadata.rust-formatter]\n\
             toml-max-width = 120\nexclude = [\"generated/**\"]\n",
        )
        .unwrap();

        let map = env_from(&[]);
        let resolved = resolve_in(&member, &map, &SettingsCli::default());

        assert_eq!(resolved.settings.sort_deps, Some(Toggle(true)));
        assert_eq!(resolved.settings.toml_max_width, Some(120));
        assert_eq!(
            resolved.settings.exclude,
            Some(vec!["vendor/**".to_string(), "generated/**".to_string()])
        );
        assert!(matches!(
            resolved.provenance.source_of("toml_max_width"),
            Source::PackageMetadata(_)
        ));
    }

    #[test]
    fn the_environment_accumulates_a_list_and_merges_a_table() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("rust-formatter.toml"),
            "exclude = [\"vendor/**\"]\n[config]\nmax_width = 120\nhard_tabs = true\n",
        )
        .unwrap();
        let map = env_from(&[
            ("RUST_FORMATTER_EXCLUDE", r#"["gen/**"]"#),
            ("RUST_FORMATTER_CONFIG", "{ max_width = 100 }"),
        ]);

        let resolved = resolve_in(temp.path(), &map, &SettingsCli::default());
        let config = resolved.settings.config.unwrap();

        assert_eq!(
            resolved.settings.exclude,
            Some(vec!["vendor/**".to_string(), "gen/**".to_string()])
        );
        assert_eq!(config["max_width"], ConfigValue::Integer(100));
        assert_eq!(config["hard_tabs"], ConfigValue::Bool(true));
        assert_eq!(
            resolved.provenance.source_of("exclude"),
            Source::Environment("RUST_FORMATTER_EXCLUDE".to_string())
        );
        assert_eq!(
            resolved.provenance.source_of("config"),
            Source::Environment("RUST_FORMATTER_CONFIG".to_string())
        );
    }

    #[test]
    fn the_sources_name_each_environment_variable_in_precedence_order() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("rust-formatter.toml");
        fs::write(&file, "toml-max-width = 80\n").unwrap();
        let map = env_from(&[
            ("RUST_FORMATTER_TOML_MAX_WIDTH", "90"),
            ("RUST_FORMATTER_SORT_DEPS", "1"),
        ]);

        let resolved = resolve_in(temp.path(), &map, &SettingsCli::default());

        assert_eq!(
            resolved.sources,
            vec![
                Source::File(detector::canonical_dir(temp.path()).join("rust-formatter.toml")),
                Source::Environment("RUST_FORMATTER_TOML_MAX_WIDTH".to_string()),
                Source::Environment("RUST_FORMATTER_SORT_DEPS".to_string()),
            ]
        );
    }

    #[test]
    fn no_environment_source_is_named_when_no_variable_is_set() {
        let temp = tempdir().unwrap();
        let map = env_from(&[]);

        let resolved = resolve_in(temp.path(), &map, &SettingsCli::default());

        assert!(
            !resolved
                .sources
                .iter()
                .any(|source| matches!(source, Source::Environment(_))),
            "{:?}",
            resolved.sources
        );
    }

    fn pinned_to_stable() -> tempfile::TempDir {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        temp
    }

    fn toolchain_under(dir: &Path, pairs: &[(&str, &str)]) -> (Option<String>, Source) {
        let map = env_from(pairs);
        let resolved = resolve_in(dir, &map, &SettingsCli::default());
        (
            resolved.settings.toolchain,
            resolved.provenance.source_of("toolchain"),
        )
    }

    #[test]
    fn a_rustup_plus_toolchain_outranks_the_pin() {
        let temp = pinned_to_stable();

        let found = toolchain_under(
            temp.path(),
            &[
                ("RUSTUP_TOOLCHAIN", "nightly-x86_64-unknown-linux-gnu"),
                ("RUSTUP_TOOLCHAIN_SOURCE", "cli"),
            ],
        );

        assert_eq!(
            found,
            (
                Some("nightly-x86_64-unknown-linux-gnu".to_string()),
                Source::Environment("RUSTUP_TOOLCHAIN".to_string())
            )
        );
    }

    #[test]
    fn a_rustup_toolchain_from_the_environment_outranks_the_pin() {
        let temp = pinned_to_stable();

        let through_a_proxy = toolchain_under(
            temp.path(),
            &[
                ("RUSTUP_TOOLCHAIN", "nightly"),
                ("RUSTUP_TOOLCHAIN_SOURCE", "env"),
            ],
        );
        let exported_directly = toolchain_under(temp.path(), &[("RUSTUP_TOOLCHAIN", "nightly")]);

        let expected = (
            Some("nightly".to_string()),
            Source::Environment("RUSTUP_TOOLCHAIN".to_string()),
        );
        assert_eq!(through_a_proxy, expected);
        assert_eq!(exported_directly, expected);
    }

    #[test]
    fn a_rustup_toolchain_rustup_resolved_on_its_own_leaves_the_pin() {
        let temp = pinned_to_stable();
        let pin =
            Source::ToolchainFile(detector::canonical_dir(temp.path()).join("rust-toolchain.toml"));

        for source in ["toolchain-file", "override", "default", "unrecognised", ""] {
            let found = toolchain_under(
                temp.path(),
                &[
                    ("RUSTUP_TOOLCHAIN", "stable-x86_64-unknown-linux-gnu"),
                    ("RUSTUP_TOOLCHAIN_SOURCE", source),
                ],
            );

            assert_eq!(found, (Some("stable".to_string()), pin.clone()), "{source}");
        }
    }

    #[test]
    fn an_empty_rustup_toolchain_leaves_the_pin() {
        let temp = pinned_to_stable();

        let (toolchain, _) = toolchain_under(temp.path(), &[("RUSTUP_TOOLCHAIN", "")]);

        assert_eq!(toolchain.as_deref(), Some("stable"));
    }

    #[test]
    fn a_rustup_toolchain_without_a_pin_leaves_the_default() {
        let temp = tempdir().unwrap();

        let found = toolchain_under(
            temp.path(),
            &[
                ("RUSTUP_TOOLCHAIN", "stable-x86_64-unknown-linux-gnu"),
                ("RUSTUP_TOOLCHAIN_SOURCE", "default"),
            ],
        );

        assert_eq!(found, (None, Source::Default));
    }

    #[test]
    fn a_configured_toolchain_still_outranks_a_rustup_plus_toolchain() {
        let temp = pinned_to_stable();
        fs::write(
            temp.path().join("rust-formatter.toml"),
            "toolchain = \"beta\"\n",
        )
        .unwrap();

        let found = toolchain_under(
            temp.path(),
            &[
                ("RUSTUP_TOOLCHAIN", "nightly"),
                ("RUSTUP_TOOLCHAIN_SOURCE", "cli"),
            ],
        );

        assert_eq!(found.0.as_deref(), Some("beta"));
    }

    #[test]
    fn a_discovered_file_outranks_the_manifest_tables() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\n\n\
             [package.metadata.rust-formatter]\ntoml-max-width = 80\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("rust-formatter.toml"),
            "toml-max-width = 70\n",
        )
        .unwrap();

        let map = env_from(&[]);
        let resolved = resolve_in(temp.path(), &map, &SettingsCli::default());
        assert_eq!(resolved.settings.toml_max_width, Some(70));
    }

    #[test]
    fn discovery_stops_at_the_workspace_root() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let member = workspace.join("member");
        fs::create_dir_all(&member).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nmembers = [\"member\"]\n",
        )
        .unwrap();
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        // Above the workspace, so no checkout of it inherits the file.
        let outside = temp.path().join("rust-formatter.toml");
        fs::write(&outside, "toml-max-width = 33\n").unwrap();

        assert_eq!(discover(&member), None);
        assert_eq!(discover(temp.path()), Some(outside));
    }

    #[test]
    fn no_config_reads_nothing() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("rust-formatter.toml"),
            "toml-max-width = 70\n",
        )
        .unwrap();

        let map = env_from(&[]);
        let resolved = resolve_in(
            temp.path(),
            &map,
            &SettingsCli {
                no_config: true,
                ..SettingsCli::default()
            },
        );
        assert_eq!(resolved.settings.toml_max_width, None);
    }

    /// A preset applies under the layer that named it, so naming one never
    /// discards the keys that layer also set.
    #[test]
    fn a_preset_applies_below_the_layer_that_named_it() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("rust-formatter.toml"),
            "preset = [\"cargo\"]\ntoml-max-width = 70\n",
        )
        .unwrap();

        let map = env_from(&[]);
        let resolved = resolve_in(temp.path(), &map, &SettingsCli::default());

        assert_eq!(resolved.settings.toml_max_width, Some(70));
        assert_eq!(resolved.settings.sort_keys, Some(Toggle(true)));
        assert!(matches!(
            resolved.provenance.source_of("sort_keys"),
            Source::Preset(_)
        ));
    }

    #[test]
    fn a_preset_named_on_the_command_line_outranks_a_file() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("rust-formatter.toml"),
            "toml-max-width = 70\n",
        )
        .unwrap();

        let map = env_from(&[]);
        let resolved = resolve_in(
            temp.path(),
            &map,
            &SettingsCli {
                preset: vec!["narrow-tabs".to_string()],
                ..SettingsCli::default()
            },
        );
        assert_eq!(resolved.settings.toml_max_width, Some(60));
    }

    #[test]
    fn a_file_may_define_its_own_preset() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("rust-formatter.toml"),
            "[presets.house]\nsort-keys = true\ntoml-max-width = 55\n",
        )
        .unwrap();

        let map = env_from(&[]);
        let resolved = resolve_in(
            temp.path(),
            &map,
            &SettingsCli {
                preset: vec!["house".to_string()],
                ..SettingsCli::default()
            },
        );
        assert_eq!(resolved.settings.toml_max_width, Some(55));
        assert_eq!(resolved.settings.sort_keys, Some(Toggle(true)));
    }

    #[test]
    fn a_preset_may_not_shadow_a_built_in_or_name_another() {
        let temp = tempdir().unwrap();
        let map = env_from(&[]);

        fs::write(
            temp.path().join("rust-formatter.toml"),
            "[presets.cargo]\nsort-keys = true\n",
        )
        .unwrap();
        let err = resolve(temp.path(), &lookup(&map), &SettingsCli::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("built-in preset"), "{err}");

        fs::write(
            temp.path().join("rust-formatter.toml"),
            "[presets.house]\npreset = [\"cargo\"]\n",
        )
        .unwrap();
        let err = resolve(temp.path(), &lookup(&map), &SettingsCli::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("names another preset"), "{err}");
    }

    #[test]
    fn an_unknown_preset_lists_the_ones_there_are() {
        let temp = tempdir().unwrap();
        let map = env_from(&[]);
        let err = resolve(
            temp.path(),
            &lookup(&map),
            &SettingsCli {
                preset: vec!["nope".to_string()],
                ..SettingsCli::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown preset `nope`"), "{err}");
        assert!(err.contains("narrow-tabs"), "{err}");
    }

    /// serde would answer with all fifty-odd settings it accepts, which buries
    /// the one that was meant.
    #[test]
    fn a_misspelled_key_names_the_one_it_meant() {
        let err = deserialize_file(Path::new("<test>"), "sort_deps = true\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("did you mean `sort-deps`"), "{err}");

        let err = deserialize_file(Path::new("<test>"), "wibble = true\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown setting `wibble`"), "{err}");
        assert!(!err.contains("did you mean"), "{err}");
    }

    #[test]
    fn the_style_guide_preset_is_the_cargo_preset() {
        let cargo = builtin_preset("cargo").unwrap();
        assert_eq!(builtin_preset("style-guide"), Some(cargo));
    }

    #[test]
    fn every_named_preset_resolves() {
        for name in BUILTIN_PRESETS {
            assert!(builtin_preset(name).is_some(), "{name}");
        }
    }

    #[test]
    fn the_rendered_settings_name_the_layer_each_value_came_from() {
        let mut settings = Settings::default();
        let mut provenance = Provenance::default();
        settings.merge_from(
            parse("toml-max-width = 70\n"),
            &Source::File(PathBuf::from("/repo/rust-formatter.toml")),
            &mut provenance,
        );
        let rendered = render(&settings, &provenance, false);
        assert!(rendered.contains("toml-max-width = 70"), "{rendered}");
        assert!(
            rendered.contains("# from: /repo/rust-formatter.toml"),
            "{rendered}"
        );

        let json = render(&settings, &provenance, true);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["settings"]["toml-max-width"], 70);
        assert_eq!(
            parsed["sources"]["toml-max-width"],
            "/repo/rust-formatter.toml"
        );
    }
}
