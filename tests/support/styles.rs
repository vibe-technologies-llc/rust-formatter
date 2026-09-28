use rust_formatter::{InlineTableStyle, TomlStyle, settings};

use super::{comments, spellings, toml_tree::Tree};

pub struct Profile {
    pub name: String,
    pub style: TomlStyle,
}

/// The name is already a full rendering of the style for a generated profile,
/// so printing both would double every proptest failure message.
impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

impl Profile {
    /// Whether this profile moves array elements. Read off the resolved style
    /// rather than listed by hand, so a preset that gains a reordering rule
    /// cannot quietly weaken the oracle.
    pub fn reorders_arrays(&self) -> bool {
        self.style.sort_arrays || self.style.sort_targets
    }

    /// Whether this profile moves table entries -- and with them their
    /// comments. Beyond the sort rules, `--toml-inline-tables section` promotes
    /// an over-wide inline table to its own `[header]`, which moves that key
    /// past its siblings.
    pub fn reorders(&self) -> bool {
        self.style.sorts() || self.style.inline_tables == InlineTableStyle::Section
    }

    pub fn normalize(&self, tree: &Tree) -> Tree {
        tree.normalized(self.reorders_arrays(), self.reorders())
    }

    /// Every comment body, ordered when the profile is not allowed to move one
    /// and as a multiset when it is. Guarantee 4 promises comments keep their
    /// place, so the ordered form is the one that states it.
    pub fn comment_view(&self, src: &str) -> Vec<String> {
        let mut list = comments::normalized(src);
        if self.reorders() {
            list.sort();
        }
        list
    }

    /// Guarantee 5, viewed the same way: path-keyed and ordered unless the
    /// profile reorders, in which case only the bag of spellings can be stated.
    pub fn spelling_view(&self, pairs: &[(String, String)]) -> Vec<String> {
        if self.reorders() || self.reorders_arrays() {
            spellings::multiset(pairs)
        } else {
            pairs
                .iter()
                .map(|(path, repr)| format!("{path} = {repr}"))
                .collect()
        }
    }
}

/// The presets the binary ships, as the profiles every corpus and property check
/// runs. The formatter's guarantees are not promises about the defaults, so the
/// whole option surface is exercised rather than one setting of it -- and by
/// taking the shipped presets rather than a copy of them, these checks cover what
/// `--preset` actually gives a user.
///
/// `cargo-conventions` is in no preset: it rewrites `dep = { version = "1" }` as
/// `dep = "1"`, so no value-tree comparison survives it. Its coverage is the unit
/// and CLI tests.
const PROFILES: [&str; 7] = [
    "default",
    "compact",
    "expand",
    "everything",
    "cargo",
    "narrow-tabs",
    "aligned-indented",
];

pub fn profiles() -> Vec<Profile> {
    PROFILES
        .iter()
        .map(|name| Profile {
            name: (*name).to_owned(),
            style: settings::builtin_preset(name)
                .unwrap_or_else(|| panic!("no built-in preset `{name}`"))
                .toml_style(),
        })
        .collect()
}
