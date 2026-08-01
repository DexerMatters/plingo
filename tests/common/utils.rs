//! Test-facing adapter for the reusable immutable AST renderer.
#![allow(dead_code)]

use color_print::cprintln;
use plingo::{
    Graph, ReadGraph,
    component::parse::{AstKey, ParseSnapshot, data::AstBox},
    utils::PrettyDisplay,
    visual::AstTree,
};

use super::json::{JsonDocument, JsonToken};

/// Prints parser-published JSON roots through the shared visual AST module.
pub fn print_json_ast(graph: &Graph, roots: &[AstKey]) {
    let Some(uri) = roots.first().map(|root| root.uri) else {
        cprintln!("<bold,cyan>◇ AST</> <dim>∅ no parse roots</>");
        return;
    };
    let Some(snapshot) = graph.get::<ParseSnapshot<JsonToken>>(uri) else {
        cprintln!("<bold,red>AST</> <red>no parser snapshot is materialized</>");
        return;
    };
    for root in roots {
        let tree = AstTree::new(AstBox::<JsonDocument>::new(root.id, root.uri));
        cprintln!("{}", tree.pretty(&snapshot));
    }
}
