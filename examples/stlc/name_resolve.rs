//! Incremental lexical name resolution for the STLC example.

use std::sync::Arc;

use fluent_uri::Uri;
use plingo::{
    Component, Context, NodeError, Result,
    component::writes,
    component::{
        parse::{AstKey, data::AstBox},
        scope::{
            ScopeDefinitions, ScopeEdges, ScopeEntries, ScopeId, ScopeProperty, SourceRequirements,
        },
        structural::StructureEntry,
    },
    scheme::node::Graph,
};

use super::syntax::{StlcDeclaration, StlcDocument, StlcExpr, StlcParam, StlcPath, StlcToken};

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
    Document(Uri<&'static str>),
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
    ShadowedVariable {
        name: Arc<str>,
    },
    AmbiguousVariable {
        name: Arc<str>,
        candidates: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, plingo::component::scope::ScopeDomain)]
#[scope_domain(
    scope_key = StlcScopeKey,
    scope_data = StlcScopeData,
    label = StlcScopeLabel,
    request = Arc<str>
)]
pub struct StlcScope;

/// The one document coordinator for name resolution.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NameDocument {
    pub uri: Uri<&'static str>,
}

/// One AST name task with its inherited scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NameAst {
    pub ast: AstKey,
    pub incoming: ScopeId<StlcScope>,
}

impl Component for NameDocument {
    type Output = ();
    type Writes = writes!(
        ScopeDefinitions<StlcScope>,
        ScopeEdges<StlcScope>,
        ScopeEntries<StlcScope, Uri<&'static str>, ()>,
    );

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<()> {
        name_document(cx, self.uri)
    }
}

impl Component for NameAst {
    type Output = ();
    type Writes = writes!(
        ScopeDefinitions<StlcScope>,
        ScopeEdges<StlcScope>,
        SourceRequirements<StlcScope>,
    );

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<()> {
        name_ast(cx, self.ast.clone(), self.incoming)
    }
}

fn name_document(cx: &mut Context<'_, NameDocument>, uri: Uri<&'static str>) -> Result<()> {
    let Some(document) = cx
        .view::<plingo::component::Parsed<StlcToken, StlcDocument>>()
        .accepted(uri)?
    else {
        return Ok(());
    };
    let root_ast = document.key();

    let body_scope = {
        let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
        let document_scope =
            scopes.declare(StlcScopeKey::Document(uri), StlcScopeData::Document)?;
        let body_scope = scopes.declare_linked(
            StlcScopeKey::Lexical(root_ast.clone()),
            StlcScopeData::Lexical,
            document_scope,
            StlcScopeLabel::Lexical,
            ScopeProperty::Acyclic,
        )?;
        scopes.support_entry(StructureEntry::new(uri, document_scope, ()))?;
        body_scope
    };

    if let StlcDocument::Lines(declarations) = document.value().as_ref() {
        cx.keep_all(declarations.iter().map(|declaration| NameAst {
            ast: declaration.key(),
            incoming: body_scope,
        }));
    }
    Ok(())
}

fn name_ast(
    cx: &mut Context<'_, NameAst>,
    ast: AstKey,
    incoming: ScopeId<StlcScope>,
) -> Result<()> {
    let (declaration, expression) = {
        let mut parsed = cx.view::<plingo::component::Parsed<StlcToken, StlcDocument>>();
        (
            parsed.artifact::<StlcDeclaration>(ast.clone()),
            parsed.artifact::<StlcExpr>(ast.clone()),
        )
    };
    if let Some(declaration) = declaration {
        name_declaration(cx, ast, incoming, declaration)?;
    } else if let Some(expression) = expression {
        name_expression(cx, ast, incoming, expression)?;
    }
    Ok(())
}

fn parameter_name(
    parsed: &mut plingo::component::parse::ParsedView<'_, '_, NameAst, StlcToken, StlcDocument>,
    parameter: AstBox<StlcParam>,
) -> Result<Option<Arc<str>>> {
    let value = parsed
        .artifact::<StlcParam>(parameter.key())
        .ok_or_else(|| NodeError::message("missing parameter AST"))?;
    let token = match value.as_ref() {
        StlcParam::Bare(name, _) | StlcParam::Parenthesized(name, _) => *name,
    };
    Ok(parsed.token_text(parameter.uri, token))
}

fn path_text(
    parsed: &mut plingo::component::parse::ParsedView<'_, '_, NameAst, StlcToken, StlcDocument>,
    path: AstBox<StlcPath>,
) -> Result<Option<Arc<str>>> {
    let value = parsed
        .artifact::<StlcPath>(path.key())
        .ok_or_else(|| NodeError::message("missing path AST"))?;
    let StlcPath::Segments(segments) = value.as_ref();
    let mut text = String::new();
    for (index, segment) in segments.iter().enumerate() {
        if index != 0 {
            text.push('.');
        }
        let Some(value) = parsed.token_text(path.uri, *segment) else {
            return Ok(None);
        };
        text.push_str(&value);
    }
    Ok(Some(text.into()))
}

fn name_declaration(
    cx: &mut Context<'_, NameAst>,
    current: AstKey,
    incoming: ScopeId<StlcScope>,
    declaration: Arc<StlcDeclaration>,
) -> Result<()> {
    let StlcDeclaration::Value(name, parameters, _, body) = declaration.as_ref() else {
        if let StlcDeclaration::Import(path) = declaration.as_ref() {
            let mut parsed = cx.view::<plingo::component::Parsed<StlcToken, StlcDocument>>();
            let Some(path) = path_text(&mut parsed, *path)? else {
                return Ok(());
            };
            let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
            let target = scopes.declare(
                StlcScopeKey::External(Arc::clone(&path)),
                StlcScopeData::External {
                    path: Arc::clone(&path),
                },
            )?;
            scopes.support_edge(
                incoming,
                StlcScopeLabel::Import,
                target,
                ScopeProperty::Acyclic,
            )?;
            scopes.require_source(path)?;
        }
        return Ok(());
    };

    let mut parsed = cx.view::<plingo::component::Parsed<StlcToken, StlcDocument>>();
    let Some(name) = parsed.token_text(current.uri, *name) else {
        return Ok(());
    };
    let mut parameter_names = Vec::with_capacity(parameters.len());
    for parameter in parameters {
        let Some(name) = parameter_name(&mut parsed, *parameter)? else {
            return Ok(());
        };
        parameter_names.push(name);
    }
    drop(parsed);

    let body_scope = {
        let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
        let body_scope = scopes.declare_linked(
            StlcScopeKey::Lexical(current.clone()),
            StlcScopeData::Lexical,
            incoming,
            StlcScopeLabel::Lexical,
            ScopeProperty::Acyclic,
        )?;
        scopes.declare_linked(
            StlcScopeKey::Declaration(current.clone()),
            StlcScopeData::Declaration {
                name,
                definition: current.clone(),
            },
            body_scope,
            StlcScopeLabel::Declaration,
            ScopeProperty::Acyclic,
        )?;
        for (parameter, name) in parameters.iter().zip(parameter_names) {
            scopes.declare_linked(
                StlcScopeKey::Declaration(parameter.key()),
                StlcScopeData::Declaration {
                    name,
                    definition: parameter.key(),
                },
                body_scope,
                StlcScopeLabel::Declaration,
                ScopeProperty::Acyclic,
            )?;
        }
        body_scope
    };
    cx.keep(NameAst {
        ast: body.key(),
        incoming: body_scope,
    });
    Ok(())
}

fn name_expression(
    cx: &mut Context<'_, NameAst>,
    current: AstKey,
    incoming: ScopeId<StlcScope>,
    expression: Arc<StlcExpr>,
) -> Result<()> {
    match expression.as_ref() {
        StlcExpr::If(condition, then_branch, else_branch) => {
            cx.keep(NameAst {
                ast: condition.key(),
                incoming,
            });
            cx.keep(NameAst {
                ast: then_branch.key(),
                incoming,
            });
            cx.keep(NameAst {
                ast: else_branch.key(),
                incoming,
            });
        }
        StlcExpr::Case(scrutinee, zero_branch, successor, successor_branch) => {
            cx.keep(NameAst {
                ast: scrutinee.key(),
                incoming,
            });
            cx.keep(NameAst {
                ast: zero_branch.key(),
                incoming,
            });
            let successor_scope = {
                let mut parsed = cx.view::<plingo::component::Parsed<StlcToken, StlcDocument>>();
                let Some(name) = parsed.token_text(current.uri, *successor) else {
                    return Ok(());
                };
                let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
                let successor_scope = scopes.declare(
                    StlcScopeKey::CaseSuccessor(current.clone()),
                    StlcScopeData::CaseSuccessor,
                )?;
                scopes.support_edge(
                    successor_scope,
                    StlcScopeLabel::Lexical,
                    incoming,
                    ScopeProperty::Acyclic,
                )?;
                scopes.declare_linked(
                    StlcScopeKey::Declaration(current.clone()),
                    StlcScopeData::Declaration {
                        name,
                        definition: current.clone(),
                    },
                    successor_scope,
                    StlcScopeLabel::Declaration,
                    ScopeProperty::Acyclic,
                )?;
                successor_scope
            };
            cx.keep(NameAst {
                ast: successor_branch.key(),
                incoming: successor_scope,
            });
        }
        StlcExpr::Let(name, value, body) => {
            cx.keep(NameAst {
                ast: value.key(),
                incoming,
            });
            let let_scope = {
                let mut parsed = cx.view::<plingo::component::Parsed<StlcToken, StlcDocument>>();
                let Some(name) = parsed.token_text(current.uri, *name) else {
                    return Ok(());
                };
                let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
                let let_scope = scopes.declare_linked(
                    StlcScopeKey::Lexical(current.clone()),
                    StlcScopeData::Lexical,
                    incoming,
                    StlcScopeLabel::Lexical,
                    ScopeProperty::Acyclic,
                )?;
                scopes.declare_linked(
                    StlcScopeKey::Declaration(current.clone()),
                    StlcScopeData::Declaration {
                        name,
                        definition: current.clone(),
                    },
                    let_scope,
                    StlcScopeLabel::Declaration,
                    ScopeProperty::Acyclic,
                )?;
                let_scope
            };
            cx.keep(NameAst {
                ast: body.key(),
                incoming: let_scope,
            });
        }
        StlcExpr::Lambda(parameter, body) => {
            let mut parsed = cx.view::<plingo::component::Parsed<StlcToken, StlcDocument>>();
            let Some(name) = parameter_name(&mut parsed, *parameter)? else {
                return Ok(());
            };
            let lambda_scope = {
                let mut scopes = cx.view::<plingo::component::Scope<StlcScope>>();
                let lambda_scope = scopes.declare_linked(
                    StlcScopeKey::Lexical(current.clone()),
                    StlcScopeData::Lexical,
                    incoming,
                    StlcScopeLabel::Lexical,
                    ScopeProperty::Acyclic,
                )?;
                scopes.declare_linked(
                    StlcScopeKey::Declaration(parameter.key()),
                    StlcScopeData::Declaration {
                        name,
                        definition: parameter.key(),
                    },
                    lambda_scope,
                    StlcScopeLabel::Declaration,
                    ScopeProperty::Acyclic,
                )?;
                lambda_scope
            };
            cx.keep(NameAst {
                ast: body.key(),
                incoming: lambda_scope,
            });
        }
        StlcExpr::Succ(inner) | StlcExpr::Group(inner) => {
            cx.keep(NameAst {
                ast: inner.key(),
                incoming,
            });
        }
        StlcExpr::Add(left, right) | StlcExpr::Apply(left, right) => {
            cx.keep(NameAst {
                ast: left.key(),
                incoming,
            });
            cx.keep(NameAst {
                ast: right.key(),
                incoming,
            });
        }
        StlcExpr::True(_)
        | StlcExpr::False(_)
        | StlcExpr::Number(_)
        | StlcExpr::Variable(_)
        | StlcExpr::Unit(_)
        | StlcExpr::Error(_) => {}
    }
    Ok(())
}

pub fn install_name_components(graph: &mut Graph) -> std::result::Result<(), NodeError> {
    graph.register::<NameDocument>()?;
    graph.register::<NameAst>()?;
    Ok(())
}
