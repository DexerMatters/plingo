//! A dependency-driven transform from the parser's homogeneous syntax tree
//! into a separate heterogeneous lowered tree.
//!
//! Each source payload owns one target payload/provenance row. Edge and
//! child-order components own the corresponding target topology facts, while
//! the root component owns document root membership. Consequently a source
//! payload rewrite updates that target payload without rebuilding an unchanged
//! ancestor or sibling; a child-order rewrite updates only the corresponding
//! target order/link facts.

use std::sync::Arc;

use plingo::framework::parse::{
    ParserTreeEdges, ParserTreeOrders, ParserTreePayloads, ParserTreeRoots, ParserTreeStatuses,
};
use plingo::reactive::component::{EachKey, Read, Write};
use plingo::reactive::kind::{Map, Tree, TreeFact, TreeKey, emit_view};
use plingo::reactive::prelude::*;
use plingo::reactive::view::Node;
use plingo::reactive::{Engine, Result};
use reactive_macros::view;

use super::syntax::{
    TransformDeclarationNode, TransformDocument, TransformDocumentNode, TransformExprNode,
    TransformNode, TransformTree,
};

/// Payload language deliberately distinct from the parser's generated tree.
/// The transform erases surface tokens and assigns semantic roles instead.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LoweredNode {
    Module,
    Binding,
    Sum,
    Difference,
    Group,
    Number,
    Name,
    ParseError,
    Missing,
}

/// One lowered forest per source URI.
#[view]
pub struct LoweredTree(Tree<String, LoweredNode>);

/// Provenance is a separate view so consumers can join a lowered node back to
/// exactly one syntax-tree node without traversing either tree.
#[view]
pub struct LoweredOrigin(Map<Node<LoweredTree>, Node<TransformTree>>);

/// Stable source-to-target node identity join.
#[view]
pub struct LoweredNodes(Map<Node<TransformTree>, Node<LoweredTree>>);

/// One source payload owns exactly one target payload and provenance row.
#[reactive_macros::component]
pub fn lower_source_node(
    key: EachKey<ParserTreePayloads<TransformDocument>>,
    payloads: Read<ParserTreePayloads<TransformDocument>>,
    nodes: Read<LoweredNodes>,
    node_writes: Write<LoweredNodes>,
) -> Result<()> {
    let Some(payload) = payloads.get(&key)? else {
        return Ok(());
    };
    let target = match nodes.get(&key)? {
        Some(target) => target.as_ref().clone(),
        None => emit_view::<LoweredTree>()?.allocate()?,
    };
    emit_view::<LoweredTree>()?.put(
        TreeKey::Payload(target.clone()),
        Some(TreeFact::Payload(lowered_payload_kind(&payload))),
    )?;
    node_writes.insert(key.clone(), target.clone())?;
    emit_view::<LoweredOrigin>()?.insert(target, key)?;
    Ok(())
}

/// One source edge owns one target parent fact and one target link fact.
#[reactive_macros::component]
pub fn lower_source_edge(
    key: EachKey<ParserTreeEdges<TransformDocument>>,
    edges: Read<ParserTreeEdges<TransformDocument>>,
    nodes: Read<LoweredNodes>,
) -> Result<()> {
    if edges.get(&key)?.is_none() {
        return Ok(());
    }
    let (source_parent, source_child) = key;
    let Some(parent) = nodes.get(&source_parent)? else {
        return Ok(());
    };
    let Some(child) = nodes.get(&source_child)? else {
        return Ok(());
    };
    let tree = emit_view::<LoweredTree>()?;
    tree.put(
        TreeKey::Parent(child.as_ref().clone()),
        Some(TreeFact::Parent(Some(parent.as_ref().clone()))),
    )?;
    tree.put(
        TreeKey::ChildLink(parent.as_ref().clone(), child.as_ref().clone()),
        Some(TreeFact::Link(child.as_ref().clone())),
    )?;
    Ok(())
}

/// One source child-order row owns one target child-order fact.
#[reactive_macros::component]
pub fn lower_source_order(
    key: EachKey<ParserTreeOrders<TransformDocument>>,
    orders: Read<ParserTreeOrders<TransformDocument>>,
    nodes: Read<LoweredNodes>,
) -> Result<()> {
    let Some(order) = orders.get(&key)? else {
        return Ok(());
    };
    let Some(parent) = nodes.get(&key)? else {
        return Ok(());
    };
    let mut children = Vec::with_capacity(order.len());
    for source_child in order.iter() {
        let Some(child) = nodes.get(source_child)? else {
            return Ok(());
        };
        children.push(child.as_ref().clone());
    }
    emit_view::<LoweredTree>()?.put(
        TreeKey::ChildOrder(parent.as_ref().clone()),
        Some(TreeFact::Order(Arc::from(children))),
    )
}

/// One source root row owns one target document root list.
#[reactive_macros::component]
pub fn lower_source_root(
    key: EachKey<ParserTreeRoots<TransformDocument>>,
    roots: Read<ParserTreeRoots<TransformDocument>>,
    nodes: Read<LoweredNodes>,
) -> Result<()> {
    let Some(source_root) = roots.get(&key)? else {
        return Ok(());
    };
    let Some(target_root) = nodes.get(source_root.as_ref())? else {
        return Ok(());
    };
    emit_view::<LoweredTree>()?.replace_roots(&key, &[target_root.as_ref().clone()])
}

/// Installs the parser-backed lowering components as one transform pass.
pub fn lower_pass_install(engine: &mut Engine) -> Result<()> {
    lower_source_node_install(engine)?;
    lower_source_edge_install(engine)?;
    lower_source_order_install(engine)?;
    lower_source_root_install(engine)?;
    Ok(())
}

fn lowered_payload_kind(payload: &TransformNode) -> LoweredNode {
    match payload {
        TransformNode::Document(TransformDocumentNode::Program { .. }) => LoweredNode::Module,
        TransformNode::Document(TransformDocumentNode::Error { .. })
        | TransformNode::Declaration(TransformDeclarationNode::Error { .. })
        | TransformNode::Expr(TransformExprNode::Error { .. }) => LoweredNode::ParseError,
        TransformNode::Declaration(TransformDeclarationNode::Binding { .. }) => {
            LoweredNode::Binding
        }
        TransformNode::Expr(TransformExprNode::Add { .. }) => LoweredNode::Sum,
        TransformNode::Expr(TransformExprNode::Subtract { .. }) => LoweredNode::Difference,
        TransformNode::Expr(TransformExprNode::Group { .. }) => LoweredNode::Group,
        TransformNode::Expr(TransformExprNode::Number { .. }) => LoweredNode::Number,
        TransformNode::Expr(TransformExprNode::Name { .. }) => LoweredNode::Name,
    }
}

// ---------------------------------------------------------------------------
// Semantic digest (follow-up plan §4): complete parser-backed family content,
// ID-erased and canonically ordered.
// ---------------------------------------------------------------------------

use std::collections::HashMap;

use plingo::framework::parse::{AstSnapshot, AstSnapshots, ParseStatus};
use plingo::framework::source::{SourceSnapshot, source_snapshot};
use plingo::reactive::digest::SemanticDigest;

/// The structural path of a child at `index` under `parent` (`""` is the root).
fn child_path(parent: &str, index: usize) -> String {
    if parent.is_empty() {
        index.to_string()
    } else {
        format!("{parent}.{index}")
    }
}

/// Renders one parse status so recovery states appear in digests.
fn render_status(status: &ParseStatus) -> String {
    match status {
        ParseStatus::Clean => "clean".to_owned(),
        ParseStatus::Recovered { diagnostics } => format!("recovered({diagnostics})"),
        ParseStatus::Unrecoverable { diagnostics } => format!("unrecoverable({diagnostics})"),
    }
}

/// The exact source lexeme a Number/Name leaf was built from, resolved by
/// joining through the origin view into the parser's payload and token
/// coordinates, then slicing the committed source text.
fn leaf_lexeme(
    snapshot: &Snapshot,
    ast: Option<&AstSnapshot>,
    source: Option<&SourceSnapshot>,
    node: Node<LoweredTree>,
) -> String {
    let missing = || "?".to_owned();
    let Some(origin) = snapshot
        .observe::<LoweredOrigin>(node.clone())
        .map(|origin| origin.as_ref().clone())
    else {
        return missing();
    };
    let Some(payload) = snapshot.observe::<ParserTreePayloads<TransformDocument>>(origin) else {
        return missing();
    };
    let token = match payload.as_ref() {
        TransformNode::Expr(TransformExprNode::Number { f0, .. })
        | TransformNode::Expr(TransformExprNode::Name { f0, .. }) => *f0,
        _ => return missing(),
    };
    let Some(entry) = ast.and_then(|ast| ast.token(token)) else {
        return missing();
    };
    let (start, end): (usize, usize) = entry.span.range.into();
    source
        .and_then(|source| source.byte_slice(start..end).ok())
        .unwrap_or_else(missing)
}
fn render_payload(
    snapshot: &Snapshot,
    ast: Option<&AstSnapshot>,
    source: Option<&SourceSnapshot>,
    node: Node<LoweredTree>,
) -> String {
    let kind = snapshot
        .tree_payload::<LoweredTree>(node.clone())
        .map(|kind| (*kind).clone())
        .unwrap_or(LoweredNode::Missing);
    match kind {
        LoweredNode::Number | LoweredNode::Name => {
            format!("{kind:?}({})", leaf_lexeme(snapshot, ast, source, node))
        }
        other => format!("{other:?}"),
    }
}

/// Captures every lowered node, provenance link, and parse status of this
/// family across every document, ID-erased and canonically ordered.
pub fn semantic_digest(snapshot: &Snapshot) -> SemanticDigest {
    let mut digest = SemanticDigest::new();

    // Document domain: every parser root plus every lowered forest root,
    // canonically sorted.
    let mut uris: Vec<String> = snapshot.inputs::<ParserTreeRoots<TransformDocument>>();
    uris.extend(
        snapshot
            .inputs::<LoweredTree>()
            .into_iter()
            .filter_map(|input| match input {
                TreeKey::RootOrder(uri) => Some(uri),
                _ => None,
            }),
    );
    uris.sort();
    uris.dedup();

    for uri in uris {
        let status = snapshot
            .observe::<ParserTreeStatuses>(uri.clone())
            .map(|status| (*status).clone());
        if let Some(status) = status {
            digest.insert("parse", &uri, &render_status(&status));
        }
        let ast = snapshot
            .observe::<AstSnapshots<TransformDocument>>(uri.clone())
            .map(|document| document.arc().clone());
        let source = source_snapshot(snapshot, &uri);

        // One parser-view DFS builds an id-to-path map so origin rows name
        // structural positions instead of raw ordinals.
        let mut source_paths: HashMap<Node<TransformTree>, String> = HashMap::new();
        if let Some(root) = snapshot
            .observe::<ParserTreeRoots<TransformDocument>>(uri.clone())
            .map(|root| root.as_ref().clone())
        {
            let mut stack = vec![(root, String::new())];
            while let Some((node, path)) = stack.pop() {
                source_paths.insert(node.clone(), path.clone());
                let children = snapshot
                    .observe::<ParserTreeOrders<TransformDocument>>(node)
                    .map(|order| order.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                for (index, child) in children.into_iter().enumerate() {
                    stack.push((child, child_path(&path, index)));
                }
            }
        }

        for root in snapshot.tree_roots_of::<LoweredTree>(&uri) {
            let mut stack = vec![(root, String::new())];
            while let Some((node, path)) = stack.pop() {
                digest.insert(
                    "lowered",
                    &format!("{uri}#{path}"),
                    &render_payload(snapshot, ast.as_deref(), source.as_ref(), node.clone()),
                );
                let source_path = snapshot
                    .observe::<LoweredOrigin>(node.clone())
                    .map(|origin| origin.as_ref().clone())
                    .and_then(|origin| source_paths.get(&origin).cloned());
                digest.insert(
                    "origin",
                    &format!("{uri}#{path}"),
                    &match source_path {
                        Some(source_path) => format!("{uri}#{source_path}"),
                        None => "detached".to_owned(),
                    },
                );
                for (index, child) in snapshot
                    .tree_children::<LoweredTree>(node)
                    .into_iter()
                    .enumerate()
                {
                    stack.push((child, child_path(&path, index)));
                }
            }
        }
    }
    digest
}
