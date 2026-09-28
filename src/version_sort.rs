use std::cmp::Ordering;

/// Compares two strings the way the Rust Style Guide's "Sorting" section
/// specifies, so that `x8` sorts before `x16`.
///
/// The guide describes the comparison as chunk-by-chunk, but a literal chunk
/// comparison contradicts its own published example: `u_zzz` is one chunk while
/// `u8` starts with the chunk `u`, and every lexicographic rule puts the prefix
/// first, whereas the example requires `u_zzz` before `u8`. The reading that
/// reproduces the whole example is a single character walk that switches to a
/// numeric comparison wherever both sides are looking at a digit.
pub(crate) fn version_cmp(a: &str, b: &str) -> Ordering {
    let mut left = a;
    let mut right = b;
    // Which side carried more digits at the earliest numeric chunk that was
    // equal in value but not in width. Only that first one breaks a tie.
    let mut leading_zeroes = Ordering::Equal;

    loop {
        let (Some(x), Some(y)) = (left.chars().next(), right.chars().next()) else {
            return match (left.is_empty(), right.is_empty()) {
                (true, true) => leading_zeroes.reverse(),
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => unreachable!(),
            };
        };

        if x.is_ascii_digit() && y.is_ascii_digit() {
            let (x, x_rest) = split_digits(left);
            let (y, y_rest) = split_digits(right);
            match numeric_cmp(x, y) {
                Ordering::Equal => {
                    if leading_zeroes == Ordering::Equal {
                        leading_zeroes = x.len().cmp(&y.len());
                    }
                }
                other => return other,
            }
            left = x_rest;
            right = y_rest;
            continue;
        }

        match rank(x).cmp(&rank(y)) {
            Ordering::Equal => {}
            other => return other,
        }
        left = &left[x.len_utf8()..];
        right = &right[y.len_utf8()..];
    }
}

/// Sorts characters into the four tiers the guide names, then by code point
/// within a tier. `_` outranking every character but a space is what makes this
/// more than a `char` comparison: it has to beat digits and `-` as well.
fn rank(ch: char) -> (u8, char) {
    match ch {
        ' ' => (0, ch),
        '_' => (1, ch),
        _ if !ch.is_lowercase() => (2, ch),
        _ => (3, ch),
    }
}

fn split_digits(text: &str) -> (&str, &str) {
    let end = text
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(text.len());
    text.split_at(end)
}

fn numeric_cmp(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ordered example from the style guide's "Sorting" section, verbatim.
    /// `ZY_XW` appears twice upstream; a stable sort is indifferent to it, a
    /// strict-ordering assertion would not be.
    const ORDERED: &[&str] = &[
        "_ZYXW", "_abcd", "A2", "ABCD", "Z_YXW", "ZY_XW", "ZY_XW", "ZYXW", "ZYXW_", "a1", "abcd",
        "u_zzz", "u8", "u16", "u32", "u64", "u128", "u256", "ua", "usize", "uz", "v000", "v00",
        "v0", "v0s", "v00t", "v0u", "v001", "v01", "v1", "v009", "v09", "v9", "v010", "v10",
        "w005s09t", "w5s009t", "x64", "x86", "x86_32", "x86_64", "x86_128", "x87", "zyxw",
    ];

    #[test]
    fn the_style_guide_example_is_already_sorted() {
        for pair in ORDERED.windows(2) {
            assert_ne!(
                version_cmp(pair[0], pair[1]),
                Ordering::Greater,
                "{} should not sort after {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn the_style_guide_example_is_a_fixed_point() {
        let mut reversed: Vec<&str> = ORDERED.iter().copied().rev().collect();
        reversed.sort_by(|a, b| version_cmp(a, b));
        assert_eq!(reversed, ORDERED);
    }

    #[test]
    fn numeric_chunks_compare_by_value() {
        assert_eq!(version_cmp("x8", "x16"), Ordering::Less);
        assert_eq!(version_cmp("x16", "x8"), Ordering::Greater);
        assert_eq!(version_cmp("a9b", "a10b"), Ordering::Less);
        assert_eq!(version_cmp("a0009", "a9"), Ordering::Less);
    }

    #[test]
    fn underscore_sorts_before_every_other_character() {
        assert_eq!(version_cmp("x86_64", "x86-64"), Ordering::Less);
        assert_eq!(version_cmp("a_", "a0"), Ordering::Less);
        assert_eq!(version_cmp("a_", "aA"), Ordering::Less);
        assert_eq!(version_cmp("a ", "a_"), Ordering::Less);
    }

    #[test]
    fn non_lowercase_sorts_before_lowercase() {
        assert_eq!(version_cmp("Zed", "aaa"), Ordering::Less);
        assert_eq!(version_cmp("Bee", "Zed"), Ordering::Less);
        assert_eq!(version_cmp("1", "a"), Ordering::Less);
    }

    #[test]
    fn more_leading_zeroes_wins_only_when_nothing_else_differs() {
        assert_eq!(version_cmp("v000", "v0"), Ordering::Less);
        assert_eq!(version_cmp("v0s", "v00t"), Ordering::Less);
        assert_eq!(version_cmp("w005s09t", "w5s009t"), Ordering::Less);
    }

    #[test]
    fn equal_strings_compare_equal() {
        assert_eq!(version_cmp("", ""), Ordering::Equal);
        assert_eq!(version_cmp("serde", "serde"), Ordering::Equal);
        assert_eq!(version_cmp("", "a"), Ordering::Less);
    }

    #[test]
    fn a_multibyte_character_does_not_split_a_boundary() {
        assert_eq!(version_cmp("版1", "版2"), Ordering::Less);
        assert_eq!(version_cmp("a版", "a版"), Ordering::Equal);
    }
}
