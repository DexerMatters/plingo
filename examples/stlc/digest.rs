//! Canonical, identity-erased digest for the STLC public semantic views.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;

use plingo::framework::lex::{Tokens, observe_token};
use plingo::framework::parse::{AstToken, ParseDiagnostics, ParseStatus, ParserTreeStatuses};
use plingo::framework::scope::{ScopeGraph, ScopeNode};
use plingo::reactive::abstract_tree::{AstBox, SnapshotTree};
use plingo::reactive::digest::SemanticDigest;
use plingo::reactive::{Snapshot, View};

use super::check::{
    StlcDefinitionTypes, StlcExpectedTypes, StlcSynthesizedTypes, StlcTypeDiagnostics,
};
use super::name_resolve::{
    StlcContinuationScopes, StlcDeclarationScopes, StlcIncomingScopes, StlcReferenceCandidates,
    StlcResolution, StlcResolvedReferences, StlcRootScopes, StlcScope, StlcScopeData,
    StlcScopeLabel,
};
use super::structural::{
    StlcLowered, StlcLoweredOrigin, StlcLoweredSummary, StlcLoweringDiagnostics, StlcNodeIndex,
};
use super::syntax::{
    StlcDeclaration, StlcDeclarationView, StlcDocument, StlcDocumentView, StlcExpr, StlcExprView,
    StlcParam, StlcParamView, StlcPath, StlcToken, StlcTree, StlcType, StlcTypeAtom,
    StlcTypeAtomView, StlcTypeView,
};

fn add_path(
    paths: &mut HashMap<AstBox<()>, String>,
    kinds: &mut BTreeMap<String, String>,
    node: AstBox<()>,
    path: String,
    kind: &str,
) {
    paths.insert(node, path.clone());
    kinds.insert(path, kind.to_owned());
}

fn visit_document(
    tree: &SnapshotTree<StlcTree>,
    node: AstBox<StlcDocument>,
    path: String,
    paths: &mut HashMap<AstBox<()>, String>,
    kinds: &mut BTreeMap<String, String>,
) -> plingo::Result<()> {
    add_path(paths, kinds, node.erased(), path.clone(), "document");
    if let StlcDocumentView::Lines(lines) = tree.view(node.clone())? {
        for (index, child) in lines.declarations()?.iter().enumerate() {
            visit_declaration(
                tree,
                child,
                format!("{path}.declaration[{index}]"),
                paths,
                kinds,
            )?;
        }
    }
    Ok(())
}

fn visit_declaration(
    tree: &SnapshotTree<StlcTree>,
    node: AstBox<StlcDeclaration>,
    path: String,
    paths: &mut HashMap<AstBox<()>, String>,
    kinds: &mut BTreeMap<String, String>,
) -> plingo::Result<()> {
    add_path(paths, kinds, node.erased(), path.clone(), "declaration");
    if let StlcDeclarationView::Value(value) = tree.view(node.clone())? {
        if let Some(annotation) = value.annotation()? {
            visit_type(tree, annotation, format!("{path}.annotation"), paths, kinds)?;
        }
        for (index, parameter) in value.parameters()?.iter().enumerate() {
            visit_param(
                tree,
                parameter,
                format!("{path}.parameter[{index}]"),
                paths,
                kinds,
            )?;
        }
        visit_expr(tree, value.body()?, format!("{path}.body"), paths, kinds)?;
    } else if let StlcDeclarationView::Import(import) = tree.view(node.clone())? {
        visit_path(tree, import.path()?, format!("{path}.path"), paths, kinds)?;
    } else if let StlcDeclarationView::Export(export) = tree.view(node.clone())? {
        visit_path(tree, export.path()?, format!("{path}.path"), paths, kinds)?;
    }
    Ok(())
}

fn visit_path(
    tree: &SnapshotTree<StlcTree>,
    node: AstBox<StlcPath>,
    path: String,
    paths: &mut HashMap<AstBox<()>, String>,
    kinds: &mut BTreeMap<String, String>,
) -> plingo::Result<()> {
    add_path(paths, kinds, node.erased(), path, "path");
    let _ = tree.view(node)?;
    Ok(())
}

fn visit_param(
    tree: &SnapshotTree<StlcTree>,
    node: AstBox<StlcParam>,
    path: String,
    paths: &mut HashMap<AstBox<()>, String>,
    kinds: &mut BTreeMap<String, String>,
) -> plingo::Result<()> {
    add_path(paths, kinds, node.erased(), path.clone(), "parameter");
    let annotation = match tree.view(node.clone())? {
        StlcParamView::Bare(param) => param.annotation()?,
        StlcParamView::Parenthesized(param) => param.annotation()?,
    };
    if let Some(annotation) = annotation {
        visit_type(tree, annotation, format!("{path}.annotation"), paths, kinds)?;
    }
    Ok(())
}

fn visit_type(
    tree: &SnapshotTree<StlcTree>,
    node: AstBox<StlcType>,
    path: String,
    paths: &mut HashMap<AstBox<()>, String>,
    kinds: &mut BTreeMap<String, String>,
) -> plingo::Result<()> {
    add_path(paths, kinds, node.erased(), path.clone(), "type");
    match tree.view(node)? {
        StlcTypeView::Arrow(arrow) => {
            visit_type_atom(tree, arrow.left()?, format!("{path}.left"), paths, kinds)?;
            visit_type(tree, arrow.right()?, format!("{path}.right"), paths, kinds)?;
        }
        StlcTypeView::Atom(atom) => {
            visit_type_atom(tree, atom.atom()?, format!("{path}.atom"), paths, kinds)?;
        }
        StlcTypeView::Error(_) => {}
    }
    Ok(())
}

fn visit_type_atom(
    tree: &SnapshotTree<StlcTree>,
    node: AstBox<StlcTypeAtom>,
    path: String,
    paths: &mut HashMap<AstBox<()>, String>,
    kinds: &mut BTreeMap<String, String>,
) -> plingo::Result<()> {
    add_path(paths, kinds, node.erased(), path.clone(), "type-atom");
    if let StlcTypeAtomView::Parenthesized(parenthesized) = tree.view(node)? {
        visit_type(
            tree,
            parenthesized.ty()?,
            format!("{path}.type"),
            paths,
            kinds,
        )?;
    }
    Ok(())
}

fn visit_expr(
    tree: &SnapshotTree<StlcTree>,
    node: AstBox<StlcExpr>,
    path: String,
    paths: &mut HashMap<AstBox<()>, String>,
    kinds: &mut BTreeMap<String, String>,
) -> plingo::Result<()> {
    add_path(paths, kinds, node.erased(), path.clone(), "expression");
    match tree.view(node)? {
        StlcExprView::If(if_) => {
            visit_expr(
                tree,
                if_.condition()?,
                format!("{path}.condition"),
                paths,
                kinds,
            )?;
            visit_expr(tree, if_.when_true()?, format!("{path}.then"), paths, kinds)?;
            visit_expr(
                tree,
                if_.when_false()?,
                format!("{path}.else"),
                paths,
                kinds,
            )?;
        }
        StlcExprView::Case(case) => {
            visit_expr(
                tree,
                case.scrutinee()?,
                format!("{path}.scrutinee"),
                paths,
                kinds,
            )?;
            visit_expr(
                tree,
                case.zero_branch()?,
                format!("{path}.zero"),
                paths,
                kinds,
            )?;
            visit_expr(
                tree,
                case.successor_branch()?,
                format!("{path}.successor"),
                paths,
                kinds,
            )?;
        }
        StlcExprView::Let(let_) => {
            visit_expr(tree, let_.value()?, format!("{path}.value"), paths, kinds)?;
            visit_expr(tree, let_.body()?, format!("{path}.body"), paths, kinds)?;
        }
        StlcExprView::Lambda(lambda) => {
            visit_param(
                tree,
                lambda.parameter()?,
                format!("{path}.parameter"),
                paths,
                kinds,
            )?;
            visit_expr(tree, lambda.body()?, format!("{path}.body"), paths, kinds)?;
        }
        StlcExprView::Add(add) => {
            visit_expr(tree, add.left()?, format!("{path}.left"), paths, kinds)?;
            visit_expr(tree, add.right()?, format!("{path}.right"), paths, kinds)?;
        }
        StlcExprView::Apply(apply) => {
            visit_expr(
                tree,
                apply.function()?,
                format!("{path}.function"),
                paths,
                kinds,
            )?;
            visit_expr(
                tree,
                apply.argument()?,
                format!("{path}.argument"),
                paths,
                kinds,
            )?;
        }
        StlcExprView::Succ(succ) => {
            visit_expr(tree, succ.value()?, format!("{path}.value"), paths, kinds)?;
        }
        StlcExprView::Group(group) => {
            visit_expr(
                tree,
                group.expression()?,
                format!("{path}.expression"),
                paths,
                kinds,
            )?;
        }
        StlcExprView::True(_)
        | StlcExprView::False(_)
        | StlcExprView::Number(_)
        | StlcExprView::Variable(_)
        | StlcExprView::Unit(_)
        | StlcExprView::Error(_) => {}
    }
    Ok(())
}

fn tree_paths(
    snapshot: &Snapshot,
) -> plingo::Result<(HashMap<AstBox<()>, String>, BTreeMap<String, String>)> {
    let tree = snapshot.tree::<StlcTree>();
    let mut paths = HashMap::new();
    let mut kinds = BTreeMap::new();
    for domain in tree.domains() {
        for (index, root) in tree.roots(&domain).enumerate() {
            visit_document(
                &tree,
                root,
                format!("{domain}#{index}"),
                &mut paths,
                &mut kinds,
            )?;
        }
    }
    Ok((paths, kinds))
}

fn node_path(paths: &HashMap<AstBox<()>, String>, node: &AstBox<()>) -> String {
    paths
        .get(node)
        .cloned()
        .unwrap_or_else(|| "orphan".to_owned())
}

fn insert_ast_map<V>(
    digest: &mut SemanticDigest,
    snapshot: &Snapshot,
    paths: &HashMap<AstBox<()>, String>,
    name: &str,
) where
    V: View<Input = AstBox<()>>,
    V::Output: std::fmt::Debug,
{
    for key in snapshot.inputs::<V>() {
        if let Some(value) = snapshot.observe::<V>(key.clone()) {
            digest.insert(name, &node_path(paths, &key), &format!("{value:?}"));
        }
    }
}

fn insert_definitions(digest: &mut SemanticDigest, snapshot: &Snapshot) {
    for key in snapshot.inputs::<StlcDefinitionTypes>() {
        if let Some(value) = snapshot.observe::<StlcDefinitionTypes>(key.clone()) {
            digest.insert("definitions", &format!("{key:?}"), &format!("{value:?}"));
        }
    }
}

fn insert_expected(
    digest: &mut SemanticDigest,
    snapshot: &Snapshot,
    paths: &HashMap<AstBox<()>, String>,
) {
    for (parent, child) in snapshot.inputs::<StlcExpectedTypes>() {
        if let Some(value) = snapshot.observe::<StlcExpectedTypes>((parent.clone(), child.clone()))
        {
            digest.insert(
                "expected",
                &format!("{}>{}", node_path(paths, &parent), node_path(paths, &child)),
                &format!("{value:?}"),
            );
        }
    }
}

fn insert_list<V>(
    digest: &mut SemanticDigest,
    snapshot: &Snapshot,
    paths: &HashMap<AstBox<()>, String>,
    name: &str,
) where
    V: plingo::reactive::kind::ListView<Key = AstBox<()>>,
    V::Item: std::fmt::Debug,
{
    for domain in snapshot.list_domains::<V>() {
        let items = snapshot.list::<V>(&domain);
        let base = node_path(paths, &domain);
        digest.insert(
            name,
            &format!("{base}.len"),
            &format!("Len({})", items.len()),
        );
        for (index, value) in items.iter().enumerate() {
            digest.insert(
                name,
                &format!("{base}.slot[{index}]"),
                &format!("Item({value:?})"),
            );
        }
    }
}
fn scope_data(data: &StlcScopeData, _paths: &HashMap<AstBox<()>, String>) -> String {
    match data {
        StlcScopeData::Document => "document".to_owned(),
        StlcScopeData::Lexical => "lexical".to_owned(),
        StlcScopeData::CaseSuccessor => "case-successor".to_owned(),
        StlcScopeData::External { path } => format!("external({path})"),
        StlcScopeData::Declaration { name } => format!("declaration({name})"),
    }
}

fn scope_label(label: &StlcScopeLabel) -> String {
    match label {
        StlcScopeLabel::Lexical => "lexical".to_owned(),
        StlcScopeLabel::Declaration(name) => format!("declaration({name})"),
        StlcScopeLabel::Import(path) => format!("import({path})"),
    }
}

fn insert_graph(
    digest: &mut SemanticDigest,
    snapshot: &Snapshot,
    _paths: &HashMap<AstBox<()>, String>,
) {
    let mut rows = Vec::new();
    for node in snapshot.graph_nodes::<ScopeGraph<StlcScope>>() {
        let Some(payload) = snapshot.graph_node::<ScopeGraph<StlcScope>>(node) else {
            continue;
        };
        let row = match payload.as_ref() {
            ScopeNode::Scope(data) => format!("node scope {}", scope_data(data, _paths)),
            ScopeNode::Declaration(data) => {
                format!("node declaration {}", scope_data(data, _paths))
            }
            ScopeNode::Reference(data) => {
                format!("node reference {}", scope_data(data, _paths))
            }
        };
        rows.push(row);
    }
    for (_node, label, targets) in snapshot.graph_buckets::<ScopeGraph<StlcScope>>() {
        let target_rows = targets
            .iter()
            .filter_map(|target| snapshot.graph_node::<ScopeGraph<StlcScope>>(target.clone()))
            .map(|payload| match payload.as_ref() {
                ScopeNode::Scope(data) => scope_data(data, _paths),
                ScopeNode::Declaration(data) => scope_data(data, _paths),
                ScopeNode::Reference(data) => scope_data(data, _paths),
            })
            .collect::<Vec<_>>();
        rows.push(format!(
            "bucket {} -> {:?}",
            scope_label(&label),
            target_rows
        ));
    }
    rows.sort();
    for (index, row) in rows.iter().enumerate() {
        digest.insert("graph", &format!("#{index:06}"), row);
    }
}

/// Captures every committed public semantic view without exposing raw node or
/// graph ordinals.  Tree paths are derived from the generated child readers.
pub fn stlc_digest(snapshot: &Snapshot) -> SemanticDigest {
    let (paths, kinds) = tree_paths(snapshot).unwrap_or_default();
    let mut digest = SemanticDigest::new();

    for (path, kind) in kinds {
        digest.insert("tree", &path, &kind);
    }
    for key in snapshot.inputs::<Tokens<StlcToken>>() {
        if let Some(tokens) = snapshot.observe::<Tokens<StlcToken>>(key.clone()) {
            let mut values = String::new();
            for token in &tokens.tokens[..] {
                if !values.is_empty() {
                    values.push('|');
                }
                let _ = write!(
                    &mut values,
                    "{}..{}:{:?}",
                    token.start,
                    token.start + token.length,
                    token.value
                );
            }
            digest.insert("tokens", &key, &values);
        }
    }
    for key in snapshot.inputs::<ParserTreeStatuses>() {
        if let Some(status) = snapshot.observe::<ParserTreeStatuses>(key.clone()) {
            digest.insert("parse", &key, &format!("{status:?}"));
        }
    }
    for key in snapshot.inputs::<ParseDiagnostics>() {
        if let Some(value) = snapshot.observe::<ParseDiagnostics>(key.clone()) {
            digest.insert(
                "parse-diagnostics",
                &format!("{key:?}"),
                &format!("{value:?}"),
            );
        }
    }

    insert_ast_map::<StlcIncomingScopes>(&mut digest, snapshot, &paths, "incoming");
    insert_ast_map::<StlcRootScopes>(&mut digest, snapshot, &paths, "roots");
    insert_ast_map::<StlcContinuationScopes>(&mut digest, snapshot, &paths, "continuations");
    insert_definitions(&mut digest, snapshot);
    insert_ast_map::<StlcReferenceCandidates>(&mut digest, snapshot, &paths, "candidates");
    insert_ast_map::<StlcResolvedReferences>(&mut digest, snapshot, &paths, "resolved");
    insert_ast_map::<StlcSynthesizedTypes>(&mut digest, snapshot, &paths, "synthesized");
    insert_expected(&mut digest, snapshot, &paths);
    insert_list::<StlcTypeDiagnostics>(&mut digest, snapshot, &paths, "type-diagnostics");
    insert_ast_map::<StlcNodeIndex>(&mut digest, snapshot, &paths, "node-index");
    insert_ast_map::<StlcLowered>(&mut digest, snapshot, &paths, "lowered");
    insert_ast_map::<StlcLoweredOrigin>(&mut digest, snapshot, &paths, "lowered-origin");
    insert_ast_map::<StlcLoweredSummary>(&mut digest, snapshot, &paths, "lowered-summary");
    insert_graph(&mut digest, snapshot, &paths);

    digest
}

#[allow(dead_code)]
fn token_value(token: AstToken<StlcToken>) -> plingo::Result<Option<Arc<StlcToken>>> {
    observe_token(token)
}

#[allow(dead_code)]
fn _status_is_clean(status: &ParseStatus) -> bool {
    matches!(status, ParseStatus::Clean)
}
