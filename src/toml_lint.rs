use std::ops::Range;

use toml_edit::{Decor, Document, InlineTable, Item, Key, Table, Value};

use crate::toml_fmt::raw_to_str;

const ESCAPE_E: &str = r"\e escape is TOML 1.1 only";
const ESCAPE_X: &str = r"\xHH escape is TOML 1.1 only";
const TIME_WITHOUT_SECONDS: &str = "a time without seconds is TOML 1.1 only";
const INLINE_ACROSS_LINES: &str = "an inline table across lines is TOML 1.1 only";
const INLINE_TRAILING_COMMA: &str = "a trailing comma in an inline table is TOML 1.1 only";

/// A TOML 1.1-only construct, positioned in the text it was found in. Lines and
/// columns are 1-based, and a column counts characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlIssue {
    pub line: usize,
    pub column: usize,
    pub message: &'static str,
}

/// Every TOML 1.1-only construct in `text`, in source order.
///
/// The formatter never re-serialises a scalar, so running this over its
/// *output* covers both what the input carried and what the layout produced.
/// Text that does not parse yields nothing: the caller already reports that.
pub fn toml_1_0_issues(text: &str) -> Vec<TomlIssue> {
    let Ok(doc) = Document::parse(text) else {
        return Vec::new();
    };

    let mut scan = Scan {
        text,
        found: Vec::new(),
    };
    scan.table(doc.as_table());
    scan.found.sort_unstable_by_key(|(offset, _)| *offset);
    scan.found
        .into_iter()
        .map(|(offset, message)| {
            let (line, column) = locate(text, offset);
            TomlIssue {
                line,
                column,
                message,
            }
        })
        .collect()
}

struct Scan<'a> {
    text: &'a str,
    found: Vec<(usize, &'static str)>,
}

impl Scan<'_> {
    fn at(&mut self, span: Option<Range<usize>>, message: &'static str) {
        if let Some(span) = span {
            self.found.push((span.start, message));
        }
    }

    fn table(&mut self, table: &Table) {
        for (name, item) in table {
            if let Some((key, _)) = table.get_key_value(name) {
                self.key(key);
            }
            match item {
                Item::Value(value) => self.value(value),
                Item::Table(child) => self.table(child),
                Item::ArrayOfTables(children) => {
                    for child in children {
                        self.table(child);
                    }
                }
                Item::None => {}
            }
        }
    }

    fn value(&mut self, value: &Value) {
        match value {
            Value::String(string) => self.escapes(string.span()),
            Value::Datetime(datetime) => {
                if matches!(datetime.value().time, Some(time) if time.second.is_none()) {
                    self.at(datetime.span(), TIME_WITHOUT_SECONDS);
                }
            }
            Value::Array(array) => {
                for element in array {
                    self.value(element);
                }
            }
            Value::InlineTable(table) => self.inline_table(table),
            Value::Integer(_) | Value::Float(_) | Value::Boolean(_) => {}
        }
    }

    fn inline_table(&mut self, table: &InlineTable) {
        if table.trailing_comma() {
            self.at(table.span(), INLINE_TRAILING_COMMA);
        }
        if self.inline_spans_lines(table) {
            self.at(table.span(), INLINE_ACROSS_LINES);
        }
        for (name, value) in table {
            if let Some(key) = table.key(name) {
                self.key(key);
            }
            self.value(value);
        }
    }

    /// The whitespace between the braces that is not part of a value. A
    /// multi-line string sitting on one line of entries is TOML 1.0, so the
    /// value's own text is deliberately not consulted.
    fn inline_spans_lines(&self, table: &InlineTable) -> bool {
        if self.has_newline(Some(table.trailing())) {
            return true;
        }
        table.iter().any(|(name, value)| {
            table.key(name).is_some_and(|key| {
                self.decor_has_newline(key.leaf_decor())
                    || self.decor_has_newline(key.dotted_decor())
            }) || self.decor_has_newline(value.decor())
        })
    }

    fn decor_has_newline(&self, decor: &Decor) -> bool {
        self.has_newline(decor.prefix()) || self.has_newline(decor.suffix())
    }

    fn has_newline(&self, raw: Option<&toml_edit::RawString>) -> bool {
        raw_to_str(raw, self.text).contains('\n')
    }

    fn key(&mut self, key: &Key) {
        self.escapes(key.span());
    }

    /// A basic string is the only place an escape can appear, and the opening
    /// quote is what tells the two kinds apart in the raw text.
    fn escapes(&mut self, span: Option<Range<usize>>) {
        let Some(span) = span else {
            return;
        };
        let Some(repr) = self.text.get(span.clone()) else {
            return;
        };
        if repr.starts_with('\'') {
            return;
        }

        let bytes = repr.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'\\' {
                index += 1;
                continue;
            }
            let message = match bytes.get(index + 1) {
                Some(b'e') => Some(ESCAPE_E),
                Some(b'x') => Some(ESCAPE_X),
                _ => None,
            };
            if let Some(message) = message {
                self.found.push((span.start + index, message));
            }
            index += 2;
        }
    }
}

fn locate(text: &str, offset: usize) -> (usize, usize) {
    let Some(head) = text.get(..offset) else {
        return (1, 1);
    };
    let line_start = head.rfind('\n').map_or(0, |index| index + 1);
    (
        head.matches('\n').count() + 1,
        head[line_start..].chars().count() + 1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issues(text: &str) -> Vec<(usize, usize, &'static str)> {
        toml_1_0_issues(text)
            .into_iter()
            .map(|issue| (issue.line, issue.column, issue.message))
            .collect()
    }

    #[test]
    fn a_clean_document_reports_nothing() {
        assert_eq!(
            issues("a = \"x\"\nb = 07:32:00\nc = { d = 1, e = [1, 2] }\n"),
            []
        );
    }

    #[test]
    fn text_that_does_not_parse_reports_nothing() {
        assert_eq!(issues("a = \n"), []);
    }

    #[test]
    fn hex_and_escape_escapes_are_found_with_their_position() {
        assert_eq!(
            issues("a = 1\nkey = \"\\x41 and \\e[0m\"\n"),
            [(2, 8, ESCAPE_X), (2, 17, ESCAPE_E)]
        );
    }

    #[test]
    fn an_escaped_backslash_does_not_open_an_escape() {
        assert_eq!(issues("a = \"c:\\\\xff\"\n"), []);
        assert_eq!(issues("a = \"c:\\\\\\x41\"\n"), [(1, 10, ESCAPE_X)]);
    }

    #[test]
    fn a_literal_string_has_no_escapes_to_find() {
        assert_eq!(issues("a = '\\x41'\nb = '''\\e[0m'''\n"), []);
    }

    #[test]
    fn a_multi_line_basic_string_is_scanned() {
        assert_eq!(issues("a = \"\"\"\n\\e[0m\n\"\"\"\n"), [(2, 1, ESCAPE_E)]);
    }

    #[test]
    fn escapes_are_found_in_quoted_keys_and_headers() {
        assert_eq!(issues("\"\\x41\" = 1\n"), [(1, 2, ESCAPE_X)]);
        assert_eq!(issues("[a.\"\\e\"]\nx = 1\n"), [(1, 5, ESCAPE_E)]);
        assert_eq!(issues("[[\"\\e\"]]\nx = 1\n"), [(1, 4, ESCAPE_E)]);
    }

    #[test]
    fn escapes_are_found_at_every_depth() {
        assert_eq!(
            issues("a = { b = [{ c = \"\\e\" }] }\n"),
            [(1, 19, ESCAPE_E)]
        );
    }

    #[test]
    fn a_time_without_seconds_is_found_but_a_full_one_is_not() {
        assert_eq!(issues("a = 07:32\n"), [(1, 5, TIME_WITHOUT_SECONDS)]);
        assert_eq!(
            issues("a = 1979-05-27T07:32\n"),
            [(1, 5, TIME_WITHOUT_SECONDS)]
        );
        assert_eq!(issues("a = 07:32:00\nb = 1979-05-27\n"), []);
    }

    #[test]
    fn an_inline_table_across_lines_is_found() {
        assert_eq!(
            issues("a = {\n    b = 1\n}\n"),
            [(1, 5, INLINE_ACROSS_LINES)]
        );
        assert_eq!(
            issues("a = { b = 1, # note\n c = 2 }\n"),
            [(1, 5, INLINE_ACROSS_LINES)]
        );
    }

    #[test]
    fn a_multi_line_string_does_not_break_a_one_line_inline_table() {
        assert_eq!(issues("a = { b = \"\"\"\nx\n\"\"\" }\n"), []);
    }

    #[test]
    fn a_trailing_comma_is_found_only_inside_an_inline_table() {
        assert_eq!(issues("a = { b = 1, }\n"), [(1, 5, INLINE_TRAILING_COMMA)]);
        assert_eq!(issues("a = [\n    1,\n    2,\n]\n"), []);
    }
}
