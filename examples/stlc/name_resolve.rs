//! Incremental lexical name resolution for the STLC example (plan §6, §22).
//!
//! Scope identities are AUTOMATIC graph-node outputs (plan §22.1): every
//! scope/declaration node is a generated `Output<ScopeGraph>` port of the
//! component that semantically creates it. No identity `Role` enum, no
//! manual anchored scope construction, and no nested effectful `run`
//! recursion remain.
//!
//! Routing is per generated field edge (plan §22.4): one
//! `route_incoming_scope` instance per `ParserTreeEdges` entry reads the
//! exact parent case and publishes the child's incoming scope; the
//! per-node `emit_node_scopes` instance reads that exact incoming element
//! and owns the node's continuation/declaration/reference outputs.

use std::sync::Arc;

use plingo::framework::lex::observe_token;
use plingo::framework::parse::{AstToken, ParserTreeEdges, ParserTreePayloads, TreeParseUnits};
use plingo::framework::scope::outgoing;
pub use plingo::framework::scope::{Scope, ScopeGraph, ScopeNode};
use plingo::reactive::component::{EachKey, Output, Read, Write};
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
    CaseSuccessor,
    External {
        path: Arc<str>,
    },
}


#[derive(Clone, Debug, PartialEq, Eq, Hash, plingo_macros::ScopeDomain)]
#[scope_domain(scope_data = StlcScopeData, label = StlcScopeLabel, request = Arc<str>)]
pub struct StlcScope;

// ---------------------------------------------------------------------------
// Views (plan §22.1): scope values flow through views; consumers never
// reconstruct an identity.
// ---------------------------------------------------------------------------

/// Cross-document source requirements of the name pass: one map entry per
/// document (plan §7.3, replacing the engine-invisible accumulator).
#[view]
pub struct StlcRequirements(Map<String, Vec<Arc<str>>>);

/// The automatic document scope per URI (plan §22.3).
#[view]
pub struct StlcDocumentScopes(Map<String, Scope<StlcScope>>);

/// The automatic root scope per document root (the body scope).
#[view]
pub struct StlcRootScopes(Map<Node<StlcTree>, Scope<StlcScope>>);

/// Each node's incoming scope, derived through the exact tree parent chain.
#[view]
pub struct StlcIncomingScopes(Map<Node<StlcTree>, Scope<StlcScope>>);

/// One binder's continuation scope (lexical continuation).
#[view]
pub struct StlcContinuationScopes(Map<Node<StlcTree>, Scope<StlcScope>>);

/// One binder's declaration graph node.
#[view]
pub struct StlcDeclarationScopes(Map<Node<StlcTree>, Scope<StlcScope>>);

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
fn token_text(token: AstToken<StlcToken>) -> Result<Option<Arc<str>>> {
    let value = observe_token::<StlcToken>(token)?;
    let Some(value) = value else {
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
// The document scope component (plan §22.3)
// ---------------------------------------------------------------------------

/// The document lifecycle component: creates the automatic document scope
/// and the root body scope, publishes `StlcDocumentScopes[uri]`,
/// `StlcRootScopes[root]`, and the root's incoming scope. An open empty
/// document keeps its document scope.
#[reactive_macros::component]
pub fn emit_document_scope(
    key: EachKey<TreeParseUnits<StlcDocument>>,
    document_scopes: Write<StlcDocumentScopes>,
    root_scopes: Write<StlcRootScopes>,
    incoming: Write<StlcIncomingScopes>,
    requirements: Write<StlcRequirements>,
    document_node: Output<ScopeGraph<StlcScope>>,
    body_node: Output<ScopeGraph<StlcScope>>,
) -> Result<()> {
    let uri = key;
    let Some(unit) = observe_view::<TreeParseUnits<StlcDocument>>()?.get(&uri)? else {
        return requirements.remove(uri);
    };
    let graph = emit_view::<ScopeGraph<StlcScope>>()?;
    let document = Scope::from_graph_node(document_node.node());
    graph.set_node(document.node(), ScopeNode::Scope(StlcScopeData::Document))?;
    document_scopes.insert(uri.clone(), document.clone())?;
    let Some(root) = unit.root.clone() else {
        return requirements.insert(uri, Vec::new());
    };
    let body = Scope::from_graph_node(body_node.node());
    graph.set_node(body.node(), ScopeNode::Scope(StlcScopeData::Lexical))?;
    root_scopes.insert(root.clone(), body.clone())?;
    incoming.insert(root, body)?;
    requirements.insert(uri, Vec::new())
}

// ---------------------------------------------------------------------------
// Field-edge routing (plan §22.4)
// ---------------------------------------------------------------------------

/// One routing component per generated field edge `(parent, child)`. It
/// reads the parent's exact case, the parent's incoming scope, and — for
/// the only sibling-sensitive case, the top-level declaration chain — the
/// previous declaration's continuation scope. It writes exactly the
/// child's incoming scope; a missing predecessor continuation writes
/// nothing and the exact absent-key read wakes it when the predecessor
/// publishes.
#[reactive_macros::component]
pub fn route_incoming_scope(
    key: EachKey<ParserTreeEdges<StlcDocument>>,
    incoming: Read<StlcIncomingScopes>,
    continuations: Read<StlcContinuationScopes>,
    enclosing: Write<StlcIncomingScopes>,
) -> Result<()> {
    let (parent, child) = key;
    let parent_case = StlcTree::observe_case(parent.clone())?;
    let Some(parent_incoming) = incoming.get(&parent)? else {
        return Ok(());
    };
    let base = parent_incoming.as_ref().clone();

    // Top-level declarations form a continuation chain. Looking up the
    // predecessor is the only sibling-sensitive operation.
    let mut base_incoming = base.clone();
    if matches!(parent_case, Some(StlcCase::Document(_))) {
        let siblings = StlcTree::observe_children(parent.clone())?;
        if let Some(index) = siblings.iter().position(|candidate| *candidate == child)
            && index > 0
        {
            let Some(predecessor) = continuations.get(&siblings[index - 1])? else {
                return Ok(());
            };
            base_incoming = predecessor.as_ref().clone();
        }
    }

    let incoming_child = match &parent_case {
        Some(StlcCase::Document(_)) => base_incoming.clone(),
        Some(StlcCase::Expr(StlcExprCase::Let {
            f1: value,
            f2: body,
            ..
        })) if *value == child => base.clone(),
        Some(StlcCase::Expr(StlcExprCase::Let { f2: body, .. })) if *body == child => {
            let Some(scope) = continuations.get(&parent)? else {
                return Ok(());
            };
            scope.as_ref().clone()
        }
        Some(StlcCase::Expr(StlcExprCase::Lambda {
            f0: parameter,
            f1: body,
        })) if *parameter == child => {
            let Some(scope) = continuations.get(&parent)? else {
                return Ok(());
            };
            scope.as_ref().clone()
        }
        Some(StlcCase::Expr(StlcExprCase::Lambda {
            f0: parameter,
            f1: body,
        })) if *body == child => {
            let Some(scope) = continuations.get(parameter)? else {
                return Ok(());
            };
            scope.as_ref().clone()
        }
        Some(StlcCase::Declaration(StlcDeclarationCase::Value {
            f1: annotation,
            f2: body,
            f3: parameters,
            ..
        })) => {
            if annotation.as_ref().is_some_and(|annotation| *annotation == child) {
                base.clone()
            } else if *body == child {
                match parameters.last().cloned() {
                    Some(parameter) => {
                        let Some(scope) = continuations.get(&parameter)? else {
                            return Ok(());
                        };
                        scope.as_ref().clone()
                    }
                    None => base.clone(),
                }
            } else if let Some(index) =
                parameters.iter().position(|parameter| *parameter == child)
            {
                if index == 0 {
                    base.clone()
                } else {
                    let Some(scope) = continuations.get(&parameters[index - 1])? else {
                        return Ok(());
                    };
                    scope.as_ref().clone()
                }
            } else {
                base.clone()
            }
        }
        Some(StlcCase::Expr(StlcExprCase::Case {
            f0: scrutinee,
            f1: zero_branch,
            f3: successor_branch,
            ..
        })) if *scrutinee == child || *zero_branch == child => base.clone(),
        Some(StlcCase::Expr(StlcExprCase::Case {
            f3: successor_branch,
            ..
        })) if *successor_branch == child => {
            let Some(scope) = continuations.get(&parent)? else {
                return Ok(());
            };
            scope.as_ref().clone()
        }
        _ => base.clone(),
    };

    enclosing.insert(child, incoming_child)
}

// ---------------------------------------------------------------------------
// The per-node scope component (plan §22.5)
// ---------------------------------------------------------------------------

/// One component per syntax node, driven by the exact parser payload. It
/// reads its own incoming scope (published by the field-edge routing) and
/// emits its binder continuation/declaration scopes through automatic
/// output ports. A missing incoming scope writes nothing; the exact
/// absent-key read wakes this instance when the edge routes it.
#[reactive_macros::component]
pub fn emit_node_scopes(
    key: EachKey<ParserTreePayloads<StlcDocument>>,
    incoming: Read<StlcIncomingScopes>,
    continuation_writes: Write<StlcContinuationScopes>,
    declarations: Write<StlcDeclarationScopes>,
    candidates: Write<StlcReferenceCandidates>,
    continuation_node: Output<ScopeGraph<StlcScope>>,
    declaration_node: Output<ScopeGraph<StlcScope>>,
    successor_node: Output<ScopeGraph<StlcScope>>,
    external_node: Output<ScopeGraph<StlcScope>>,
) -> Result<()> {
    let id = key;
    let case = StlcTree::observe_case(id.clone())?;
    let Some(incoming_scope) = incoming.get(&id)? else {
        return Ok(());
    };
    let incoming_scope = incoming_scope.as_ref().clone();

    if matches!(case, Some(StlcCase::Expr(StlcExprCase::Variable { .. }))) {
        candidates.insert(id.clone(), ())?;
    }
    let graph = emit_view::<ScopeGraph<StlcScope>>()?;
    // The node's own contributions for binder shapes whose scope anchors
    // at this node.
    match &case {
        Some(StlcCase::Expr(StlcExprCase::Let { f0: name, .. })) => {
            let lex = Scope::from_graph_node(continuation_node.node());
            graph.set_node(lex.node(), ScopeNode::Scope(StlcScopeData::Lexical))?;
            graph.link(lex.node(), StlcScopeLabel::Lexical, incoming_scope.node())?;
            continuation_writes.insert(id.clone(), lex.clone())?;
            if let Some(ident) = ident_lexeme_of(*name)? {
                let decl = Scope::from_graph_node(declaration_node.node());
                graph.set_node(
                    decl.node(),
                    ScopeNode::Declaration(StlcScopeData::Declaration {
                        name: ident.clone(),
                        definition: id.clone(),
                    }),
                )?;
                graph.link(lex.node(), StlcScopeLabel::Declaration(ident), decl.node())?;
                declarations.insert(id.clone(), decl)?;
            }
        }
        Some(StlcCase::Expr(StlcExprCase::Lambda { .. })) => {
            let lex = Scope::from_graph_node(continuation_node.node());
            graph.set_node(lex.node(), ScopeNode::Scope(StlcScopeData::Lexical))?;
            graph.link(lex.node(), StlcScopeLabel::Lexical, incoming_scope.node())?;
            continuation_writes.insert(id.clone(), lex)?;
        }
        Some(StlcCase::Declaration(StlcDeclarationCase::Value {
            f0: name,
            ..
        })) => {
            // The declaration continuation is visible to the next sibling.
            // Its body is routed separately through the ordered parameter
            // continuations below, so the top-level binder is non-recursive.
            let lex = Scope::from_graph_node(continuation_node.node());
            graph.set_node(lex.node(), ScopeNode::Scope(StlcScopeData::Lexical))?;
            graph.link(lex.node(), StlcScopeLabel::Lexical, incoming_scope.node())?;
            continuation_writes.insert(id.clone(), lex.clone())?;
            if let Some(ident) = ident_lexeme_of(*name)? {
                let decl = Scope::from_graph_node(declaration_node.node());
                graph.set_node(
                    decl.node(),
                    ScopeNode::Declaration(StlcScopeData::Declaration {
                        name: ident.clone(),
                        definition: id.clone(),
                    }),
                )?;
                graph.link(lex.node(), StlcScopeLabel::Declaration(ident), decl.node())?;
                declarations.insert(id.clone(), decl)?;
            }
        }
        Some(StlcCase::Param(
            StlcParamCase::Bare { f0: name, .. }
            | StlcParamCase::Parenthesized { f0: name, .. },
        )) => {
            // Each parameter is a continuation, not a shared parameter
            // bucket. This preserves left-to-right visibility and shadowing.
            let lex = Scope::from_graph_node(continuation_node.node());
            graph.set_node(lex.node(), ScopeNode::Scope(StlcScopeData::Lexical))?;
            graph.link(lex.node(), StlcScopeLabel::Lexical, incoming_scope.node())?;
            continuation_writes.insert(id.clone(), lex.clone())?;
            if let Some(ident) = ident_lexeme_of(*name)? {
                let decl = Scope::from_graph_node(declaration_node.node());
                graph.set_node(
                    decl.node(),
                    ScopeNode::Declaration(StlcScopeData::Declaration {
                        name: ident.clone(),
                        definition: id.clone(),
                    }),
                )?;
                graph.link(lex.node(), StlcScopeLabel::Declaration(ident), decl.node())?;
                declarations.insert(id.clone(), decl)?;
            }
        }
        Some(StlcCase::Expr(StlcExprCase::Case { f2: successor, .. })) => {
            let successor_scope = Scope::from_graph_node(successor_node.node());
            graph.set_node(
                successor_scope.node(),
                ScopeNode::Scope(StlcScopeData::CaseSuccessor),
            )?;
            graph.link(
                successor_scope.node(),
                StlcScopeLabel::Lexical,
                incoming_scope.node(),
            )?;
            continuation_writes.insert(id.clone(), successor_scope.clone())?;
            if let Some(ident) = ident_lexeme_of(*successor)? {
                let decl = Scope::from_graph_node(declaration_node.node());
                graph.set_node(
                    decl.node(),
                    ScopeNode::Declaration(StlcScopeData::Declaration {
                        name: ident.clone(),
                        definition: id.clone(),
                    }),
                )?;
                graph.link(
                    successor_scope.node(),
                    StlcScopeLabel::Declaration(ident),
                    decl.node(),
                )?;
                declarations.insert(id.clone(), decl)?;
            }
        }
        _ => {}
    }

    // Import declarations collect their external path.
    if let Some(StlcCase::Declaration(StlcDeclarationCase::Import { f0: path_node })) = &case {
        collect_import(path_node, external_node.node())?;
    }
    Ok(())
}

/// Back-compat installer used by tests that install via components: the
/// document lifecycle, the field-edge routing, and the per-node scope pass.
pub fn name_pass_install(engine: &mut plingo::reactive::Engine) -> Result<()> {
    emit_document_scope_install(engine)?;
    route_incoming_scope_install(engine)?;
    emit_node_scopes_install(engine)?;
    Ok(())
}

/// Resolves one variable node from its exact incoming scope and name bucket.
pub fn resolve_node(id: Node<StlcTree>) -> Result<()> {
    let output = emit_view::<StlcResolvedReferences>()?;
    let Some(scope) = observe_view::<StlcIncomingScopes>()?.get(&id)? else {
        return output.remove(id);
    };
    let Some(StlcCase::Expr(StlcExprCase::Variable { f0: token })) =
        StlcTree::observe_case(id.clone())?
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
    let mut current = scope.as_ref().clone();
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current.clone()) {
            break;
        }
        if let Some(declaration) = outgoing(current.clone(), &label)?.first().cloned() {
            return output.insert(id.clone(), StlcResolution::Resolved { declaration });
        }
        let Some(parent) = outgoing(current.clone(), &StlcScopeLabel::Lexical)?
            .first()
            .cloned()
        else {
            break;
        };
        current = parent;
    }
    output.insert(id, StlcResolution::Unbound { name })
}

/// The resolution pass: one child per reference candidate (plan §16.2).
#[reactive_macros::component]
pub fn resolve_pass(key: EachKey<StlcReferenceCandidates>) -> Result<()> {
    resolve_node(key)
}

fn collect_import(path_node: &Node<StlcTree>, external: Node<ScopeGraph<StlcScope>>) -> Result<()> {
    let Some(StlcCase::Path(StlcPathCase::Segments { f0: segments })) =
        StlcTree::observe_case(path_node.clone())?
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
    let path: Arc<str> = Arc::from(text.as_str());
    graph.set_node(external, ScopeNode::Scope(StlcScopeData::External { path }))
}
