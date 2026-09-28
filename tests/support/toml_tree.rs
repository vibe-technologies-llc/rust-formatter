use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

/// A TOML document reduced to the data it carries, with every formatting
/// decision erased. `[a] b = 1`, `a.b = 1` and `a = { b = 1 }` all reduce to the
/// same tree, so a formatter is free to move between those spellings but not to
/// add, drop, alter or reorder a value.
///
/// A table keeps its keys in document order rather than in a `BTreeMap`: with
/// sorting off -- five of the seven presets -- a scrambled table is a bug, and a
/// map-backed tree cannot see one. [`Tree::normalized`] is the escape hatch for
/// the profiles that reorder on purpose.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tree {
    Str(String),
    Int(i64),
    Float(String),
    Bool(bool),
    Datetime(String),
    Array(Vec<Tree>),
    Table(Vec<(String, Tree)>),
}

impl Tree {
    /// The same tree with whatever the profile is allowed to move reduced to a
    /// multiset, so its own rule is not reported as a lost value.
    ///
    /// `sort_arrays` covers `--sort-arrays` / `--sort-targets`; `sort_keys`
    /// covers every table-level sort *and* `--toml-inline-tables section`,
    /// which promotes an over-wide inline table to its own `[header]` and so
    /// moves that key past its siblings.
    pub fn normalized(&self, sort_arrays: bool, sort_keys: bool) -> Self {
        match self {
            Self::Array(items) => {
                let mut items: Vec<Self> = items
                    .iter()
                    .map(|item| item.normalized(sort_arrays, sort_keys))
                    .collect();
                if sort_arrays {
                    items.sort();
                }
                Self::Array(items)
            }
            Self::Table(entries) => {
                let mut entries: Vec<(String, Self)> = entries
                    .iter()
                    .map(|(key, value)| (key.clone(), value.normalized(sort_arrays, sort_keys)))
                    .collect();
                if sort_keys {
                    entries.sort();
                }
                Self::Table(entries)
            }
            other => other.clone(),
        }
    }
}

pub fn tree_of(doc: &DocumentMut) -> Tree {
    table_tree(doc.as_table())
}

pub fn parse_tree(src: &str) -> Result<Tree, toml_edit::TomlError> {
    Ok(tree_of(&src.parse::<DocumentMut>()?))
}

fn table_tree(table: &Table) -> Tree {
    let mut entries = Vec::new();
    for (key, item) in table {
        if let Some(value) = item_tree(item) {
            entries.push((key.to_owned(), value));
        }
    }
    Tree::Table(entries)
}

fn inline_table_tree(table: &InlineTable) -> Tree {
    Tree::Table(
        table
            .iter()
            .map(|(key, value)| (key.to_owned(), value_tree(value)))
            .collect(),
    )
}

fn item_tree(item: &Item) -> Option<Tree> {
    match item {
        Item::None => None,
        Item::Value(value) => Some(value_tree(value)),
        Item::Table(table) => Some(table_tree(table)),
        Item::ArrayOfTables(array) => Some(array_of_tables_tree(array)),
    }
}

fn array_of_tables_tree(array: &ArrayOfTables) -> Tree {
    Tree::Array(array.iter().map(table_tree).collect())
}

fn array_tree(array: &Array) -> Tree {
    Tree::Array(array.iter().map(value_tree).collect())
}

fn value_tree(value: &Value) -> Tree {
    match value {
        Value::String(v) => Tree::Str(v.value().clone()),
        Value::Integer(v) => Tree::Int(*v.value()),
        // f64 is neither Eq nor Ord, and `1.0`/`1.00`/`1e0` are the same
        // number; the shortest round-trip repr canonicalises all three.
        Value::Float(v) => Tree::Float(v.value().to_string()),
        Value::Boolean(v) => Tree::Bool(*v.value()),
        Value::Datetime(v) => Tree::Datetime(v.value().to_string()),
        Value::Array(v) => array_tree(v),
        Value::InlineTable(v) => inline_table_tree(v),
    }
}
