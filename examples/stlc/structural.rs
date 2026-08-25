//! Plain reactive structural products for the STLC syntax family: one fact
//! per syntax node (smallest-unit granularity, plan §5.1) — no aggregate
//! bubbling.

use plingo::framework::parse::TreeParseUnits;
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

pub fn structural_pass(_: ()) -> Result<()> {
    run_each_key::<TreeParseUnits<StlcDocument>, _>(classify_document)
}

pub fn classify_document(uri: String) -> Result<()> {
    let Some(unit) = observe_view::<TreeParseUnits<StlcDocument>>()?.get(&uri)? else {
        return Ok(());
    };
    let Some(root) = unit.root else {
        return Ok(());
    };
    run(
        |(uri, id): (String, Node<StlcTree>)| classify_node(uri, id),
        (uri, root),
    )?;
    Ok(())
}

/// Classifies ONE node and recurses. Each node owns its own facts; a
/// changed subtree rewrites only its own nodes' facts.
fn classify_node(uri: String, id: Node<StlcTree>) -> Result<()> {
    let kind = match StlcTree::observe_case(id)? {
        Some(StlcCase::Document(_)) => StlcNodeKind::Document,
        Some(StlcCase::Declaration(_)) => StlcNodeKind::Declaration,
        Some(StlcCase::Expr(_)) => StlcNodeKind::Expression,
        Some(StlcCase::Type(_)) | Some(StlcCase::TypeAtom(_)) => StlcNodeKind::Type,
        _ => StlcNodeKind::Other,
    };
    let lowered = format!("untyped::{kind:?}");
    let is_other = matches!(kind, StlcNodeKind::Other);
    let kind = kind.clone();

    let index = emit_view::<StlcNodeIndex>()?;
    let lowered_view = emit_view::<StlcLowered>()?;
    let origins = emit_view::<StlcLoweredOrigin>()?;
    let diagnostics = emit_view::<StlcLoweringDiagnostics>()?;
    let summaries = emit_view::<StlcLoweredSummary>()?;

    index.insert(id, kind)?;
    lowered_view.insert(id, lowered.clone())?;
    origins.insert(id, id)?;
    if is_other {
        diagnostics
            .replace(&id, vec![format!("unclassified source node {id:?}")])?;
    } else {
        diagnostics.replace(&id, Vec::new())?;
    }
    summaries.insert(id, format!("summary:{lowered}"),)?;

    for child in StlcTree::observe_children(id)?.iter().copied() {
        run(
            |(uri, child): (String, Node<StlcTree>)| classify_node(uri, child),
            (uri.clone(), child),
        )?;
    }
    Ok(())
}
