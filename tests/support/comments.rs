/// Every comment body in a TOML document, in source order.
///
/// This scans the rendered text rather than walking `toml_edit`'s `Decor`
/// values: a decor walk has to visit table decor, leaf and dotted key decor,
/// value decor, array and inline-table interiors and the document trailer, and
/// missing any one of those makes the comment-preservation property pass
/// vacuously. A scanner cannot miss a position, provided it knows where strings
/// are - hence the literal tracking below, so a `#` inside a string is never
/// read as a comment.
pub fn comments(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'#' => {
                let start = i + 1;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                out.push(src[start..i].trim_end_matches('\r').trim().to_owned());
            }
            b'"' | b'\'' => i = skip_string(bytes, i),
            _ => i += 1,
        }
    }

    out
}

/// Every comment body with the marker padding normalised away.
///
/// The formatter is documented to rewrite exactly that padding -- `##Banner`
/// becomes `## Banner`, `#   padded` becomes `# padded` -- so comparing raw
/// bodies reports the rule as a lost comment. The run of `#` that opens the
/// body is part of the key, so gaining or losing one is still a change; only
/// the whitespace around it is forgiven.
pub fn normalized(src: &str) -> Vec<String> {
    comments(src)
        .into_iter()
        .map(|body| {
            let text = body.trim_start_matches('#');
            format!("{}#{}", body.len() - text.len(), text.trim())
        })
        .collect()
}

/// Index just past the string literal starting at `open`.
pub fn skip_string(bytes: &[u8], open: usize) -> usize {
    let quote = bytes[open];
    let multiline = bytes[open..].starts_with(&[quote, quote, quote]);
    let escapes = quote == b'"';

    if multiline {
        let mut i = open + 3;
        while i < bytes.len() {
            if escapes && bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i..].starts_with(&[quote, quote, quote]) {
                // TOML allows up to two extra quotes to abut the delimiter.
                let mut end = i + 3;
                while end < bytes.len() && bytes[end] == quote && end - i < 5 {
                    end += 1;
                }
                return end;
            }
            i += 1;
        }
        return bytes.len();
    }

    let mut i = open + 1;
    while i < bytes.len() {
        if escapes && bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        // An unterminated single-line string cannot span a newline.
        if bytes[i] == b'\n' {
            return i;
        }
        i += 1;
    }
    bytes.len()
}
