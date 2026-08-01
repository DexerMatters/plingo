//! Incremental lexical name resolution for the STLC example.

use std::sync::Arc;

use super::syntax::*;
use plingo::component::{
    parse::{AstKey, data::AstBox},
    scope::{ScopeId, ScopeProperty},
    semantic::{Elaboration, ElaboratorCx, ElaboratorError, NoDiagnostic},
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcScopeLabel {
    Lexical,
    Declaration,
    Type,
    Import,
}

/// Domain-owned identity for every semantic STLC scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcScopeKey {
    Document(fluent_uri::Uri<&'static str>),
    Lexical(AstKey),
    Declaration(AstKey),
    Type(AstKey),
    CaseSuccessor(AstKey),
    External(Arc<str>),
}

/// The one datum mapped to an STLC semantic scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcScopeData {
    Document,
    Lexical,
    Declaration { name: Arc<str>, definition: AstKey },
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
    ExpectedArrow {
        expected: StlcTypeValue,
    },
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
    AmbiguousVariable {
        name: Arc<str>,
        candidates: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, plingo::component::scope::ScopeDomain)]
#[scope_domain(root = StlcToken, ast = StlcDocument, scope_key = StlcScopeKey, scope_data = StlcScopeData, label = StlcScopeLabel, request = Arc<str>)]
pub struct StlcScope;

#[derive(plingo::ElaboratorRole)]
#[elaborator(domain = StlcScope)]
pub struct StlcNames;

type NameCx<'a, 'transaction, 'nodes> = ElaboratorCx<'a, 'transaction, 'nodes, StlcNames>;
type NameError = ElaboratorError<NoDiagnostic>;

fn parameter_name(
    here: &mut NameCx<'_, '_, '_>,
    parameter: AstBox<StlcParam>,
) -> Result<Arc<str>, NameError> {
    let parameter = here.ast(parameter)?;
    match parameter.as_ref() {
        StlcParam::Bare(name, _) | StlcParam::Parenthesized(name, _) => here.text(*name),
    }
}

fn declare(
    here: &mut NameCx<'_, '_, '_>,
    environment: ScopeId<StlcScope>,
    name: Arc<str>,
    definition: AstKey,
) -> Result<ScopeId<StlcScope>, NameError> {
    let declaration = here.declare(
        StlcScopeKey::Declaration(definition.clone()),
        StlcScopeData::Declaration { name, definition },
    )?;
    here.edge(
        environment,
        StlcScopeLabel::Declaration,
        declaration,
        ScopeProperty::Acyclic,
    )?;
    here.seal(declaration)?;
    Ok(declaration)
}

fn lexical_scope(
    here: &mut NameCx<'_, '_, '_>,
    key: AstKey,
    incoming: ScopeId<StlcScope>,
) -> Result<ScopeId<StlcScope>, NameError> {
    let scope = here.declare(StlcScopeKey::Lexical(key), StlcScopeData::Lexical)?;
    here.edge(
        scope,
        StlcScopeLabel::Lexical,
        incoming,
        ScopeProperty::Acyclic,
    )?;
    Ok(scope)
}

fn elaborate_document(
    here: &mut NameCx<'_, '_, '_>,
    document: Arc<StlcDocument>,
) -> Result<(), NameError> {
    let uri = here.ast_key().uri;
    let document_scope = here.declare_root(StlcScopeKey::Document(uri), StlcScopeData::Document)?;
    let root_ast = here.ast_key();
    let body_scope = lexical_scope(here, root_ast, document_scope)?;
    if let StlcDocument::Lines(declarations) = document.as_ref() {
        for declaration in declarations {
            here.schedule(*declaration, body_scope, ())?;
        }
    }
    here.seal(body_scope)?;
    here.seal(document_scope)?;
    Ok(())
}

fn elaborate_declaration(
    here: &mut NameCx<'_, '_, '_>,
    declaration: Arc<StlcDeclaration>,
) -> Result<(), NameError> {
    let incoming = here.incoming_scope();
    let current = here.ast_key();
    match declaration.as_ref() {
        StlcDeclaration::Value(name, parameters, _, body) => {
            let body_scope = lexical_scope(here, current.clone(), incoming)?;
            let declaration_name = here.text(*name)?;
            declare(here, body_scope, declaration_name, current.clone())?;
            for parameter in parameters {
                let name = parameter_name(here, *parameter)?;
                declare(here, body_scope, name, parameter.key())?;
            }
            here.schedule(*body, body_scope, ())?;
            here.seal(body_scope)?;
        }
        StlcDeclaration::Import(path) => {
            let path = path_text(here, *path)?;
            let target = here.declare(
                StlcScopeKey::External(Arc::clone(&path)),
                StlcScopeData::External {
                    path: Arc::clone(&path),
                },
            )?;
            here.edge(
                incoming,
                StlcScopeLabel::Import,
                target,
                ScopeProperty::Acyclic,
            )?;
            here.seal(target)?;
            here.require_source(path);
        }
        StlcDeclaration::Export(_) | StlcDeclaration::Error(_) => {}
    }
    Ok(())
}

fn path_text(here: &mut NameCx<'_, '_, '_>, path: AstBox<StlcPath>) -> Result<Arc<str>, NameError> {
    let path = here.ast(path)?;
    let StlcPath::Segments(segments) = path.as_ref();
    Ok(segments
        .iter()
        .map(|segment| here.text(*segment))
        .collect::<Result<Vec<_>, _>>()?
        .join(".")
        .into())
}

fn elaborate_expression(
    here: &mut NameCx<'_, '_, '_>,
    expression: Arc<StlcExpr>,
) -> Result<(), NameError> {
    let incoming = here.incoming_scope();
    let current = here.ast_key();
    match expression.as_ref() {
        StlcExpr::If(..)
        | StlcExpr::Succ(..)
        | StlcExpr::Group(..)
        | StlcExpr::Add(..)
        | StlcExpr::Apply(..) => {
            here.schedule_children(expression.as_ref(), incoming, ())?;
        }
        StlcExpr::Case(scrutinee, zero_branch, successor, successor_branch) => {
            here.schedule(*scrutinee, incoming, ())?;
            here.schedule(*zero_branch, incoming, ())?;
            let successor_scope = here.declare(
                StlcScopeKey::CaseSuccessor(current.clone()),
                StlcScopeData::CaseSuccessor,
            )?;
            here.edge(
                successor_scope,
                StlcScopeLabel::Lexical,
                incoming,
                ScopeProperty::Acyclic,
            )?;
            let name = here.text(*successor)?;
            declare(here, successor_scope, name, current.clone())?;
            here.schedule(*successor_branch, successor_scope, ())?;
            here.seal(successor_scope)?;
        }
        StlcExpr::Let(name, value, body) => {
            let let_scope = lexical_scope(here, current.clone(), incoming)?;
            let name = here.text(*name)?;
            declare(here, let_scope, name, current.clone())?;
            here.schedule(*value, incoming, ())?;
            here.schedule(*body, let_scope, ())?;
            here.seal(let_scope)?;
        }
        StlcExpr::Lambda(parameter, body) => {
            let lambda_scope = lexical_scope(here, current, incoming)?;
            let name = parameter_name(here, *parameter)?;
            declare(here, lambda_scope, name, parameter.key())?;
            here.schedule(*body, lambda_scope, ())?;
            here.seal(lambda_scope)?;
        }
        StlcExpr::Variable(_)
        | StlcExpr::True(_)
        | StlcExpr::False(_)
        | StlcExpr::Number(_)
        | StlcExpr::Unit(_)
        | StlcExpr::Error(_) => {}
    }
    Ok(())
}

pub fn stlc_name_rules() -> impl for<'a, 't, 'n> Fn(
    &mut ElaboratorCx<'a, 't, 'n, StlcNames>,
) -> Result<Elaboration<()>, NameError>
+ Send
+ Sync
+ 'static {
    plingo::component::semantic::rules::<StlcNames>()
        .root(elaborate_document)
        .case(elaborate_declaration)
        .case(elaborate_expression)
        .otherwise(|_| Ok(()))
        .build()
        .expect("STLC name rule table is valid")
}
