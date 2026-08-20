//! Diagnostic collection traverses only accepted products and de-duplicates
//! recovery data already stored in the session.

use std::collections::HashSet;

use super::{
    data::{
        green::{ParseErrorInfo, TreeData},
        product::{ProductData, ProductId},
    },
    parsing::ParserSessionState,
    types::SessionArenas,
};

pub(crate) fn collect_parse_diagnostics(
    state: &ParserSessionState,
    arenas: Option<&SessionArenas>,
    roots: &[ProductId],
) -> Vec<ParseErrorInfo> {
    let mut diagnostics = Vec::new();
    let mut seen_diagnostics = HashSet::new();

    for info in &state.diagnostics {
        if seen_diagnostics.insert(info.clone()) {
            diagnostics.push(info.clone());
        }
    }

    let Some(arenas) = arenas else {
        return diagnostics;
    };

    let mut seen_products = HashSet::new();
    for &pid in roots {
        collect_ast_parse_diagnostics(
            pid,
            arenas,
            &mut seen_products,
            &mut seen_diagnostics,
            &mut diagnostics,
        );
    }

    diagnostics
}

fn collect_ast_parse_diagnostics(
    product_id: ProductId,
    arenas: &SessionArenas,
    seen_products: &mut HashSet<ProductId>,
    seen_diagnostics: &mut HashSet<ParseErrorInfo>,
    diagnostics: &mut Vec<ParseErrorInfo>,
) {
    if !seen_products.insert(product_id) {
        return;
    }
    let Some(product) = arenas.products.get(product_id) else {
        return;
    };

    match &product.data {
        ProductData::Error { .. } => {
            let Some(tree) = arenas.trees.get(product.green) else {
                return;
            };
            let TreeData::Error {
                kind,
                node,
                unexpected,
                expected,
                recovered,
                location,
                ..
            } = &tree.data
            else {
                return;
            };
            let info = ParseErrorInfo {
                kind: kind.clone(),
                node: *node,
                length: tree.length,
                unexpected: *unexpected,
                expected: *expected,
                recovered: *recovered,
                location: *location,
            };
            if seen_diagnostics.insert(info.clone()) {
                diagnostics.push(info);
            }
        }
        ProductData::Node { children, .. } => {
            for child in children.iter().copied() {
                collect_ast_parse_diagnostics(
                    child,
                    arenas,
                    seen_products,
                    seen_diagnostics,
                    diagnostics,
                );
            }
        }
        ProductData::Token { .. } => {}
    }
}
