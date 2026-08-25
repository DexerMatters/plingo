//! Plain reactive bidirectional typechecking for the STLC example
//! (plan §6.2): one visitor per syntax node, joined to the SAME scope graph
//! as name resolution. Inferred types flow up as graph facts (`type_scope`
//! nodes); expected types flow down through the `run` recursion input.
//! Diagnostics live in per-node list slots.

use std::sync::Arc;

use plingo::framework::parse::TreeParseUnits;
use plingo::framework::scope::{ScopeNode, outgoing};
use plingo::reactive::prelude::*;
use plingo::reactive::view::Node;
use reactive_macros::view;

use super::name_resolve::{
    Scope, ScopeGraph, StlcResolution, StlcResolvedReferences, StlcScope, StlcScopeData,
    StlcScopeLabel, StlcTypeError, StlcTypeValue, declaration_scope, type_scope,
};
use super::syntax::{
    StlcCase, StlcDeclarationCase, StlcDocument, StlcExprCase, StlcTree, StlcTypeAtomCase,
    StlcTypeCase,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum StlcTypeMode {
    #[default]
    Infer,
    Check(StlcTypeValue),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StlcTypeDiagnostic {
    pub expression: Node<StlcTree>,
    pub error: StlcTypeError,
}

/// Per-node diagnostics: each node's visitor replaces its own slots under
/// its own key, so writers never collide (T5) and a consumer of one node's
/// diagnostics wakes only on that node's slots (plan §6.2 item 5).
#[view]
pub struct StlcTypeDiagnostics(List<Node<StlcTree>, StlcTypeDiagnostic>);

// ---------------------------------------------------------------------------
// Stable identities for check-owned scope-graph facts
// ---------------------------------------------------------------------------

/// The scope-graph payload carrying an inferred type.
fn type_payload(ty: &StlcTypeValue) -> StlcScopeData {
    StlcScopeData::Type(ty.clone())
}

// ---------------------------------------------------------------------------
// The check pass
// ---------------------------------------------------------------------------

pub fn check_pass(_: ()) -> Result<()> {
    run_each_key::<TreeParseUnits<StlcDocument>, _>(check_document)
}

pub fn check_document(uri: String) -> Result<()> {
    let Some(unit) = observe_view::<TreeParseUnits<StlcDocument>>()?.get(&uri)? else {
        return Ok(());
    };
    let Some(root) = unit.root else {
        return Ok(());
    };
    run(
        |(uri, id, mode): (String, Node<StlcTree>, StlcTypeMode)| {
            type_node(uri, id, mode)
        },
        (uri, root, StlcTypeMode::Infer),
    )?;
    Ok(())
}

/// Reads one node's inferred type from its graph fact.
fn observed_type(uri: &str, id: Node<StlcTree>) -> Result<Option<StlcTypeValue>> {
    let observe = ::plingo::reactive::kind::observe_view::<ScopeGraph<StlcScope>>()?;
    Ok(match observe.payload(type_scope(uri, id).node())?.as_deref() {
        Some(ScopeNode::Scope(StlcScopeData::Type(ty))) => Some(ty.clone()),
        _ => None,
    })
}
fn resolved_type(declaration: Scope<StlcScope>) -> Result<Option<StlcTypeValue>> {
    let Some(type_node) = outgoing(declaration, &StlcScopeLabel::Type)?.first().copied() else {
        return Ok(None);
    };
    let observe = observe_view::<ScopeGraph<StlcScope>>()?;
    Ok(match observe.payload(type_node.node())?.as_deref() {
        Some(ScopeNode::Scope(StlcScopeData::Type(ty))) => Some(ty.clone()),
        _ => None,
    })
}

/// One syntax node's bidirectional check. Parents read each child's type
/// from the child's own graph fact — one fact read per child, exactly the
/// dependency the engine needs (plan §6.2 item 4).
fn type_node(
    uri: String,
    id: Node<StlcTree>,
    mode: StlcTypeMode,
) -> Result<Option<StlcTypeValue>> {
    let case = StlcTree::observe_case(id)?;
    let children = StlcTree::observe_children(id)?.to_vec();
    let graph = ::plingo::reactive::kind::emit_view::<ScopeGraph<StlcScope>>()?;

    // Recurse into immediate children so each child publishes its own
    // diagnostics and type fact. The type READS are not pro read: a child's
    // type is observed only when a variant arm needs it (plan §16), so the
    // parent's own type depends on exactly the children its case reads.
    for child in &children {
        run(
            |(uri, child, mode): (String, Node<StlcTree>, StlcTypeMode)| {
                type_node(uri, child, mode)
            },
            (uri.clone(), *child, StlcTypeMode::Infer),
        )?;
    }

    let child_type = |id: &Node<StlcTree>| -> Option<StlcTypeValue> {
        observed_type(&uri, *id).ok().flatten()
    };

    let mut diagnostics: Vec<StlcTypeDiagnostic> = Vec::new();
    let ty = match &case {
        Some(StlcCase::Document(_)) | Some(StlcCase::Path(_)) => None,
        Some(StlcCase::Type(StlcTypeCase::Arrow { f0, f1 })) => match (
            child_type(f0),
            child_type(f1),
        ) {
            (Some(parameter), Some(result)) => {
                Some(StlcTypeValue::Arrow(Box::new(parameter), Box::new(result)))
            }
            _ => None,
        },
        Some(StlcCase::Type(StlcTypeCase::Atom { f0 })) => child_type(f0),
        Some(StlcCase::TypeAtom(StlcTypeAtomCase::Nat { .. })) => Some(StlcTypeValue::Nat),
        Some(StlcCase::TypeAtom(StlcTypeAtomCase::Bool { .. })) => Some(StlcTypeValue::Bool),
        Some(StlcCase::TypeAtom(StlcTypeAtomCase::Unit { .. })) => Some(StlcTypeValue::Unit),
        Some(StlcCase::TypeAtom(StlcTypeAtomCase::Parenthesized { f0 })) => child_type(f0),
        Some(StlcCase::Expr(StlcExprCase::True { .. }))
        | Some(StlcCase::Expr(StlcExprCase::False { .. })) => Some(StlcTypeValue::Bool),
        Some(StlcCase::Expr(StlcExprCase::Number { .. })) => Some(StlcTypeValue::Nat),
        Some(StlcCase::Expr(StlcExprCase::Unit { .. })) => Some(StlcTypeValue::Unit),
        Some(StlcCase::Expr(StlcExprCase::Group { f0 })) => child_type(f0),
        Some(StlcCase::Expr(StlcExprCase::Succ { f0 })) => {
            let found = child_type(f0);
            if found != Some(StlcTypeValue::Nat)
                && let Some(found) = found.clone() {
                    diagnostics.push(StlcTypeDiagnostic {
                        expression: *f0,
                        error: StlcTypeError::Mismatch {
                            expected: StlcTypeValue::Nat,
                            found,
                        },
                    });
                }
            Some(StlcTypeValue::Nat)
        }
        Some(StlcCase::Expr(StlcExprCase::Add { f0, f1 })) => {
            for child in [f0, f1] {
                if let Some(found) = child_type(child)
                    && found != StlcTypeValue::Nat
                {
                    diagnostics.push(StlcTypeDiagnostic {
                        expression: *child,
                        error: StlcTypeError::Mismatch {
                            expected: StlcTypeValue::Nat,
                            found,
                        },
                    });
                }
            }
            Some(StlcTypeValue::Nat)
        }
        Some(StlcCase::Expr(StlcExprCase::If {
            f0: condition,
            f1: then_branch,
            f2: else_branch,
        })) => {
            if let Some(found) = child_type(condition)
                && found != StlcTypeValue::Bool
            {
                diagnostics.push(StlcTypeDiagnostic {
                    expression: *condition,
                    error: StlcTypeError::Mismatch {
                        expected: StlcTypeValue::Bool,
                        found,
                    },
                });
            }
            let then_ty = child_type(then_branch);
            let else_ty = child_type(else_branch);
            if let (Some(then_ty), Some(else_ty)) = (&then_ty, &else_ty)
                && then_ty != else_ty
            {
                diagnostics.push(StlcTypeDiagnostic {
                    expression: id,
                    error: StlcTypeError::BranchMismatch {
                        then_ty: then_ty.clone(),
                        else_ty: else_ty.clone(),
                    },
                });
            }
            then_ty.or(else_ty)
        }
        Some(StlcCase::Expr(StlcExprCase::Lambda {
            f0: parameter,
            f1: body,
        })) => {
            let parameter_ty =
                child_type(parameter).unwrap_or_else(|| parameter_default(parameter));
            let body_ty = child_type(body).unwrap_or(StlcTypeValue::Unit);
            Some(StlcTypeValue::Arrow(
                Box::new(parameter_ty),
                Box::new(body_ty),
            ))
        }
        Some(StlcCase::Expr(StlcExprCase::Apply { f0, f1 })) => {
            let function_ty = child_type(f0);
            let argument_ty = child_type(f1);
            match function_ty {
                Some(StlcTypeValue::Arrow(parameter, result)) => {
                    if let Some(argument_ty) = argument_ty
                        && *parameter != argument_ty
                    {
                        diagnostics.push(StlcTypeDiagnostic {
                            expression: *f1,
                            error: StlcTypeError::Mismatch {
                                expected: *parameter,
                                found: argument_ty,
                            },
                        });
                    }
                    Some(*result)
                }
                Some(found) => {
                    diagnostics.push(StlcTypeDiagnostic {
                        expression: *f0,
                        error: StlcTypeError::NonFunctionApplication { found },
                    });
                    None
                }
                None => None,
            }
        }
        Some(StlcCase::Expr(StlcExprCase::Let {
            f0: _,
            f1: value,
            f2: body,
        })) => {
            let _ = child_type(value);
            child_type(body)
        }
        Some(StlcCase::Expr(StlcExprCase::Case {
            f0: scrutinee,
            f1: zero_branch,
            f2: _,
            f3: successor_branch,
        })) => {
            if let Some(found) = child_type(scrutinee)
                && found != StlcTypeValue::Nat
            {
                diagnostics.push(StlcTypeDiagnostic {
                    expression: *scrutinee,
                    error: StlcTypeError::Mismatch {
                        expected: StlcTypeValue::Nat,
                        found,
                    },
                });
            }
            let zero_ty = child_type(zero_branch);
            let successor_ty = child_type(successor_branch);
            if let (Some(zero_ty), Some(successor_ty)) = (&zero_ty, &successor_ty)
                && zero_ty != successor_ty
            {
                diagnostics.push(StlcTypeDiagnostic {
                    expression: id,
                    error: StlcTypeError::BranchMismatch {
                        then_ty: zero_ty.clone(),
                        else_ty: successor_ty.clone(),
                    },
                });
            }
            zero_ty.or(successor_ty)
        }
        Some(StlcCase::Expr(StlcExprCase::Variable { .. })) => {
            match observe_view::<StlcResolvedReferences>()?.get(&id)?.as_deref() {
                Some(StlcResolution::Resolved { declaration }) => resolved_type(*declaration)?,
                Some(StlcResolution::Unbound { name }) => {
                    diagnostics.push(StlcTypeDiagnostic {
                        expression: id,
                        error: StlcTypeError::UnboundVariable {
                            name: Arc::clone(name),
                        },
                    });
                    None
                }
                None => None,
            }
        }
        Some(StlcCase::Declaration(StlcDeclarationCase::Value {
            f0: _,
            f1: annotation,
            f2: body,
            f3: parameters,
        })) => {
            // Literal bodies are typed from their OWN payload variant
            // (plan §16: observe exactly the variant-required facts), so a
            // terminal-kind change wakes this declaration through the
            // payload fact itself.
            let literal_ty: Option<StlcTypeValue> = (|| {
                match StlcTree::observe_case(*body).ok()? {
                    Some(StlcCase::Expr(StlcExprCase::True { .. }))
                    | Some(StlcCase::Expr(StlcExprCase::False { .. })) => {
                        Some(StlcTypeValue::Bool)
                    }
                    Some(StlcCase::Expr(StlcExprCase::Number { .. })) => {
                        Some(StlcTypeValue::Nat)
                    }
                    Some(StlcCase::Expr(StlcExprCase::Unit { .. })) => {
                        Some(StlcTypeValue::Unit)
                    }
                    _ => None,
                }
            })();
            let body_ty = literal_ty
                .or_else(|| child_type(body))
                .unwrap_or(StlcTypeValue::Unit);
            let mut result = body_ty;
            for parameter in parameters.iter().rev() {
                let parameter_ty = child_type(parameter)
                    .unwrap_or_else(|| parameter_default(parameter));
                result = StlcTypeValue::Arrow(Box::new(parameter_ty), Box::new(result));
            }
            if let Some(annotation) = annotation
                && let Some(expected) = child_type(annotation) {
                    if expected != result {
                        diagnostics.push(StlcTypeDiagnostic {
                            expression: id,
                            error: StlcTypeError::Mismatch {
                                expected: expected.clone(),
                                found: result.clone(),
                            },
                        });
                    }
                    result = expected;
                }
            Some(result)
        }
        Some(StlcCase::Declaration(StlcDeclarationCase::Import { .. }))
        | Some(StlcCase::Declaration(StlcDeclarationCase::Export { .. }))
        | Some(StlcCase::Declaration(StlcDeclarationCase::Error { .. }))
        | Some(StlcCase::Expr(StlcExprCase::Error { .. }))
        | Some(StlcCase::Type(StlcTypeCase::Error { .. }))
        | Some(StlcCase::Param(_))
        | None => None,
    };

    if let (Some(expected), Some(found)) = (mode_expected(&mode), ty.clone())
        && expected != found
    {
        diagnostics.push(StlcTypeDiagnostic {
            expression: id,
            error: StlcTypeError::Mismatch {
                expected: expected.clone(),
                found: found.clone(),
            },
        });
    }

    // Publish this node's type as a graph fact and, for binders, the Type
    // edge from the name pass's declaration node (disjoint fact sets:
    // multi-producer by ownership, plan §6.2 item 2).
    if let Some(ty) = &ty {
        graph.set_node(
            type_scope(&uri, id).node(),
            ScopeNode::Scope(type_payload(ty)),
        )?;
    }
    match &case {
        Some(StlcCase::Declaration(StlcDeclarationCase::Value { .. })) => {
            if ty.is_some() {
                graph.link(
                    declaration_scope(&uri, id).node(),
                    StlcScopeLabel::Type,
                    type_scope(&uri, id).node(),
                )?;
            }
        }
        Some(StlcCase::Param(_)) => {
            if ty.is_some() {
                graph.link(
                    declaration_scope(&uri, id).node(),
                    StlcScopeLabel::Type,
                    type_scope(&uri, id).node(),
                )?;
            }
        }
        Some(StlcCase::Expr(StlcExprCase::Let { .. }))
            if ty.is_some() => {
                graph.link(
                    declaration_scope(&uri, id).node(),
                    StlcScopeLabel::Type,
                    type_scope(&uri, id).node(),
                )?;
            }
        _ => {}
    }

    // This node's own diagnostic slots (replace diff; equal stays cold).
    emit_view::<StlcTypeDiagnostics>()?.replace(&id, diagnostics)?;

    Ok(ty)
}

fn parameter_default(_parameter: &Node<StlcTree>) -> StlcTypeValue {
    // Missing annotations default to `Nat` plus a diagnostic (plan §6.2).
    StlcTypeValue::Nat
}

fn mode_expected(mode: &StlcTypeMode) -> Option<StlcTypeValue> {
    match mode {
        StlcTypeMode::Infer => None,
        StlcTypeMode::Check(expected) => Some(expected.clone()),
    }
}

