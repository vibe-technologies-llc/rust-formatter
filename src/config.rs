use std::{borrow::Cow, collections::BTreeMap};

use serde::{Deserialize, Serialize};

use crate::error::Usage;

/// Default configuration key-value pairs required for:
/// - std -> external -> crate import ordering (`group_imports = "StdExternalCrate"`)
/// - merged `use crate::{x, y};` imports (`imports_granularity = "Crate"`)
pub const DEFAULT_CONFIGS: &[(&str, &str)] = &[
    ("group_imports", "StdExternalCrate"),
    ("imports_granularity", "Crate"),
];

/// The rustfmt options an opinionated wrapper should have an opinion about,
/// grouped so a caller selects a position rather than a dozen `--config`
/// strings. Each preset states its values outright, so a change to rustfmt's own
/// defaults cannot move them.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RustStyle {
    /// Comments are formatted like code: wrapped to the width budget, given
    /// consistent markers, and Rust inside a doc comment is formatted too.
    Comments,
    /// Literals and the shapes around them are spelled one way.
    Literals,
    /// Everything above, plus the options that rewrite string literals and
    /// reorder the items of an `impl`.
    Strict,
}

impl RustStyle {
    /// The options this preset sets in its own right. `Strict` layers over the
    /// other two rather than restating them; `options_layered` is what applies.
    pub fn options(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Comments => &[
                ("format_code_in_doc_comments", "true"),
                ("normalize_comments", "true"),
                ("wrap_comments", "true"),
            ],
            Self::Literals => &[
                ("condense_wildcard_suffixes", "true"),
                ("hex_literal_case", "Upper"),
                ("overflow_delimited_expr", "true"),
            ],
            Self::Strict => &[
                ("blank_lines_upper_bound", "1"),
                ("format_strings", "true"),
                ("reorder_impl_items", "true"),
            ],
        }
    }

    /// Every option the preset implies, in application order.
    pub fn options_layered(self) -> impl Iterator<Item = (&'static str, &'static str)> {
        let layers: &'static [Self] = match self {
            Self::Comments => &[Self::Comments],
            Self::Literals => &[Self::Literals],
            Self::Strict => &[Self::Comments, Self::Literals, Self::Strict],
        };
        layers
            .iter()
            .flat_map(|layer| layer.options().iter().copied())
    }
}

type Key = Cow<'static, str>;

/// Builder and manager for rustfmt `--config` options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustfmtConfig {
    options: BTreeMap<Key, Key>,
}

impl Default for RustfmtConfig {
    fn default() -> Self {
        Self {
            options: DEFAULT_CONFIGS
                .iter()
                .map(|&(key, value)| (Cow::Borrowed(key), Cow::Borrowed(value)))
                .collect(),
        }
    }
}

impl RustfmtConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// No options at all, not even the defaults this tool adds. What a caller
    /// needs to read back the options it was handed rather than the ones the
    /// tool would apply on its own.
    pub fn empty() -> Self {
        Self {
            options: BTreeMap::new(),
        }
    }

    /// Add a key-value setting (e.g. `max_width=120`). Overwrites existing key if present.
    pub fn set(&mut self, key: impl Into<Key>, value: impl Into<Key>) -> &mut Self {
        self.options.insert(key.into(), value.into());
        self
    }

    /// Layer a named preset over what is already set. A preset never overwrites
    /// a key a later `--config` will set, because `--config` is applied after it.
    pub fn apply_style(&mut self, style: RustStyle) -> &mut Self {
        for (key, value) in style.options_layered() {
            self.set(Cow::Borrowed(key), Cow::Borrowed(value));
        }
        self
    }

    /// Remove a key so rustfmt falls back to its own default. This is the only
    /// way to disable an entry of `DEFAULT_CONFIGS`, which `set` can overwrite
    /// but never drop. Reports whether anything was actually removed, so a key
    /// this run never set can be named rather than passing silently.
    pub fn unset(&mut self, key: &str) -> bool {
        self.options.remove(key).is_some()
    }

    /// Remove every key in `keys`, returning the ones that were not set.
    pub fn unset_all<T: AsRef<str>>(&mut self, keys: &[T]) -> Vec<String> {
        keys.iter()
            .filter(|key| !self.unset(key.as_ref()))
            .map(|key| key.as_ref().to_owned())
            .collect()
    }

    /// Remove a key and return its value. Used to lift a setting that has a
    /// dedicated flag out of the pass-through map, so both spellings reach the
    /// same place instead of racing on rustfmt's command line.
    pub fn take(&mut self, key: &str) -> Option<String> {
        self.options.remove(key).map(Cow::into_owned)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.options.get(key).map(Cow::as_ref)
    }

    pub fn is_empty(&self) -> bool {
        self.options.is_empty()
    }

    pub fn extend_from_str(&mut self, config_str: &str) -> Result<&mut Self, Usage> {
        for entry in split_entries(config_str) {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((key, value)) = entry.split_once('=') else {
                return Err(Usage::ConfigEntryWithoutValue {
                    entry: entry.to_owned(),
                });
            };
            let key = key.trim();
            if key.is_empty() {
                return Err(Usage::ConfigEntryWithoutKey {
                    entry: entry.to_owned(),
                });
            }
            self.set(key.to_owned(), value.trim().to_owned());
        }
        Ok(self)
    }

    pub fn extend_from_slice<T: AsRef<str>>(&mut self, configs: &[T]) -> Result<&mut Self, Usage> {
        for config in configs {
            self.extend_from_str(config.as_ref())?;
        }
        Ok(self)
    }

    /// Format as a single string suitable for `--config <value>`, leaving out
    /// what only a configuration file can carry.
    pub fn to_config_arg(&self) -> String {
        let mut out = String::new();
        for (key, value) in self.command_line_options() {
            if !out.is_empty() {
                out.push(',');
            }
            out.push_str(key);
            out.push('=');
            out.push_str(value);
        }
        out
    }

    /// Iterate over the configured options in key order.
    pub fn options(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.options
            .iter()
            .map(|(key, value)| (key.as_ref(), value.as_ref()))
    }

    /// The options rustfmt can be told about on its command line.
    pub fn command_line_options(&self) -> impl Iterator<Item = (&str, &str)> {
        self.options()
            .filter(|(_, value)| !needs_config_file(value))
    }

    /// The options that have no command-line spelling and must reach rustfmt
    /// through `--config-path`.
    pub fn config_file_options(&self) -> impl Iterator<Item = (&str, &str)> {
        self.options().filter(|(_, value)| needs_config_file(value))
    }

    pub fn needs_config_file(&self) -> bool {
        self.config_file_options().next().is_some()
    }
}

/// rustfmt splits every `--config` occurrence on `,` itself and then parses each
/// value as a scalar, so an array -- or anything holding a comma -- has no
/// command-line spelling at all and can only reach rustfmt through a file.
fn needs_config_file(value: &str) -> bool {
    let value = value.trim();
    value.contains(',') || value.starts_with('[')
}

/// Split on the commas that separate entries, not on the ones inside a value.
fn split_entries(text: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0usize;

    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('"') if ch == '\\' => escaped = true,
            Some(open) if ch == open => quote = None,
            Some(_) => {}
            None => match ch {
                '"' | '\'' => quote = Some(ch),
                '[' | '{' => depth += 1,
                ']' | '}' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    entries.push(&text[start..index]);
                    start = index + ch.len_utf8();
                }
                _ => {}
            },
        }
    }

    entries.push(&text[start..]);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = RustfmtConfig::default();
        let arg = cfg.to_config_arg();
        assert!(arg.contains("group_imports=StdExternalCrate"));
        assert!(arg.contains("imports_granularity=Crate"));
    }

    #[test]
    fn test_override_and_extend() {
        let mut cfg = RustfmtConfig::default();
        cfg.extend_from_str("max_width=120,edition=2024").unwrap();
        let arg = cfg.to_config_arg();
        assert!(arg.contains("group_imports=StdExternalCrate"));
        assert!(arg.contains("imports_granularity=Crate"));
        assert!(arg.contains("max_width=120"));
        assert!(arg.contains("edition=2024"));
    }

    #[test]
    fn test_config_arg_is_exactly_sized() {
        let mut cfg = RustfmtConfig::default();
        cfg.extend_from_str("max_width=120").unwrap();
        let arg = cfg.to_config_arg();
        assert_eq!(arg.matches(',').count(), cfg.options().len() - 1);
    }

    /// The whole point of the aware split: `ignore` and `skip_macro_invocations`
    /// are the two options a wrapper most needs to pass through, and both are
    /// arrays.
    #[test]
    fn an_array_value_survives_the_split() {
        let mut cfg = RustfmtConfig::default();
        cfg.extend_from_str(r#"ignore=["a","b"],max_width=120"#)
            .unwrap();
        assert_eq!(cfg.get("ignore"), Some(r#"["a","b"]"#));
        assert_eq!(cfg.get("max_width"), Some("120"));
    }

    #[test]
    fn a_comma_inside_a_quoted_value_is_not_a_separator() {
        let mut cfg = RustfmtConfig::default();
        cfg.extend_from_str(r#"skip_macro_invocations=["a,b"]"#)
            .unwrap();
        assert_eq!(cfg.get("skip_macro_invocations"), Some(r#"["a,b"]"#));
        cfg.extend_from_str(r#"x="a\",b""#).unwrap();
        assert_eq!(cfg.get("x"), Some(r#""a\",b""#));
    }

    #[test]
    fn an_array_value_leaves_the_command_line() {
        let mut cfg = RustfmtConfig::default();
        cfg.extend_from_str(r#"ignore=["a","b"],max_width=120"#)
            .unwrap();
        assert!(cfg.needs_config_file());
        assert!(!cfg.to_config_arg().contains("ignore"));
        assert!(cfg.to_config_arg().contains("max_width=120"));
        let file: Vec<_> = cfg.config_file_options().collect();
        assert_eq!(file, vec![("ignore", r#"["a","b"]"#)]);
    }

    /// A single-element array holds no comma but is still rejected by rustfmt's
    /// own `--config` parser, so the bracket has to route it too.
    #[test]
    fn a_single_element_array_also_takes_the_file_route() {
        let mut cfg = RustfmtConfig::default();
        cfg.extend_from_str(r#"ignore=["vendor"]"#).unwrap();
        assert!(cfg.needs_config_file());
    }

    #[test]
    fn a_bare_key_is_rejected_rather_than_becoming_true() {
        let mut cfg = RustfmtConfig::default();
        let err = cfg.extend_from_str("max_width").unwrap_err();
        assert!(err.to_string().contains("max_width"), "{err}");
        assert_eq!(cfg.get("max_width"), None);
        assert!(cfg.extend_from_str("=120").is_err());
    }

    #[test]
    fn an_empty_entry_is_still_tolerated() {
        let mut cfg = RustfmtConfig::default();
        cfg.extend_from_str("max_width=120,,").unwrap();
        assert_eq!(cfg.get("max_width"), Some("120"));
    }

    #[test]
    fn unset_reports_whether_it_removed_anything() {
        let mut cfg = RustfmtConfig::default();
        assert!(cfg.unset("group_imports"));
        assert!(!cfg.unset("group_imports"));
        assert_eq!(
            cfg.unset_all(&["imports_granularity", "max_width"]),
            vec!["max_width".to_string()]
        );
    }

    #[test]
    fn take_lifts_a_value_out_of_the_pass_through_map() {
        let mut cfg = RustfmtConfig::default();
        cfg.extend_from_str("edition=2018,max_width=120").unwrap();
        assert_eq!(cfg.take("edition").as_deref(), Some("2018"));
        assert_eq!(cfg.take("edition"), None);
        assert!(!cfg.to_config_arg().contains("edition"));
    }

    #[test]
    fn strict_layers_over_the_two_presets_below_it() {
        let mut cfg = RustfmtConfig::default();
        cfg.apply_style(RustStyle::Strict);
        assert_eq!(cfg.get("wrap_comments"), Some("true"));
        assert_eq!(cfg.get("hex_literal_case"), Some("Upper"));
        assert_eq!(cfg.get("format_strings"), Some("true"));

        let mut narrow = RustfmtConfig::default();
        narrow.apply_style(RustStyle::Literals);
        assert_eq!(narrow.get("wrap_comments"), None);
        assert_eq!(narrow.get("hex_literal_case"), Some("Upper"));
    }

    /// The precedence the CLI documents: a preset fills in, `--config` overrides
    /// it, `--unset-config` drops it.
    #[test]
    fn a_config_flag_wins_over_a_preset() {
        let mut cfg = RustfmtConfig::default();
        cfg.apply_style(RustStyle::Comments);
        cfg.extend_from_str("wrap_comments=false").unwrap();
        assert_eq!(cfg.get("wrap_comments"), Some("false"));
        cfg.unset_all(&["normalize_comments"]);
        assert_eq!(cfg.get("normalize_comments"), None);
    }
}
