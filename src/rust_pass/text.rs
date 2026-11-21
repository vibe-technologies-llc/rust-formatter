use std::ops::Range;

use proc_macro2::{LineColumn, Span, TokenStream, TokenTree};

pub fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(at, _)| at + 1))
        .collect()
}

pub fn token_ranges(
    tokens: TokenStream,
    text: &str,
    starts: &[usize],
    ranges: &mut Vec<Range<usize>>,
) {
    for tree in tokens {
        match tree {
            TokenTree::Group(group) => {
                ranges.push(byte_range(group.span_open(), text, starts));
                token_ranges(group.stream(), text, starts, ranges);
                ranges.push(byte_range(group.span_close(), text, starts));
            }
            TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_) => {
                ranges.push(byte_range(tree.span(), text, starts));
            }
        }
    }
}

pub fn byte_range(span: Span, text: &str, starts: &[usize]) -> Range<usize> {
    byte_offset(span.start(), text, starts)..byte_offset(span.end(), text, starts)
}

pub fn byte_offset(at: LineColumn, text: &str, starts: &[usize]) -> usize {
    let Some(&start) = starts.get(at.line.saturating_sub(1)) else {
        return text.len();
    };
    text[start..]
        .char_indices()
        .nth(at.column)
        .map_or(text.len(), |(offset, _)| start + offset)
}
