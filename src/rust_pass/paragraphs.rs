use std::{collections::BTreeSet, ops::Range};

use proc_macro2::{LineColumn, Span, TokenStream, TokenTree};
use syn::{
    Block, Expr, Item, Local, Pat, Stmt,
    spanned::Spanned,
    visit::{self, Visit},
};

use super::{
    skip::{contains_skip, guarded, skipped},
    text::{line_starts, token_ranges},
};

const ASSERTION_PREFIXES: [&str; 3] = ["assert", "debug_assert", "prop_assert"];

pub fn apply(text: &str, max_blank_lines: usize) -> String {
    let edits = edits(text, max_blank_lines);

    proc_macro2::extra::invalidate_current_thread_spans();

    if edits.is_empty() {
        return text.to_owned();
    }
    rewrite(text, &edits)
}

#[derive(Default)]
struct Edits {
    blank_after: BTreeSet<usize>,
    removed: BTreeSet<usize>,
}

impl Edits {
    fn is_empty(&self) -> bool {
        self.blank_after.is_empty() && self.removed.is_empty()
    }
}

fn edits(text: &str, max_blank_lines: usize) -> Edits {
    let Ok(file) = syn::parse_file(text) else {
        return Edits::default();
    };

    if skipped(&file.attrs) {
        return Edits::default();
    }

    let lines: Vec<&str> = text.split('\n').collect();
    let mut planner = Planner {
        lines: &lines,
        breaks: BTreeSet::new(),
        protected: Vec::new(),
    };

    planner.visit_file(&file);

    let removed = surplus_blank_lines(text, &planner.protected, max_blank_lines);
    Edits {
        blank_after: planner.breaks,
        removed,
    }
}

fn rewrite(text: &str, edits: &Edits) -> String {
    let mut out = String::with_capacity(text.len() + edits.blank_after.len());

    for (index, line) in text.split_inclusive('\n').enumerate() {
        let number = index + 1;

        if edits.removed.contains(&number) {
            continue;
        }
        out.push_str(line);
        if edits.blank_after.contains(&number) && line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn surplus_blank_lines(
    text: &str,
    protected: &[Range<usize>],
    max_blank_lines: usize,
) -> BTreeSet<usize> {
    let Some(blank) = real_blank_lines(text) else {
        return BTreeSet::new();
    };

    let mut surplus = BTreeSet::new();
    let mut run = 0;

    for (index, is_blank) in blank.into_iter().enumerate() {
        let number = index + 1;
        if is_blank && !protected.iter().any(|lines| lines.contains(&number)) {
            run += 1;
            if run > max_blank_lines {
                surplus.insert(number);
            }
        } else {
            run = 0;
        }
    }
    surplus
}

fn real_blank_lines(text: &str) -> Option<Vec<bool>> {
    let tokens: TokenStream = without_shebang(text).parse().ok()?;
    let starts = line_starts(text);

    let mut ranges = Vec::new();

    token_ranges(tokens, text, &starts, &mut ranges);
    ranges.sort_unstable_by_key(|range| (range.start, range.end));

    let mut scanner = Gaps {
        text: text.as_bytes(),
        ranges: &ranges,
        next: 0,
        pos: 0,
        depth: 0,
    };

    let blank = starts
        .iter()
        .zip(starts.iter().skip(1))
        .map(|(&start, &next)| {
            scanner.advance(start);

            let in_gap = scanner.pos == start && scanner.depth == 0;
            in_gap && text[start..next].trim().is_empty()
        })
        .collect();
    Some(blank)
}

fn without_shebang(text: &str) -> std::borrow::Cow<'_, str> {
    let Some(rest) = text.strip_prefix("#!") else {
        return text.into();
    };

    if rest.trim_start().starts_with('[') {
        return text.into();
    }
    match text.find('\n') {
        Some(end) => text[end..].to_owned().into(),
        None => String::new().into(),
    }
}

struct Gaps<'a> {
    text: &'a [u8],
    ranges: &'a [Range<usize>],
    next: usize,
    pos: usize,
    depth: usize,
}

impl Gaps<'_> {
    fn advance(&mut self, target: usize) {
        while self.pos < target {
            while self
                .ranges
                .get(self.next)
                .is_some_and(|range| range.end <= self.pos)
            {
                self.next += 1;
            }
            if let Some(range) = self.ranges.get(self.next)
                && range.start <= self.pos
            {
                self.pos = range.end;
                continue;
            }

            let rest = &self.text[self.pos..];
            if rest.starts_with(b"/*") {
                self.depth += 1;
                self.pos += 2;
            } else if self.depth > 0 && rest.starts_with(b"*/") {
                self.depth -= 1;
                self.pos += 2;
            } else if self.depth == 0 && rest.starts_with(b"//") {
                self.pos += rest
                    .iter()
                    .position(|&byte| byte == b'\n')
                    .unwrap_or(rest.len());
            } else {
                self.pos += 1;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Binding,
    Assertion,
    Item,
    Statement,
    Tail,
    Opaque,
}

struct Placed<'ast> {
    stmt: &'ast Stmt,
    kind: Kind,
    start: LineColumn,
    end: LineColumn,
    skipped: bool,
}

struct Planner<'text> {
    lines: &'text [&'text str],
    breaks: BTreeSet<usize>,
    protected: Vec<Range<usize>>,
}

impl<'ast> Visit<'ast> for Planner<'_> {
    guarded!();

    fn visit_block(&mut self, block: &'ast Block) {
        self.plan(block);
        visit::visit_block(self, block);
    }
}

impl<'ast> Planner<'_> {
    fn protect(&mut self, span: Span) {
        self.protected.push(span.start().line..span.end().line + 1);
    }

    fn plan(&mut self, block: &'ast Block) {
        let count = block.stmts.len();
        let placed: Vec<Placed<'ast>> = block
            .stmts
            .iter()
            .enumerate()
            .map(|(index, stmt)| {
                let span = stmt.span();
                Placed {
                    stmt,
                    kind: kind_of(stmt, index + 1 == count),
                    start: span.start(),
                    end: span.end(),
                    skipped: contains_skip(stmt),
                }
            })
            .collect();

        for stmt in placed.iter().filter(|stmt| stmt.skipped) {
            self.protected.push(stmt.start.line..stmt.end.line + 1);
        }

        let mut paragraph = BTreeSet::new();
        for pair in placed.windows(2) {
            let [above, below] = pair else {
                continue;
            };

            if above.kind == Kind::Binding {
                paragraph.extend(bound_names(above.stmt));
            } else {
                paragraph.clear();
            }

            if !self.adjacent(above, below) {
                paragraph.clear();
                continue;
            }
            if separates(above, below, &paragraph) {
                self.breaks.insert(above.end.line);
                paragraph.clear();
            }
        }
    }

    fn adjacent(&self, above: &Placed<'_>, below: &Placed<'_>) -> bool {
        if above.skipped || below.skipped || below.start.line != above.end.line + 1 {
            return false;
        }

        let (Some(above_line), Some(below_line)) = (
            self.lines.get(above.end.line - 1),
            self.lines.get(below.start.line - 1),
        ) else {
            return false;
        };

        let trailing: String = above_line.chars().skip(above.end.column).collect();
        let trailing = trailing.trim();
        let leading_clear = below_line
            .chars()
            .take(below.start.column)
            .all(char::is_whitespace);
        leading_clear && (trailing.is_empty() || trailing.starts_with("//"))
    }
}

fn separates(above: &Placed<'_>, below: &Placed<'_>, paragraph: &BTreeSet<String>) -> bool {
    match (above.kind, below.kind) {
        (Kind::Opaque, _) | (_, Kind::Opaque | Kind::Tail) => false,
        (Kind::Binding, Kind::Binding) => {
            let reads = initializer_reads(below.stmt);
            let subject = mutable_subject(above.stmt).is_some_and(|name| reads.contains(&name));
            let combined = reads.intersection(paragraph).count() >= 2;
            subject || combined
        }
        (left, right) => left != right,
    }
}

fn kind_of(stmt: &Stmt, last: bool) -> Kind {
    match stmt {
        Stmt::Local(_) => Kind::Binding,
        Stmt::Macro(stmt) => macro_kind(&stmt.mac),
        Stmt::Item(Item::Macro(item)) if item.mac.path.is_ident("macro_rules") => Kind::Item,
        Stmt::Item(Item::Macro(item)) => macro_kind(&item.mac),
        Stmt::Item(Item::Verbatim(_)) | Stmt::Expr(Expr::Verbatim(_), _) => Kind::Opaque,
        Stmt::Item(_) => Kind::Item,
        Stmt::Expr(_, None) if last => Kind::Tail,
        Stmt::Expr(Expr::Macro(expr), _) => macro_kind(&expr.mac),
        Stmt::Expr(..) => Kind::Statement,
    }
}

fn macro_kind(mac: &syn::Macro) -> Kind {
    let Some(name) = mac
        .path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
    else {
        return Kind::Opaque;
    };
    if ASSERTION_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        Kind::Assertion
    } else {
        Kind::Statement
    }
}

fn mutable_subject(stmt: &Stmt) -> Option<String> {
    let Stmt::Local(local) = stmt else {
        return None;
    };
    let pat = match &local.pat {
        Pat::Type(typed) => &*typed.pat,
        other => other,
    };
    let Pat::Ident(ident) = pat else {
        return None;
    };
    let plain = ident.by_ref.is_none() && ident.subpat.is_none();
    (plain && ident.mutability.is_some()).then(|| ident.ident.to_string())
}

fn bound_names(stmt: &Stmt) -> BTreeSet<String> {
    let mut names = Names::default();

    if let Stmt::Local(local) = stmt {
        names.visit_pat(&local.pat);
    }
    names.bound
}

fn initializer_reads(stmt: &Stmt) -> BTreeSet<String> {
    let Stmt::Local(Local {
        init: Some(init), ..
    }) = stmt
    else {
        return BTreeSet::new();
    };

    let mut names = Names::default();

    names.visit_expr(&init.expr);
    names.read.difference(&names.bound).cloned().collect()
}

#[derive(Default)]
struct Names {
    read: BTreeSet<String>,
    bound: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Names {
    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        if node.qself.is_none()
            && let Some(ident) = node.path.get_ident()
        {
            self.read.insert(ident.to_string());
        }
        visit::visit_expr_path(self, node);
    }

    fn visit_pat_ident(&mut self, node: &'ast syn::PatIdent) {
        self.bound.insert(node.ident.to_string());
        visit::visit_pat_ident(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        macro_reads(node.tokens.clone(), &mut self.read);
    }
}

fn macro_reads(tokens: TokenStream, read: &mut BTreeSet<String>) {
    let mut after_dot = false;
    for tree in tokens {
        match &tree {
            TokenTree::Ident(ident) if !after_dot => {
                read.insert(ident.to_string());
            }
            TokenTree::Group(group) => macro_reads(group.stream(), read),
            TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
        after_dot = matches!(&tree, TokenTree::Punct(punct) if punct.as_char() == '.');
    }
}

#[cfg(test)]
mod tests {
    use super::apply;

    fn assert_paragraphs(source: &str, expected: &str) {
        let once = apply(source, 1);

        assert_eq!(once, expected);
        assert_eq!(apply(&once, 1), once);
    }

    fn assert_untouched(source: &str) {
        assert_eq!(apply(source, 1), source);
    }

    #[test]
    fn combining_two_bindings_starts_a_paragraph() {
        assert_paragraphs(
            "fn f() {\n    let x = 1;\n    let y = 2;\n    let z = x + y;\n    println!(\"{z}\");\n}\n",
            "fn f() {\n    let x = 1;\n    let y = 2;\n\n    let z = x + y;\n\n    println!(\"{z}\");\n}\n",
        );
    }

    #[test]
    fn a_binding_derived_from_one_other_stays_with_it() {
        assert_paragraphs(
            "fn f(input: &str) -> Result<(), E> {\n    let x = parse(input)?;\n    let y = x.normalize();\n    assert!(x.is_valid());\n    assert_eq!(y.len(), 3);\n    let z = combine(x, y);\n    Ok(())\n}\n",
            "fn f(input: &str) -> Result<(), E> {\n    let x = parse(input)?;\n    let y = x.normalize();\n\n    assert!(x.is_valid());\n    assert_eq!(y.len(), 3);\n\n    let z = combine(x, y);\n    Ok(())\n}\n",
        );
    }

    #[test]
    fn a_mutable_subject_is_its_own_paragraph() {
        assert_paragraphs(
            "fn f() {\n    let mut project = Project::new(\"test\");\n    let asset = project.add_asset(\"a.mkv\".into(), video_info(3));\n    let preview = project.clip_for(asset, 0, Time::ZERO).unwrap();\n    let again = project.clip_for(asset, 0, Time::from_seconds(5)).unwrap();\n    let placed = project.place_clip(asset, 0, Time::ZERO).unwrap();\n}\n",
            "fn f() {\n    let mut project = Project::new(\"test\");\n\n    let asset = project.add_asset(\"a.mkv\".into(), video_info(3));\n    let preview = project.clip_for(asset, 0, Time::ZERO).unwrap();\n    let again = project.clip_for(asset, 0, Time::from_seconds(5)).unwrap();\n    let placed = project.place_clip(asset, 0, Time::ZERO).unwrap();\n}\n",
        );
    }

    #[test]
    fn a_typed_mutable_subject_counts() {
        assert_paragraphs(
            "fn f() {\n    let mut out: Vec<u8> = Vec::new();\n    let n = out.len();\n}\n",
            "fn f() {\n    let mut out: Vec<u8> = Vec::new();\n\n    let n = out.len();\n}\n",
        );
    }

    #[test]
    fn a_tail_expression_stays_attached() {
        assert_untouched("fn f() -> usize {\n    let x = 1;\n    x + 1\n}\n");
        assert_untouched("fn f() {\n    run();\n    done()\n}\n");
    }

    #[test]
    fn statements_of_one_kind_stay_together() {
        assert_untouched(
            "fn f() {\n    a();\n    b.c();\n    if d {\n        e();\n    }\n    x = 1;\n}\n",
        );
        assert_untouched(
            "fn f() {\n    assert!(a);\n    assert_eq!(b, c);\n    debug_assert!(d);\n}\n",
        );
    }

    #[test]
    fn items_are_separated_from_statements() {
        assert_paragraphs(
            "fn f() {\n    use std::fmt;\n    let x = 1;\n    go(x);\n}\n",
            "fn f() {\n    use std::fmt;\n\n    let x = 1;\n\n    go(x);\n}\n",
        );
    }

    #[test]
    fn nested_blocks_and_closures_are_reached() {
        assert_paragraphs(
            "fn f() {\n    if a {\n        let x = 1;\n        go(x);\n    }\n    let g = || {\n        let y = 2;\n        go(y);\n    };\n}\n",
            "fn f() {\n    if a {\n        let x = 1;\n\n        go(x);\n    }\n\n    let g = || {\n        let y = 2;\n\n        go(y);\n    };\n}\n",
        );
    }

    #[test]
    fn an_existing_blank_line_resets_the_paragraph() {
        assert_untouched("fn f() {\n    let x = 1;\n\n    let y = 2;\n    let z = x + y;\n}\n");
    }

    #[test]
    fn a_comment_between_statements_is_left_alone() {
        assert_untouched("fn f() {\n    let x = 1;\n    // why\n    go(x);\n}\n");
        assert_untouched("fn f() {\n    let x = 1;\n    /* why */ go(x);\n}\n");
    }

    #[test]
    fn a_trailing_line_comment_does_not_block() {
        assert_paragraphs(
            "fn f() {\n    let x = 1; // one\n    go(x);\n}\n",
            "fn f() {\n    let x = 1; // one\n\n    go(x);\n}\n",
        );
    }

    #[test]
    fn a_trailing_block_comment_blocks() {
        assert_untouched("fn f() {\n    let x = 1; /* one */\n    go(x);\n}\n");
    }

    #[test]
    fn statements_sharing_a_line_are_left_alone() {
        assert_untouched("fn f() {\n    let x = 1; go(x);\n}\n");
    }

    #[test]
    fn rustfmt_skip_is_honoured() {
        assert_untouched("#[rustfmt::skip]\nfn f() {\n    let x = 1;\n    go(x);\n}\n");
        assert_untouched("#![rustfmt::skip]\nfn f() {\n    let x = 1;\n    go(x);\n}\n");
        assert_untouched("fn f() {\n    let x = 1;\n    #[rustfmt::skip]\n    go(x);\n}\n");
        assert_untouched("fn f() {\n    #![rustfmt::skip]\n    let x = 1;\n    go(x);\n}\n");
        assert_untouched(
            "#[cfg_attr(rustfmt, rustfmt::skip)]\nfn f() {\n    let x = 1;\n    go(x);\n}\n",
        );
        assert_untouched("#[rustfmt_skip]\nfn f() {\n    let x = 1;\n    go(x);\n}\n");
        assert_untouched(
            "impl A {\n    #[rustfmt::skip]\n    fn f() {\n        let x = 1;\n        go(x);\n    }\n}\n",
        );
    }

    #[test]
    fn a_statement_holding_a_skipped_block_is_not_separated() {
        assert_untouched(
            "fn f() {\n    let x = 1;\n    let t = #[rustfmt::skip] {\n        1\n    };\n}\n",
        );
    }

    #[test]
    fn macro_bodies_are_not_entered() {
        assert_untouched("fn f() {\n    m! {\n        let x = 1;\n        go(x);\n    }\n}\n");
        assert_untouched(
            "macro_rules! m {\n    () => {\n        let x = 1;\n        go(x);\n    };\n}\n",
        );
    }

    #[test]
    fn unparseable_source_is_left_alone() {
        assert_untouched("fn f() {\n    let x = ;\n    go(x);\n}\n");
    }

    #[test]
    fn names_bound_inside_an_initializer_are_not_reads() {
        assert_untouched(
            "fn f() {\n    let x = 1;\n    let y = 2;\n    let g = |x, y| x + y;\n}\n",
        );
    }

    #[test]
    fn macro_arguments_count_as_reads_except_after_a_dot() {
        assert_paragraphs(
            "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = format!(\"{}{}\", a, b);\n}\n",
            "fn f() {\n    let a = 1;\n    let b = 2;\n\n    let c = format!(\"{}{}\", a, b);\n}\n",
        );
        assert_untouched(
            "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = m!(s.a, s.b);\n}\n",
        );
    }

    #[test]
    fn a_shebang_keeps_line_numbers_aligned() {
        assert_paragraphs(
            "#!/usr/bin/env run-cargo-script\nfn main() {\n    let x = 1;\n    go(x);\n}\n",
            "#!/usr/bin/env run-cargo-script\nfn main() {\n    let x = 1;\n\n    go(x);\n}\n",
        );
    }

    #[test]
    fn attributes_on_a_statement_count_as_its_first_line() {
        assert_paragraphs(
            "fn f() {\n    let x = 1;\n    #[cfg(test)]\n    go(x);\n}\n",
            "fn f() {\n    let x = 1;\n\n    #[cfg(test)]\n    go(x);\n}\n",
        );
    }

    #[test]
    fn multi_line_statements_split_after_their_last_line() {
        assert_paragraphs(
            "fn f() {\n    let x = call(\n        1,\n    );\n    go(x);\n}\n",
            "fn f() {\n    let x = call(\n        1,\n    );\n\n    go(x);\n}\n",
        );
    }

    #[test]
    fn wide_characters_before_a_trailing_comment_are_columns_not_bytes() {
        assert_paragraphs(
            "fn f() {\n    let s = \"ééé\"; // é\n    go(s);\n}\n",
            "fn f() {\n    let s = \"ééé\"; // é\n\n    go(s);\n}\n",
        );
    }

    #[test]
    fn runs_of_blank_lines_collapse_to_one() {
        assert_paragraphs(
            "fn f() {\n    let x = 1;\n    let y = 2;\n\n\n    let z = x + y;\n\n\n\n    go(z);\n}\n",
            "fn f() {\n    let x = 1;\n    let y = 2;\n\n    let z = x + y;\n\n    go(z);\n}\n",
        );
    }

    #[test]
    fn whitespace_only_lines_count_as_blank() {
        assert_paragraphs(
            "fn f() {}\n  \n\t\n\nfn g() {}\n",
            "fn f() {}\n  \nfn g() {}\n",
        );
    }

    #[test]
    fn macro_bodies_collapse_too() {
        assert_paragraphs(
            "macro_rules! m {\n    () => {\n        a();\n\n\n        b();\n    };\n}\n",
            "macro_rules! m {\n    () => {\n        a();\n\n        b();\n    };\n}\n",
        );
    }

    #[test]
    fn a_wider_bound_keeps_that_many() {
        let source = "fn f() {}\n\n\n\nfn g() {}\n";

        assert_eq!(super::apply(source, 2), "fn f() {}\n\n\nfn g() {}\n");
    }

    #[test]
    fn blank_lines_inside_literals_stay() {
        assert_untouched("const S: &str = \"a\n\n\n\nb\";\n");
        assert_untouched("const S: &str = r#\"a\n\n\n\nb\"#;\n");
        assert_untouched("const S: &[u8] = b\"a\n\n\nb\";\n");
        assert_untouched("fn f() {\n    m!(\"a\n\n\nb\");\n}\n");
    }

    #[test]
    fn blank_lines_inside_comments_stay() {
        assert_untouched("/* a\n\n\n\nb */\nfn f() {}\n");
        assert_untouched("/* a /* nested\n\n\n*/ still\n\n\n*/\nfn f() {}\n");
        assert_untouched("/** doc\n\n\n\n*/\nfn f() {}\n");
    }

    #[test]
    fn a_line_comment_does_not_open_a_block() {
        assert_paragraphs(
            "// a /* b\nfn f() {}\n\n\nfn g() {}\n",
            "// a /* b\nfn f() {}\n\nfn g() {}\n",
        );
    }

    #[test]
    fn blank_lines_in_skipped_code_stay() {
        assert_untouched("#[rustfmt::skip]\nfn f() {\n    a();\n\n\n    b();\n}\n");
        assert_untouched(
            "fn f() {\n    #[rustfmt::skip]\n    m! {\n        a\n\n\n        b\n    }\n}\n",
        );
        assert_untouched("#![rustfmt::skip]\nfn f() {}\n\n\nfn g() {}\n");
    }

    #[test]
    fn a_shebang_does_not_stop_the_collapse() {
        assert_paragraphs(
            "#!/usr/bin/env run\nfn f() {}\n\n\nfn g() {}\n",
            "#!/usr/bin/env run\nfn f() {}\n\nfn g() {}\n",
        );
    }

    #[test]
    fn unparseable_source_keeps_its_blank_lines() {
        assert_untouched("fn f() {\n    let x = ;\n\n\n    go(x);\n}\n");
    }
}
