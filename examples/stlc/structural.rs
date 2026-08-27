//! Reactive structural products for the STLC syntax family: one fact
//! per syntax node (smallest-unit granularity, plan §5.1, §24.6) — no
//! aggregate bubbling and no nested effectful recursion. One component
//! instance per syntax node is driven by the exact parser payload.

use plingo::framework::parse::{ParserTreePayloads, TreeParseUnits};
use plingo::reactive::component::{EachKey, Write};
use plingo::reactive::kind::{emit_view, observe_view};
use plingo::reactive::prelude::*;
use plingo::reactive::view::Node;
use reactive_macros::view;

use super::syntax::{StlcCase, StlcDocument, StlcTree};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcNodeKind {
    Document,
    Declaration,
    Expression,
    Type,
    Other,
}

/// One node's classification.
#[view]
pub struct StlcNodeIndex(Map<Node<StlcTree>, StlcNodeKind>);

/// One node's lowering label.
#[view]
pub struct StlcLowered(Map<Node<StlcTree>, String>);

/// One node's origin (itself in the untyped lowering).
#[view]
pub struct StlcLoweredOrigin(Map<Node<StlcTree>, Node<StlcTree>>);

/// One node's lowering diagnostics (a list so a node may carry several).
#[view]
pub struct StlcLoweringDiagnostics(List<Node<StlcTree>, String>);

/// One node's summary line.
#[view]
pub struct StlcLoweredSummary(Map<Node<StlcTree>, String>);

/// One structural instance per syntax node, driven by the exact parser
/// payload. It reads only its own case and owns exactly its own facts; a
/// changed subtree rewrites only its own nodes' facts. Removal retires the
/// instance and its outputs automatically.
#[reactive_macros::component]
pub fn structural_node(
    key: EachKey<ParserTreePayloads<StlcDocument>>,
    index: Write<StlcNodeIndex>,
    lowered: Write<StlcLowered>,
    origins: Write<StlcLoweredOrigin>,
    summaries: Write<StlcLoweredSummary>,
) -> Result<()> {
    let id = key;
    let kind = match StlcTree::observe_case(id.clone())? {
        Some(StlcCase::Document(_)) => StlcNodeKind::Document,
        Some(StlcCase::Declaration(_)) => StlcNodeKind::Declaration,
        Some(StlcCase::Expr(_)) => StlcNodeKind::Expression,
        Some(StlcCase::Type(_)) | Some(StlcCase::TypeAtom(_)) => StlcNodeKind::Type,
        _ => StlcNodeKind::Other,
    };
    let lowered_text = format!("untyped::{kind:?}");
    let is_other = matches!(kind, StlcNodeKind::Other);

    index.insert(id.clone(), kind)?;
    lowered.insert(id.clone(), lowered_text.clone())?;
    origins.insert(id.clone(), id.clone())?;
    if is_other {
        emit_view::<StlcLoweringDiagnostics>()?
            .replace(&id, vec![format!("unclassified source node {id:?}")])?;
    } else {
        emit_view::<StlcLoweringDiagnostics>()?.replace(&id, Vec::new())?;
    }
    summaries.insert(id, format!("summary:{lowered_text}"))?;
    Ok(())
}

/// Back-compat installer: the structural pass is the per-node component.
pub fn structural_pass_install(engine: &mut plingo::reactive::Engine) -> Result<()> {
    structural_node_install(engine)?;
    Ok(())
}
