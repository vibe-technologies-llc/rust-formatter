use std::borrow::Cow;

use crate::{
    toml_directive::Regions,
    toml_scan::{self, single_line_string},
    toml_style::TomlStyle,
    toml_width,
};

/// Pads entries and same-line comments into columns, over the rendered text
/// rather than the document.
///
/// A value that collapses to a dotted key (`foo = { workspace = true }` becomes
/// `foo.workspace = true`) only takes its final key while it is being laid out,
/// and letting padding feed back into the width budget would let a wrap change
/// the grouping that produced it. Measuring the finished line settles both.
pub(crate) fn align<'a>(text: &'a str, style: &TomlStyle) -> Cow<'a, str> {
    if text.is_empty() {
        return Cow::Borrowed(text);
    }

    let tab = style.tab_width();
    let mut current = Cow::Borrowed(text);
    if style.align_entries {
        current = pad(&current, tab, Column::Equals, style.directives).map_or(current, Cow::Owned);
    }
    if style.align_comments {
        current = pad(&current, tab, Column::Comment, style.directives).map_or(current, Cow::Owned);
    }
    current
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    Equals,
    Comment,
}

/// A rendered line split at the point a column would be padded to.
struct Line<'a> {
    /// Everything up to and including the line terminator.
    raw: &'a str,
    /// `raw` without its terminator.
    content: &'a str,
    /// Byte offset in `content` of the first `=` outside a string.
    equals: Option<usize>,
    /// Byte offset in `content` of the `#` that opens a comment.
    comment: Option<usize>,
    /// Inside a `fmt: off` region, so it neither pads nor joins a group.
    frozen: bool,
}

impl Line<'_> {
    fn split(&self, column: Column) -> Option<usize> {
        if self.frozen {
            return None;
        }
        match column {
            Column::Equals => {
                let at = self.equals?;
                is_key_path(&self.content[..at]).then_some(at)
            }
            Column::Comment => {
                let at = self.comment?;
                (!self.content[..at].trim().is_empty()).then_some(at)
            }
        }
    }
}

fn pad(text: &str, tab: usize, column: Column, directives: bool) -> Option<String> {
    // The markers survive the render byte for byte, so the regions the document
    // carried are still findable in its output. Rescanned per pass: the first
    // one inserts padding, which moves every offset after it.
    let regions = if directives {
        Regions::scan(text)
    } else {
        Regions::none()
    };
    let lines = scan(text, &regions);
    let mut targets: Vec<Option<usize>> = vec![None; lines.len()];
    let mut group: Vec<usize> = Vec::new();
    let mut widest = 0;
    let mut indent = 0;

    for index in 0..=lines.len() {
        let head = lines
            .get(index)
            .and_then(|line| line.split(column).map(|at| line.content[..at].trim_end()));
        let joins = match (head, column) {
            (Some(head), Column::Equals) => group.is_empty() || leading_width(head, tab) == indent,
            (Some(_), Column::Comment) => true,
            (None, _) => false,
        };

        if !joins {
            flush(&group, widest, &mut targets);
            group.clear();
            widest = 0;
        }
        let Some(head) = head else {
            continue;
        };
        if group.is_empty() {
            indent = leading_width(head, tab);
        }
        widest = widest.max(toml_width::width(head, tab));
        group.push(index);
    }
    flush(&group, widest, &mut targets);

    if targets.iter().all(Option::is_none) {
        return None;
    }

    let mut out = String::with_capacity(text.len() + targets.len());
    for (line, target) in lines.iter().zip(&targets) {
        let Some(target) = *target else {
            out.push_str(line.raw);
            continue;
        };
        let at = line.split(column).expect("target implies a split point");
        let head = line.content[..at].trim_end();
        out.push_str(head);
        for _ in 0..=target - toml_width::width(head, tab) {
            out.push(' ');
        }
        out.push_str(&line.content[at..]);
        out.push_str(&line.raw[line.content.len()..]);
    }
    Some(out)
}

/// A group of one needs no padding, but it still has to be reproduced verbatim,
/// which `None` does more cheaply than a zero-width pad.
fn flush(group: &[usize], widest: usize, targets: &mut [Option<usize>]) {
    if group.len() < 2 {
        return;
    }
    for &index in group {
        targets[index] = Some(widest);
    }
}

fn leading_width(line: &str, tab: usize) -> usize {
    let indent = &line[..line.len() - line.trim_start().len()];
    toml_width::width(indent, tab)
}

fn scan<'a>(text: &'a str, regions: &Regions) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    let mut offset = 0;

    for (split, equals, comment) in toml_scan::lines(text) {
        let end = offset + split.raw.len();
        lines.push(Line {
            raw: split.raw,
            content: split.content,
            equals,
            comment,
            frozen: regions.hits(Some(offset..end)),
        });
        offset = end;
    }
    lines
}

/// Whether `text` is what a TOML entry writes to the left of its `=`: a bare,
/// quoted or dotted key. Anything else sharing a line — an inline table inside
/// a wrapped array, say — has an `=` that belongs to something nested.
fn is_key_path(text: &str) -> bool {
    let mut rest = text.trim();
    if rest.is_empty() {
        return false;
    }

    loop {
        let end = if let Some(&quote @ (b'"' | b'\'')) = rest.as_bytes().first() {
            let end = single_line_string(rest.as_bytes(), 1, quote, quote == b'"');
            if rest.as_bytes().get(end - 1) != Some(&quote) || end == 1 {
                return false;
            }
            end
        } else {
            let end = rest
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
                .count();
            if end == 0 {
                return false;
            }
            end
        };

        rest = rest[end..].trim_start();
        match rest.strip_prefix('.') {
            Some(tail) => rest = tail.trim_start(),
            None => return rest.is_empty(),
        }
        if rest.is_empty() {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toml_style::TomlStyle;

    fn style(entries: bool, comments: bool) -> TomlStyle {
        TomlStyle {
            align_entries: entries,
            align_comments: comments,
            ..TomlStyle::default()
        }
    }

    fn entries(text: &str) -> String {
        align(text, &style(true, false)).into_owned()
    }

    fn comments(text: &str) -> String {
        align(text, &style(false, true)).into_owned()
    }

    #[test]
    fn entries_pad_to_the_widest_key() {
        assert_eq!(
            entries("a = 1\nbbb = 2\ncc = 3\n"),
            "a   = 1\nbbb = 2\ncc  = 3\n"
        );
    }

    #[test]
    fn a_blank_line_ends_a_group() {
        assert_eq!(
            entries("a = 1\nbbb = 2\n\ncc = 3\ndddd = 4\n"),
            "a   = 1\nbbb = 2\n\ncc   = 3\ndddd = 4\n"
        );
    }

    #[test]
    fn a_lone_entry_is_left_alone() {
        assert_eq!(entries("a = 1\n"), "a = 1\n");
    }

    #[test]
    fn groups_do_not_cross_indentation() {
        assert_eq!(
            entries("x = {\n    a = 1,\n    bbb = 2\n}\n"),
            "x = {\n    a   = 1,\n    bbb = 2\n}\n"
        );
    }

    #[test]
    fn an_array_element_is_not_an_entry() {
        let source = "x = [\n    { a = 1 },\n    { bbb = 2 }\n]\n";
        assert_eq!(entries(source), source);
    }

    #[test]
    fn a_header_is_not_an_entry() {
        assert_eq!(
            entries("[a]\nk = 1\nlong = 2\n"),
            "[a]\nk    = 1\nlong = 2\n"
        );
    }

    #[test]
    fn an_equals_inside_a_string_is_not_a_key() {
        assert_eq!(
            entries("a = \"x = y\"\nbbb = 2\n"),
            "a   = \"x = y\"\nbbb = 2\n"
        );
    }

    #[test]
    fn a_dotted_key_aligns_on_its_whole_path() {
        assert_eq!(entries("a.b = 1\nc = 2\n"), "a.b = 1\nc   = 2\n");
    }

    #[test]
    fn comments_pad_to_the_widest_line() {
        assert_eq!(
            comments("a = 1 # one\nbbb = 22 # two\n"),
            "a = 1    # one\nbbb = 22 # two\n"
        );
    }

    #[test]
    fn a_comment_only_line_ends_a_group() {
        assert_eq!(
            comments("a = 1 # one\n# lone\nbbb = 22 # two\n"),
            "a = 1 # one\n# lone\nbbb = 22 # two\n"
        );
    }

    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        let source = "a = \"# no\"\nb = '# no'\n";
        assert_eq!(comments(source), source);
    }

    #[test]
    fn a_multi_line_string_hides_its_hashes() {
        let source = "a = \"\"\"\n# not a comment\nstill\"\"\"\nb = 1 # yes\nc = 2 # also\n";
        assert_eq!(
            comments(source),
            "a = \"\"\"\n# not a comment\nstill\"\"\"\nb = 1 # yes\nc = 2 # also\n"
        );
    }

    #[test]
    fn both_passes_compose() {
        assert_eq!(
            align("a = 1 # one\nbbb = 2 # two\n", &style(true, true)).into_owned(),
            "a   = 1 # one\nbbb = 2 # two\n"
        );
    }

    #[test]
    fn key_paths_are_recognised() {
        assert!(is_key_path("a"));
        assert!(is_key_path("  a.b.c "));
        assert!(is_key_path("\"quoted key\""));
        assert!(is_key_path("'lit' . bare"));
        assert!(!is_key_path(""));
        assert!(!is_key_path("{ a "));
        assert!(!is_key_path("a b"));
        assert!(!is_key_path("a."));
        assert!(!is_key_path("\"\"\"x\"\"\""));
    }

    #[test]
    fn a_file_without_a_final_newline_survives() {
        assert_eq!(entries("a = 1\nbbb = 2"), "a   = 1\nbbb = 2");
    }

    #[test]
    fn crlf_terminators_survive() {
        assert_eq!(entries("a = 1\r\nbbb = 2\r\n"), "a   = 1\r\nbbb = 2\r\n");
    }

    #[test]
    fn a_frozen_region_is_not_padded() {
        let source = "# fmt: off\na = 1\nbb = 2\n# fmt: on\n";
        assert_eq!(entries(source), source);
    }

    #[test]
    fn a_frozen_region_splits_the_group_around_it() {
        assert_eq!(
            entries("a = 1\nbb = 2\n# fmt: off\nx=9\n# fmt: on\nc = 3\ndddd = 4\n"),
            "a  = 1\nbb = 2\n# fmt: off\nx=9\n# fmt: on\nc    = 3\ndddd = 4\n"
        );
    }

    /// The entry pass inserts padding, so the comment pass sees different byte
    /// offsets and has to find the regions again for itself.
    #[test]
    fn the_second_pass_finds_the_region_where_the_first_pass_left_it() {
        let source =
            "aaaaaaa = 1 # pad me\nb = 2 # and me\n# fmt: off\nc = 3 # not me\ncc = 4 # nor me\n";
        let style = TomlStyle {
            align_entries: true,
            align_comments: true,
            ..TomlStyle::default()
        };
        assert_eq!(
            align(source, &style).into_owned(),
            "aaaaaaa = 1 # pad me\nb       = 2 # and me\n# fmt: off\nc = 3 # not me\ncc = 4 # nor me\n"
        );
    }

    #[test]
    fn directives_off_pads_through_the_markers() {
        let source = "# fmt: off\na = 1\nbb = 2\n# fmt: on\n";
        let style = TomlStyle {
            align_entries: true,
            directives: false,
            ..TomlStyle::default()
        };
        assert_eq!(
            align(source, &style).into_owned(),
            "# fmt: off\na  = 1\nbb = 2\n# fmt: on\n"
        );
    }
}
