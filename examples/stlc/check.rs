//! Component-separated STLC checker (Cut J, plan §23).
//!
//! Three independent EachKey components — synthesis, diagnostics, and
//! definition publication — split the single-node bidirectional pass.
//! Expectation writing is part of the parent's synthesis component (each
//! parent node writes `StlcExpectedTypes` for its children based on grammar
//! rules and the parent's own expectation). No `run` recursion, no
//! `StlcTypeMode`, no graph `Type` nodes, no `StlcTypeScopes`, no `Arrow`.
//!
//! Synthesis reads child `StlcSynthesizedTypes` and parent `StlcExpectedTypes`,
//! writes own `StlcSynthesizedTypes` and child `StlcExpectedTypes`.
//! Diagnostics read both for the same node. Definition publication reads
//! `StlcDeclarationScopes` and `StlcSynthesizedTypes`.

use std::sync::Arc;

use plingo::framework::parse::{ParserTreePayloads, TreeParseUnits};
use plingo::reactive::component::EachKey;
use plingo::reactive::prelude::*;
use plingo::reactive::view::Node;
use reactive_macros::view;

use super::name_resolve::{
    Scope, StlcDeclarationScopes, StlcResolution, StlcResolvedReferences, StlcScope,
};
use super::syntax::{
    StlcCase, StlcDeclarationCase, StlcDocument, StlcExprCase, StlcParamCase, StlcTree,
    StlcTypeAtomCase, StlcTypeCase,
};

// ---------------------------------------------------------------------------
// Type value — canonical curried function spine replaces boxed Arrow
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionType {
    parameters: Arc<[StlcTypeValue]>,
    result: StlcTypeValue,
}

impl FunctionType {
    pub fn parameters(&self) -> &[StlcTypeValue] {
        &self.parameters
    }
    pub fn result(&self) -> &StlcTypeValue {
        &self.result
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StlcTypeValue {
    Nat,
    Bool,
    Unit,
    Function(Arc<FunctionType>),
}

impl StlcTypeValue {
    pub fn function<I>(parameters: I, result: StlcTypeValue) -> StlcTypeValue
    where I: IntoIterator<Item = StlcTypeValue> {
        let mut params: Vec<StlcTypeValue> = parameters.into_iter().collect();
        let terminal = match result {
            StlcTypeValue::Function(f) => { params.extend(f.parameters.iter().cloned()); f.result.clone() }
            other => other,
        };
        if params.is_empty() { terminal }
        else { StlcTypeValue::Function(Arc::new(FunctionType { parameters: Arc::from(params), result: terminal })) }
    }
    pub fn apply_one(&self) -> Option<StlcTypeValue> {
        match self {
            StlcTypeValue::Function(f) => {
                if f.parameters.len() == 1 { Some(f.result.clone()) }
                else { Some(StlcTypeValue::Function(Arc::new(FunctionType { parameters: Arc::from(&f.parameters[1..]), result: f.result.clone() }))) }
            }
            _ => None,
        }
    }
    pub fn function_parts(&self) -> Option<(Vec<StlcTypeValue>, StlcTypeValue)> {
        match self { StlcTypeValue::Function(f) => Some((f.parameters.to_vec(), f.result.clone())), _ => None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcTypeError {
    Mismatch { expected: StlcTypeValue, found: StlcTypeValue },
    NonFunctionApplication { found: StlcTypeValue },
    BranchMismatch { then_ty: StlcTypeValue, else_ty: StlcTypeValue },
    UnboundVariable { name: Arc<str> },
    MissingParameterAnnotation,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcTypeResult { Known(StlcTypeValue), Unknown }
impl From<Option<StlcTypeValue>> for StlcTypeResult {
    fn from(v: Option<StlcTypeValue>) -> Self { match v { Some(v) => Self::Known(v), None => Self::Unknown } }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StlcTypeDiagnostic { pub expression: Node<StlcTree>, pub error: StlcTypeError }

#[view]
pub struct StlcSynthesizedTypes(Map<Node<StlcTree>, StlcTypeResult>);

/// Expected type per edge `(parent, child)` (plan §23.5). Each parent's
/// synthesis writes entries for its children; the child reads its own
/// expectation by looking up `(tree_parent, self)`.
#[view]
pub struct StlcExpectedTypes(Map<(Node<StlcTree>, Node<StlcTree>), StlcTypeValue>);

#[view]
pub struct StlcDefinitionTypes(Map<Scope<StlcScope>, StlcTypeResult>);

#[view]
pub struct StlcTypeDiagnostics(List<Node<StlcTree>, StlcTypeDiagnostic>);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn child_type(id: &Node<StlcTree>) -> Option<StlcTypeValue> {
    observe_view::<StlcSynthesizedTypes>().ok()
        .and_then(|v| v.get(id).ok()).flatten()
        .and_then(|r| match &*r { StlcTypeResult::Known(ty) => Some(ty.clone()), _ => None })
}

fn node_expected(id: &Node<StlcTree>) -> Option<StlcTypeValue> {
    // Find the parent via the tree parent fact, then read the edge-keyed expectation.
    let parent = StlcTree::observe_parent(id.clone()).ok()??;
    observe_view::<StlcExpectedTypes>().ok()
        .and_then(|v| v.get(&(parent, id.clone())).ok()).flatten()
        .map(|a| (*a).clone())
}

// ---------------------------------------------------------------------------
// Synthesis + expectation: per-node (plan §23.4/§23.5)
// ---------------------------------------------------------------------------

#[reactive_macros::component]
pub fn synthesize_node(key: EachKey<ParserTreePayloads<StlcDocument>>) -> Result<()> {
    let id: Node<StlcTree> = key;
    let case_opt = StlcTree::observe_case(id.clone())?;
    let mut errs = Vec::<StlcTypeDiagnostic>::new();
    let syn = match &case_opt {
        Some(case) => synthesize_and_expect(&id, case, &mut errs),
        None => StlcTypeResult::Unknown,
    };
    emit_view::<StlcSynthesizedTypes>()?.insert(id.clone(), syn)?;
    write_diagnostics(&id, errs);
    Ok(())
}

/// Computes the synthesized type for `id` AND writes expectations for its
/// children (plan §23.4 rules, §23.5 table). Returns the synthesized result.
fn synthesize_and_expect(id: &Node<StlcTree>, case: &StlcCase, errs: &mut Vec<StlcTypeDiagnostic>) -> StlcTypeResult {
    let exp = |child, ty| expect_child_typed(id, child, ty);
    let parent_exp = node_expected(id);
    let result = match case {
        StlcCase::Document(_) | StlcCase::Path(_) => StlcTypeResult::Unknown,
        StlcCase::Type(StlcTypeCase::Arrow { f0, f1 }) => {
            exp(f0, None);
            exp(f1, None);
            match (child_type(f0), child_type(f1)) {
                (Some(p), Some(r)) => StlcTypeResult::Known(StlcTypeValue::function([p], r)),
                _ => StlcTypeResult::Unknown,
            }
        }
        StlcCase::Type(StlcTypeCase::Atom { f0 }) => {
            exp(f0, None);
            child_type(f0).into()
        }
        StlcCase::TypeAtom(StlcTypeAtomCase::Nat { .. }) => StlcTypeResult::Known(StlcTypeValue::Nat),
        StlcCase::TypeAtom(StlcTypeAtomCase::Bool { .. }) => StlcTypeResult::Known(StlcTypeValue::Bool),
        StlcCase::TypeAtom(StlcTypeAtomCase::Unit { .. }) => StlcTypeResult::Known(StlcTypeValue::Unit),
        StlcCase::TypeAtom(StlcTypeAtomCase::Parenthesized { f0 }) => {
            exp(f0, None);
            child_type(f0).into()
        }
        StlcCase::Expr(StlcExprCase::True { .. }) | StlcCase::Expr(StlcExprCase::False { .. }) => StlcTypeResult::Known(StlcTypeValue::Bool),
        StlcCase::Expr(StlcExprCase::Number { .. }) => StlcTypeResult::Known(StlcTypeValue::Nat),
        StlcCase::Expr(StlcExprCase::Unit { .. }) => StlcTypeResult::Known(StlcTypeValue::Unit),
        StlcCase::Expr(StlcExprCase::Group { f0 }) => { exp(f0, parent_exp.clone()); child_type(f0).into() }
        StlcCase::Expr(StlcExprCase::Succ { f0 }) => { exp(f0, Some(StlcTypeValue::Nat)); StlcTypeResult::Known(StlcTypeValue::Nat) }
        StlcCase::Expr(StlcExprCase::Add { f0, f1 }) => {
            exp(f0, Some(StlcTypeValue::Nat));
            exp(f1, Some(StlcTypeValue::Nat));
            StlcTypeResult::Known(StlcTypeValue::Nat)
        }
        StlcCase::Expr(StlcExprCase::If { f0: cond, f1: then_branch, f2: else_branch, .. }) => {
            exp(cond, Some(StlcTypeValue::Bool));
            exp(then_branch, parent_exp.clone());
            exp(else_branch, parent_exp.clone());
            match (child_type(then_branch), child_type(else_branch)) {
                (Some(a), Some(b)) if a == b => StlcTypeResult::Known(a),
                _ => StlcTypeResult::Unknown,
            }
        }
        StlcCase::Expr(StlcExprCase::Lambda { f0: parameter, f1: body }) => {
            let pe = parent_exp.as_ref().and_then(|pe| pe.function_parts());
            let param_exp = pe.as_ref().and_then(|(ps, _)| ps.first().cloned());
            let body_exp = pe.map(|(mut ps, r)| { if ps.len() <= 1 { r } else { ps.remove(0); StlcTypeValue::function(ps, r) } });
            exp(parameter, param_exp);
            exp(body, body_exp);
            match (child_type(parameter), child_type(body)) {
                (Some(p), Some(r)) => StlcTypeResult::Known(StlcTypeValue::function([p], r)),
                _ => StlcTypeResult::Unknown,
            }
        }
        StlcCase::Expr(StlcExprCase::Apply { f0, f1 }) => {
            exp(f0, None);
            let fn_ty = child_type(f0);
            let arg_exp = fn_ty.as_ref().and_then(|fn_ty| fn_ty.function_parts().and_then(|(ps, _)| ps.into_iter().next()));
            exp(f1, arg_exp);
            if let Some(fn_ty) = fn_ty.as_ref() {
                if fn_ty.function_parts().is_none() {
                    errs.push(StlcTypeDiagnostic { expression: id.clone(), error: StlcTypeError::NonFunctionApplication { found: fn_ty.clone() } });
                }
            }
            match fn_ty { Some(fn_ty) => fn_ty.apply_one().into(), None => StlcTypeResult::Unknown }
        }
        StlcCase::Expr(StlcExprCase::Let { f1: value, f2: body, .. }) => {
            exp(value, None);
            exp(body, parent_exp);
            let _ = child_type(value);
            child_type(body).into()
        }
        StlcCase::Expr(StlcExprCase::Case { f0: scrutinee, f1: zero_branch, f3: successor_branch, .. }) => {
            exp(scrutinee, Some(StlcTypeValue::Nat));
            exp(zero_branch, parent_exp.clone());
            exp(successor_branch, parent_exp);
            match (child_type(zero_branch), child_type(successor_branch)) {
                (Some(a), Some(b)) if a == b => StlcTypeResult::Known(a),
                _ => StlcTypeResult::Unknown,
            }
        }
        StlcCase::Expr(StlcExprCase::Variable { .. }) => {
            match observe_view::<StlcResolvedReferences>().ok()
                .and_then(|v| v.get(id).ok()).flatten().map(|a| a.as_ref().clone())
            {
                Some(StlcResolution::Resolved { declaration }) => {
                    observe_view::<StlcDefinitionTypes>().ok()
                        .and_then(|v| v.get(&declaration).ok()).flatten()
                        .map(|r| (&*r).clone())
                        .unwrap_or(StlcTypeResult::Unknown)
                }
                Some(StlcResolution::Unbound { name }) => {
                    errs.push(StlcTypeDiagnostic { expression: id.clone(), error: StlcTypeError::UnboundVariable { name } });
                    StlcTypeResult::Unknown
                }
                None => StlcTypeResult::Unknown,
            }
        }
        StlcCase::Declaration(StlcDeclarationCase::Value { f1: annotation, f2: body, f3: parameters, .. }) => {
            if let Some(annotation) = annotation { exp(annotation, None); }
            for p in parameters { exp(p, None); }
            let body_exp = annotation.as_ref().and_then(child_type);
            exp(body, body_exp);
            let result = child_type(body);
            let mut result = result;
            for p in parameters.iter().rev() {
                let p_ty = child_type(p);
                result = match (p_ty, result) {
                    (Some(p), Some(r)) => Some(StlcTypeValue::function([p], r)),
                    _ => None,
                };
            }
            if let Some(annotation) = annotation {
                if let Some(expected) = child_type(annotation) {
                    if let Some(ref found) = result {
                        if expected != *found {
                            errs.push(StlcTypeDiagnostic { expression: id.clone(), error: StlcTypeError::Mismatch { expected, found: found.clone() } });
                            result = None;
                        } else { result = Some(expected); }
                    }
                }
            }
            result.into()
        }
        StlcCase::Param(StlcParamCase::Bare { f1: Some(annotation), .. })
        | StlcCase::Param(StlcParamCase::Parenthesized { f1: Some(annotation), .. }) => {
            exp(annotation, None);
            child_type(annotation).into()
        }
        StlcCase::Param(StlcParamCase::Bare { f1: None, .. })
        | StlcCase::Param(StlcParamCase::Parenthesized { f1: None, .. }) => {
            errs.push(StlcTypeDiagnostic { expression: id.clone(), error: StlcTypeError::MissingParameterAnnotation });
            StlcTypeResult::Unknown
        }
        _ => StlcTypeResult::Unknown,
    };

    // Type mismatch: expected vs found
    if let Some(expected) = node_expected(id) {
        if let StlcTypeResult::Known(ref found) = result {
            if expected != *found {
                errs.push(StlcTypeDiagnostic { expression: id.clone(), error: StlcTypeError::Mismatch { expected, found: found.clone() } });
            }
        }
    }
    // Use a separate insert for diagnostics to avoid `id` borrow conflict.
    result
}

fn write_diagnostics(id: &Node<StlcTree>, errs: Vec<StlcTypeDiagnostic>) {
    if let Ok(view) = emit_view::<StlcTypeDiagnostics>() {
        let _ = view.replace(id, errs);
    }
}

/// Writes an expected type for a child node (plan §23.5). `None` removes
/// the expectation (child is inferred). Uses `observe_view` to check if the
/// child exists; writes via `emit_view`.
fn expect_child_typed(parent: &Node<StlcTree>, child: &Node<StlcTree>, expected: Option<StlcTypeValue>) {
    let Ok(view) = emit_view::<StlcExpectedTypes>() else { return };
    match expected {
        Some(ty) => { let _ = view.insert((parent.clone(), child.clone()), ty); }
        None => { let _ = view.remove((parent.clone(), child.clone())); }
    }
}

// ---------------------------------------------------------------------------
// Definition publication: binder types (plan §23.6)
// ---------------------------------------------------------------------------

#[reactive_macros::component]
pub fn publish_definition(key: EachKey<ParserTreePayloads<StlcDocument>>) -> Result<()> {
    let id: Node<StlcTree> = key;
    let Some(case) = StlcTree::observe_case(id.clone())? else { return Ok(()) };
    let is_binder = matches!(&case,
        StlcCase::Declaration(StlcDeclarationCase::Value { .. })
        | StlcCase::Param(_)
        | StlcCase::Expr(StlcExprCase::Let { .. })
    );
    if !is_binder { return Ok(()) }
    let Some(scope) = observe_view::<StlcDeclarationScopes>()?.get(&id)? else { return Ok(()) };
    let scope_val = (*scope).clone();
    let syn = child_type(&id);
    emit_view::<StlcDefinitionTypes>()?.insert(scope_val, syn.into())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Installer
// ---------------------------------------------------------------------------

pub fn check_pass_install(engine: &mut plingo::reactive::Engine) -> Result<()> {
    synthesize_node_install(engine)?;
    publish_definition_install(engine)?;
    Ok(())
}