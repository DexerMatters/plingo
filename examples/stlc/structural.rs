//! User-authored components that publish composable STLC structural views
//! (reactive rewrite, plan Phase 6). The lowering pass classifies every
//! syntax node, publishes its untyped lowered value, its origin, its
//! diagnostics, and a downstream summary — with per-node child visitor
//! granularity.

use std::sync::{Arc, Mutex};

use plingo::framework::parse::ParseUnits;
use plingo::reactive::prelude::*;
use plingo::reactive::view::NodeId;
use plingo::reactive_component as component;
use plingo::reactive_view as view;

use super::syntax::{StlcCase, StlcDocument, StlcObservedExt, StlcTree};

/// A compact typed classification of parser artifacts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcNodeKind {
    Document,
    Declaration,
    Expression,
    Type,
    Other,
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// Per-node structural kind (the parser-to-index product).
#[view(map, key = String, value = Vec<NodeFact>)]
pub struct StlcNodeIndex;

/// One per-node structural fact.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeFact {
    pub node: NodeId,
    pub kind: StlcNodeKind,
}

/// Per-node lowered values (untyped).
#[view(map, key = String, value = Vec<LoweredFact>)]
pub struct StlcLowered;

/// One per-node lowered fact.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LoweredFact {
    pub node: NodeId,
    pub value: String,
}

/// Origin of one lowered node in the source AST.
#[view(map, key = String, value = Vec<OriginFact>)]
pub struct StlcLoweredOrigin;

/// One per-node origin fact.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OriginFact {
    pub node: NodeId,
    pub origin: NodeId,
}

/// Lowering diagnostics owned by one AST item.
#[view(map, key = String, value = Vec<LoweringDiag>)]
pub struct StlcLoweringDiagnostics;

/// One per-node lowering diagnostic list.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LoweringDiag {
    pub node: NodeId,
    pub messages: Arc<[String]>,
}

/// A downstream consumer proving that lowered structural views compose.
#[view(map, key = String, value = Vec<SummaryFact>)]
pub struct StlcLoweredSummary;

/// One per-node summary fact.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SummaryFact {
    pub node: NodeId,
    pub value: String,
}

// ---------------------------------------------------------------------------
// The lowering pass
// ---------------------------------------------------------------------------

/// The structural pass: one child visitor per document over
/// [`ParseUnits<StlcDocument>`], then per-node child visitors inside a
/// document. Publishes the per-node index, lowered, origin, diagnostics,
/// and summary maps.
#[component]
pub fn structural_pass(
    units: ParseUnits<StlcDocument>,
    syntax: StlcTree,
) -> (
    StlcNodeIndex,
    StlcLowered,
    StlcLoweredOrigin,
    StlcLoweringDiagnostics,
    StlcLoweredSummary,
) {
    let index = Emitted::<StlcNodeIndex>::new()?;
    let lowered = Emitted::<StlcLowered>::new()?;
    let origins = Emitted::<StlcLoweredOrigin>::new()?;
    let lower_diags = Emitted::<StlcLoweringDiagnostics>::new()?;
    let summaries = Emitted::<StlcLoweredSummary>::new()?;
    let (index_c, lowered_c, origins_c, diags_c, summaries_c) = (
        index.clone(), lowered.clone(), origins.clone(), lower_diags.clone(),
        summaries.clone(),
    );
    units.visit_each(move |uri, unit| -> Result<()> {
        let Some(unit) = unit else {
            return Ok(());
        };
        let facts: Arc<Mutex<Vec<(NodeId, StlcNodeKind)>>> = Arc::new(Mutex::new(Vec::new()));
        let lowered_buf: Arc<Mutex<Vec<(NodeId, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let origin_buf: Arc<Mutex<Vec<(NodeId, NodeId)>>> = Arc::new(Mutex::new(Vec::new()));
        let diag_buf: Arc<Mutex<Vec<(NodeId, Arc<[String]>)>>> = Arc::new(Mutex::new(Vec::new()));
        let summary_buf: Arc<Mutex<Vec<(NodeId, String)>>> = Arc::new(Mutex::new(Vec::new()));

        // Classify this node, then recurse per-child.
        classify_node(
            &uri,
            &syntax,
            &facts,
            &lowered_buf,
            &origin_buf,
            &diag_buf,
            &summary_buf,
            unit.root,
        )?;

        let mut f = facts.lock().expect("facts lock");
        index_c.set(uri.clone(), f.drain(..).map(|(n, k)| NodeFact { node: n, kind: k }).collect())?;
        drop(f);
        let mut l = lowered_buf.lock().expect("lowered lock");
        lowered_c.set(uri.clone(), l.drain(..).map(|(n, v)| LoweredFact { node: n, value: v }).collect())?;
        drop(l);
        let mut o = origin_buf.lock().expect("origin lock");
        origins_c.set(uri.clone(), o.drain(..).map(|(n, x)| OriginFact { node: n, origin: x }).collect())?;
        drop(o);
        let mut d = diag_buf.lock().expect("diag lock");
        diags_c.set(uri.clone(), d.drain(..).map(|(n, m)| LoweringDiag { node: n, messages: m }).collect())?;
        drop(d);
        let mut s = summary_buf.lock().expect("summary lock");
        summaries_c.set(uri.clone(), s.drain(..).map(|(n, v)| SummaryFact { node: n, value: v }).collect())?;
        Ok(())
    })?;
    Ok((
        index, lowered, origins, lower_diags, summaries,
    ))
}

/// Classifies one node and recurses into its children (each child its
/// own visitor instance, so edits to one declaration re-run only that
/// declaration's structural facts).
fn classify_node(
    uri: &str,
    syntax: &ObservedHandle<StlcTree>,
    facts: &Arc<Mutex<Vec<(NodeId, StlcNodeKind)>>>,
    lowered: &Arc<Mutex<Vec<(NodeId, String)>>>,
    origins: &Arc<Mutex<Vec<(NodeId, NodeId)>>>,
    diags: &Arc<Mutex<Vec<(NodeId, Arc<[String]>)>>>,
    summaries: &Arc<Mutex<Vec<(NodeId, String)>>>,
    id: NodeId,
) -> Result<()> {
    let kind = match syntax.case(id)? {
        Some(StlcCase::Document(_)) => StlcNodeKind::Document,
        Some(StlcCase::Declaration(_)) => StlcNodeKind::Declaration,
        Some(StlcCase::Expr(_)) => StlcNodeKind::Expression,
        Some(StlcCase::Type(_)) | Some(StlcCase::TypeAtom(_)) => StlcNodeKind::Type,
        _ => StlcNodeKind::Other,
    };
    let lowered_value = format!("untyped::{kind:?}");
    facts
        .lock()
        .expect("facts lock")
        .push((id, kind.clone()));
    lowered
        .lock()
        .expect("lowered lock")
        .push((id, lowered_value.clone()));
    origins
        .lock()
        .expect("origin lock")
        .push((id, id));
    let messages: Arc<[String]> = if matches!(&kind, StlcNodeKind::Other) {
        vec![format!("unclassified source node {id:?}")].into()
    } else {
        Vec::new().into()
    };
    diags
        .lock()
        .expect("diag lock")
        .push((id, messages));
    summaries
        .lock()
        .expect("summary lock")
        .push((id, format!("summary:{lowered_value}")));

    // Recurse per-child as separate visitor instances.
    let children = TreeObservedExt::children(syntax, id)?;
    for child in children {
        let uri = uri.to_string();
        let recursion = syntax.clone();
        let facts = Arc::clone(facts);
        let lowered = Arc::clone(lowered);
        let origins = Arc::clone(origins);
        let diags = Arc::clone(diags);
        let summaries = Arc::clone(summaries);
        TreeObservedExt::visit_node(&syntax.clone(), child, move |_id, _payload| {
            classify_node(
                &uri,
                &recursion,
                &facts,
                &lowered,
                &origins,
                &diags,
                &summaries,
                child,
            )
        })?;
    }
    Ok(())
}

use plingo::reactive::api::TreeObservedExt;