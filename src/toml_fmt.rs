use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    fmt::Write as _,
};

use ahash::AHashMap;
use toml_edit::{
    Array, Decor, Document, DocumentMut, InlineTable, Item, Key, KeyMut, RawString, Table,
    TableLike, TomlError, Value,
};

use crate::{
    semver::PartialVersion,
    toml_align,
    toml_directive::{Cover, Freeze, Guard, Regions},
    toml_sort,
    toml_style::{ArrayStyle, InlineTableStyle, Spacing, TomlStyle, TomlVersion, TrailingComma},
    toml_width,
    versions::{DepRequest, Resolution, SkipReason, VersionLookup, VersionRecord},
};

/// What a manifest cannot say about itself: the workspace value behind
/// `rust-version.workspace = true`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ManifestContext {
    pub workspace_rust_version: Option<PartialVersion>,
}

pub struct TomlFormatOutput {
    pub text: String,
    /// One entry per dependency a version lookup considered, whether or not it
    /// was rewritten. Empty unless a lookup was supplied.
    pub versions: Vec<VersionRecord>,
}

pub fn format_toml(input: &str, style: &TomlStyle) -> Result<String, TomlError> {
    format_toml_inner(input, style, None, &ManifestContext::default()).map(|output| output.text)
}

pub fn format_toml_with_versions(
    input: &str,
    style: &TomlStyle,
    lookup: &dyn VersionLookup,
    context: &ManifestContext,
) -> Result<TomlFormatOutput, TomlError> {
    format_toml_inner(input, style, Some(lookup), context)
}

/// Every requirement a run would act on, for a caller that wants to resolve a
/// whole tree's crates before formatting any of it. Frozen regions are excluded,
/// so a `# fmt: off` dependency costs no request.
pub fn owned_dep_requests(
    input: &str,
    style: &TomlStyle,
) -> Result<Vec<(String, String)>, TomlError> {
    let regions = if style.directives {
        Regions::scan(input)
    } else {
        Regions::none()
    };
    let parsed = Document::parse(input)?;
    let freeze = (!regions.is_empty())
        .then(|| Freeze::build(parsed.as_table(), parsed.trailing(), &regions));
    let guard = Guard::of(freeze.as_ref());

    let mut requests = Vec::new();
    collect_dep_requests(parsed.as_table(), Scope::ROOT, guard, &mut requests);
    Ok(requests
        .into_iter()
        .map(|request| (request.name.to_owned(), request.req.to_owned()))
        .collect())
}

fn format_toml_inner(
    input: &str,
    style: &TomlStyle,
    lookup: Option<&dyn VersionLookup>,
    context: &ManifestContext,
) -> Result<TomlFormatOutput, TomlError> {
    let regions = if style.directives {
        Regions::scan(input)
    } else {
        Regions::none()
    };
    if regions.is_empty() {
        return format_regions(input, style, lookup, context, &regions);
    }

    // `toml_edit` regroups interleaved dotted keys as it renders, which moves
    // lines and can carry one marker past another. Reading the regions off the
    // text it will actually produce is what keeps a second pass a no-op.
    let regrouped = Document::parse(input)?.into_mut().to_string();
    if regrouped == input {
        return format_regions(input, style, lookup, context, &regions);
    }
    let regions = Regions::scan(&regrouped);
    format_regions(&regrouped, style, lookup, context, &regions)
}

fn format_regions(
    input: &str,
    style: &TomlStyle,
    lookup: Option<&dyn VersionLookup>,
    context: &ManifestContext,
    regions: &Regions,
) -> Result<TomlFormatOutput, TomlError> {
    // Spans live only until `into_mut` despans the tree, so what the directives
    // cover has to be read off the parse before anything is touched.
    let parsed = Document::parse(input)?;
    let freeze =
        (!regions.is_empty()).then(|| Freeze::build(parsed.as_table(), parsed.trailing(), regions));
    // Spans die with `into_mut`, so a finding's location has to be taken from
    // the parse; a dependency the layout later moves simply has none.
    let spans = lookup
        .map(|_| dep_spans(parsed.as_table()))
        .unwrap_or_default();
    let rust_version = lookup.and_then(|_| manifest_rust_version(parsed.as_table(), context));
    let mut doc = parsed.into_mut();
    let guard = Guard::of(freeze.as_ref());

    // Both rewrites run before layout: sorting decides which header opens the
    // document, and a shortened key changes what fits on a line.
    if style.sorts() {
        toml_sort::sort_document(doc.as_table_mut(), input, style, guard);
    }
    if style.normalize_keys {
        normalize_table_keys(doc.as_table_mut(), input, guard);
    }

    // Promotion is width-driven, so it has to see the final key spellings, and
    // it can change which header opens the document — hence a throwaway probe
    // before the formatter that reads `leading_header`.
    Formatter {
        source: input,
        style,
        lookup: None,
        rust_version: None,
        spans: AHashMap::default(),
        records: RefCell::new(Vec::new()),
        leading_header: None,
        header_index: Cell::new(0),
    }
    .promote_sections(&mut doc, guard);

    let formatter = Formatter {
        source: input,
        style,
        lookup,
        rust_version,
        spans,
        records: RefCell::new(Vec::new()),
        leading_header: leading_header(doc.as_table()),
        header_index: Cell::new(0),
    };

    formatter.prewarm_versions(&doc, guard);
    formatter.table(
        doc.as_table_mut(),
        Scope::ROOT,
        TableKind::Root,
        Level::ROOT,
        guard,
    );
    formatter.document_trailing(&mut doc, freeze.as_ref().and_then(Freeze::trailing));

    let versions = formatter.records.into_inner();
    let rendered = render(&doc, input);
    if !style.aligns() {
        return Ok(TomlFormatOutput {
            text: rendered,
            versions,
        });
    }
    Ok(TomlFormatOutput {
        text: toml_align::align(&rendered, style).into_owned(),
        versions,
    })
}

fn render(doc: &DocumentMut, input: &str) -> String {
    let mut out = String::with_capacity(input.len() + input.len() / 8 + 1);
    let _ = write!(out, "{doc}");

    if out.is_empty() {
        if !input.is_empty() {
            out.push('\n');
        }
    } else if !out.ends_with('\n') {
        out.push('\n');
    }

    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableKind {
    Root,
    /// A `a.b = 1` line renders from the leaf, so the columns its ancestors
    /// already spent have to reach the value that decides its own layout.
    Dotted {
        key_start: usize,
    },
    Header {
        array_of_tables: bool,
    },
}

/// Indentation levels a table hands down. `inherited` is what a child header
/// takes; `body` is where this table's own entries sit.
#[derive(Clone, Copy)]
struct Level {
    inherited: usize,
    body: usize,
}

impl Level {
    const ROOT: Self = Self {
        inherited: 0,
        body: 0,
    };
}

/// Where a value starts and how deeply it nests, so a container can decide its
/// own layout before its children are touched.
#[derive(Clone, Copy)]
struct Ctx {
    indent: usize,
    /// Column the value begins at, key and `" = "` included.
    column: usize,
    /// Columns still owed on the value's line after it ends: the comma that
    /// separates it from the next entry, and any comment sharing the line.
    reserved: usize,
    dotted_ok: bool,
    /// Set inside a container already committed to one line, where no child may
    /// break however wide it is.
    force_inline: bool,
}

/// How a rebuilt comment block sits on the page.
#[derive(Clone, Copy)]
struct CommentBlock {
    /// Emit the line break that ends the line the block opens on. Set inside a
    /// wrapped container, whose opening bracket has no newline of its own.
    open_newline: bool,
    /// Level the comment lines take.
    indent: usize,
    /// Level the block leaves the following line at.
    close: usize,
}

impl CommentBlock {
    /// A table body or the document itself, where the previous line has already
    /// ended and a blank line above the block is the author's.
    const fn flush(indent: usize) -> Self {
        Self {
            open_newline: false,
            indent,
            close: indent,
        }
    }

    const fn wrapped(indent: usize) -> Self {
        Self {
            open_newline: true,
            indent,
            close: indent,
        }
    }

    /// The run before a closing bracket, whose comments sit one level in from
    /// the bracket they precede.
    const fn closing(indent: usize) -> Self {
        Self {
            open_newline: true,
            indent: indent + 1,
            close: indent,
        }
    }
}

struct Formatter<'a> {
    source: &'a str,
    style: &'a TomlStyle,
    lookup: Option<&'a dyn VersionLookup>,
    rust_version: Option<PartialVersion>,
    /// Byte offset of each dependency key, keyed by section and key.
    spans: AHashMap<String, usize>,
    records: RefCell<Vec<VersionRecord>>,
    /// Traversal index of the header that opens the document, if one does.
    leading_header: Option<usize>,
    header_index: Cell<usize>,
}

impl<'a> Formatter<'a> {
    fn prewarm_versions(&self, doc: &DocumentMut, guard: Guard<'_>) {
        let Some(lookup) = self.lookup else {
            return;
        };

        let mut requests = Vec::new();
        collect_dep_requests(doc.as_table(), Scope::ROOT, guard, &mut requests);
        if !requests.is_empty() {
            lookup.prewarm(&requests);
        }
    }

    fn record(
        &self,
        scope: Scope<'_>,
        crate_key: &str,
        name: &str,
        requirement: &str,
        outcome: Resolution,
    ) {
        let position = self
            .spans
            .get(&span_key(scope.path, crate_key))
            .and_then(|offset| crate::error::line_column(self.source, *offset));
        self.records.borrow_mut().push(VersionRecord {
            crate_name: name.to_owned(),
            section: scope.path.to_owned(),
            requirement: requirement.to_owned(),
            line: position.map(|(line, _)| line),
            column: position.map(|(_, column)| column),
            outcome,
        });
    }

    fn child_path(&self, scope: Scope<'_>, key: &str) -> String {
        if self.lookup.is_none() {
            return String::new();
        }
        if scope.path.is_empty() {
            key.to_owned()
        } else {
            format!("{}.{key}", scope.path)
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the dispatch over every shape a table entry can take. Each arm is \
                  short; there are simply many of them, and splitting the match \
                  scatters the decor handling they share."
    )]
    fn table(
        &self,
        table: &mut Table,
        scope: Scope<'_>,
        kind: TableKind,
        level: Level,
        guard: Guard<'_>,
    ) {
        let header = match kind {
            TableKind::Header { array_of_tables } => {
                let index = self.header_index.get();
                self.header_index.set(index + 1);
                Some((index, array_of_tables))
            }
            TableKind::Root | TableKind::Dotted { .. } => None,
        };

        let style = self.style;
        let (header_level, level) = match kind {
            TableKind::Root => (0, Level::ROOT),
            TableKind::Dotted { .. } => (level.body, level),
            TableKind::Header { array_of_tables } => {
                // A header nobody writes indents nothing beneath it, so an
                // implicit `[a.b.c]` with no `[a]` above it stays flush.
                let renders = renders_header(table, array_of_tables);
                let body = level.inherited + usize::from(style.indent_entries && renders);
                let inherited = level.inherited + usize::from(style.indent_tables && renders);
                (level.inherited, Level { inherited, body })
            }
        };
        let key_start = match kind {
            TableKind::Dotted { key_start } => key_start,
            TableKind::Root | TableKind::Header { .. } => style.indent.width(level.body),
        };
        let body_block = CommentBlock::flush(level.body);

        for (mut key, item) in table.iter_mut() {
            let frozen_key = guard.key_frozen(key.get());
            match item {
                Item::Value(value) => {
                    let entry = guard.child(key.get(), 0);
                    if frozen_key {
                        if scope.kind.holds_deps() {
                            self.record_frozen(scope, key.get(), value);
                        }
                        // The line is the author's, but only as far up as the
                        // marker: whatever sits above that is still ours. A run
                        // with no marker in it lies wholly inside the region.
                        if let Some(cover) = entry.prefix() {
                            let prefix = self.comment_block(
                                key.leaf_decor().prefix(),
                                body_block,
                                Some(cover),
                            );
                            set_prefix(key.leaf_decor_mut(), self.source, &prefix);
                        }
                        continue;
                    }
                    if scope.kind.holds_deps() {
                        self.pin_dep(key.get(), value, scope);
                    }
                    if scope.kind == ScopeKind::Deps {
                        self.apply_cargo_conventions(value);
                    }
                    let column = self.key_end(key_start, &key) + 3;
                    let reserved =
                        self.comment_width(&raw_to_str(value.decor().suffix(), self.source));
                    self.value(
                        value,
                        Ctx {
                            indent: level.body,
                            column,
                            reserved,
                            dotted_ok: true,
                            force_inline: false,
                        },
                    );

                    let prefix =
                        self.comment_block(key.leaf_decor().prefix(), body_block, entry.prefix());
                    let moved = match value.as_inline_table_mut() {
                        Some(inline) if inline.is_dotted() => {
                            self.take_over_dotted_leaf(inline, &prefix);
                            true
                        }
                        _ => false,
                    };
                    self.key_decor(&mut key, if moved { "" } else { &prefix });
                    self.leaf_decor(value);
                }
                Item::Table(child) => {
                    let child_guard = guard.child(key.get(), 0);
                    let child_kind = if child.is_dotted() {
                        if !frozen_key {
                            key.dotted_decor_mut().clear();
                        }
                        TableKind::Dotted {
                            key_start: self.key_end(key_start, &key) + 1,
                        }
                    } else {
                        if !frozen_key {
                            Self::header_key_decor(&mut key);
                        }
                        TableKind::Header {
                            array_of_tables: false,
                        }
                    };
                    if scope.kind.holds_deps() {
                        self.pin_dep_table(key.get(), child, scope, child_guard);
                    }
                    let child_path = self.child_path(scope, key.get());
                    self.table(
                        child,
                        scope.child(&child_path, key.get()),
                        child_kind,
                        level,
                        child_guard,
                    );
                }
                Item::ArrayOfTables(children) => {
                    if !frozen_key {
                        Self::header_key_decor(&mut key);
                    }
                    let child_path = self.child_path(scope, key.get());
                    let child_scope = scope.child(&child_path, key.get());
                    for (index, child) in children.iter_mut().enumerate() {
                        self.table(
                            child,
                            child_scope,
                            TableKind::Header {
                                array_of_tables: true,
                            },
                            level,
                            guard.child(key.get(), index),
                        );
                    }
                }
                Item::None => {}
            }
        }

        let source = self.source;
        let cover = guard.prefix();
        if !guard.own() || cover.is_some() {
            let prefix = self.comment_block(
                table.decor().prefix(),
                CommentBlock::flush(header_level),
                cover,
            );
            // A region that opened above this run owns the line the header sits
            // on, and a blank line inserted here would land between the markers.
            let prefix = if cover.is_some_and(|cover| cover.before) {
                prefix
            } else {
                self.header_prefix(table, header, prefix)
            };
            set_prefix(table.decor_mut(), source, &prefix);
        }
        if !guard.own() {
            let suffix = same_line_comment_suffix(&raw_to_str(table.decor().suffix(), source));
            set_suffix(table.decor_mut(), source, &suffix);
        }
    }

    /// The comment block above an entry.
    ///
    /// Only the stretch a directive covers is copied through; what sits above
    /// and below it is still rebuilt, so the blank lines around a marker are
    /// clamped and the entry the run introduces is still indented. The marker
    /// line itself is inside the stretch, which is what keeps `push_hash_comment`
    /// from canonicalizing the very line that asked to be left alone.
    fn comment_block(
        &self,
        prefix: Option<&RawString>,
        block: CommentBlock,
        cover: Option<Cover>,
    ) -> Cow<'a, str> {
        let current = raw_to_str(prefix, self.source);
        let Some(cover) = cover else {
            return self.rebuild_comments(&current, block);
        };
        // The offsets were measured against the source. Every pass that rewrites
        // a run leaves a frozen one alone, so they still fit; keeping the run
        // whole rather than panicking is the safe way to be wrong about that.
        debug_assert!(cover.to <= current.len(), "cover outside its run");
        if cover.to > current.len()
            || !current.is_char_boundary(cover.from)
            || !current.is_char_boundary(cover.to)
        {
            return Cow::Owned(current.into_owned());
        }

        // A rebuilt run ends with the indent of the line that follows it. The
        // head is followed by the frozen stretch, which brings its own; the tail
        // is followed by whatever the run introduces, unless a region is open
        // there, in which case that line is the author's too.
        let head = self.rebuild_comments(&current[..cover.from], block);
        let head = head.trim_end_matches([' ', '\t']);
        let tail = self.rebuild_comments(&current[cover.to..], block);
        let tail = if cover.beyond {
            tail.trim_end_matches([' ', '\t'])
        } else {
            &tail
        };

        let middle = &current[cover.from..cover.to];
        let mut out = String::with_capacity(head.len() + middle.len() + tail.len());
        out.push_str(head);
        out.push_str(middle);
        out.push_str(tail);
        Cow::Owned(out)
    }

    fn header_prefix(
        &self,
        table: &Table,
        header: Option<(usize, bool)>,
        prefix: Cow<'a, str>,
    ) -> Cow<'a, str> {
        if !self.style.blank_line_before_tables {
            return prefix;
        }
        let Some((index, array_of_tables)) = header else {
            return prefix;
        };
        if !renders_header(table, array_of_tables) {
            return prefix;
        }

        if Some(index) == self.leading_header {
            // Every leading newline, not one: with `max_blank_lines` above 1 a
            // single strip leaves a blank line that the next pass strips again,
            // so the document is not a fixed point.
            let flush = prefix.trim_start_matches('\n');
            return if flush.len() == prefix.len() {
                prefix
            } else {
                Cow::Owned(flush.to_owned())
            };
        }
        if prefix.starts_with('\n') {
            prefix
        } else {
            Cow::Owned(format!("\n{prefix}"))
        }
    }

    fn document_trailing(&self, doc: &mut DocumentMut, cover: Option<Cover>) {
        let current = raw_to_str(Some(doc.trailing()), self.source);
        let rebuilt = self.comment_block(Some(doc.trailing()), CommentBlock::flush(0), cover);
        // Trailing whitespace alone is dropped, but a directive can ask for a
        // blank line at the end of the file and mean it.
        let trailing = if cover.is_none() && rebuilt.trim().is_empty() {
            ""
        } else {
            &rebuilt
        };

        if current != trailing {
            doc.set_trailing(trailing);
        }
    }

    fn value(&self, value: &mut Value, ctx: Ctx) {
        match value {
            Value::InlineTable(table) => self.inline_table(table, ctx),
            Value::Array(array) => self.array(array, ctx),
            _ => {}
        }
    }

    fn inline_table(&self, table: &mut InlineTable, ctx: Ctx) {
        let keys = table.iter().count();
        let dotted = keys == 1 && ctx.dotted_ok && !self.collapse_drops_comment(table);

        if dotted {
            for (key, value) in table.iter_mut() {
                let column = self.key_end(ctx.column + 1, &key);
                self.value(value, Ctx { column, ..ctx });
            }
            self.collapse_to_dotted(table);
            return;
        }

        if self.inline_fits(table, keys, ctx) {
            let child = Ctx {
                indent: ctx.indent,
                column: 0,
                reserved: 0,
                dotted_ok: false,
                force_inline: true,
            };
            for (_, value) in table.iter_mut() {
                self.value(value, child);
            }
            if keys == 0 {
                Self::collapse_empty(table);
            } else {
                self.collapse_to_one_line(table);
            }
            return;
        }

        let level = ctx.indent + 1;
        let base = self.style.indent.width(level);
        let comma = self.style.trailing_comma == TrailingComma::Multiline
            && !table.is_empty()
            && self.style.toml_version != TomlVersion::V1_0;
        let mut decors = self.inline_decors(table);
        let trailing = raw_to_str(Some(table.trailing()), self.source).into_owned();
        let rest = split_container_trailing(&mut decors, &trailing, comma);
        let comments = self.same_line_comment_widths(&decors, &rest);
        let last = keys.saturating_sub(1);
        for (index, (key, value)) in table.iter_mut().enumerate() {
            let column = self.key_end(base, &key) + 3;
            let reserved = usize::from(index < last || comma) + comments[index];
            self.value(
                value,
                Ctx {
                    indent: level,
                    column,
                    reserved,
                    dotted_ok: false,
                    force_inline: false,
                },
            );
        }
        self.expand_over_lines(table, ctx.indent, comma, &decors, &rest);
    }

    /// `dep = { version = "1" }` becomes `dep = "1"`, which is the spelling
    /// cargo itself writes for a plain registry dependency. This is the one
    /// rewrite that changes the TOML value tree rather than its layout, which is
    /// why it takes a flag of its own.
    fn apply_cargo_conventions(&self, value: &mut Value) {
        if !self.style.cargo_conventions {
            return;
        }
        let Some(table) = value.as_inline_table() else {
            return;
        };
        if self.collapse_drops_comment(table) {
            return;
        }

        let values = table.get_values();
        let [(path, inner)] = values.as_slice() else {
            return;
        };
        let [key] = path.as_slice() else {
            return;
        };
        if key.get() != "version" || !inner.is_str() {
            return;
        }

        let suffix = merged_comment_suffix(
            &raw_to_str(inner.decor().suffix(), self.source),
            &raw_to_str(table.decor().suffix(), self.source),
        );
        let mut replacement = (*inner).clone();
        replacement.decor_mut().clear();
        set_suffix(replacement.decor_mut(), self.source, &suffix);
        *value = replacement;
    }

    /// `--toml-inline-tables section`: a table too wide for its line becomes its
    /// own header instead of wrapping, which is what the Rust Style Guide asks
    /// for. Promotion needs the table the entry sits in to render a header of
    /// its own, so a root-level or array-nested value falls back to wrapping.
    fn promote_sections(&self, doc: &mut DocumentMut, guard: Guard<'_>) {
        if self.inline_table_style() != InlineTableStyle::Section {
            return;
        }

        // A promoted table's own over-wide entries only become promotable once
        // it renders a header, so rounds repeat until one promotes nothing.
        loop {
            let mut promoted = Vec::new();
            self.promote_round(
                doc.as_table_mut(),
                Level::ROOT,
                false,
                &mut Vec::new(),
                &mut promoted,
                guard,
            );
            if promoted.is_empty() {
                return;
            }
            renumber_positions(doc.as_table_mut(), &promoted);
        }
    }

    fn promote_round(
        &self,
        table: &mut Table,
        level: Level,
        renders: bool,
        path: &mut Vec<Step>,
        out: &mut Vec<(Anchor, Vec<Step>)>,
        guard: Guard<'_>,
    ) {
        // A promoted header is written after the whole subtree it came out of,
        // and a directive anywhere in that subtree can be an unclosed one whose
        // region runs past the insertion point — so a table holding one promotes
        // nothing, or the new lines would land between the markers.
        if renders && !table.iter().any(|(name, _)| guard.touched(name)) {
            let key_start = self.style.indent.width(level.body);
            let order: Vec<String> = table.iter().map(|(name, _)| name.to_owned()).collect();
            let wanted: Vec<String> = table
                .iter()
                .filter(|(name, item)| self.wants_promotion(table, name, item, key_start))
                .map(|(name, _)| name.to_owned())
                .collect();

            // Anchors come first, while the map is still as written: promoting
            // moves the entry to the end of it, which would hide it from the
            // next entry's search for a sibling to follow.
            let anchors: Vec<Anchor> = wanted
                .iter()
                .map(|name| promotion_anchor(table, path, name, &wanted))
                .collect();
            for (name, anchor) in wanted.iter().zip(anchors) {
                self.promote_entry(table, name);
                let mut promoted = path.clone();
                promoted.push(Step::Key(name.clone()));
                out.push((anchor, promoted));
            }
            if !wanted.is_empty() {
                // `insert_formatted` appends, and map order decides both which
                // header the walk sees first and where the next promotion in
                // this table anchors, so the table is rebuilt as it was written.
                restore_order(table, &order);
            }
        }

        for (key, item) in table.iter_mut() {
            let Item::Table(child) = item else {
                continue;
            };
            if child.is_dotted() {
                continue;
            }
            let name = key.get().to_owned();
            let child_renders = renders_header(child, false);
            let child_level = Level {
                inherited: level.inherited + usize::from(self.style.indent_tables && child_renders),
                body: level.inherited + usize::from(self.style.indent_entries && child_renders),
            };
            let positioned = child.position().is_some();
            let child_guard = guard.child(&name, 0);
            path.push(Step::Key(name));
            self.promote_round(child, child_level, positioned, path, out, child_guard);
            path.pop();
        }
    }

    fn wants_promotion(&self, table: &Table, name: &str, item: &Item, key_start: usize) -> bool {
        let Item::Value(Value::InlineTable(inline)) = item else {
            return false;
        };
        // A multi-line inline table can hold a comment between a key and its
        // `=`, between the `=` and the value, and before the closing brace —
        // three positions a table body cannot render at all. Rather than sort
        // out which of them a given table uses, a commented table keeps the
        // braces and `Auto`'s wrapping, which loses nothing.
        if inline_body_has_comment(inline, self.source) {
            return false;
        }
        // One key collapses to a dotted line instead, and always fits.
        if inline.iter().count() < 2 {
            return false;
        }
        let Some((key, _)) = table.get_key_value(name) else {
            return false;
        };

        let column = self.text_end(key_start, &key.display_repr()) + 3;
        let reserved = self.comment_width(&raw_to_str(inline.decor().suffix(), self.source));
        !self.inline_fits(
            inline,
            inline.iter().count(),
            Ctx {
                indent: 0,
                column,
                reserved,
                dotted_ok: false,
                force_inline: false,
            },
        )
    }

    /// Rebuilt entry by entry rather than through `InlineTable::into_table`,
    /// which normalizes every decor and would drop the comments the keys carry.
    fn promote_entry(&self, table: &mut Table, name: &str) {
        let Some((mut key, item)) = table.remove_entry(name) else {
            return;
        };
        let Item::Value(Value::InlineTable(mut inline)) = item else {
            return;
        };

        let prefix = raw_to_str(key.leaf_decor().prefix(), self.source).into_owned();
        let suffix = raw_to_str(inline.decor().suffix(), self.source).into_owned();

        let names: Vec<String> = inline.iter().map(|(name, _)| name.to_owned()).collect();
        let mut promoted = Table::new();
        for name in names {
            let (key, value) = inline
                .remove_entry(&name)
                .expect("key came from this table");
            promoted.insert_formatted(&key, Item::Value(value));
        }
        promoted.set_implicit(false);
        promoted.decor_mut().set_prefix(prefix);
        promoted.decor_mut().set_suffix(suffix);

        key.leaf_decor_mut().clear();
        key.dotted_decor_mut().clear();
        table.insert_formatted(&key, Item::Table(promoted));
    }

    /// An inline table across lines is TOML 1.1, so targeting 1.0 overrides
    /// whatever `--toml-inline-tables` asked for.
    fn inline_table_style(&self) -> InlineTableStyle {
        match self.style.toml_version {
            TomlVersion::V1_0 => InlineTableStyle::Compact,
            TomlVersion::V1_1 => self.style.inline_tables,
        }
    }

    /// A comment is the only hard bar to one line; everything else is style.
    fn inline_fits(&self, table: &InlineTable, keys: usize, ctx: Ctx) -> bool {
        if inline_body_has_comment(table, self.source) {
            return false;
        }
        if ctx.force_inline {
            return true;
        }
        match self.inline_table_style() {
            InlineTableStyle::Compact => true,
            InlineTableStyle::Expand => keys <= 1 && !self.prefers_break_inline(table),
            // `Section` measures the same as `Auto`; what differs is what
            // happens to a table that does not fit, and by the time the layout
            // pass runs, `promote_sections` has already moved the ones it could.
            InlineTableStyle::Auto | InlineTableStyle::Section => {
                !self.prefers_break_inline(table)
                    && self
                        .inline_end(table, ctx.column)
                        .is_some_and(|end| end + ctx.reserved <= self.style.max_width)
            }
        }
    }

    /// Everything rendered after the sole value and before the closing brace.
    ///
    /// Two pieces, because `toml_edit` splits them: a comment written before the
    /// separating comma lands on the value's suffix and one written after it
    /// lands on the table's trailing decor. [`Self::collapse_drops_comment`]
    /// decides whether a collapse is safe by reading this, and
    /// [`Self::collapse_to_dotted`] carries the comment it found; reading it in
    /// one place is what keeps the two from disagreeing and dropping one.
    fn below_last_value(&self, table: &InlineTable) -> String {
        let Some((_, value)) = table.get_values().into_iter().next() else {
            return String::new();
        };
        let mut below = raw_to_str(value.decor().suffix(), self.source).into_owned();
        below.push_str(&raw_to_str(Some(table.trailing()), self.source));
        below
    }

    /// Collapsing to a dotted key keeps only the comment that shares the value's line,
    /// so anything below it, or above the key, has to hold the table open.
    fn collapse_drops_comment(&self, table: &InlineTable) -> bool {
        let source = self.source;
        let Some((key, value)) = table.get_values().into_iter().next() else {
            return false;
        };

        // The value's *prefix* is above it and has nowhere to go. Its suffix is
        // not checked here: that is the first half of `below_last_value`, and a
        // comment there is carried rather than blocking. Blocking on it instead
        // costs a fixed point, because the layout pass moves such a comment past
        // the separating comma and the next pass then reads it as carryable.
        if key_has_comment(&key, source) || raw_to_str(value.decor().prefix(), source).contains('#')
        {
            return true;
        }

        let below = self.below_last_value(table);
        let start = below.find(['\n', '\r']).unwrap_or(below.len());
        below[start..].contains('#')
    }

    fn collapse_empty(table: &mut InlineTable) {
        table.set_dotted(false);
        table.set_trailing_comma(false);
        table.set_trailing("");
        table.fmt();
    }

    fn collapse_to_one_line(&self, table: &mut InlineTable) {
        table.set_trailing_comma(false);
        // `inline_fits` already ruled out a comment here, so whatever the author
        // left between the last value and the brace is dead whitespace that
        // would otherwise survive as a second space.
        table.set_trailing("");
        table.fmt();
        if self.style.inline_table_spacing == Spacing::Compact {
            table.set_trailing("");
            trim_leading_space(table);
            trim_trailing_space(table);
        }
    }

    /// `foo = { workspace = true }` becomes `foo.workspace = true`, keeping any
    /// trailing comment that sat on either the inner value or the table itself.
    fn collapse_to_dotted(&self, table: &mut InlineTable) {
        let suffix = {
            let outer = raw_to_str(table.decor().suffix(), self.source);
            merged_comment_suffix(&self.below_last_value(table), &outer)
        };

        table.set_dotted(true);
        table.set_trailing_comma(false);
        table.set_trailing("");
        table.fmt();

        self.hand_suffix_to_dotted_leaf(table, &suffix);
    }

    /// The mirror of [`Self::take_over_dotted_leaf`] for trailing comments: only
    /// the innermost value of a collapsed chain still renders, so a suffix left
    /// on an intermediate table would be dropped.
    fn hand_suffix_to_dotted_leaf(&self, table: &mut InlineTable, suffix: &str) {
        let Some((_, value)) = table.iter_mut().next() else {
            return;
        };

        match value.as_inline_table_mut() {
            Some(child) if child.is_dotted() => self.hand_suffix_to_dotted_leaf(child, suffix),
            _ => set_suffix(value.decor_mut(), self.source, suffix),
        }
    }

    fn inline_decors(&self, table: &mut InlineTable) -> Vec<(String, String)> {
        let source = self.source;
        let mut decors: Vec<(String, String)> = table
            .iter_mut()
            .map(|(key, value)| {
                let mut prefix = raw_to_str(key.leaf_decor().prefix(), source).into_owned();
                // A comment between the key and its `=`, or between `=` and the
                // value, has nowhere else to go once the entry is rewritten as
                // `key = value`: both spans are overwritten. Each is lifted onto
                // its own line above the key, in source order, and the leading
                // newline is what makes it land there rather than trailing
                // whatever came before.
                for between in [
                    raw_to_str(key.leaf_decor().suffix(), source),
                    raw_to_str(value.decor().prefix(), source),
                ] {
                    if !between.contains('#') {
                        continue;
                    }
                    prefix.truncate(prefix.trim_end_matches([' ', '\t']).len());
                    if !prefix.ends_with(['\n', '\r']) {
                        prefix.push('\n');
                    }
                    prefix.push_str(&between);
                }
                (
                    prefix,
                    raw_to_str(value.decor().suffix(), source).into_owned(),
                )
            })
            .collect();
        shift_comments_past_commas(&mut decors);
        decors
    }

    fn expand_over_lines(
        &self,
        table: &mut InlineTable,
        indent: usize,
        comma: bool,
        decors: &[(String, String)],
        rest: &str,
    ) {
        let source = self.source;
        table.set_dotted(false);
        table.set_trailing_comma(comma);

        for ((mut key, value), (prefix, suffix)) in table.iter_mut().zip(decors) {
            let prefix = self.wrap_prefix(prefix, indent + 1);
            set_prefix(key.leaf_decor_mut(), source, &prefix);
            set_suffix(key.leaf_decor_mut(), source, " ");
            key.dotted_decor_mut().clear();
            set_prefix(value.decor_mut(), source, " ");
            set_suffix(value.decor_mut(), source, &same_line_comment_suffix(suffix));
        }

        let trailing = self.closing_block(rest, indent);
        set_inline_trailing(table, source, &trailing);
    }

    fn array(&self, array: &mut Array, ctx: Ctx) {
        if self.array_wraps(array, ctx) {
            let level = ctx.indent + 1;
            let column = self.style.indent.width(level);
            let comma = self.style.trailing_comma == TrailingComma::Multiline && !array.is_empty();
            let mut decors = self.array_decors(array);
            let trailing = raw_to_str(Some(array.trailing()), self.source).into_owned();
            let rest = split_container_trailing(&mut decors, &trailing, comma);
            let comments = self.same_line_comment_widths(&decors, &rest);
            let last = array.len().saturating_sub(1);
            for (index, value) in array.iter_mut().enumerate() {
                let reserved = usize::from(index < last || comma) + comments[index];
                self.value(
                    value,
                    Ctx {
                        indent: level,
                        column,
                        reserved,
                        dotted_ok: false,
                        force_inline: false,
                    },
                );
            }
            self.wrap_array(array, ctx.indent, comma, &decors, &rest);
        } else {
            let child = Ctx {
                indent: ctx.indent,
                column: 0,
                reserved: 0,
                dotted_ok: false,
                force_inline: true,
            };
            for value in array.iter_mut() {
                self.value(value, child);
            }
            self.inline_array(array);
        }
    }

    fn array_wraps(&self, array: &Array, ctx: Ctx) -> bool {
        if array_body_has_comment(array, self.source) {
            return true;
        }
        if ctx.force_inline {
            return false;
        }
        if self.prefers_break_array(array) {
            return true;
        }
        match self.array_end(array, ctx.column) {
            Some(end) => end + ctx.reserved > self.style.max_width,
            None => true,
        }
    }

    /// Style choices that break a container regardless of how narrow it is.
    fn prefers_break_value(&self, value: &Value) -> bool {
        match value {
            Value::Array(array) => self.prefers_break_array(array),
            Value::InlineTable(table) => self.prefers_break_inline(table),
            _ => false,
        }
    }

    fn prefers_break_array(&self, array: &Array) -> bool {
        if array.is_empty() {
            return false;
        }
        match self.style.arrays {
            ArrayStyle::Preserve if array_is_multiline(array, self.source) => return true,
            ArrayStyle::Expand if array.iter().take(2).count() == 2 => return true,
            _ => {}
        }
        array.iter().any(|value| self.prefers_break_value(value))
    }

    fn prefers_break_inline(&self, table: &InlineTable) -> bool {
        (self.inline_table_style() == InlineTableStyle::Expand && table.iter().take(2).count() == 2)
            || table
                .iter()
                .any(|(_, value)| self.prefers_break_value(value))
    }

    fn array_decors(&self, array: &Array) -> Vec<(String, String)> {
        let source = self.source;
        let mut decors: Vec<(String, String)> = array
            .iter()
            .map(|value| {
                (
                    raw_to_str(value.decor().prefix(), source).into_owned(),
                    raw_to_str(value.decor().suffix(), source).into_owned(),
                )
            })
            .collect();
        shift_comments_past_commas(&mut decors);
        decors
    }

    fn wrap_array(
        &self,
        array: &mut Array,
        indent: usize,
        comma: bool,
        decors: &[(String, String)],
        rest: &str,
    ) {
        let source = self.source;
        array.set_trailing_comma(comma);

        for (value, (prefix, suffix)) in array.iter_mut().zip(decors) {
            let prefix = self.wrap_prefix(prefix, indent + 1);
            set_prefix(value.decor_mut(), source, &prefix);
            set_suffix(value.decor_mut(), source, &same_line_comment_suffix(suffix));
        }

        let trailing = self.closing_block(rest, indent);
        set_array_trailing(array, source, &trailing);
    }

    fn inline_array(&self, array: &mut Array) {
        let pad = if self.style.array_spacing == Spacing::Spaced && !array.is_empty() {
            " "
        } else {
            ""
        };
        array.set_trailing_comma(false);
        array.set_trailing(pad);

        for (index, value) in array.iter_mut().enumerate() {
            let suffix = same_line_comment_suffix(&raw_to_str(value.decor().suffix(), self.source));
            value
                .decor_mut()
                .set_prefix(if index == 0 { pad } else { " " });
            value.decor_mut().set_suffix(suffix);
        }
    }

    fn key_decor(&self, key: &mut KeyMut<'_>, prefix: &str) {
        let source = self.source;
        set_prefix(key.leaf_decor_mut(), source, prefix);
        set_suffix(key.leaf_decor_mut(), source, " ");
        key.dotted_decor_mut().clear();
    }

    /// A header path renders from its own decors — the leaf's `leaf_decor` pads
    /// the inside of the brackets and every `dotted_decor` pads a dot — while
    /// the comment block above the header lives on the table. Neither slot can
    /// hold anything but whitespace, so `[  a  .  b  ]` normalizes by clearing.
    fn header_key_decor(key: &mut KeyMut<'_>) {
        key.leaf_decor_mut().clear();
        key.dotted_decor_mut().clear();
    }

    /// `toml_edit` keeps a dotted line's leading comment on the *last* key of
    /// the path, so collapsing `a = { b = 1 }` into `a.b = 1` demotes `a` out of
    /// the slot the comment was rendered from. Hand the prefix down to whichever
    /// key ends up as the leaf, or it is silently dropped.
    fn take_over_dotted_leaf(&self, table: &mut InlineTable, prefix: &str) {
        let Some((mut key, value)) = table.iter_mut().next() else {
            return;
        };

        match value.as_inline_table_mut() {
            Some(child) if child.is_dotted() => {
                self.take_over_dotted_leaf(child, prefix);
                key.dotted_decor_mut().clear();
            }
            _ => set_prefix(key.leaf_decor_mut(), self.source, prefix),
        }
    }

    fn leaf_decor(&self, value: &mut Value) {
        let source = self.source;
        let suffix = same_line_comment_suffix(&raw_to_str(value.decor().suffix(), source));
        set_prefix(value.decor_mut(), source, " ");
        set_suffix(value.decor_mut(), source, &suffix);
    }

    /// Whitespace before a closing bracket or brace. Anything on the closing
    /// run's first line stays on the last value's line, which is where a
    /// trailing comma leaves a comment that could not sit before it.
    fn closing_block(&self, rest: &str, indent: usize) -> Cow<'a, str> {
        let split = rest.find(['\n', '\r']).unwrap_or(rest.len());
        let head = same_line_comment_suffix(&rest[..split]);
        let tail = self.trailing_block(&rest[split..], indent);
        if head.is_empty() {
            return tail;
        }

        let mut out = head;
        out.push_str(&tail);
        Cow::Owned(out)
    }

    fn trailing_block(&self, old: &str, indent: usize) -> Cow<'a, str> {
        self.rebuild_comments(old, CommentBlock::closing(indent))
    }

    fn wrap_prefix(&self, old: &str, indent: usize) -> Cow<'a, str> {
        let block = CommentBlock::wrapped(indent);
        if old.starts_with(['\n', '\r']) || !old.contains('#') {
            return self.rebuild_comments(old, block);
        }
        let first_line_end = old.find(['\n', '\r']).unwrap_or(old.len());
        let first_line = &old[..first_line_end];
        if !first_line.contains('#') {
            return self.rebuild_comments(old, block);
        }

        let rest = self.rebuild_comments(&old[first_line_end..], block);
        let mut out = same_line_comment_suffix(first_line);
        out.push_str(&rest);
        Cow::Owned(out)
    }

    /// Rewrite a decor prefix down to its comment lines, dropping stray whitespace
    /// but keeping up to `max_blank_lines` blank lines between them.
    fn rebuild_comments(&self, old: &str, block: CommentBlock) -> Cow<'a, str> {
        let style = self.style;
        if !old.contains('#') {
            if block.open_newline {
                return style.indent.newline(block.close);
            }
            let blanks = normalize_newlines(old)
                .matches('\n')
                .count()
                .min(style.max_blank_lines);
            if blanks == 0 {
                return style.indent.text(block.close);
            }
            let closing = style.indent.text(block.close);
            let mut out = String::with_capacity(blanks + closing.len());
            out.extend(std::iter::repeat_n('\n', blanks));
            out.push_str(&closing);
            return Cow::Owned(out);
        }

        let text = normalize_newlines(old);
        // Only the segments strictly between the previous line and the one this
        // block ends on are lines of its own: inside a wrapped container the
        // first closes the line the opening bracket sits on, and the last is the
        // leading whitespace of whatever follows — unless it is a comment with no
        // newline after it, which is a file's last line.
        let mut segments = text.split('\n');
        if block.open_newline {
            segments.next();
        }
        let mut lines: Vec<&str> = segments.collect();
        if lines
            .last()
            .is_some_and(|line| !line.trim_start().starts_with('#'))
        {
            lines.pop();
        }

        let indent = style.indent.text(block.indent);
        let mut out = String::with_capacity(text.len() + indent.len() + 1);
        if block.open_newline {
            out.push('\n');
        }

        // A container never opens on a blank line, so blanks only survive above
        // the first comment when the block starts flush against a table body.
        let mut wrote = !block.open_newline;
        let mut blanks = 0;
        for line in lines {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix('#') {
                if wrote {
                    out.extend(std::iter::repeat_n('\n', blanks.min(style.max_blank_lines)));
                }
                blanks = 0;
                wrote = true;
                out.push_str(&indent);
                push_hash_comment(&mut out, rest);
                out.push('\n');
            } else if trimmed.is_empty() {
                blanks += 1;
            }
        }
        if wrote {
            out.extend(std::iter::repeat_n('\n', blanks.min(style.max_blank_lines)));
        }

        out.push_str(&style.indent.text(block.close));
        Cow::Owned(out)
    }

    fn text_end(&self, start: usize, text: &str) -> usize {
        toml_width::advance(start, text, self.style.tab_width())
    }

    fn key_end(&self, start: usize, key: &KeyMut<'_>) -> usize {
        self.text_end(start, &key.display_repr())
    }

    /// Columns a same-line comment adds to the line its value ends on.
    fn comment_width(&self, raw: &str) -> usize {
        self.text_end(0, &same_line_comment_suffix(raw))
    }

    /// Columns each element of a wrapped container owes to the comment that
    /// ends up on its line.
    ///
    /// Once [`shift_comments_past_commas`] and [`split_container_trailing`] have
    /// run, that comment is the element's own suffix, or the first line of what
    /// follows the comma — the next element's prefix, or the closing run for the
    /// last one. A container's trailing comment reaches the last element this
    /// way, which is why the budget cannot read the decors as the author left
    /// them.
    fn same_line_comment_widths(&self, decors: &[(String, String)], rest: &str) -> Vec<usize> {
        (0..decors.len())
            .map(|index| match self.comment_width(&decors[index].1) {
                0 => match decors.get(index + 1) {
                    Some((prefix, _)) => self.comment_width(prefix),
                    None => self.comment_width(rest),
                },
                width => width,
            })
            .collect()
    }

    /// Column the canonical one-line rendering of `value` ends at, or `None`
    /// when a value's own text spans lines and no column count describes it.
    fn value_end(&self, value: &Value, start: usize) -> Option<usize> {
        match value {
            Value::Array(array) => self.array_end(array, start),
            Value::InlineTable(table) => self.inline_end(table, start),
            _ => self.scalar_end(value, start),
        }
    }

    fn array_end(&self, array: &Array, start: usize) -> Option<usize> {
        let pad = usize::from(self.style.array_spacing == Spacing::Spaced && !array.is_empty());
        let mut column = start + 1 + pad;
        for (index, value) in array.iter().enumerate() {
            if index > 0 {
                column += 2;
            }
            column = self.value_end(value, column)?;
        }
        Some(column + pad + 1)
    }

    fn inline_end(&self, table: &InlineTable, start: usize) -> Option<usize> {
        let values = table.get_values();
        if values.is_empty() {
            return Some(start + 2);
        }

        let pad = usize::from(self.style.inline_table_spacing == Spacing::Spaced);
        let mut column = start + 1 + pad;
        for (index, (path, value)) in values.iter().enumerate() {
            if index > 0 {
                column += 2;
            }
            for (segment, key) in path.iter().enumerate() {
                if segment > 0 {
                    column += 1;
                }
                column = self.text_end(column, &key.display_repr());
            }
            column = self.value_end(value, column + 3)?;
        }
        Some(column + pad + 1)
    }

    fn scalar_end(&self, value: &Value, start: usize) -> Option<usize> {
        let mut width = InlineWidth::new(start, self.style.tab_width());
        let _ = write!(width, "{value}");
        width.get()
    }

    fn record_frozen(&self, scope: Scope<'_>, crate_key: &str, value: &Value) {
        if self.lookup.is_none() {
            return;
        }
        let facts = value_dep_facts(crate_key, value);
        if facts.is_reportable() {
            self.record(
                scope,
                crate_key,
                facts.name,
                facts.req.unwrap_or_default(),
                Resolution::Skipped(SkipReason::Frozen),
            );
        }
    }

    fn pin_dep(&self, crate_key: &str, value: &mut Value, scope: Scope<'_>) {
        let Some(lookup) = self.lookup else {
            return;
        };

        let outcome = {
            let facts = value_dep_facts(crate_key, value);
            if !facts.is_reportable() {
                return;
            }
            let outcome = self.decide(lookup, scope, &facts);
            self.record(
                scope,
                crate_key,
                facts.name,
                facts.req.unwrap_or_default(),
                outcome.clone(),
            );
            outcome
        };

        let Resolution::Pinned(full) = outcome else {
            return;
        };
        if value.as_str().is_some() {
            self.replace_version(value, full);
            return;
        }
        if let Some(version) = value
            .as_inline_table_mut()
            .and_then(|table| table.get_mut("version"))
        {
            self.replace_version(version, full);
        }
    }

    fn pin_dep_table(
        &self,
        crate_key: &str,
        table: &mut Table,
        scope: Scope<'_>,
        guard: Guard<'_>,
    ) {
        let Some(lookup) = self.lookup else {
            return;
        };

        let outcome = {
            let facts = dep_facts(crate_key, table);
            if !facts.is_reportable() {
                return;
            }
            let outcome = if guard.key_frozen("version") {
                Resolution::Skipped(SkipReason::Frozen)
            } else {
                self.decide(lookup, scope, &facts)
            };
            self.record(
                scope,
                crate_key,
                facts.name,
                facts.req.unwrap_or_default(),
                outcome.clone(),
            );
            outcome
        };

        let Resolution::Pinned(full) = outcome else {
            return;
        };
        if let Some(Item::Value(version)) = table.get_mut("version") {
            self.replace_version(version, full);
        }
    }

    fn decide(
        &self,
        lookup: &dyn VersionLookup,
        scope: Scope<'_>,
        facts: &DepFacts<'_>,
    ) -> Resolution {
        // A `[patch]` entry is skipped for being a patch whatever source it
        // names, so the section outranks the entry's own source marker.
        if let Some(reason) = scope.kind.skip().or_else(|| facts.skip.clone()) {
            return Resolution::Skipped(reason);
        }
        let Some(req) = facts.req else {
            return Resolution::Unchanged;
        };
        match lookup.resolve(
            DepRequest {
                name: facts.name,
                req,
            },
            self.rust_version,
        ) {
            Resolution::Pinned(full) if full == req => Resolution::Unchanged,
            other => other,
        }
    }

    fn replace_version(&self, value: &mut Value, full: String) {
        let suffix = raw_to_str(value.decor().suffix(), self.source).into_owned();
        *value = Value::from(full);
        if !suffix.is_empty() {
            value.decor_mut().set_suffix(suffix);
        }
    }
}

/// Traversal index of the header `toml_edit` writes first, or `None` when the
/// root's own body already opens the document. The encoder orders headers by
/// `(position, traversal order)`, so this pass mirrors that and the formatting
/// walk counts the same way.
fn leading_header(root: &Table) -> Option<usize> {
    if !root.get_values().is_empty() {
        return None;
    }

    let mut walk = HeaderWalk::default();
    let mut headers = Vec::new();
    scan_headers(root, true, false, &mut walk, &mut headers);
    headers.into_iter().min().map(|(_, index)| index)
}

#[derive(Default)]
struct HeaderWalk {
    index: usize,
    last_position: isize,
}

fn scan_headers(
    table: &Table,
    root: bool,
    array_of_tables: bool,
    walk: &mut HeaderWalk,
    out: &mut Vec<(isize, usize)>,
) {
    if !root && !table.is_dotted() {
        if let Some(position) = table.position() {
            walk.last_position = position;
        }
        if renders_header(table, array_of_tables) {
            out.push((walk.last_position, walk.index));
        }
        walk.index += 1;
    }

    for (_, item) in table {
        match item {
            Item::Table(child) => scan_headers(child, false, false, walk, out),
            Item::ArrayOfTables(children) => {
                for child in children {
                    scan_headers(child, false, true, walk, out);
                }
            }
            _ => {}
        }
    }
}

/// One step of a path to a table: a key, or an index into an array of tables.
#[derive(Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
}

/// Where a promoted table goes: after the whole block of the sibling that
/// precedes it in the table it was written in, or, when it has no such sibling,
/// directly after the header of that table.
struct Anchor {
    path: Vec<Step>,
    whole_subtree: bool,
}

/// The line a promoted table has to follow if the output is to survive being
/// read back: re-parsing puts headers in the order they were written, so the new
/// header belongs where its key already sat among its siblings.
fn promotion_anchor(table: &Table, path: &[Step], name: &str, promoting: &[String]) -> Anchor {
    let mut anchor = Anchor {
        path: path.to_vec(),
        whole_subtree: false,
    };

    for (sibling, item) in table {
        if sibling == name {
            break;
        }
        // Every non-dotted table either renders a header or exists because a
        // descendant does, so it always contributes a line to follow — and a
        // sibling this same pass is about to promote will contribute one too.
        let heads_a_block = match item {
            Item::Table(child) => !child.is_dotted(),
            Item::ArrayOfTables(children) => !children.is_empty(),
            _ => promoting.iter().any(|promoted| promoted == sibling),
        };
        if heads_a_block {
            let mut sibling_path = path.to_vec();
            sibling_path.push(Step::Key(sibling.to_owned()));
            anchor = Anchor {
                path: sibling_path,
                whole_subtree: true,
            };
        }
    }

    anchor
}

fn restore_order(table: &mut Table, order: &[String]) {
    let mut taken = Vec::with_capacity(order.len());
    for name in order {
        if let Some(entry) = table.remove_entry(name) {
            taken.push(entry);
        }
    }
    for (key, item) in taken {
        table.insert_formatted(&key, item);
    }
}

/// Gives every header a fresh dense position, keeping the order the document
/// already renders in and slotting each promoted table in at its anchor.
///
/// Existing positions are never reinterpreted: a document that writes `[a.b]`
/// before `[a]` keeps that sequence, because the list starts out sorted by the
/// positions the parser handed out.
fn renumber_positions(root: &mut Table, promoted: &[(Anchor, Vec<Step>)]) {
    let mut positioned = Vec::new();
    collect_positioned(root, &mut Vec::new(), &mut positioned);
    positioned.sort_by_key(|(position, _)| *position);

    let mut order: Vec<Vec<Step>> = positioned.into_iter().map(|(_, path)| path).collect();
    for (anchor, child) in promoted {
        let at = if anchor.whole_subtree {
            order
                .iter()
                .rposition(|path| path.starts_with(anchor.path.as_slice()))
        } else {
            order.iter().position(|path| *path == anchor.path)
        };
        let at = at.map_or(order.len(), |index| index + 1);
        order.insert(at, child.clone());
    }

    for (position, path) in order.iter().enumerate() {
        if let Some(table) = table_at_mut(root, path) {
            table.set_position(Some(isize::try_from(position).unwrap_or(isize::MAX)));
        }
    }
}

fn collect_positioned(table: &Table, path: &mut Vec<Step>, out: &mut Vec<(isize, Vec<Step>)>) {
    for (name, item) in table {
        match item {
            Item::Table(child) if !child.is_dotted() => {
                path.push(Step::Key(name.to_owned()));
                if let Some(position) = child.position() {
                    out.push((position, path.clone()));
                }
                collect_positioned(child, path, out);
                path.pop();
            }
            Item::ArrayOfTables(children) => {
                path.push(Step::Key(name.to_owned()));
                for (index, child) in children.iter().enumerate() {
                    path.push(Step::Index(index));
                    if let Some(position) = child.position() {
                        out.push((position, path.clone()));
                    }
                    collect_positioned(child, path, out);
                    path.pop();
                }
                path.pop();
            }
            _ => {}
        }
    }
}

fn table_at_mut<'t>(root: &'t mut Table, path: &[Step]) -> Option<&'t mut Table> {
    let mut table = root;
    let mut steps = path.iter();
    while let Some(step) = steps.next() {
        let Step::Key(name) = step else {
            return None;
        };
        match table.get_mut(name)? {
            Item::Table(child) => table = child,
            Item::ArrayOfTables(children) => {
                let Some(Step::Index(index)) = steps.next() else {
                    return None;
                };
                table = children.get_mut(*index)?;
            }
            _ => return None,
        }
    }
    Some(table)
}

/// An implicit table with no body of its own writes no header; an array of
/// tables always does.
fn renders_header(table: &Table, array_of_tables: bool) -> bool {
    array_of_tables || !(table.is_implicit() && table.get_values().is_empty())
}

fn normalize_table_keys(table: &mut Table, source: &str, guard: Guard<'_>) {
    for (mut key, item) in table.iter_mut() {
        let frozen_key = guard.key_frozen(key.get());
        if !frozen_key {
            normalize_key(&mut key, source);
        }
        match item {
            Item::Value(value) => {
                if !frozen_key {
                    normalize_value_keys(value, source);
                }
            }
            Item::Table(child) => {
                normalize_table_keys(child, source, guard.child(key.get(), 0));
            }
            Item::ArrayOfTables(children) => {
                for (index, child) in children.iter_mut().enumerate() {
                    normalize_table_keys(child, source, guard.child(key.get(), index));
                }
            }
            Item::None => {}
        }
    }
}

fn normalize_value_keys(value: &mut Value, source: &str) {
    match value {
        Value::InlineTable(table) => {
            for (mut key, value) in table.iter_mut() {
                normalize_key(&mut key, source);
                normalize_value_keys(value, source);
            }
        }
        Value::Array(array) => {
            for value in array.iter_mut() {
                normalize_value_keys(value, source);
            }
        }
        _ => {}
    }
}

/// Drops the quotes from a key the spec allows bare. `Key::fmt` is the only way
/// to reset a repr and it clears both decors on the way, so they are captured
/// and put back; an unset decor stays unset, which is not the same as empty.
fn normalize_key(key: &mut KeyMut<'_>, source: &str) {
    let bare = key.get().to_owned();
    if key.display_repr() == bare {
        return;
    }
    if key.default_repr().as_raw().as_str() != Some(bare.as_str()) {
        return;
    }

    let leaf = capture_decor(key.leaf_decor(), source);
    let dotted = capture_decor(key.dotted_decor(), source);
    key.fmt();
    restore_decor(key.leaf_decor_mut(), leaf);
    restore_decor(key.dotted_decor_mut(), dotted);
}

type CapturedDecor = (Option<String>, Option<String>);

fn capture_decor(decor: &Decor, source: &str) -> CapturedDecor {
    (
        decor
            .prefix()
            .map(|raw| raw_to_str(Some(raw), source).into_owned()),
        decor
            .suffix()
            .map(|raw| raw_to_str(Some(raw), source).into_owned()),
    )
}

fn restore_decor(decor: &mut Decor, (prefix, suffix): CapturedDecor) {
    if let Some(prefix) = prefix {
        decor.set_prefix(prefix);
    }
    if let Some(suffix) = suffix {
        decor.set_suffix(suffix);
    }
}

/// Where in a manifest the walk currently is, which is what tells a dependency
/// entry apart from an ordinary table and names it in a report.
#[derive(Clone, Copy)]
struct Scope<'s> {
    path: &'s str,
    kind: ScopeKind,
    depth: usize,
}

impl Scope<'_> {
    const ROOT: Self = Self {
        path: "",
        kind: ScopeKind::Other,
        depth: 0,
    };

    fn child<'c>(self, path: &'c str, key: &str) -> Scope<'c> {
        Scope {
            path,
            kind: self.kind.child(key, self.depth == 0),
            depth: self.depth + 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Other,
    Deps,
    /// `[patch]` itself; its children are the per-registry patch tables.
    PatchRoot,
    Patch,
    Replace,
}

impl ScopeKind {
    fn child(self, key: &str, at_root: bool) -> Self {
        match self {
            Self::PatchRoot => Self::Patch,
            Self::Other if is_deps_section(key) => Self::Deps,
            Self::Other if at_root && key == "patch" => Self::PatchRoot,
            Self::Other if at_root && key == "replace" => Self::Replace,
            // A dependency section's children are entries, not sections, and
            // nothing below one is a section either.
            _ => Self::Other,
        }
    }

    fn holds_deps(self) -> bool {
        matches!(self, Self::Deps | Self::Patch | Self::Replace)
    }

    /// `[patch]` and `[replace]` entries are read only to report them: a patch
    /// constrains the source it redirects to, which is not the one this tool
    /// resolves against.
    fn skip(self) -> Option<SkipReason> {
        match self {
            Self::Patch => Some(SkipReason::PatchSection),
            Self::Replace => Some(SkipReason::ReplaceSection),
            _ => None,
        }
    }
}

/// One dependency entry as the version path sees it.
struct DepFacts<'a> {
    name: &'a str,
    req: Option<&'a str>,
    skip: Option<SkipReason>,
}

impl DepFacts<'_> {
    /// A table with neither a requirement nor a source is not a dependency this
    /// tool has anything to say about.
    fn is_reportable(&self) -> bool {
        self.req.is_some() || self.skip.is_some()
    }
}

fn dep_facts<'a>(crate_key: &'a str, table: &'a impl TableLike) -> DepFacts<'a> {
    let skip = if table.contains_key("path") {
        Some(SkipReason::PathSource)
    } else if table.contains_key("git") {
        Some(SkipReason::GitSource)
    } else if table.contains_key("registry") || table.contains_key("registry-index") {
        Some(SkipReason::AlternateRegistry)
    } else if table.get("workspace").and_then(Item::as_bool) == Some(true) {
        Some(SkipReason::WorkspaceInherited)
    } else {
        None
    };

    DepFacts {
        name: table
            .get("package")
            .and_then(Item::as_str)
            .unwrap_or(crate_key),
        req: table.get("version").and_then(Item::as_str),
        skip,
    }
}

fn value_dep_facts<'a>(crate_key: &'a str, value: &'a Value) -> DepFacts<'a> {
    match value {
        Value::String(_) => DepFacts {
            name: crate_key,
            req: value.as_str(),
            skip: None,
        },
        Value::InlineTable(table) => dep_facts(crate_key, table),
        _ => DepFacts {
            name: crate_key,
            req: None,
            skip: None,
        },
    }
}

fn collect_dep_requests<'a>(
    table: &'a Table,
    scope: Scope<'_>,
    guard: Guard<'_>,
    out: &mut Vec<DepRequest<'a>>,
) {
    for (key, item) in table {
        if guard.key_frozen(key) {
            continue;
        }
        let child_guard = guard.child(key, 0);
        match item {
            Item::Value(value) if scope.kind == ScopeKind::Deps => {
                push_request(&value_dep_facts(key, value), out);
            }
            Item::Table(child) => {
                if scope.kind == ScopeKind::Deps && !child_guard.key_frozen("version") {
                    push_request(&dep_facts(key, child), out);
                }
                collect_dep_requests(child, scope.child("", key), child_guard, out);
            }
            Item::ArrayOfTables(children) => {
                let child_scope = scope.child("", key);
                for (index, child) in children.iter().enumerate() {
                    collect_dep_requests(child, child_scope, guard.child(key, index), out);
                }
            }
            _ => {}
        }
    }
}

fn push_request<'a>(facts: &DepFacts<'a>, out: &mut Vec<DepRequest<'a>>) {
    if facts.skip.is_some() {
        return;
    }
    if let Some(req) = facts.req {
        out.push(DepRequest {
            name: facts.name,
            req,
        });
    }
}

fn span_key(section: &str, crate_key: &str) -> String {
    format!("{section}\0{crate_key}")
}

/// Byte offsets of every dependency key, taken while the parse still carries
/// spans so a check-mode finding can name a line.
fn dep_spans(table: &Table) -> AHashMap<String, usize> {
    let mut out = AHashMap::default();
    collect_dep_spans(table, Scope::ROOT, &mut out);
    out
}

fn collect_dep_spans(table: &Table, scope: Scope<'_>, out: &mut AHashMap<String, usize>) {
    for (key, item) in table {
        if scope.kind.holds_deps()
            && let Some(start) = table.key(key).and_then(Key::span).map(|span| span.start)
        {
            out.insert(span_key(scope.path, key), start);
        }
        match item {
            Item::Table(child) => {
                let path = child_path(scope.path, key);
                collect_dep_spans(child, scope.child(&path, key), out);
            }
            Item::ArrayOfTables(children) => {
                let path = child_path(scope.path, key);
                let child_scope = scope.child(&path, key);
                for child in children {
                    collect_dep_spans(child, child_scope, out);
                }
            }
            _ => {}
        }
    }
}

fn child_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_owned()
    } else {
        format!("{parent}.{key}")
    }
}

/// The compiler floor a pin must not raise: the manifest's own `rust-version`,
/// the workspace value it inherits, or none.
fn manifest_rust_version(table: &Table, context: &ManifestContext) -> Option<PartialVersion> {
    let workspace = table
        .get("workspace")
        .and_then(Item::as_table_like)
        .and_then(|workspace| workspace.get("package"))
        .and_then(Item::as_table_like)
        .and_then(|package| package.get("rust-version"))
        .and_then(Item::as_str)
        .and_then(PartialVersion::parse)
        .or(context.workspace_rust_version);

    let Some(package) = table.get("package").and_then(Item::as_table_like) else {
        return workspace;
    };
    let Some(declared) = package.get("rust-version") else {
        return workspace;
    };
    if let Some(literal) = declared.as_str() {
        return PartialVersion::parse(literal);
    }
    if declared
        .as_table_like()
        .and_then(|inherit| inherit.get("workspace"))
        .and_then(Item::as_bool)
        == Some(true)
    {
        return workspace;
    }
    None
}

pub(crate) fn is_deps_section(name: &str) -> bool {
    matches!(
        name,
        "dependencies" | "dev-dependencies" | "build-dependencies"
    )
}

fn array_is_multiline(array: &Array, source: &str) -> bool {
    if raw_to_str(Some(array.trailing()), source).contains(['\n', '\r']) {
        return true;
    }
    array.iter().any(|value| {
        raw_to_str(value.decor().prefix(), source).contains(['\n', '\r'])
            || raw_to_str(value.decor().suffix(), source).contains(['\n', '\r'])
    })
}

fn value_has_comment(value: &Value, source: &str) -> bool {
    decor_has_comment(value.decor(), source)
        || match value {
            Value::Array(array) => array_body_has_comment(array, source),
            Value::InlineTable(table) => inline_body_has_comment(table, source),
            _ => false,
        }
}

pub(crate) fn array_body_has_comment(array: &Array, source: &str) -> bool {
    raw_to_str(Some(array.trailing()), source).contains('#')
        || array.iter().any(|value| value_has_comment(value, source))
}

fn inline_body_has_comment(table: &InlineTable, source: &str) -> bool {
    raw_to_str(Some(table.trailing()), source).contains('#')
        || table
            .get_values()
            .into_iter()
            .any(|(path, value)| key_has_comment(&path, source) || value_has_comment(value, source))
}

/// Columns of a rendering with its surrounding decor whitespace trimmed away,
/// tracked as an absolute column so a tab reaches the right stop.
///
/// A newline inside the trimmed span belongs to the value itself, and no column
/// count describes it.
struct InlineWidth {
    tab_width: usize,
    start: usize,
    /// Column just after the last non-whitespace character.
    end: usize,
    /// Column the next character lands on, whitespace included.
    cursor: usize,
    started: bool,
    pending_newline: bool,
    interior_newline: bool,
}

impl InlineWidth {
    fn new(start: usize, tab_width: usize) -> Self {
        Self {
            tab_width,
            start,
            end: start,
            cursor: start,
            started: false,
            pending_newline: false,
            interior_newline: false,
        }
    }

    fn get(&self) -> Option<usize> {
        (!self.interior_newline).then_some(self.end)
    }
}

impl std::fmt::Write for InlineWidth {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        for ch in s.chars() {
            if ch.is_whitespace() {
                if self.started {
                    self.cursor = toml_width::advance_char(self.cursor, ch, self.tab_width);
                    self.pending_newline |= ch == '\n' || ch == '\r';
                }
                continue;
            }
            if self.started {
                self.interior_newline |= self.pending_newline;
            } else {
                self.cursor = self.start;
                self.started = true;
            }
            self.pending_newline = false;
            self.cursor = toml_width::advance_char(self.cursor, ch, self.tab_width);
            self.end = self.cursor;
        }
        Ok(())
    }
}

/// Clears the space a one-line inline table opens with, following a dotted key
/// down to the segment that actually renders it.
fn trim_leading_space(table: &mut InlineTable) {
    let Some((mut key, value)) = table.iter_mut().next() else {
        return;
    };
    key.leaf_decor_mut().set_prefix("");
    key.dotted_decor_mut().set_prefix("");
    if let Some(child) = value.as_inline_table_mut()
        && child.is_dotted()
    {
        trim_leading_space(child);
    }
}

/// The space before the closing brace, which `InlineTable::fmt` leaves on the
/// last value rather than on the table's own trailing run.
fn trim_trailing_space(table: &mut InlineTable) {
    let Some((_, value)) = table.iter_mut().last() else {
        return;
    };
    match value.as_inline_table_mut() {
        Some(child) if child.is_dotted() => trim_trailing_space(child),
        _ => value.decor_mut().set_suffix(""),
    }
}

/// `toml_edit` writes a value's suffix before the comma that separates it from the
/// next one, so a same-line comment left there would swallow the comma. Carry it to
/// the following prefix, which is rendered after that comma.
fn shift_comments_past_commas(decors: &mut [(String, String)]) {
    for index in 1..decors.len() {
        if !decors[index - 1].1.contains('#') {
            continue;
        }
        let carried = std::mem::take(&mut decors[index - 1].1);
        decors[index].0.insert_str(0, &carried);
    }
}

/// Splits what belongs to the last value from what belongs to the closing
/// bracket. Without a trailing comma the two runs are joined, so the last
/// value's line takes the first line of the result; with one, the comma is
/// written after the value's suffix and nothing may stay there.
fn split_container_trailing(
    decors: &mut [(String, String)],
    trailing: &str,
    trailing_comma: bool,
) -> String {
    let Some((_, suffix)) = decors.last_mut() else {
        return trailing.to_owned();
    };

    if trailing_comma {
        let mut rest = std::mem::take(suffix);
        rest.push_str(trailing);
        return rest;
    }

    suffix.push_str(trailing);
    let split = suffix.find(['\n', '\r']).unwrap_or(suffix.len());
    suffix.split_off(split)
}

fn key_has_comment(path: &[&Key], source: &str) -> bool {
    path.iter().any(|key| {
        decor_has_comment(key.leaf_decor(), source) || decor_has_comment(key.dotted_decor(), source)
    })
}

fn decor_has_comment(decor: &Decor, source: &str) -> bool {
    raw_to_str(decor.prefix(), source).contains('#')
        || raw_to_str(decor.suffix(), source).contains('#')
}

pub(crate) fn raw_to_str<'a>(raw: Option<&'a RawString>, source: &'a str) -> Cow<'a, str> {
    let Some(raw) = raw else {
        return Cow::Borrowed("");
    };
    if let Some(text) = raw.as_str() {
        return Cow::Borrowed(text);
    }
    match raw.span() {
        Some(span) => Cow::Borrowed(source.get(span).unwrap_or("")),
        None => Cow::Borrowed(""),
    }
}

/// Decor writes always allocate, so skip the ones that would not change anything.
/// An unset prefix is not the same as an empty one: it renders as the default.
fn set_prefix(decor: &mut Decor, source: &str, new: &str) {
    let unchanged = decor
        .prefix()
        .is_some_and(|raw| raw_to_str(Some(raw), source) == new);
    if !unchanged {
        decor.set_prefix(new);
    }
}

fn set_array_trailing(array: &mut Array, source: &str, new: &str) {
    if raw_to_str(Some(array.trailing()), source) != new {
        array.set_trailing(new);
    }
}

fn set_inline_trailing(table: &mut InlineTable, source: &str, new: &str) {
    if raw_to_str(Some(table.trailing()), source) != new {
        table.set_trailing(new);
    }
}

fn set_suffix(decor: &mut Decor, source: &str, new: &str) {
    let unchanged = decor
        .suffix()
        .is_some_and(|raw| raw_to_str(Some(raw), source) == new);
    if !unchanged {
        decor.set_suffix(new);
    }
}

fn push_hash_comment(out: &mut String, comment: &str) {
    let extra_hashes = comment.bytes().take_while(|&b| b == b'#').count();
    let body = comment.trim_start_matches('#').trim();
    out.push('#');
    for _ in 0..extra_hashes {
        out.push('#');
    }
    if !body.is_empty() {
        out.push(' ');
        out.push_str(body);
    }
}

fn same_line_comment_suffix(text: &str) -> String {
    let first = text.split(['\n', '\r']).next().unwrap_or("");
    let Some(hash) = first.find('#') else {
        return String::new();
    };

    let mut out = String::with_capacity(first.len() - hash + 2);
    out.push(' ');
    push_hash_comment(&mut out, &first[hash + 1..]);
    out
}

fn merged_comment_suffix(inner: &str, outer: &str) -> String {
    let from_inner = same_line_comment_suffix(inner);
    let from_outer = same_line_comment_suffix(outer);
    if from_inner.is_empty() {
        return from_outer;
    }
    if from_outer.is_empty() {
        return from_inner;
    }
    let mut out = from_inner;
    out.push('\n');
    out.push_str(from_outer.trim_start());
    out
}

fn normalize_newlines(text: &str) -> Cow<'_, str> {
    if !text.contains('\r') {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(cr) = rest.find('\r') {
        out.push_str(&rest[..cr]);
        out.push('\n');
        rest = &rest[cr + 1..];
        rest = rest.strip_prefix('\n').unwrap_or(rest);
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toml_style::TomlIndent;

    fn fmt(input: &str) -> String {
        format_toml(input, &TomlStyle::default()).unwrap()
    }

    fn fmt_with(input: &str, style: &TomlStyle) -> String {
        format_toml(input, style).unwrap()
    }

    fn with(apply: impl FnOnce(&mut TomlStyle)) -> TomlStyle {
        let mut style = TomlStyle::default();
        apply(&mut style);
        style
    }

    struct MapLookup(&'static [(&'static str, &'static str, &'static str)]);

    impl VersionLookup for MapLookup {
        fn resolve(&self, request: DepRequest<'_>, _: Option<PartialVersion>) -> Resolution {
            self.0
                .iter()
                .find(|(name, req, _)| *name == request.name && *req == request.req)
                .map_or(
                    Resolution::Skipped(SkipReason::NoMatchingRelease),
                    |(_, _, full)| Resolution::Pinned((*full).to_string()),
                )
        }
    }

    /// The formatted text of a run with a lookup, for tests that only care what
    /// was written.
    fn pin(input: &str, lookup: &dyn VersionLookup) -> String {
        format_toml_with_versions(
            input,
            &TomlStyle::default(),
            lookup,
            &ManifestContext::default(),
        )
        .unwrap()
        .text
    }

    fn pin_records(input: &str, lookup: &dyn VersionLookup) -> Vec<VersionRecord> {
        format_toml_with_versions(
            input,
            &TomlStyle::default(),
            lookup,
            &ManifestContext::default(),
        )
        .unwrap()
        .versions
    }

    // ------------------------------------------------------------ dotted keys

    #[test]
    fn dotted_path() {
        assert_eq!(
            fmt("project = { path = \"crates/foo\" }\n"),
            "project.path = \"crates/foo\"\n"
        );
    }

    #[test]
    fn dotted_workspace() {
        assert_eq!(
            fmt("foo = { workspace = true }\n"),
            "foo.workspace = true\n"
        );
    }

    #[test]
    fn chain_collapse() {
        assert_eq!(fmt("a = { b = { c = 1 } }\n"), "a.b.c = 1\n");
    }

    #[test]
    fn already_dotted() {
        assert_eq!(fmt("project.path = \"x\"\n"), "project.path = \"x\"\n");
    }

    #[test]
    fn single_key_tables_keep_braces_where_a_dotted_key_cannot_go() {
        assert_eq!(fmt("items = [{ a = 1 }]\n"), "items = [{ a = 1 }]\n");
        assert_eq!(
            fmt("x = { a = 1, b = { c = 2 } }\n"),
            "x = { a = 1, b = { c = 2 } }\n"
        );
    }

    #[test]
    fn single_key_table_with_a_comment_below_stays_open() {
        assert_eq!(
            fmt("x = {\n    a = 1\n    # tail\n}\n"),
            "x = {\n    a = 1\n    # tail\n}\n"
        );
        assert_eq!(
            fmt("x = {\n    # note\n    a = 1\n}\n"),
            "x = {\n    # note\n    a = 1\n}\n"
        );
    }

    #[test]
    fn collapse_keeps_both_an_inner_comment_and_one_on_the_brace() {
        assert_eq!(
            fmt("m = { a = 1 # inner\n}# outer\n"),
            "m.a = 1 # inner\n# outer\n"
        );
        assert_eq!(fmt("m = { a = 1 # inner\n}#\n"), "m.a = 1 # inner\n#\n");
        assert_eq!(fmt("m = { a = 1 }# keep\n"), "m.a = 1 # keep\n");
        assert_eq!(
            fmt("m = { a = 1 # inner\n}# outer\n"),
            fmt("m.a = 1 # inner\n# outer\n")
        );
    }

    #[test]
    fn collapsed_table_indents_children_from_its_own_level() {
        let out = fmt("a = { b = [ { c = 1, d = [1,2] } ] }\n");
        assert_eq!(out, "a.b = [{ c = 1, d = [1, 2] }]\n");
        assert_eq!(fmt(&out), out);
    }

    // ------------------------------------------------------- inline tables

    #[test]
    fn compact_inline_table_spacing_drops_both_inner_spaces() {
        let style = with(|style| style.inline_table_spacing = Spacing::Compact);
        assert_eq!(
            fmt_with("a = {x = 1, y = 2}\n", &style),
            "a = {x = 1, y = 2}\n"
        );
        assert_eq!(
            fmt_with("a = { x = 1, y = 2 }\n", &style),
            "a = {x = 1, y = 2}\n"
        );
        assert_eq!(fmt_with("a = {}\n", &style), "a = {}\n");
    }

    #[test]
    fn compact_inline_table_spacing_reaches_a_nested_table() {
        let style = with(|style| style.inline_table_spacing = Spacing::Compact);
        assert_eq!(
            fmt_with("a = { p = { q = 1, r = 2 } }\n", &style),
            "a.p = {q = 1, r = 2}\n"
        );
    }

    #[test]
    fn compact_inline_table_spacing_narrows_the_budget() {
        let style = TomlStyle {
            inline_table_spacing: Spacing::Compact,
            max_width: 17,
            ..TomlStyle::default()
        };
        // `a = {x = 1, y = 2}` is 18 columns and its spaced form is 20, so only
        // the spacing in force may be measured.
        assert_eq!(
            fmt_with("a = {x = 1, y = 2}\n", &style),
            "a = {\n    x = 1,\n    y = 2\n}\n"
        );
        let style = with(|style| {
            style.inline_table_spacing = Spacing::Compact;
            style.max_width = 18;
        });
        assert_eq!(
            fmt_with("a = {x = 1, y = 2}\n", &style),
            "a = {x = 1, y = 2}\n"
        );
    }

    #[test]
    fn a_table_that_fits_stays_on_one_line() {
        assert_eq!(
            fmt("clap = { version = \"4.6.6\", features = [\"derive\"] }\n"),
            "clap = { version = \"4.6.6\", features = [\"derive\"] }\n"
        );
    }

    #[test]
    fn a_table_past_the_width_expands() {
        let style = with(|style| style.max_width = 30);
        assert_eq!(
            fmt_with(
                "clap = { version = \"4.6.6\", features = [\"derive\"] }\n",
                &style
            ),
            "clap = {\n    version = \"4.6.6\",\n    features = [\"derive\"]\n}\n"
        );
    }

    #[test]
    fn compact_never_expands_however_wide() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Compact);
        let long = "clap = { version = \"4.6.6\", features = [\"derive\", \"cargo\", \"env\", \"unicode\", \"wrap_help\"] }\n";
        assert_eq!(fmt_with(long, &style), long);
    }

    #[test]
    fn compact_holds_a_nested_array_inline_too() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Compact);
        assert_eq!(
            fmt_with(
                "x = { a = 1, f = [\n    \"one\",\n    \"two\"\n] }\n",
                &style
            ),
            "x = { a = 1, f = [\"one\", \"two\"] }\n"
        );
    }

    #[test]
    fn compact_breaks_only_for_a_comment_it_cannot_carry() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Compact);
        assert_eq!(
            fmt_with("x = {\n    a = 1, # why\n    b = 2\n}\n", &style),
            "x = {\n    a = 1, # why\n    b = 2\n}\n"
        );
    }

    #[test]
    fn expand_keeps_the_two_key_rule() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Expand);
        assert_eq!(
            fmt_with("clap = { version = \"4.6.6\" }\n", &style),
            "clap.version = \"4.6.6\"\n"
        );
        assert_eq!(
            fmt_with(
                "clap = { version = \"4.6.6\", features = [\"derive\"] }\n",
                &style
            ),
            "clap = {\n    version = \"4.6.6\",\n    features = [\"derive\"]\n}\n"
        );
    }

    #[test]
    fn expand_reaches_a_nested_table_through_its_parent() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Expand);
        assert_eq!(
            fmt_with("items = [{ a = { b = 1, c = 2 } }]\n", &style),
            "items = [\n    {\n        a = {\n            b = 1,\n            c = 2\n        }\n    }\n]\n"
        );
    }

    #[test]
    fn multiline_inline_table_output_is_stable() {
        let style = with(|style| style.max_width = 30);
        let once = fmt_with(
            "clap = { version = \"4.6.6\", features = [\"derive\"] }\n",
            &style,
        );
        assert_eq!(fmt_with(&once, &style), once);
    }

    #[test]
    fn targeting_toml_1_0_pins_every_inline_table_to_one_line() {
        let style = with(|style| style.toml_version = TomlVersion::V1_0);
        let long = "clap = { version = \"4.6.6\", features = [\"derive\", \"cargo\", \"env\", \"unicode\", \"wrap_help\"] }\n";
        assert_eq!(fmt_with(long, &style), long);
        assert_eq!(
            fmt_with(
                "x = { a = 1, f = [\n    \"one\",\n    \"two\"\n] }\n",
                &style
            ),
            "x = { a = 1, f = [\"one\", \"two\"] }\n"
        );
    }

    #[test]
    fn targeting_toml_1_0_keeps_a_comment_that_forces_a_break() {
        let style = with(|style| style.toml_version = TomlVersion::V1_0);
        assert_eq!(
            fmt_with("x = {\n    a = 1, # why\n    b = 2\n}\n", &style),
            "x = {\n    a = 1, # why\n    b = 2\n}\n"
        );
    }

    #[test]
    fn targeting_toml_1_0_withholds_the_inline_trailing_comma() {
        let style = TomlStyle {
            toml_version: TomlVersion::V1_0,
            trailing_comma: TrailingComma::Multiline,
            ..TomlStyle::default()
        };
        assert_eq!(
            fmt_with("x = {\n    a = 1, # why\n    b = 2\n}\n", &style),
            "x = {\n    a = 1, # why\n    b = 2\n}\n"
        );
        assert_eq!(
            fmt_with("y = [\n    1,\n    2\n]\n", &style),
            "y = [\n    1,\n    2,\n]\n"
        );
    }

    #[test]
    fn section_promotes_a_table_too_wide_for_its_line() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Section);
        let out = fmt_with(
            "[dependencies]\nshort = \"1\"\nextremely_long_crate_name_goes_here = { version = \"4.5.6\", path = \"extremely_long_path_name_goes_right_here\" }\n",
            &style,
        );
        assert_eq!(
            out,
            "[dependencies]\nshort = \"1\"\n[dependencies.extremely_long_crate_name_goes_here]\nversion = \"4.5.6\"\npath = \"extremely_long_path_name_goes_right_here\"\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn section_leaves_a_table_that_fits_inline() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Section);
        let source = "[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n";
        assert_eq!(fmt_with(source, &style), source);
    }

    #[test]
    fn section_carries_the_comment_block_onto_the_new_header() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Section);
        let out = fmt_with(
            "[dependencies]\n# about it\nextremely_long_crate_name_goes_here = { version = \"4.5.6\", path = \"extremely_long_path_name_goes_right_here\" }\n",
            &style,
        );
        assert_eq!(
            out,
            "[dependencies]\n# about it\n[dependencies.extremely_long_crate_name_goes_here]\nversion = \"4.5.6\"\npath = \"extremely_long_path_name_goes_right_here\"\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn section_wraps_rather_than_losing_an_interior_comment() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Section);
        let out = fmt_with(
            "[dependencies]\nextremely_long_crate_name_goes_here = { version = \"4.5.6\", # why\n path = \"extremely_long_path_name_here\" }\n",
            &style,
        );
        assert_eq!(
            out,
            "[dependencies]\nextremely_long_crate_name_goes_here = {\n    version = \"4.5.6\", # why\n    path = \"extremely_long_path_name_here\"\n}\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn section_falls_back_where_no_header_can_be_written() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Section);
        let out = fmt_with(
            "a = [{ version = \"4.5.6\", path = \"extremely_long_path_name_goes_right_here_and_here_and_here_and_more\" }]\n",
            &style,
        );
        assert_eq!(
            out,
            "a = [\n    {\n        version = \"4.5.6\",\n        path = \"extremely_long_path_name_goes_right_here_and_here_and_here_and_more\"\n    }\n]\n"
        );
        assert_eq!(fmt_with(&out, &style), out);

        let out = fmt_with(
            "extremely_long_crate_name_goes_here = { version = \"4.5.6\", path = \"extremely_long_path_name_goes_right_here\" }\n",
            &style,
        );
        assert_eq!(
            out,
            "extremely_long_crate_name_goes_here = {\n    version = \"4.5.6\",\n    path = \"extremely_long_path_name_goes_right_here\"\n}\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn section_promotes_a_table_nested_in_a_promoted_one() {
        let style = with(|style| style.inline_tables = InlineTableStyle::Section);
        let out = fmt_with(
            "[t]\nouter = { inner = { version = \"4.5.6\", path = \"extremely_long_path_name_goes_right_here_yes\" }, b = 1 }\n",
            &style,
        );
        assert_eq!(
            out,
            "[t]\n[t.outer]\ninner = { version = \"4.5.6\", path = \"extremely_long_path_name_goes_right_here_yes\" }\nb = 1\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    // ------------------------------------------------------------------ arrays

    #[test]
    fn expand_arrays_break_at_two_elements() {
        let style = with(|style| style.arrays = ArrayStyle::Expand);
        assert_eq!(fmt_with("a = []\n", &style), "a = []\n");
        assert_eq!(fmt_with("a = [1]\n", &style), "a = [1]\n");
        assert_eq!(
            fmt_with("a = [1, 2]\n", &style),
            "a = [\n    1,\n    2\n]\n"
        );
    }

    #[test]
    fn expand_arrays_reach_a_nested_array_through_its_parent() {
        let style = with(|style| style.arrays = ArrayStyle::Expand);
        assert_eq!(
            fmt_with("a = { k = [1, 2] }\n", &style),
            "a.k = [\n    1,\n    2\n]\n"
        );
    }

    #[test]
    fn spaced_array_spacing_pads_the_brackets() {
        let style = with(|style| style.array_spacing = Spacing::Spaced);
        assert_eq!(fmt_with("a = [1, 2]\n", &style), "a = [ 1, 2 ]\n");
        assert_eq!(fmt_with("a = []\n", &style), "a = []\n");
    }

    #[test]
    fn spaced_array_spacing_narrows_the_budget() {
        let style = TomlStyle {
            array_spacing: Spacing::Spaced,
            max_width: 11,
            ..TomlStyle::default()
        };
        assert_eq!(
            fmt_with("a = [1, 2]\n", &style),
            "a = [\n    1,\n    2\n]\n"
        );
        let style = with(|style| {
            style.array_spacing = Spacing::Spaced;
            style.max_width = 12;
        });
        assert_eq!(fmt_with("a = [1, 2]\n", &style), "a = [ 1, 2 ]\n");
    }

    #[test]
    fn keeps_multiline_array() {
        assert_eq!(
            fmt("features = [\n    \"bmp\",\n    \"webp\",\n]\n"),
            "features = [\n    \"bmp\",\n    \"webp\"\n]\n"
        );
        assert_eq!(
            fmt("features = [\n    \"bmp\",\n    \"webp\"\n]\n"),
            "features = [\n    \"bmp\",\n    \"webp\"\n]\n"
        );
    }

    #[test]
    fn auto_arrays_reflow_a_multiline_array_that_fits() {
        let style = with(|style| style.arrays = ArrayStyle::Auto);
        assert_eq!(
            fmt_with("features = [\n    \"bmp\",\n    \"webp\"\n]\n", &style),
            "features = [\"bmp\", \"webp\"]\n"
        );
    }

    #[test]
    fn wrapped_array_has_no_trailing_comma() {
        let out = fmt(
            "features = [\"one-very-long-feature-name-that-should-wrap-because-it-is-quite-long\", \"two-very-long-feature-name-that-also-is-quite-long\"]\n",
        );
        assert_eq!(
            out,
            "features = [\n    \"one-very-long-feature-name-that-should-wrap-because-it-is-quite-long\",\n    \"two-very-long-feature-name-that-also-is-quite-long\"\n]\n"
        );
    }

    #[test]
    fn short_array_stays_inline() {
        assert_eq!(
            fmt("features = [\"derive\"]\n"),
            "features = [\"derive\"]\n"
        );
    }

    #[test]
    fn empty_array_collapses_onto_one_line() {
        assert_eq!(fmt("a = [\n]\n"), "a = []\n");
        assert_eq!(fmt("a = [ ]\n"), "a = []\n");
    }

    #[test]
    fn the_width_budget_counts_the_key() {
        let style = with(|style| style.max_width = 20);
        assert_eq!(fmt_with("k = [1, 2, 3]\n", &style), "k = [1, 2, 3]\n");
        assert_eq!(
            fmt_with("longer_key = [1, 2, 3]\n", &style),
            "longer_key = [\n    1,\n    2,\n    3\n]\n"
        );
    }

    #[test]
    fn the_width_budget_counts_an_authors_dotted_key_path() {
        let style = with(|style| style.max_width = 22);
        assert_eq!(fmt_with("a.b = [1, 2, 3]\n", &style), "a.b = [1, 2, 3]\n");

        let style = with(|style| style.max_width = 14);
        assert_eq!(
            fmt_with("a.b = [1, 2, 3]\n", &style),
            "a.b = [\n    1,\n    2,\n    3\n]\n"
        );
        assert_eq!(fmt_with("b = [1, 2, 3]\n", &style), "b = [1, 2, 3]\n");
    }

    #[test]
    fn a_padded_dotted_key_path_measures_as_it_renders() {
        for width in [13, 14, 15, 19, 22] {
            let style = with(|style| style.max_width = width);
            assert_eq!(
                fmt_with("a  .  b = [1, 2, 3]\n", &style),
                fmt_with("a.b = [1, 2, 3]\n", &style),
                "width {width}"
            );
        }
    }

    #[test]
    fn the_width_budget_counts_the_indent_of_a_nested_value() {
        let style = with(|style| style.max_width = 20);
        assert_eq!(
            fmt_with("x = { a = 1, k = [1, 2, 3, 4] }\n", &style),
            "x = {\n    a = 1,\n    k = [1, 2, 3, 4]\n}\n"
        );

        let style = with(|style| style.max_width = 19);
        assert_eq!(
            fmt_with("x = { a = 1, k = [1, 2, 3, 4] }\n", &style),
            "x = {\n    a = 1,\n    k = [\n        1,\n        2,\n        3,\n        4\n    ]\n}\n"
        );
    }

    // --------------------------------------------------------- trailing comma

    #[test]
    fn trailing_comma_applies_to_wrapped_containers_only() {
        let style = with(|style| {
            style.trailing_comma = TrailingComma::Multiline;
            style.max_width = 12;
        });
        assert_eq!(
            fmt_with("a = [1, 2, 3]\nb = [1]\n", &style),
            "a = [\n    1,\n    2,\n    3,\n]\nb = [1]\n"
        );
    }

    #[test]
    fn trailing_comma_keeps_a_comment_on_the_last_element() {
        let style = with(|style| style.trailing_comma = TrailingComma::Multiline);
        let out = fmt_with("a = [\n    1,\n    2 # two\n]\n", &style);
        assert_eq!(out, "a = [\n    1,\n    2, # two\n]\n");
        assert!(out.parse::<DocumentMut>().is_ok());
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn trailing_comma_keeps_a_comment_before_the_closing_bracket() {
        let style = with(|style| style.trailing_comma = TrailingComma::Multiline);
        let out = fmt_with("a = [\n    1,\n    2\n    # tail\n]\n", &style);
        assert_eq!(out, "a = [\n    1,\n    2,\n    # tail\n]\n");
        assert!(out.parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn trailing_comma_reaches_an_expanded_inline_table() {
        let style = with(|style| {
            style.trailing_comma = TrailingComma::Multiline;
            style.inline_tables = InlineTableStyle::Expand;
        });
        let out = fmt_with("x = { a = 1, b = 2 }\n", &style);
        assert_eq!(out, "x = {\n    a = 1,\n    b = 2,\n}\n");
        assert!(out.parse::<DocumentMut>().is_ok());
        assert_eq!(fmt_with(&out, &style), out);
    }

    // ---------------------------------------------------------------- indent

    #[test]
    fn a_tab_indent_leaves_the_closing_bracket_flush() {
        let style = with(|style| {
            style.indent = TomlIndent::tab();
            style.max_width = 5;
        });
        assert_eq!(
            fmt_with("k = [\n  1,\n  # why\n]\n", &style),
            "k = [\n\t1\n\t# why\n]\n"
        );
    }

    #[test]
    fn a_tab_level_costs_a_whole_tab_stop_in_the_budget() {
        let style = with(|style| {
            style.indent = TomlIndent::tab();
            style.max_width = 15;
        });
        // The nested array starts at column 4, so `[1, 2, 3, 4]` ends at 16.
        assert_eq!(
            fmt_with("k = [[1, 2, 3, 4]]\n", &style),
            "k = [\n\t[\n\t\t1,\n\t\t2,\n\t\t3,\n\t\t4\n\t]\n]\n"
        );
        let style = with(|style| {
            style.indent = TomlIndent::with_tab_width("\t", 1);
            style.max_width = 15;
        });
        assert_eq!(
            fmt_with("k = [[1, 2, 3, 4]]\n", &style),
            "k = [\n\t[1, 2, 3, 4]\n]\n"
        );
    }

    #[test]
    fn indent_entries_indents_a_table_body() {
        let style = with(|style| style.indent_entries = true);
        assert_eq!(
            fmt_with("a = 0\n[t]\nk = 1\n[t.u]\nm = 2\n", &style),
            "a = 0\n[t]\n    k = 1\n[t.u]\n    m = 2\n"
        );
    }

    #[test]
    fn indent_tables_indents_a_header_by_the_headers_above_it() {
        let style = with(|style| style.indent_tables = true);
        assert_eq!(
            fmt_with("[t]\nk = 1\n[t.u]\nm = 2\n[[v]]\nn = 3\n", &style),
            "[t]\nk = 1\n    [t.u]\n    m = 2\n[[v]]\nn = 3\n"
        );
    }

    #[test]
    fn indent_tables_skips_a_header_nobody_writes() {
        let style = with(|style| {
            style.indent_tables = true;
            style.indent_entries = true;
        });
        assert_eq!(fmt_with("[a.b.c]\nk = 1\n", &style), "[a.b.c]\n    k = 1\n");
    }

    #[test]
    fn an_indented_body_carries_its_indent_into_the_budget() {
        let style = with(|style| {
            style.indent_entries = true;
            style.max_width = 17;
        });
        // `    k = [1, 2, 3]` is 17 columns; one more element passes the budget.
        assert_eq!(
            fmt_with("[t]\nk = [1, 2, 3]\n", &style),
            "[t]\n    k = [1, 2, 3]\n"
        );
        assert_eq!(
            fmt_with("[t]\nk = [1, 2, 3, 4]\n", &style),
            "[t]\n    k = [\n        1,\n        2,\n        3,\n        4\n    ]\n"
        );
    }

    #[test]
    fn an_indented_body_indents_its_comments() {
        let style = with(|style| style.indent_entries = true);
        assert_eq!(
            fmt_with("[t]\n# note\nk = 1\n", &style),
            "[t]\n    # note\n    k = 1\n"
        );
    }

    #[test]
    fn indent_is_configurable() {
        let style = with(|style| {
            style.indent = TomlIndent::spaces(2);
            style.max_width = 12;
        });
        assert_eq!(
            fmt_with("a = [1, 2, 3]\n", &style),
            "a = [\n  1,\n  2,\n  3\n]\n"
        );

        let style = with(|style| {
            style.indent = TomlIndent::tab();
            style.max_width = 12;
        });
        assert_eq!(
            fmt_with("a = [1, 2, 3]\n", &style),
            "a = [\n\t1,\n\t2,\n\t3\n]\n"
        );
    }

    #[test]
    fn indent_reaches_a_comment_inside_a_container() {
        let style = with(|style| style.indent = TomlIndent::spaces(2));
        assert_eq!(
            fmt_with("a = [\n    # why\n    1,\n    2\n]\n", &style),
            "a = [\n  # why\n  1,\n  2\n]\n"
        );
    }

    // ------------------------------------------------------------ blank lines

    #[test]
    fn a_blank_line_of_whitespace_is_kept() {
        assert_eq!(fmt("a = 1\n\n# c\n \nb = 2\n"), "a = 1\n\n# c\n\nb = 2\n");
        assert_eq!(fmt("a = 1\n \nb = 2\n"), "a = 1\n\nb = 2\n");
        assert_eq!(fmt("a = 1\n\t\n[t]\nb = 2\n"), "a = 1\n\n[t]\nb = 2\n");
    }

    #[test]
    fn the_blank_line_maximum_is_configurable() {
        let source = "a = 1\n\n\n\nb = 2\n";
        assert_eq!(fmt(source), "a = 1\n\nb = 2\n");
        assert_eq!(
            fmt_with(source, &with(|style| style.max_blank_lines = 2)),
            "a = 1\n\n\nb = 2\n"
        );
        assert_eq!(
            fmt_with(source, &with(|style| style.max_blank_lines = 0)),
            "a = 1\nb = 2\n"
        );
    }

    #[test]
    fn the_blank_line_maximum_reaches_comment_blocks() {
        let source = "# one\n\n\n# two\n\n\na = 1\n";
        assert_eq!(fmt(source), "# one\n\n# two\n\na = 1\n");
        assert_eq!(
            fmt_with(source, &with(|style| style.max_blank_lines = 2)),
            "# one\n\n\n# two\n\n\na = 1\n"
        );
        assert_eq!(
            fmt_with(source, &with(|style| style.max_blank_lines = 0)),
            "# one\n# two\na = 1\n"
        );
    }

    #[test]
    fn blank_lines() {
        assert_eq!(fmt("a = 1\n\nb = 2\n"), "a = 1\n\nb = 2\n");
        assert_eq!(fmt("a = 1\n\n\n\nb = 2\n"), "a = 1\n\nb = 2\n");
    }

    #[test]
    fn blank_after_table_header() {
        assert_eq!(
            fmt("[package]\n\nname = \"foo\"\n"),
            "[package]\n\nname = \"foo\"\n"
        );
    }

    #[test]
    fn blank_before_comment() {
        assert_eq!(fmt("a = 1\n\n# c\nb = 2\n"), "a = 1\n\n# c\nb = 2\n");
    }

    #[test]
    fn blank_between_comment_blocks() {
        assert_eq!(
            fmt("# first\n# still first\n\n# second\nkey = 1\n"),
            "# first\n# still first\n\n# second\nkey = 1\n"
        );
        assert_eq!(
            fmt("a = 1\n# first\n\n# second\nb = 2\n"),
            "a = 1\n# first\n\n# second\nb = 2\n"
        );
        assert_eq!(fmt("# a\n\n\n# b\nk = 1\n"), "# a\n\n# b\nk = 1\n");
        assert_eq!(
            fmt("# top\n\n# top2\n\n[package]\nname = \"x\"\n"),
            "# top\n\n# top2\n\n[package]\nname = \"x\"\n"
        );
        assert_eq!(
            fmt("[package]\nname = \"x\"\n\n# mid\n\n# mid2\n[deps]\n"),
            "[package]\nname = \"x\"\n\n# mid\n\n# mid2\n[deps]\n"
        );
        assert_eq!(
            fmt("key = 1\n\n# end one\n\n# end two\n"),
            "key = 1\n\n# end one\n\n# end two\n"
        );
        assert_eq!(
            fmt("x = {\n    # first\n\n    # second\n    a = 1,\n    b = 2\n}\n"),
            "x = {\n    # first\n\n    # second\n    a = 1,\n    b = 2\n}\n"
        );
        assert_eq!(
            fmt("x = [\n    # first\n\n    # second\n    1,\n    2\n]\n"),
            "x = [\n    # first\n\n    # second\n    1,\n    2\n]\n"
        );
    }

    #[test]
    fn blank_line_before_tables_is_inserted_on_request() {
        let style = with(|style| style.blank_line_before_tables = true);
        assert_eq!(
            fmt_with("[a]\nx = 1\n[b]\ny = 2\n", &style),
            "[a]\nx = 1\n\n[b]\ny = 2\n"
        );
        assert_eq!(
            fmt_with("[[bin]]\nname = \"x\"\n[[bin]]\nname = \"y\"\n", &style),
            "[[bin]]\nname = \"x\"\n\n[[bin]]\nname = \"y\"\n"
        );
    }

    #[test]
    fn blank_line_before_tables_leaves_the_opening_header_flush() {
        let style = with(|style| style.blank_line_before_tables = true);
        assert_eq!(fmt_with("\n\n[a]\nx = 1\n", &style), "[a]\nx = 1\n");
        assert_eq!(
            fmt_with("[a.b]\nx = 1\n[a.c]\ny = 2\n", &style),
            "[a.b]\nx = 1\n\n[a.c]\ny = 2\n"
        );
    }

    #[test]
    fn blank_line_before_tables_follows_a_root_body() {
        let style = with(|style| style.blank_line_before_tables = true);
        assert_eq!(
            fmt_with("x = 1\n[a]\ny = 2\n", &style),
            "x = 1\n\n[a]\ny = 2\n"
        );
    }

    #[test]
    fn blank_line_before_tables_goes_above_the_comment_block() {
        let style = with(|style| style.blank_line_before_tables = true);
        assert_eq!(
            fmt_with("[a]\nx = 1\n# note\n[b]\n", &style),
            "[a]\nx = 1\n\n# note\n[b]\n"
        );
    }

    // ------------------------------------------------------------------ keys

    #[test]
    fn preserve_quotes() {
        assert_eq!(fmt("key = 'win\\path'\n"), "key = 'win\\path'\n");
    }

    #[test]
    fn quoted_keys_are_kept_by_default() {
        assert_eq!(fmt("\"quoted\" = 1\n"), "\"quoted\" = 1\n");
    }

    #[test]
    fn normalize_keys_unquotes_only_what_the_spec_allows() {
        let style = with(|style| style.normalize_keys = true);
        assert_eq!(fmt_with("\"quoted\" = 1\n", &style), "quoted = 1\n");
        assert_eq!(fmt_with("'literal' = 1\n", &style), "literal = 1\n");
        assert_eq!(fmt_with("\"1234\" = 1\n", &style), "1234 = 1\n");
        assert_eq!(fmt_with("'lit key' = 1\n", &style), "'lit key' = 1\n");
        assert_eq!(fmt_with("\"\" = 1\n", &style), "\"\" = 1\n");
        assert_eq!(fmt_with("\"a.b\" = 1\n", &style), "\"a.b\" = 1\n");
    }

    #[test]
    fn normalize_keys_reaches_headers_and_inline_entries() {
        let style = with(|style| style.normalize_keys = true);
        assert_eq!(
            fmt_with("[\"pkg\"]\n\"name\" = \"x\"\n", &style),
            "[pkg]\nname = \"x\"\n"
        );
        assert_eq!(
            fmt_with("x = { \"a\" = 1, \"b\" = 2 }\n", &style),
            "x = { a = 1, b = 2 }\n"
        );
        assert_eq!(
            fmt_with("[[\"bin\"]]\nname = \"x\"\n", &style),
            "[[bin]]\nname = \"x\"\n"
        );
    }

    #[test]
    fn normalize_keys_keeps_the_comment_above_a_key() {
        let style = with(|style| style.normalize_keys = true);
        assert_eq!(
            fmt_with("# note\n\"quoted\" = 1 # tail\n", &style),
            "# note\nquoted = 1 # tail\n"
        );
    }

    #[test]
    fn a_dotted_path_loses_the_padding_around_its_dots() {
        let out = fmt("a  .  b  .  c = 1\n");
        assert_eq!(out, "a.b.c = 1\n");
        assert_eq!(fmt(&out), out);
    }

    #[test]
    fn a_table_header_loses_the_padding_inside_its_brackets() {
        assert_eq!(fmt("[  a  .  b  ]\nx = 1\n"), "[a.b]\nx = 1\n");
        assert_eq!(fmt("[\ta\t.\tb\t]\nx = 1\n"), "[a.b]\nx = 1\n");
    }

    #[test]
    fn an_array_of_tables_header_loses_its_padding_on_every_element() {
        assert_eq!(
            fmt("[[  z  ]]\nname = \"q\"\n[[  z  ]]\nname = \"r\"\n"),
            "[[z]]\nname = \"q\"\n[[z]]\nname = \"r\"\n"
        );
    }

    #[test]
    fn padding_goes_but_the_quoting_of_each_segment_stays() {
        assert_eq!(fmt("\"a\" . 'b' = 1\n"), "\"a\".'b' = 1\n");
        assert_eq!(fmt("[ \"a\" . 'b' ]\nx = 1\n"), "[\"a\".'b']\nx = 1\n");
        let style = with(|style| style.normalize_keys = true);
        assert_eq!(fmt_with("\"a\" . 'b' = 1\n", &style), "a.b = 1\n");
        assert_eq!(
            fmt_with("[ \"a\" . 'b' ]\nx = 1\n", &style),
            "[a.b]\nx = 1\n"
        );
    }

    #[test]
    fn padding_goes_without_disturbing_the_comments_around_the_key() {
        assert_eq!(
            fmt("# note\na  .  b = 1 # tail\n"),
            "# note\na.b = 1 # tail\n"
        );
        assert_eq!(
            fmt("# note\n[  a  .  b  ] # tail\nx = 1\n"),
            "# note\n[a.b] # tail\nx = 1\n"
        );
    }

    // --------------------------------------------------------------- sorting

    #[test]
    fn order_is_preserved_by_default() {
        assert_eq!(fmt("b = 1\na = 2\n"), "b = 1\na = 2\n");
        assert_eq!(
            fmt("[dependencies]\nzzz = \"1\"\naaa = \"2\"\n"),
            "[dependencies]\nzzz = \"1\"\naaa = \"2\"\n"
        );
    }

    #[test]
    fn sort_deps_orders_every_dependency_table() {
        let style = with(|style| style.sort_deps = true);
        for section in [
            "dependencies",
            "dev-dependencies",
            "build-dependencies",
            "workspace.dependencies",
            "target.'cfg(unix)'.dependencies",
        ] {
            let input = format!("[{section}]\nzzz = \"1\"\naaa = \"2\"\n");
            let expected = format!("[{section}]\naaa = \"2\"\nzzz = \"1\"\n");
            assert_eq!(fmt_with(&input, &style), expected, "{section}");
        }
    }

    #[test]
    fn sort_deps_leaves_other_tables_alone() {
        let style = with(|style| style.sort_deps = true);
        assert_eq!(
            fmt_with("[features]\nzzz = []\naaa = []\n", &style),
            "[features]\nzzz = []\naaa = []\n"
        );
        assert_eq!(
            fmt_with(
                "[dependencies.serde]\nversion = \"1\"\nfeatures = []\n",
                &style
            ),
            "[dependencies.serde]\nversion = \"1\"\nfeatures = []\n"
        );
    }

    #[test]
    fn sort_deps_carries_a_comment_block_with_its_entry() {
        let style = with(|style| style.sort_deps = true);
        assert_eq!(
            fmt_with(
                "[dependencies]\nzzz = \"1\"\n# about aaa\naaa = \"2\"\n",
                &style
            ),
            "[dependencies]\n# about aaa\naaa = \"2\"\nzzz = \"1\"\n"
        );
    }

    #[test]
    fn sort_deps_keeps_the_blank_line_under_the_header() {
        let style = with(|style| style.sort_deps = true);
        assert_eq!(
            fmt_with("[dependencies]\n\nzzz = \"1\"\naaa = \"2\"\n", &style),
            "[dependencies]\n\naaa = \"2\"\nzzz = \"1\"\n"
        );
    }

    #[test]
    fn sort_deps_reorders_header_sub_tables_and_their_spacing() {
        let style = with(|style| style.sort_deps = true);
        let out = fmt_with(
            "[dependencies.zzz]\nversion = \"1\"\n\n[dependencies.aaa]\nversion = \"2\"\n",
            &style,
        );
        assert_eq!(
            out,
            "[dependencies.aaa]\nversion = \"2\"\n\n[dependencies.zzz]\nversion = \"1\"\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn sort_deps_mixes_value_and_header_entries() {
        let style = with(|style| style.sort_deps = true);
        let out = fmt_with(
            "[dependencies]\nzzz = \"1\"\nbbb = \"2\"\n\n[dependencies.aaa]\nversion = \"3\"\n",
            &style,
        );
        assert_eq!(
            out,
            "[dependencies]\nbbb = \"2\"\nzzz = \"1\"\n\n[dependencies.aaa]\nversion = \"3\"\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn sort_package_uses_the_canonical_order() {
        let style = with(|style| style.sort_package = true);
        assert_eq!(
            fmt_with(
                "[package]\ndescription = \"d\"\ncustom = 1\nname = \"n\"\nedition = \"2024\"\nother = 2\n",
                &style
            ),
            "[package]\nname = \"n\"\nedition = \"2024\"\ndescription = \"d\"\ncustom = 1\nother = 2\n"
        );
        assert_eq!(
            fmt_with(
                "[workspace.package]\nedition = \"2024\"\nname = \"n\"\n",
                &style
            ),
            "[workspace.package]\nname = \"n\"\nedition = \"2024\"\n"
        );
    }

    #[test]
    fn sort_package_leaves_a_lock_file_package_array_alone() {
        let style = with(|style| style.sort_package = true);
        assert_eq!(
            fmt_with("[[package]]\nversion = \"1\"\nname = \"n\"\n", &style),
            "[[package]]\nversion = \"1\"\nname = \"n\"\n"
        );
        assert_eq!(
            fmt_with("[tool.package]\nversion = \"1\"\nname = \"n\"\n", &style),
            "[tool.package]\nversion = \"1\"\nname = \"n\"\n"
        );
    }

    #[test]
    fn keys_sort_by_the_style_guide_version_order() {
        let style = with(|style| style.sort_deps = true);
        assert_eq!(
            fmt_with(
                "[dependencies]\nZed = \"1\"\naaa = \"2\"\nBee = \"3\"\n_x = \"4\"\n",
                &style
            ),
            "[dependencies]\n_x = \"4\"\nBee = \"3\"\nZed = \"1\"\naaa = \"2\"\n"
        );
        assert_eq!(
            fmt_with(
                "[dependencies]\nx32 = \"1\"\nx8 = \"2\"\nx16 = \"3\"\n",
                &style
            ),
            "[dependencies]\nx8 = \"2\"\nx16 = \"3\"\nx32 = \"1\"\n"
        );
    }

    #[test]
    fn sort_deps_reaches_patch_and_replace() {
        let style = with(|style| style.sort_deps = true);
        assert_eq!(
            fmt_with("[patch.crates-io]\nzzz = \"1\"\naaa = \"2\"\n", &style),
            "[patch.crates-io]\naaa = \"2\"\nzzz = \"1\"\n"
        );
        assert_eq!(
            fmt_with("[replace]\nzzz = \"1\"\naaa = \"2\"\n", &style),
            "[replace]\naaa = \"2\"\nzzz = \"1\"\n"
        );
    }

    #[test]
    fn dep_fields_are_only_ordered_on_request() {
        let source =
            "[dependencies.serde]\nfeatures = [\"derive\"]\noptional = true\nversion = \"1\"\n";
        assert_eq!(fmt(source), source);
        let style = with(|style| style.sort_dep_fields = true);
        assert_eq!(
            fmt_with(source, &style),
            "[dependencies.serde]\nversion = \"1\"\nfeatures = [\"derive\"]\noptional = true\n"
        );
    }

    #[test]
    fn dep_fields_are_ordered_in_every_spelling() {
        let style = with(|style| style.sort_dep_fields = true);
        assert_eq!(
            fmt_with(
                "[dependencies]\nserde = { optional = true, version = \"1\", git = \"g\" }\n",
                &style
            ),
            "[dependencies]\nserde = { version = \"1\", git = \"g\", optional = true }\n"
        );
        assert_eq!(
            fmt_with(
                "[dependencies]\nserde.optional = true\nserde.version = \"1\"\n",
                &style
            ),
            "[dependencies]\nserde.version = \"1\"\nserde.optional = true\n"
        );
    }

    #[test]
    fn dep_fields_leave_other_tables_alone() {
        let style = with(|style| style.sort_dep_fields = true);
        let source = "[profile.release]\noptional = true\nversion = \"1\"\n";
        assert_eq!(fmt_with(source, &style), source);
    }

    #[test]
    fn features_are_only_ordered_on_request() {
        let source = "[features]\nzzz = []\naaa = []\n";
        assert_eq!(fmt(source), source);
        let style = with(|style| style.sort_features = true);
        assert_eq!(fmt_with(source, &style), "[features]\naaa = []\nzzz = []\n");
    }

    #[test]
    fn sortable_arrays_are_the_ones_cargo_reads_as_a_set() {
        let style = with(|style| style.sort_arrays = true);
        assert_eq!(
            fmt_with(
                "[package]\nkeywords = [\"z\", \"a\"]\ncategories = [\"z\", \"a\"]\nauthors = [\"z\", \"a\"]\n",
                &style
            ),
            "[package]\nkeywords = [\"a\", \"z\"]\ncategories = [\"a\", \"z\"]\nauthors = [\"z\", \"a\"]\n"
        );
        assert_eq!(
            fmt_with(
                "[workspace]\nmembers = [\"crates/z\", \"crates/a\"]\n",
                &style
            ),
            "[workspace]\nmembers = [\"crates/a\", \"crates/z\"]\n"
        );
        assert_eq!(
            fmt_with("[features]\na = [\"z\", \"b\"]\n", &style),
            "[features]\na = [\"b\", \"z\"]\n"
        );
        assert_eq!(
            fmt_with(
                "[dependencies]\nserde = { version = \"1\", features = [\"z\", \"a\"] }\n",
                &style
            ),
            "[dependencies]\nserde = { version = \"1\", features = [\"a\", \"z\"] }\n"
        );
    }

    #[test]
    fn a_commented_or_non_string_array_is_left_alone() {
        let style = with(|style| style.sort_arrays = true);
        assert_eq!(
            fmt_with(
                "[features]\na = [\n    \"z\",\n    # keep\n    \"a\"\n]\n",
                &style
            ),
            "[features]\na = [\n    \"z\",\n    # keep\n    \"a\"\n]\n"
        );
        assert_eq!(
            fmt_with("[features]\na = [3, 1, 2]\n", &style),
            "[features]\na = [3, 1, 2]\n"
        );
    }

    #[test]
    fn target_arrays_are_only_ordered_on_request() {
        let source = "[[bin]]\nname = \"zzz\"\n\n[[bin]]\nname = \"aaa\"\n";
        assert_eq!(fmt(source), source);
        let style = with(|style| style.sort_targets = true);
        let out = fmt_with(source, &style);
        assert_eq!(out, "[[bin]]\nname = \"aaa\"\n\n[[bin]]\nname = \"zzz\"\n");
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn target_arrays_leave_a_lock_file_package_array_alone() {
        let style = with(|style| style.sort_targets = true);
        let source = "[[package]]\nname = \"zzz\"\n\n[[package]]\nname = \"aaa\"\n";
        assert_eq!(fmt_with(source, &style), source);
    }

    #[test]
    fn the_document_sequence_is_only_ordered_on_request() {
        let source = "[dependencies]\na = \"1\"\n\n[features]\nx = []\n\n[package]\nname = \"n\"\n";
        assert_eq!(fmt(source), source);
        let style = with(|style| style.sort_tables = true);
        let out = fmt_with(source, &style);
        assert_eq!(
            out,
            "[package]\nname = \"n\"\n\n[features]\nx = []\n\n[dependencies]\na = \"1\"\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn sort_keys_reaches_every_section_but_package_and_the_root() {
        let style = with(|style| style.sort_keys = true);
        let out = fmt_with(
            "[profile.release]\nzzz = 1\naaa = 2\n\n[package]\nversion = \"1\"\nname = \"n\"\n",
            &style,
        );
        assert_eq!(
            out,
            "[profile.release]\naaa = 2\nzzz = 1\n\n[package]\nversion = \"1\"\nname = \"n\"\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn grouped_sorting_never_moves_an_entry_across_a_blank_line() {
        let style = with(|style| {
            style.sort_deps = true;
            style.sort_grouped = true;
        });
        let source = "[dependencies]\nzzz = \"1\"\nmmm = \"2\"\n\nbbb = \"3\"\naaa = \"4\"\n";
        let out = fmt_with(source, &style);
        assert_eq!(
            out,
            "[dependencies]\nmmm = \"2\"\nzzz = \"1\"\n\naaa = \"4\"\nbbb = \"3\"\n"
        );
        assert_eq!(fmt_with(&out, &style), out);

        let ungrouped = with(|style| style.sort_deps = true);
        assert_eq!(
            fmt_with(source, &ungrouped),
            "[dependencies]\naaa = \"4\"\nbbb = \"3\"\n\nmmm = \"2\"\nzzz = \"1\"\n"
        );
    }

    #[test]
    fn grouped_sorting_partitions_header_blocks_too() {
        let style = with(|style| {
            style.sort_deps = true;
            style.sort_grouped = true;
        });
        let out = fmt_with(
            "[dependencies.zzz]\nv = 1\n\n[dependencies.bbb]\nv = 2\n[dependencies.aaa]\nv = 3\n",
            &style,
        );
        assert_eq!(
            out,
            "[dependencies.zzz]\nv = 1\n\n[dependencies.aaa]\nv = 3\n[dependencies.bbb]\nv = 2\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn the_style_guide_package_order_puts_description_last() {
        let style = with(|style| {
            style.sort_package = true;
            style.package_order = crate::toml_style::PackageOrder::StyleGuide;
        });
        let out = fmt_with(
            "[package]\ndescription = \"d\"\nzed = 1\nversion = \"1\"\nname = \"n\"\nedition = \"2024\"\n",
            &style,
        );
        assert_eq!(
            out,
            "[package]\nname = \"n\"\nversion = \"1\"\nedition = \"2024\"\nzed = 1\ndescription = \"d\"\n"
        );
        assert_eq!(fmt_with(&out, &style), out);
    }

    #[test]
    fn sorting_is_idempotent_together() {
        let style = with(|style| {
            style.sort_deps = true;
            style.sort_package = true;
            style.sort_dep_fields = true;
            style.sort_features = true;
            style.sort_arrays = true;
            style.sort_targets = true;
            style.sort_tables = true;
            style.sort_keys = true;
            style.sort_grouped = true;
            style.normalize_keys = true;
            style.blank_line_before_tables = true;
        });
        let input = "[dependencies]\nzzz = \"1\"\n# note\naaa = { features = [\"z\", \"a\"], version = \"2\" }\n\nmmm = \"3\"\n[features]\nzed = [\"b\", \"a\"]\n[package]\nversion = \"0.1.0\"\n\"name\" = \"demo\"\nkeywords = [\"z\", \"a\"]\n[[bin]]\nname = \"zzz\"\n[[bin]]\nname = \"aaa\"\n[dependencies.qqq]\nversion = \"3\"\n";
        let once = fmt_with(input, &style);
        assert_eq!(fmt_with(&once, &style), once);
    }

    // ------------------------------------------------------ cargo conventions

    #[test]
    fn a_version_only_table_is_only_collapsed_on_request() {
        let source = "[dependencies]\nserde = { version = \"1.0\" }\n";
        assert_eq!(fmt(source), "[dependencies]\nserde.version = \"1.0\"\n");
        let style = with(|style| style.cargo_conventions = true);
        assert_eq!(
            fmt_with(source, &style),
            "[dependencies]\nserde = \"1.0\"\n"
        );
    }

    #[test]
    fn collapsing_a_version_keeps_the_comments_around_it() {
        let style = with(|style| style.cargo_conventions = true);
        assert_eq!(
            fmt_with(
                "[dependencies]\n# about it\nserde = { version = \"1.0\" } # tail\n",
                &style
            ),
            "[dependencies]\n# about it\nserde = \"1.0\" # tail\n"
        );
        // A comment after the value is carried out with it. Holding the table
        // open for one instead is not a fixed point: the layout pass moves the
        // comment past the separating comma, and the next pass collapses.
        assert_eq!(
            fmt_with(
                "[dependencies]\nserde = { version = \"1.0\" # why\n }\n",
                &style
            ),
            "[dependencies]\nserde = \"1.0\" # why\n"
        );
        // A comment the collapse really would drop still holds it open.
        assert_eq!(
            fmt_with(
                "[dependencies]\nserde = { version = \"1.0\" # why\n# and more\n }\n",
                &style
            ),
            "[dependencies]\nserde = {\n    version = \"1.0\" # why\n    # and more\n}\n"
        );
    }

    #[test]
    fn collapsing_a_version_leaves_everything_else_alone() {
        let style = with(|style| style.cargo_conventions = true);
        assert_eq!(
            fmt_with(
                "[dependencies]\nserde = { version = \"1\", optional = true }\n",
                &style
            ),
            "[dependencies]\nserde = { version = \"1\", optional = true }\n"
        );
        assert_eq!(
            fmt_with("[foo]\nbar = { version = \"3\" }\n", &style),
            "[foo]\nbar.version = \"3\"\n"
        );
        assert_eq!(
            fmt_with("[dependencies.serde]\nversion = \"1\"\n", &style),
            "[dependencies.serde]\nversion = \"1\"\n"
        );
    }

    // -------------------------------------------------------------- comments

    #[test]
    fn a_document_that_is_only_a_comment_keeps_it() {
        assert_eq!(fmt("#a comment"), "# a comment\n");
        assert_eq!(fmt("a = 1\n# tail"), "a = 1\n# tail\n");
    }

    #[test]
    fn aligned_entries_pad_to_the_widest_key() {
        let style = with(|style| style.align_entries = true);
        assert_eq!(
            fmt_with("a = 1\nbbb = 2\n\ncc = 3\ndddd = 4\n", &style),
            "a   = 1\nbbb = 2\n\ncc   = 3\ndddd = 4\n"
        );
    }

    #[test]
    fn aligned_entries_reach_a_collapsed_dotted_key() {
        let style = with(|style| style.align_entries = true);
        assert_eq!(
            fmt_with("a = { workspace = true }\nb = 1\n", &style),
            "a.workspace = true\nb           = 1\n"
        );
    }

    #[test]
    fn aligned_comments_pad_to_the_widest_line() {
        let style = with(|style| style.align_comments = true);
        assert_eq!(
            fmt_with("a = 1 # one\nbbb = 22 # two\n", &style),
            "a = 1    # one\nbbb = 22 # two\n"
        );
    }

    #[test]
    fn alignment_leaves_hashes_and_equals_inside_strings_alone() {
        let style = with(|style| {
            style.align_entries = true;
            style.align_comments = true;
        });
        // The first `=` outside a string is the entry's, and a `#` inside one
        // opens nothing.
        assert_eq!(
            fmt_with(
                "a = \"# x = y\"\nbbb = \"\"\"\n# not a comment\n\"\"\"\n",
                &style
            ),
            "a   = \"# x = y\"\nbbb = \"\"\"\n# not a comment\n\"\"\"\n"
        );
    }

    #[test]
    fn alignment_is_a_fixed_point() {
        let style = with(|style| {
            style.align_entries = true;
            style.align_comments = true;
        });
        let once = fmt_with("a = 1 # one\nbbb = 22 # two\ncc = 3\n", &style);
        assert_eq!(fmt_with(&once, &style), once);
    }

    #[test]
    fn alignment_groups_break_at_a_wrapped_container() {
        let style = with(|style| {
            style.align_entries = true;
            style.max_width = 12;
        });
        let plain = with(|style| style.max_width = 12);
        let source = "k = [1, 2, 3, 4]\nmm = 1\n";
        assert_eq!(fmt_with(source, &style), fmt_with(source, &plain));
    }

    /// Padding is applied once the layout is settled, so it never changes what
    /// wrapped — and a padded line may end past the budget, as an unbreakable
    /// value already can.
    #[test]
    fn alignment_pads_after_layout() {
        let style = with(|style| {
            style.align_entries = true;
            style.max_width = 14;
        });
        assert_eq!(
            fmt_with("k = [1, 2]\nlonger = [3, 4]\n", &style),
            "k      = [1, 2]\nlonger = [\n    3,\n    4\n]\n"
        );
    }

    #[test]
    fn comment_space() {
        assert_eq!(fmt("key=\"v\"#c\n"), "key = \"v\" # c\n");
    }

    #[test]
    fn comments_survive_dotted() {
        let out = fmt("project = { path = \"x\" } # keep\n");
        assert_eq!(out, "project.path = \"x\" # keep\n");
    }

    #[test]
    fn comments_survive_table_header() {
        assert_eq!(
            fmt("[package] # keep-header\nname = \"x\"\n"),
            "[package] # keep-header\nname = \"x\"\n"
        );
    }

    #[test]
    fn comments_survive_array_of_tables_header() {
        assert_eq!(
            fmt("[[bin]] # c\nname = \"x\"\n"),
            "[[bin]] # c\nname = \"x\"\n"
        );
    }

    #[test]
    fn a_comment_outside_the_braces_does_not_break_the_table() {
        assert_eq!(
            fmt("x = { a = 1, b = 2 } # keep\n"),
            "x = { a = 1, b = 2 } # keep\n"
        );
    }

    #[test]
    fn a_comment_inside_the_braces_does() {
        assert_eq!(
            fmt("x = { a = 1, # why\n b = 2 }\n"),
            "x = {\n    a = 1, # why\n    b = 2\n}\n"
        );
    }

    #[test]
    fn heading_comment_hashes_are_preserved() {
        assert_eq!(fmt("## Banner\nkey = 1\n"), "## Banner\nkey = 1\n");
        assert_eq!(fmt("### Title\nkey = 1\n"), "### Title\nkey = 1\n");
        assert_eq!(fmt("#\nkey = 1\n"), "#\nkey = 1\n");
    }

    #[test]
    fn trailing_array_comment_stays_on_its_element() {
        assert_eq!(
            fmt("arr = [\n    1, # one\n    2\n]\n"),
            "arr = [\n    1, # one\n    2\n]\n"
        );
        assert_eq!(
            fmt("arr = [1, # one\n2]\n"),
            "arr = [\n    1, # one\n    2\n]\n"
        );
        let once = fmt("arr = [\n    1, # one\n    2\n]\n");
        assert_eq!(fmt(&once), once);
    }

    #[test]
    fn standalone_array_comment_stays_between_elements() {
        assert_eq!(
            fmt("arr = [\n    1,\n    # two\n    2\n]\n"),
            "arr = [\n    1,\n    # two\n    2\n]\n"
        );
    }

    #[test]
    fn comment_before_a_comma_keeps_the_comma() {
        assert_eq!(fmt("a = [1 # c\n, 2]\n"), "a = [\n    1, # c\n    2\n]\n");
        assert_eq!(
            fmt("x = {a = 1 # c\n, b = 2}\n"),
            "x = {\n    a = 1, # c\n    b = 2\n}\n"
        );
        assert!(fmt("a = [1 # c\n, 2]\n").parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn comment_before_a_closing_bracket_survives() {
        assert_eq!(
            fmt("a = [\n    1,\n    2\n    # tail\n]\n"),
            "a = [\n    1,\n    2\n    # tail\n]\n"
        );
        assert_eq!(
            fmt("x = {\n    a = 1,\n    b = 2\n    # tail\n}\n"),
            "x = {\n    a = 1,\n    b = 2\n    # tail\n}\n"
        );
        assert_eq!(
            fmt("a = [\n    1,\n    2, # two\n]\n"),
            "a = [\n    1,\n    2 # two\n]\n"
        );
    }

    #[test]
    fn comment_only_array_keeps_its_comments() {
        assert_eq!(
            fmt("targets = [\n    # keep\n    # me\n]\n"),
            "targets = [\n    # keep\n    # me\n]\n"
        );
        assert_eq!(fmt("x = {\n    # keep\n}\n"), "x = {\n    # keep\n}\n");
    }

    #[test]
    fn comment_above_an_array_element_survives() {
        assert_eq!(
            fmt("a = [\n    # why\n    { b = 1, c = 2 }\n]\n"),
            "a = [\n    # why\n    { b = 1, c = 2 }\n]\n"
        );
    }

    // -------------------------------------------------------------- structure

    #[test]
    fn header_tables_stay() {
        let out = fmt("[package]\nname = \"foo\"\nversion = \"0.1.0\"\n");
        assert_eq!(out, "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n");
    }

    #[test]
    fn array_of_tables() {
        let out = fmt("[[bin]]\nname = \"x\"\n\n[[bin]]\nname = \"y\"\n");
        assert!(out.contains("[[bin]]"));
        assert!(out.contains("name = \"x\""));
        assert!(out.contains("name = \"y\""));
    }

    #[test]
    fn invalid_toml() {
        assert!(format_toml("key =\n", &TomlStyle::default()).is_err());
    }

    // ------------------------------------------------------------ measurement

    #[test]
    fn the_width_budget_counts_the_separating_comma() {
        let style = with(|style| style.max_width = 22);
        // `    aaa = [1, 2, 3, 4],` is 23 columns once the comma is counted.
        assert_eq!(
            fmt_with("x = { aaa = [1, 2, 3, 4], b = 2 }\n", &style),
            "x = {\n    aaa = [\n        1,\n        2,\n        3,\n        4\n    ],\n    b = 2\n}\n"
        );
        let style = with(|style| style.max_width = 23);
        assert_eq!(
            fmt_with("x = { aaa = [1, 2, 3, 4], b = 2 }\n", &style),
            "x = {\n    aaa = [1, 2, 3, 4],\n    b = 2\n}\n"
        );
    }

    #[test]
    fn the_last_entry_owes_a_comma_only_when_one_is_written() {
        let style = TomlStyle {
            max_width: 22,
            trailing_comma: TrailingComma::Multiline,
            ..TomlStyle::default()
        };
        assert_eq!(
            fmt_with("x = { b = 2, aaa = [1, 2, 3, 4] }\n", &style),
            "x = {\n    b = 2,\n    aaa = [\n        1,\n        2,\n        3,\n        4,\n    ],\n}\n"
        );
        let style = with(|style| style.max_width = 22);
        assert_eq!(
            fmt_with("x = { b = 2, aaa = [1, 2, 3, 4] }\n", &style),
            "x = {\n    b = 2,\n    aaa = [1, 2, 3, 4]\n}\n"
        );
    }

    #[test]
    fn the_width_budget_counts_a_same_line_comment() {
        let style = with(|style| style.max_width = 20);
        assert_eq!(fmt_with("k = [1, 2, 3]\n", &style), "k = [1, 2, 3]\n");
        assert_eq!(
            fmt_with("k = [1, 2, 3] # a long trailing comment\n", &style),
            "k = [\n    1,\n    2,\n    3\n] # a long trailing comment\n"
        );
    }

    /// A comment written after the comma belongs to the container's trailing
    /// run, not to the element's own decor, but it still lands on the element's
    /// line — so reading the author's decors would leave a first pass and a
    /// second pass disagreeing about what fits.
    #[test]
    fn a_container_trailing_comment_reaches_the_last_entry() {
        let style = with(|style| style.max_width = 30);
        let source = "k = [\n    { a = 1 }, # a comment that does not fit\n]\n";
        let once = fmt_with(source, &style);
        assert_eq!(
            once,
            "k = [\n    {\n        a = 1\n    } # a comment that does not fit\n]\n"
        );
        assert_eq!(fmt_with(&once, &style), once);
        assert_eq!(
            fmt_with("k = [\n    { a = 1 }, # short\n]\n", &style),
            "k = [\n    { a = 1 } # short\n]\n"
        );
    }

    #[test]
    fn the_width_budget_counts_display_columns() {
        let style = with(|style| style.max_width = 25);
        // 20 characters wide, 32 columns wide.
        let source = "k = [\"日本語日本語日本語日本語\"]\n";
        assert_eq!(
            fmt_with(source, &style),
            "k = [\n    \"日本語日本語日本語日本語\"\n]\n"
        );
    }

    #[test]
    fn a_combining_mark_costs_nothing() {
        let style = with(|style| style.max_width = 12);
        let source = "k = [\"é́́\"]\n";
        assert_eq!(fmt_with(source, &style), source);
    }

    fn measure<'a>(source: &'a str, style: &'a TomlStyle) -> Formatter<'a> {
        Formatter {
            source,
            style,
            lookup: None,
            rust_version: None,
            spans: AHashMap::default(),
            records: RefCell::new(Vec::new()),
            leading_header: None,
            header_index: Cell::new(0),
        }
    }

    #[test]
    fn one_line_width_matches_the_rendering() {
        for arrays in [Spacing::Compact, Spacing::Spaced] {
            for inline_tables in [Spacing::Compact, Spacing::Spaced] {
                measurement_matches_rendering(arrays, inline_tables);
            }
        }
    }

    fn measurement_matches_rendering(array_spacing: Spacing, inline_table_spacing: Spacing) {
        let style = TomlStyle {
            inline_tables: InlineTableStyle::Compact,
            arrays: ArrayStyle::Auto,
            max_width: 4096,
            array_spacing,
            inline_table_spacing,
            ..TomlStyle::default()
        };
        let cases = [
            "1",
            "3.14",
            "true",
            "1979-05-27T07:32:00Z",
            "\"s\"",
            "'lit'",
            "\"ünïcödé\"",
            "[]",
            "[1, 2]",
            "[\"a\", \"b\", \"c\"]",
            "{}",
            "{ a = 1 }",
            "{ a = 1, b = 2 }",
            "{ a.b = 1 }",
            "{ \"q k\" = 1 }",
            "{ a = [1, 2], b = { c = 3 } }",
            "[{ a = 1 }, { b = 2 }]",
            "\"日本語\"",
            "{ \"版\" = [\"🦀\"] }",
        ];

        for case in cases {
            let source = format!("k = [{case}]\n");
            let doc = source.parse::<DocumentMut>().expect(case);
            let value = doc["k"].as_value().expect(case);
            let rendered = fmt_with(&source, &style);
            let rendered = rendered
                .trim_end_matches('\n')
                .strip_prefix("k = ")
                .expect(case);
            assert_eq!(
                measure(&source, &style).value_end(value, 0),
                Some(crate::toml_width::width(rendered, 4)),
                "{case} -> {rendered}"
            );
        }
    }

    #[test]
    fn a_measurement_starts_from_the_column_it_is_given() {
        let style = TomlStyle::default();
        let source = "k = [1, 2]\n";
        let doc = source.parse::<DocumentMut>().unwrap();
        let value = doc["k"].as_value().unwrap();
        assert_eq!(measure(source, &style).value_end(value, 0), Some(6));
        assert_eq!(measure(source, &style).value_end(value, 7), Some(13));
    }

    #[test]
    fn a_tab_in_a_literal_string_reaches_the_next_stop() {
        let style = TomlStyle::default();
        let source = "k = 'a\tb'\n";
        let doc = source.parse::<DocumentMut>().unwrap();
        let value = doc["k"].as_value().unwrap();
        // The tab absorbs the shift: from column 0 or 1 it reaches stop 4
        // either way, so both renderings end at the same column.
        assert_eq!(measure(source, &style).value_end(value, 0), Some(6));
        assert_eq!(measure(source, &style).value_end(value, 1), Some(6));
        assert_eq!(measure(source, &style).value_end(value, 4), Some(10));
    }

    #[test]
    fn a_value_spanning_lines_has_no_column_count() {
        let style = TomlStyle::default();
        let source = "k = \"\"\"a\nb\"\"\"\n";
        let doc = source.parse::<DocumentMut>().unwrap();
        let value = doc["k"].as_value().unwrap();
        assert_eq!(measure(source, &style).value_end(value, 0), None);
    }

    #[test]
    fn decor_newlines_do_not_hide_a_narrow_value() {
        let style = TomlStyle::default();
        let source = "k = [\n    1,\n    2\n]\n";
        let doc = source.parse::<DocumentMut>().unwrap();
        let array = doc["k"].as_array().unwrap();
        assert_eq!(measure(source, &style).array_end(array, 0), Some(6));
    }

    #[test]
    fn normalizes_crlf_and_lone_cr() {
        assert_eq!(normalize_newlines("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(normalize_newlines("plain"), "plain");
    }

    // -------------------------------------------------------- version pinning

    #[test]
    fn does_not_pin_versions_by_default() {
        let out = fmt("[dependencies]\nignore = \"0.4\"\n");
        assert!(out.contains("ignore = \"0.4\""));
    }

    #[test]
    fn pins_versions_when_lookup_provided() {
        let lookup = MapLookup(&[("ignore", "0.4", "0.4.33"), ("clap", "4.6", "4.6.6")]);
        let out = pin(
            "[dependencies]\nignore = \"0.4\"\nclap = { version = \"4.6\", features = [\"derive\"] }\n",
            &lookup,
        );
        assert!(out.contains("ignore = \"0.4.33\""));
        assert!(out.contains("version = \"4.6.6\""));
        assert!(out.contains("features = [\"derive\"]"));
    }

    #[test]
    fn pinning_writes_resolve_result_verbatim() {
        let lookup = MapLookup(&[("clap", "^4.6", "^4.6.6")]);
        let out = pin(
            "[dependencies]\nclap = { version = \"^4.6\", features = [\"derive\"] }\n",
            &lookup,
        );
        assert!(out.contains("version = \"^4.6.6\""));
    }

    #[test]
    fn pins_table_form_dependencies() {
        let lookup = MapLookup(&[
            ("serde", "1.0", "1.0.230"),
            ("tokio", "1", "1.40.0"),
            ("nix-real", "0.2", "0.2.9"),
        ]);
        let out = pin(
            "[dependencies.serde]\nversion = \"1.0\"\n\n[workspace.dependencies.tokio]\nversion = \"1\"\n\n[target.'cfg(unix)'.dependencies.nix]\npackage = \"nix-real\"\nversion = \"0.2\"\n",
            &lookup,
        );
        assert!(out.contains("[dependencies.serde]"));
        assert!(out.contains("version = \"1.0.230\""));
        assert!(out.contains("[workspace.dependencies.tokio]"));
        assert!(out.contains("version = \"1.40.0\""));
        assert!(out.contains("[target.'cfg(unix)'.dependencies.nix]"));
        assert!(out.contains("package = \"nix-real\""));
        assert!(out.contains("version = \"0.2.9\""));
    }

    #[test]
    fn table_form_path_dep_without_version_is_unchanged() {
        let lookup = MapLookup(&[("foo", "1.0", "1.0.0")]);
        let out = pin("[dependencies.foo]\npath = \"../foo\"\n", &lookup);
        assert_eq!(out, "[dependencies.foo]\npath = \"../foo\"\n");
    }

    #[test]
    fn path_dep_with_version_is_unchanged() {
        let lookup = MapLookup(&[("local", "0.1", "0.1.9")]);
        let out = pin(
            "[dependencies]\nlocal = { path = \"../local\", version = \"0.1\" }\n",
            &lookup,
        );
        assert_eq!(
            out,
            "[dependencies]\nlocal = { path = \"../local\", version = \"0.1\" }\n"
        );
    }

    #[test]
    fn git_dep_with_version_is_unchanged() {
        let lookup = MapLookup(&[("bar", "1.0", "1.0.9")]);
        let out = pin(
            "[dependencies]\nbar = { git = \"https://github.com/foo/bar\", version = \"1.0\" }\n",
            &lookup,
        );
        assert_eq!(
            out,
            "[dependencies]\nbar = { git = \"https://github.com/foo/bar\", version = \"1.0\" }\n"
        );
    }

    #[test]
    fn registry_dep_with_version_is_unchanged() {
        let lookup = MapLookup(&[("serde", "1.0", "1.0.230")]);
        let out = pin(
            "[dependencies]\nserde = { version = \"1.0\", registry = \"private\" }\n",
            &lookup,
        );
        assert_eq!(
            out,
            "[dependencies]\nserde = { version = \"1.0\", registry = \"private\" }\n"
        );
    }

    #[test]
    fn table_form_path_dep_with_version_is_unchanged() {
        let lookup = MapLookup(&[("foo", "0.1", "0.1.9")]);
        let out = pin(
            "[dependencies.foo]\npath = \"../foo\"\nversion = \"0.1\"\n",
            &lookup,
        );
        assert_eq!(
            out,
            "[dependencies.foo]\npath = \"../foo\"\nversion = \"0.1\"\n"
        );
    }

    #[test]
    fn pinning_keeps_trailing_comment() {
        let lookup = MapLookup(&[("ignore", "0.4", "0.4.33")]);
        let out = pin("[dependencies]\nignore = \"0.4\" # keep\n", &lookup);
        assert_eq!(out, "[dependencies]\nignore = \"0.4.33\" # keep\n");
    }

    #[test]
    fn collects_every_dependency_request() {
        let doc = "[dependencies]\nignore = \"0.4\"\nclap = { version = \"4.6\" }\nlocal.path = \"x\"\n\n[dependencies.serde]\nversion = \"1.0\"\n\n[workspace.dependencies.tokio]\nversion = \"1\"\n\n[target.'cfg(unix)'.dev-dependencies]\nnix = { package = \"nix-real\", version = \"0.2\" }\n\n[target.'cfg(unix)'.dependencies.libc]\npackage = \"libc-real\"\nversion = \"0.2\"\n"
            .parse::<DocumentMut>()
            .unwrap();

        let mut requests = Vec::new();
        collect_dep_requests(doc.as_table(), Scope::ROOT, Guard::of(None), &mut requests);

        assert_eq!(
            requests
                .iter()
                .map(|request| (request.name, request.req))
                .collect::<Vec<_>>(),
            vec![
                ("ignore", "0.4"),
                ("clap", "4.6"),
                ("serde", "1.0"),
                ("tokio", "1"),
                ("nix-real", "0.2"),
                ("libc-real", "0.2"),
            ]
        );
    }

    #[test]
    fn skips_path_git_registry_dep_requests() {
        let doc = "[dependencies]\nlocal = { path = \"../local\", version = \"0.1\" }\nbar = { git = \"https://github.com/foo/bar\", version = \"1.0\" }\nserde = { version = \"1.0\", registry = \"private\" }\nignore = \"0.4\"\n\n[dependencies.foo]\npath = \"../foo\"\nversion = \"0.1\"\n"
            .parse::<DocumentMut>()
            .unwrap();

        let mut requests = Vec::new();
        collect_dep_requests(doc.as_table(), Scope::ROOT, Guard::of(None), &mut requests);

        assert_eq!(
            requests
                .iter()
                .map(|request| (request.name, request.req))
                .collect::<Vec<_>>(),
            vec![("ignore", "0.4")]
        );
    }

    // ------------------------------------------------------------- directives

    /// Every knob that moves a line, so a region that survives this survives
    /// anything.
    fn loud() -> TomlStyle {
        TomlStyle {
            indent: TomlIndent::tab(),
            max_width: 40,
            arrays: ArrayStyle::Expand,
            inline_tables: InlineTableStyle::Expand,
            trailing_comma: TrailingComma::Multiline,
            blank_line_before_tables: true,
            max_blank_lines: 0,
            array_spacing: Spacing::Spaced,
            inline_table_spacing: Spacing::Compact,
            align_entries: true,
            align_comments: true,
            indent_tables: true,
            indent_entries: true,
            normalize_keys: true,
            sort_deps: true,
            sort_package: true,
            sort_dep_fields: true,
            sort_features: true,
            sort_arrays: true,
            sort_targets: true,
            sort_tables: true,
            sort_keys: true,
            ..TomlStyle::default()
        }
    }

    /// The bytes each directive region covers, read straight out of the text.
    fn regions(text: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut open = None;
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let end = offset + line.len();
            match line.trim() {
                "# fmt: off" if open.is_none() => open = Some(offset),
                "# fmt: on" => {
                    if let Some(start) = open.take() {
                        found.push(text[start..end].to_owned());
                    }
                }
                _ => {}
            }
            offset = end;
        }
        if let Some(start) = open {
            found.push(text[start..].to_owned());
        }
        found
    }

    fn survives(input: &str, style: &TomlStyle) -> String {
        let once = fmt_with(input, style);
        assert_eq!(regions(input), regions(&once), "region text changed");
        assert_eq!(once, fmt_with(&once, style), "not a fixed point");
        once
    }

    #[test]
    fn a_frozen_entry_keeps_its_spelling() {
        let input = "a=1\n# fmt: off\nb   =   2\n# fmt: on\nc=3\n";
        assert_eq!(
            survives(input, &TomlStyle::default()),
            "a = 1\n# fmt: off\nb   =   2\n# fmt: on\nc = 3\n"
        );
    }

    #[test]
    fn a_hand_aligned_block_survives_every_knob() {
        let input = concat!(
            "[package]\n",
            "name = \"x\"\n",
            "# fmt: off\n",
            "matrix = [\n",
            "  1,   2,   3,\n",
            "  40,  50,  60,\n",
            "]\n",
            "#   a   hand   aligned   note\n",
            "# fmt: on\n",
            "version = \"1\"\n",
        );
        let out = survives(input, &loud());
        assert!(out.contains("  40,  50,  60,\n"), "{out}");
        assert!(out.contains("#   a   hand   aligned   note\n"), "{out}");
    }

    #[test]
    fn a_frozen_header_keeps_its_brackets_and_its_body() {
        let input = "[a]\nx = 1\n\n# fmt: off\n[  b  ]\ny   =   2\nz=3\n# fmt: on\n\n[c]\nw = 4\n";
        let out = survives(input, &loud());
        assert!(out.contains("[  b  ]\ny   =   2\nz=3\n"), "{out}");
    }

    #[test]
    fn a_whole_document_can_be_frozen() {
        let input = "# fmt: off\n[a]\nx   =   1\n\n\n[b]\ny=2\n";
        assert_eq!(survives(input, &loud()), input);
    }

    #[test]
    fn a_region_in_the_document_trailer_survives() {
        let input = "a=1\n# fmt: off\n#   a   trailing   note\n# fmt: on\n";
        assert_eq!(
            survives(input, &loud()),
            "a = 1\n# fmt: off\n#   a   trailing   note\n# fmt: on\n"
        );
    }

    #[test]
    fn a_marker_inside_an_array_freezes_the_whole_entry() {
        let input = "a = [\n  1,\n  # fmt: off\n  2,   3,\n  # fmt: on\n  4,\n]\n";
        assert_eq!(survives(input, &loud()), input);
    }

    #[test]
    fn a_frozen_dotted_line_keeps_its_padding() {
        let input = "x = 1\n# fmt: off\na . b   =   2\n# fmt: on\ny = 3\n";
        let out = survives(input, &loud());
        assert!(out.contains("a . b   =   2\n"), "{out}");
    }

    #[test]
    fn a_frozen_dependency_is_not_pinned() {
        let lookup = MapLookup(&[("serde", "1.0", "1.0.230"), ("tokio", "1.0", "1.0.3")]);
        let input = "[dependencies]\n# fmt: off\nserde = \"1.0\"\n# fmt: on\ntokio = \"1.0\"\n";
        let out = pin(input, &lookup);
        assert!(out.contains("serde = \"1.0\"\n"), "{out}");
        assert!(out.contains("tokio = \"1.0.3\"\n"), "{out}");
    }

    #[test]
    fn a_frozen_dependency_section_is_not_pinned() {
        let lookup = MapLookup(&[("serde", "1.0", "1.0.230")]);
        let input = "[dependencies.serde]\n# fmt: off\nversion = \"1.0\"\n# fmt: on\n";
        let out = pin(input, &lookup);
        assert_eq!(out, input);
    }

    #[test]
    fn a_record_names_the_section_the_dependency_lives_in() {
        let lookup = MapLookup(&[("serde", "1.0", "1.0.230"), ("nix-real", "0.2", "0.2.7")]);
        let records = pin_records(
            "[dependencies]\nserde = \"1.0\"\n\n\
             [target.'cfg(unix)'.dev-dependencies]\nnix = { package = \"nix-real\", version = \"0.2\" }\n",
            &lookup,
        );

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].crate_name, "serde");
        assert_eq!(records[0].section, "dependencies");
        assert_eq!(records[0].requirement, "1.0");
        assert_eq!(records[0].line, Some(2));
        assert_eq!(records[0].column, Some(1));
        assert_eq!(records[0].outcome, Resolution::Pinned("1.0.230".to_owned()));

        assert_eq!(records[1].crate_name, "nix-real");
        assert_eq!(records[1].section, "target.cfg(unix).dev-dependencies");
    }

    #[test]
    fn a_patch_or_replace_entry_is_recorded_but_never_pinned() {
        let lookup = MapLookup(&[("serde", "1.0", "1.0.230")]);
        let input = "[patch.crates-io]\nserde = { git = \"https://example.com/s\", version = \"1.0\" }\n\n\
                     [replace]\n\"old:1.0.0\" = { path = \"../old\", version = \"1.0\" }\n";
        let records = pin_records(input, &lookup);

        assert_eq!(
            records
                .iter()
                .map(|record| record.outcome.clone())
                .collect::<Vec<_>>(),
            vec![
                Resolution::Skipped(SkipReason::PatchSection),
                Resolution::Skipped(SkipReason::ReplaceSection),
            ]
        );
        assert!(pin(input, &lookup).contains("version = \"1.0\""));
    }

    #[test]
    fn a_registry_index_dependency_is_another_source() {
        let lookup = MapLookup(&[("serde", "1.0", "1.0.230")]);
        let input =
            "[dependencies]\nserde = { version = \"1.0\", registry-index = \"https://x/i\" }\n";
        let records = pin_records(input, &lookup);

        assert_eq!(
            records[0].outcome,
            Resolution::Skipped(SkipReason::AlternateRegistry)
        );
        assert!(pin(input, &lookup).contains("version = \"1.0\""));
    }

    #[test]
    fn an_inherited_or_frozen_dependency_is_accounted_for() {
        let lookup = MapLookup(&[("serde", "1.0", "1.0.230")]);
        let records = pin_records(
            "[dependencies]\nshared = { workspace = true, features = [\"a\"] }\n\
             # fmt: off\nserde = \"1.0\"\n# fmt: on\n",
            &lookup,
        );

        assert_eq!(
            records
                .iter()
                .map(|record| (record.crate_name.clone(), record.outcome.clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "shared".to_owned(),
                    Resolution::Skipped(SkipReason::WorkspaceInherited)
                ),
                ("serde".to_owned(), Resolution::Skipped(SkipReason::Frozen)),
            ]
        );
    }

    #[test]
    fn a_requirement_already_at_the_newest_release_is_unchanged() {
        let lookup = MapLookup(&[("serde", "1.0.230", "1.0.230")]);
        let records = pin_records("[dependencies]\nserde = \"1.0.230\"\n", &lookup);
        assert_eq!(records[0].outcome, Resolution::Unchanged);
    }

    #[test]
    fn a_rust_version_is_read_from_the_package_or_the_workspace() {
        let doc = |source: &str| source.parse::<DocumentMut>().unwrap();
        let none = ManifestContext::default();
        let inherited = ManifestContext {
            workspace_rust_version: PartialVersion::parse("1.70"),
        };

        assert_eq!(
            manifest_rust_version(
                doc("[package]\nrust-version = \"1.63\"\n").as_table(),
                &none
            ),
            PartialVersion::parse("1.63")
        );
        assert_eq!(
            manifest_rust_version(
                doc("[package]\nrust-version.workspace = true\n").as_table(),
                &inherited
            ),
            PartialVersion::parse("1.70")
        );
        assert_eq!(
            manifest_rust_version(
                doc("[workspace.package]\nrust-version = \"1.65\"\n").as_table(),
                &none
            ),
            PartialVersion::parse("1.65")
        );
        assert_eq!(
            manifest_rust_version(doc("[package]\nname = \"x\"\n").as_table(), &none),
            None
        );
    }

    #[test]
    fn a_frozen_table_is_not_promoted_to_a_section() {
        let style = with(|style| {
            style.inline_tables = InlineTableStyle::Section;
            style.max_width = 20;
        });
        let input = "# fmt: off\nwide = { alpha = 1, beta = 2, gamma = 3 }\n# fmt: on\n";
        assert_eq!(survives(input, &style), input);
    }

    #[test]
    fn markers_are_ordinary_comments_when_directives_are_off() {
        let style = with(|style| {
            style.directives = false;
            style.align_entries = true;
        });
        assert_eq!(
            fmt_with("# fmt: off\na=1\nbbb=2\n# fmt: on\n", &style),
            "# fmt: off\na   = 1\nbbb = 2\n# fmt: on\n"
        );
    }

    #[test]
    fn an_unterminated_marker_freezes_the_rest_of_the_document() {
        let input = "a=1\n# fmt: off\nb   =   2\nc=3\n";
        assert_eq!(
            survives(input, &loud()),
            "a = 1\n# fmt: off\nb   =   2\nc=3\n"
        );
    }

    #[test]
    fn sorting_runs_up_to_a_frozen_entry_and_starts_again_after_it() {
        let style = with(|style| style.sort_deps = true);
        let input = concat!(
            "[dependencies]\n",
            "zzz = \"1\"\n",
            "bbb = \"1\"\n",
            "# fmt: off\n",
            "kkk = \"1\"\n",
            "# fmt: on\n",
            "yyy = \"1\"\n",
            "ddd = \"1\"\n",
            "aaa = \"1\"\n",
        );
        assert_eq!(
            survives(input, &style),
            concat!(
                "[dependencies]\n",
                "bbb = \"1\"\n",
                "zzz = \"1\"\n",
                "# fmt: off\n",
                "kkk = \"1\"\n",
                "# fmt: on\n",
                "yyy = \"1\"\n",
                "aaa = \"1\"\n",
                "ddd = \"1\"\n",
            )
        );
    }

    #[test]
    fn a_frozen_section_is_not_carried_off_by_table_ordering() {
        let style = with(|style| style.sort_tables = true);
        let input =
            "[dev-dependencies]\nx = \"1\"\n\n# fmt: off\n[dependencies]\ny = \"1\"\n# fmt: on\n";
        assert_eq!(survives(input, &style), input);
    }

    #[test]
    fn a_frozen_target_element_pins_the_array_it_is_in() {
        let style = with(|style| style.sort_targets = true);
        let input = concat!(
            "[[bin]]\n",
            "name = \"z\"\n",
            "\n",
            "# fmt: off\n",
            "[[bin]]\n",
            "name = \"a\"\n",
            "# fmt: on\n",
        );
        assert_eq!(survives(input, &style), input);
    }

    /// A table's positions are not always in map order — a document may write
    /// `[a.b]` above `[a]` — so the slots a permutation redistributes have to
    /// leave a walled entry's own subtree alone.
    #[test]
    fn a_wall_keeps_its_position_when_the_table_around_it_sorts() {
        let style = with(|style| style.sort_deps = true);
        let input = concat!(
            "[dependencies]\n",
            "zzz = \"1\"\n",
            "yyy = \"1\"\n",
            "\n",
            "[dependencies.a.sub]\n",
            "q = 1\n",
            "\n",
            "[dependencies.c]\n",
            "r = 1\n",
            "\n",
            "[dependencies.a]\n",
            "# fmt: off\n",
            "s  =  1\n",
            "# fmt: on\n",
        );
        let out = survives(input, &style);
        assert!(out.contains("yyy = \"1\"\nzzz = \"1\"\n"), "{out}");
        assert!(
            out.ends_with("[dependencies.a]\n# fmt: off\ns  =  1\n# fmt: on\n"),
            "{out}"
        );
    }

    /// A promoted section is written after every body line of the table it came
    /// out of, which for an unclosed region is inside it.
    #[test]
    fn a_table_holding_a_directive_promotes_nothing() {
        let style = with(|style| {
            style.inline_tables = InlineTableStyle::Section;
            style.max_width = 20;
        });
        let input = concat!(
            "[a]\n",
            "wide = { alpha = 1, beta = 2, gamma = 3 }\n",
            "# fmt: off\n",
            "k  =  1\n",
            "# fmt: on\n",
        );
        let out = survives(input, &style);
        assert!(!out.contains("[a.wide]"), "{out}");
    }

    #[test]
    fn a_dotted_key_shared_with_a_frozen_line_keeps_its_padding() {
        let input = "[a]\nb .x = [\n  # fmt: off\n  1,\n]\nb .y = 2\n";
        let out = survives(input, &loud());
        assert!(out.contains("b .y = 2\n"), "{out}");
    }

    #[test]
    fn a_header_key_shared_with_a_frozen_header_keeps_its_padding() {
        let input = "[\"with space\" .a]\n# fmt: off\n[\"with space\" .b]\nk = 1\n";
        let out = survives(input, &loud());
        assert!(out.contains("[\"with space\" .b]\n"), "{out}");
    }

    /// `toml_edit` regroups interleaved dotted keys as it renders, which can
    /// carry one marker past another and leave the second pass reading a
    /// different set of regions than the first.
    #[test]
    fn a_regrouped_document_still_settles_in_one_pass() {
        let input = concat!(
            "\"dot\".a = 1\n",
            "b = [\n",
            "  # fmt: off\n",
            "  2,\n",
            "]\n",
            "\"dot\".c = [\n",
            "  # fmt: on\n",
            "  3,\n",
            "]\n",
        );
        let once = fmt_with(input, &loud());
        assert_eq!(fmt_with(&once, &loud()), once, "not a fixed point:\n{once}");
    }

    /// Only the marked lines are the author's: the blank lines above the `off`
    /// marker and the comment block below the `on` marker are still the
    /// formatter's, and so is the indent of the entry the run introduces.
    #[test]
    fn a_directive_freezes_the_run_it_sits_in() {
        let style = with(|style| {
            style.indent_entries = true;
            style.max_blank_lines = 0;
        });
        let input = "[a]\nx = 1\n\n\n# fmt: off\ny  =  2\n# fmt: on\n\n\n# a note\nz = 3\n";
        assert_eq!(
            survives(input, &style),
            "[a]\n    x = 1\n# fmt: off\ny  =  2\n# fmt: on\n    # a note\n    z = 3\n"
        );
    }

    /// Line terminators are normalized for the whole document, a frozen region
    /// included; the file path puts CRLF back on the way out.
    #[test]
    fn a_frozen_region_takes_the_document_line_ending() {
        assert_eq!(
            fmt("a=1\r\n# fmt: off\r\nb   =   2\r\n# fmt: on\r\n"),
            "a = 1\n# fmt: off\nb   =   2\n# fmt: on\n"
        );
    }

    #[test]
    fn a_frozen_array_of_tables_element_keeps_its_order_and_its_body() {
        let input = concat!(
            "[[bin]]\n",
            "name = \"z\"\n",
            "\n",
            "# fmt: off\n",
            "[[bin]]\n",
            "name   =   \"a\"\n",
            "# fmt: on\n",
        );
        let out = survives(input, &loud());
        assert!(out.contains("name   =   \"a\""), "{out}");
    }
}
