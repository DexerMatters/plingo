//! A complete public-view pipeline built only from semantic components.
//!
//! `Programs` owns a recursive surface tree.  A root component lowers that
//! tree into a second abstract tree, and node components project scopes,
//! resolutions, analyses, and summaries through returned effects.  Component
//! calls and `AstBox` identities own tree lifetime; no application code names
//! encoded tree facts or constructs a node identity.

use plingo::framework::scope::{
    Scope, ScopeDomain, ScopeGraph, ScopeNode, outgoing, snapshot_node, snapshot_nodes,
    snapshot_outgoing, snapshot_scope,
};
use plingo::prelude::*;
use plingo::reactive::Snapshot;
use plingo::reactive::digest::SemanticDigest;
use std::collections::{BTreeMap, HashMap};

// ---------------------------------------------------------------------------
// Semantic input and parser-independent trees
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Program {
    pub binding: String,
    pub value: i64,
    pub reference: Option<String>,
}
#[view]
pub struct Programs(Map<String, Program>);

/// The source tree is ordinary user data.  `AstBox` is the only child marker;
#[derive(Clone, Debug, PartialEq, Eq)]
#[abstract_tree(domain = String, tree = SurfaceTree)]
pub enum SurfaceNode {
    Document {
        declarations: Vec<AstBox<SurfaceNode>>,
    },
    Binding {
        name: String,
        value: AstBox<SurfaceNode>,
    },
    Add {
        operands: Vec<AstBox<SurfaceNode>>,
    },
    Number {
        value: i64,
    },
    Name {
        value: String,
    },
    Error {
        diagnostic: String,
    },
}

/// The lowered tree has an independent schema and is also recursive.
#[derive(Clone, Debug, PartialEq, Eq)]
#[abstract_tree(domain = String, tree = LoweredTree)]
pub enum LoweredNode {
    Module {
        declarations: Vec<AstBox<LoweredNode>>,
    },
    Definition {
        name: String,
        value: AstBox<LoweredNode>,
    },
    ApplyAdd {
        operands: Vec<AstBox<LoweredNode>>,
    },
    Integer {
        value: i64,
    },
    Variable {
        name: String,
    },
    Error {
        diagnostic: String,
    },
}

// ---------------------------------------------------------------------------
// Recursive surface construction
// ---------------------------------------------------------------------------

/// Map membership owns one source-document call per URI.  The entry payload
/// is intentionally not read here: payload-dependent child components own
/// their exact reads and retain their output identities across edits.
#[component]
pub fn build_surface(entry: Each<Programs>) -> Result<AstBox<SurfaceNode>> {
    surface_document(entry.key().clone())
}

#[component]
fn surface_document(uri: String) -> Result<AstBox<SurfaceNode>> {
    let declaration = surface_binding(uri)?;
    SurfaceNode::render(SurfaceNode::Document {
        declarations: vec![declaration],
    })
}

#[component]
fn surface_binding(uri: String) -> Result<AstBox<SurfaceNode>> {
    let Some(program) = Programs::get(&uri)? else {
        return SurfaceNode::render(SurfaceNode::Error {
            diagnostic: "missing program".to_owned(),
        });
    };
    let value = surface_add(uri)?;
    SurfaceNode::render(SurfaceNode::Binding {
        name: program.as_ref().binding.clone(),
        value,
    })
}

#[component]
fn surface_add(uri: String) -> Result<AstBox<SurfaceNode>> {
    let Some(program) = Programs::get(&uri)? else {
        return SurfaceNode::render(SurfaceNode::Error {
            diagnostic: "missing program".to_owned(),
        });
    };
    let number = surface_number(uri.clone())?;
    let reference = program
        .as_ref()
        .reference
        .as_ref()
        .map(|_| surface_name(uri.clone()))
        .transpose()?;
    let mut operands = vec![number];
    if let Some(reference) = reference {
        operands.push(reference);
    }
    SurfaceNode::render(SurfaceNode::Add { operands })
}

#[component]
fn surface_number(uri: String) -> Result<AstBox<SurfaceNode>> {
    let value = Programs::get(&uri)?.map_or(0, |program| program.as_ref().value);
    SurfaceNode::render(SurfaceNode::Number { value })
}

#[component]
fn surface_name(uri: String) -> Result<AstBox<SurfaceNode>> {
    let value = Programs::get(&uri)?
        .and_then(|program| program.as_ref().reference.clone())
        .unwrap_or_default();
    SurfaceNode::render(SurfaceNode::Name { value })
}

// ---------------------------------------------------------------------------
// Recursive lowering
// ---------------------------------------------------------------------------

/// The only externally rooted lowering computation.  Its child calls create
/// the declaration and expression outputs; no projection map or raw topology
/// write is needed.
#[component]
pub fn lower_document(source: AstBox<SurfaceNode>) -> Result<AstBox<LoweredNode>> {
    let value = match source.view()? {
        SurfaceNodeView::Document(document) => LoweredNode::Module {
            declarations: document
                .declarations()?
                .iter()
                .map(lower_node)
                .collect::<Result<Vec<_>>>()?,
        },
        SurfaceNodeView::Error(error) => LoweredNode::Error {
            diagnostic: error.diagnostic()?.as_ref().clone(),
        },
        _ => LoweredNode::Error {
            diagnostic: "surface root is not a document".to_owned(),
        },
    };
    LoweredNode::render(value)
}

/// One recursive child component per source node.  Each invocation reads the
/// discriminant and only the fields used by its selected variant.
#[component]
pub fn lower_node(source: AstBox<SurfaceNode>) -> Result<AstBox<LoweredNode>> {
    let value = match source.view()? {
        SurfaceNodeView::Document(document) => LoweredNode::Module {
            declarations: document
                .declarations()?
                .iter()
                .map(lower_node)
                .collect::<Result<Vec<_>>>()?,
        },
        SurfaceNodeView::Binding(binding) => LoweredNode::Definition {
            name: binding.name()?.as_ref().clone(),
            value: lower_node(binding.value()?)?,
        },
        SurfaceNodeView::Add(add) => LoweredNode::ApplyAdd {
            operands: add
                .operands()?
                .iter()
                .map(lower_node)
                .collect::<Result<Vec<_>>>()?,
        },
        SurfaceNodeView::Number(number) => LoweredNode::Integer {
            value: *number.value()?.as_ref(),
        },
        SurfaceNodeView::Name(name) => LoweredNode::Variable {
            name: name.value()?.as_ref().clone(),
        },
        SurfaceNodeView::Error(error) => LoweredNode::Error {
            diagnostic: error.diagnostic()?.as_ref().clone(),
        },
    };
    LoweredNode::render(value)
}

// ---------------------------------------------------------------------------
// Scope graph and exact semantic joins
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

/// Every key in the derived maps is a typed lowered-tree identity.
#[view]
pub struct DocumentScopes(Map<AstBox<LoweredNode>, Scope<PipelineScope>>);

#[view]
pub struct IncomingScopes(Map<AstBox<LoweredNode>, Scope<PipelineScope>>);

#[view]
pub struct ReferenceScopes(Map<AstBox<LoweredNode>, Scope<PipelineScope>>);

#[view]
pub struct ReferenceCandidates(Map<AstBox<LoweredNode>, String>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Resolution {
    Resolved { declaration: Scope<PipelineScope> },
    Unbound { name: String },
}

#[view]
pub struct Resolutions(Map<AstBox<LoweredNode>, Resolution>);

/// One root component per mounted target-tree node.  Only roots emit a
/// document scope; non-roots return no desired effects.
#[component]
pub fn emit_document_scope(
    root: AstBox<LoweredNode>,
) -> Result<
    Option<(
        Set<DocumentScopes>,
        Set<IncomingScopes>,
        GraphRender<ScopeGraph<PipelineScope>>,
    )>,
> {
    if root.parent()?.is_some() {
        return Ok(None);
    }
    let scope = Scope::<PipelineScope>::automatic()?;
    let graph = scope.clone().render(ScopeNode::Scope(ScopeData::Document));
    Ok(Some((
        DocumentScopes::set(root.clone(), scope.clone()),
        IncomingScopes::set(root, scope),
        graph,
    )))
}

/// Emits one declaration/reference node and its incoming-scope edge.  A
/// component may return several independent graph bucket patches; each patch
/// is still owned by the returned effect and is retracted when omitted.
#[component]
pub fn emit_node_scope(
    node: AstBox<LoweredNode>,
) -> Result<(
    Option<Set<IncomingScopes>>,
    Option<Set<ReferenceScopes>>,
    Option<GraphRender<ScopeGraph<PipelineScope>>>,
    Option<GraphRender<ScopeGraph<PipelineScope>>>,
)> {
    let parent = node.parent()?;
    let incoming = match parent.as_ref() {
        Some(parent) => IncomingScopes::get(parent)?.map(|scope| scope.as_ref().clone()),
        None => IncomingScopes::get(&node)?.map(|scope| scope.as_ref().clone()),
    };
    let Some(incoming) = incoming else {
        return Ok((None, None, None, None));
    };
    let incoming_output = parent
        .is_some()
        .then(|| IncomingScopes::set(node.clone(), incoming.clone()));

    let output = match node.view()? {
        LoweredNodeView::Definition(definition) => {
            let name = definition.name()?.as_ref().clone();
            let scope = Scope::<PipelineScope>::automatic()?;
            let graph = scope
                .clone()
                .render(ScopeNode::Declaration(ScopeData::Definition(name.clone())));
            let incoming_patch = incoming
                .clone()
                .patch()
                .bucket(ScopeLabel::Declaration(name), vec![scope.clone()]);
            (incoming_output, None, Some(graph), Some(incoming_patch))
        }
        LoweredNodeView::Variable(variable) => {
            let name = variable.name()?.as_ref().clone();
            let scope = Scope::<PipelineScope>::automatic()?;
            let graph = scope
                .clone()
                .render(ScopeNode::Reference(ScopeData::Reference(name.clone())));
            let incoming_patch = incoming
                .clone()
                .patch()
                .bucket(ScopeLabel::Reference(name), vec![scope.clone()]);
            (
                incoming_output,
                Some(ReferenceScopes::set(node, scope)),
                Some(graph),
                Some(incoming_patch),
            )
        }
        LoweredNodeView::Module(_)
        | LoweredNodeView::ApplyAdd(_)
        | LoweredNodeView::Integer(_)
        | LoweredNodeView::Error(_) => (incoming_output, None, None, None),
    };
    Ok(output)
}

/// Publishes only reference candidates.  Omission retracts an old candidate
/// when a node changes away from the variable variant.
#[component]
pub fn publish_candidate(node: AstBox<LoweredNode>) -> Result<Option<Set<ReferenceCandidates>>> {
    let LoweredNodeView::Variable(variable) = node.view()? else {
        return Ok(None);
    };
    Ok(Some(ReferenceCandidates::set(
        node,
        variable.name()?.as_ref().clone(),
    )))
}

/// Resolves one candidate by reading its exact candidate, incoming scope, and
/// declaration bucket.  A resolved edge is a bucket patch on the existing
/// reference scope; an unbound result intentionally returns no graph patch.
#[component]
pub fn resolve_pass(
    node: AstBox<LoweredNode>,
) -> Result<
    Option<(
        Set<Resolutions>,
        Option<GraphRender<ScopeGraph<PipelineScope>>>,
    )>,
> {
    let Some(name) = ReferenceCandidates::get(&node)? else {
        return Ok(None);
    };
    let Some(reference_scope) = ReferenceScopes::get(&node)? else {
        return Ok(None);
    };
    let Some(incoming_scope) = IncomingScopes::get(&node)? else {
        return Ok(None);
    };
    let name = name.as_ref().clone();
    let reference_scope = reference_scope.as_ref().clone();
    let incoming_scope = incoming_scope.as_ref().clone();
    let declaration = outgoing(incoming_scope, &ScopeLabel::Declaration(name.clone()))?
        .into_iter()
        .next();
    let (resolution, edge) = match declaration {
        Some(declaration) => (
            Resolution::Resolved {
                declaration: declaration.clone(),
            },
            Some(
                reference_scope
                    .patch()
                    .bucket(ScopeLabel::ResolvesTo, vec![declaration]),
            ),
        ),
        None => (Resolution::Unbound { name }, None),
    };
    Ok(Some((Resolutions::set(node, resolution), edge)))
}

#[view]
pub struct AnalysisLabels(Map<AstBox<LoweredNode>, String>);

#[view]
pub struct AnalysisOrigins(Map<AstBox<LoweredNode>, bool>);

#[view]
pub struct AnalysisScopePresence(Map<AstBox<LoweredNode>, bool>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeAnalysis {
    pub label: String,
    pub diagnostics: usize,
    pub has_origin: bool,
    pub has_scope: bool,
}

#[view]
pub struct Analyses(Map<AstBox<LoweredNode>, NodeAnalysis>);

#[view]
pub struct Diagnostics(List<AstBox<LoweredNode>, String>);

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct DocumentSummary {
    pub nodes: usize,
    pub diagnostics: usize,
}

#[view]
pub struct NodeSummaries(Map<AstBox<LoweredNode>, DocumentSummary>);

#[view]
pub struct DocumentSummaries(Map<AstBox<LoweredNode>, DocumentSummary>);

#[component]
pub fn analysis_label(node: AstBox<LoweredNode>) -> Result<Option<Set<AnalysisLabels>>> {
    let label = match node.view()? {
        LoweredNodeView::Module(_) => "module".to_owned(),
        LoweredNodeView::Definition(definition) => {
            format!("definition {}", definition.name()?.as_ref())
        }
        LoweredNodeView::ApplyAdd(_) => "apply add".to_owned(),
        LoweredNodeView::Integer(integer) => format!("integer {}", integer.value()?.as_ref()),
        LoweredNodeView::Error(error) => {
            format!("error {}", error.diagnostic()?.as_ref())
        }
        LoweredNodeView::Variable(variable) => {
            let name = variable.name()?.as_ref().clone();
            match Resolutions::get(&node)?.as_deref() {
                Some(Resolution::Resolved { declaration }) => {
                    let target_name = match snapshot_node_from_effect(declaration.clone())? {
                        Some(ScopeNode::Declaration(ScopeData::Definition(target))) => target,
                        _ => "<invalid declaration>".to_owned(),
                    };
                    format!("reference {name} -> {target_name}")
                }
                Some(Resolution::Unbound { .. }) => format!("reference {name} -> <unbound>"),
                None => format!("reference {name} -> <pending>"),
            }
        }
    };
    Ok(Some(AnalysisLabels::set(node, label)))
}

/// Reads a graph node through the semantic scope API while remaining usable
/// inside an active effect context.
fn snapshot_node_from_effect(
    scope: Scope<PipelineScope>,
) -> Result<Option<ScopeNode<PipelineScope>>> {
    plingo::framework::scope::observe_node(scope).map(|node| node.map(|node| node.as_ref().clone()))
}

#[component]
pub fn analysis_origin(node: AstBox<LoweredNode>) -> Result<Set<AnalysisOrigins>> {
    Ok(AnalysisOrigins::set(node, true))
}

#[component]
pub fn analysis_scope_presence(
    node: AstBox<LoweredNode>,
) -> Result<Option<Set<AnalysisScopePresence>>> {
    if IncomingScopes::get(&node)?.is_some() {
        Ok(Some(AnalysisScopePresence::set(node, true)))
    } else {
        Ok(None)
    }
}

#[component]
pub fn analysis_diagnostics(node: AstBox<LoweredNode>) -> Result<Option<Replace<Diagnostics>>> {
    let resolution = Resolutions::get(&node)?;
    let Some(Resolution::Unbound { name }) = resolution.as_deref() else {
        return Ok(None);
    };
    Ok(Some(Diagnostics::replace(
        node,
        vec![format!("unbound reference {name}")],
    )))
}

#[component]
pub fn join_analyses(node: AstBox<LoweredNode>) -> Result<Option<Set<Analyses>>> {
    let Some(label) = AnalysisLabels::get(&node)? else {
        return Ok(None);
    };
    let has_origin = AnalysisOrigins::get(&node)?.is_some_and(|value| *value);
    let has_scope = AnalysisScopePresence::get(&node)?.is_some_and(|value| *value);
    let diagnostics = Diagnostics::len(&node)?;
    Ok(Some(Analyses::set(
        node,
        NodeAnalysis {
            label: label.as_ref().clone(),
            diagnostics,
            has_origin,
            has_scope,
        },
    )))
}

fn lowered_children(node: &AstBox<LoweredNode>) -> Result<Vec<AstBox<LoweredNode>>> {
    match node.view()? {
        LoweredNodeView::Module(module) => Ok(module.declarations()?.to_vec()),
        LoweredNodeView::Definition(definition) => Ok(vec![definition.value()?]),
        LoweredNodeView::ApplyAdd(add) => Ok(add.operands()?.to_vec()),
        LoweredNodeView::Integer(_) | LoweredNodeView::Variable(_) | LoweredNodeView::Error(_) => {
            Ok(Vec::new())
        }
    }
}

#[component]
pub fn node_summary(node: AstBox<LoweredNode>) -> Result<Option<Set<NodeSummaries>>> {
    let Some(analysis) = Analyses::get(&node)? else {
        return Ok(None);
    };
    let mut summary = DocumentSummary {
        nodes: 1,
        diagnostics: analysis.as_ref().diagnostics,
    };
    for child in lowered_children(&node)? {
        let Some(child_summary) = NodeSummaries::get(&child)? else {
            return Ok(None);
        };
        summary.nodes += child_summary.as_ref().nodes;
        summary.diagnostics += child_summary.as_ref().diagnostics;
    }
    Ok(Some(NodeSummaries::set(node, summary)))
}

#[component]
pub fn document_summary(node: AstBox<LoweredNode>) -> Result<Option<Set<DocumentSummaries>>> {
    if node.parent()?.is_some() {
        return Ok(None);
    }
    let Some(summary) = NodeSummaries::get(&node)? else {
        return Ok(None);
    };
    Ok(Some(DocumentSummaries::set(node, summary.as_ref().clone())))
}

// ---------------------------------------------------------------------------
// Snapshot digest: generated tree readers, typed map/list readers, scopes
// ---------------------------------------------------------------------------

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
        SurfaceNode::Document { .. } => "Document".to_owned(),
        SurfaceNode::Binding { name, .. } => format!("Binding({name:?})"),
        SurfaceNode::Add { .. } => "Add".to_owned(),
        SurfaceNode::Number { value } => format!("Number({value})"),
        SurfaceNode::Name { value } => format!("Name({value:?})"),
        SurfaceNode::Error { diagnostic } => format!("Error({diagnostic:?})"),
    }
}

fn render_lowered(payload: &LoweredNode) -> String {
    match payload {
        LoweredNode::Module { .. } => "Module".to_owned(),
        LoweredNode::Definition { name, .. } => format!("Definition({name:?})"),
        LoweredNode::ApplyAdd { .. } => "ApplyAdd".to_owned(),
        LoweredNode::Integer { value } => format!("Integer({value})"),
        LoweredNode::Variable { name } => format!("Variable({name:?})"),
        LoweredNode::Error { diagnostic } => format!("Error({diagnostic:?})"),
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

fn render_resolution(snapshot: &Snapshot, resolution: &Resolution) -> String {
    match resolution {
        Resolution::Resolved { declaration } => {
            let target = snapshot_node(snapshot, declaration.clone())
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

fn walk_surface(
    tree: &SnapshotTree<SurfaceTree>,
    node: AstBox<SurfaceNode>,
    uri: &str,
    path: &str,
    paths: &mut HashMap<AstBox<SurfaceNode>, String>,
    digest: &mut SemanticDigest,
) {
    let Ok(value) = tree.materialize(node.clone()) else {
        return;
    };
    digest.insert(
        "surface_tree",
        &format!("{uri}#{path}"),
        &render_surface(&value),
    );
    paths.insert(node, path.to_owned());
    let children: Vec<AstBox<SurfaceNode>> = match value {
        SurfaceNode::Document { declarations } => declarations,
        SurfaceNode::Binding { value, .. } => vec![value],
        SurfaceNode::Add { operands } => operands,
        SurfaceNode::Number { .. } | SurfaceNode::Name { .. } | SurfaceNode::Error { .. } => {
            Vec::new()
        }
    };
    for (index, child) in children.into_iter().enumerate() {
        walk_surface(tree, child, uri, &format!("{path}{index}"), paths, digest);
    }
}

fn walk_lowered(
    tree: &SnapshotTree<LoweredTree>,
    node: AstBox<LoweredNode>,
    uri: &str,
    path: &str,
    paths: &mut HashMap<AstBox<LoweredNode>, String>,
    digest: &mut SemanticDigest,
) {
    let Ok(value) = tree.materialize(node.clone()) else {
        return;
    };
    digest.insert(
        "lowered_tree",
        &format!("{uri}#{path}"),
        &render_lowered(&value),
    );
    paths.insert(node, path.to_owned());
    let children: Vec<AstBox<LoweredNode>> = match value {
        LoweredNode::Module { declarations } => declarations,
        LoweredNode::Definition { value, .. } => vec![value],
        LoweredNode::ApplyAdd { operands } => operands,
        LoweredNode::Integer { .. } | LoweredNode::Variable { .. } | LoweredNode::Error { .. } => {
            Vec::new()
        }
    };
    for (index, child) in children.into_iter().enumerate() {
        walk_lowered(tree, child, uri, &format!("{path}{index}"), paths, digest);
    }
}

fn lowered_path(
    paths: &HashMap<AstBox<LoweredNode>, String>,
    node: &AstBox<LoweredNode>,
) -> String {
    paths
        .get(node)
        .cloned()
        .unwrap_or_else(|| "orphan".to_owned())
}

fn lowered_rows(
    paths: &HashMap<AstBox<LoweredNode>, String>,
    nodes: impl IntoIterator<Item = AstBox<LoweredNode>>,
) -> Vec<(String, AstBox<LoweredNode>)> {
    let mut rows: Vec<_> = nodes
        .into_iter()
        .map(|node| (lowered_path(paths, &node), node))
        .collect();
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    rows.dedup_by(|left, right| left.0 == right.0);
    rows
}

/// Captures every present semantic view using generated tree readers and
/// typed scope snapshot helpers.  Materialization is used only by this
/// tooling digest, never by the granularity-sensitive components above.
pub fn semantic_digest(snapshot: &Snapshot) -> SemanticDigest {
    let mut digest = SemanticDigest::new();
    let mut uris = snapshot.inputs::<Programs>();
    uris.sort();

    for uri in &uris {
        if let Some(program) = snapshot.observe::<Programs>(uri.clone()) {
            digest.insert("programs", uri, &render_program(program.as_ref()));
        }
    }

    let surface_tree = snapshot.tree::<SurfaceTree>();
    let lowered_tree = snapshot.tree::<LoweredTree>();
    let mut surface_paths = HashMap::new();
    let mut lowered_paths = HashMap::new();
    for uri in &uris {
        for (index, root) in surface_tree.roots(uri).enumerate() {
            walk_surface(
                &surface_tree,
                root,
                uri,
                &format!("{index}"),
                &mut surface_paths,
                &mut digest,
            );
        }
        let roots: Vec<_> = lowered_tree.roots(uri).collect();
        for (index, root) in roots.into_iter().enumerate() {
            let path = format!("{index}");
            digest.insert("lowered_roots", uri, &format!("{uri}#{path}"));
            walk_lowered(
                &lowered_tree,
                root,
                uri,
                &path,
                &mut lowered_paths,
                &mut digest,
            );
        }
    }

    let lowered_nodes: Vec<_> = lowered_paths.keys().cloned().collect();
    let lowered_rows = lowered_rows(&lowered_paths, lowered_nodes.clone());

    for (path, node) in &lowered_rows {
        if let Some(value) = snapshot.observe::<DocumentScopes>(node.clone()) {
            let row = snapshot_scope(snapshot, value.as_ref().clone())
                .as_deref()
                .map(render_scope_data)
                .unwrap_or_else(|| "absent".to_owned());
            digest.insert("document_scopes", path, &row);
        }
        if let Some(value) = snapshot.observe::<IncomingScopes>(node.clone()) {
            let row = snapshot_scope(snapshot, value.as_ref().clone())
                .as_deref()
                .map(render_scope_data)
                .unwrap_or_else(|| "absent".to_owned());
            digest.insert("incoming_scopes", path, &row);
        }
        if let Some(value) = snapshot.observe::<ReferenceScopes>(node.clone()) {
            let row = snapshot_node(snapshot, value.as_ref().clone())
                .as_deref()
                .map(render_scope_node)
                .unwrap_or_else(|| "absent".to_owned());
            digest.insert("reference_scopes", path, &row);
        }
        if let Some(value) = snapshot.observe::<ReferenceCandidates>(node.clone()) {
            digest.insert("reference_candidates", path, value.as_ref());
        }
        if let Some(value) = snapshot.observe::<Resolutions>(node.clone()) {
            digest.insert(
                "resolutions",
                path,
                &render_resolution(snapshot, value.as_ref()),
            );
        }
        if let Some(value) = snapshot.observe::<Analyses>(node.clone()) {
            digest.insert("analyses", path, &render_analysis(value.as_ref()));
        }
        let diagnostics = snapshot.list::<Diagnostics>(node);
        if !diagnostics.is_empty() {
            let rendered: Vec<String> =
                diagnostics.iter().map(|item| format!("{item:?}")).collect();
            digest.insert("diagnostics", path, &format!("[{}]", rendered.join(",")));
        }
        if let Some(value) = snapshot.observe::<NodeSummaries>(node.clone()) {
            digest.insert("node_summaries", path, &render_summary(value.as_ref()));
        }
        if let Some(value) = snapshot.observe::<DocumentSummaries>(node.clone()) {
            digest.insert("document_summaries", path, &render_summary(value.as_ref()));
        }
    }

    let scopes = snapshot_nodes::<PipelineScope>(snapshot);
    let mut node_texts: Vec<String> = scopes
        .iter()
        .filter_map(|scope| {
            snapshot_node(snapshot, scope.clone())
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
    for node in &lowered_nodes {
        let Ok(value) = lowered_tree.materialize(node.clone()) else {
            continue;
        };
        match value {
            LoweredNode::Definition { name, .. } => {
                let label = ScopeLabel::Declaration(name);
                labels.insert(render_scope_label(&label), label);
            }
            LoweredNode::Variable { name } => {
                let label = ScopeLabel::Reference(name);
                labels.insert(render_scope_label(&label), label);
            }
            _ => {}
        }
    }
    let mut triples = Vec::new();
    for scope in &scopes {
        let Some(source) = snapshot_node(snapshot, scope.clone()) else {
            continue;
        };
        let source_text = render_scope_node(source.as_ref());
        for (label_text, label) in &labels {
            for target in snapshot_outgoing(snapshot, scope.clone(), label) {
                let Some(target_node) = snapshot_node(snapshot, target) else {
                    continue;
                };
                triples.push(format!(
                    "({source_text},{label_text})->{}",
                    render_scope_node(target_node.as_ref())
                ));
            }
        }
    }
    triples.sort();
    for (ordinal, triple) in triples.iter().enumerate() {
        digest.insert_domain("scope_edges", ordinal, triple);
    }

    digest
}
