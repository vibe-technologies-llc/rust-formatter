use std::{collections::BTreeSet, ops::Range};

use syn::{
    AttrStyle, Attribute, Meta, Path, Stmt, Token,
    parse::Parser,
    punctuated::Punctuated,
    spanned::Spanned,
    visit::{self, Visit},
};

use super::{
    skip::{contains_skip, guarded, skipped},
    text::{byte_range, line_starts},
};

const STD_DERIVES: [&str; 9] = [
    "Clone",
    "Copy",
    "Debug",
    "Default",
    "Hash",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
];
const STD_ROOTS: [&str; 2] = ["std", "core"];
const LINT_LEVELS: [&str; 5] = ["allow", "warn", "deny", "forbid", "expect"];
const MOVABLE: [&str; 7] = [
    "must_use",
    "inline",
    "cold",
    "track_caller",
    "non_exhaustive",
    "deprecated",
    "repr",
];

pub fn apply(text: &str) -> String {
    let replacements = replacements(text);

    proc_macro2::extra::invalidate_current_thread_spans();

    splice(text, replacements)
}

fn splice(text: &str, mut replacements: Vec<(Range<usize>, String)>) -> String {
    let mut out = text.to_owned();

    replacements.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    for (range, replacement) in replacements {
        out.replace_range(range, &replacement);
    }
    out
}

#[derive(Clone, PartialEq, Eq)]
enum Role {
    Lint(String),
    Movable,
    Fixed,
}

struct Placed {
    range: Range<usize>,
    lines: Option<Range<usize>>,
    first_line: usize,
    last_line: usize,
    inner: bool,
    role: Role,
    sorted: String,
}

impl Placed {
    fn changed(&self, text: &str) -> bool {
        self.sorted != text[self.range.clone()]
    }

    fn block(&self, text: &str) -> Option<String> {
        let lines = self.lines.clone()?;
        Some(format!(
            "{}{}{}",
            &text[lines.start..self.range.start],
            self.sorted,
            &text[self.range.end..lines.end]
        ))
    }
}

fn replacements(text: &str) -> Vec<(Range<usize>, String)> {
    let Ok(file) = syn::parse_file(text) else {
        return Vec::new();
    };

    if skipped(&file.attrs) {
        return Vec::new();
    }

    let starts = line_starts(text);
    let mut collector = Collector::default();

    collector.visit_file(&file);

    let mut placed: Vec<Placed> = collector
        .found
        .iter()
        .filter_map(|attr| place(attr, text, &starts))
        .filter(|attr| !collector.is_protected(attr.first_line))
        .collect();

    placed.sort_by_key(|attr| attr.range.start);
    placed.dedup_by_key(|attr| attr.range.start);

    let mut replacements = Vec::new();
    let mut moved = BTreeSet::new();

    for run in runs(&placed) {
        if let Some(replacement) = reordered(&placed[run.clone()], text) {
            replacements.push(replacement);
            moved.extend(run);
        }
    }
    for (index, attr) in placed.iter().enumerate() {
        if !moved.contains(&index) && attr.changed(text) {
            replacements.push((attr.range.clone(), attr.sorted.clone()));
        }
    }
    replacements
}

fn runs(placed: &[Placed]) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start = 0;

    for index in 1..=placed.len() {
        let continues = placed.get(index).is_some_and(|next| {
            let previous = &placed[index - 1];
            movable(previous)
                && movable(next)
                && next.inner == previous.inner
                && next.first_line == previous.last_line + 1
        });
        if !continues {
            if index - start >= 2 && movable(&placed[start]) {
                runs.push(start..index);
            }
            start = index;
        }
    }
    runs
}

fn movable(attr: &Placed) -> bool {
    attr.lines.is_some() && attr.role != Role::Fixed
}

fn reordered(run: &[Placed], text: &str) -> Option<(Range<usize>, String)> {
    let levels: BTreeSet<&str> = run
        .iter()
        .filter_map(|attr| match &attr.role {
            Role::Lint(level) => Some(level.as_str()),
            Role::Movable | Role::Fixed => None,
        })
        .collect();

    if levels.len() > 1 {
        return None;
    }

    let mut order: Vec<usize> = (0..run.len()).collect();

    order.sort_by(|&left, &right| {
        let key = |index: usize| (run[index].sorted.chars().count(), &run[index].sorted);
        key(left).cmp(&key(right))
    });
    if order.iter().copied().eq(0..run.len()) {
        return None;
    }

    let blocks: Option<String> = order.iter().map(|&index| run[index].block(text)).collect();
    let start = run.first()?.lines.as_ref()?.start;
    let end = run.last()?.lines.as_ref()?.end;
    Some((start..end, blocks?))
}

fn place(attr: &Attribute, text: &str, starts: &[usize]) -> Option<Placed> {
    let span = attr.span();
    let range = byte_range(span, text, starts);
    let first_line = span.start().line;
    let last_line = span.end().line;

    let line_start = *starts.get(first_line.checked_sub(1)?)?;
    let line_end = text[range.end..].find('\n').map(|at| range.end + at);

    let whole_line = text[line_start..range.start].trim().is_empty()
        && line_end.is_some_and(|end| text[range.end..end].trim().is_empty());
    let lines = whole_line
        .then_some(line_end)
        .flatten()
        .map(|end| line_start..end + 1);

    let sorted = sorted_lists(attr, &text[range.clone()], range.start, text, starts)
        .unwrap_or_else(|| text[range.clone()].to_owned());
    Some(Placed {
        range,
        lines,
        first_line,
        last_line,
        inner: matches!(attr.style, AttrStyle::Inner(_)),
        role: role(attr),
        sorted,
    })
}

fn role(attr: &Attribute) -> Role {
    let Some(name) = attr.path().get_ident().map(ToString::to_string) else {
        return Role::Fixed;
    };
    if LINT_LEVELS.contains(&name.as_str()) {
        Role::Lint(name)
    } else if MOVABLE.contains(&name.as_str()) {
        Role::Movable
    } else {
        Role::Fixed
    }
}

struct Slot {
    range: Range<usize>,
    key: (usize, String, String),
}

fn sorted_lists(
    attr: &Attribute,
    own: &str,
    origin: usize,
    text: &str,
    starts: &[usize],
) -> Option<String> {
    let slots = list_slots(attr, text, starts)?;

    if slots.len() < 2 || !separated_by_commas(&slots, text) {
        return None;
    }

    let mut order: Vec<&Slot> = slots.iter().collect();

    order.sort_by(|left, right| left.key.cmp(&right.key));
    if order
        .iter()
        .zip(&slots)
        .all(|(sorted, slot)| sorted.range == slot.range)
    {
        return None;
    }

    let mut out = own.to_owned();

    for (slot, replacement) in slots.iter().zip(&order).rev() {
        let local = slot.range.start - origin..slot.range.end - origin;

        out.replace_range(local, &text[replacement.range.clone()]);
    }
    Some(out)
}

fn list_slots(attr: &Attribute, text: &str, starts: &[usize]) -> Option<Vec<Slot>> {
    let Meta::List(list) = &attr.meta else {
        return None;
    };
    let slot = |span: proc_macro2::Span, key: (usize, String, String)| {
        let range = byte_range(span, text, starts);
        let item = &text[range.clone()];
        (!item.contains("//") && !item.contains("/*")).then_some(Slot { range, key })
    };

    if attr.path().is_ident("derive") {
        let paths = Punctuated::<Path, Token![,]>::parse_terminated
            .parse2(list.tokens.clone())
            .ok()?;

        return paths
            .iter()
            .map(|path| slot(path.span(), derive_key(path)))
            .collect();
    }

    if !matches!(role(attr), Role::Lint(_)) {
        return None;
    }

    let entries = Punctuated::<Meta, Token![,]>::parse_terminated
        .parse2(list.tokens.clone())
        .ok()?;
    let mut slots = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        match entry {
            Meta::Path(path) => slots.push(slot(path.span(), (0, compact(path), String::new()))?),
            Meta::NameValue(reason)
                if reason.path.is_ident("reason") && index + 1 == entries.len() => {}
            Meta::NameValue(_) | Meta::List(_) => return None,
        }
    }
    Some(slots)
}

fn separated_by_commas(slots: &[Slot], text: &str) -> bool {
    slots
        .windows(2)
        .all(|pair| text[pair[0].range.end..pair[1].range.start].trim() == ",")
}

fn derive_key(path: &Path) -> (usize, String, String) {
    let segments: Vec<String> = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    let last = segments.last().cloned().unwrap_or_default();

    let from_std = match segments.as_slice() {
        [_] => path.leading_colon.is_none(),
        [root, ..] => STD_ROOTS.contains(&root.as_str()),
        [] => false,
    };
    let rank = STD_DERIVES
        .iter()
        .position(|derive| *derive == last)
        .filter(|_| from_std);

    match rank {
        Some(rank) => (rank, String::new(), String::new()),
        None => (STD_DERIVES.len(), last, compact(path)),
    }
}

fn compact(path: &Path) -> String {
    let mut out = String::new();

    if path.leading_colon.is_some() {
        out.push_str("::");
    }
    for (index, segment) in path.segments.iter().enumerate() {
        if index > 0 {
            out.push_str("::");
        }
        out.push_str(&segment.ident.to_string());
    }
    out
}

#[derive(Default)]
struct Collector<'ast> {
    found: Vec<&'ast Attribute>,
    protected: Vec<Range<usize>>,
}

impl Collector<'_> {
    fn protect(&mut self, span: proc_macro2::Span) {
        self.protected.push(span.start().line..span.end().line + 1);
    }

    fn is_protected(&self, line: usize) -> bool {
        self.protected.iter().any(|lines| lines.contains(&line))
    }
}

impl<'ast> Visit<'ast> for Collector<'ast> {
    guarded!();

    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        self.found.push(attr);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if !contains_skip(stmt) {
            visit::visit_stmt(self, stmt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::apply;

    fn assert_sorted(source: &str, expected: &str) {
        let once = apply(source);

        assert_eq!(once, expected);
        assert_eq!(apply(&once), once);
    }

    fn assert_untouched(source: &str) {
        assert_eq!(apply(source), source);
    }

    #[test]
    fn std_derives_follow_their_families() {
        assert_sorted(
            "#[derive(Ord, Eq, Debug, PartialOrd, Copy, Hash, Default, PartialEq, Clone)]\nstruct S;\n",
            "#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]\nstruct S;\n",
        );
    }

    #[test]
    fn other_derives_follow_alphabetically() {
        assert_sorted(
            "#[derive(Serialize, Debug, serde::Deserialize, Clone, thiserror::Error)]\nstruct S;\n",
            "#[derive(Clone, Debug, serde::Deserialize, thiserror::Error, Serialize)]\nstruct S;\n",
        );
    }

    #[test]
    fn std_paths_count_as_std() {
        assert_sorted(
            "#[derive(core::fmt::Debug, std::clone::Clone)]\nstruct S;\n",
            "#[derive(std::clone::Clone, core::fmt::Debug)]\nstruct S;\n",
        );
    }

    #[test]
    fn a_vertical_derive_keeps_its_layout() {
        assert_sorted(
            "#[derive(\n    Debug,\n    Clone,\n    PartialEq,\n)]\nstruct S;\n",
            "#[derive(\n    Clone,\n    Debug,\n    PartialEq,\n)]\nstruct S;\n",
        );
    }

    #[test]
    fn a_comment_inside_a_derive_leaves_it_alone() {
        assert_untouched("#[derive(Debug, /* why */ Clone)]\nstruct S;\n");
        assert_untouched("#[derive(\n    Debug,\n    // why\n    Clone,\n)]\nstruct S;\n");
    }

    #[test]
    fn lint_lists_are_sorted() {
        assert_sorted(
            "#[allow(dead_code, clippy::too_many_lines, unused)]\nfn f() {}\n",
            "#[allow(clippy::too_many_lines, dead_code, unused)]\nfn f() {}\n",
        );
        assert_sorted(
            "#![deny(warnings, clippy::all)]\n",
            "#![deny(clippy::all, warnings)]\n",
        );
    }

    #[test]
    fn a_lint_reason_stays_last() {
        assert_sorted(
            "#[expect(unused, dead_code, reason = \"later\")]\nfn f() {}\n",
            "#[expect(dead_code, unused, reason = \"later\")]\nfn f() {}\n",
        );
    }

    #[test]
    fn unknown_lists_are_left_alone() {
        assert_untouched("#[serde(rename_all = \"kebab-case\", deny_unknown_fields)]\nstruct S;\n");
        assert_untouched("#[cfg_attr(test, derive(Debug, Clone))]\nstruct S;\n");
        assert_untouched("#[repr(u8, C)]\nenum E { A }\n");
    }

    #[test]
    fn movable_attributes_go_shortest_first() {
        assert_sorted(
            "#[must_use]\n#[inline]\npub fn f() -> u8 {\n    1\n}\n",
            "#[inline]\n#[must_use]\npub fn f() -> u8 {\n    1\n}\n",
        );
        assert_sorted(
            "#[deprecated(note = \"use g\")]\n#[track_caller]\n#[cold]\nfn f() {}\n",
            "#[cold]\n#[track_caller]\n#[deprecated(note = \"use g\")]\nfn f() {}\n",
        );
    }

    #[test]
    fn lint_attributes_of_one_level_move_together() {
        assert_sorted(
            "#[allow(clippy::too_many_lines)]\n#[allow(dead_code)]\n#[inline]\nfn f() {}\n",
            "#[inline]\n#[allow(dead_code)]\n#[allow(clippy::too_many_lines)]\nfn f() {}\n",
        );
    }

    #[test]
    fn mixed_lint_levels_keep_their_order() {
        assert_untouched("#[deny(warnings)]\n#[allow(dead_code)]\nfn f() {}\n");
        assert_untouched("#![forbid(unsafe_code)]\n#![allow(dead_code)]\n");
    }

    #[test]
    fn fixed_attributes_split_runs_and_never_move() {
        assert_sorted(
            "/// Doc.\n#[derive(Debug)]\n#[non_exhaustive]\n#[must_use]\n#[cfg(test)]\n#[must_use]\n#[inline]\nstruct S;\n",
            "/// Doc.\n#[derive(Debug)]\n#[must_use]\n#[non_exhaustive]\n#[cfg(test)]\n#[inline]\n#[must_use]\nstruct S;\n",
        );
        assert_untouched("#[must_use]\n#[serde(default)]\n#[inline]\nfn f() {}\n");
        assert_untouched("#[must_use]\n#[test]\n#[inline]\nfn f() {}\n");
    }

    #[test]
    fn a_comment_between_attributes_splits_the_run() {
        assert_untouched("#[must_use]\n// why\n#[inline]\nfn f() {}\n");
    }

    #[test]
    fn attributes_sharing_a_line_do_not_move() {
        assert_untouched("#[must_use] #[inline]\nfn f() {}\n");
    }

    #[test]
    fn inner_and_outer_attributes_do_not_mix() {
        assert_untouched("mod m {\n    #![allow(dead_code)]\n}\n");
    }

    #[test]
    fn fields_and_variants_are_reached() {
        assert_sorted(
            "struct S {\n    #[allow(b, a)]\n    x: u8,\n}\nenum E {\n    #[allow(d, c)]\n    A,\n}\n",
            "struct S {\n    #[allow(a, b)]\n    x: u8,\n}\nenum E {\n    #[allow(c, d)]\n    A,\n}\n",
        );
    }

    #[test]
    fn a_run_whose_lists_change_carries_the_sorted_lists() {
        assert_sorted(
            "#[allow(unused, dead_code)]\n#[inline]\nfn f() {}\n",
            "#[inline]\n#[allow(dead_code, unused)]\nfn f() {}\n",
        );
    }

    #[test]
    fn rustfmt_skip_is_honoured() {
        assert_untouched("#[rustfmt::skip]\n#[derive(Debug, Clone)]\nstruct S;\n");
        assert_untouched("#[derive(Debug, Clone)]\n#[rustfmt::skip]\nstruct S;\n");
        assert_untouched("#![rustfmt::skip]\n#[derive(Debug, Clone)]\nstruct S;\n");
        assert_untouched(
            "#[rustfmt::skip]\nmod m {\n    #[derive(Debug, Clone)]\n    struct S;\n}\n",
        );
        assert_untouched("fn f() {\n    #[rustfmt::skip]\n    #[allow(b, a)]\n    let x = 1;\n}\n");
    }

    #[test]
    fn macro_bodies_are_not_entered() {
        assert_untouched("m! {\n    #[derive(Debug, Clone)]\n    struct S;\n}\n");
    }

    #[test]
    fn unparseable_source_is_left_alone() {
        assert_untouched("#[derive(Debug, Clone)]\nstruct S\n");
    }
}
