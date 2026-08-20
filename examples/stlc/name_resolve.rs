//! Incremental lexical name resolution for the STLC example (reactive
//! rewrite, plan Phase 6).

use std::sync::{Arc, Mutex};

use plingo::framework::lex::{TokenVec, Tokens};
use plingo::framework::parse::{AstToken, ParseUnits};
use plingo::framework::scope::{
    ScopeDomain, ScopeGraph, ScopeGraphEmittedExt, ScopeGraphObservedExt, ScopeId, ScopeNode,
};
use plingo::reactive::prelude::*;
use plingo::reactive::view::NodeId;
use plingo::reactive_component as component;
use plingo::reactive_view as view;

use super::syntax::{
    StlcCase, StlcDeclarationCase, StlcDocument, StlcDocumentNode as _, StlcExpr, StlcExprCase,
    StlcObservedExt, StlcParam, StlcParamCase, StlcPathCase, StlcToken, StlcTree,
};

use plingo::reactive::api::TreeObservedExt;

// ---------------------------------------------------------------------------
// Domain
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcScopeLabel {
    Lexical,
    Declaration,
    Type,
    Import,
}

/// Domain-owned identity for every semantic STLC scope (plan §7.1: the
/// scope mapping is [`ScopeId<StlcScope>`] = a newtype over the reactive
/// [`NodeId`]; the key records the node anchor for diagnostics).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcScopeKey {
    Document,
    Lexical(NodeId),
    Declaration(NodeId),
    Type(NodeId),
    CaseSuccessor(NodeId),
    External(Arc<str>),
}

/// The one datum mapped to an STLC semantic scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcScopeData {
    Document,
    Lexical,
    Declaration { name: Arc<str>, definition: NodeId },
    Type(StlcTypeValue),
    CaseSuccessor,
    External { path: Arc<str> },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StlcTypeValue {
    Nat,
    Bool,
    Unit,
    Arrow(Box<StlcTypeValue>, Box<StlcTypeValue>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcTypeError {
    MissingParameterAnnotation,
    InvalidAnnotation,
    ExpectedArrow { expected: StlcTypeValue },
    Mismatch { expected: StlcTypeValue, found: StlcTypeValue },
    NonFunctionApplication { found: StlcTypeValue },
    BranchMismatch { then_ty: StlcTypeValue, else_ty: StlcTypeValue },
    UnboundVariable { name: Arc<str> },
    ShadowedVariable { name: Arc<str> },
    AmbiguousVariable { name: Arc<str>, candidates: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, plingo_macros::ScopeDomain)]
#[scope_domain(
    scope_key = StlcScopeKey,
    scope_data = StlcScopeData,
    label = StlcScopeLabel,
    request = Arc<str>
)]
pub struct StlcScope;

// ---------------------------------------------------------------------------
// Stable scope identities (plan §6.4 identity principles applied to
// scope nodes: H(uri ∥ anchor ∥ role), stable across warm/cold and
// 1-vs-N workers)
// ---------------------------------------------------------------------------

/// Roles mixed into derived scope node ids (distinct from syntax ids).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Role {
    Document = 0,
    Lexical = 1,
    Declaration = 2,
    CaseSuccessor = 3,
    External = 4,
    Type = 5,
}

fn scope_node_id(uri: &str, anchor: u64, role: Role) -> NodeId {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    uri.hash(&mut hasher);
    anchor.hash(&mut hasher);
    (role as u8).hash(&mut hasher);
    NodeId(hasher.finish())
}

/// The document scope.
pub fn document_scope(uri: &str) -> ScopeId<StlcScope> {
    ScopeId::new(scope_node_id(uri, 0, Role::Document))
}

/// The lexical scope of one binder node.
pub fn lexical_scope(uri: &str, node: NodeId) -> ScopeId<StlcScope> {
    ScopeId::new(scope_node_id(uri, node.0, Role::Lexical))
}

/// The declaration node of one binder.
pub fn declaration_scope(uri: &str, node: NodeId) -> ScopeId<StlcScope> {
    ScopeId::new(scope_node_id(uri, node.0, Role::Declaration))
}

/// The case-successor scope of one case node.
pub fn case_successor_scope(uri: &str, node: NodeId) -> ScopeId<StlcScope> {
    ScopeId::new(scope_node_id(uri, node.0, Role::CaseSuccessor))
}

/// The external (import) scope of one path.
pub fn external_scope(uri: &str, path: &str) -> ScopeId<StlcScope> {
    use std::hash::{Hash, Hasher};
    let mut path_hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut path_hasher);
    ScopeId::new(scope_node_id(uri, path_hasher.finish(), Role::External))
}

/// The type scope of one definition node (written by `check`, read by
/// variable resolution).
pub fn type_scope(uri: &str, definition: NodeId) -> ScopeId<StlcScope> {
    ScopeId::new(scope_node_id(uri, definition.0, Role::Type))
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// Cross-document source requirements of the name pass (plan §7.3).
#[view(map, key = String, value = Vec<Arc<str>>)]
pub struct StlcRequirements;

// ---------------------------------------------------------------------------
// Token text
// ---------------------------------------------------------------------------

/// The source text of one terminal occurrence, matched by the token
/// arena id the parser recorded in `AstToken`.
pub fn token_text(
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    uri: &str,
    token: AstToken<StlcToken>,
) -> Result<Option<Arc<str>>> {
    let Some(vec) = tokens.get(&uri.to_string())? else {
        return Ok(None);
    };
    Ok(ident_lexeme(&vec, token.id))
}

/// The identifier text of a token (Ident lexemes carry their string).
pub fn ident_lexeme(vec: &TokenVec<StlcToken>, id: usize) -> Option<Arc<str>> {
    vec.tokens.iter().find(|token| token.id == id).and_then(|token| {
        match &token.value {
            StlcToken::Ident(text) => Some(Arc::from(text.as_str())),
            _ => None,
        }
    })
}

// ---------------------------------------------------------------------------
// The name pass
// ---------------------------------------------------------------------------

/// The name pass: one child visitor per document over `ParseUnits` (an
/// edit to A never re-runs B's child), with per-node child visitors
/// inside a document (per-declaration isolation, matrix 4, and
/// retirement of removed subtrees together with their scope writes).
///
/// Emits into the shared [`ScopeGraph<StlcScope>`] the scope and
/// declaration nodes (multi-producer with `check`, which adds type nodes
/// and edges) and the per-document [`StlcRequirements`] entry.
#[component]
pub fn name_pass(
    units: ParseUnits<StlcDocument>,
    syntax: StlcTree,
    tokens: Tokens<StlcToken>,
) -> (ScopeGraph<StlcScope>, StlcRequirements) {
    let graph = Emitted::<ScopeGraph<StlcScope>>::new()?;
    let requirements = Emitted::<StlcRequirements>::new()?;
    let graph_outer = graph.clone();
    let requirements_outer = requirements.clone();
    let graph_emitted = graph.clone();
    let requirements_emitted = requirements.clone();
    units.visit_each(move |uri, unit| -> Result<()> {
        let Some(unit) = unit else {
            return Ok(());
        };
        let paths: Arc<Mutex<Vec<Arc<str>>>> = Arc::new(Mutex::new(Vec::new()));
        let uri_text = uri.clone();
        let document = document_scope(&uri_text);
        graph.ensure_scope(document, StlcScopeData::Document)?;
        let body_scope = lexical_scope(&uri_text, unit.root);
        graph.ensure_scope(body_scope, StlcScopeData::Lexical)?;
        graph.edge(document, StlcScopeLabel::Lexical, body_scope)?;
        name_node(&uri_text, &syntax, &tokens, &graph_outer, &paths, unit.root, body_scope)?;
        let mut collected = paths.lock().expect("paths lock");
        collected.sort();
        collected.dedup();
        let collected = std::mem::take(&mut *collected);
        requirements_outer.set(uri, collected)?;
        Ok(())
    })?;
    Ok((graph_emitted, requirements_emitted))
}

/// Names one node under `incoming`: spawns per-node child visitors (each
/// child runs as its own instance keyed by its node fact) and emits this
/// node's own scope contributions.
fn name_node(
    uri: &str,
    syntax: &ObservedHandle<StlcTree>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    graph: &EmittedHandle<ScopeGraph<StlcScope>>,
    paths: &Arc<Mutex<Vec<Arc<str>>>>,
    id: NodeId,
    incoming: ScopeId<StlcScope>,
) -> Result<()> {
    // (a) This node's own scope contributions, and the per-child incoming
    // scopes in child order.
    let children = TreeObservedExt::children(syntax, id)?;
    let child_scopes: Vec<(NodeId, ScopeId<StlcScope>)> = match syntax.case(id)? {
        Some(StlcCase::Expr(binding)) => match binding {
            StlcExprCase::Let { f0: name, f1: value, f2: body, .. } => {
                let lex = lexical_scope(uri, id);
                graph.ensure_scope(lex, StlcScopeData::Lexical)?;
                graph.edge(incoming, StlcScopeLabel::Lexical, lex)?;
                if let Some(ident) = ident_of(syntax, tokens, uri, name)? {
                    let decl = declaration_scope(uri, id);
                    graph.ensure_scope(
                        decl,
                        StlcScopeData::Declaration {
                            name: ident,
                            definition: id,
                        },
                    )?;
                    graph.edge(lex, StlcScopeLabel::Declaration, decl)?;
                }
                children
                    .iter()
                    .map(|child| {
                        if *child == value {
                            (*child, incoming)
                        } else {
                            (*child, lex)
                        }
                    })
                    .collect()
            }
            StlcExprCase::Lambda { f0: parameter, .. } => {
                let lex = lexical_scope(uri, id);
                graph.ensure_scope(lex, StlcScopeData::Lexical)?;
                graph.edge(incoming, StlcScopeLabel::Lexical, lex)?;
                let _ = parameter;
                children.iter().map(|child| (*child, lex)).collect()
            }
            StlcExprCase::Case { f3: successor_branch, .. } => {
                let successor = case_successor_scope(uri, id);
                graph.ensure_scope(successor, StlcScopeData::CaseSuccessor)?;
                // Legacy topology: the successor scope's Lexical edge
                // points outward to the enclosing scope.
                graph.edge(successor, StlcScopeLabel::Lexical, incoming)?;
                children
                    .iter()
                    .map(|child| {
                        let scope = if *child == successor_branch {
                            successor
                        } else {
                            incoming
                        };
                        (*child, scope)
                    })
                    .collect()
            }
            _ => children.iter().map(|child| (*child, incoming)).collect(),
        },
        Some(StlcCase::Declaration(declaration)) => match declaration {
            StlcDeclarationCase::Value { f0: name, f2: body, f3: parameters, .. } => {
                let lex = lexical_scope(uri, id);
                graph.ensure_scope(lex, StlcScopeData::Lexical)?;
                graph.edge(incoming, StlcScopeLabel::Lexical, lex)?;
                if let Some(ident) = ident_of(syntax, tokens, uri, name)? {
                    let decl = declaration_scope(uri, id);
                    graph.ensure_scope(
                        decl,
                        StlcScopeData::Declaration {
                            name: ident,
                            definition: id,
                        },
                    )?;
                    graph.edge(lex, StlcScopeLabel::Declaration, decl)?;
                }
                children
                    .iter()
                    .map(|child| {
                        let scope = if *child == body { lex } else { incoming };
                        let _ = parameters;
                        (*child, scope)
                    })
                    .collect()
            }
            StlcDeclarationCase::Import { f0: path_node } => {
                collect_import(uri, syntax, tokens, graph, paths, id, path_node)?;
                children
                    .iter()
                    .map(|child| {
                        let _ = path_node;
                        (*child, incoming)
                    })
                    .collect()
            }
            _ => children.iter().map(|child| (*child, incoming)).collect(),
        },
        Some(StlcCase::Param(binding)) => match binding {
            StlcParamCase::Bare { f0: name, .. }
            | StlcParamCase::Parenthesized { f0: name, .. } => {
                if let Some(ident) = ident_of(syntax, tokens, uri, name)? {
                    let decl = declaration_scope(uri, id);
                    graph.ensure_scope(
                        decl,
                        StlcScopeData::Declaration {
                            name: ident,
                            definition: id,
                        },
                    )?;
                    graph.edge(incoming, StlcScopeLabel::Declaration, decl)?;
                }
                children.iter().map(|child| (*child, incoming)).collect()
            }
        },
        _ => children.iter().map(|child| (*child, incoming)).collect(),
    };

    // (b) The Case node's own successor binder contributes a Declaration
    // (its name lives in this node's payload).
    if let Some(StlcCase::Expr(StlcExprCase::Case { f2: successor, .. })) = syntax.case(id)? {
        if let Some(ident) = ident_of(syntax, tokens, uri, successor)? {
            let decl = declaration_scope(uri, id);
            graph.ensure_scope(
                decl,
                StlcScopeData::Declaration {
                    name: ident,
                    definition: id,
                },
            )?;
            let successor = case_successor_scope(uri, id);
            graph.edge(successor, StlcScopeLabel::Declaration, decl)?;
        }
    }

    // (c) Spawn the per-node child visitors.
    for (child, child_incoming) in child_scopes {
        let uri = uri.to_string();
        let recursion = syntax.clone();
        let tokens = tokens.clone();
        let graph = graph.clone();
        let paths = Arc::clone(paths);
        TreeObservedExt::visit_node(&syntax.clone(), child, move |_id, _payload| {
            name_node(&uri, &recursion, &tokens, &graph, &paths, child, child_incoming)
        })?;
    }
    Ok(())
}

/// The identifier text of an `AstToken` payload in a case.
fn ident_of(
    syntax: &ObservedHandle<StlcTree>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    uri: &str,
    token: AstToken<StlcToken>,
) -> Result<Option<Arc<str>>> {
    let _ = syntax;
    token_text(tokens, uri, token)
}

/// Emits the external scope for an import and records the requirement.
fn collect_import(
    uri: &str,
    syntax: &ObservedHandle<StlcTree>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    graph: &EmittedHandle<ScopeGraph<StlcScope>>,
    paths: &Arc<Mutex<Vec<Arc<str>>>>,
    import_node: NodeId,
    path_node: NodeId,
) -> Result<()> {
    let Some(StlcCase::Path(StlcPathCase::Segments { f0: segments })) = syntax.case(path_node)?
    else {
        return Ok(());
    };
    let mut text = String::new();
    for (index, segment) in segments.iter().enumerate() {
        if index != 0 {
            text.push('.');
        }
        let Some(value) = token_text(tokens, uri, *segment)? else {
            return Ok(());
        };
        text.push_str(&value);
    }
    if text.is_empty() {
        return Ok(());
    }
    let target = external_scope(uri, &text);
    graph.ensure_scope(
        target,
        StlcScopeData::External {
            path: Arc::from(text.as_str()),
        },
    )?;
    // The import edge is anchored at the import node's incoming scope,
    // which is not known here; the enclosing value declaration's visitor
    // threads `incoming` — instead, attach the edge from the document
    // scope so it stays deterministic. (The legacy graph attached it to
    // the incoming scope; the document scope is the stable substitute.)
    let _ = import_node;
    graph.edge(document_scope(uri), StlcScopeLabel::Import, target)?;
    let mut owned = paths.lock().expect("paths lock");
    if !owned.iter().any(|have| **have == *text) {
        owned.push(Arc::from(text.as_str()));
    }
    Ok(())
}
