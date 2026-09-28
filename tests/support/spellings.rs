use std::fmt::Write as _;

use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

/// Every scalar value in a document as `(path, raw spelling)`, in source order.
///
/// Guarantee 5 of `docs/toml-style.md` promises the *spelling* survives, which
/// is exactly what the value tree in `toml_tree.rs` throws away: it decodes
/// `0x1F` to `31`, `1.50` to `1.5` and `'win\path'` to its unescaped text. This
/// oracle reads `display_repr` instead, which is the raw slice the parser saw.
///
/// The path is the identity, not the position, because `[a] b = 1`, `a.b = 1`
/// and `a = { b = 1 }` are interchangeable spellings the formatter may move
/// between; all three yield `a.b`. Keys are deliberately not covered --
/// `--toml-normalize-keys` rewrites them on purpose.
pub fn spellings(doc: &DocumentMut) -> Vec<(String, String)> {
    let mut out = Vec::new();
    table_spellings(doc.as_table(), &mut String::new(), &mut out);
    out
}

pub fn parse_spellings(src: &str) -> Result<Vec<(String, String)>, toml_edit::TomlError> {
    Ok(spellings(&src.parse::<DocumentMut>()?))
}

/// The spellings alone, sorted. A profile that reorders arrays or tables moves
/// values on purpose, so the path-keyed sequence would report its own rule as a
/// changed spelling.
pub fn multiset(pairs: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = pairs.iter().map(|(_, repr)| repr.clone()).collect();
    out.sort();
    out
}

fn table_spellings(table: &Table, path: &mut String, out: &mut Vec<(String, String)>) {
    for (key, item) in table {
        with_segment(path, key, |path| item_spellings(item, path, out));
    }
}

fn inline_table_spellings(table: &InlineTable, path: &mut String, out: &mut Vec<(String, String)>) {
    for (key, value) in table {
        with_segment(path, key, |path| value_spellings(value, path, out));
    }
}

fn item_spellings(item: &Item, path: &mut String, out: &mut Vec<(String, String)>) {
    match item {
        Item::None => {}
        Item::Value(value) => value_spellings(value, path, out),
        Item::Table(table) => table_spellings(table, path, out),
        Item::ArrayOfTables(array) => array_of_tables_spellings(array, path, out),
    }
}

fn array_of_tables_spellings(
    array: &ArrayOfTables,
    path: &mut String,
    out: &mut Vec<(String, String)>,
) {
    for (index, table) in array.iter().enumerate() {
        with_index(path, index, |path| table_spellings(table, path, out));
    }
}

fn array_spellings(array: &Array, path: &mut String, out: &mut Vec<(String, String)>) {
    for (index, value) in array.iter().enumerate() {
        with_index(path, index, |path| value_spellings(value, path, out));
    }
}

fn value_spellings(value: &Value, path: &mut String, out: &mut Vec<(String, String)>) {
    let repr = match value {
        Value::String(v) => v.display_repr().into_owned(),
        Value::Integer(v) => v.display_repr().into_owned(),
        Value::Float(v) => v.display_repr().into_owned(),
        Value::Boolean(v) => v.display_repr().into_owned(),
        Value::Datetime(v) => v.display_repr().into_owned(),
        Value::Array(v) => return array_spellings(v, path, out),
        Value::InlineTable(v) => return inline_table_spellings(v, path, out),
    };
    out.push((path.clone(), repr));
}

fn with_segment(path: &mut String, key: &str, body: impl FnOnce(&mut String)) {
    let restore = path.len();
    if !path.is_empty() {
        path.push('.');
    }
    path.push_str(key);
    body(path);
    path.truncate(restore);
}

fn with_index(path: &mut String, index: usize, body: impl FnOnce(&mut String)) {
    let restore = path.len();
    let _ = write!(path, "[{index}]");
    body(path);
    path.truncate(restore);
}
