use std::{collections::HashMap, ops::Range};

use toml_edit::{Decor, Item, Key, RawString, Table};

use crate::toml_scan;

/// Longest marker body plus room for the spacing a writer may put in it. A
/// comment past this length cannot be a marker, which keeps prose off the
/// normalizing path below.
const MAX_MARKER: usize = 40;

const OFF: &[&str] = &["fmt:off", "taplo:fmt-off", "rust-formatter:fmt-off"];
const ON: &[&str] = &["fmt:on", "taplo:fmt-on", "rust-formatter:fmt-on"];

/// The byte ranges a document asked to be left alone, each covering whole lines
/// from the `off` marker through the `on` marker that closes it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Regions(Vec<Range<usize>>);

impl Regions {
    pub(crate) fn none() -> Self {
        Self(Vec::new())
    }

    /// An `off` with no `on` after it runs to the end of the document; an `on`
    /// with no `off` before it, and an `off` inside an open region, are ignored.
    pub(crate) fn scan(text: &str) -> Self {
        // Every marker is a comment, and most documents hold no comment at all.
        if !text.contains('#') {
            return Self::none();
        }

        let mut spans: Vec<Range<usize>> = Vec::new();
        let mut open: Option<usize> = None;
        let mut offset = 0;

        for (split, _, comment) in toml_scan::lines(text) {
            let end = offset + split.raw.len();
            if let Some(at) = comment {
                match (marker(&split.content[at + 1..]), open) {
                    (Some(Marker::Off), None) => open = Some(offset),
                    (Some(Marker::On), Some(start)) => {
                        spans.push(start..end);
                        open = None;
                    }
                    _ => {}
                }
            }
            offset = end;
        }

        if let Some(start) = open {
            spans.push(start..text.len());
        }
        Self(spans)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn spans(&self) -> &[Range<usize>] {
        &self.0
    }

    /// Whether `span` overlaps a region. An absent or empty span never does:
    /// there is nothing of it inside the markers to preserve.
    pub(crate) fn hits(&self, span: Option<Range<usize>>) -> bool {
        let Some(span) = span else {
            return false;
        };
        self.0
            .iter()
            .any(|region| span.start < region.end && region.start < span.end)
    }

    /// How much of `span` the directives cover, in offsets relative to its
    /// start. Two regions with a gap between them are reported as one stretch,
    /// which freezes the gap as well: never less than was asked for.
    fn cover(&self, span: Option<Range<usize>>) -> Option<Cover> {
        let span = span?;
        let mut bounds: Option<(usize, usize)> = None;

        for region in &self.0 {
            if span.start >= region.end || region.start >= span.end {
                continue;
            }
            let from = region.start.saturating_sub(span.start);
            let to = region.end.min(span.end) - span.start;
            bounds = Some(match bounds {
                Some((seen, was)) => (seen.min(from), was.max(to)),
                None => (from, to),
            });
        }

        let (from, to) = bounds?;
        Some(Cover {
            from,
            to,
            before: self.encloses(span.start),
            beyond: self.encloses(span.end),
        })
    }

    /// Whether a byte written at `offset` would land inside a region. A region
    /// begins at the start of a line, so writing at that very offset lands on
    /// the first line it covers; a region that ends exactly here has already
    /// closed, since its last byte is the newline before this one.
    fn encloses(&self, offset: usize) -> bool {
        self.0
            .iter()
            .any(|region| region.start <= offset && offset < region.end)
    }

    /// The text of each region, in document order.
    #[cfg(test)]
    pub(crate) fn texts<'a>(&self, text: &'a str) -> Vec<&'a str> {
        self.0.iter().map(|region| &text[region.clone()]).collect()
    }
}

/// The stretch of one decor run that a directive covers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cover {
    /// Offsets into the run. Everything outside them is still the formatter's.
    pub(crate) from: usize,
    pub(crate) to: usize,
    /// A region is open where this run starts, so nothing may be written above
    /// it.
    pub(crate) before: bool,
    /// A region is open where this run ends, so nothing may be written below it.
    pub(crate) beyond: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    Off,
    On,
}

/// A marker is the whole body of a comment, wherever that comment sits.
/// Spacing and case are free; anything trailing the marker is not one.
///
/// Position-independence is what makes the regions stable. The formatter hoists
/// a comment written between `=` and its value onto a line of its own, so a rule
/// that asked for a line of its own would see a different set of markers on the
/// second pass than on the first.
fn marker(body: &str) -> Option<Marker> {
    let body = body.trim();
    if body.is_empty() || body.len() > MAX_MARKER {
        return None;
    }

    let mut squeezed = String::with_capacity(body.len());
    for ch in body.chars().filter(|ch| !ch.is_whitespace()) {
        squeezed.extend(ch.to_lowercase());
    }

    if OFF.contains(&squeezed.as_str()) {
        return Some(Marker::Off);
    }
    ON.contains(&squeezed.as_str()).then_some(Marker::On)
}

/// What a region covers, laid over the document's own shape.
///
/// Built from the spanned parse before anything mutates it, and keyed by
/// `Key::get` rather than by position: sorting only permutes, and key
/// normalization rewrites a key's spelling but never its value, so the path to
/// an entry outlives both.
#[derive(Debug)]
pub(crate) struct Frozen {
    /// The entry's own text: its `key = value` line, or a table's `[header]`.
    own: bool,
    /// How much of the whitespace and comment run above it a directive covers,
    /// which is where a marker that introduces the entry lives.
    prefix: Option<Cover>,
    /// Whether rewriting the `Key` this entry is stored under would touch a
    /// frozen line. One `Key` renders on every line its path is written on: the
    /// first segment of `["with space".a]` and `["with space".b]` is one object,
    /// as is the header of every `[[bin]]` element and the `b` of `b.x = 1` and
    /// `b.y = 2`. A single frozen line among them settles it for all.
    keyed: bool,
    /// Anything at all under here is frozen, which is what makes an entry a wall
    /// the surrounding entries cannot be sorted across.
    touched: bool,
    children: HashMap<Box<str>, Vec<Frozen>>,
}

/// The whole document's freeze map.
#[derive(Debug)]
pub(crate) struct Freeze {
    root: Frozen,
    trailing: Option<Cover>,
}

impl Freeze {
    pub(crate) fn build(root: &Table, trailing: &RawString, regions: &Regions) -> Self {
        Self {
            root: Frozen {
                own: false,
                prefix: None,
                keyed: false,
                touched: true,
                children: children_of(root, regions).children,
            },
            trailing: regions.cover(trailing.span()),
        }
    }

    pub(crate) fn guard(&self) -> Guard<'_> {
        Guard(Some(&self.root))
    }

    pub(crate) fn trailing(&self) -> Option<Cover> {
        self.trailing
    }
}

/// A borrowed position in a [`Freeze`], or nowhere at all — which is what every
/// pass gets when the document carries no directives.
#[derive(Clone, Copy, Default)]
pub(crate) struct Guard<'a>(Option<&'a Frozen>);

impl<'a> Guard<'a> {
    pub(crate) fn of(freeze: Option<&'a Freeze>) -> Self {
        freeze.map_or_else(Self::default, Freeze::guard)
    }

    pub(crate) fn own(self) -> bool {
        self.0.is_some_and(|node| node.own)
    }

    pub(crate) fn prefix(self) -> Option<Cover> {
        self.0.and_then(|node| node.prefix)
    }

    /// One table behind `key`: the only one, or one element of an array of
    /// tables.
    pub(crate) fn child(self, key: &str, index: usize) -> Self {
        Self(self.entry(key).get(index))
    }

    /// Whether rewriting the `Key` this entry is stored under would touch a
    /// frozen line.
    pub(crate) fn key_frozen(self, key: &str) -> bool {
        self.entry(key).iter().any(|node| node.keyed)
    }

    pub(crate) fn touched(self, key: &str) -> bool {
        self.entry(key).iter().any(|node| node.touched)
    }

    fn entry(self, key: &str) -> &'a [Frozen] {
        self.0
            .and_then(|node| node.children.get(key))
            .map_or(&[], Vec::as_slice)
    }
}

/// One table's entries, and the two ways a parent key can be dragged into a
/// region by them.
struct Built {
    children: HashMap<Box<str>, Vec<Frozen>>,
    /// A frozen line under a child that writes its own header. A header table's
    /// key renders on those and nowhere else.
    headers: bool,
    /// A frozen line under any child at all. A dotted key's segments render on
    /// every leaf line beneath them.
    any: bool,
}

/// Untouched subtrees are dropped rather than recorded, so a lookup that finds
/// nothing means nothing below it is frozen.
fn children_of(table: &Table, regions: &Regions) -> Built {
    let mut children = HashMap::new();
    let mut headers = false;
    let mut any = false;

    for (name, item) in table {
        let key = table.key(name);
        let key_span = key.and_then(Key::span);
        let key_prefix = key.map(Key::leaf_decor).and_then(prefix_span);

        let nodes = match item {
            Item::Value(value) => {
                let own = regions.hits(key_span) || regions.hits(value.span());
                let prefix = regions.cover(key_prefix);
                vec![Frozen {
                    own,
                    prefix,
                    keyed: own,
                    touched: own || prefix.is_some(),
                    children: HashMap::new(),
                }]
            }
            Item::Table(child) => vec![frozen_table(
                child,
                key_span.as_ref(),
                key_prefix.as_ref(),
                regions,
            )],
            Item::ArrayOfTables(elements) => elements
                .iter()
                .map(|child| frozen_table(child, key_span.as_ref(), key_prefix.as_ref(), regions))
                .collect(),
            Item::None => continue,
        };

        let keyed = nodes.iter().any(|node| node.keyed);
        any |= keyed;
        if writes_a_header(item) {
            headers |= keyed;
        }
        if nodes.iter().any(|node| node.touched) {
            children.insert(Box::from(name), nodes);
        }
    }

    Built {
        children,
        headers,
        any,
    }
}

/// Whether this entry renders `[its.path]` rather than sitting on a line of its
/// parent's body. A dotted table does not: `b.c = 1` writes no header, so the
/// key of the table holding `b` never appears on that line.
fn writes_a_header(item: &Item) -> bool {
    match item {
        Item::Table(table) => !table.is_dotted(),
        Item::ArrayOfTables(_) => true,
        _ => false,
    }
}

fn frozen_table(
    table: &Table,
    key_span: Option<&Range<usize>>,
    key_prefix: Option<&Range<usize>>,
    regions: &Regions,
) -> Frozen {
    let own = regions.hits(key_span.cloned()) || regions.hits(table.span());
    // The cover is read off the run it will be spliced back into, never off
    // another: `table.decor()` is what a header writes above itself, while the
    // key's own leading run is cleared wholesale by `header_key_decor` and so
    // belongs with the key's verdict instead.
    let prefix = regions.cover(prefix_span(table.decor()));
    let built = children_of(table, regions);
    let below = if table.is_dotted() {
        built.any
    } else {
        built.headers
    };
    Frozen {
        own,
        prefix,
        keyed: own || below || regions.hits(key_prefix.cloned()),
        touched: own
            || prefix.is_some()
            || regions.hits(key_prefix.cloned())
            || built.children.values().flatten().any(|node| node.touched),
        children: built.children,
    }
}

fn prefix_span(decor: &Decor) -> Option<Range<usize>> {
    decor.prefix().and_then(RawString::span)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions(text: &str) -> Vec<&str> {
        Regions::scan(text).texts(text)
    }

    #[test]
    fn a_region_covers_both_marker_lines() {
        assert_eq!(
            regions("a = 1\n# fmt: off\nb = 2\n# fmt: on\nc = 3\n"),
            ["# fmt: off\nb = 2\n# fmt: on\n"]
        );
    }

    #[test]
    fn an_unterminated_off_runs_to_the_end() {
        assert_eq!(
            regions("a = 1\n# fmt: off\nb = 2\n"),
            ["# fmt: off\nb = 2\n"]
        );
    }

    #[test]
    fn a_stray_on_is_ignored() {
        assert!(regions("a = 1\n# fmt: on\nb = 2\n").is_empty());
    }

    #[test]
    fn a_nested_off_is_ignored() {
        assert_eq!(
            regions("# fmt: off\n# fmt: off\na = 1\n# fmt: on\n# fmt: on\n"),
            ["# fmt: off\n# fmt: off\na = 1\n# fmt: on\n"]
        );
    }

    #[test]
    fn two_regions_are_separate() {
        assert_eq!(
            regions("# fmt: off\na = 1\n# fmt: on\nb = 2\n# fmt: off\nc = 3\n# fmt: on\n"),
            [
                "# fmt: off\na = 1\n# fmt: on\n",
                "# fmt: off\nc = 3\n# fmt: on\n"
            ]
        );
    }

    #[test]
    fn every_spelling_is_recognised() {
        for off in OFF {
            let text = format!("# {off}\na = 1\n");
            assert_eq!(regions(&text).len(), 1, "{off}");
        }
        for (off, on) in OFF.iter().zip(ON) {
            let text = format!("# {off}\na = 1\n# {on}\nb = 2\n");
            assert_eq!(regions(&text), [format!("# {off}\na = 1\n# {on}\n")]);
        }
    }

    #[test]
    fn spacing_and_case_are_free() {
        assert_eq!(regions("\t#   FMT :  Off  \na = 1\n").len(), 1);
        assert_eq!(regions("#fmt:off\na = 1\n").len(), 1);
    }

    /// A comment carries the same weight wherever it is written, so that the
    /// formatter moving one cannot change what the document asked for.
    #[test]
    fn a_marker_sharing_a_line_opens_the_region_at_that_line() {
        assert_eq!(
            regions("a = 1 # fmt: off\nb = 2\n"),
            ["a = 1 # fmt: off\nb = 2\n"]
        );
        assert_eq!(
            regions("# fmt: off\na = 1\nb = 2 # fmt: on\nc = 3\n"),
            ["# fmt: off\na = 1\nb = 2 # fmt: on\n"]
        );
    }

    #[test]
    fn prose_after_the_marker_is_not_a_marker() {
        assert!(regions("# fmt: off please\na = 1\n").is_empty());
        assert!(regions("## fmt: off\na = 1\n").is_empty());
    }

    #[test]
    fn a_marker_inside_a_string_is_text() {
        assert!(regions("a = \"\"\"\n# fmt: off\n\"\"\"\nb = 2\n").is_empty());
        assert!(regions("a = '''\n# fmt: off\n'''\nb = 2\n").is_empty());
    }

    #[test]
    fn crlf_regions_keep_their_terminators() {
        assert_eq!(
            regions("a = 1\r\n# fmt: off\r\nb = 2\r\n# fmt: on\r\n"),
            ["# fmt: off\r\nb = 2\r\n# fmt: on\r\n"]
        );
    }

    #[test]
    fn a_document_with_no_markers_has_no_regions() {
        assert!(Regions::scan("a = 1\n# a comment\n[b]\nc = 2\n").is_empty());
    }

    #[test]
    fn a_long_comment_is_never_a_marker() {
        let text = format!("# fmt: off{}\na = 1\n", "!".repeat(MAX_MARKER));
        assert!(regions(&text).is_empty());
    }

    /// `docs/toml-style.md` says spacing inside a marker is free up to 40
    /// characters, and `tests/support/directives.rs` states the same rule to
    /// stay independent of this file. Both sides need the boundary pinned, or a
    /// change here reads as a formatter bug over there.
    #[test]
    fn a_marker_is_capped_at_the_documented_length() {
        let padded = |extra: usize| {
            let body = format!("taplo: fm{}t-off", " ".repeat(extra));
            assert_eq!(body.len(), 14 + extra);
            format!("# {body}\na = 1\n# taplo: fmt-on\n")
        };

        assert!(marker("taplo: fmt-off").is_some());
        assert_eq!(regions(&padded(MAX_MARKER - 14)).len(), 1);
        assert!(regions(&padded(MAX_MARKER - 13)).is_empty());
    }
}
