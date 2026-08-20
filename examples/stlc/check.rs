//! Incremental bidirectional typechecking for the STLC example (reactive
//! rewrite, plan Phase 6).
//!
//! The `check` pass types every declaration and expression with per-node
//! child-visitor granularity, publishing per-node [`TypeFact`]s and
//! per-document [`StlcTypeDiagnostic`]s. Its type contributions ride in
//! [`StlcTypeFacts`]/[`StlcTypeScopes`] (owned by this pass), so the
//! shared [`ScopeGraph<StlcScope>`] is written only by `name_pass` —
//! deterministic single-writer, no cross-producer retirement churn.

use std::sync::{Arc, Mutex};

use plingo::framework::lex::Tokens;
use plingo::framework::parse::{AstToken, ParseUnits};
use plingo::framework::scope::{ScopeGraph, ScopeGraphObservedExt, ScopeId};
use plingo::reactive::prelude::*;
use plingo::reactive::view::NodeId;
use plingo::reactive_component as component;
use plingo::reactive_view as view;
use plingo::reactive::api::TreeObservedExt;

use super::name_resolve::{
    StlcScope, StlcTypeError, StlcTypeValue, case_successor_scope, declaration_scope,
    lexical_scope, token_text, type_scope,
};
use super::syntax::{
    StlcCase, StlcDeclarationCase, StlcDocument, StlcExprCase, StlcObservedExt, StlcParamCase,
    StlcToken, StlcTypeCase, StlcTypeAtomCase, StlcTree,
};

/// Input mode is part of a type judgment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StlcTypeMode {
    #[default]
    Infer,
    Check(StlcTypeValue),
}

impl StlcTypeMode {
    fn expected(&self) -> Option<StlcTypeValue> {
        match self {
            Self::Infer => None,
            Self::Check(ty) => Some(ty.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StlcTypeDiagnostic {
    pub expression: NodeId,
    pub error: StlcTypeError,
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// Per-node typing judgments.
#[view(map, key = String, value = Vec<TypeFact>)]
pub struct StlcTypeFacts;

/// One per-node type fact.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeFact {
    pub node: NodeId,
    pub ty: StlcTypeValue,
}

/// Per-document type diagnostics.
#[view(map, key = String, value = Vec<StlcTypeDiagnostic>)]
pub struct StlcTypeDiagnostics;

/// The type scope for one definition (parity with the legacy
/// `StlcScopeKey::Type` allocations).
#[view(map, key = String, value = Vec<TypeScopeFact>)]
pub struct StlcTypeScopes;

/// One definition's type scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeScopeFact {
    pub definition: NodeId,
    pub ty: StlcTypeValue,
}

// ---------------------------------------------------------------------------
// The check pass
// ---------------------------------------------------------------------------

/// The check pass: one child visitor per document over
/// [`ParseUnits<StlcDocument>`], then per-node child visitors inside a
/// document (per-declaration isolation, matrix 4).
#[component]
pub fn check_pass(
    units: ParseUnits<StlcDocument>,
    syntax: StlcTree,
    scopes: ScopeGraph<StlcScope>,
    tokens: Tokens<StlcToken>,
) -> (
    StlcTypeFacts,
    StlcTypeDiagnostics,
    StlcTypeScopes,
) {
    let facts = Emitted::<StlcTypeFacts>::new()?;
    let diagnostics = Emitted::<StlcTypeDiagnostics>::new()?;
    let type_scopes = Emitted::<StlcTypeScopes>::new()?;
    let facts_handle = facts.clone();
    let diagnostics_handle = diagnostics.clone();
    let type_scopes_handle = type_scopes.clone();
    units.visit_each(move |uri, unit| -> Result<()> {
        let Some(unit) = unit else {
            return Ok(());
        };
        let docs: Arc<Mutex<Vec<StlcTypeDiagnostic>>> = Arc::new(Mutex::new(Vec::new()));
        let facts_buf: Arc<Mutex<Vec<TypeFact>>> = Arc::new(Mutex::new(Vec::new()));
        let scopes_buf: Arc<Mutex<Vec<TypeScopeFact>>> = Arc::new(Mutex::new(Vec::new()));
        let incoming = lexical_scope(&uri, unit.root);
        // Type each top-level declaration (the document's children).
        let doc_children = TreeObservedExt::children(&syntax, unit.root)?;
        for declaration in doc_children {
            let _ = type_node(
                &uri,
                &syntax,
                &scopes,
                &tokens,
                &facts_buf,
                &docs,
                &scopes_buf,
                declaration,
                incoming,
                StlcTypeMode::Infer,
            )?;
        }
        let mut local_facts = facts_buf.lock().expect("facts lock");
        let collected_facts = std::mem::take(&mut *local_facts);
        let mut local_scopes = scopes_buf.lock().expect("scopes lock");
        let collected_scopes = std::mem::take(&mut *local_scopes);
        let mut diags = docs.lock().expect("docs lock");
        let diags = std::mem::take(&mut *diags);
        diagnostics_handle.set(uri.clone(), diags)?;
        facts_handle.set(uri.clone(), collected_facts)?;
        type_scopes_handle.set(uri, collected_scopes)?;
        Ok(())
    })?;
    Ok((facts, diagnostics, type_scopes))
}

// ---------------------------------------------------------------------------
// The dispatch
// ---------------------------------------------------------------------------

/// Types one node under `incoming` with `mode`. Returns the node's type.
#[allow(clippy::too_many_arguments)]
fn type_node(
    uri: &str,
    syntax: &ObservedHandle<StlcTree>,
    scopes: &ObservedHandle<ScopeGraph<StlcScope>>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    facts: &Arc<Mutex<Vec<TypeFact>>>,
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    type_scopes: &Arc<Mutex<Vec<TypeScopeFact>>>,
    id: NodeId,
    incoming: ScopeId<StlcScope>,
    mode: StlcTypeMode,
) -> Result<Option<StlcTypeValue>> {
    let result = match syntax.case(id)? {
        None => None,
        Some(StlcCase::Document(_)) => None,
        Some(StlcCase::Declaration(declaration)) => match declaration {
            StlcDeclarationCase::Value {
                f0: _name,
                f1: annotation,
                f2: body,
                f3: parameters,
            } => type_declaration(
                uri, syntax, scopes, tokens, facts, diagnostics, type_scopes, id, incoming,
                annotation, body, parameters,
            )?,
            StlcDeclarationCase::Import { .. }
            | StlcDeclarationCase::Export { .. }
            | StlcDeclarationCase::Error { .. } => None,
        },
        Some(StlcCase::Expr(expression)) => match expression {
            StlcExprCase::If {
                f0: condition,
                f1: then_branch,
                f2: else_branch,
                ..
            } => {
                let condition = check_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    condition, incoming, StlcTypeValue::Bool,
                )?;
                let then_ty = infer_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    then_branch, incoming,
                )?;
                let else_ty = infer_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    else_branch, incoming,
                )?;
                agree_branches(diagnostics, id, condition, then_ty, else_ty)
            }
            StlcExprCase::Case {
                f0: scrutinee,
                f1: zero_branch,
                f2: _successor,
                f3: successor_branch,
                ..
            } => type_case(
                uri, syntax, scopes, tokens, facts, diagnostics, type_scopes, id, incoming,
                scrutinee, zero_branch, successor_branch,
            )?,
            StlcExprCase::Let {
                f0: _name,
                f1: value,
                f2: body,
                ..
            } => {
                let value_ty = infer_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    value, incoming,
                )?;
                let Some(value_ty) = value_ty else {
                    return Ok(None);
                };
                emit_binding_type(type_scopes, id, value_ty.clone());
                let body_scope = lexical_scope(uri, id);
                infer_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    body, body_scope,
                )?
            }
            StlcExprCase::Lambda { f0: parameter, f1: body, .. } => {
                let parameter_ty = parameter_annotation(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    parameter, incoming,
                )?;
                let Some(parameter_ty) = parameter_ty else {
                    return Ok(None);
                };
                let lambda_scope = lexical_scope(uri, id);
                emit_binding_type(type_scopes, parameter, parameter_ty.clone());
                let Some(body_ty) = infer_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    body, lambda_scope,
                )?
                else {
                    return Ok(None);
                };
                Some(StlcTypeValue::Arrow(Box::new(parameter_ty), Box::new(body_ty)))
            }
            StlcExprCase::Add { f0: left, f1: right, .. } => {
                let left = check_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    left, incoming, StlcTypeValue::Nat,
                )?;
                let right = check_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    right, incoming, StlcTypeValue::Nat,
                )?;
                (left.is_some() && right.is_some()).then_some(StlcTypeValue::Nat)
            }
            StlcExprCase::Apply { f0: fun, f1: arg, .. } => {
                let function_ty = infer_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    fun, incoming,
                )?;
                let Some(function_ty) = function_ty else {
                    return Ok(None);
                };
                let StlcTypeValue::Arrow(domain, codomain) = function_ty else {
                    emit_diagnostic(
                        diagnostics,
                        id,
                        StlcTypeError::NonFunctionApplication { found: function_ty },
                    );
                    return Ok(None);
                };
                check_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    arg, incoming, *domain,
                )?
                .map(|_| *codomain)
            }
            StlcExprCase::Succ { f0: inner, .. } => check_child(
                uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                inner, incoming, StlcTypeValue::Nat,
            )?
            .map(|_| StlcTypeValue::Nat),
            StlcExprCase::Group { f0: inner, .. } => infer_child(
                uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                inner, incoming,
            )?,
            StlcExprCase::True { .. } | StlcExprCase::False { .. } => {
                Some(StlcTypeValue::Bool)
            }
            StlcExprCase::Number { .. } => Some(StlcTypeValue::Nat),
            StlcExprCase::Unit { .. } => Some(StlcTypeValue::Unit),
            StlcExprCase::Variable { f0: name, .. } => infer_variable(
                syntax, scopes, tokens, diagnostics, id, incoming, name,
            )?,
            StlcExprCase::Error { .. } => None,
        },
        Some(StlcCase::Type(ty)) => match ty {
            StlcTypeCase::Arrow { f0: domain, f1: codomain, .. } => {
                let domain = infer_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    domain, incoming,
                )?;
                let codomain = infer_child(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    codomain, incoming,
                )?;
                match (domain, codomain) {
                    (Some(domain), Some(codomain)) => Some(StlcTypeValue::Arrow(
                        Box::new(domain),
                        Box::new(codomain),
                    )),
                    _ => None,
                }
            }
            StlcTypeCase::Atom { f0: atom, .. } => infer_child(
                uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                atom, incoming,
            )?,
            StlcTypeCase::Error { .. } => None,
        },
        Some(StlcCase::TypeAtom(atom)) => match atom {
            StlcTypeAtomCase::Nat { .. } => Some(StlcTypeValue::Nat),
            StlcTypeAtomCase::Bool { .. } => Some(StlcTypeValue::Bool),
            StlcTypeAtomCase::Unit { .. } => Some(StlcTypeValue::Unit),
            StlcTypeAtomCase::Parenthesized { f0: inner, .. } => infer_child(
                uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                inner, incoming,
            )?,
        },
        Some(StlcCase::Path(_)) | Some(StlcCase::Param(_)) => None,
    };
    // Check-mode: non-structural expressions are checked by
    // inference-then-agreement (legacy `check_inferred`).
    match mode.expected() {
        Some(expected) => Ok(check_inferred(diagnostics, id, expected, result)),
        None => Ok(result),
    }
}

// ---------------------------------------------------------------------------
// Declarations, cases, helpers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn type_declaration(
    uri: &str,
    syntax: &ObservedHandle<StlcTree>,
    scopes: &ObservedHandle<ScopeGraph<StlcScope>>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    facts: &Arc<Mutex<Vec<TypeFact>>>,
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    type_scopes: &Arc<Mutex<Vec<TypeScopeFact>>>,
    id: NodeId,
    incoming: ScopeId<StlcScope>,
    annotation: Option<NodeId>,
    body: NodeId,
    parameters: Vec<NodeId>,
) -> Result<Option<StlcTypeValue>> {
    let declared = match annotation {
        Some(node) => match infer_child(
            uri, syntax, scopes, tokens, facts, diagnostics, type_scopes, node, incoming,
        )? {
            Some(ty) => Some(ty),
            None => {
                emit_diagnostic(diagnostics, id, StlcTypeError::InvalidAnnotation);
                return Ok(None);
            }
        },
        None => None,
    };
    let (parameter_types, body_expected) = match declared.as_ref() {
        Some(signature) => {
            let Some((types, body_ty)) = split_function_type(signature, parameters.len()) else {
                emit_diagnostic(diagnostics, id, StlcTypeError::InvalidAnnotation);
                return Ok(None);
            };
            for (parameter, expected) in parameters.iter().zip(&types) {
                if !parameter_matches(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    *parameter, incoming, expected,
                )? {
                    return Ok(None);
                }
            }
            (types, Some(body_ty))
        }
        None => {
            let mut types = Vec::with_capacity(parameters.len());
            for parameter in parameters.iter() {
                let Some(ty) = parameter_annotation(
                    uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
                    *parameter, incoming,
                )?
                else {
                    emit_diagnostic(
                        diagnostics,
                        *parameter,
                        StlcTypeError::MissingParameterAnnotation,
                    );
                    return Ok(None);
                };
                types.push(ty);
            }
            (types, None)
        }
    };

    if let Some(signature) = &declared {
        emit_binding_type(type_scopes, id, signature.clone());
    }
    for (parameter, ty) in parameters.iter().zip(&parameter_types) {
        emit_binding_type(type_scopes, *parameter, ty.clone());
    }
    let body_scope = lexical_scope(uri, id);
    let body_ty = match body_expected {
        Some(expected) => match check_child(
            uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
            body, body_scope, expected,
        )? {
            Some(ty) => ty,
            None => return Ok(None),
        },
        None => match infer_child(
            uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
            body, body_scope,
        )? {
            Some(ty) => ty,
            None => return Ok(None),
        },
    };
    let binding_ty = declared.unwrap_or_else(|| curry_type(&parameter_types, body_ty));
    if annotation.is_none() {
        emit_binding_type(type_scopes, id, binding_ty.clone());
    }
    Ok(Some(binding_ty))
}

#[allow(clippy::too_many_arguments)]
fn type_case(
    uri: &str,
    syntax: &ObservedHandle<StlcTree>,
    scopes: &ObservedHandle<ScopeGraph<StlcScope>>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    facts: &Arc<Mutex<Vec<TypeFact>>>,
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    type_scopes: &Arc<Mutex<Vec<TypeScopeFact>>>,
    id: NodeId,
    incoming: ScopeId<StlcScope>,
    scrutinee: NodeId,
    zero_branch: NodeId,
    successor_branch: NodeId,
) -> Result<Option<StlcTypeValue>> {
    let scrutinized = check_child(
        uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
        scrutinee, incoming, StlcTypeValue::Nat,
    )?;
    let zero_ty = infer_child(
        uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
        zero_branch, incoming,
    )?;
    let successor_scope = case_successor_scope(uri, id);
    emit_binding_type(type_scopes, id, StlcTypeValue::Nat);
    let successor_ty = infer_child(
        uri, syntax, scopes, tokens, facts, diagnostics, type_scopes,
        successor_branch, successor_scope,
    )?;
    Ok(agree_branches(diagnostics, id, scrutinized, zero_ty, successor_ty))
}

/// Infers one child's type as its own visitor instance.
#[allow(clippy::too_many_arguments)]
fn infer_child(
    uri: &str,
    syntax: &ObservedHandle<StlcTree>,
    scopes: &ObservedHandle<ScopeGraph<StlcScope>>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    facts: &Arc<Mutex<Vec<TypeFact>>>,
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    type_scopes: &Arc<Mutex<Vec<TypeScopeFact>>>,
    child: NodeId,
    incoming: ScopeId<StlcScope>,
) -> Result<Option<StlcTypeValue>> {
    let uri = uri.to_string();
    let recursion = syntax.clone();
    let scopes = scopes.clone();
    let tokens = tokens.clone();
    let facts_for_tail = Arc::clone(facts);
    let diagnostics_for_tail = Arc::clone(diagnostics);
    let type_scopes_for_tail = Arc::clone(type_scopes);
    let facts = Arc::clone(facts);
    let diagnostics = Arc::clone(diagnostics);
    let type_scopes = Arc::clone(type_scopes);
    let result: Arc<Mutex<Option<StlcTypeValue>>> = Arc::new(Mutex::new(None));
    let result_handle = Arc::clone(&result);
    let closure_handle = recursion.clone();
    TreeObservedExt::visit_node(&recursion, child, move |_id, _payload| -> Result<(), Error> {
        let value = type_node(
            &uri,
            &closure_handle,
            &scopes,
            &tokens,
            &facts,
            &diagnostics,
            &type_scopes,
            child,
            incoming,
            StlcTypeMode::Infer,
        )?;
        *result_handle.lock().expect("result lock") = value;
        Ok(())
    })?;
    let mut guard = result.lock().expect("result lock");
    let value = guard.take();
    if let Some(ty) = &value {
        facts_for_tail
            .lock()
            .expect("facts lock")
            .push(TypeFact { node: child, ty: ty.clone() });
    }
    let _ = (diagnostics_for_tail, type_scopes_for_tail);
    Ok(value)
}

/// Checks one child against `expected` as its own visitor instance.
#[allow(clippy::too_many_arguments)]
fn check_child(
    uri: &str,
    syntax: &ObservedHandle<StlcTree>,
    scopes: &ObservedHandle<ScopeGraph<StlcScope>>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    facts: &Arc<Mutex<Vec<TypeFact>>>,
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    type_scopes: &Arc<Mutex<Vec<TypeScopeFact>>>,
    child: NodeId,
    incoming: ScopeId<StlcScope>,
    expected: StlcTypeValue,
) -> Result<Option<StlcTypeValue>> {
    let uri = uri.to_string();
    let recursion = syntax.clone();
    let scopes = scopes.clone();
    let tokens = tokens.clone();
    let facts_for_tail = Arc::clone(facts);
    let diagnostics_for_tail = Arc::clone(diagnostics);
    let type_scopes_for_tail = Arc::clone(type_scopes);
    let facts = Arc::clone(facts);
    let diagnostics = Arc::clone(diagnostics);
    let type_scopes = Arc::clone(type_scopes);
    let result: Arc<Mutex<Option<StlcTypeValue>>> = Arc::new(Mutex::new(None));
    let result_handle = Arc::clone(&result);
    let closure_handle = recursion.clone();
    TreeObservedExt::visit_node(&recursion, child, move |_id, _payload| -> Result<(), Error> {
        let value = type_node(
            &uri,
            &closure_handle,
            &scopes,
            &tokens,
            &facts,
            &diagnostics,
            &type_scopes,
            child,
            incoming,
            StlcTypeMode::Check(expected.clone()),
        )?;
        *result_handle.lock().expect("result lock") = value;
        Ok(())
    })?;
    let mut guard = result.lock().expect("result lock");
    let value = guard.take();
    if let Some(ty) = &value {
        facts_for_tail
            .lock()
            .expect("facts lock")
            .push(TypeFact { node: child, ty: ty.clone() });
    }
    let _ = (diagnostics_for_tail, type_scopes_for_tail);
    Ok(value)
}

fn emit_diagnostic(
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    expression: NodeId,
    error: StlcTypeError,
) {
    diagnostics
        .lock()
        .expect("docs lock")
        .push(StlcTypeDiagnostic { expression, error });
}

fn emit_binding_type(
    type_scopes: &Arc<Mutex<Vec<TypeScopeFact>>>,
    definition: NodeId,
    ty: StlcTypeValue,
) {
    type_scopes
        .lock()
        .expect("type scopes lock")
        .push(TypeScopeFact { definition, ty });
}

fn infer_variable(
    _syntax: &ObservedHandle<StlcTree>,
    scopes: &ObservedHandle<ScopeGraph<StlcScope>>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    current: NodeId,
    incoming: ScopeId<StlcScope>,
    name: AstToken<StlcToken>,
) -> Result<Option<StlcTypeValue>> {
    let Some(name) = token_text(tokens, "", name)? else {
        return Ok(None);
    };
    // Resolve through the scope graph: Lexical* followed by Declaration,
    // reading exactly the touched buckets.
    let path = plingo::framework::scope::ScopePath::from(
        plingo::framework::scope::PathExpr::label(super::name_resolve::StlcScopeLabel::Lexical)
            .star()
            .then(plingo::framework::scope::PathExpr::label(
                super::name_resolve::StlcScopeLabel::Declaration,
            )),
    );
    let resolved = scopes.resolve(incoming, path, |payload| match payload {
        plingo::framework::scope::ScopeNode::Scope(
            super::name_resolve::StlcScopeData::Declaration { name: binding, .. },
        ) => **binding == *name,
        _ => false,
    })?;
    let mut best: Option<StlcTypeValue> = None;
    for path in resolved {
        let nodes = path.scopes;
        let last = nodes[nodes.len() - 1];
        if let Some(ty) = binding_type_at(scopes, "", last.node())? {
            if best.is_none() {
                best = Some(ty);
            }
        }
    }
    match best {
        Some(ty) => Ok(Some(ty)),
        None => {
            emit_diagnostic(diagnostics, current, StlcTypeError::UnboundVariable { name });
            Ok(None)
        }
    }
}

fn check_inferred(
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    current: NodeId,
    expected: StlcTypeValue,
    found: Option<StlcTypeValue>,
) -> Option<StlcTypeValue> {
    let Some(found) = found else {
        emit_diagnostic(
            diagnostics,
            current,
            StlcTypeError::Mismatch {
                expected: expected.clone(),
                found: StlcTypeValue::Unit,
            },
        );
        return None;
    };
    if found == expected {
        Some(expected)
    } else {
        emit_diagnostic(diagnostics, current, StlcTypeError::Mismatch { expected, found });
        None
    }
}

fn agree_branches(
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    current: NodeId,
    prerequisite: Option<StlcTypeValue>,
    left: Option<StlcTypeValue>,
    right: Option<StlcTypeValue>,
) -> Option<StlcTypeValue> {
    if prerequisite.is_none() {
        return None;
    }
    match (left, right) {
        (Some(left), Some(right)) if left == right => Some(left),
        (Some(then_ty), Some(else_ty)) => {
            emit_diagnostic(
                diagnostics,
                current,
                StlcTypeError::BranchMismatch { then_ty, else_ty },
            );
            None
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn parameter_annotation(
    uri: &str,
    syntax: &ObservedHandle<StlcTree>,
    scopes: &ObservedHandle<ScopeGraph<StlcScope>>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    facts: &Arc<Mutex<Vec<TypeFact>>>,
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    type_scopes: &Arc<Mutex<Vec<TypeScopeFact>>>,
    parameter: NodeId,
    incoming: ScopeId<StlcScope>,
) -> Result<Option<StlcTypeValue>> {
    let annotation = match syntax.case(parameter)? {
        Some(StlcCase::Param(StlcParamCase::Bare { f1: annotation, .. }))
        | Some(StlcCase::Param(StlcParamCase::Parenthesized { f1: annotation, .. })) => annotation,
        _ => None,
    };
    match annotation {
        Some(node) => infer_child(
            uri, syntax, scopes, tokens, facts, diagnostics, type_scopes, node, incoming,
        ),
        None => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
fn parameter_matches(
    uri: &str,
    syntax: &ObservedHandle<StlcTree>,
    scopes: &ObservedHandle<ScopeGraph<StlcScope>>,
    tokens: &ObservedHandle<Tokens<StlcToken>>,
    facts: &Arc<Mutex<Vec<TypeFact>>>,
    diagnostics: &Arc<Mutex<Vec<StlcTypeDiagnostic>>>,
    type_scopes: &Arc<Mutex<Vec<TypeScopeFact>>>,
    parameter: NodeId,
    incoming: ScopeId<StlcScope>,
    expected: &StlcTypeValue,
) -> Result<bool> {
    let Some(found) = parameter_annotation(
        uri, syntax, scopes, tokens, facts, diagnostics, type_scopes, parameter, incoming,
    )?
    else {
        return Ok(true);
    };
    if found == *expected {
        return Ok(true);
    }
    emit_diagnostic(
        diagnostics,
        parameter,
        StlcTypeError::Mismatch {
            expected: expected.clone(),
            found,
        },
    );
    Ok(false)
}

fn split_function_type(
    signature: &StlcTypeValue,
    parameters: usize,
) -> Option<(Vec<StlcTypeValue>, StlcTypeValue)> {
    let mut remaining = signature;
    let mut parameter_types = Vec::with_capacity(parameters);
    for _ in 0..parameters {
        let StlcTypeValue::Arrow(domain, codomain) = remaining else {
            return None;
        };
        parameter_types.push((**domain).clone());
        remaining = codomain;
    }
    Some((parameter_types, remaining.clone()))
}

fn curry_type(parameters: &[StlcTypeValue], result: StlcTypeValue) -> StlcTypeValue {
    parameters.iter().rev().fold(result, |result, parameter| {
        StlcTypeValue::Arrow(Box::new(parameter.clone()), Box::new(result))
    })
}

/// Reads the committed type scope of one definition (stored in the
/// separate [`StlcTypeScopes`] view, queried here from a snapshot).
pub fn binding_type_at(
    scopes: &ObservedHandle<ScopeGraph<StlcScope>>,
    _uri: &str,
    definition: NodeId,
) -> Result<Option<StlcTypeValue>> {
    let type_scope = type_scope(_uri, definition);
    match scopes.node(type_scope.node())? {
        Some(payload) => match &*payload {
            plingo::framework::scope::ScopeNode::Scope(
                super::name_resolve::StlcScopeData::Type(ty),
            ) => Ok(Some(ty.clone())),
            _ => Ok(None),
        },
        None => Ok(None),
    }
}
