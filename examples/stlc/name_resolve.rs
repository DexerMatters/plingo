//! Incremental lexical name resolution for the STLC example (plan §6).
//!
//! One child visitor per document over `TreeParseUnits`, with per-node
//! child visitors inside a document. Every scope-graph bucket has exactly
//! one writer: a node's visitor owns the scopes anchored at that node and
//! the edges leaving them; the PARENT visitor owns the edge from the shared
//! incoming scope to each child's lexical scope (plan §5.7).

use std::sync::Arc;

use plingo::framework::lex::observe_token;
use plingo::framework::parse::{AstToken, TreeParseUnits};
pub use plingo::framework::scope::{Scope, ScopeGraph, ScopeNode};
use plingo::framework::scope::outgoing;
use plingo::reactive::prelude::*;
use plingo::reactive::view::Node;
use reactive_macros::view;

use super::syntax::{
    StlcCase, StlcDeclarationCase, StlcDocument, StlcExprCase, StlcParamCase, StlcPathCase,
    StlcToken, StlcTree,
};

// ---------------------------------------------------------------------------
// Domain
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcScopeLabel {
    Lexical,
    Declaration(Arc<str>),
    Type,
    Import(Arc<str>),
}
/// The one datum mapped to an STLC semantic scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcScopeData {
    Document,
    Lexical,
    Declaration {
        name: Arc<str>,
        definition: Node<StlcTree>,
    },
    Type(StlcTypeValue),
    CaseSuccessor,
    External {
        path: Arc<str>,
    },
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
    Mismatch {
        expected: StlcTypeValue,
        found: StlcTypeValue,
    },
    NonFunctionApplication {
        found: StlcTypeValue,
    },
    BranchMismatch {
        then_ty: StlcTypeValue,
        else_ty: StlcTypeValue,
    },
    UnboundVariable {
        name: Arc<str>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, plingo_macros::ScopeDomain)]
#[scope_domain(scope_data = StlcScopeData, label = StlcScopeLabel, request = Arc<str>)]
pub struct StlcScope;

// ---------------------------------------------------------------------------
// Stable scope identities (plan §6.4 principles applied to scope nodes)
// ---------------------------------------------------------------------------

/// Roles mixed into derived scope node ids (distinct from syntax ids).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Role {
    Document,
    Lexical,
    Declaration,
    CaseSuccessor,
    External,
    Type,
}

/// `H(domain ∥ uri ∥ role ∥ anchor)` — stable across warm/cold builds and
/// 1-vs-N workers.
pub fn anchored_scope(anchor: &impl std::hash::Hash) -> Scope<StlcScope> {
    Scope::anchored(anchor)
}

/// The document scope.
pub fn document_scope(uri: &str) -> Scope<StlcScope> {
    anchored_scope(&(uri, Role::Document, 0u64))
}

/// The lexical scope of one binder node.
pub fn lexical_scope(uri: &str, node: Node<StlcTree>) -> Scope<StlcScope> {
    anchored_scope(&(uri, Role::Lexical, node))
}

/// The declaration node of one binder.
pub fn declaration_scope(uri: &str, node: Node<StlcTree>) -> Scope<StlcScope> {
    anchored_scope(&(uri, Role::Declaration, node))
}

/// The case-successor scope of one case node.
pub fn case_successor_scope(uri: &str, node: Node<StlcTree>) -> Scope<StlcScope> {
    anchored_scope(&(uri, Role::CaseSuccessor, node))
}

/// The external (import) scope of one path.
pub fn external_scope(uri: &str, path: &str) -> Scope<StlcScope> {
    anchored_scope(&(uri, Role::External, path))
}

/// The type scope of one typed node (written by `check`, read by variable
/// resolution).
pub fn type_scope(uri: &str, node: Node<StlcTree>) -> Scope<StlcScope> {
    anchored_scope(&(uri, Role::Type, node))
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// Cross-document source requirements of the name pass: one map entry per
/// document (plan §7.3, replacing the engine-invisible accumulator).
#[view]
pub struct StlcRequirements(Map<String, Vec<Arc<str>>>);

#[view]
pub struct StlcEnclosingScopes(Map<Node<StlcTree>, Scope<StlcScope>>);

/// Nodes whose payload is a reference (variable) candidate. The resolver is
/// keyed off this map so a node that can never be a reference never spawns
/// a resolver child (plan §16.2).
#[view]
pub struct StlcReferenceCandidates(Map<Node<StlcTree>, ()>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcResolution {
    Resolved { declaration: Scope<StlcScope> },
    Unbound { name: Arc<str> },
}

#[view]
pub struct StlcResolvedReferences(Map<Node<StlcTree>, StlcResolution>);
// ---------------------------------------------------------------------------
// Token text
// ---------------------------------------------------------------------------

/// The identifier text of one terminal occurrence. A missing token
/// publication resolves no idents.
/// The identifier text of one terminal occurrence.
fn token_text(token: AstToken<StlcToken>) -> Result<Option<Arc<str>>> {
    let Some(value) = observe_token::<StlcToken>(token)? else {
        return Ok(None);
    };
    Ok(match value.as_ref() {
        StlcToken::Ident(text) => Some(Arc::from(text.as_str())),
        _ => None,
    })
}

fn ident_lexeme_of(token: AstToken<StlcToken>) -> Result<Option<Arc<str>>> {
    token_text(token).map(|text| text.filter(|value| !value.is_empty()))
}

fn parameter_ident(parameter: Node<StlcTree>) -> Result<Option<Arc<str>>> {
    match StlcTree::observe_case(parameter)? {
        Some(StlcCase::Param(StlcParamCase::Bare { f0: name, .. }))
        | Some(StlcCase::Param(StlcParamCase::Parenthesized { f0: name, .. })) => {
            ident_lexeme_of(name)
        }
        _ => Ok(None),
    }
}


// ---------------------------------------------------------------------------
// The name pass
// ---------------------------------------------------------------------------

/// The name pass root: one child per document.
pub fn name_pass(_: ()) -> Result<()> {
    run_each_key::<TreeParseUnits<StlcDocument>, _>(name_document)
}

pub fn name_document(uri: String) -> Result<()> {
    let Some(unit) = observe_view::<TreeParseUnits<StlcDocument>>()?.get(&uri)? else {
        return emit_view::<StlcRequirements>()?.remove(uri);
    };
    let Some(root) = unit.root else {
        return emit_view::<StlcRequirements>()?
            .insert(uri.clone(), Vec::new());
    };
    let graph = emit_view::<ScopeGraph<StlcScope>>()?;
    let document = document_scope(&uri);
    graph.set_node(document.node(), ScopeNode::Scope(StlcScopeData::Document))?;
    let body_scope = lexical_scope(&uri, root);
    graph.set_node(body_scope.node(), ScopeNode::Scope(StlcScopeData::Lexical))?;
    run(
        |(uri, id, incoming): (String, Node<StlcTree>, Scope<StlcScope>)| {
            name_node(uri, id, incoming)
        },
        (uri.clone(), root, body_scope),
    )?;
    emit_view::<StlcRequirements>()?.insert(uri, Vec::new())
}

/// Names one node under `incoming` and spawns its per-node child visitors.
///
/// Bucket ownership (plan §5.7): this visitor writes only scopes anchored
/// at `id` and edges leaving them. The edge from the SHARED `incoming`
/// scope to a child's lexical scope belongs to THIS visitor (the parent),
fn name_node(uri: String, id: Node<StlcTree>, incoming: Scope<StlcScope>) -> Result<()> {
    let case = StlcTree::observe_case(id)?;
    emit_view::<StlcEnclosingScopes>()?.insert(id, incoming)?;
    if matches!(case, Some(StlcCase::Expr(StlcExprCase::Variable { .. }))) {
        emit_view::<StlcReferenceCandidates>()?.insert(id, ())?;
    }
    let graph = emit_view::<ScopeGraph<StlcScope>>()?;
    // The node's own contributions for binder shapes whose scope anchors
    // at this node.
    match &case {
        Some(StlcCase::Expr(StlcExprCase::Let { f0: name, .. })) => {
            let lex = lexical_scope(&uri, id);
            graph.set_node(lex.node(), ScopeNode::Scope(StlcScopeData::Lexical))?;
            graph.link(
                lex.node(),
                StlcScopeLabel::Lexical,
                incoming.node(),
            )?;
            if let Some(ident) = ident_lexeme_of(*name)? {
                let decl = declaration_scope(&uri, id);
                graph.set_node(
                    decl.node(),
                    ScopeNode::Declaration(StlcScopeData::Declaration {
                        name: ident.clone(),
                        definition: id,
                    }),
                )?;
                graph.link(
                    lex.node(),
                    StlcScopeLabel::Declaration(ident),
                    decl.node(),
                )?;
            }
        }
        Some(StlcCase::Expr(StlcExprCase::Lambda { .. })) => {
            let lex = lexical_scope(&uri, id);
            graph.set_node(lex.node(), ScopeNode::Scope(StlcScopeData::Lexical))?;
            graph.link(
                lex.node(),
                StlcScopeLabel::Lexical,
                incoming.node(),
            )?;
        }
        Some(StlcCase::Declaration(StlcDeclarationCase::Value { f0: name, .. })) => {
            let lex = lexical_scope(&uri, id);
            graph.set_node(lex.node(), ScopeNode::Scope(StlcScopeData::Lexical))?;
            graph.link(lex.node(), StlcScopeLabel::Lexical, incoming.node())?;
            let ident = ident_lexeme_of(*name)?;
            if let Some(ident) = ident {
                let decl = declaration_scope(&uri, id);
                graph.set_node(
                    decl.node(),
                    ScopeNode::Declaration(StlcScopeData::Declaration {
                        name: ident.clone(),
                        definition: id,
                    }),
                )?;
                graph.link(
                    lex.node(),
                    StlcScopeLabel::Declaration(ident),
                    decl.node(),
                )?;
            }
        }
        Some(StlcCase::Expr(StlcExprCase::Case { f2: successor, .. })) => {
            let successor_scope = case_successor_scope(&uri, id);
            graph.set_node(successor_scope.node(), ScopeNode::Scope(StlcScopeData::CaseSuccessor))?;
            // Legacy topology: the successor scope's Lexical edge points
            // outward to the enclosing scope. The source (successor scope)
            // is anchored here, so this visitor owns the bucket.
            graph.link(
                successor_scope.node(),
                StlcScopeLabel::Lexical,
                incoming.node(),
            )?;
            if let Some(ident) = ident_lexeme_of(*successor)? {
                let decl = declaration_scope(&uri, id);
                graph.set_node(
                    decl.node(),
                    ScopeNode::Declaration(StlcScopeData::Declaration {
                        name: ident.clone(),
                        definition: id,
                    }),
                )?;
                graph.link(
                    successor_scope.node(),
                    StlcScopeLabel::Declaration(ident),
                    decl.node(),
                )?;
            }
        }
        _ => {}
    }

    // Keyed child routing (plan §16 child-link relationship): one stable
    // effect per child link. An inserted declaration creates exactly one
    // new relationship; unchanged declarations are not re-enumerated when
    // a sibling changes.
    run_each_child_of::<StlcTree, _>(id, {
        let uri = uri.clone();
        move |parent, child| route_name_child(&uri, parent, child)
    })?;

    // Import declarations collect their external path.
    if let Some(StlcCase::Declaration(StlcDeclarationCase::Import { f0: path_node })) = &case {
        collect_import(&uri, path_node)?;
    }
    Ok(())
}

/// Routes ONE child of `parent` through the scope graph and continues the
/// name pass under the child's incoming scope (plan §16). Reads only the
/// parent's case and enclosing scope plus this child's own case; the
/// child's routing is independent of every sibling.
fn route_name_child(uri: &str, parent: Node<StlcTree>, child: Node<StlcTree>) -> Result<()> {
    let parent_case = StlcTree::observe_case(parent)?;
    let Some(parent_incoming) = observe_view::<StlcEnclosingScopes>()?.get(&parent)? else {
        return Ok(());
    };
    let child_case = StlcTree::observe_case(child)?;
    // A binder node creates its own lexical scope; the enclosing parent
    // remains the input scope passed to the child.
    let child_scope = match &parent_case {
        Some(StlcCase::Expr(StlcExprCase::Let { .. }))
        | Some(StlcCase::Expr(StlcExprCase::Lambda { .. }))
        | Some(StlcCase::Declaration(StlcDeclarationCase::Value { .. })) => {
            Some(lexical_scope(uri, parent))
        }
        _ => None,
    };
    let incoming_child = match &child_case {
        Some(StlcCase::Expr(StlcExprCase::Case { f3: successor_branch, .. }))
            if child == *successor_branch =>
        {
            case_successor_scope(uri, parent)
        }
        Some(StlcCase::Param(_)) => {
            let owner = child_scope.unwrap_or(*parent_incoming);
            let graph = emit_view::<ScopeGraph<StlcScope>>()?;
            if let Some(ident) = parameter_ident(child)? {
                let decl = declaration_scope(uri, child);
                graph.set_node(
                    decl.node(),
                    ScopeNode::Declaration(StlcScopeData::Declaration {
                        name: ident.clone(),
                        definition: child,
                    }),
                )?;
                graph.link(
                    owner.node(),
                    StlcScopeLabel::Declaration(ident),
                    decl.node(),
                )?;
            }
            owner
        }
        _ => child_scope.unwrap_or(*parent_incoming),
    };
    run(
        |(uri, child, incoming): (String, Node<StlcTree>, Scope<StlcScope>)| {
            name_node(uri, child, incoming)
        },
        (uri.to_string(), child, incoming_child),
    )
}

/// Resolves one variable node from its exact enclosing scope and name bucket.
pub fn resolve_node(id: Node<StlcTree>) -> Result<()> {
    let output = emit_view::<StlcResolvedReferences>()?;
    let Some(scope) = observe_view::<StlcEnclosingScopes>()?.get(&id)? else {
        return output.remove(id);
    };
    let Some(StlcCase::Expr(StlcExprCase::Variable { f0: token })) =
        StlcTree::observe_case(id)?
    else {
        return output.remove(id);
    };
    let Some(value) = observe_token::<StlcToken>(token)? else {
        return output.insert(
            id,
            StlcResolution::Unbound {
                name: Arc::from(""),
            },
        );
    };
    let StlcToken::Ident(text) = value.as_ref() else {
        return output.insert(
            id,
            StlcResolution::Unbound {
                name: Arc::from(""),
            },
        );
    };
    let name: Arc<str> = Arc::from(text.as_str());
    let label = StlcScopeLabel::Declaration(Arc::clone(&name));
    let mut current = *scope;
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current) {
            break;
        }
        if let Some(declaration) = outgoing(current, &label)?.first().copied() {
            return output.insert(
                id,
                StlcResolution::Resolved {
                    declaration,
                },
            );
        }
        let Some(parent) = outgoing(current, &StlcScopeLabel::Lexical)?.first().copied() else {
            break;
        };
        current = parent;
    }
    output.insert(id, StlcResolution::Unbound { name })
}

/// The resolution pass: one child per reference candidate (plan §16.2).
pub fn resolve_pass(_: ()) -> Result<()> {
    run_each_key::<StlcReferenceCandidates, _>(resolve_node)
}

fn collect_import(
    uri: &str,
    path_node: &Node<StlcTree>,
) -> Result<()> {
    let Some(StlcCase::Path(StlcPathCase::Segments { f0: segments })) =
        StlcTree::observe_case(*path_node)?
    else {
        return Ok(());
    };
    let mut text = String::new();
    for (index, segment) in segments.iter().enumerate() {
        if index != 0 {
            text.push('.');
        }
        let Some(value) = ident_lexeme_of(*segment)? else {
            return Ok(());
        };
        text.push_str(&value);
    }
    if text.is_empty() {
        return Ok(());
    }
    let graph = emit_view::<ScopeGraph<StlcScope>>()?;
    graph.set_node(
        external_scope(uri, &text).node(),
        ScopeNode::Scope(StlcScopeData::External {
            path: Arc::from(text.as_str()),
        }),
    )
}
