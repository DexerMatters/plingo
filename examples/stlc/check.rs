//! Incremental bidirectional typechecking for the STLC example.

use std::sync::Arc;

use fluent_uri::Uri;
use plingo::{
    Component, Context, NodeError, Result,
    component::writes,
    component::{
        parse::{AstKey, data::AstBox},
        scope::{
            PathOrder, ScopeDefinitions, ScopeEdges, ScopeId, ScopeProperty, SourceRequirements,
        },
    },
    scope_path,
};

use super::{
    name_resolve::{
        NameAst, NameDocument, StlcScope, StlcScopeData, StlcScopeKey, StlcScopeLabel,
        StlcTypeError, StlcTypeValue,
    },
    syntax::{
        StlcDeclaration, StlcDocument, StlcExpr, StlcParam, StlcToken, StlcType, StlcTypeAtom,
    },
};

/// Input mode is part of a type task identity.
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

/// One typing judgment: check or infer one AST item under one scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeOf {
    pub ast: AstKey,
    pub incoming: ScopeId<StlcScope>,
    pub mode: StlcTypeMode,
}

/// The one document coordinator for type checking.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeDocument {
    pub uri: Uri<&'static str>,
}

impl Component for TypeDocument {
    type Output = Option<StlcTypeValue>;
    type Writes = writes!();

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<Self::Output> {
        type_document(cx, self.uri)
    }
}

impl Component for TypeOf {
    type Output = Option<StlcTypeValue>;
    type Writes = writes!(
        ScopeDefinitions<StlcScope>,
        ScopeEdges<StlcScope>,
        SourceRequirements<StlcScope>,
        plingo::component::Diagnostics<StlcTypeDiagnostic>,
    );

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<Self::Output> {
        type_of(cx, self.ast.clone(), self.incoming, self.mode.clone())
    }
}

type TypeCx<'tx> = Context<'tx, TypeOf>;

fn type_document(
    cx: &mut Context<'_, TypeDocument>,
    uri: Uri<&'static str>,
) -> Result<Option<StlcTypeValue>> {
    cx.call(NameDocument { uri })?;
    let Some(document) = cx
        .view::<plingo::component::Parsed<StlcToken, StlcDocument>>()
        .accepted(uri)?
    else {
        return Ok(None);
    };
    let root_ast = document.key();
    let incoming = {
        let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
        scopes.scope(StlcScopeKey::Lexical(root_ast))?
    };
    if let StlcDocument::Lines(declarations) = document.value().as_ref() {
        cx.keep_all(declarations.iter().map(|declaration| TypeOf {
            ast: declaration.key(),
            incoming,
            mode: StlcTypeMode::Infer,
        }));
    }
    Ok(None)
}

fn type_of(
    cx: &mut TypeCx<'_>,
    ast: AstKey,
    incoming: ScopeId<StlcScope>,
    mode: StlcTypeMode,
) -> Result<Option<StlcTypeValue>> {
    cx.call(NameAst {
        ast: ast.clone(),
        incoming,
    })?;
    let (declaration, expression, ty, atom) = {
        let mut parsed = cx.view::<plingo::component::Parsed<StlcToken, StlcDocument>>();
        (
            parsed.artifact::<StlcDeclaration>(ast.clone()),
            parsed.artifact::<StlcExpr>(ast.clone()),
            parsed.artifact::<StlcType>(ast.clone()),
            parsed.artifact::<StlcTypeAtom>(ast.clone()),
        )
    };
    if let Some(declaration) = declaration {
        type_declaration(cx, incoming, mode, ast, declaration)
    } else if let Some(expression) = expression {
        type_expression(cx, incoming, mode, ast, expression)
    } else if let Some(ty) = ty {
        infer_type(cx, incoming, ty)
    } else if let Some(atom) = atom {
        infer_type_atom(cx, incoming, atom)
    } else {
        Ok(None)
    }
}

fn infer_child(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    child: AstKey,
) -> Result<Option<StlcTypeValue>> {
    cx.call(TypeOf {
        ast: child,
        incoming,
        mode: StlcTypeMode::Infer,
    })
}

fn check_child(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    child: AstKey,
    expected: StlcTypeValue,
) -> Result<Option<StlcTypeValue>> {
    cx.call(TypeOf {
        ast: child,
        incoming,
        mode: StlcTypeMode::Check(expected),
    })
}

fn check_child_as(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    child: AstKey,
    expected: StlcTypeValue,
) -> Result<Option<StlcTypeValue>> {
    Ok(check_child(cx, incoming, child, expected.clone())?.map(|_| expected))
}

fn type_declaration(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    _mode: StlcTypeMode,
    current: AstKey,
    declaration: Arc<StlcDeclaration>,
) -> Result<Option<StlcTypeValue>> {
    let StlcDeclaration::Value(_, parameters, annotation, body) = declaration.as_ref() else {
        return Ok(None);
    };
    let declared = match annotation {
        Some(syntax) => match infer_child(cx, incoming, syntax.key())? {
            Some(ty) => Some(ty),
            None => {
                emit_diagnostic(cx, current.clone(), StlcTypeError::InvalidAnnotation)?;
                return Ok(None);
            }
        },
        None => None,
    };
    let (parameter_types, body_expected) = match declared.as_ref() {
        Some(signature) => {
            let Some((types, body_ty)) = split_function_type(signature, parameters.len()) else {
                emit_diagnostic(cx, current.clone(), StlcTypeError::InvalidAnnotation)?;
                return Ok(None);
            };
            for (parameter, expected) in parameters.iter().zip(&types) {
                if !parameter_matches(cx, incoming, *parameter, expected)? {
                    return Ok(None);
                }
            }
            (types, Some(body_ty))
        }
        None => {
            let mut types = Vec::with_capacity(parameters.len());
            for parameter in parameters {
                let Some(ty) = parameter_annotation(cx, incoming, *parameter)? else {
                    emit_diagnostic(
                        cx,
                        parameter.key(),
                        StlcTypeError::MissingParameterAnnotation,
                    )?;
                    return Ok(None);
                };
                types.push(ty);
            }
            (types, None)
        }
    };

    if let Some(signature) = &declared {
        emit_binding_type(cx, current.clone(), signature.clone())?;
    }
    for (parameter, ty) in parameters.iter().zip(&parameter_types) {
        emit_binding_type(cx, parameter.key(), ty.clone())?;
    }
    let body_scope = {
        let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
        scopes.scope(StlcScopeKey::Lexical(current.clone()))?
    };
    let body_ty = match body_expected {
        Some(expected) => match check_child(cx, body_scope, body.key(), expected)? {
            Some(ty) => ty,
            None => return Ok(None),
        },
        None => match infer_child(cx, body_scope, body.key())? {
            Some(ty) => ty,
            None => return Ok(None),
        },
    };
    let binding_ty = declared.unwrap_or_else(|| curry_type(&parameter_types, body_ty));
    if annotation.is_none() {
        emit_binding_type(cx, current, binding_ty.clone())?;
    }
    Ok(Some(binding_ty))
}

fn type_expression(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    mode: StlcTypeMode,
    current: AstKey,
    expression: Arc<StlcExpr>,
) -> Result<Option<StlcTypeValue>> {
    match mode.expected() {
        Some(expected) => check_expression(cx, incoming, current, expression, expected),
        None => infer_expression(cx, incoming, current, expression),
    }
}

fn infer_expression(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    current: AstKey,
    expression: Arc<StlcExpr>,
) -> Result<Option<StlcTypeValue>> {
    match expression.as_ref() {
        StlcExpr::True(_) | StlcExpr::False(_) => Ok(Some(StlcTypeValue::Bool)),
        StlcExpr::Number(_) => Ok(Some(StlcTypeValue::Nat)),
        StlcExpr::Unit(_) => Ok(Some(StlcTypeValue::Unit)),
        StlcExpr::Succ(inner) => check_child_as(cx, incoming, inner.key(), StlcTypeValue::Nat),
        StlcExpr::Add(left, right) => {
            let left = check_child(cx, incoming, left.key(), StlcTypeValue::Nat)?;
            let right = check_child(cx, incoming, right.key(), StlcTypeValue::Nat)?;
            Ok((left.is_some() && right.is_some()).then_some(StlcTypeValue::Nat))
        }
        StlcExpr::Case(scrutinee, zero_branch, _, successor_branch) => {
            let scrutinized = check_child(cx, incoming, scrutinee.key(), StlcTypeValue::Nat)?;
            let zero_ty = infer_child(cx, incoming, zero_branch.key())?;
            let successor_scope = {
                let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
                scopes.scope(StlcScopeKey::CaseSuccessor(current.clone()))?
            };
            emit_binding_type(cx, current.clone(), StlcTypeValue::Nat)?;
            let successor_ty = infer_child(cx, successor_scope, successor_branch.key())?;
            agree_branches(cx, current, scrutinized, zero_ty, successor_ty)
        }
        StlcExpr::Group(inner) => infer_child(cx, incoming, inner.key()),
        StlcExpr::If(condition, then_branch, else_branch) => {
            let condition = check_child(cx, incoming, condition.key(), StlcTypeValue::Bool)?;
            let then_ty = infer_child(cx, incoming, then_branch.key())?;
            let else_ty = infer_child(cx, incoming, else_branch.key())?;
            agree_branches(cx, current, condition, then_ty, else_ty)
        }
        StlcExpr::Apply(function, argument) => {
            let Some(function_ty) = infer_child(cx, incoming, function.key())? else {
                return Ok(None);
            };
            let StlcTypeValue::Arrow(domain, codomain) = function_ty else {
                emit_diagnostic(
                    cx,
                    current,
                    StlcTypeError::NonFunctionApplication { found: function_ty },
                )?;
                return Ok(None);
            };
            Ok(check_child(cx, incoming, argument.key(), *domain)?.map(|_| *codomain))
        }
        StlcExpr::Let(_, value, body) => {
            let Some(value_ty) = infer_child(cx, incoming, value.key())? else {
                return Ok(None);
            };
            let body_scope = {
                let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
                scopes.scope(StlcScopeKey::Lexical(current.clone()))?
            };
            emit_binding_type(cx, current, value_ty)?;
            infer_child(cx, body_scope, body.key())
        }
        StlcExpr::Lambda(parameter, body) => {
            let Some(parameter_ty) = parameter_annotation(cx, incoming, *parameter)? else {
                emit_diagnostic(cx, current, StlcTypeError::MissingParameterAnnotation)?;
                return Ok(None);
            };
            let lambda_scope = {
                let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
                scopes.scope(StlcScopeKey::Lexical(current.clone()))?
            };
            emit_binding_type(cx, parameter.key(), parameter_ty.clone())?;
            let Some(body_ty) = infer_child(cx, lambda_scope, body.key())? else {
                return Ok(None);
            };
            Ok(Some(StlcTypeValue::Arrow(
                Box::new(parameter_ty),
                Box::new(body_ty),
            )))
        }
        StlcExpr::Variable(token) => infer_variable(cx, incoming, current, *token),
        StlcExpr::Error(_) => Ok(None),
    }
}

fn check_expression(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    current: AstKey,
    expression: Arc<StlcExpr>,
    expected: StlcTypeValue,
) -> Result<Option<StlcTypeValue>> {
    match expression.as_ref() {
        StlcExpr::Lambda(parameter, body) => {
            let StlcTypeValue::Arrow(domain, codomain) = expected.clone() else {
                emit_diagnostic(cx, current, StlcTypeError::ExpectedArrow { expected })?;
                return Ok(None);
            };
            if !parameter_matches(cx, incoming, *parameter, &domain)? {
                return Ok(None);
            }
            let lambda_scope = {
                let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
                scopes.scope(StlcScopeKey::Lexical(current.clone()))?
            };
            emit_binding_type(cx, parameter.key(), (*domain).clone())?;
            let Some(_) = check_child(cx, lambda_scope, body.key(), (*codomain).clone())? else {
                return Ok(None);
            };
            Ok(Some(StlcTypeValue::Arrow(domain, codomain)))
        }
        StlcExpr::If(condition, then_branch, else_branch) => {
            let condition = check_child(cx, incoming, condition.key(), StlcTypeValue::Bool)?;
            let then_branch = check_child(cx, incoming, then_branch.key(), expected.clone())?;
            let else_branch = check_child(cx, incoming, else_branch.key(), expected.clone())?;
            Ok(
                (condition.is_some() && then_branch.is_some() && else_branch.is_some())
                    .then_some(expected),
            )
        }
        StlcExpr::Case(scrutinee, zero_branch, _, successor_branch) => {
            let scrutinized = check_child(cx, incoming, scrutinee.key(), StlcTypeValue::Nat)?;
            let successor_scope = {
                let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
                scopes.scope(StlcScopeKey::CaseSuccessor(current.clone()))?
            };
            emit_binding_type(cx, current.clone(), StlcTypeValue::Nat)?;
            let zero_branch = check_child(cx, incoming, zero_branch.key(), expected.clone())?;
            let successor_branch = check_child(
                cx,
                successor_scope,
                successor_branch.key(),
                expected.clone(),
            )?;
            Ok(
                (scrutinized.is_some() && zero_branch.is_some() && successor_branch.is_some())
                    .then_some(expected),
            )
        }
        StlcExpr::Let(_, value, body) => {
            let Some(value_ty) = infer_child(cx, incoming, value.key())? else {
                return Ok(None);
            };
            let body_scope = {
                let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
                scopes.scope(StlcScopeKey::Lexical(current.clone()))?
            };
            emit_binding_type(cx, current, value_ty)?;
            let Some(_) = check_child(cx, body_scope, body.key(), expected.clone())? else {
                return Ok(None);
            };
            Ok(Some(expected))
        }
        StlcExpr::Group(inner) => check_child(cx, incoming, inner.key(), expected),
        _ => Ok(infer_expression(cx, incoming, current.clone(), expression)?
            .and_then(|found| check_inferred(cx, current, expected, found))),
    }
}

fn infer_variable(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    current: AstKey,
    token: plingo::component::parse::AstToken<StlcToken>,
) -> Result<Option<StlcTypeValue>> {
    let name = {
        let mut parsed = cx.view::<plingo::component::Parsed<StlcToken, StlcDocument>>();
        parsed
            .token_text(current.uri, token)
            .ok_or_else(|| NodeError::message("missing variable token text"))?
    };
    let filter_name = Arc::clone(&name);
    cx.view::<plingo::component::Scope<StlcScope>>()
        .query_from(incoming)
        .along(scope_path!(
            StlcScopeLabel::Lexical * StlcScopeLabel::Declaration
        ))
        .filter(move |data| {
            matches!(
                data,
                StlcScopeData::Declaration { name: binding, .. }
                    if binding.as_ref() == filter_name.as_ref()
            )
        })
        .visible_under(
            PathOrder::new().prefer(StlcScopeLabel::Declaration, StlcScopeLabel::Lexical),
        )
        .with_context((current, name))
        .on_shadowed(|cx, (current, name), _, _| {
            emit_diagnostic(
                cx,
                current.clone(),
                StlcTypeError::ShadowedVariable {
                    name: Arc::clone(name),
                },
            )?;
            Ok(())
        })
        .on_missing(|cx, (current, name)| {
            emit_diagnostic(cx, current, StlcTypeError::UnboundVariable { name })?;
            Ok(None)
        })
        .on_unique(|cx, (_, _), resolution| {
            let StlcScopeData::Declaration { definition, .. } = resolution.data() else {
                return Ok(None);
            };
            binding_type_at(cx, &definition)
        })
        .on_ambiguous(|cx, (current, name), candidates| {
            emit_diagnostic(
                cx,
                current,
                StlcTypeError::AmbiguousVariable { name, candidates },
            )?;
            Ok(None)
        })
        .resolve()
}

fn agree_branches(
    cx: &mut TypeCx<'_>,
    current: AstKey,
    prerequisite: Option<StlcTypeValue>,
    left: Option<StlcTypeValue>,
    right: Option<StlcTypeValue>,
) -> Result<Option<StlcTypeValue>> {
    if prerequisite.is_none() {
        return Ok(None);
    }
    match (left, right) {
        (Some(left), Some(right)) if left == right => Ok(Some(left)),
        (Some(then_ty), Some(else_ty)) => {
            emit_diagnostic(
                cx,
                current,
                StlcTypeError::BranchMismatch { then_ty, else_ty },
            )?;
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn check_inferred(
    cx: &mut TypeCx<'_>,
    current: AstKey,
    expected: StlcTypeValue,
    found: StlcTypeValue,
) -> Option<StlcTypeValue> {
    if found == expected {
        Some(expected)
    } else {
        emit_diagnostic(cx, current, StlcTypeError::Mismatch { expected, found })
            .expect("diagnostic emission cannot fail");
        None
    }
}

fn infer_type_atom(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    atom: Arc<StlcTypeAtom>,
) -> Result<Option<StlcTypeValue>> {
    match atom.as_ref() {
        StlcTypeAtom::Nat(_) => Ok(Some(StlcTypeValue::Nat)),
        StlcTypeAtom::Bool(_) => Ok(Some(StlcTypeValue::Bool)),
        StlcTypeAtom::Unit(_) => Ok(Some(StlcTypeValue::Unit)),
        StlcTypeAtom::Parenthesized(inner) => infer_child(cx, incoming, inner.key()),
    }
}

fn infer_type(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    syntax: Arc<StlcType>,
) -> Result<Option<StlcTypeValue>> {
    match syntax.as_ref() {
        StlcType::Arrow(domain, codomain) => {
            let Some(domain) = infer_child(cx, incoming, domain.key())? else {
                return Ok(None);
            };
            let Some(codomain) = infer_child(cx, incoming, codomain.key())? else {
                return Ok(None);
            };
            Ok(Some(StlcTypeValue::Arrow(
                Box::new(domain),
                Box::new(codomain),
            )))
        }
        StlcType::Atom(atom) => infer_child(cx, incoming, atom.key()),
        StlcType::Error(_) => Ok(None),
    }
}

fn parameter_annotation(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    parameter: AstBox<StlcParam>,
) -> Result<Option<StlcTypeValue>> {
    let value = {
        let mut parsed = cx.view::<plingo::component::Parsed<StlcToken, StlcDocument>>();
        parsed
            .artifact::<StlcParam>(parameter.key())
            .ok_or_else(|| NodeError::message("missing parameter AST"))?
    };
    let annotation = match value.as_ref() {
        StlcParam::Bare(_, annotation) | StlcParam::Parenthesized(_, annotation) => annotation,
    };
    match annotation {
        Some(syntax) => infer_child(cx, incoming, syntax.key()),
        None => Ok(None),
    }
}

fn parameter_matches(
    cx: &mut TypeCx<'_>,
    incoming: ScopeId<StlcScope>,
    parameter: AstBox<StlcParam>,
    expected: &StlcTypeValue,
) -> Result<bool> {
    let Some(found) = parameter_annotation(cx, incoming, parameter)? else {
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
    )?;
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

fn binding_type_at(cx: &mut TypeCx<'_>, definition: &AstKey) -> Result<Option<StlcTypeValue>> {
    let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
    let Some(type_scope) = scopes.find_scope(StlcScopeKey::Type(definition.clone())) else {
        return Ok(None);
    };
    Ok(match scopes.data(type_scope) {
        Some(data) => match data.as_ref() {
            StlcScopeData::Type(ty) => Some(ty.clone()),
            _ => None,
        },
        None => None,
    })
}

fn emit_binding_type(cx: &mut TypeCx<'_>, definition: AstKey, ty: StlcTypeValue) -> Result<()> {
    let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
    let declaration = scopes.scope(StlcScopeKey::Declaration(definition.clone()))?;
    scopes.declare_linked(
        StlcScopeKey::Type(definition),
        StlcScopeData::Type(ty),
        declaration,
        StlcScopeLabel::Type,
        ScopeProperty::Acyclic,
    )?;
    Ok(())
}

fn emit_diagnostic(cx: &mut TypeCx<'_>, expression: AstKey, error: StlcTypeError) -> Result<()> {
    cx.view::<plingo::component::Diagnostics<StlcTypeDiagnostic>>()
        .add(StlcTypeDiagnostic { expression, error })
}

pub fn install_type_components(
    graph: &mut plingo::scheme::node::Graph,
) -> std::result::Result<(), NodeError> {
    graph.register::<TypeDocument>()?;
    graph.register::<TypeOf>()?;
    Ok(())
}
