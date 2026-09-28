/// The string state a line ends in, which is what decides whether the next
/// line's `#` opens a comment or sits inside a multi-line string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Code,
    MultiBasic,
    MultiLiteral,
}

/// Walks one line, returning the offset of the first `=` outside a string, the
/// offset of the `#` that opens a comment, and the state the next line opens in.
pub(crate) fn scan_line(content: &str, mut mode: Mode) -> (Option<usize>, Option<usize>, Mode) {
    let bytes = content.as_bytes();
    let mut equals = None;
    let mut index = 0;

    while index < bytes.len() {
        match mode {
            Mode::MultiBasic => match closing_run(bytes, index, b'"', true) {
                Some(end) => {
                    mode = Mode::Code;
                    index = end;
                }
                None => return (equals, None, mode),
            },
            Mode::MultiLiteral => match closing_run(bytes, index, b'\'', false) {
                Some(end) => {
                    mode = Mode::Code;
                    index = end;
                }
                None => return (equals, None, mode),
            },
            Mode::Code => match bytes[index] {
                b'#' => return (equals, Some(index), mode),
                b'=' => {
                    equals = equals.or(Some(index));
                    index += 1;
                }
                quote @ (b'"' | b'\'') => {
                    let escapes = quote == b'"';
                    if bytes[index..].starts_with(&[quote; 3]) {
                        mode = if escapes {
                            Mode::MultiBasic
                        } else {
                            Mode::MultiLiteral
                        };
                        index += 3;
                    } else {
                        index = single_line_string(bytes, index + 1, quote, escapes);
                    }
                }
                _ => index += 1,
            },
        }
    }
    (equals, None, mode)
}

/// Offset just past a multi-line delimiter, which takes the last three quotes
/// of a run so that `""""` is one quote of content and a close.
fn closing_run(bytes: &[u8], from: usize, quote: u8, escapes: bool) -> Option<usize> {
    let mut index = from;
    while index < bytes.len() {
        if escapes && bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] != quote {
            index += 1;
            continue;
        }
        let run = bytes[index..].iter().take_while(|&&b| b == quote).count();
        if run >= 3 {
            return Some(index + run);
        }
        index += run;
    }
    None
}

/// Offset just past the closing quote, or the end of the line when the string
/// is unterminated — which valid TOML never is.
pub(crate) fn single_line_string(bytes: &[u8], from: usize, quote: u8, escapes: bool) -> usize {
    let mut index = from;
    while index < bytes.len() {
        if escapes && bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == quote {
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

/// A line split from `text`, with its terminator kept apart so a caller can
/// reproduce CRLF and a missing final newline unchanged.
pub(crate) struct Split<'a> {
    pub(crate) raw: &'a str,
    pub(crate) content: &'a str,
}

/// Splits `text` into lines and threads the string state through them, so each
/// line arrives with the offsets `scan_line` found for it.
pub(crate) fn lines(text: &str) -> impl Iterator<Item = (Split<'_>, Option<usize>, Option<usize>)> {
    let mut mode = Mode::Code;
    text.split_inclusive('\n').map(move |raw| {
        let content = raw
            .strip_suffix('\n')
            .map_or(raw, |rest| rest.strip_suffix('\r').unwrap_or(rest));
        let (equals, comment, next) = scan_line(content, mode);
        mode = next;
        (Split { raw, content }, equals, comment)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multi_line_string_ends_and_code_resumes() {
        let (equals, comment, mode) = scan_line("still\"\"\" # tail", Mode::MultiBasic);
        assert_eq!(equals, None);
        assert_eq!(comment, Some(9));
        assert_eq!(mode, Mode::Code);
    }

    #[test]
    fn four_quotes_close_a_multi_line_string() {
        let (_, comment, mode) = scan_line("a\"\"\"\" # tail", Mode::MultiBasic);
        assert_eq!(comment, Some(6));
        assert_eq!(mode, Mode::Code);
    }

    #[test]
    fn an_escaped_quote_does_not_close_a_multi_line_string() {
        let (_, comment, mode) = scan_line("\\\"\"\" # no", Mode::MultiBasic);
        assert_eq!(comment, None);
        assert_eq!(mode, Mode::MultiBasic);
    }

    #[test]
    fn a_hash_inside_a_single_line_string_is_not_a_comment() {
        let (_, comment, _) = scan_line("a = \"# no\" # yes", Mode::Code);
        assert_eq!(comment, Some(11));
    }

    #[test]
    fn lines_keep_their_terminators() {
        let split: Vec<_> = lines("a = 1\r\nb = 2")
            .map(|(split, ..)| split.raw)
            .collect();
        assert_eq!(split, ["a = 1\r\n", "b = 2"]);
    }

    #[test]
    fn a_multi_line_string_hides_the_hashes_of_later_lines() {
        let found: Vec<_> = lines("a = \"\"\"\n# hidden\n\"\"\"\n# real\n")
            .map(|(_, _, comment)| comment)
            .collect();
        assert_eq!(found, [None, None, None, Some(0)]);
    }
}
