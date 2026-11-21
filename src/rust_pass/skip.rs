use std::collections::BTreeSet;

use proc_macro2::{TokenStream, TokenTree};
use syn::{Attribute, Meta, Stmt, visit::Visit};

macro_rules! guarded {
    () => {
        $crate::rust_pass::skip::guarded! {
            visit_item_fn: ItemFn => visit_item_fn,
            visit_impl_item_fn: ImplItemFn => visit_impl_item_fn,
            visit_trait_item_fn: TraitItemFn => visit_trait_item_fn,
            visit_item_mod: ItemMod => visit_item_mod,
            visit_item_impl: ItemImpl => visit_item_impl,
            visit_item_trait: ItemTrait => visit_item_trait,
            visit_item_const: ItemConst => visit_item_const,
            visit_item_static: ItemStatic => visit_item_static,
            visit_item_struct: ItemStruct => visit_item_struct,
            visit_item_enum: ItemEnum => visit_item_enum,
            visit_item_union: ItemUnion => visit_item_union,
            visit_item_type: ItemType => visit_item_type,
            visit_item_use: ItemUse => visit_item_use,
            visit_item_foreign_mod: ItemForeignMod => visit_item_foreign_mod,
            visit_item_macro: ItemMacro => visit_item_macro,
            visit_item_extern_crate: ItemExternCrate => visit_item_extern_crate,
            visit_item_trait_alias: ItemTraitAlias => visit_item_trait_alias,
            visit_impl_item_const: ImplItemConst => visit_impl_item_const,
            visit_impl_item_type: ImplItemType => visit_impl_item_type,
            visit_impl_item_macro: ImplItemMacro => visit_impl_item_macro,
            visit_trait_item_const: TraitItemConst => visit_trait_item_const,
            visit_trait_item_type: TraitItemType => visit_trait_item_type,
            visit_trait_item_macro: TraitItemMacro => visit_trait_item_macro,
            visit_foreign_item_fn: ForeignItemFn => visit_foreign_item_fn,
            visit_foreign_item_static: ForeignItemStatic => visit_foreign_item_static,
            visit_foreign_item_type: ForeignItemType => visit_foreign_item_type,
            visit_foreign_item_macro: ForeignItemMacro => visit_foreign_item_macro,
            visit_field: Field => visit_field,
            visit_variant: Variant => visit_variant,
            visit_arm: Arm => visit_arm,
            visit_local: Local => visit_local,
            visit_stmt_macro: StmtMacro => visit_stmt_macro,
            visit_expr_macro: ExprMacro => visit_expr_macro,
            visit_expr_block: ExprBlock => visit_expr_block,
            visit_expr_async: ExprAsync => visit_expr_async,
            visit_expr_unsafe: ExprUnsafe => visit_expr_unsafe,
            visit_expr_const: ExprConst => visit_expr_const,
            visit_expr_closure: ExprClosure => visit_expr_closure,
            visit_expr_if: ExprIf => visit_expr_if,
            visit_expr_match: ExprMatch => visit_expr_match,
            visit_expr_loop: ExprLoop => visit_expr_loop,
            visit_expr_while: ExprWhile => visit_expr_while,
            visit_expr_for_loop: ExprForLoop => visit_expr_for_loop,
        }
    };
    ($($method:ident: $node:ident => $walk:ident),* $(,)?) => {
        $(
            fn $method(&mut self, node: &'ast ::syn::$node) {
                if $crate::rust_pass::skip::skipped(&node.attrs) {
                    self.protect(::syn::spanned::Spanned::span(node));
                } else {
                    ::syn::visit::$walk(self, node);
                }
            }
        )*
    };
}

pub(crate) use guarded;

pub fn contains_skip(stmt: &Stmt) -> bool {
    let mut finder = SkipFinder::default();

    finder.visit_stmt(stmt);
    finder.found
}

#[derive(Default)]
struct SkipFinder {
    found: bool,
}

impl<'ast> Visit<'ast> for SkipFinder {
    fn visit_attribute(&mut self, node: &'ast Attribute) {
        self.found |= is_skip(node);
    }
}

pub fn skipped(attrs: &[Attribute]) -> bool {
    attrs.iter().any(is_skip)
}

fn is_skip(attr: &Attribute) -> bool {
    let path = attr.path();
    let segments: Vec<String> = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    match segments.as_slice() {
        [first, second, ..] if first == "rustfmt" && second == "skip" => true,
        [only] if only == "rustfmt_skip" => true,
        [only] if only == "cfg_attr" => match &attr.meta {
            Meta::List(list) => mentions_skip(list.tokens.clone()),
            Meta::Path(_) | Meta::NameValue(_) => false,
        },
        _ => false,
    }
}

fn mentions_skip(tokens: TokenStream) -> bool {
    let mut idents = BTreeSet::new();

    collect_idents(tokens, &mut idents);
    idents.contains("rustfmt_skip") || (idents.contains("rustfmt") && idents.contains("skip"))
}

fn collect_idents(tokens: TokenStream, idents: &mut BTreeSet<String>) {
    for tree in tokens {
        match tree {
            TokenTree::Ident(ident) => {
                idents.insert(ident.to_string());
            }
            TokenTree::Group(group) => collect_idents(group.stream(), idents),
            TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
    }
}
