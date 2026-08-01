//! Incremental bidirectional typechecking for the STLC example.

use std::sync::Arc;

use plingo::{
    component::{
        parse::{AstKey, data::AstBox},
        scope::{ScopeId, ScopeProperty},
        semantic::{Elaboration, ElaboratorCx, ElaboratorError},
    },
    rlregex,
};

use super::{
    name_resolve::{
        StlcScope, StlcScopeData, StlcScopeKey, StlcScopeLabel, StlcTypeError, StlcTypeValue,
    },
    syntax::{StlcDeclaration, StlcDocument, StlcExpr, StlcParam, StlcType, StlcTypeAtom},
};

/// Input mode is part of an elaborator task identity.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
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
    pub expression: AstKey,
    pub error: StlcTypeError,
}

#[derive(plingo::ElaboratorRole)]
#[elaborator(
    domain = StlcScope,
    input = StlcTypeMode,
    output = Option<StlcTypeValue>,
    diagnostic = StlcTypeDiagnostic,
    access = Extend,
)]
pub struct StlcTypes;

type TypeCx<'a, 'transaction, 'nodes> = ElaboratorCx<'a, 'transaction, 'nodes, StlcTypes>;
type TypeError = ElaboratorError<StlcTypeDiagnostic>;

fn stlc_typecheck_declaration(
    cx: &mut TypeCx<'_, '_, '_>,
    declaration: Arc<StlcDeclaration>,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let current = cx.ast_key();
    let StlcDeclaration::Value(_, parameters, annotation, body) = declaration.as_ref() else {
        return Ok(None);
    };

    let declared = match annotation {
        Some(syntax) => match type_from_syntax(cx, *syntax)? {
            Some(ty) => Some(ty),
            None => {
                emit_diagnostic(cx, current, StlcTypeError::InvalidAnnotation);
                return Ok(None);
            }
        },
        None => None,
    };
    let (parameter_types, body_expected) = match declared.as_ref() {
        Some(signature) => {
            let Some((types, body)) = split_function_type(signature, parameters.len()) else {
                emit_diagnostic(cx, current, StlcTypeError::InvalidAnnotation);
                return Ok(None);
            };
            for (parameter, expected) in parameters.iter().zip(&types) {
                if !parameter_matches(cx, *parameter, expected)? {
                    return Ok(None);
                }
            }
            (types, Some(body))
        }
        None => {
            let mut types = Vec::with_capacity(parameters.len());
            for parameter in parameters {
                let Some(ty) = parameter_annotation(cx, *parameter)? else {
                    emit_diagnostic(
                        cx,
                        parameter.key(),
                        StlcTypeError::MissingParameterAnnotation,
                    );
                    return Ok(None);
                };
                types.push(ty);
            }
            (types, None)
        }
    };

    // The declaration task owns every type scope for its signature. Publishing
    // an explicit signature before visiting the body makes recursive references
    // observe one stable, declaration-owned type instead of competing writes.
    if let Some(signature) = &declared {
        emit_binding_type(cx, current.clone(), signature.clone())?;
    }
    for (parameter, ty) in parameters.iter().zip(&parameter_types) {
        emit_binding_type(cx, parameter.key(), ty.clone())?;
    }

    let body_scope = cx.scope(StlcScopeKey::Lexical(current.clone()))?;
    let body_ty = match body_expected {
        Some(expected) => check_child(cx, *body, body_scope, expected),
        None => infer_child(cx, *body, body_scope),
    }?;
    let Some(body_ty) = body_ty else {
        return Ok(None);
    };

    let binding_ty = declared.unwrap_or_else(|| curry_type(&parameter_types, body_ty));
    if annotation.is_none() {
        emit_binding_type(cx, current, binding_ty.clone())?;
    }
    Ok(Some(binding_ty))
}

fn stlc_typecheck_expression(
    cx: &mut TypeCx<'_, '_, '_>,
    expression: Arc<StlcExpr>,
) -> Result<Option<StlcTypeValue>, TypeError> {
    match cx.input().expected() {
        Some(expected) => check_expression(cx, expression, expected),
        None => infer_expression(cx, expression),
    }
}

fn infer_expression(
    cx: &mut TypeCx<'_, '_, '_>,
    expression: Arc<StlcExpr>,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let incoming = cx.incoming_scope();
    let current = cx.ast_key();
    match expression.as_ref() {
        StlcExpr::True(_) | StlcExpr::False(_) => Ok(Some(StlcTypeValue::Bool)),
        StlcExpr::Number(_) => Ok(Some(StlcTypeValue::Nat)),
        StlcExpr::Unit(_) => Ok(Some(StlcTypeValue::Unit)),
        StlcExpr::Succ(inner) => check_child_as(cx, *inner, incoming, StlcTypeValue::Nat),
        StlcExpr::Add(left, right) => {
            let left = check_child(cx, *left, incoming, StlcTypeValue::Nat)?;
            let right = check_child(cx, *right, incoming, StlcTypeValue::Nat)?;
            Ok((left.is_some() && right.is_some()).then_some(StlcTypeValue::Nat))
        }
        StlcExpr::Case(scrutinee, zero_branch, _, successor_branch) => {
            let scrutinized = check_child(cx, *scrutinee, incoming, StlcTypeValue::Nat)?;
            let zero_ty = infer_child(cx, *zero_branch, incoming)?;
            let successor_scope = cx.scope(StlcScopeKey::CaseSuccessor(current.clone()))?;
            emit_binding_type(cx, current.clone(), StlcTypeValue::Nat)?;
            let successor_ty = infer_child(cx, *successor_branch, successor_scope)?;
            Ok(agree_branches(
                cx,
                current,
                scrutinized,
                zero_ty,
                successor_ty,
            ))
        }
        StlcExpr::Group(inner) => infer_child(cx, *inner, incoming),
        StlcExpr::If(condition, then_branch, else_branch) => {
            let condition = check_child(cx, *condition, incoming, StlcTypeValue::Bool)?;
            let then_ty = infer_child(cx, *then_branch, incoming)?;
            let else_ty = infer_child(cx, *else_branch, incoming)?;
            Ok(agree_branches(cx, current, condition, then_ty, else_ty))
        }
        StlcExpr::Apply(function, argument) => {
            let Some(function_ty) = infer_child(cx, *function, incoming)? else {
                return Ok(None);
            };
            let StlcTypeValue::Arrow(domain, codomain) = function_ty else {
                emit_diagnostic(
                    cx,
                    current,
                    StlcTypeError::NonFunctionApplication { found: function_ty },
                );
                return Ok(None);
            };
            Ok(check_child(cx, *argument, incoming, *domain)?.map(|_| *codomain))
        }
        StlcExpr::Let(_, value, body) => {
            let Some(value_ty) = infer_child(cx, *value, incoming)? else {
                return Ok(None);
            };
            let body_scope = cx.scope(StlcScopeKey::Lexical(current.clone()))?;
            emit_binding_type(cx, current, value_ty)?;
            infer_child(cx, *body, body_scope)
        }
        StlcExpr::Lambda(parameter, body) => {
            let Some(parameter_ty) = parameter_annotation(cx, *parameter)? else {
                emit_diagnostic(cx, current, StlcTypeError::MissingParameterAnnotation);
                return Ok(None);
            };
            let lambda_scope = cx.scope(StlcScopeKey::Lexical(current.clone()))?;
            emit_binding_type(cx, parameter.key(), parameter_ty.clone())?;
            Ok(infer_child(cx, *body, lambda_scope)?
                .map(|body_ty| StlcTypeValue::Arrow(Box::new(parameter_ty), Box::new(body_ty))))
        }
        StlcExpr::Variable(token) => infer_variable(cx, current, incoming, *token),
        StlcExpr::Error(_) => Ok(None),
    }
}

fn check_expression(
    cx: &mut TypeCx<'_, '_, '_>,
    expression: Arc<StlcExpr>,
    expected: StlcTypeValue,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let incoming = cx.incoming_scope();
    let current = cx.ast_key();
    match expression.as_ref() {
        StlcExpr::Lambda(parameter, body) => {
            let StlcTypeValue::Arrow(domain, codomain) = expected.clone() else {
                emit_diagnostic(cx, current, StlcTypeError::ExpectedArrow { expected });
                return Ok(None);
            };
            if !parameter_matches(cx, *parameter, &domain)? {
                return Ok(None);
            }
            let lambda_scope = cx.scope(StlcScopeKey::Lexical(current.clone()))?;
            emit_binding_type(cx, parameter.key(), (*domain).clone())?;
            check_child_as(cx, *body, lambda_scope, *codomain)
        }
        StlcExpr::If(condition, then_branch, else_branch) => {
            let condition = check_child(cx, *condition, incoming, StlcTypeValue::Bool)?;
            let then_branch = check_child(cx, *then_branch, incoming, expected.clone())?;
            let else_branch = check_child(cx, *else_branch, incoming, expected.clone())?;
            Ok(
                (condition.is_some() && then_branch.is_some() && else_branch.is_some())
                    .then_some(expected),
            )
        }
        StlcExpr::Case(scrutinee, zero_branch, _, successor_branch) => {
            let scrutinized = check_child(cx, *scrutinee, incoming, StlcTypeValue::Nat)?;
            let successor_scope = cx.scope(StlcScopeKey::CaseSuccessor(current.clone()))?;
            emit_binding_type(cx, current.clone(), StlcTypeValue::Nat)?;
            let zero_branch = check_child(cx, *zero_branch, incoming, expected.clone())?;
            let successor_branch =
                check_child(cx, *successor_branch, successor_scope, expected.clone())?;
            Ok(
                (scrutinized.is_some() && zero_branch.is_some() && successor_branch.is_some())
                    .then_some(expected),
            )
        }
        StlcExpr::Let(_, value, body) => {
            let Some(value_ty) = infer_child(cx, *value, incoming)? else {
                return Ok(None);
            };
            let body_scope = cx.scope(StlcScopeKey::Lexical(current.clone()))?;
            emit_binding_type(cx, current, value_ty)?;
            check_child_as(cx, *body, body_scope, expected)
        }
        StlcExpr::Group(inner) => check_child_as(cx, *inner, incoming, expected),
        _ => Ok(infer_expression(cx, expression)?
            .and_then(|found| check_inferred(cx, current, expected, found))),
    }
}

fn infer_variable(
    cx: &mut TypeCx<'_, '_, '_>,
    current: AstKey,
    incoming: ScopeId<StlcScope>,
    token: plingo::component::parse::AstToken<super::syntax::StlcToken>,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let name = cx.text(token)?;
    let mut visible = cx
        .resolve_from(
            incoming,
            rlregex!(StlcScopeLabel::Lexical * StlcScopeLabel::Declaration),
            |data| {
                matches!(
                    data,
                    StlcScopeData::Declaration { name: binding, .. }
                        if binding.as_ref() == name.as_ref()
                )
            },
        )
        .into_iter();
    let Some(resolution) = visible.next() else {
        emit_diagnostic(cx, current, StlcTypeError::UnboundVariable { name });
        return Ok(None);
    };
    let candidates = visible.count() + 1;
    if candidates != 1 {
        emit_diagnostic(
            cx,
            current,
            StlcTypeError::AmbiguousVariable { name, candidates },
        );
        return Ok(None);
    }
    let StlcScopeData::Declaration { definition, .. } = &resolution.data else {
        return Ok(None);
    };
    binding_type_at(cx, definition)
}

fn agree_branches(
    cx: &mut TypeCx<'_, '_, '_>,
    current: AstKey,
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
                cx,
                current,
                StlcTypeError::BranchMismatch { then_ty, else_ty },
            );
            None
        }
        _ => None,
    }
}

fn check_inferred(
    cx: &mut TypeCx<'_, '_, '_>,
    current: AstKey,
    expected: StlcTypeValue,
    found: StlcTypeValue,
) -> Option<StlcTypeValue> {
    if found == expected {
        Some(expected)
    } else {
        emit_diagnostic(cx, current, StlcTypeError::Mismatch { expected, found });
        None
    }
}

fn type_from_atom(
    cx: &mut TypeCx<'_, '_, '_>,

    atom: AstBox<StlcTypeAtom>,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let atom = cx.ast(atom)?;
    match atom.as_ref() {
        StlcTypeAtom::Nat(_) => Ok(Some(StlcTypeValue::Nat)),
        StlcTypeAtom::Bool(_) => Ok(Some(StlcTypeValue::Bool)),
        StlcTypeAtom::Unit(_) => Ok(Some(StlcTypeValue::Unit)),
        StlcTypeAtom::Parenthesized(inner) => type_from_syntax(cx, *inner),
    }
}

fn type_from_syntax(
    cx: &mut TypeCx<'_, '_, '_>,
    syntax: AstBox<StlcType>,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let syntax = cx.ast(syntax)?;
    match syntax.as_ref() {
        StlcType::Arrow(domain, codomain) => {
            let Some(domain) = type_from_atom(cx, *domain)? else {
                return Ok(None);
            };
            let Some(codomain) = type_from_syntax(cx, *codomain)? else {
                return Ok(None);
            };
            Ok(Some(StlcTypeValue::Arrow(
                Box::new(domain),
                Box::new(codomain),
            )))
        }
        StlcType::Atom(atom) => type_from_atom(cx, *atom),
        StlcType::Error(_) => Ok(None),
    }
}

fn parameter_annotation(
    cx: &mut TypeCx<'_, '_, '_>,
    parameter: AstBox<StlcParam>,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let parameter = cx.ast(parameter)?;
    let annotation = match parameter.as_ref() {
        StlcParam::Bare(_, annotation) | StlcParam::Parenthesized(_, annotation) => annotation,
    };
    match annotation {
        Some(syntax) => type_from_syntax(cx, *syntax),
        None => Ok(None),
    }
}

fn parameter_matches(
    cx: &mut TypeCx<'_, '_, '_>,
    parameter: AstBox<StlcParam>,
    expected: &StlcTypeValue,
) -> Result<bool, TypeError> {
    let Some(found) = parameter_annotation(cx, parameter)? else {
        return Ok(true);
    };
    if found == *expected {
        return Ok(true);
    }
    emit_diagnostic(
        cx,
        parameter.key(),
        StlcTypeError::Mismatch {
            expected: expected.clone(),
            found,
        },
    );
    Ok(false)
}

/// Splits a declaration signature into one type per parameter and its body
/// result. A curried declaration with `n` parameters must have `n` arrows.
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

/// Reads the mapped type data from the paper-style `--TYPE-->` scope.
fn binding_type_at(
    cx: &mut TypeCx<'_, '_, '_>,
    definition: &AstKey,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let Some(type_scope) = cx.find_scope(StlcScopeKey::Type(definition.clone())) else {
        return Ok(None);
    };
    Ok(match cx.data(type_scope) {
        Some(StlcScopeData::Type(ty)) => Some(ty),
        _ => None,
    })
}

/// Publishes `declaration --TYPE--> type_scope` and its mapped type data.
fn emit_binding_type(
    cx: &mut TypeCx<'_, '_, '_>,
    definition: AstKey,
    ty: StlcTypeValue,
) -> Result<(), TypeError> {
    let declaration = cx.scope(StlcScopeKey::Declaration(definition.clone()))?;
    let type_scope = cx.declare(StlcScopeKey::Type(definition), StlcScopeData::Type(ty))?;
    cx.edge(
        declaration,
        StlcScopeLabel::Type,
        type_scope,
        ScopeProperty::Acyclic,
    )?;
    cx.seal(type_scope)?;
    Ok(())
}

fn emit_diagnostic(cx: &mut TypeCx<'_, '_, '_>, expression: AstKey, error: StlcTypeError) {
    cx.report(StlcTypeDiagnostic { expression, error });
}

fn infer_child(
    cx: &mut TypeCx<'_, '_, '_>,
    child: AstBox<StlcExpr>,
    incoming: ScopeId<StlcScope>,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let child = cx.schedule(child, incoming, StlcTypeMode::Infer)?;
    Ok(cx.observe(&child)?.flatten())
}

fn check_child(
    cx: &mut TypeCx<'_, '_, '_>,
    child: AstBox<StlcExpr>,
    incoming: ScopeId<StlcScope>,
    expected: StlcTypeValue,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let child = cx.schedule(child, incoming, StlcTypeMode::Check(expected))?;
    Ok(cx.observe(&child)?.flatten())
}

fn check_child_as(
    cx: &mut TypeCx<'_, '_, '_>,
    child: AstBox<StlcExpr>,
    incoming: ScopeId<StlcScope>,
    expected: StlcTypeValue,
) -> Result<Option<StlcTypeValue>, TypeError> {
    Ok(check_child(cx, child, incoming, expected.clone())?.map(|_| expected))
}

fn stlc_typecheck_root(
    cx: &mut TypeCx<'_, '_, '_>,
    document: Arc<StlcDocument>,
) -> Result<Option<StlcTypeValue>, TypeError> {
    let scope = cx.attach_root(StlcScopeKey::Lexical(cx.ast_key()))?;
    if let StlcDocument::Lines(declarations) = document.as_ref() {
        for declaration in declarations {
            cx.schedule(*declaration, scope, StlcTypeMode::Infer)?;
        }
    }
    Ok(None)
}

pub fn stlc_type_rules() -> impl for<'a, 't, 'n> Fn(
    &mut ElaboratorCx<'a, 't, 'n, StlcTypes>,
) -> Result<
    Elaboration<Option<StlcTypeValue>>,
    TypeError,
> + Send
+ Sync
+ 'static {
    plingo::component::semantic::rules::<StlcTypes>()
        .root(stlc_typecheck_root)
        .case::<StlcDeclaration, _>(stlc_typecheck_declaration)
        .case::<StlcExpr, _>(stlc_typecheck_expression)
        .otherwise(|_| Ok(None))
        .build()
        .expect("STLC type rule table is valid")
}
