mod support;

use proptest::prelude::*;
use rust_formatter::format_toml;
use support::{
    Profile, arb_style, generator::arb_doc, parse_spellings, parse_tree, profiles, regions,
    regions_survive, round_trip,
};

/// The stock budget is 256 cases, which this suite burns through in about a
/// second. `RUST_FORMATTER_PROPTEST_CASES` is the deep mode -- see the
/// `test-deep` recipe in the `Justfile`.
fn config() -> ProptestConfig {
    let cases = std::env::var("RUST_FORMATTER_PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(256);
    ProptestConfig {
        cases,
        max_shrink_iters: 8192,
        ..ProptestConfig::default()
    }
}

/// Every guarantee in `docs/toml-style.md`, for one document under one style.
///
/// Stated against the `toml_edit` round-trip baseline rather than the raw
/// source wherever position matters: `toml_edit` regroups interleaved dotted
/// keys on its own, which moves lines the formatter never touched.
fn check(src: &str, profile: &Profile) -> Result<(), TestCaseError> {
    let baseline = round_trip(src).expect("round trip");

    let once = format_toml(src, &profile.style).map_err(|err| {
        TestCaseError::fail(format!(
            "format failed under {}: {err}\ninput was:\n{src}",
            profile.name
        ))
    })?;

    prop_assert!(
        once.parse::<toml_edit::DocumentMut>().is_ok(),
        "output does not parse under {}:\n{once}\ninput was:\n{src}",
        profile.name
    );

    let before = profile.normalize(&parse_tree(src).expect("input parses"));
    let after = profile.normalize(&parse_tree(&once).expect("output parses"));
    prop_assert_eq!(
        &before,
        &after,
        "value tree changed under {}, input was:\n{}",
        profile.name,
        src
    );

    let before = profile.spelling_view(&parse_spellings(src).expect("input parses"));
    let after = profile.spelling_view(&parse_spellings(&once).expect("output parses"));
    prop_assert_eq!(
        &before,
        &after,
        "value spelling changed under {}, input was:\n{}",
        profile.name,
        src
    );

    let twice = format_toml(&once, &profile.style).expect("output parses, so it formats");
    prop_assert_eq!(
        &once,
        &twice,
        "not idempotent under {}, input was:\n{}",
        profile.name,
        src
    );

    let expected = profile.comment_view(&baseline);
    let actual = profile.comment_view(&once);
    prop_assert_eq!(
        &expected,
        &actual,
        "comments changed under {}, input was:\n{}",
        profile.name,
        src
    );

    // Guarantee 6 is what `--toml-directives` switches off, so it is the one
    // property that is conditional on the style rather than universal over it.
    // Checked against a formatted baseline for the same reason as the comments:
    // a marker `toml_edit` moved past its partner is not our region.
    if profile.style.directives {
        let fenced = format_toml(&baseline, &profile.style).expect("baseline formats");
        prop_assert!(
            regions_survive(&baseline, &fenced),
            "directive region changed under {}\nexpected {:?}\n  actual {:?}\ninput was:\n{}",
            profile.name,
            regions(&baseline),
            regions(&fenced),
            baseline
        );
    }

    Ok(())
}

proptest! {
    #![proptest_config(config())]

    /// Guards the generator itself. Without this, a generator that quietly
    /// emitted unparseable documents would make every property below pass
    /// vacuously.
    #[test]
    fn generated_documents_are_valid_toml(doc in arb_doc()) {
        let src = doc.render();
        if let Err(err) = src.parse::<toml_edit::DocumentMut>() {
            prop_assert!(false, "generator emitted invalid TOML: {err}\n---\n{src}\n---");
        }
    }

    /// The seven shipped presets, so the properties cover what `--preset`
    /// actually gives a user.
    #[test]
    fn presets_hold_every_guarantee(doc in arb_doc()) {
        let src = doc.render();
        for profile in profiles() {
            check(&src, &profile)?;
        }
    }

    /// The presets are seven points in a space of several thousand
    /// combinations, and none of them crosses `compact`/`expand` with a narrow
    /// width or a tab indent. Generating the style makes the option surface an
    /// axis of the search rather than a fixed list.
    #[test]
    fn generated_styles_hold_every_guarantee(doc in arb_doc(), profile in arb_style()) {
        check(&doc.render(), &profile)?;
    }
}
