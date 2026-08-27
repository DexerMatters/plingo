//! A complete public-view pipeline:
//!
//! `Programs` → `SurfaceTree` → `LoweredTree` → `ScopeGraph` → resolution and
//! analysis views → `DocumentSummaries`.
//!
//! Every derived stage is a set of exact element components (follow-up plan
//! §24.1, §24.4, §24.5): one automatic node slot per lowered node, one
//! component instance per surface/lowered node, graph identities from
//! generated `Output` ports instead of anchors, per-candidate resolution,
//! independent analysis projections joined by one component, and per-node
//! summaries that read only exact child summaries.

use plingo::framework::scope::{Scope, ScopeDomain, ScopeGraph, ScopeNode, observe_node, outgoing};
use plingo::reactive::component::{EachKey, Output, Read, Write};
use plingo::reactive::kind::{List, Map, Tree, TreeFact, emit_view, observe_view};
use plingo::reactive::prelude::*;
use plingo::reactive::view::Node;
use reactive_macros::{component, view};

// ---------------------------------------------------------------------------
// Source model and syntax/lowering trees
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Program {
    pub binding: String,
    pub value: i64,
    pub reference: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SurfaceNode {
    Document,
    Binding(String),
    Add,
    Number(i64),
    Name(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LoweredNode {
    Module,
    Definition(String),
    ApplyAdd,
    Integer(i64),
    Variable(String),
}

#[view]
pub struct Programs(Map<String, Program>);

#[view]
pub struct SurfaceRoots(Map<String, Node<SurfaceTree>>);

#[view]
pub struct SurfaceTree(Tree<String, SurfaceNode>);

/// One membership entry per live surface node: the per-node driver for the
/// lowering projection (plan §24.1). The source builder publishes it as it
/// allocates each node; a removed node retires its lowering instance.
#[view]
pub struct SurfaceNodes(Map<Node<SurfaceTree>, ()>);

#[view]
pub struct LoweredRoots(Map<String, Node<LoweredTree>>);

#[view]
pub struct LoweredTree(Tree<String, LoweredNode>);

/// One source-to-target projection row per lowered node (plan §24.1).
#[view]
pub struct LoweredOrigins(Map<Node<LoweredTree>, Node<SurfaceTree>>);

/// The inverse projection: exact source -> target mapping owned by one
/// inverse-provenance component (plan §24.4 item 8).
#[view]
pub struct LoweredBySource(Map<Node<SurfaceTree>, Node<LoweredTree>>);

/// One keyed source-tree writer per program. It is the source fixture side:
/// it publishes the surface forest plus its per-node membership, so every
/// downstream component is driven by exact elements, never an enumeration.
#[component]
pub fn build_surface_pass(key: EachKey<Programs>) -> Result<()> {
    build_surface_document(key)
}

fn build_surface_document(uri: String) -> Result<()> {
    let Some(program) = observe_view::<Programs>()?.get(&uri)? else {
        return Ok(());
    };
    let tree = emit_view::<SurfaceTree>()?;
    let membership = emit_view::<SurfaceNodes>()?;
    let root = tree.root(&uri, SurfaceNode::Document)?;
    membership.insert(root.clone(), ())?;
    let binding = tree.child(root.clone(), SurfaceNode::Binding(program.binding.clone()))?;
    membership.insert(binding.clone(), ())?;
    let add = tree.child(binding.clone(), SurfaceNode::Add)?;
    membership.insert(add.clone(), ())?;
    let number = tree.child(add.clone(), SurfaceNode::Number(program.value))?;
    membership.insert(number, ())?;
    if let Some(reference) = &program.reference {
        let name = tree.child(add, SurfaceNode::Name(reference.clone()))?;
        membership.insert(name, ())?;
    }
    emit_view::<SurfaceRoots>()?.insert(uri, root)
}

/// One lowering instance per surface node. It reads exactly its own payload,
/// parent, and child order; writes its automatic target node's payload,
/// parent, order, and links; and publishes the origin projection rows. A
/// missing endpoint projection writes nothing and its exact absent-key read
/// wakes the instance when the endpoint publishes (plan §24.1).
#[component]
pub fn lower_node(
    key: EachKey<SurfaceNodes>,
    origins: Write<LoweredOrigins>,
    by_source: Read<LoweredBySource>,
    target: Output<LoweredTree>,
) -> Result<()> {
    let source = key;
    let tree = observe_view::<SurfaceTree>()?;
    let Some(payload) = tree.payload(source.clone())? else {
        return Ok(());
    };
    let lowered_payload = match payload.as_ref() {
        SurfaceNode::Document => LoweredNode::Module,
        SurfaceNode::Binding(name) => LoweredNode::Definition(name.clone()),
        SurfaceNode::Add => LoweredNode::ApplyAdd,
        SurfaceNode::Number(value) => LoweredNode::Integer(*value),
        SurfaceNode::Name(name) => LoweredNode::Variable(name.clone()),
    };

    let lowered = target.node();
    // The origin row is unconditional: it is this instance's own output and
    // the driver for every downstream per-node component.
    origins.insert(lowered.clone(), source.clone())?;

    // Parent projection: absent endpoint projection -> write nothing; the
    // absent-key read on LoweredBySource[parent] wakes this instance.
    let lowered_parent = match tree.parent(source.clone())? {
        Some(parent) => match by_source.get(&parent)? {
            Some(parent_target) => Some(parent_target.as_ref().clone()),
            None => return Ok(()),
        },
        None => None,
    };

    // Children projection: same absent-endpoint protocol per child.
    let source_children = tree.children(source)?;
    let mut lowered_children = Vec::with_capacity(source_children.len());
    for child in source_children {
        let Some(child_target) = by_source.get(&child)? else {
            return Ok(());
        };
        lowered_children.push(child_target.as_ref().clone());
    }

    let tree = emit_view::<LoweredTree>()?;
    tree.put(
        TreeKey::Payload(lowered.clone()),
        Some(TreeFact::Payload(lowered_payload)),
    )?;
    tree.put(
        TreeKey::Parent(lowered.clone()),
        Some(TreeFact::Parent(lowered_parent)),
    )?;
    tree.set_children(lowered, lowered_children)?;
    Ok(())
}

/// The document root projection: reads the exact root target and publishes
/// the forest root plus the root map row.
#[component]
pub fn lower_root(
    key: EachKey<SurfaceRoots>,
    by_source: Read<LoweredBySource>,
    roots: Write<LoweredRoots>,
) -> Result<()> {
    let uri = key;
    let Some(source_root) = observe_view::<SurfaceRoots>()?.get(&uri)? else {
        return Ok(());
    };
    let Some(target) = by_source.get(source_root.as_ref())? else {
        return Ok(());
    };
    let target = target.as_ref().clone();
    emit_view::<LoweredTree>()?.replace_roots(&uri, &[target.clone()])?;
    roots.insert(uri, target)
}

// ---------------------------------------------------------------------------
// Scope graph and per-reference resolver
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PipelineScope;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScopeData {
    Document,
    Definition(String),
    Reference(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScopeLabel {
    Declaration(String),
    Reference(String),
    ResolvesTo,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScopeRequest {}

impl ScopeDomain for PipelineScope {
    type ScopeData = ScopeData;
    type Label = ScopeLabel;
    type Request = ScopeRequest;
}

/// The automatic document scope per URI (plan §24.4 item 1). The value is the
/// generated graph node; consumers read it from this view instead of
/// reconstructing an anchored identity.
#[view]
pub struct DocumentScopes(Map<String, Scope<PipelineScope>>);

/// Each lowered node's incoming scope, derived through the tree parent chain
/// (plan §24.4 item 3). The document root's entry is owned by the document
/// component; every child copies its parent's exact entry.
#[view]
pub struct IncomingScopes(Map<Node<LoweredTree>, Scope<PipelineScope>>);

/// The automatic reference graph node per candidate (plan §24.4 item 2/4).
/// The resolver links `ResolvesTo` from this exact node.
#[view]
pub struct ReferenceScopes(Map<Node<LoweredTree>, Scope<PipelineScope>>);

#[view]
pub struct ReferenceCandidates(Map<Node<LoweredTree>, String>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Resolution {
    Resolved { declaration: Scope<PipelineScope> },
    Unbound { name: String },
}

#[view]
pub struct Resolutions(Map<Node<LoweredTree>, Resolution>);

/// One document-scope component per lowered root. It owns the automatic
/// document graph node, the `DocumentScopes[uri]` row, and the root's
/// incoming-scope entry.
#[component]
pub fn emit_document_scope(
    key: EachKey<LoweredRoots>,
    document_scopes: Write<DocumentScopes>,
    incoming: Write<IncomingScopes>,
    document_node: Output<ScopeGraph<PipelineScope>>,
) -> Result<()> {
    let uri = key;
    let Some(root) = observe_view::<LoweredRoots>()?.get(&uri)? else {
        return Ok(());
    };
    let root = root.as_ref().clone();
    let scope = Scope::from_graph_node(document_node.node());
    document_node.set_node(ScopeNode::Scope(ScopeData::Document))?;
    document_scopes.insert(uri, scope.clone())?;
    incoming.insert(root, scope)
}

/// One scope component per lowered node. It reads exactly its own payload and
/// parent (for the incoming scope), publishes the definition/reference graph
/// node and semantic bucket, and records the reference node for the resolver.
#[component]
pub fn emit_node_scope(
    key: EachKey<LoweredOrigins>,
    incoming: Read<IncomingScopes>,
    reference_scopes: Write<ReferenceScopes>,
    node_output: Output<ScopeGraph<PipelineScope>>,
) -> Result<()> {
    let node = key;
    let tree = observe_view::<LoweredTree>()?;
    let Some(payload) = tree.payload(node.clone())? else {
        return Ok(());
    };
    // Incoming scope: the root reads its own entry (owned by the document
    // component); every child copies its parent's exact entry.
    let parent = tree.parent(node.clone())?;
    let incoming_scope = match &parent {
        Some(parent) => match incoming.get(parent)? {
            Some(scope) => scope.as_ref().clone(),
            None => return Ok(()),
        },
        None => match incoming.get(&node)? {
            Some(scope) => scope.as_ref().clone(),
            None => return Ok(()),
        },
    };
    if parent.is_some() {
        emit_view::<IncomingScopes>()?.insert(node.clone(), incoming_scope.clone())?;
    }

    match payload.as_ref() {
        LoweredNode::Variable(name) => {
            let graph_node = node_output.node();
            node_output.set_node(ScopeNode::Reference(ScopeData::Reference(name.clone())))?;
            emit_view::<ScopeGraph<PipelineScope>>()?.link(
                incoming_scope.node(),
                ScopeLabel::Reference(name.clone()),
                graph_node.clone(),
            )?;
            reference_scopes.insert(node, Scope::from_graph_node(graph_node))?;
        }
        LoweredNode::Definition(name) => {
            let graph_node = node_output.node();
            node_output.set_node(ScopeNode::Declaration(ScopeData::Definition(name.clone())))?;
            emit_view::<ScopeGraph<PipelineScope>>()?.link(
                incoming_scope.node(),
                ScopeLabel::Declaration(name.clone()),
                graph_node,
            )?;
        }
        LoweredNode::Module | LoweredNode::ApplyAdd | LoweredNode::Integer(_) => {}
    }
    Ok(())
}

/// One candidate component per reference node: publishes the exact identifier
/// element (plan §24.4 item 4).
#[component]
pub fn publish_candidate(
    key: EachKey<LoweredOrigins>,
    candidates: Write<ReferenceCandidates>,
) -> Result<()> {
    let node = key;
    let Some(payload) = observe_view::<LoweredTree>()?.payload(node.clone())? else {
        return Ok(());
    };
    match payload.as_ref() {
        LoweredNode::Variable(name) => candidates.insert(node, name.clone()),
        _ => Ok(()),
    }
}

/// One resolver per candidate (plan §24.4 item 5): reads only the exact
/// reference node, its incoming scope, and the declaration bucket for its own
/// name.
#[component]
pub fn resolve_pass(
    key: EachKey<ReferenceCandidates>,
    reference_scopes: Read<ReferenceScopes>,
    incoming: Read<IncomingScopes>,
    resolutions: Write<Resolutions>,
) -> Result<()> {
    let node = key;
    let Some(name) = observe_view::<ReferenceCandidates>()?.get(&node)? else {
        return Ok(());
    };
    let name = (*name).clone();
    let Some(reference_scope) = reference_scopes.get(&node)? else {
        return Ok(());
    };
    let reference_scope = reference_scope.as_ref().clone();
    let Some(incoming_scope) = incoming.get(&node)? else {
        return Ok(());
    };
    let incoming_scope = incoming_scope.as_ref().clone();
    let candidates = outgoing(incoming_scope, &ScopeLabel::Declaration(name.clone()))?;
    let resolution = match candidates.first().cloned() {
        Some(declaration) => {
            emit_view::<ScopeGraph<PipelineScope>>()?.link(
                reference_scope.node(),
                ScopeLabel::ResolvesTo,
                declaration.node(),
            )?;
            Resolution::Resolved { declaration }
        }
        None => Resolution::Unbound { name },
    };
    resolutions.insert(node, resolution)
}

// ---------------------------------------------------------------------------
// Independent analysis projections and their join (plan §24.4 items 6-7)
// ---------------------------------------------------------------------------

/// One label component per node: reads payload plus (for references) the
/// exact resolution element.
#[view]
pub struct AnalysisLabels(Map<Node<LoweredTree>, String>);

/// One origin-presence component per node.
#[view]
pub struct AnalysisOrigins(Map<Node<LoweredTree>, bool>);

/// One incoming-scope-presence component per node.
#[view]
pub struct AnalysisScopePresence(Map<Node<LoweredTree>, bool>);

#[component]
pub fn analysis_label(
    key: EachKey<LoweredOrigins>,
    labels: Write<AnalysisLabels>,
) -> Result<()> {
    let node = key;
    let Some(payload) = observe_view::<LoweredTree>()?.payload(node.clone())? else {
        return Ok(());
    };
    let label = match payload.as_ref() {
        LoweredNode::Module => "module".to_owned(),
        LoweredNode::Definition(name) => format!("definition {name}"),
        LoweredNode::ApplyAdd => "apply add".to_owned(),
        LoweredNode::Integer(value) => format!("integer {value}"),
        LoweredNode::Variable(name) => {
            let resolution = observe_view::<Resolutions>()?.get(&node)?;
            match resolution.as_deref() {
                Some(Resolution::Resolved { declaration }) => {
                    let target_name = match observe_node(declaration.clone())?.as_deref() {
                        Some(ScopeNode::Declaration(ScopeData::Definition(target))) => {
                            target.clone()
                        }
                        _ => "<invalid declaration>".to_owned(),
                    };
                    format!("reference {name} -> {target_name}")
                }
                Some(Resolution::Unbound { .. }) => {
                    format!("reference {name} -> <unbound>")
                }
                None => format!("reference {name} -> <pending>"),
            }
        }
    };
    labels.insert(node, label)
}

#[component]
pub fn analysis_origin(
    key: EachKey<LoweredOrigins>,
    origins: Write<AnalysisOrigins>,
) -> Result<()> {
    origins.insert(key, true)
}

#[component]
pub fn analysis_scope_presence(
    key: EachKey<LoweredOrigins>,
    incoming: Read<IncomingScopes>,
    presence: Write<AnalysisScopePresence>,
) -> Result<()> {
    let node = key;
    let Some(_) = incoming.get(&node)? else {
        return Ok(());
    };
    presence.insert(node, true)
}

/// One diagnostics component per resolution: publishes the exact list slot
/// only for an unbound reference; a resolved instance owns no slots and its
/// re-evaluation retracts a previously owned slot.
#[component]
pub fn analysis_diagnostics(
    key: EachKey<Resolutions>,
) -> Result<()> {
    let node = key;
    let Some(resolution) = observe_view::<Resolutions>()?.get(&node)? else {
        return Ok(());
    };
    if let Resolution::Unbound { name } = resolution.as_ref() {
        emit_view::<Diagnostics>()?
            .replace(&node, vec![format!("unbound reference {name}")])?;
    }
    Ok(())
}

/// One join per node: combines the exact label/origin/scope-presence
/// projections and the diagnostics list length into `Analyses[node]`.
#[component]
pub fn join_analyses(
    key: EachKey<LoweredOrigins>,
    labels: Read<AnalysisLabels>,
    origins: Read<AnalysisOrigins>,
    presence: Read<AnalysisScopePresence>,
    analyses: Write<Analyses>,
) -> Result<()> {
    let node = key;
    let Some(label) = labels.get(&node)? else {
        return Ok(());
    };
    let label = (*label).clone();
    let has_origin = origins.get(&node)?.is_some_and(|value| *value);
    let has_scope = presence.get(&node)?.is_some_and(|value| *value);
    let diagnostics = observe_view::<Diagnostics>()?.len(&node)?;
    analyses.insert(
        node,
        NodeAnalysis {
            label,
            diagnostics,
            has_origin,
            has_scope,
        },
    )
}

/// One inverse-provenance component per node (plan §24.4 item 8): maps
/// `Origin[target] -> LoweredBySource[source]`.
#[component]
pub fn inverse_provenance(
    key: EachKey<LoweredOrigins>,
    by_source: Write<LoweredBySource>,
) -> Result<()> {
    let node = key;
    let Some(source) = observe_view::<LoweredOrigins>()?.get(&node)? else {
        return Ok(());
    };
    by_source.insert(source.as_ref().clone(), node)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeAnalysis {
    pub label: String,
    pub diagnostics: usize,
    pub has_origin: bool,
    pub has_scope: bool,
}

#[view]
pub struct Analyses(Map<Node<LoweredTree>, NodeAnalysis>);

#[view]
pub struct Diagnostics(List<Node<LoweredTree>, String>);

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct DocumentSummary {
    pub nodes: usize,
    pub diagnostics: usize,
}

/// One per-node summary component (plan §24.5): reads exactly its own
/// analysis and each exact child summary; a leaf change wakes only actual
/// ancestors. The example's fields are fixed-degree, so each summary reads
/// the exact child elements directly.
#[view]
pub struct NodeSummaries(Map<Node<LoweredTree>, DocumentSummary>);

#[view]
pub struct DocumentSummaries(Map<String, DocumentSummary>);

#[component]
pub fn node_summary(
    key: EachKey<LoweredOrigins>,
    analyses: Read<Analyses>,
    summaries: Write<NodeSummaries>,
) -> Result<()> {
    let node = key;
    let Some(analysis) = analyses.get(&node)? else {
        return Ok(());
    };
    let mut summary = DocumentSummary {
        nodes: 1,
        diagnostics: analysis.diagnostics,
    };
    let tree = observe_view::<LoweredTree>()?;
    for child in tree.children(node.clone())? {
        let Some(child_summary) = observe_view::<NodeSummaries>()?.get(&child)? else {
            return Ok(());
        };
        summary.nodes += child_summary.nodes;
        summary.diagnostics += child_summary.diagnostics;
    }
    summaries.insert(node, summary)
}

/// The document summary mirrors the exact root summary (plan §24.5).
#[component]
pub fn document_summary(
    key: EachKey<LoweredRoots>,
    summaries: Read<NodeSummaries>,
    documents: Write<DocumentSummaries>,
) -> Result<()> {
    let uri = key;
    let Some(root) = observe_view::<LoweredRoots>()?.get(&uri)? else {
        return Ok(());
    };
    let Some(summary) = summaries.get(root.as_ref())? else {
        return Ok(());
    };
    documents.insert(uri, (*summary).clone())
}

// ---------------------------------------------------------------------------
// Installers
// ---------------------------------------------------------------------------

/// Installs the per-node lowering projection and the root projection.
pub fn lower_pass_install(engine: &mut plingo::reactive::Engine) -> plingo::Result<()> {
    lower_node_install(engine)?;
    lower_root_install(engine)?;
    inverse_provenance_install(engine)?;
    Ok(())
}

/// Installs the document/scope-node emission plus candidate publication.
pub fn emit_scopes_pass_install(engine: &mut plingo::reactive::Engine) -> plingo::Result<()> {
    emit_document_scope_install(engine)?;
    emit_node_scope_install(engine)?;
    publish_candidate_install(engine)?;
    Ok(())
}

/// Installs the independent analysis projections, their join, and the
/// diagnostics component.
pub fn analyze_pass_install(engine: &mut plingo::reactive::Engine) -> plingo::Result<()> {
    analysis_label_install(engine)?;
    analysis_origin_install(engine)?;
    analysis_scope_presence_install(engine)?;
    analysis_diagnostics_install(engine)?;
    join_analyses_install(engine)?;
    Ok(())
}

/// Installs the per-node summaries and the document summary.
pub fn summarize_pass_install(engine: &mut plingo::reactive::Engine) -> plingo::Result<()> {
    node_summary_install(engine)?;
    document_summary_install(engine)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Semantic digest (follow-up plan §24.4 / Phase 0): complete public-view
// content of this family, ID-erased and canonically ordered.
// ---------------------------------------------------------------------------

use std::collections::{BTreeMap, HashMap};

use plingo::framework::scope::{snapshot_nodes, snapshot_outgoing, snapshot_scope};
use plingo::reactive::digest::SemanticDigest;
use plingo::reactive::kind::{ListKey, TreeKey, TreeView};

fn render_program(program: &Program) -> String {
    let reference = match &program.reference {
        Some(name) => format!("some({name:?})"),
        None => "none".to_owned(),
    };
    format!(
        "program{{binding:{:?},value:{},reference:{reference}}}",
        program.binding, program.value
    )
}

fn render_surface(payload: &SurfaceNode) -> String {
    match payload {
        SurfaceNode::Document => "Document".to_owned(),
        SurfaceNode::Binding(name) => format!("Binding({name:?})"),
        SurfaceNode::Add => "Add".to_owned(),
        SurfaceNode::Number(value) => format!("Number({value})"),
        SurfaceNode::Name(name) => format!("Name({name:?})"),
    }
}

fn render_lowered(payload: &LoweredNode) -> String {
    match payload {
        LoweredNode::Module => "Module".to_owned(),
        LoweredNode::Definition(name) => format!("Definition({name:?})"),
        LoweredNode::ApplyAdd => "ApplyAdd".to_owned(),
        LoweredNode::Integer(value) => format!("Integer({value})"),
        LoweredNode::Variable(name) => format!("Variable({name:?})"),
    }
}

fn render_scope_data(data: &ScopeData) -> String {
    match data {
        ScopeData::Document => "Document".to_owned(),
        ScopeData::Definition(name) => format!("Definition({name:?})"),
        ScopeData::Reference(name) => format!("Reference({name:?})"),
    }
}

fn render_scope_node(node: &ScopeNode<PipelineScope>) -> String {
    match node {
        ScopeNode::Scope(data) => format!("Scope({})", render_scope_data(data)),
        ScopeNode::Declaration(data) => format!("Declaration({})", render_scope_data(data)),
        ScopeNode::Reference(data) => format!("Reference({})", render_scope_data(data)),
    }
}

fn render_scope_label(label: &ScopeLabel) -> String {
    match label {
        ScopeLabel::Declaration(name) => format!("Declaration({name:?})"),
        ScopeLabel::Reference(name) => format!("Reference({name:?})"),
        ScopeLabel::ResolvesTo => "ResolvesTo".to_owned(),
    }
}

fn render_resolution(snapshot: &plingo::reactive::Snapshot, resolution: &Resolution) -> String {
    match resolution {
        Resolution::Resolved { declaration } => {
            let target = snapshot
                .graph_node::<ScopeGraph<PipelineScope>>(declaration.node())
                .as_deref()
                .map(render_scope_node)
                .unwrap_or_else(|| "<unknown>".to_owned());
            format!("Resolved{{declaration:{target}}}")
        }
        Resolution::Unbound { name } => format!("Unbound{{name:{name:?}}}"),
    }
}

fn render_analysis(analysis: &NodeAnalysis) -> String {
    format!(
        "analysis{{label:{:?},diagnostics:{},has_origin:{},has_scope:{}}}",
        analysis.label, analysis.diagnostics, analysis.has_origin, analysis.has_scope
    )
}

fn render_summary(summary: &DocumentSummary) -> String {
    format!(
        "summary{{nodes:{},diagnostics:{}}}",
        summary.nodes, summary.diagnostics
    )
}

/// Depth-first walk of one rooted forest: records one
/// `{key}#{root}.{child...} = payload` row per reachable node and indexes
/// every reached node by its structural path.
fn walk_tree<V: TreeView>(
    snapshot: &plingo::reactive::Snapshot,
    view: &str,
    render: &dyn Fn(&V::Payload) -> String,
    paths: &mut HashMap<Node<V>, String>,
    digest: &mut SemanticDigest,
    node: Node<V>,
    path: &str,
) {
    if let Some(payload) = snapshot.tree_payload::<V>(node.clone()) {
        digest.insert(view, path, &render(&payload));
    }
    paths.insert(node.clone(), path.to_owned());
    for (index, child) in snapshot.tree_children::<V>(node.clone()).into_iter().enumerate() {
        walk_tree(
            snapshot,
            view,
            render,
            paths,
            digest,
            child,
            &format!("{path}.{index}"),
        );
    }
}

/// Enumerates one tree-kind view through its committed inputs: every domain
/// key with a root order, DFS per root; payload facts unreachable from any
/// root are recorded as sorted orphan rows so a leaked subtree cannot hide.
/// Returns the node -> structural-path index.
fn index_tree<V>(
    snapshot: &plingo::reactive::Snapshot,
    view: &str,
    render: impl Fn(&V::Payload) -> String,
    digest: &mut SemanticDigest,
) -> HashMap<Node<V>, String>
where
    V: TreeView,
    V::Key: Ord + Clone + std::fmt::Display,
{
    let mut paths: HashMap<Node<V>, String> = HashMap::new();
    let mut keys: Vec<V::Key> = snapshot
        .inputs::<V>()
        .into_iter()
        .filter_map(|input| match input {
            TreeKey::RootOrder(key) => Some(key),
            _ => None,
        })
        .collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        for (root_index, root) in snapshot.tree_roots_of::<V>(&key).into_iter().enumerate() {
            walk_tree(
                snapshot,
                view,
                &render,
                &mut paths,
                digest,
                root,
                &format!("{key}#{root_index}"),
            );
        }
    }
    let mut orphans: Vec<String> = snapshot
        .inputs::<V>()
        .into_iter()
        .filter_map(|input| match input {
            TreeKey::Payload(node) if !paths.contains_key(&node) => {
                snapshot.tree_payload::<V>(node).as_deref().map(&render)
            }
            _ => None,
        })
        .collect();
    orphans.sort();
    orphans.dedup();
    for (ordinal, text) in orphans.iter().enumerate() {
        digest.insert_domain(view, ordinal, &format!("orphan {text}"));
    }
    paths
}

/// Resolves a lowered node to its structural path; an unreachable node falls
/// back to a payload-rendered key so the row stays ID-erased.
fn lowered_path(
    snapshot: &plingo::reactive::Snapshot,
    paths: &HashMap<Node<LoweredTree>, String>,
    node: Node<LoweredTree>,
) -> String {
    paths
        .get(&node)
        .cloned()
        .unwrap_or_else(|| match snapshot.tree_payload::<LoweredTree>(node) {
            Some(payload) => format!("orphan {}", render_lowered(&payload)),
            None => "orphan <unknown>".to_owned(),
        })
}

/// Same fallback resolution for surface nodes.
fn surface_path(
    snapshot: &plingo::reactive::Snapshot,
    paths: &HashMap<Node<SurfaceTree>, String>,
    node: Node<SurfaceTree>,
) -> String {
    paths
        .get(&node)
        .cloned()
        .unwrap_or_else(|| match snapshot.tree_payload::<SurfaceTree>(node) {
            Some(payload) => format!("orphan {}", render_surface(&payload)),
            None => "orphan <unknown>".to_owned(),
        })
}

/// Collects `(structural path, node)` pairs for one lowered-node-keyed view,
/// ordered canonically by path.
fn rows_by_lowered_path(
    snapshot: &plingo::reactive::Snapshot,
    paths: &HashMap<Node<LoweredTree>, String>,
    nodes: impl IntoIterator<Item = Node<LoweredTree>>,
) -> Vec<(String, Node<LoweredTree>)> {
    let mut rows: Vec<(String, Node<LoweredTree>)> = nodes
        .into_iter()
        .map(|node| (lowered_path(snapshot, paths, node.clone()), node))
        .collect();
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    rows.dedup_by(|left, right| left.0 == right.0);
    rows
}

/// Captures every present entry of every public view of this family.
pub fn semantic_digest(snapshot: &plingo::reactive::Snapshot) -> SemanticDigest {
    let mut digest = SemanticDigest::new();

    // Programs map.
    let mut uris = snapshot.inputs::<Programs>();
    uris.sort();
    for uri in uris {
        let row = snapshot
            .observe::<Programs>(uri.clone())
            .as_deref()
            .map(render_program)
            .unwrap_or_else(|| "absent".to_owned());
        digest.insert("programs", &uri, &row);
    }

    // Surface and lowered forests.
    let surface_paths =
        index_tree::<SurfaceTree>(snapshot, "surface_tree", render_surface, &mut digest);
    let lowered_paths =
        index_tree::<LoweredTree>(snapshot, "lowered_tree", render_lowered, &mut digest);

    // Root maps: document URI -> resolved structural root path.
    let mut roots = snapshot.inputs::<SurfaceRoots>();
    roots.sort();
    for uri in roots {
        let row = match snapshot.observe::<SurfaceRoots>(uri.clone()) {
            Some(root) => surface_path(snapshot, &surface_paths, root.as_ref().clone()),
            None => "absent".to_owned(),
        };
        digest.insert("surface_roots", &uri, &row);
    }
    let mut roots = snapshot.inputs::<LoweredRoots>();
    roots.sort();
    for uri in roots {
        let row = match snapshot.observe::<LoweredRoots>(uri.clone()) {
            Some(root) => lowered_path(snapshot, &lowered_paths, root.as_ref().clone()),
            None => "absent".to_owned(),
        };
        digest.insert("lowered_roots", &uri, &row);
    }

    // Provenance edge pairs resolved to both endpoints' structural paths.
    let origins = rows_by_lowered_path(
        snapshot,
        &lowered_paths,
        snapshot.inputs::<LoweredOrigins>(),
    );
    for (key, node) in origins {
        let row = match snapshot.observe::<LoweredOrigins>(node) {
            Some(source) => surface_path(snapshot, &surface_paths, source.as_ref().clone()),
            None => "absent".to_owned(),
        };
        digest.insert("lowered_origins", &key, &row);
    }
    let mut inverses: Vec<(String, Node<SurfaceTree>)> = snapshot
        .inputs::<LoweredBySource>()
        .into_iter()
        .map(|source| (surface_path(snapshot, &surface_paths, source.clone()), source))
        .collect();
    inverses.sort_by(|left, right| left.0.cmp(&right.0));
    inverses.dedup_by(|left, right| left.0 == right.0);
    for (key, source) in inverses {
        let row = match snapshot.observe::<LoweredBySource>(source) {
            Some(target) => lowered_path(snapshot, &lowered_paths, target.as_ref().clone()),
            None => "absent".to_owned(),
        };
        digest.insert("lowered_by_source", &key, &row);
    }

    // Scope graph: node payload multiset plus edge triples rendered from
    // both endpoints' payloads and the label, as sorted domain rows.
    let scopes = snapshot_nodes::<PipelineScope>(snapshot);
    let mut node_texts: Vec<String> = scopes
        .iter()
        .filter_map(|scope| {
            snapshot
                .graph_node::<ScopeGraph<PipelineScope>>(scope.node())
                .as_deref()
                .map(render_scope_node)
        })
        .collect();
    node_texts.sort();
    for (ordinal, text) in node_texts.iter().enumerate() {
        digest.insert_domain("scope_nodes", ordinal, text);
    }

    let mut labels: BTreeMap<String, ScopeLabel> = BTreeMap::new();
    labels.insert("ResolvesTo".to_owned(), ScopeLabel::ResolvesTo);
    for node in lowered_paths.keys() {
        let Some(payload) = snapshot.tree_payload::<LoweredTree>(node.clone()) else {
            continue;
        };
        match payload.as_ref() {
            LoweredNode::Definition(name) => {
                let label = ScopeLabel::Declaration(name.clone());
                labels.insert(render_scope_label(&label), label);
            }
            LoweredNode::Variable(name) => {
                let label = ScopeLabel::Reference(name.clone());
                labels.insert(render_scope_label(&label), label);
            }
            LoweredNode::Module | LoweredNode::ApplyAdd | LoweredNode::Integer(_) => {}
        }
    }
    let mut triples: Vec<String> = Vec::new();
    for scope in &scopes {
        let source_text = snapshot
            .graph_node::<ScopeGraph<PipelineScope>>(scope.node())
            .as_deref()
            .map(render_scope_node)
            .unwrap_or_else(|| "<unknown>".to_owned());
        for (label_text, label) in &labels {
            for target in snapshot_outgoing(snapshot, scope.clone(), label) {
                let target_text = snapshot
                    .graph_node::<ScopeGraph<PipelineScope>>(target.node())
                    .as_deref()
                    .map(render_scope_node)
                    .unwrap_or_else(|| "<unknown>".to_owned());
                triples.push(format!("({source_text},{label_text})->{target_text}"));
            }
        }
    }
    triples.sort();
    for (ordinal, triple) in triples.iter().enumerate() {
        digest.insert_domain("scope_edges", ordinal, triple);
    }

    // Document and per-node automatic scope joins.
    let mut document_scopes = snapshot.inputs::<DocumentScopes>();
    document_scopes.sort();
    for uri in document_scopes {
        let row = match snapshot.observe::<DocumentScopes>(uri.clone()) {
            Some(scope) => snapshot_scope(snapshot, scope.as_ref().clone())
                .as_deref()
                .map(render_scope_data)
                .unwrap_or_else(|| "absent".to_owned()),
            None => "absent".to_owned(),
        };
        digest.insert("document_scopes", &uri, &row);
    }

    let incoming =
        rows_by_lowered_path(snapshot, &lowered_paths, snapshot.inputs::<IncomingScopes>());
    for (key, node) in incoming {
        let row = match snapshot.observe::<IncomingScopes>(node) {
            Some(scope) => snapshot_scope(snapshot, scope.as_ref().clone())
                .as_deref()
                .map(render_scope_data)
                .unwrap_or_else(|| "absent".to_owned()),
            None => "absent".to_owned(),
        };
        digest.insert("incoming_scopes", &key, &row);
    }

    let reference_scopes =
        rows_by_lowered_path(snapshot, &lowered_paths, snapshot.inputs::<ReferenceScopes>());
    for (key, node) in reference_scopes {
        let row = match snapshot.observe::<ReferenceScopes>(node) {
            Some(scope) => snapshot
                .graph_node::<ScopeGraph<PipelineScope>>(scope.as_ref().node())
                .as_deref()
                .map(render_scope_node)
                .unwrap_or_else(|| "absent".to_owned()),
            None => "absent".to_owned(),
        };
        digest.insert("reference_scopes", &key, &row);
    }

    let candidates = rows_by_lowered_path(
        snapshot,
        &lowered_paths,
        snapshot.inputs::<ReferenceCandidates>(),
    );
    for (key, node) in candidates {
        let row = snapshot
            .observe::<ReferenceCandidates>(node)
            .as_deref()
            .map(|name| name.clone())
            .unwrap_or_else(|| "absent".to_owned());
        digest.insert("reference_candidates", &key, &row);
    }

    let resolutions =
        rows_by_lowered_path(snapshot, &lowered_paths, snapshot.inputs::<Resolutions>());
    for (key, node) in resolutions {
        let row = match snapshot.observe::<Resolutions>(node) {
            Some(resolution) => render_resolution(snapshot, &resolution),
            None => "absent".to_owned(),
        };
        digest.insert("resolutions", &key, &row);
    }

    let analyses = rows_by_lowered_path(snapshot, &lowered_paths, snapshot.inputs::<Analyses>());
    for (key, node) in analyses {
        let row = snapshot
            .observe::<Analyses>(node)
            .as_deref()
            .map(render_analysis)
            .unwrap_or_else(|| "absent".to_owned());
        digest.insert("analyses", &key, &row);
    }

    let diagnostics: Vec<(String, Node<LoweredTree>)> = {
        let mut rows = rows_by_lowered_path(
            snapshot,
            &lowered_paths,
            snapshot
                .inputs::<Diagnostics>()
                .into_iter()
                .filter_map(|input| match input {
                    ListKey::Slot(key, _) | ListKey::Len(key) => Some(key),
                }),
        );
        rows.dedup_by(|left, right| left.0 == right.0);
        rows
    };
    for (key, node) in diagnostics {
        let items = snapshot.list::<Diagnostics>(&node);
        let rendered: Vec<String> = items.iter().map(|item| format!("{item:?}")).collect();
        digest.insert("diagnostics", &key, &format!("[{}]", rendered.join(",")));
    }

    // Per-node and document aggregates.
    let summaries =
        rows_by_lowered_path(snapshot, &lowered_paths, snapshot.inputs::<NodeSummaries>());
    for (key, node) in summaries {
        let row = snapshot
            .observe::<NodeSummaries>(node)
            .as_deref()
            .map(render_summary)
            .unwrap_or_else(|| "absent".to_owned());
        digest.insert("node_summaries", &key, &row);
    }

    let mut summary_keys = snapshot.inputs::<DocumentSummaries>();
    summary_keys.sort();
    for uri in summary_keys {
        let row = snapshot
            .observe::<DocumentSummaries>(uri.clone())
            .as_deref()
            .map(render_summary)
            .unwrap_or_else(|| "absent".to_owned());
        digest.insert("document_summaries", &uri, &row);
    }

    digest
}
