//! Incremental lexical name resolution for the STLC syntax family.
//!
//! The resolver consumes typed generated tree views and returns typed map and
//! graph effects.  Scope identities are allocated by the active component;
//! application code never supplies an identity seed or graph node ordinal.

use std::sync::Arc;

use plingo::framework::lex::observe_token;
use plingo::framework::parse::AstToken;
pub use plingo::framework::scope::Scope;
use plingo::framework::scope::{ScopeGraph, ScopeNode, outgoing};
use plingo::prelude::*;

use super::syntax::{
    StlcDeclaration, StlcDeclarationView, StlcDocument, StlcDocumentView, StlcExpr, StlcExprView,
    StlcParam, StlcParamView, StlcPath, StlcToken, StlcType, StlcTypeAtom, StlcTypeAtomView,
    StlcTypeView,
};

// ---------------------------------------------------------------------------
// Domain and semantic views
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcScopeLabel {
    Lexical,
    Declaration(Arc<str>),
    Import(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcScopeData {
    Document,
    Lexical,
    Declaration { name: Arc<str> },
    CaseSuccessor,
    External { path: Arc<str> },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, ScopeDomain)]
#[scope_domain(scope_data = StlcScopeData, label = StlcScopeLabel, request = Arc<str>)]
pub struct StlcScope;

#[view]
pub struct StlcRequirements(Map<String, Vec<Arc<str>>>);

#[view]
pub struct StlcDocumentScopes(Map<String, Scope<StlcScope>>);

#[view]
pub struct StlcRootScopes(Map<AstBox<()>, Scope<StlcScope>>);

#[view]
pub struct StlcIncomingScopes(Map<AstBox<()>, Scope<StlcScope>>);

#[view]
pub struct StlcContinuationScopes(Map<AstBox<()>, Scope<StlcScope>>);

#[view]
pub struct StlcDeclarationScopes(Map<AstBox<()>, Scope<StlcScope>>);

#[view]
pub struct StlcReferenceCandidates(Map<AstBox<()>, ()>);

#[view]
pub struct StlcResolvedReferences(Map<AstBox<()>, StlcResolution>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcResolution {
    Resolved { declaration: Scope<StlcScope> },
    Unbound { name: Arc<str> },
}

#[derive(Clone, Debug, PartialEq, Effects)]
struct NameEffects {
    incoming: Vec<Set<StlcIncomingScopes>>,
    roots: Vec<Set<StlcRootScopes>>,
    continuations: Vec<Set<StlcContinuationScopes>>,
    declarations: Vec<Set<StlcDeclarationScopes>>,
    candidates: Vec<Set<StlcReferenceCandidates>>,
    graph: Vec<GraphRender<ScopeGraph<StlcScope>>>,
}

impl Default for NameEffects {
    fn default() -> Self {
        Self {
            incoming: Vec::new(),
            roots: Vec::new(),
            continuations: Vec::new(),
            declarations: Vec::new(),
            candidates: Vec::new(),
            graph: Vec::new(),
        }
    }
}

fn incoming_set(node: AstBox<()>, scope: Scope<StlcScope>) -> Set<StlcIncomingScopes> {
    StlcIncomingScopes::set(node, scope)
}

fn root_set(node: AstBox<()>, scope: Scope<StlcScope>) -> Set<StlcRootScopes> {
    StlcRootScopes::set(node, scope)
}

fn continuation_set(node: AstBox<()>, scope: Scope<StlcScope>) -> Set<StlcContinuationScopes> {
    StlcContinuationScopes::set(node, scope)
}

fn declaration_set(node: AstBox<()>, scope: Scope<StlcScope>) -> Set<StlcDeclarationScopes> {
    StlcDeclarationScopes::set(node, scope)
}

fn candidate_set(node: AstBox<()>) -> Set<StlcReferenceCandidates> {
    StlcReferenceCandidates::set(node, ())
}

fn current_scope(node: AstBox<()>) -> Result<Option<Scope<StlcScope>>> {
    Ok(StlcIncomingScopes::get(&node)?.map(|scope| scope.as_ref().clone()))
}

fn add_incoming(effects: &mut NameEffects, child: AstBox<()>, scope: &Scope<StlcScope>) {
    effects.incoming.push(incoming_set(child, scope.clone()));
}

fn token_text(token: AstToken<StlcToken>) -> Result<Option<Arc<str>>> {
    let Some(value) = observe_token::<StlcToken>(token)? else {
        return Ok(None);
    };
    Ok(match value.as_ref() {
        StlcToken::Ident(text) if !text.is_empty() => Some(Arc::from(text.as_str())),
        _ => None,
    })
}

fn name_of(token: &Arc<AstToken<StlcToken>>) -> Result<Option<Arc<str>>> {
    token_text(**token)
}

fn declaration_render(
    scope: Scope<StlcScope>,
    name: Option<Arc<str>>,
    lexical_parent: &Scope<StlcScope>,
) -> GraphRender<ScopeGraph<StlcScope>> {
    scope
        .render(ScopeNode::Declaration(StlcScopeData::Declaration {
            name: name.unwrap_or_default(),
        }))
        .bucket(StlcScopeLabel::Lexical, [lexical_parent.node()])
}

// ---------------------------------------------------------------------------
// One semantic component per generated tree member
// ---------------------------------------------------------------------------

#[component]
pub fn name_document(node: AstBox<StlcDocument>) -> Result<NameEffects> {
    let id = node.erased();
    let document = Scope::<StlcScope>::automatic()?;
    let mut effects = NameEffects::default();
    effects.graph.push(
        document
            .clone()
            .render(ScopeNode::Scope(StlcScopeData::Document)),
    );
    effects.roots.push(root_set(id.clone(), document.clone()));
    effects.incoming.push(incoming_set(id, document.clone()));

    let StlcDocumentView::Lines(lines) = node.view()? else {
        return Ok(effects);
    };
    let mut current = document;
    for declaration in lines.declarations()?.iter() {
        let child = declaration.erased();
        add_incoming(&mut effects, child, &current);
        current = StlcContinuationScopes::get(&declaration.erased())?
            .map(|scope| scope.as_ref().clone())
            .unwrap_or(current);
    }
    Ok(effects)
}

#[component]
pub fn name_declaration(node: AstBox<StlcDeclaration>) -> Result<NameEffects> {
    let id = node.erased();
    let Some(incoming) = current_scope(id.clone())? else {
        return Ok(NameEffects::default());
    };
    let mut effects = NameEffects::default();
    let StlcDeclarationView::Value(value) = node.view()? else {
        match node.view()? {
            StlcDeclarationView::Import(import) => {
                if let Ok(path) = import.path() {
                    add_incoming(&mut effects, path.erased(), &incoming);
                }
            }
            StlcDeclarationView::Export(export) => {
                if let Ok(path) = export.path() {
                    add_incoming(&mut effects, path.erased(), &incoming);
                }
            }
            StlcDeclarationView::Error(_) | StlcDeclarationView::Value(_) => {}
        }
        return Ok(effects);
    };

    let name = name_of(&value.name()?)?;
    let declaration = Scope::<StlcScope>::automatic()?;
    effects.graph.push(declaration_render(
        declaration.clone(),
        name.clone(),
        &incoming,
    ));
    effects
        .declarations
        .push(declaration_set(id.clone(), declaration.clone()));
    effects
        .continuations
        .push(continuation_set(id.clone(), declaration.clone()));
    if let Some(name) = name {
        effects.graph.push(
            declaration
                .clone()
                .patch()
                .bucket(StlcScopeLabel::Declaration(name), [declaration.node()]),
        );
    }

    if let Some(annotation) = value.annotation()? {
        add_incoming(&mut effects, annotation.erased(), &incoming);
    }
    let mut parameter_scope = declaration;
    for parameter in value.parameters()?.iter() {
        add_incoming(&mut effects, parameter.erased(), &parameter_scope);
        parameter_scope = StlcContinuationScopes::get(&parameter.erased())?
            .map(|scope| scope.as_ref().clone())
            .unwrap_or(parameter_scope);
    }
    add_incoming(&mut effects, value.body()?.erased(), &parameter_scope);
    Ok(effects)
}

#[component]
pub fn name_path(node: AstBox<StlcPath>) -> Result<NameEffects> {
    let _ = node.view()?;
    Ok(NameEffects::default())
}

#[component]
pub fn name_param(node: AstBox<StlcParam>) -> Result<NameEffects> {
    let id = node.erased();
    let Some(incoming) = current_scope(id.clone())? else {
        return Ok(NameEffects::default());
    };
    let (name, annotation) = match node.view()? {
        StlcParamView::Bare(param) => (param.name()?, param.annotation()?),
        StlcParamView::Parenthesized(param) => (param.name()?, param.annotation()?),
    };
    let declaration = Scope::<StlcScope>::automatic()?;
    let mut effects = NameEffects::default();
    effects.graph.push(declaration_render(
        declaration.clone(),
        name_of(&name)?,
        &incoming,
    ));
    effects
        .declarations
        .push(declaration_set(id.clone(), declaration.clone()));
    effects
        .continuations
        .push(continuation_set(id.clone(), declaration.clone()));
    if let Some(name) = name_of(&name)? {
        effects.graph.push(
            declaration
                .clone()
                .patch()
                .bucket(StlcScopeLabel::Declaration(name), [declaration.node()]),
        );
    }
    if let Some(annotation) = annotation {
        add_incoming(&mut effects, annotation.erased(), &incoming);
    }
    Ok(effects)
}

#[component]
pub fn name_type(node: AstBox<StlcType>) -> Result<NameEffects> {
    let id = node.erased();
    let Some(incoming) = current_scope(id)? else {
        return Ok(NameEffects::default());
    };
    let mut effects = NameEffects::default();
    match node.view()? {
        StlcTypeView::Arrow(arrow) => {
            add_incoming(&mut effects, arrow.left()?.erased(), &incoming);
            add_incoming(&mut effects, arrow.right()?.erased(), &incoming);
        }
        StlcTypeView::Atom(atom) => add_incoming(&mut effects, atom.atom()?.erased(), &incoming),
        StlcTypeView::Error(_) => {}
    }
    Ok(effects)
}

#[component]
pub fn name_type_atom(node: AstBox<StlcTypeAtom>) -> Result<NameEffects> {
    let id = node.erased();
    let Some(incoming) = current_scope(id)? else {
        return Ok(NameEffects::default());
    };
    let mut effects = NameEffects::default();
    if let StlcTypeAtomView::Parenthesized(parenthesized) = node.view()? {
        add_incoming(&mut effects, parenthesized.ty()?.erased(), &incoming);
    }
    Ok(effects)
}

#[component]
pub fn name_expr(node: AstBox<StlcExpr>) -> Result<NameEffects> {
    let id = node.erased();
    let Some(incoming) = current_scope(id.clone())? else {
        return Ok(NameEffects::default());
    };
    let mut effects = NameEffects::default();
    match node.view()? {
        StlcExprView::If(if_) => {
            add_incoming(&mut effects, if_.condition()?.erased(), &incoming);
            add_incoming(&mut effects, if_.when_true()?.erased(), &incoming);
            add_incoming(&mut effects, if_.when_false()?.erased(), &incoming);
        }
        StlcExprView::Case(case) => {
            add_incoming(&mut effects, case.scrutinee()?.erased(), &incoming);
            add_incoming(&mut effects, case.zero_branch()?.erased(), &incoming);
            let successor = Scope::<StlcScope>::automatic()?;
            effects.graph.push(declaration_render(
                successor.clone(),
                name_of(&case.binder()?)?,
                &incoming,
            ));
            effects
                .continuations
                .push(continuation_set(id.clone(), successor.clone()));
            effects
                .declarations
                .push(declaration_set(id.clone(), successor.clone()));
            if let Some(name) = name_of(&case.binder()?)? {
                effects.graph.push(
                    successor
                        .clone()
                        .patch()
                        .bucket(StlcScopeLabel::Declaration(name), [successor.node()]),
                );
            }
            add_incoming(&mut effects, case.successor_branch()?.erased(), &successor);
        }
        StlcExprView::Let(let_) => {
            let bound = let_.value()?;
            add_incoming(&mut effects, bound.erased(), &incoming);
            let continuation = Scope::<StlcScope>::automatic()?;
            effects.graph.push(declaration_render(
                continuation.clone(),
                name_of(&let_.name()?)?,
                &incoming,
            ));
            effects
                .continuations
                .push(continuation_set(id.clone(), continuation.clone()));
            effects
                .declarations
                .push(declaration_set(id.clone(), continuation.clone()));
            if let Some(name) = name_of(&let_.name()?)? {
                effects.graph.push(
                    continuation
                        .clone()
                        .patch()
                        .bucket(StlcScopeLabel::Declaration(name), [continuation.node()]),
                );
            }
            add_incoming(&mut effects, let_.body()?.erased(), &continuation);
        }
        StlcExprView::Lambda(lambda) => {
            let parameter = lambda.parameter()?;
            add_incoming(&mut effects, parameter.erased(), &incoming);
            let body_scope = StlcContinuationScopes::get(&parameter.erased())?
                .map(|scope| scope.as_ref().clone())
                .unwrap_or(incoming);
            add_incoming(&mut effects, lambda.body()?.erased(), &body_scope);
        }
        StlcExprView::Add(add) => {
            add_incoming(&mut effects, add.left()?.erased(), &incoming);
            add_incoming(&mut effects, add.right()?.erased(), &incoming);
        }
        StlcExprView::Apply(apply) => {
            add_incoming(&mut effects, apply.function()?.erased(), &incoming);
            add_incoming(&mut effects, apply.argument()?.erased(), &incoming);
        }
        StlcExprView::Succ(succ) => add_incoming(&mut effects, succ.value()?.erased(), &incoming),
        StlcExprView::Group(group) => {
            add_incoming(&mut effects, group.expression()?.erased(), &incoming)
        }
        StlcExprView::Variable(_) => effects.candidates.push(candidate_set(id)),
        StlcExprView::True(_)
        | StlcExprView::False(_)
        | StlcExprView::Number(_)
        | StlcExprView::Unit(_)
        | StlcExprView::Error(_) => {}
    }
    Ok(effects)
}

// ---------------------------------------------------------------------------
// Reference resolution
// ---------------------------------------------------------------------------

fn resolve_variable(node: AstBox<StlcExpr>) -> Result<Option<Set<StlcResolvedReferences>>> {
    let id = node.erased();
    let StlcExprView::Variable(variable) = node.view()? else {
        return Ok(None);
    };
    let Some(scope) = current_scope(id.clone())? else {
        return Ok(None);
    };
    let Some(value) = observe_token::<StlcToken>(*variable.token()?.as_ref())? else {
        return Ok(Some(StlcResolvedReferences::set(
            id,
            StlcResolution::Unbound {
                name: Arc::from(""),
            },
        )));
    };
    let StlcToken::Ident(text) = value.as_ref() else {
        return Ok(Some(StlcResolvedReferences::set(
            id,
            StlcResolution::Unbound {
                name: Arc::from(""),
            },
        )));
    };
    let name: Arc<str> = Arc::from(text.as_str());
    let label = StlcScopeLabel::Declaration(name.clone());
    let mut current = scope;
    let mut seen = std::collections::HashSet::new();
    while seen.insert(current.clone()) {
        if let Some(declaration) = outgoing(current.clone(), &label)?.first().cloned() {
            return Ok(Some(StlcResolvedReferences::set(
                id,
                StlcResolution::Resolved { declaration },
            )));
        }
        let Some(parent) = outgoing(current.clone(), &StlcScopeLabel::Lexical)?
            .first()
            .cloned()
        else {
            break;
        };
        current = parent;
    }
    Ok(Some(StlcResolvedReferences::set(
        id,
        StlcResolution::Unbound { name },
    )))
}

#[component]
pub fn resolve_expr(node: AstBox<StlcExpr>) -> Result<Option<Set<StlcResolvedReferences>>> {
    resolve_variable(node)
}
