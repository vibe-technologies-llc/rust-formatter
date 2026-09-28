use std::cmp::Ordering;

use toml_edit::{Array, ArrayOfTables, Decor, InlineTable, Item, Table, Value};

use crate::{
    toml_directive::Guard,
    toml_fmt::{array_body_has_comment, is_deps_section, raw_to_str},
    toml_style::{DEP_ORDER, PACKAGE_ORDER, PackageOrder, TABLE_ORDER, TomlStyle},
    version_sort::version_cmp,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Root,
    Workspace,
    Package,
    Features,
    /// `[patch]` itself. Its dependency tables are one level further down, which
    /// is what separates it from every other dependency section.
    Patch,
    Deps,
    DepEntry,
    Other,
}

/// How one table's keys are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Version,
    /// Known names in the listed order, then everything else in the order it was
    /// written.
    Known(&'static [&'static str]),
    /// `name`, `version`, every other key version-sorted, `description` last.
    StyleGuidePackage,
}

impl Rule {
    /// A permutation of `keys`, not a sorted list. Every sort here is stable, so
    /// keys a rule cannot separate keep the arrangement they were written in.
    fn order(self, keys: &[&str]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..keys.len()).collect();
        match self {
            Self::Version => order.sort_by(|&a, &b| version_cmp(keys[a], keys[b])),
            Self::Known(known) => order.sort_by_key(|&index| rank_in(known, keys[index])),
            Self::StyleGuidePackage => order.sort_by(|&a, &b| {
                let (a, b) = (keys[a], keys[b]);
                match (style_guide_rank(a), style_guide_rank(b)) {
                    (rank, other) if rank != other => rank.cmp(&other),
                    (MIDDLE_RANK, _) => version_cmp(a, b),
                    _ => Ordering::Equal,
                }
            }),
        }
        order
    }
}

fn rank_in(known: &[&str], key: &str) -> usize {
    known
        .iter()
        .position(|name| *name == key)
        .unwrap_or(known.len())
}

/// The bucket every key that is neither `name`, `version` nor `description`
/// falls into, and the only one whose members are version-sorted.
const MIDDLE_RANK: usize = 2;

fn style_guide_rank(key: &str) -> usize {
    match key {
        "name" => 0,
        "version" => 1,
        "description" => 3,
        _ => MIDDLE_RANK,
    }
}

pub(crate) fn sort_document(root: &mut Table, source: &str, style: &TomlStyle, guard: Guard<'_>) {
    walk(root, Section::Root, source, style, guard);
}

fn walk(table: &mut Table, section: Section, source: &str, style: &TomlStyle, guard: Guard<'_>) {
    if let Some(rule) = rule_for(section, style) {
        reorder(table, source, style, rule, guard);
    }

    for (key, item) in table.iter_mut() {
        let name = key.get();
        match item {
            Item::Value(value) => {
                if !guard.key_frozen(name) {
                    walk_value(value, section, name, source, style);
                }
            }
            Item::Table(child) => {
                let child_section = descend(section, name, false);
                walk(child, child_section, source, style, guard.child(name, 0));
            }
            Item::ArrayOfTables(children) => {
                if style.sort_targets
                    && section == Section::Root
                    && is_target_array(name)
                    && !guard.touched(name)
                {
                    sort_target_array(children, source);
                }
                let child_section = descend(section, name, true);
                for (index, child) in children.iter_mut().enumerate() {
                    walk(
                        child,
                        child_section,
                        source,
                        style,
                        guard.child(name, index),
                    );
                }
            }
            Item::None => {}
        }
    }
}

/// `section` is the section the entry was found in; `name` is its key. An inline
/// table's own contents belong to the section one level down, which is what
/// makes `serde = { version = "1" }` reachable for [`DEP_ORDER`].
fn walk_value(value: &mut Value, section: Section, name: &str, source: &str, style: &TomlStyle) {
    match value {
        Value::InlineTable(table) => {
            let inner = descend(section, name, false);
            if let Some(rule) = rule_for(inner, style) {
                reorder_inline(table, rule);
            }
            for (key, child) in table.iter_mut() {
                let child_name = key.get().to_owned();
                walk_value(child, inner, &child_name, source, style);
            }
        }
        Value::Array(array) => {
            if style.sort_arrays && sortable_array(section, name) {
                sort_array(array, source);
            }
            for element in array.iter_mut() {
                walk_value(element, Section::Other, "", source, style);
            }
        }
        _ => {}
    }
}

fn rule_for(section: Section, style: &TomlStyle) -> Option<Rule> {
    match section {
        Section::Deps if style.sort_deps => Some(Rule::Version),
        Section::Package if style.sort_package => Some(match style.package_order {
            PackageOrder::Book => Rule::Known(PACKAGE_ORDER),
            PackageOrder::StyleGuide => Rule::StyleGuidePackage,
        }),
        Section::DepEntry if style.sort_dep_fields => Some(Rule::Known(DEP_ORDER)),
        Section::Features if style.sort_features => Some(Rule::Version),
        Section::Root if style.sort_tables => Some(Rule::Known(TABLE_ORDER)),
        // `--sort-keys` is the Style Guide's "version-sort key names within each
        // section" rule. `[package]` has its own order, and the document
        // sequence is `--sort-tables`' job, so neither falls through to it.
        Section::Package | Section::Root => None,
        _ if style.sort_keys => Some(Rule::Version),
        _ => None,
    }
}

/// `[[package]]` in a `Cargo.lock` is not a manifest's `[package]`, so the
/// canonical order is offered to a plain table only.
fn descend(section: Section, name: &str, array_of_tables: bool) -> Section {
    match (section, name) {
        (Section::Root | Section::Workspace, "package") if !array_of_tables => Section::Package,
        (Section::Root, "workspace") if !array_of_tables => Section::Workspace,
        (Section::Root | Section::Workspace, "features") => Section::Features,
        (Section::Root, "patch") => Section::Patch,
        // `[replace]` holds dependency entries directly; `[patch]` holds a table
        // of registries whose entries are one level further down.
        (Section::Root, "replace") | (Section::Patch, _) => Section::Deps,
        (Section::Deps, _) => Section::DepEntry,
        (Section::DepEntry, _) => Section::Other,
        (_, name) if is_deps_section(name) => Section::Deps,
        _ => Section::Other,
    }
}

fn is_target_array(name: &str) -> bool {
    matches!(name, "bin" | "example" | "test" | "bench")
}

/// Arrays cargo reads as a set rather than a sequence. `authors` is deliberately
/// absent: the Cargo Book gives its order meaning.
fn sortable_array(section: Section, name: &str) -> bool {
    match section {
        Section::Features => true,
        Section::DepEntry => name == "features",
        Section::Package => matches!(name, "keywords" | "categories" | "exclude" | "include"),
        Section::Workspace => matches!(name, "members" | "default-members" | "exclude"),
        _ => false,
    }
}

fn reorder(table: &mut Table, source: &str, style: &TomlStyle, rule: Rule, guard: Guard<'_>) {
    let names: Vec<String> = table.iter().map(|(name, _)| name.to_owned()).collect();
    if names.len() < 2 {
        return;
    }

    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    let walls: Vec<&str> = borrowed
        .iter()
        .copied()
        .filter(|name| guard.touched(name))
        .collect();
    let order = run_order(table, source, &borrowed, rule, style.sort_grouped, &walls);
    if order.iter().copied().eq(0..order.len()) {
        return;
    }

    // Vertical spacing is positional, not entry-owned: the blank above a body
    // line and the blanks between header blocks belong to the slot rather than
    // to whichever entry happened to sit there, so they are lifted off before
    // the permutation and put back afterwards. A comment block still travels
    // with the entry it introduces.
    //
    // A walled entry is left out of both lists. Its own subtree's positions are
    // not always a contiguous run — a document may write `[a.b]` above `[a]` —
    // so redistributing them would hand its header someone else's slot and carry
    // the frozen lines somewhere they were not written.
    let mut blanks = Vec::new();
    body_blanks(table, source, &walls, &mut blanks);
    set_body_blanks(table, source, &walls, &mut std::iter::repeat(String::new()));

    let mut slots = Vec::new();
    for (name, item) in table.iter() {
        if !walls.contains(&name) {
            collect_slots(item, source, &mut slots);
        }
    }
    slots.sort_by_key(|(position, _)| *position);

    let mut taken = Vec::with_capacity(order.len());
    for &index in &order {
        let entry = table
            .remove_entry(&names[index])
            .expect("key came from this table");
        taken.push(entry);
    }
    for (key, item) in taken {
        table.insert_formatted(&key, item);
    }

    set_body_blanks(table, source, &walls, &mut blanks.into_iter());

    let mut slots = slots.into_iter();
    for (key, item) in table.iter_mut() {
        if !walls.contains(&key.get()) {
            assign_slots(item, source, &mut slots);
        }
    }
}

/// A permutation that never moves an entry out of the run it sits in. `--sort-grouped`
/// makes a blank line end a run, so the groups keep their sizes; an entry a
/// directive froze always ends one and opens the next, because sorting past it
/// would carry the region away from the lines it was written against.
fn run_order(
    table: &Table,
    source: &str,
    names: &[&str],
    rule: Rule,
    grouped: bool,
    walls: &[&str],
) -> Vec<usize> {
    let walled: Vec<bool> = names.iter().map(|name| walls.contains(name)).collect();
    let starts: Vec<bool> = table
        .iter()
        .enumerate()
        .map(|(index, (name, item))| {
            walled[index]
                || index.checked_sub(1).is_some_and(|before| walled[before])
                || (grouped && entry_starts_group(table, name, item, source))
        })
        .collect();

    let mut order = Vec::with_capacity(names.len());
    let mut start = 0;
    for index in 1..=names.len() {
        if index < names.len() && !starts[index] {
            continue;
        }
        let group = rule.order(&names[start..index]);
        order.extend(group.into_iter().map(|offset| start + offset));
        start = index;
    }
    order
}

/// Whether a blank line opens the block this entry renders. An implicit table
/// carries no decor of its own — `[package.metadata.docs.rs]` with no `[package]`
/// above it — so the blank has to be read off the first header its subtree
/// actually writes, not off the entry.
fn entry_starts_group(table: &Table, name: &str, item: &Item, source: &str) -> bool {
    match item {
        Item::Table(child) if child.is_dotted() => {
            let mut blanks = Vec::new();
            body_blanks(child, source, &[], &mut blanks);
            blanks.first().is_some_and(|blank| !blank.is_empty())
        }
        Item::Table(_) | Item::ArrayOfTables(_) => {
            let mut slots = Vec::new();
            collect_slots(item, source, &mut slots);
            slots
                .into_iter()
                .min_by(|(left, _), (right, _)| left.cmp(right))
                .is_some_and(|(_, blanks)| !blanks.is_empty())
        }
        _ => table
            .get_key_value(name)
            .is_some_and(|(key, _)| !leading_blanks(key.leaf_decor(), source).is_empty()),
    }
}

fn reorder_inline(table: &mut InlineTable, rule: Rule) {
    let names: Vec<String> = table.iter().map(|(name, _)| name.to_owned()).collect();
    if names.len() < 2 {
        return;
    }

    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    let order = rule.order(&borrowed);
    if order.iter().copied().eq(0..order.len()) {
        return;
    }

    // An inline table renders in map order and the layout pass rewrites every
    // separator, so the permutation alone is the whole rewrite here.
    let mut taken = Vec::with_capacity(order.len());
    for &index in &order {
        let entry = table
            .remove_entry(&names[index])
            .expect("key came from this table");
        taken.push(entry);
    }
    for (key, value) in taken {
        table.insert_formatted(&key, value);
    }
}

/// Reordering elements past an interior comment would move the comment away from
/// what it describes, so a commented array is left alone entirely.
fn sort_array(array: &mut Array, source: &str) {
    if array.len() < 2 || array_body_has_comment(array, source) {
        return;
    }

    let Some(values) = array.iter().map(Value::as_str).collect::<Option<Vec<_>>>() else {
        return;
    };
    let order = Rule::Version.order(&values);
    if order.iter().copied().eq(0..order.len()) {
        return;
    }

    let mut taken: Vec<Option<Value>> = (0..array.len()).map(|_| Some(array.remove(0))).collect();
    for &index in &order {
        let value = taken[index].take().expect("each index is used once");
        array.push_formatted(value);
    }
}

fn sort_target_array(children: &mut ArrayOfTables, source: &str) {
    let names: Vec<String> = children
        .iter()
        .map(|child| {
            child
                .get("name")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    if names.len() < 2 {
        return;
    }

    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    let order = Rule::Version.order(&borrowed);
    if order.iter().copied().eq(0..order.len()) {
        return;
    }

    let mut slots = Vec::new();
    for child in children.iter() {
        collect_subtree_slots(child, source, &mut slots);
    }
    slots.sort_by_key(|(position, _)| *position);

    let mut taken: Vec<Option<Table>> = (0..children.len())
        .map(|_| Some(children.remove(0)))
        .collect();
    for &index in &order {
        children.push(taken[index].take().expect("each index is used once"));
    }

    let mut slots = slots.into_iter();
    for child in children.iter_mut() {
        assign_subtree_slots(child, source, &mut slots);
    }
}

/// Header tables render in `position` order, not map order, so permuting the map
/// alone is invisible for `[dependencies.serde]`-style entries. Each direct
/// child's whole subtree is handed a contiguous run of the slots the group
/// already occupied, which keeps the block in place in the wider document and
/// converges after one pass.
type Slot = (isize, String);

fn collect_slots(item: &Item, source: &str, out: &mut Vec<Slot>) {
    match item {
        Item::Table(table) => collect_subtree_slots(table, source, out),
        Item::ArrayOfTables(tables) => {
            for table in tables {
                collect_subtree_slots(table, source, out);
            }
        }
        _ => {}
    }
}

fn collect_subtree_slots(table: &Table, source: &str, out: &mut Vec<Slot>) {
    if !table.is_dotted()
        && let Some(position) = table.position()
    {
        out.push((position, leading_blanks(table.decor(), source)));
    }
    for (_, item) in table {
        collect_slots(item, source, out);
    }
}

fn assign_slots(item: &mut Item, source: &str, slots: &mut impl Iterator<Item = Slot>) {
    match item {
        Item::Table(table) => assign_subtree_slots(table, source, slots),
        Item::ArrayOfTables(tables) => {
            for table in tables.iter_mut() {
                assign_subtree_slots(table, source, slots);
            }
        }
        _ => {}
    }
}

fn assign_subtree_slots(table: &mut Table, source: &str, slots: &mut impl Iterator<Item = Slot>) {
    if !table.is_dotted()
        && table.position().is_some()
        && let Some((position, blank)) = slots.next()
    {
        table.set_position(Some(position));
        set_leading_blanks(table.decor_mut(), source, &blank);
    }
    for (_, item) in table.iter_mut() {
        assign_slots(item, source, slots);
    }
}

/// The blank lines above each line a table's body renders, in render order. A
/// dotted entry renders from the leaf of its path, so the decor that carries the
/// blank lives one or more levels down.
///
/// `skip` names the entries of this table that are walls; it is empty for every
/// nested call, whose keys live in their own namespace.
fn body_blanks(table: &Table, source: &str, skip: &[&str], out: &mut Vec<String>) {
    for (name, item) in table {
        if skip.contains(&name) {
            continue;
        }
        match item {
            Item::Value(_) => {
                if let Some((key, _)) = table.get_key_value(name) {
                    out.push(leading_blanks(key.leaf_decor(), source));
                }
            }
            Item::Table(child) if child.is_dotted() => body_blanks(child, source, &[], out),
            _ => {}
        }
    }
}

fn set_body_blanks(
    table: &mut Table,
    source: &str,
    skip: &[&str],
    blanks: &mut impl Iterator<Item = String>,
) {
    for (mut key, item) in table.iter_mut() {
        if skip.contains(&key.get()) {
            continue;
        }
        match item {
            Item::Value(_) => {
                if let Some(blank) = blanks.next() {
                    set_leading_blanks(key.leaf_decor_mut(), source, &blank);
                }
            }
            Item::Table(child) if child.is_dotted() => set_body_blanks(child, source, &[], blanks),
            _ => {}
        }
    }
}

/// The run of newlines a prefix opens with. One newline is the line the entry
/// sits on, so anything beyond the first is a blank line above it.
fn leading_blanks(decor: &Decor, source: &str) -> String {
    let prefix = raw_to_str(decor.prefix(), source);
    let body = prefix.trim_start_matches(['\n', '\r']);
    prefix[..prefix.len() - body.len()].to_owned()
}

fn set_leading_blanks(decor: &mut Decor, source: &str, blanks: &str) {
    let current = raw_to_str(decor.prefix(), source);
    let updated = format!("{blanks}{}", current.trim_start_matches(['\n', '\r']));
    if current != updated {
        decor.set_prefix(updated);
    }
}

#[cfg(test)]
mod tests {
    use toml_edit::DocumentMut;

    use super::*;
    use crate::toml_fmt::format_toml;

    fn style(build: impl FnOnce(&mut TomlStyle)) -> TomlStyle {
        let mut style = TomlStyle::default();
        build(&mut style);
        style
    }

    /// `sort_document` alone, with no layout pass over it, so what is asserted
    /// is the permutation rather than the formatter's rendering of it.
    fn sorted(source: &str, style: &TomlStyle) -> String {
        let mut doc: DocumentMut = source.parse().expect("test input parses");
        sort_document(doc.as_table_mut(), source, style, Guard::default());
        doc.to_string()
    }

    fn keys(source: &str, style: &TomlStyle) -> Vec<String> {
        sorted(source, style)
            .parse::<DocumentMut>()
            .expect("output parses")
            .as_table()
            .iter()
            .map(|(key, _)| key.to_owned())
            .collect()
    }

    fn keys_of(rendered: &str, path: &str) -> Vec<String> {
        let doc: DocumentMut = rendered.parse().expect("output parses");
        let mut item = doc.as_item().clone();
        for segment in path.split('.').filter(|segment| !segment.is_empty()) {
            item = item.get(segment).expect("path exists").clone();
        }
        match item {
            Item::Table(table) => table.iter().map(|(key, _)| key.to_owned()).collect(),
            Item::Value(Value::InlineTable(table)) => {
                table.iter().map(|(key, _)| key.to_owned()).collect()
            }
            other => panic!("not a table: {other:?}"),
        }
    }

    // ------------------------------------------------------------------ rules

    #[test]
    fn version_order_is_not_lexicographic() {
        let keys = ["x-10", "x-9", "x-1"];
        assert_eq!(Rule::Version.order(&keys), vec![2, 1, 0]);
    }

    #[test]
    fn known_order_keeps_unknown_keys_where_they_were() {
        let keys = ["zzz", "version", "aaa", "name"];
        let order = Rule::Known(&["name", "version"]).order(&keys);
        assert_eq!(
            order.iter().map(|&index| keys[index]).collect::<Vec<_>>(),
            vec!["name", "version", "zzz", "aaa"]
        );
    }

    #[test]
    fn the_style_guide_puts_description_last_and_version_sorts_the_middle() {
        let keys = ["description", "edition-10", "name", "edition-9", "version"];
        let order = Rule::StyleGuidePackage.order(&keys);
        assert_eq!(
            order.iter().map(|&index| keys[index]).collect::<Vec<_>>(),
            vec!["name", "version", "edition-9", "edition-10", "description"]
        );
    }

    #[test]
    fn every_rule_is_a_permutation_of_its_input() {
        let keys = ["b", "name", "a", "description", "version"];
        for rule in [
            Rule::Version,
            Rule::Known(PACKAGE_ORDER),
            Rule::StyleGuidePackage,
        ] {
            let mut order = rule.order(&keys);
            order.sort_unstable();
            assert_eq!(order, (0..keys.len()).collect::<Vec<_>>(), "{rule:?}");
        }
    }

    #[test]
    fn a_rule_is_offered_only_when_its_own_switch_is_on() {
        assert!(rule_for(Section::Deps, &TomlStyle::default()).is_none());
        assert_eq!(
            rule_for(Section::Deps, &style(|style| style.sort_deps = true)),
            Some(Rule::Version)
        );
        assert_eq!(
            rule_for(Section::Package, &style(|style| style.sort_package = true)),
            Some(Rule::Known(PACKAGE_ORDER))
        );
        assert_eq!(
            rule_for(
                Section::Package,
                &style(|style| {
                    style.sort_package = true;
                    style.package_order = PackageOrder::StyleGuide;
                })
            ),
            Some(Rule::StyleGuidePackage)
        );
        assert_eq!(
            rule_for(
                Section::DepEntry,
                &style(|style| style.sort_dep_fields = true)
            ),
            Some(Rule::Known(DEP_ORDER))
        );
        assert_eq!(
            rule_for(
                Section::Features,
                &style(|style| style.sort_features = true)
            ),
            Some(Rule::Version)
        );
        assert_eq!(
            rule_for(Section::Root, &style(|style| style.sort_tables = true)),
            Some(Rule::Known(TABLE_ORDER))
        );
    }

    /// `--sort-keys` is the catch-all, and the two sections that own an order
    /// must not fall through to it or they would be sorted twice by two rules.
    #[test]
    fn sort_keys_does_not_reach_the_root_or_the_package_table() {
        let style = style(|style| style.sort_keys = true);
        assert!(rule_for(Section::Root, &style).is_none());
        assert!(rule_for(Section::Package, &style).is_none());
        assert_eq!(rule_for(Section::Other, &style), Some(Rule::Version));
        assert_eq!(rule_for(Section::DepEntry, &style), Some(Rule::Version));
    }

    // -------------------------------------------------------------- descent

    #[test]
    fn descent_names_each_section() {
        assert_eq!(descend(Section::Root, "package", false), Section::Package);
        assert_eq!(
            descend(Section::Root, "workspace", false),
            Section::Workspace
        );
        assert_eq!(
            descend(Section::Workspace, "package", false),
            Section::Package
        );
        assert_eq!(descend(Section::Root, "features", false), Section::Features);
        assert_eq!(descend(Section::Root, "patch", false), Section::Patch);
        assert_eq!(descend(Section::Root, "replace", false), Section::Deps);
        assert_eq!(descend(Section::Patch, "crates-io", false), Section::Deps);
        assert_eq!(descend(Section::Deps, "serde", false), Section::DepEntry);
        assert_eq!(
            descend(Section::DepEntry, "anything", false),
            Section::Other
        );
        assert_eq!(
            descend(Section::Root, "dev-dependencies", false),
            Section::Deps
        );
        assert_eq!(descend(Section::Root, "whatever", false), Section::Other);
    }

    /// A `Cargo.lock` writes `[[package]]`, which is a list of locked crates and
    /// not the manifest's `[package]`; giving it the canonical field order would
    /// rewrite a file cargo owns.
    #[test]
    fn an_array_of_tables_named_package_is_not_the_package_table() {
        assert_eq!(descend(Section::Root, "package", true), Section::Other);
        assert_eq!(descend(Section::Root, "workspace", true), Section::Other);
    }

    #[test]
    fn only_set_valued_arrays_are_sortable() {
        assert!(sortable_array(Section::Features, "anything"));
        assert!(sortable_array(Section::DepEntry, "features"));
        assert!(!sortable_array(Section::DepEntry, "default-features"));
        assert!(sortable_array(Section::Package, "keywords"));
        assert!(sortable_array(Section::Workspace, "members"));
        assert!(!sortable_array(Section::Root, "keywords"));
    }

    /// The Cargo Book gives `authors` an order, so it is a sequence and not a
    /// set. Sorting it would rewrite what the manifest means.
    #[test]
    fn authors_is_never_sorted() {
        assert!(!sortable_array(Section::Package, "authors"));
        assert!(!sortable_array(Section::Workspace, "authors"));
    }

    #[test]
    fn target_arrays_are_the_cargo_target_kinds() {
        for name in ["bin", "example", "test", "bench"] {
            assert!(is_target_array(name), "{name}");
        }
        assert!(!is_target_array("lib"));
        assert!(!is_target_array("dependencies"));
    }

    // ------------------------------------------------------------- documents

    #[test]
    fn nothing_moves_with_every_switch_off() {
        let source = "[dependencies]\nzzz = \"1\"\naaa = \"2\"\n";
        assert_eq!(sorted(source, &TomlStyle::default()), source);
    }

    #[test]
    fn a_table_of_one_key_is_left_alone() {
        let source = "[dependencies]\nonly = \"1\"\n";
        assert_eq!(
            sorted(source, &style(|style| style.sort_deps = true)),
            source
        );
    }

    #[test]
    fn dependencies_are_version_sorted() {
        let rendered = sorted(
            "[dependencies]\nserde-10 = \"1\"\nserde-9 = \"1\"\nalpha = \"1\"\n",
            &style(|style| style.sort_deps = true),
        );
        assert_eq!(
            keys_of(&rendered, "dependencies"),
            ["alpha", "serde-9", "serde-10"]
        );
    }

    #[test]
    fn every_dependency_section_is_reached() {
        let style = style(|style| style.sort_deps = true);
        for section in [
            "dependencies",
            "dev-dependencies",
            "build-dependencies",
            "replace",
        ] {
            let rendered = sorted(&format!("[{section}]\nb = \"1\"\na = \"1\"\n"), &style);
            assert_eq!(keys_of(&rendered, section), ["a", "b"], "{section}");
        }
    }

    /// `[patch]` holds registries, and the dependency tables are one level
    /// further down than everywhere else.
    #[test]
    fn patch_sorts_one_level_down() {
        let rendered = sorted(
            "[patch.crates-io]\nzzz = { path = \"z\" }\naaa = { path = \"a\" }\n",
            &style(|style| style.sort_deps = true),
        );
        assert_eq!(keys_of(&rendered, "patch.crates-io"), ["aaa", "zzz"]);
    }

    #[test]
    fn the_package_table_takes_the_book_order() {
        let rendered = sorted(
            "[package]\ncustom = 1\nlicense = \"MIT\"\nname = \"x\"\nversion = \"1\"\n",
            &style(|style| style.sort_package = true),
        );
        let keys = keys_of(&rendered, "package");
        assert_eq!(&keys[..2], ["name", "version"]);
        assert_eq!(keys.last().unwrap(), "custom");
    }

    #[test]
    fn dependency_fields_take_the_canonical_order() {
        let rendered = sorted(
            "[dependencies.serde]\nfeatures = [\"derive\"]\nversion = \"1\"\n",
            &style(|style| style.sort_dep_fields = true),
        );
        assert_eq!(
            keys_of(&rendered, "dependencies.serde"),
            ["version", "features"]
        );
    }

    /// An inline table renders in map order, so `reorder_inline` is the only
    /// thing that can move its keys; the table path above uses a different one.
    #[test]
    fn an_inline_dependency_is_reordered_too() {
        let rendered = sorted(
            "[dependencies]\nserde = { features = [\"derive\"], version = \"1\" }\n",
            &style(|style| style.sort_dep_fields = true),
        );
        assert_eq!(
            keys_of(&rendered, "dependencies.serde"),
            ["version", "features"]
        );
    }

    #[test]
    fn feature_lists_are_sorted_as_sets() {
        let rendered = sorted(
            "[features]\nb = [\"z\", \"a\"]\na = []\n",
            &style(|style| {
                style.sort_features = true;
                style.sort_arrays = true;
            }),
        );
        assert_eq!(keys_of(&rendered, "features"), ["a", "b"]);
        assert!(
            rendered.find("\"a\"").unwrap() < rendered.find("\"z\"").unwrap(),
            "{rendered}"
        );
    }

    /// A comment inside an array is written against the element it sits by, so
    /// moving elements past it would change what it says.
    #[test]
    fn an_array_holding_a_comment_is_left_alone() {
        let source = "[features]\nb = [\n    # keep\n    \"z\",\n    \"a\",\n]\n";
        let rendered = sorted(source, &style(|style| style.sort_arrays = true));
        let index_z = rendered.find("\"z\"").unwrap();
        let index_a = rendered.find("\"a\"").unwrap();
        assert!(index_z < index_a, "{rendered}");
    }

    #[test]
    fn an_array_of_mixed_types_is_left_alone() {
        let source = "[features]\nb = [\"z\", 1]\n";
        assert_eq!(
            sorted(source, &style(|style| style.sort_arrays = true)),
            source
        );
    }

    #[test]
    fn target_arrays_are_sorted_by_name() {
        let rendered = sorted(
            "[[bin]]\nname = \"zzz\"\n\n[[bin]]\nname = \"aaa\"\n",
            &style(|style| style.sort_targets = true),
        );
        assert!(
            rendered.find("aaa").unwrap() < rendered.find("zzz").unwrap(),
            "{rendered}"
        );
    }

    #[test]
    fn the_document_sequence_is_the_canonical_one() {
        let rendered = sorted(
            "[dependencies]\na = \"1\"\n\n[package]\nname = \"x\"\n",
            &style(|style| style.sort_tables = true),
        );
        assert_eq!(keys(&rendered, &TomlStyle::default())[0], "package");
    }

    #[test]
    fn sort_keys_reaches_a_plain_table() {
        let rendered = sorted(
            "[whatever]\nzzz = 1\naaa = 2\n",
            &style(|style| style.sort_keys = true),
        );
        assert_eq!(keys_of(&rendered, "whatever"), ["aaa", "zzz"]);
    }

    /// `--sort-grouped` makes a blank line end a run, so each block keeps its
    /// size and its members are sorted only among themselves.
    #[test]
    fn a_blank_line_bounds_a_group() {
        let source = "[dependencies]\nzzz = \"1\"\nmmm = \"2\"\n\nbbb = \"3\"\naaa = \"4\"\n";
        let style = style(|style| {
            style.sort_deps = true;
            style.sort_grouped = true;
        });
        assert_eq!(
            keys_of(&sorted(source, &style), "dependencies"),
            ["mmm", "zzz", "aaa", "bbb"]
        );
    }

    #[test]
    fn without_grouping_a_blank_line_is_not_a_wall() {
        let source = "[dependencies]\nzzz = \"1\"\nmmm = \"2\"\n\nbbb = \"3\"\naaa = \"4\"\n";
        assert_eq!(
            keys_of(
                &sorted(source, &style(|style| style.sort_deps = true)),
                "dependencies"
            ),
            ["aaa", "bbb", "mmm", "zzz"]
        );
    }

    /// A `# fmt: off` region is written against the lines it fences, so sorting
    /// must not carry an entry out of it. Stated through `format_toml` because
    /// the wall is a `Guard`, which only the directive pass can build.
    #[test]
    fn a_frozen_entry_walls_the_sort() {
        let source = concat!(
            "[dependencies]\n",
            "zzz = \"1\"\n",
            "# fmt: off\n",
            "mmm = \"2\"\n",
            "# fmt: on\n",
            "aaa = \"3\"\n",
        );
        let out = format_toml(source, &style(|style| style.sort_deps = true)).expect("formats");
        assert_eq!(
            keys_of(&out, "dependencies"),
            ["zzz", "mmm", "aaa"],
            "{out}"
        );
    }

    #[test]
    fn a_document_with_no_directives_sorts_across_the_whole_table() {
        let source = "[dependencies]\nzzz = \"1\"\nmmm = \"2\"\naaa = \"3\"\n";
        let out = format_toml(source, &style(|style| style.sort_deps = true)).expect("formats");
        assert_eq!(keys_of(&out, "dependencies"), ["aaa", "mmm", "zzz"]);
    }
}
