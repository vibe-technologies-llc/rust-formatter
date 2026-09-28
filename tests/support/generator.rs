use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

use proptest::prelude::*;
use rust_formatter::{
    ArrayStyle, InlineTableStyle, PackageOrder, Spacing, TomlIndent, TomlStyle, TomlVersion,
    TrailingComma,
};

use super::styles::Profile;

const KEY_POOL: &[&str] = &[
    "a",
    "b",
    "c",
    "key",
    "name",
    "x-y",
    "z_1",
    "0",
    "true",
    "with space",
    "dot.ted",
    "",
    "版",
    "quote\"d",
];

/// Whitespace a key path or header may be padded with. Weighted towards none,
/// so most generated documents stay conventional.
const PAD_POOL: &[&str] = &["", "", "", "", " ", "  ", "\t"];

const STRING_POOL: &[&str] = &[
    "",
    "plain",
    "with space",
    "has # hash",
    "has \" quote",
    "has ' apostrophe",
    "back\\slash",
    "tab\there",
    "new\nline",
    "trailing ",
    "版 unicode",
    "# leading hash",
    "= equals",
    "[bracket]",
    "{brace}",
];

const DATETIME_POOL: &[&str] = &[
    "1979-05-27T07:32:00Z",
    "1979-05-27T00:32:00-07:00",
    "1979-05-27T07:32:00.999999Z",
    "1979-05-27T07:32:00",
    "1979-05-27",
    "07:32:00",
    "00:32:00.999999",
];

const COMMENT_POOL: &[&str] = &[
    "a comment",
    "",
    "  padded  ",
    "with \"quotes\" and 'apostrophes'",
    "### hashes",
    "unicode 版",
    "key = \"looks like toml\"",
];

/// Directive markers, rare enough that a generated document still exercises the
/// layout rules around them. They land in trailing comments too, where they are
/// inert, which is the other half of the rule worth checking.
const DIRECTIVE_POOL: &[&str] = &[
    " fmt: off",
    " fmt: on",
    " taplo: fmt-off",
    " taplo: fmt-on",
    " rust-formatter: fmt-off",
    " rust-formatter: fmt-on",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStyle {
    Bare,
    Basic,
    Literal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenKey {
    pub logical: String,
    pub style: KeyStyle,
}

impl GenKey {
    pub fn render(&self) -> String {
        match self.style {
            KeyStyle::Bare if is_bare_key(&self.logical) => self.logical.clone(),
            KeyStyle::Literal if can_be_literal(&self.logical) => {
                format!("'{}'", self.logical)
            }
            _ => render_basic_string(&self.logical),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrStyle {
    Basic,
    Literal,
    MlBasic,
    MlLiteral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntStyle {
    Dec,
    Plus,
    Underscore,
    Hex,
    Oct,
    Bin,
}

#[derive(Debug, Clone)]
pub struct InlineEntry {
    pub key: GenKey,
    pub value: GenValue,
    pub above: Option<String>,
    /// Comment between `=` and the value. Only legal where the entry already
    /// spans lines, and the one comment position no generated document reached
    /// before `expand_over_lines` was found to be dropping it.
    pub after_eq: Option<String>,
}

#[derive(Debug, Clone)]
pub enum GenValue {
    Str(String, StrStyle),
    Int(i64, IntStyle),
    Float(f64),
    Bool(bool),
    Datetime(String),
    Array {
        items: Vec<GenValue>,
        multiline: bool,
        trailing_comma: bool,
        /// Own-line comment emitted before element `i` when `multiline`.
        comments: Vec<Option<String>>,
    },
    Inline {
        entries: Vec<InlineEntry>,
        multiline: bool,
        trailing_comma: bool,
    },
}

impl GenValue {
    pub fn render(&self, depth: usize) -> String {
        let mut out = String::new();
        self.push(&mut out, depth);
        out
    }

    fn push(&self, out: &mut String, depth: usize) {
        match self {
            GenValue::Str(text, style) => out.push_str(&render_string(text, *style)),
            GenValue::Int(value, style) => out.push_str(&render_int(*value, *style)),
            GenValue::Float(value) => out.push_str(&render_float(*value)),
            GenValue::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            GenValue::Datetime(value) => out.push_str(value),
            GenValue::Array {
                items,
                multiline,
                trailing_comma,
                comments,
            } => push_array(out, items, *multiline, *trailing_comma, comments, depth),
            GenValue::Inline {
                entries,
                multiline,
                trailing_comma,
            } => push_inline(out, entries, *multiline, *trailing_comma, depth),
        }
    }
}

fn push_array(
    out: &mut String,
    items: &[GenValue],
    multiline: bool,
    trailing_comma: bool,
    comments: &[Option<String>],
    depth: usize,
) {
    out.push('[');
    let pad = INDENT.repeat(depth + 1);
    for (index, item) in items.iter().enumerate() {
        if multiline {
            if let Some(Some(comment)) = comments.get(index) {
                out.push('\n');
                out.push_str(&pad);
                out.push('#');
                out.push_str(comment);
            }
            out.push('\n');
            out.push_str(&pad);
        } else if index > 0 {
            out.push(' ');
        }
        item.push(out, depth + 1);
        if index + 1 < items.len() || trailing_comma {
            out.push(',');
        }
    }
    if multiline && !items.is_empty() {
        out.push('\n');
        out.push_str(&INDENT.repeat(depth));
    }
    out.push(']');
}

fn push_inline(
    out: &mut String,
    entries: &[InlineEntry],
    multiline: bool,
    trailing_comma: bool,
    depth: usize,
) {
    if entries.is_empty() {
        out.push_str("{}");
        return;
    }

    out.push('{');
    let pad = INDENT.repeat(depth + 1);
    for (index, entry) in entries.iter().enumerate() {
        if multiline {
            if let Some(comment) = &entry.above {
                out.push('\n');
                out.push_str(&pad);
                out.push('#');
                out.push_str(comment);
            }
            out.push('\n');
            out.push_str(&pad);
        } else {
            out.push(' ');
        }
        out.push_str(&entry.key.render());
        out.push_str(" =");
        match (&entry.after_eq, multiline) {
            (Some(comment), true) => {
                out.push_str(" #");
                out.push_str(comment);
                out.push('\n');
                out.push_str(&pad);
            }
            _ => out.push(' '),
        }
        entry.value.push(out, depth + 1);
        if index + 1 < entries.len() || trailing_comma {
            out.push(',');
        }
    }
    if multiline {
        out.push('\n');
        out.push_str(&INDENT.repeat(depth));
    } else {
        out.push(' ');
    }
    out.push('}');
}

const INDENT: &str = "    ";

#[derive(Debug, Clone, Default)]
pub struct Lead {
    pub blanks: usize,
    pub comments: Vec<String>,
}

impl Lead {
    fn render(&self, out: &mut String) {
        for _ in 0..self.blanks {
            out.push('\n');
        }
        for comment in &self.comments {
            out.push('#');
            out.push_str(comment);
            out.push('\n');
        }
    }
}

/// Whitespace TOML allows inside a key path and inside a header's brackets,
/// and which the formatter is expected to remove.
#[derive(Debug, Clone, Default)]
pub struct KeyPad {
    pub before_dot: String,
    pub after_dot: String,
    pub open: String,
    pub close: String,
}

#[derive(Debug, Clone)]
pub struct Pair {
    pub lead: Lead,
    pub path: Vec<GenKey>,
    pub pad: KeyPad,
    pub value: GenValue,
    pub trailing: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Body {
    Pair(Pair),
    Table {
        header: Vec<GenKey>,
        pad: KeyPad,
        array_of_tables: bool,
        trailing: Option<String>,
        pairs: Vec<Pair>,
    },
}

#[derive(Debug, Clone)]
pub struct Item {
    pub lead: Lead,
    pub body: Body,
}

#[derive(Debug, Clone)]
pub struct GenDoc {
    pub items: Vec<Item>,
    pub trailer: Lead,
    pub final_newline: bool,
}

impl GenDoc {
    pub fn render(&self) -> String {
        let mut out = String::new();
        for item in &self.items {
            item.lead.render(&mut out);
            match &item.body {
                Body::Pair(pair) => render_pair(pair, &mut out),
                Body::Table {
                    header,
                    pad,
                    array_of_tables,
                    trailing,
                    pairs,
                } => {
                    let path = render_path(header, pad);
                    let (open, close) = (&pad.open, &pad.close);
                    if *array_of_tables {
                        let _ = write!(out, "[[{open}{path}{close}]]");
                    } else {
                        let _ = write!(out, "[{open}{path}{close}]");
                    }
                    push_trailing(trailing.as_ref(), &mut out);
                    for pair in pairs {
                        pair.lead.render(&mut out);
                        render_pair(pair, &mut out);
                    }
                }
            }
        }
        self.trailer.render(&mut out);
        if !self.final_newline {
            while out.ends_with('\n') {
                out.pop();
            }
        }
        out
    }
}

fn render_pair(pair: &Pair, out: &mut String) {
    out.push_str(&render_path(&pair.path, &pair.pad));
    out.push_str(" = ");
    out.push_str(&pair.value.render(0));
    push_trailing(pair.trailing.as_ref(), out);
}

fn push_trailing(trailing: Option<&String>, out: &mut String) {
    if let Some(comment) = trailing {
        out.push_str(" #");
        out.push_str(comment);
    }
    out.push('\n');
}

fn render_path(path: &[GenKey], pad: &KeyPad) -> String {
    let mut out = String::new();
    for (index, key) in path.iter().enumerate() {
        if index > 0 {
            out.push_str(&pad.before_dot);
            out.push('.');
            out.push_str(&pad.after_dot);
        }
        out.push_str(&key.render());
    }
    out
}

fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// TOML literal strings have no escapes at all, so anything containing the
/// delimiter or a newline has to fall back to a basic string.
fn can_be_literal(text: &str) -> bool {
    !text.contains('\'') && !text.contains('\n') && !text.contains('\r')
}

fn render_string(text: &str, style: StrStyle) -> String {
    match style {
        StrStyle::Literal if can_be_literal(text) => format!("'{text}'"),
        StrStyle::MlLiteral if can_be_literal(text) && !text.ends_with('\'') => {
            format!("'''\n{text}'''")
        }
        // Escaping every quote and backslash is always legal inside `"""`, and
        // sidesteps the delimiter-abutting rules entirely.
        StrStyle::MlBasic => format!(
            "\"\"\"\n{}\"\"\"",
            text.replace('\\', "\\\\").replace('"', "\\\"")
        ),
        _ => render_basic_string(text),
    }
}

fn render_int(value: i64, style: IntStyle) -> String {
    match style {
        IntStyle::Plus if value >= 0 => format!("+{value}"),
        IntStyle::Underscore => {
            let digits = value.unsigned_abs().to_string();
            let grouped: Vec<String> = digits
                .as_bytes()
                .rchunks(3)
                .rev()
                .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                .collect();
            format!("{}{}", if value < 0 { "-" } else { "" }, grouped.join("_"))
        }
        // TOML forbids a sign on a radix-prefixed integer.
        IntStyle::Hex if value >= 0 => format!("0x{value:X}"),
        IntStyle::Oct if value >= 0 => format!("0o{value:o}"),
        IntStyle::Bin if value >= 0 => format!("0b{value:b}"),
        _ => value.to_string(),
    }
}

fn render_basic_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// TOML floats need a fractional part or an exponent; Rust's `{:?}` keeps the
/// shortest round-tripping form and always emits one of the two.
fn render_float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.to_owned();
    }
    let rendered = format!("{value:?}");
    if rendered.contains('.') || rendered.contains('e') || rendered.contains('E') {
        rendered
    } else {
        format!("{rendered}.0")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Claim {
    Value,
    Table,
    ArrayOfTables,
}

#[derive(Default)]
struct Namespace {
    claim: Option<Claim>,
    /// Brought into existence as the prefix of a dotted key. TOML forbids a
    /// later `[header]` from reopening such a table, but further dotted keys
    /// may keep extending it.
    dotted: bool,
    children: BTreeMap<String, Namespace>,
}

impl Namespace {
    /// Reserve `path`, or report that TOML would reject the definition. Every
    /// generated document is filtered through this, so the generator can draw
    /// colliding names from a small pool without emitting invalid TOML.
    fn claim(&mut self, path: &[GenKey], kind: Claim) -> bool {
        let Some((last, parents)) = path.split_last() else {
            return false;
        };
        let header = kind != Claim::Value;

        let mut node = self;
        for segment in parents {
            node = node.children.entry(segment.logical.clone()).or_default();
            match node.claim {
                Some(Claim::Value | Claim::ArrayOfTables) => return false,
                Some(Claim::Table) if header && node.dotted => return false,
                _ => {}
            }
            if header && node.dotted {
                return false;
            }
            if node.claim.is_none() {
                node.claim = Some(Claim::Table);
                node.dotted = !header;
            }
        }

        let node = node.children.entry(last.logical.clone()).or_default();
        let vacant = node.claim.is_none() && node.children.is_empty() && !node.dotted;
        match kind {
            Claim::Value | Claim::Table if !vacant => return false,
            Claim::ArrayOfTables
                if !(vacant || (node.claim == Some(Claim::ArrayOfTables) && !node.dotted)) =>
            {
                return false;
            }
            _ => {}
        }
        node.claim = Some(kind);
        true
    }

    fn descend(&mut self, path: &[GenKey]) -> &mut Namespace {
        let mut node = self;
        for segment in path {
            node = node.children.entry(segment.logical.clone()).or_default();
        }
        node
    }
}

fn sanitize_pairs(pairs: Vec<Pair>, namespace: &mut Namespace, prefix: &[GenKey]) -> Vec<Pair> {
    let mut kept = Vec::new();
    for mut pair in pairs {
        let mut full = prefix.to_vec();
        full.extend(pair.path.iter().cloned());
        if namespace.claim(&full, Claim::Value) {
            pair.value = sanitize_value(pair.value);
            kept.push(pair);
        }
    }
    kept
}

fn sanitize_value(value: GenValue) -> GenValue {
    match value {
        GenValue::Array {
            items,
            multiline,
            trailing_comma,
            mut comments,
        } => {
            let items: Vec<GenValue> = items.into_iter().map(sanitize_value).collect();
            comments.resize(items.len(), None);
            GenValue::Array {
                trailing_comma: trailing_comma && !items.is_empty(),
                items,
                multiline,
                comments,
            }
        }
        GenValue::Inline {
            entries,
            multiline,
            trailing_comma,
        } => {
            let mut seen = BTreeSet::new();
            let mut kept = Vec::new();
            for entry in entries {
                if seen.insert(entry.key.logical.clone()) {
                    kept.push(InlineEntry {
                        value: sanitize_value(entry.value),
                        ..entry
                    });
                }
            }
            GenValue::Inline {
                trailing_comma: trailing_comma && !kept.is_empty(),
                entries: kept,
                multiline,
            }
        }
        other => other,
    }
}

fn sanitize(doc: GenDoc) -> GenDoc {
    let mut namespace = Namespace::default();
    let mut items = Vec::new();

    // A bare key/value pair written after a `[table]` header belongs to that
    // table, not to the document root. Emitting every top-level pair before the
    // first header is what makes the rendered document mean what the tree says.
    let (pairs, tables): (Vec<Item>, Vec<Item>) = doc
        .items
        .into_iter()
        .partition(|item| matches!(item.body, Body::Pair(_)));

    for item in pairs.into_iter().chain(tables) {
        match item.body {
            Body::Pair(pair) => {
                if namespace.claim(&pair.path, Claim::Value) {
                    items.push(Item {
                        lead: item.lead,
                        body: Body::Pair(Pair {
                            value: sanitize_value(pair.value),
                            ..pair
                        }),
                    });
                }
            }
            Body::Table {
                header,
                pad,
                array_of_tables,
                trailing,
                pairs,
            } => {
                let kind = if array_of_tables {
                    Claim::ArrayOfTables
                } else {
                    Claim::Table
                };
                if !namespace.claim(&header, kind) {
                    continue;
                }
                // Each `[[header]]` element is an independent namespace, and no
                // `[header.sub]` can reach into one, so its keys are scoped
                // locally. A plain `[header]` shares the document namespace.
                let pairs = if array_of_tables {
                    sanitize_pairs(pairs, &mut Namespace::default(), &[])
                } else {
                    let scope = namespace.descend(&header);
                    sanitize_pairs(pairs, scope, &[])
                };
                items.push(Item {
                    lead: item.lead,
                    body: Body::Table {
                        header,
                        pad,
                        array_of_tables,
                        trailing,
                        pairs,
                    },
                });
            }
        }
    }

    GenDoc {
        items,
        trailer: doc.trailer,
        final_newline: doc.final_newline,
    }
}

fn arb_key() -> impl Strategy<Value = GenKey> {
    (
        prop::sample::select(KEY_POOL),
        prop_oneof![
            6 => Just(KeyStyle::Bare),
            2 => Just(KeyStyle::Basic),
            1 => Just(KeyStyle::Literal),
        ],
    )
        .prop_map(|(logical, style)| GenKey {
            logical: logical.to_owned(),
            style,
        })
}

fn arb_path() -> impl Strategy<Value = Vec<GenKey>> {
    prop_oneof![
        6 => prop::collection::vec(arb_key(), 1..2),
        3 => prop::collection::vec(arb_key(), 2..3),
        1 => prop::collection::vec(arb_key(), 3..4),
    ]
}

/// Mixes short atoms with lengths that straddle the formatter's 100-column
/// array wrap, so both the inline and wrapped paths are exercised. The wide
/// band costs two columns per character, so its character count and its width
/// straddle the wrap at different lengths.
fn arb_text() -> impl Strategy<Value = String> {
    prop_oneof![
        7 => prop::sample::select(STRING_POOL).prop_map(str::to_owned),
        2 => (1usize..40).prop_map(|n| "y".repeat(n)),
        1 => (85usize..115).prop_map(|n| "z".repeat(n)),
        1 => (40usize..60).prop_map(|n| "\u{8a9e}".repeat(n)),
    ]
}

fn arb_string() -> impl Strategy<Value = GenValue> {
    (
        arb_text(),
        prop_oneof![
            6 => Just(StrStyle::Basic),
            3 => Just(StrStyle::Literal),
            1 => Just(StrStyle::MlBasic),
            1 => Just(StrStyle::MlLiteral),
        ],
    )
        .prop_map(|(text, style)| GenValue::Str(text, style))
}

fn arb_int() -> impl Strategy<Value = GenValue> {
    let magnitude = prop_oneof![
        6 => -1000i64..1000,
        2 => any::<i64>(),
        1 => Just(i64::MIN),
        1 => Just(i64::MAX),
    ];
    let style = prop_oneof![
        5 => Just(IntStyle::Dec),
        1 => Just(IntStyle::Plus),
        2 => Just(IntStyle::Underscore),
        1 => Just(IntStyle::Hex),
        1 => Just(IntStyle::Oct),
        1 => Just(IntStyle::Bin),
    ];
    (magnitude, style).prop_map(|(value, style)| GenValue::Int(value, style))
}

fn arb_comment() -> impl Strategy<Value = String> {
    prop_oneof![
        12 => prop::sample::select(COMMENT_POOL),
        1 => prop::sample::select(DIRECTIVE_POOL),
    ]
    .prop_map(str::to_owned)
}

fn arb_value() -> impl Strategy<Value = GenValue> {
    let leaf = prop_oneof![
        6 => arb_string(),
        3 => arb_int(),
        2 => prop_oneof![
            any::<f64>().prop_filter("finite", |v| v.is_finite()),
            Just(f64::INFINITY),
            Just(f64::NEG_INFINITY),
            Just(f64::NAN),
        ]
        .prop_map(GenValue::Float),
        2 => any::<bool>().prop_map(GenValue::Bool),
        2 => prop::sample::select(DATETIME_POOL).prop_map(|v| GenValue::Datetime(v.to_owned())),
    ];

    leaf.prop_recursive(3, 32, 6, |inner| {
        let array = (
            prop_oneof![
                3 => prop::collection::vec(inner.clone(), 0..5),
                1 => prop::collection::vec(inner.clone(), 8..16),
            ],
            prop::bool::weighted(0.3),
            prop::bool::weighted(0.3),
        )
            .prop_flat_map(|(items, multiline, trailing_comma)| {
                let len = items.len();
                (
                    Just(items),
                    Just(multiline),
                    Just(trailing_comma),
                    prop::collection::vec(prop::option::weighted(0.3, arb_comment()), len),
                )
            })
            .prop_map(
                |(items, multiline, trailing_comma, comments)| GenValue::Array {
                    items,
                    multiline,
                    trailing_comma,
                    comments,
                },
            );

        let entry = (
            arb_key(),
            inner,
            prop::option::weighted(0.3, arb_comment()),
            prop::option::weighted(0.3, arb_comment()),
        )
            .prop_map(|(key, value, above, after_eq)| InlineEntry {
                key,
                value,
                above,
                after_eq,
            });

        let inline = (
            prop::collection::vec(entry, 0..4),
            prop::bool::weighted(0.3),
            prop::bool::weighted(0.25),
        )
            .prop_map(|(entries, multiline, trailing_comma)| GenValue::Inline {
                entries,
                multiline,
                trailing_comma,
            });

        prop_oneof![2 => array, 1 => inline]
    })
}

fn arb_lead() -> impl Strategy<Value = Lead> {
    (0usize..3, prop::collection::vec(arb_comment(), 0..3))
        .prop_map(|(blanks, comments)| Lead { blanks, comments })
}

fn arb_trailing() -> impl Strategy<Value = Option<String>> {
    prop::option::of(arb_comment())
}

fn arb_pad() -> impl Strategy<Value = KeyPad> {
    let slot = || prop::sample::select(PAD_POOL);
    (slot(), slot(), slot(), slot()).prop_map(|(before_dot, after_dot, open, close)| KeyPad {
        before_dot: before_dot.to_owned(),
        after_dot: after_dot.to_owned(),
        open: open.to_owned(),
        close: close.to_owned(),
    })
}

fn arb_pair() -> impl Strategy<Value = Pair> {
    (
        arb_lead(),
        arb_path(),
        arb_pad(),
        arb_value(),
        arb_trailing(),
    )
        .prop_map(|(lead, path, pad, value, trailing)| Pair {
            lead,
            path,
            pad,
            value,
            trailing,
        })
}

fn arb_item() -> impl Strategy<Value = Item> {
    let table = (
        arb_path(),
        arb_pad(),
        prop::bool::weighted(0.25),
        arb_trailing(),
        prop::collection::vec(arb_pair(), 0..5),
    )
        .prop_map(
            |(header, pad, array_of_tables, trailing, pairs)| Body::Table {
                header,
                pad,
                array_of_tables,
                trailing,
                pairs,
            },
        );

    (
        arb_lead(),
        prop_oneof![arb_pair().prop_map(Body::Pair), table],
    )
        .prop_map(|(lead, body)| Item { lead, body })
}

pub fn arb_doc() -> impl Strategy<Value = GenDoc> {
    (
        prop::collection::vec(arb_item(), 0..8),
        arb_lead(),
        prop::bool::weighted(0.9),
    )
        .prop_map(|(items, trailer, final_newline)| {
            sanitize(GenDoc {
                items,
                trailer,
                final_newline,
            })
        })
}

/// A style drawn from the whole option surface rather than from the seven
/// shipped presets, which between them leave most of the space untouched: none
/// crosses `compact`/`expand` with a narrow width or a tab indent.
///
/// `cargo_conventions` stays off -- it rewrites a dependency table to a string
/// on purpose, so guarantee 2 does not hold under it and no value-tree
/// comparison would survive.
pub fn arb_style() -> impl Strategy<Value = Profile> {
    let shape = (
        arb_indent(),
        arb_width(),
        arb_array_style(),
        arb_inline_table_style(),
        arb_toml_version(),
        arb_trailing_comma(),
        arb_spacing(),
        arb_spacing(),
        arb_package_order(),
    );
    let flags = (
        any::<bool>(),
        any::<bool>(),
        0usize..4,
        prop::collection::vec(any::<bool>(), 14),
    );

    (shape, flags).prop_map(|(shape, flags)| {
        let (
            indent,
            max_width,
            arrays,
            inline_tables,
            toml_version,
            trailing_comma,
            array_spacing,
            inline_table_spacing,
            package_order,
        ) = shape;
        let (directives, blank_line_before_tables, max_blank_lines, bits) = flags;

        // The two combinations `validate` rejects, so a generated style is
        // always one a user could actually ask for on the command line.
        let inline_tables =
            if toml_version == TomlVersion::V1_0 && inline_tables == InlineTableStyle::Expand {
                InlineTableStyle::Auto
            } else {
                inline_tables
            };
        let sort_grouped = bits[13] && max_blank_lines > 0;

        let style = TomlStyle {
            indent,
            max_width,
            arrays,
            inline_tables,
            toml_version,
            trailing_comma,
            directives,
            blank_line_before_tables,
            max_blank_lines,
            array_spacing,
            inline_table_spacing,
            align_entries: bits[0],
            align_comments: bits[1],
            indent_tables: bits[2],
            indent_entries: bits[3],
            normalize_keys: bits[4],
            sort_deps: bits[5],
            sort_package: bits[6],
            sort_dep_fields: bits[7],
            sort_features: bits[8],
            sort_arrays: bits[9],
            sort_targets: bits[10],
            sort_tables: bits[11],
            sort_keys: bits[12],
            sort_grouped,
            package_order,
            cargo_conventions: false,
        };
        Profile {
            name: format!("{style:?}"),
            style,
        }
    })
}

fn arb_indent() -> impl Strategy<Value = TomlIndent> {
    let tab_width = 1usize..=8;
    prop_oneof![
        6 => (0usize..=16, tab_width.clone())
            .prop_map(|(n, tab)| TomlIndent::with_tab_width(&" ".repeat(n), tab)),
        2 => tab_width.prop_map(|tab| TomlIndent::with_tab_width("\t", tab)),
    ]
}

/// Weighted onto the widths that decide a wrap for the generator's own value
/// lengths, with the degenerate ends kept because every container has to wrap
/// at 1 and none at 400.
fn arb_width() -> impl Strategy<Value = usize> {
    prop_oneof![
        1 => Just(1usize),
        2 => Just(20usize),
        2 => Just(40usize),
        2 => Just(60usize),
        4 => Just(100usize),
        2 => Just(120usize),
        1 => Just(400usize),
        2 => 0usize..=200,
    ]
}

fn arb_array_style() -> impl Strategy<Value = ArrayStyle> {
    prop_oneof![
        Just(ArrayStyle::Preserve),
        Just(ArrayStyle::Auto),
        Just(ArrayStyle::Expand),
    ]
}

fn arb_inline_table_style() -> impl Strategy<Value = InlineTableStyle> {
    prop_oneof![
        Just(InlineTableStyle::Auto),
        Just(InlineTableStyle::Compact),
        Just(InlineTableStyle::Expand),
        Just(InlineTableStyle::Section),
    ]
}

fn arb_toml_version() -> impl Strategy<Value = TomlVersion> {
    prop_oneof![Just(TomlVersion::V1_1), Just(TomlVersion::V1_0)]
}

fn arb_trailing_comma() -> impl Strategy<Value = TrailingComma> {
    prop_oneof![Just(TrailingComma::Never), Just(TrailingComma::Multiline)]
}

fn arb_spacing() -> impl Strategy<Value = Spacing> {
    prop_oneof![Just(Spacing::Compact), Just(Spacing::Spaced)]
}

fn arb_package_order() -> impl Strategy<Value = PackageOrder> {
    prop_oneof![Just(PackageOrder::Book), Just(PackageOrder::StyleGuide)]
}
