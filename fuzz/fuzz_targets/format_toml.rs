#![no_main]
// The oracle modules below are shared with the test suites, which use more of
// each of them than one fuzz target does.
#![allow(dead_code)]

use libfuzzer_sys::fuzz_target;
use toml_edit::DocumentMut;

use crate::styles::{Profile, profiles};

/// The oracles the test suites use, pulled in as source rather than copied.
///
/// Declared at the crate root because `directives` names
/// `super::comments::skip_string` and `styles` names all three of the others:
/// nested inside another module the `#[path]` roots would shift and `super`
/// would no longer find them. `generator` and `golden` are deliberately absent
/// -- they need `proptest` and `walkdir`, which a fuzz target has no use for.
#[path = "../../tests/support/comments.rs"]
mod comments;
#[path = "../../tests/support/directives.rs"]
mod directives;
#[path = "../../tests/support/spellings.rs"]
mod spellings;
#[path = "../../tests/support/styles.rs"]
mod styles;
#[path = "../../tests/support/toml_tree.rs"]
mod toml_tree;

/// The first byte picks the style, the rest is the document, so the fuzzer
/// explores the option surface and the input grammar together rather than
/// pinning one of them to its default.
fn split(data: &[u8]) -> Option<(Profile, &str)> {
    let (selector, body) = data.split_first()?;
    let profiles = profiles();
    let profile = profiles
        .into_iter()
        .nth(usize::from(*selector) % 7)
        .expect("seven presets");
    Some((profile, std::str::from_utf8(body).ok()?))
}

fuzz_target!(|data: &[u8]| {
    let Some((profile, source)) = split(data) else {
        return;
    };
    // What the formatter promises is stated about a document that parses; one
    // that does not is the runner's problem and is reported per file.
    let Ok(parsed) = source.parse::<DocumentMut>() else {
        return;
    };
    // `toml_edit` regroups interleaved dotted keys on its own, which moves lines
    // the formatter never touched. Its own output is the baseline that is free
    // of that.
    let baseline = parsed.to_string();

    let once = rust_formatter::format_toml(&baseline, &profile.style)
        .expect("a document that parses has to format");

    let after = once
        .parse::<DocumentMut>()
        .expect("the output of a successful format has to parse");

    assert_eq!(
        profile.normalize(&toml_tree::tree_of(&parsed)),
        profile.normalize(&toml_tree::tree_of(&after)),
        "value tree changed under {}\n{baseline}",
        profile.name
    );

    assert_eq!(
        profile.spelling_view(&spellings::spellings(&parsed)),
        profile.spelling_view(&spellings::spellings(&after)),
        "value spelling changed under {}\n{baseline}",
        profile.name
    );

    assert_eq!(
        profile.comment_view(&baseline),
        profile.comment_view(&once),
        "comments changed under {}\n{baseline}",
        profile.name
    );

    if profile.style.directives {
        assert!(
            directives::regions_survive(&baseline, &once),
            "directive region changed under {}\n{baseline}",
            profile.name
        );
    }

    let twice = rust_formatter::format_toml(&once, &profile.style)
        .expect("the output of a successful format has to format");
    assert_eq!(
        once, twice,
        "not idempotent under {}\n{baseline}",
        profile.name
    );
});
