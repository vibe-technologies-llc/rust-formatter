/// Comment bodies in a Rust source, in source order, with markers stripped.
///
/// Written against the raw bytes rather than rustc's lexer: an oracle that
/// shared the implementation's idea of where a comment is cannot catch the
/// implementation dropping one. It knows only that comment syntax inside a
/// string, a raw string, a byte/C string or a char literal is not a comment,
/// and that block comments nest.
pub fn comments(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if let Some(next) = skip_literal(bytes, i) {
            i = next;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'/' => {
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    out.push(line_body(&src[start..i]));
                    continue;
                }
                b'*' => {
                    let (end, closed) = skip_block(bytes, i);
                    let inner = if closed { &src[i..end] } else { &src[i..] };
                    out.push(block_body(inner, closed));
                    i = end;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    out
}

/// Whitespace-collapsed concatenation of every comment body.
///
/// `wrap_comments` and `normalize_comments` may turn a block into line comments
/// and reflow the lines; they must not drop a word. Decorative `*` prefixes on
/// block-comment continuation lines are stripped before the collapse, so a
/// `/** * foo */` doc comment fingerprints the same as `/// foo`.
pub fn fingerprint(src: &str) -> String {
    collapse(
        &comments(src)
            .iter()
            .map(|body| normalize_body(body))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// The item immediately following each `#[rustfmt::skip]` / `#![rustfmt::skip]`,
/// as a substring of `src`. Interior spacing that rustfmt would otherwise
/// collapse has to appear in the formatted output.
pub fn skip_regions(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if let Some(next) = skip_literal(bytes, i) {
            i = next;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && matches!(bytes[i + 1], b'/' | b'*') {
            i = skip_comment(bytes, i);
            continue;
        }
        if bytes[i] == b'#'
            && let Some((inner, after)) = attribute_at(bytes, i)
            && is_skip_attr(inner)
        {
            let item_start = skip_ws_comments_and_attrs(bytes, after);
            if let Some(item_end) = item_end(bytes, item_start) {
                out.push(src[item_start..item_end].to_owned());
                i = item_end;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn line_body(text: &str) -> String {
    let rest = if text.starts_with("////") {
        &text[2..]
    } else if let Some(rest) = text.strip_prefix("///") {
        rest
    } else if let Some(rest) = text.strip_prefix("//!") {
        rest
    } else {
        &text[2..]
    };
    rest.trim_end_matches('\r').to_owned()
}

fn block_body(text: &str, closed: bool) -> String {
    let mut inner = if text.starts_with("/***") {
        &text[2..]
    } else if let Some(rest) = text.strip_prefix("/**") {
        rest
    } else if let Some(rest) = text.strip_prefix("/*!") {
        rest
    } else {
        &text[2..]
    };
    if closed {
        inner = inner.strip_suffix("*/").unwrap_or(inner);
    }
    inner.to_owned()
}

fn normalize_body(body: &str) -> String {
    let mut parts = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        let stripped = trimmed.strip_prefix('*').map_or(trimmed, str::trim_start);
        if !stripped.is_empty() {
            parts.push(stripped);
        }
    }
    collapse(&parts.join(" "))
}

fn collapse(text: &str) -> String {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn at_boundary(bytes: &[u8], i: usize) -> bool {
    i == 0 || !is_ident_continue(bytes[i - 1])
}

/// If `i` starts a string, raw string, byte/C string or char literal, the
/// index just past it. Lifetimes are consumed too, so `'a` is never read as
/// an unclosed char.
fn skip_literal(bytes: &[u8], i: usize) -> Option<usize> {
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == b'\'' {
        return Some(skip_char_or_lifetime(bytes, i));
    }
    if !at_boundary(bytes, i) {
        return None;
    }

    let rest = &bytes[i..];
    if let Some(after) = strip_raw_prefix(rest)
        && (after.first() == Some(&b'"') || after.first() == Some(&b'#'))
    {
        let start = i + (rest.len() - after.len());
        return Some(skip_raw(bytes, start));
    }
    if rest.first() == Some(&b'b') || rest.first() == Some(&b'c') {
        if rest.len() > 1 && rest[1] == b'\'' {
            return Some(skip_char_or_lifetime(bytes, i + 1));
        }
        if rest.len() > 1 && rest[1] == b'"' {
            return Some(skip_cooked(bytes, i + 1));
        }
        return None;
    }
    if rest.first() == Some(&b'"') {
        return Some(skip_cooked(bytes, i));
    }
    None
}

fn strip_raw_prefix(rest: &[u8]) -> Option<&[u8]> {
    if rest.starts_with(b"br") || rest.starts_with(b"cr") {
        return Some(&rest[2..]);
    }
    if rest.first() == Some(&b'r') {
        return Some(&rest[1..]);
    }
    None
}

fn skip_cooked(bytes: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    let mut escaped = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            i += 1;
            continue;
        }
        if byte == b'"' {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn skip_raw(bytes: &[u8], i: usize) -> usize {
    let mut hashes = 0;
    let mut j = i;
    while j < bytes.len() && bytes[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'"' {
        return i;
    }
    j += 1;
    while j < bytes.len() {
        if bytes[j] == b'"' {
            let mut k = 0;
            while j + 1 + k < bytes.len() && bytes[j + 1 + k] == b'#' && k < hashes {
                k += 1;
            }
            if k == hashes {
                return j + 1 + hashes;
            }
        }
        j += 1;
    }
    bytes.len()
}

fn skip_char_or_lifetime(bytes: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    if i >= bytes.len() {
        return bytes.len();
    }
    if bytes[i] == b'\\' {
        i += 1;
        if i < bytes.len() && bytes[i] == b'u' {
            i += 1;
            if i < bytes.len() && bytes[i] == b'{' {
                i += 1;
                while i < bytes.len() && bytes[i] != b'}' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
        } else if i < bytes.len() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'\'' {
            return i + 1;
        }
        return i;
    }
    let next = i + utf8_len(bytes[i]);
    if next < bytes.len() && bytes[next] == b'\'' {
        return next + 1;
    }
    while i < bytes.len() && is_ident_continue(bytes[i]) {
        i += 1;
    }
    i
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

fn skip_block(bytes: &[u8], open: usize) -> (usize, bool) {
    let mut i = open + 2;
    let mut depth = 1usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            depth += 1;
            i += 2;
            continue;
        }
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return (i, true);
            }
            continue;
        }
        i += 1;
    }
    (bytes.len(), false)
}

fn skip_comment(bytes: &[u8], i: usize) -> usize {
    if bytes[i + 1] == b'/' {
        let mut j = i + 2;
        while j < bytes.len() && bytes[j] != b'\n' {
            j += 1;
        }
        return j;
    }
    skip_block(bytes, i).0
}

fn attribute_at(bytes: &[u8], i: usize) -> Option<(&str, usize)> {
    if bytes[i] != b'#' {
        return None;
    }
    let mut j = i + 1;
    if j < bytes.len() && bytes[j] == b'!' {
        j += 1;
    }
    j = skip_ws(bytes, j);
    if j >= bytes.len() || bytes[j] != b'[' {
        return None;
    }
    let inner_start = j + 1;
    let mut depth = 1usize;
    j += 1;
    while j < bytes.len() {
        if let Some(next) = skip_literal(bytes, j) {
            j = next;
            continue;
        }
        match bytes[j] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    let inner = std::str::from_utf8(&bytes[inner_start..j]).ok()?;
                    return Some((inner, j + 1));
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

fn is_skip_attr(inner: &str) -> bool {
    let squeezed: String = inner.chars().filter(|ch| !ch.is_whitespace()).collect();
    squeezed == "rustfmt::skip"
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn skip_ws_comments_and_attrs(bytes: &[u8], mut i: usize) -> usize {
    loop {
        i = skip_ws(bytes, i);
        if i + 1 < bytes.len() && bytes[i] == b'/' && matches!(bytes[i + 1], b'/' | b'*') {
            i = skip_comment(bytes, i);
            continue;
        }
        if i < bytes.len()
            && bytes[i] == b'#'
            && let Some((inner, after)) = attribute_at(bytes, i)
        {
            if is_skip_attr(inner) {
                return i;
            }
            i = after;
            continue;
        }
        return i;
    }
}

fn item_end(bytes: &[u8], start: usize) -> Option<usize> {
    if start >= bytes.len() {
        return None;
    }
    let mut i = start;
    let mut round = 0usize;
    let mut square = 0usize;
    let mut curly = 0usize;
    let mut seen_curly = false;

    while i < bytes.len() {
        if let Some(next) = skip_literal(bytes, i) {
            i = next;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && matches!(bytes[i + 1], b'/' | b'*') {
            i = skip_comment(bytes, i);
            continue;
        }
        match bytes[i] {
            b'(' => round += 1,
            b')' => round = round.saturating_sub(1),
            b'[' => square += 1,
            b']' => square = square.saturating_sub(1),
            b'{' => {
                curly += 1;
                seen_curly = true;
            }
            b'}' => {
                curly = curly.saturating_sub(1);
                if seen_curly && round == 0 && square == 0 && curly == 0 {
                    return Some(i + 1);
                }
            }
            b';' if round == 0 && square == 0 && curly == 0 => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}
