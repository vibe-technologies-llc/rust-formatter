use std::{borrow::Cow, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::toml_width::{self, DEFAULT_TAB_WIDTH};

/// Canonical `[package]` / `[workspace.package]` field order: cargo's own
/// manifest-reference order, with `edition` and `rust-version` hoisted ahead of
/// `authors` because that is the order the ecosystem actually writes.
///
/// This is the documentation sequence, not the normative one. The Rust Style
/// Guide's `Cargo.toml` chapter asks for `name`, `version`, every other key
/// version-sorted, and `description` **last** — the two disagree about
/// `description`, and cargo enforces neither. The Book order is the default
/// because it is what manifests in the wild look like;
/// [`PackageOrder::StyleGuide`] selects the normative one.
pub const PACKAGE_ORDER: &[&str] = &[
    "name",
    "version",
    "edition",
    "rust-version",
    "authors",
    "description",
    "documentation",
    "readme",
    "homepage",
    "repository",
    "license",
    "license-file",
    "keywords",
    "categories",
    "workspace",
    "build",
    "links",
    "exclude",
    "include",
    "publish",
    "metadata",
    "default-run",
    "autolib",
    "autobins",
    "autoexamples",
    "autotests",
    "autobenches",
    "resolver",
];

/// Field order inside one dependency entry, whether it is written as
/// `[dependencies.serde]`, `serde.version = "1"` or `serde = { version = "1" }`.
/// Source first, then what is built from it. `registry-index` sits beside
/// `registry` because cargo accepts both spellings.
pub const DEP_ORDER: &[&str] = &[
    "package",
    "version",
    "registry",
    "registry-index",
    "path",
    "git",
    "branch",
    "tag",
    "rev",
    "features",
    "optional",
    "default-features",
    "workspace",
];

/// Top-level table order for a manifest: the Cargo Book's own chapter sequence,
/// with `[features]` pulled ahead of the dependency tables because a feature
/// list reads as part of the package's surface rather than its inputs.
pub const TABLE_ORDER: &[&str] = &[
    "package",
    "lib",
    "bin",
    "example",
    "test",
    "bench",
    "features",
    "dependencies",
    "dev-dependencies",
    "build-dependencies",
    "target",
    "badges",
    "lints",
    "workspace",
    "profile",
    "patch",
    "replace",
];

/// Which of the two published `[package]` orders `--sort-package` applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageOrder {
    /// [`PACKAGE_ORDER`], the Cargo Book sequence.
    #[default]
    Book,
    /// The Rust Style Guide: `name`, `version`, everything else version-sorted,
    /// `description` last.
    StyleGuide,
}

/// Bytes of indentation kept pre-built. Deeper nesting than this allocates.
const POOL_BYTES: usize = 64;

/// One indentation unit plus the pools that let every nesting level up to
/// [`POOL_BYTES`] be served as a slice instead of a fresh allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlIndent {
    unit: String,
    pool: String,
    newline_pool: String,
    pooled_levels: usize,
    tab_width: usize,
    /// Columns one level occupies, or `None` when the unit holds a tab and the
    /// cost depends on the column the level starts at.
    level_width: Option<usize>,
}

impl TomlIndent {
    pub fn new(unit: &str) -> Self {
        Self::with_tab_width(unit, DEFAULT_TAB_WIDTH)
    }

    pub fn with_tab_width(unit: &str, tab_width: usize) -> Self {
        let pooled_levels = if unit.is_empty() {
            0
        } else {
            POOL_BYTES.div_ceil(unit.len())
        };
        let pool = unit.repeat(pooled_levels);
        let mut newline_pool = String::with_capacity(pool.len() + 1);
        newline_pool.push('\n');
        newline_pool.push_str(&pool);
        let level_width = (!unit.contains('\t')).then(|| toml_width::width(unit, tab_width));

        Self {
            unit: unit.to_owned(),
            pool,
            newline_pool,
            pooled_levels,
            tab_width,
            level_width,
        }
    }

    pub fn spaces(count: usize) -> Self {
        Self::new(&" ".repeat(count))
    }

    pub fn tab() -> Self {
        Self::new("\t")
    }

    pub fn unit(&self) -> &str {
        &self.unit
    }

    /// Columns a tab advances by, which also applies to a tab inside a value.
    pub fn tab_width(&self) -> usize {
        self.tab_width
    }

    /// Columns a nesting level occupies, counted as the line renders: a wide
    /// character costs two and a tab reaches the next tab stop.
    pub fn width(&self, level: usize) -> usize {
        if let Some(width) = self.level_width {
            return width * level;
        }
        (0..level).fold(0, |column, _| {
            toml_width::advance(column, &self.unit, self.tab_width)
        })
    }

    pub fn text(&self, level: usize) -> Cow<'_, str> {
        if level > self.pooled_levels {
            return Cow::Owned(self.unit.repeat(level));
        }
        Cow::Borrowed(&self.pool[..level * self.unit.len()])
    }

    /// [`Self::text`] preceded by the newline that opens a wrapped container.
    pub fn newline(&self, level: usize) -> Cow<'_, str> {
        if level > self.pooled_levels {
            return Cow::Owned(format!("\n{}", self.unit.repeat(level)));
        }
        Cow::Borrowed(&self.newline_pool[..=(level * self.unit.len())])
    }
}

impl Default for TomlIndent {
    fn default() -> Self {
        Self::spaces(4)
    }
}

/// The widest indent unit accepted, in spaces.
pub const MAX_INDENT: usize = 16;

/// The indent unit as it is written, before the tab width that gives it a column
/// cost is known. The CLI and a configuration file both name it this way, so
/// both reach [`TomlIndent`] through one parser.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IndentSpec {
    Tab,
    Spaces(usize),
}

impl IndentSpec {
    pub fn resolve(self, tab_width: usize) -> TomlIndent {
        match self {
            Self::Tab => TomlIndent::with_tab_width("\t", tab_width),
            Self::Spaces(count) => TomlIndent::with_tab_width(&" ".repeat(count), tab_width),
        }
    }
}

impl Default for IndentSpec {
    fn default() -> Self {
        Self::Spaces(4)
    }
}

impl fmt::Display for IndentSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tab => f.write_str("tab"),
            Self::Spaces(count) => write!(f, "{count}"),
        }
    }
}

impl FromStr for IndentSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("tab") {
            return Ok(Self::Tab);
        }
        match value.parse::<usize>() {
            Ok(count) if count <= MAX_INDENT => Ok(Self::Spaces(count)),
            Ok(_) => Err(format!("at most {MAX_INDENT} spaces")),
            Err(_) => Err(format!(
                "expected a number of spaces or `tab`, found `{value}`"
            )),
        }
    }
}

impl Serialize for IndentSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Tab => serializer.serialize_str("tab"),
            Self::Spaces(count) => serializer.serialize_u64(*count as u64),
        }
    }
}

impl<'de> Deserialize<'de> for IndentSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = IndentSpec;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a number of spaces (0-{MAX_INDENT}) or \"tab\"")
            }

            fn visit_u64<E: serde::de::Error>(self, count: u64) -> Result<Self::Value, E> {
                match usize::try_from(count) {
                    Ok(count) if count <= MAX_INDENT => Ok(IndentSpec::Spaces(count)),
                    _ => Err(E::custom(format!("at most {MAX_INDENT} spaces"))),
                }
            }

            fn visit_i64<E: serde::de::Error>(self, count: i64) -> Result<Self::Value, E> {
                u64::try_from(count)
                    .map_err(|_| E::custom("a negative indent"))
                    .and_then(|count| self.visit_u64(count))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArrayStyle {
    /// An array the author wrote across lines stays across lines.
    #[default]
    Preserve,
    /// Width alone decides, so an array that fits is pulled back onto one line.
    Auto,
    /// Any array with two or more elements becomes multi-line.
    Expand,
}

/// Whether a one-line container pads the inside of its brackets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Spacing {
    /// `[a, b]`, `{a = 1}`.
    #[default]
    Compact,
    /// `[ a, b ]`, `{ a = 1 }`. An empty container stays `[]` / `{}`.
    Spaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InlineTableStyle {
    /// One line until the rendered form exceeds the width budget.
    #[default]
    Auto,
    /// Never multi-line, so the formatter adds no TOML 1.1 construct of its
    /// own. A 1.1-only spelling already in the input still survives verbatim;
    /// [`TomlVersion::V1_0`] is the checked guarantee.
    Compact,
    /// Any table with two or more keys becomes multi-line.
    Expand,
    /// A table too wide for its line is promoted to its own `[header]` section
    /// rather than wrapped, which is what the Rust Style Guide asks for. Where
    /// promotion is impossible — inside an array, inside another inline table,
    /// or at the document root — this falls back to [`Self::Auto`].
    Section,
}

/// The TOML revision the output has to satisfy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TomlVersion {
    /// Emit no 1.1 construct, and report the ones already in the document that
    /// cannot be removed without rewriting a value.
    #[serde(rename = "1.0")]
    V1_0,
    #[default]
    #[serde(rename = "1.1")]
    V1_1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrailingComma {
    #[default]
    Never,
    /// Only on wrapped arrays and expanded inline tables; one-line forms never
    /// take one.
    Multiline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlStyle {
    pub indent: TomlIndent,
    pub max_width: usize,
    pub arrays: ArrayStyle,
    pub inline_tables: InlineTableStyle,
    pub toml_version: TomlVersion,
    pub trailing_comma: TrailingComma,
    /// Whether `# fmt: off` and friends exempt the region they open.
    pub directives: bool,
    pub blank_line_before_tables: bool,
    pub max_blank_lines: usize,
    pub array_spacing: Spacing,
    pub inline_table_spacing: Spacing,
    pub align_entries: bool,
    pub align_comments: bool,
    pub indent_tables: bool,
    pub indent_entries: bool,
    pub normalize_keys: bool,
    pub sort_deps: bool,
    pub sort_package: bool,
    pub sort_dep_fields: bool,
    pub sort_features: bool,
    pub sort_arrays: bool,
    pub sort_targets: bool,
    pub sort_tables: bool,
    pub sort_keys: bool,
    pub sort_grouped: bool,
    pub package_order: PackageOrder,
    pub cargo_conventions: bool,
}

impl TomlStyle {
    /// Whether any rule would reorder something. `sort_document` is skipped
    /// entirely when this is false, so a new sort knob missing from here would
    /// silently never run.
    pub fn sorts(&self) -> bool {
        self.sort_deps
            || self.sort_package
            || self.sort_dep_fields
            || self.sort_features
            || self.sort_arrays
            || self.sort_targets
            || self.sort_tables
            || self.sort_keys
    }

    pub fn aligns(&self) -> bool {
        self.align_entries || self.align_comments
    }

    pub fn tab_width(&self) -> usize {
        self.indent.tab_width()
    }
}

impl Default for TomlStyle {
    fn default() -> Self {
        Self {
            indent: TomlIndent::default(),
            max_width: 100,
            arrays: ArrayStyle::default(),
            inline_tables: InlineTableStyle::default(),
            toml_version: TomlVersion::default(),
            trailing_comma: TrailingComma::default(),
            directives: true,
            blank_line_before_tables: false,
            max_blank_lines: 1,
            array_spacing: Spacing::Compact,
            inline_table_spacing: Spacing::Spaced,
            align_entries: false,
            align_comments: false,
            indent_tables: false,
            indent_entries: false,
            normalize_keys: false,
            sort_deps: false,
            sort_package: false,
            sort_dep_fields: false,
            sort_features: false,
            sort_arrays: false,
            sort_targets: false,
            sort_tables: false,
            sort_keys: false,
            sort_grouped: false,
            package_order: PackageOrder::Book,
            cargo_conventions: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pools_match_repeat() {
        for unit in ["    ", "  ", "\t", ""] {
            let indent = TomlIndent::new(unit);
            for level in [0, 1, 4, 16, 17, 64, 65] {
                assert_eq!(indent.text(level).as_ref(), unit.repeat(level), "{unit:?}");
                assert_eq!(
                    indent.newline(level).as_ref(),
                    format!("\n{}", unit.repeat(level)),
                    "{unit:?}"
                );
            }
        }
    }

    #[test]
    fn width_counts_display_columns() {
        assert_eq!(TomlIndent::spaces(4).width(3), 12);
        assert_eq!(TomlIndent::new("").width(3), 0);
    }

    #[test]
    fn a_tab_level_reaches_the_next_tab_stop() {
        assert_eq!(TomlIndent::tab().width(3), 12);
        assert_eq!(TomlIndent::with_tab_width("\t", 8).width(3), 24);
        assert_eq!(TomlIndent::with_tab_width("\t", 1).width(3), 3);
    }

    #[test]
    fn order_tables_have_no_duplicates() {
        for table in [PACKAGE_ORDER, DEP_ORDER, TABLE_ORDER] {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(sorted.len(), before, "{table:?}");
        }
    }
}
