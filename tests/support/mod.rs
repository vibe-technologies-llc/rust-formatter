// Compiled separately into each integration-test binary, so anything only one
// of them needs looks unused from the others.
#![allow(dead_code, unused_imports, unused_macros)]

pub mod comments;
pub mod corpus;
pub mod directives;
pub mod generator;
pub mod golden;
pub mod rust_comments;
pub mod snippets;
pub mod spellings;
pub mod styles;
pub mod toml_tree;
pub mod toolchain;

pub use comments::comments;
pub use directives::{regions, regions_survive};
pub use generator::arb_style;
pub use spellings::{parse_spellings, spellings};
pub use styles::{Profile, profiles};
pub use toml_tree::{Tree, parse_tree, tree_of};
pub(crate) use toolchain::{needs_nightly, needs_stable};
pub use toolchain::{nightly_available, stable_available};

/// What `toml_edit` alone does to a document, with no `rust-formatter` code
/// involved.
///
/// `toml_edit` does not round-trip documents that interleave dotted keys with
/// different first segments: it regroups them under their first segment, which
/// moves lines and strands any comment that sat above a moved line. Stating the
/// comment and ordering properties against this baseline rather than against the
/// raw input keeps that upstream behaviour from being reported as a formatter
/// bug.
pub fn round_trip(src: &str) -> Result<String, toml_edit::TomlError> {
    Ok(src.parse::<toml_edit::DocumentMut>()?.to_string())
}
