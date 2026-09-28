/// The exact text of every `fmt: off` region in a TOML document, in source
/// order.
///
/// Written against the raw bytes rather than reusing `toml_directive`, for the
/// same reason `comments` is: an oracle that shares the implementation's idea of
/// where a marker is cannot catch the implementation misplacing one. It knows
/// only that a `#` inside a string is not a comment.
///
/// A missing final newline is supplied first. The formatter always writes one,
/// and a region that reaches the end of the document would otherwise be reported
/// as changed for a reason that has nothing to do with the region.
pub fn regions(src: &str) -> Vec<String> {
    let owned;
    let src = if src.is_empty() || src.ends_with('\n') {
        src
    } else {
        owned = format!("{src}\n");
        &owned
    };

    let mut out = Vec::new();
    let mut open: Option<usize> = None;

    for (at, body) in comment_offsets(src) {
        match (marker(body), open) {
            (Some(Marker::Off), None) => open = Some(line_start(src, at)),
            (Some(Marker::On), Some(start)) => {
                out.push(src[start..line_end(src, at)].to_owned());
                open = None;
            }
            _ => {}
        }
    }

    if let Some(start) = open {
        out.push(src[start..].to_owned());
    }
    out
}

enum Marker {
    Off,
    On,
}

/// Longest marker body, as `docs/toml-style.md` states it: spacing inside a
/// marker is free only up to here, and a longer comment is prose. Mirrored
/// rather than imported because an oracle that shared the implementation's
/// constant could not catch the implementation changing it -- which is what
/// `a_marker_is_capped_at_the_documented_length` in `src/toml_directive.rs`
/// pins from the other side.
const MAX_MARKER: usize = 40;

/// A marker is the whole body of a comment, wherever on its line that comment
/// opens.
fn marker(body: &str) -> Option<Marker> {
    let body = body.trim();
    if body.is_empty() || body.len() > MAX_MARKER {
        return None;
    }
    let squeezed: String = body
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    match squeezed.as_str() {
        "fmt:off" | "taplo:fmt-off" | "rust-formatter:fmt-off" => Some(Marker::Off),
        "fmt:on" | "taplo:fmt-on" | "rust-formatter:fmt-on" => Some(Marker::On),
        _ => None,
    }
}

/// Every comment in `src` as `(offset of its `#`, body without the newline)`.
fn comment_offsets(src: &str) -> Vec<(usize, &str)> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'#' => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                out.push((start, src[start + 1..i].trim_end_matches('\r')));
            }
            b'"' | b'\'' => i = super::comments::skip_string(bytes, i),
            _ => i += 1,
        }
    }
    out
}

fn line_start(src: &str, at: usize) -> usize {
    src[..at].rfind('\n').map_or(0, |newline| newline + 1)
}

fn line_end(src: &str, at: usize) -> usize {
    src[at..]
        .find('\n')
        .map_or(src.len(), |newline| at + newline + 1)
}

/// Whether the regions of `after` are exactly those of `before`.
///
/// Equality, not containment: a marker counts wherever it is written, so the
/// formatter can neither create one by moving a comment onto a line of its own
/// nor lose one, and the fenced bytes have to come out whole.
pub fn regions_survive(before: &str, after: &str) -> bool {
    regions(before) == regions(after)
}
