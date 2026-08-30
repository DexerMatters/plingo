//! Incremental STLC type checking authored against generated abstract-tree views.

use std::sync::Arc;

use plingo::prelude::*;

use super::name_resolve::{
    Scope, StlcDeclarationScopes, StlcResolution, StlcResolvedReferences, StlcScope,
};
use super::syntax::{
    StlcDeclaration, StlcDeclarationView, StlcDocument, StlcExpr, StlcExprView, StlcParam,
    StlcParamView, StlcPath, StlcType, StlcTypeAtom, StlcTypeAtomView, StlcTypeView,
};

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
    where
        I: IntoIterator<Item = StlcTypeValue>,
    {
        let mut parameters: Vec<_> = parameters.into_iter().collect();
        let result = match result {
            StlcTypeValue::Function(function) => {
                parameters.extend(function.parameters.iter().cloned());
                function.result.clone()
            }
            result => result,
        };
        if parameters.is_empty() {
            result
        } else {
            StlcTypeValue::Function(Arc::new(FunctionType {
                parameters: Arc::from(parameters),
                result,
            }))
        }
    }

    pub fn apply_one(&self) -> Option<StlcTypeValue> {
        let StlcTypeValue::Function(function) = self else {
            return None;
        };
        if function.parameters.len() == 1 {
            Some(function.result.clone())
        } else {
            Some(StlcTypeValue::Function(Arc::new(FunctionType {
                parameters: Arc::from(&function.parameters[1..]),
                result: function.result.clone(),
            })))
        }
    }

    pub fn function_parts(&self) -> Option<(Vec<StlcTypeValue>, StlcTypeValue)> {
        match self {
            StlcTypeValue::Function(function) => {
                Some((function.parameters.to_vec(), function.result.clone()))
            }
            _ => None,
        }
    }
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
    MissingParameterAnnotation,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcTypeResult {
    Known(StlcTypeValue),
    Unknown,
}

impl From<Option<StlcTypeValue>> for StlcTypeResult {
    fn from(value: Option<StlcTypeValue>) -> Self {
        value.map_or(Self::Unknown, Self::Known)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StlcTypeDiagnostic {
    pub expression: AstBox<()>,
    pub error: StlcTypeError,
}

#[view]
pub struct StlcSynthesizedTypes(Map<AstBox<()>, StlcTypeResult>);

#[view]
pub struct StlcExpectedTypes(Map<(AstBox<()>, AstBox<()>), StlcTypeValue>);

#[view]
pub struct StlcDefinitionTypes(Map<Scope<StlcScope>, StlcTypeResult>);

#[view]
pub struct StlcTypeDiagnostics(List<AstBox<()>, StlcTypeDiagnostic>);

#[derive(Clone, Debug, PartialEq, Effects)]
struct SynthesisEffects {
    synthesized: Set<StlcSynthesizedTypes>,
    expected: Vec<Set<StlcExpectedTypes>>,
    expected_removed: Vec<Remove<StlcExpectedTypes>>,
    diagnostics: Replace<StlcTypeDiagnostics>,
}

fn child_type(id: &AstBox<()>) -> Result<Option<StlcTypeValue>> {
    Ok(
        StlcSynthesizedTypes::get(id)?.and_then(|result| match result.as_ref() {
            StlcTypeResult::Known(value) => Some(value.clone()),
            StlcTypeResult::Unknown => None,
        }),
    )
}

fn expected<T: AbstractTreeNode>(node: &AstBox<T>) -> Result<Option<StlcTypeValue>> {
    let Some(parent) = node.parent()? else {
        return Ok(None);
    };
    Ok(StlcExpectedTypes::get(&(parent.erased(), node.erased()))?
        .map(|value| value.as_ref().clone()))
}
fn expectation(
    effects: &mut SynthesisEffects,
    parent: AstBox<()>,
    child: AstBox<()>,
    value: Option<StlcTypeValue>,
) {
    match value {
        Some(value) => effects
            .expected
            .push(StlcExpectedTypes::set((parent, child), value)),
        None => effects
            .expected_removed
            .push(StlcExpectedTypes::remove((parent, child))),
    }
}

fn result(
    id: AstBox<()>,
    value: Option<StlcTypeValue>,
    diagnostics: Vec<StlcTypeDiagnostic>,
) -> SynthesisEffects {
    SynthesisEffects {
        synthesized: StlcSynthesizedTypes::set(id.clone(), value.clone().into()),
        expected: Vec::new(),
        expected_removed: Vec::new(),
        diagnostics: StlcTypeDiagnostics::replace(id, diagnostics),
    }
}

fn finish(
    node: AstBox<()>,
    value: Option<StlcTypeValue>,
    expected_value: Option<StlcTypeValue>,
    diagnostics: &mut Vec<StlcTypeDiagnostic>,
    mut effects: SynthesisEffects,
) -> SynthesisEffects {
    if let (Some(expected), Some(found)) = (expected_value, value.as_ref()) {
        if expected != *found {
            diagnostics.push(StlcTypeDiagnostic {
                expression: node.clone(),
                error: StlcTypeError::Mismatch {
                    expected,
                    found: found.clone(),
                },
            });
        }
    }
    effects.synthesized = StlcSynthesizedTypes::set(node.clone(), value.into());
    effects.diagnostics = StlcTypeDiagnostics::replace(node, diagnostics.clone());
    effects
}

fn synth_type_atom(node: AstBox<StlcTypeAtom>) -> Result<SynthesisEffects> {
    let id = node.erased();
    let value = match node.view()? {
        StlcTypeAtomView::Nat(_) => Some(StlcTypeValue::Nat),
        StlcTypeAtomView::Bool(_) => Some(StlcTypeValue::Bool),
        StlcTypeAtomView::Unit(_) => Some(StlcTypeValue::Unit),
        StlcTypeAtomView::Parenthesized(parenthesized) => {
            let child = parenthesized.ty()?;
            child_type(&child.erased())?
        }
    };
    Ok(finish(
        id.clone(),
        value.clone(),
        expected(&node)?,
        &mut Vec::new(),
        result(id, value, Vec::new()),
    ))
}

fn synth_type(node: AstBox<StlcType>) -> Result<SynthesisEffects> {
    let id = node.erased();
    let mut effects = result(id.clone(), None, Vec::new());
    let mut value = None;
    match node.view()? {
        StlcTypeView::Arrow(arrow) => {
            let left = arrow.left()?;
            let right = arrow.right()?;
            expectation(&mut effects, id.clone(), left.erased(), None);
            expectation(&mut effects, id.clone(), right.erased(), None);
            value = match (child_type(&left.erased())?, child_type(&right.erased())?) {
                (Some(left), Some(right)) => Some(StlcTypeValue::function([left], right)),
                _ => None,
            };
        }
        StlcTypeView::Atom(atom) => {
            let child = atom.atom()?;
            expectation(&mut effects, id.clone(), child.erased(), None);
            value = child_type(&child.erased())?;
        }
        StlcTypeView::Error(_) => {}
    }
    let expected_value = expected(&node)?;
    let mut diagnostics = Vec::new();
    Ok(finish(id, value, expected_value, &mut diagnostics, effects))
}

fn synth_param(node: AstBox<StlcParam>) -> Result<SynthesisEffects> {
    let id = node.erased();
    let mut effects = result(id.clone(), None, Vec::new());
    let annotation = match node.view()? {
        StlcParamView::Bare(param) => param.annotation()?,
        StlcParamView::Parenthesized(param) => param.annotation()?,
    };
    let value = if let Some(annotation) = annotation.as_ref() {
        expectation(&mut effects, id.clone(), annotation.erased(), None);
        child_type(&annotation.erased())?
    } else {
        None
    };
    let mut diagnostics = Vec::new();
    if value.is_none() && annotation.is_none() {
        diagnostics.push(StlcTypeDiagnostic {
            expression: id.clone(),
            error: StlcTypeError::MissingParameterAnnotation,
        });
    }
    Ok(finish(
        id,
        value,
        expected(&node)?,
        &mut diagnostics,
        effects,
    ))
}

fn synth_expr(node: AstBox<StlcExpr>) -> Result<SynthesisEffects> {
    let id = node.erased();
    let mut effects = result(id.clone(), None, Vec::new());
    let mut diagnostics = Vec::new();
    let mut value = None;
    match node.view()? {
        StlcExprView::True(_) | StlcExprView::False(_) => value = Some(StlcTypeValue::Bool),
        StlcExprView::Number(_) => value = Some(StlcTypeValue::Nat),
        StlcExprView::Unit(_) => value = Some(StlcTypeValue::Unit),
        StlcExprView::Group(group) => {
            let child = group.expression()?;
            let expected_value = expected(&node)?;
            expectation(&mut effects, id.clone(), child.erased(), expected_value);
            value = child_type(&child.erased())?;
        }
        StlcExprView::Succ(succ) => {
            let child = succ.value()?;
            expectation(
                &mut effects,
                id.clone(),
                child.erased(),
                Some(StlcTypeValue::Nat),
            );
            value = Some(StlcTypeValue::Nat);
        }
        StlcExprView::Add(add) => {
            let left = add.left()?;
            let right = add.right()?;
            expectation(
                &mut effects,
                id.clone(),
                left.erased(),
                Some(StlcTypeValue::Nat),
            );
            expectation(
                &mut effects,
                id.clone(),
                right.erased(),
                Some(StlcTypeValue::Nat),
            );
            value = Some(StlcTypeValue::Nat);
        }
        StlcExprView::If(if_) => {
            let condition = if_.condition()?;
            let when_true = if_.when_true()?;
            let when_false = if_.when_false()?;
            expectation(
                &mut effects,
                id.clone(),
                condition.erased(),
                Some(StlcTypeValue::Bool),
            );
            let parent_expected = expected(&node)?;
            expectation(
                &mut effects,
                id.clone(),
                when_true.erased(),
                parent_expected.clone(),
            );
            expectation(
                &mut effects,
                id.clone(),
                when_false.erased(),
                parent_expected,
            );
            match (
                child_type(&when_true.erased())?,
                child_type(&when_false.erased())?,
            ) {
                (Some(left), Some(right)) if left == right => value = Some(left),
                (Some(left), Some(right)) => diagnostics.push(StlcTypeDiagnostic {
                    expression: id.clone(),
                    error: StlcTypeError::BranchMismatch {
                        then_ty: left,
                        else_ty: right,
                    },
                }),
                _ => {}
            }
        }
        StlcExprView::Case(case) => {
            let scrutinee = case.scrutinee()?;
            let zero = case.zero_branch()?;
            let successor = case.successor_branch()?;
            expectation(
                &mut effects,
                id.clone(),
                scrutinee.erased(),
                Some(StlcTypeValue::Nat),
            );
            let parent_expected = expected(&node)?;
            expectation(
                &mut effects,
                id.clone(),
                zero.erased(),
                parent_expected.clone(),
            );
            expectation(
                &mut effects,
                id.clone(),
                successor.erased(),
                parent_expected,
            );
            match (
                child_type(&zero.erased())?,
                child_type(&successor.erased())?,
            ) {
                (Some(left), Some(right)) if left == right => value = Some(left),
                (Some(left), Some(right)) => diagnostics.push(StlcTypeDiagnostic {
                    expression: id.clone(),
                    error: StlcTypeError::BranchMismatch {
                        then_ty: left,
                        else_ty: right,
                    },
                }),
                _ => {}
            }
        }
        StlcExprView::Lambda(lambda) => {
            let parameter = lambda.parameter()?;
            let body = lambda.body()?;
            let parameter_expectation = expected(&node)?.and_then(|value| {
                value
                    .function_parts()
                    .and_then(|(parameters, _)| parameters.first().cloned())
            });
            let body_expectation = expected(&node)?.and_then(|value| {
                value.function_parts().map(|(mut parameters, result)| {
                    if parameters.len() <= 1 {
                        result
                    } else {
                        parameters.remove(0);
                        StlcTypeValue::function(parameters, result)
                    }
                })
            });
            expectation(
                &mut effects,
                id.clone(),
                parameter.erased(),
                parameter_expectation,
            );
            expectation(&mut effects, id.clone(), body.erased(), body_expectation);
            if let (Some(parameter), Some(body)) = (
                child_type(&parameter.erased())?,
                child_type(&body.erased())?,
            ) {
                value = Some(StlcTypeValue::function([parameter], body));
            }
        }
        StlcExprView::Apply(apply) => {
            let function = apply.function()?;
            let argument = apply.argument()?;
            expectation(&mut effects, id.clone(), function.erased(), None);
            let function_type = child_type(&function.erased())?;
            let argument_expected = function_type
                .as_ref()
                .and_then(StlcTypeValue::function_parts)
                .and_then(|(parameters, _)| parameters.into_iter().next());
            expectation(
                &mut effects,
                id.clone(),
                argument.erased(),
                argument_expected,
            );
            if let Some(function_type) = function_type {
                if function_type.function_parts().is_none() {
                    diagnostics.push(StlcTypeDiagnostic {
                        expression: id.clone(),
                        error: StlcTypeError::NonFunctionApplication {
                            found: function_type,
                        },
                    });
                } else {
                    value = function_type.apply_one();
                }
            }
        }
        StlcExprView::Let(let_) => {
            let bound = let_.value()?;
            let body = let_.body()?;
            expectation(&mut effects, id.clone(), bound.erased(), None);
            expectation(&mut effects, id.clone(), body.erased(), expected(&node)?);
            value = child_type(&body.erased())?;
        }
        StlcExprView::Variable(_) => {
            if let Some(reference) = StlcResolvedReferences::get(&id)? {
                match reference.as_ref() {
                    StlcResolution::Resolved { declaration } => {
                        value =
                            StlcDefinitionTypes::get(declaration)?.and_then(|value| {
                                match value.as_ref() {
                                    StlcTypeResult::Known(value) => Some(value.clone()),
                                    StlcTypeResult::Unknown => None,
                                }
                            });
                    }
                    StlcResolution::Unbound { name } => diagnostics.push(StlcTypeDiagnostic {
                        expression: id.clone(),
                        error: StlcTypeError::UnboundVariable { name: name.clone() },
                    }),
                }
            }
        }
        StlcExprView::Error(_) => {}
    }
    Ok(finish(
        id,
        value,
        expected(&node)?,
        &mut diagnostics,
        effects,
    ))
}

fn synth_declaration(node: AstBox<StlcDeclaration>) -> Result<SynthesisEffects> {
    let id = node.erased();
    let mut effects = result(id.clone(), None, Vec::new());
    let mut value = None;
    let diagnostics = Vec::new();
    if let StlcDeclarationView::Value(declaration) = node.view()? {
        let annotation = declaration.annotation()?;
        if let Some(annotation) = &annotation {
            expectation(&mut effects, id.clone(), annotation.erased(), None);
        }
        let parameters = declaration.parameters()?;
        for parameter in parameters.iter() {
            expectation(&mut effects, id.clone(), parameter.erased(), None);
        }
        let body = declaration.body()?;
        let body_expected = annotation
            .as_ref()
            .and_then(|annotation| child_type(&annotation.erased()).ok().flatten());
        expectation(&mut effects, id.clone(), body.erased(), body_expected);
        value = child_type(&body.erased())?;
        let parameters = parameters.to_vec();
        for parameter in parameters.iter().rev() {
            value = match (child_type(&parameter.erased())?, value) {
                (Some(parameter), Some(value)) => Some(StlcTypeValue::function([parameter], value)),
                _ => None,
            };
        }
        if let (Some(annotation), Some(found)) = (
            annotation.and_then(|annotation| child_type(&annotation.erased()).ok().flatten()),
            value.as_ref(),
        ) {
            if annotation != *found {
                // The declaration itself is the most stable diagnostic anchor.
                let mut diagnostics = diagnostics;
                diagnostics.push(StlcTypeDiagnostic {
                    expression: id.clone(),
                    error: StlcTypeError::Mismatch {
                        expected: annotation,
                        found: found.clone(),
                    },
                });
                return Ok(finish(
                    id,
                    None,
                    expected(&node)?,
                    &mut diagnostics,
                    effects,
                ));
            }
        }
    }
    Ok(finish(
        id,
        value,
        expected(&node)?,
        &mut diagnostics.clone(),
        effects,
    ))
}

fn publish_definition<T: AbstractTreeNode>(
    node: AstBox<T>,
    binder: bool,
) -> Result<Option<Set<StlcDefinitionTypes>>> {
    if !binder {
        return Ok(None);
    }
    let Some(scope) = StlcDeclarationScopes::get(&node.erased())? else {
        return Ok(None);
    };
    let value = child_type(&node.erased())?.into();
    Ok(Some(StlcDefinitionTypes::set(
        scope.as_ref().clone(),
        value,
    )))
}

#[component]
pub fn synthesize_expr(node: AstBox<StlcExpr>) -> Result<SynthesisEffects> {
    synth_expr(node)
}

#[component]
pub fn synthesize_type(node: AstBox<StlcType>) -> Result<SynthesisEffects> {
    synth_type(node)
}

#[component]
pub fn synthesize_type_atom(node: AstBox<StlcTypeAtom>) -> Result<SynthesisEffects> {
    synth_type_atom(node)
}

#[component]
pub fn synthesize_param(node: AstBox<StlcParam>) -> Result<SynthesisEffects> {
    synth_param(node)
}

#[component]
pub fn synthesize_declaration(node: AstBox<StlcDeclaration>) -> Result<SynthesisEffects> {
    synth_declaration(node)
}

#[component]
pub fn publish_expr(node: AstBox<StlcExpr>) -> Result<Option<Set<StlcDefinitionTypes>>> {
    let binder = matches!(node.view()?, StlcExprView::Let(_));
    publish_definition(node, binder)
}

#[component]
pub fn publish_param(node: AstBox<StlcParam>) -> Result<Option<Set<StlcDefinitionTypes>>> {
    publish_definition(node, true)
}

#[component]
pub fn publish_declaration(
    node: AstBox<StlcDeclaration>,
) -> Result<Option<Set<StlcDefinitionTypes>>> {
    let binder = matches!(node.view()?, StlcDeclarationView::Value(_));
    publish_definition(node, binder)
}
