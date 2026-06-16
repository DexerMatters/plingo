use std::{any::TypeId, collections::HashSet, marker::PhantomData};

use fluent_uri::Uri;
use plingo_macros::resolve_action;

use crate::{
    component::{
        lex::{Lexer, LexerRoot, policy::GetTokenById},
        parse::{
            AstToken, IncrementalParseStats, ParsePath, Parser, ParserSessionState,
            data::{
                ast::AstBox,
                green::{ParseErrorInfo, TreeData},
                product::{ProductData, ProductId},
            },
        },
    },
    scheme::{Context, NonTopLayer, Outcome, Resolve, SnapshotLayer},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GetNode(pub ParsePath);

#[resolve_action]
impl<Root, Lower> Resolve<GetNode> for Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<_Key = ParsePath, _Value = super::ParseForest> + Send + Sync + 'static,
{
    type Output = Vec<ProductId>;

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a GetNode,
    ) -> impl Future<Output = Outcome<GetNode, Self>> + Send + 'a {
        async move {
            let state = self.state(ctx.snapshot()).unwrap_or(self.latest_state());
            Outcome::ok(self.products_at_path(state, &action.0))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DerefAstBox<T>(pub AstBox<T>);

#[resolve_action]
impl<Root, Lower, T> Resolve<DerefAstBox<T>> for Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<_Key = ParsePath, _Value = super::ParseForest> + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    type Output = T;

    fn resolve<'a>(
        &'a mut self,
        _ctx: &'a Context,
        action: &'a DerefAstBox<T>,
    ) -> impl Future<Output = Outcome<DerefAstBox<T>, Self>> + Send + 'a {
        async move {
            let Some(arenas) = self.session_arenas.get(&action.0.uri) else {
                return Outcome::fail(super::ParseError::Build(
                    super::grammar::BuildError::MissingProduct(0),
                ));
            };
            match arenas.ast.cloned::<T>(action.0.id) {
                Some(v) => Outcome::ok(v),
                None => Outcome::fail(super::ParseError::Build(
                    super::grammar::BuildError::TypeMismatch { product: 0 },
                )),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DerefAstToken<T>(pub AstToken<T>);

#[resolve_action]
impl<Root, Lower, T> Resolve<DerefAstToken<T>> for Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<_Key = ParsePath, _Value = super::ParseForest> + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    type Output = T;

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a DerefAstToken<T>,
    ) -> impl Future<Output = Outcome<DerefAstToken<T>, Self>> + Send + 'a {
        async move {
            match ctx
                .post::<Lexer<Root, Self>, GetTokenById<T>>(GetTokenById(action.0.id, PhantomData))
                .await
            {
                Ok(value) => Outcome::ok(value),
                Err(_) => Outcome::fail(super::ParseError::Build(
                    super::grammar::BuildError::TypeMismatch { product: 0 },
                )),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GetAstTree<T>(pub ParsePath, pub PhantomData<T>);

#[resolve_action]
impl<Root, Lower, T> Resolve<GetAstTree<T>> for Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<_Key = ParsePath, _Value = super::ParseForest> + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    type Output = Vec<AstBox<T>>;

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a GetAstTree<T>,
    ) -> impl Future<Output = Outcome<GetAstTree<T>, Self>> + Send + 'a {
        async move {
            let state = self.state(ctx.snapshot()).unwrap_or(self.latest_state());
            Outcome::ok(self.ast_boxes_at_path::<T>(state, &action.0))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GetAstToken<T>(pub ParsePath, pub PhantomData<T>);

#[resolve_action]
impl<Root, Lower, T> Resolve<GetAstToken<T>> for Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<_Key = ParsePath, _Value = super::ParseForest> + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    type Output = Vec<AstToken<T>>;

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a GetAstToken<T>,
    ) -> impl Future<Output = Outcome<GetAstToken<T>, Self>> + Send + 'a {
        async move {
            let state = self.state(ctx.snapshot()).unwrap_or(self.latest_state());
            Outcome::ok(self.ast_tokens_at_path::<T>(state, &action.0))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GetRootAstBox<T>(pub Uri<&'static str>, pub PhantomData<T>);

#[resolve_action]
impl<Root, Lower, T> Resolve<GetRootAstBox<T>> for Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<_Key = ParsePath, _Value = super::ParseForest> + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    type Output = Vec<AstBox<T>>;

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a GetRootAstBox<T>,
    ) -> impl Future<Output = Outcome<GetRootAstBox<T>, Self>> + Send + 'a {
        async move {
            let state = self.state(ctx.snapshot()).unwrap_or(self.latest_state());
            let Some(roots) = state.roots.get(&action.0) else {
                return Outcome::ok(Vec::new());
            };
            let Some(arenas) = self.session_arenas.get(&action.0) else {
                return Outcome::ok(Vec::new());
            };
            let target = TypeId::of::<T>();
            Outcome::ok(
                roots
                    .iter()
                    .filter_map(|&pid| match &arenas.products.get(pid)?.data {
                        ProductData::Node { ast, ty, .. } if *ty == target => {
                            Some(AstBox::new(*ast, action.0))
                        }
                        _ => None,
                    })
                    .collect(),
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DescribeProduct(pub Uri<&'static str>, pub ProductId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GetIncrementalStats(pub Uri<&'static str>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GetParseDiagnostics(pub Uri<&'static str>);

#[resolve_action]
impl<Root, Lower> Resolve<DescribeProduct> for Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<_Key = ParsePath, _Value = super::ParseForest> + Send + Sync + 'static,
{
    type Output = String;

    fn resolve<'a>(
        &'a mut self,
        _ctx: &'a Context,
        action: &'a DescribeProduct,
    ) -> impl Future<Output = Outcome<DescribeProduct, Self>> + Send + 'a {
        async move {
            let Some(arenas) = self.session_arenas.get(&action.0) else {
                return Outcome::ok("(missing)".to_string());
            };
            let Some(product) = arenas.products.get(action.1) else {
                return Outcome::ok("(missing)".to_string());
            };
            let desc = match arenas.trees.get(product.green) {
                Some(tree) => {
                    use crate::component::parse::data::green::TreeData;
                    match &tree.data {
                        TreeData::Node { id, .. } => {
                            let nt = &self.grammar.non_terminals[*id as usize];
                            nt.label.to_string()
                        }
                        TreeData::Leaf { id } => {
                            if let Some(&idx) = self.grammar.terminal_indices.get(id) {
                                self.grammar.terminals[idx].label.to_string()
                            } else {
                                "(unknown terminal)".to_string()
                            }
                        }
                        TreeData::Error { .. } => "(error)".to_string(),
                    }
                }
                None => "(no tree)".to_string(),
            };
            Outcome::ok(desc)
        }
    }
}

#[resolve_action]
impl<Root, Lower> Resolve<GetIncrementalStats> for Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<_Key = ParsePath, _Value = super::ParseForest> + Send + Sync + 'static,
{
    type Output = Option<IncrementalParseStats>;

    fn resolve<'a>(
        &'a mut self,
        _ctx: &'a Context,
        action: &'a GetIncrementalStats,
    ) -> impl Future<Output = Outcome<GetIncrementalStats, Self>> + Send + 'a {
        async move { Outcome::ok(self.incremental_stats(action.0)) }
    }
}

#[resolve_action]
impl<Root, Lower> Resolve<GetParseDiagnostics> for Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<_Key = ParsePath, _Value = super::ParseForest> + Send + Sync + 'static,
{
    type Output = Vec<ParseErrorInfo>;

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a GetParseDiagnostics,
    ) -> impl Future<Output = Outcome<GetParseDiagnostics, Self>> + Send + 'a {
        async move {
            let state = self.state(ctx.snapshot()).unwrap_or(self.latest_state());
            let Some(session) = state.sessions.get(&action.0) else {
                return Outcome::ok(Vec::new());
            };
            let roots = state.roots.get(&action.0).map(Vec::as_slice).unwrap_or(&[]);
            let diagnostics =
                collect_parse_diagnostics(session, self.session_arenas.get(&action.0), roots);
            Outcome::ok(diagnostics)
        }
    }
}

pub(crate) fn collect_parse_diagnostics(
    state: &ParserSessionState,
    arenas: Option<&super::SessionArenas>,
    roots: &[ProductId],
) -> Vec<ParseErrorInfo> {
    let mut diagnostics = Vec::new();
    let mut seen_diagnostics = HashSet::new();

    for info in &state.diagnostics {
        if seen_diagnostics.insert(info.clone()) {
            diagnostics.push(info.clone());
        }
    }

    let Some(arenas) = arenas else {
        return diagnostics;
    };

    let mut seen_products = HashSet::new();
    for &pid in roots {
        collect_ast_parse_diagnostics(
            pid,
            arenas,
            &mut seen_products,
            &mut seen_diagnostics,
            &mut diagnostics,
        );
    }

    diagnostics
}

fn collect_ast_parse_diagnostics(
    product_id: ProductId,
    arenas: &super::SessionArenas,
    seen_products: &mut HashSet<ProductId>,
    seen_diagnostics: &mut HashSet<ParseErrorInfo>,
    diagnostics: &mut Vec<ParseErrorInfo>,
) {
    if !seen_products.insert(product_id) {
        return;
    }
    let Some(product) = arenas.products.get(product_id) else {
        return;
    };

    match &product.data {
        ProductData::Error => {
            let Some(tree) = arenas.trees.get(product.green) else {
                return;
            };
            let TreeData::Error {
                kind,
                node,
                unexpected,
                expected,
                recovered,
                location,
                ..
            } = &tree.data
            else {
                return;
            };
            let info = ParseErrorInfo {
                kind: kind.clone(),
                node: *node,
                length: tree.length,
                unexpected: *unexpected,
                expected: *expected,
                recovered: *recovered,
                location: *location,
            };
            if seen_diagnostics.insert(info.clone()) {
                diagnostics.push(info);
            }
        }
        ProductData::Node { children, .. } => {
            for child in children.iter().copied() {
                collect_ast_parse_diagnostics(
                    child,
                    arenas,
                    seen_products,
                    seen_diagnostics,
                    diagnostics,
                );
            }
        }
        ProductData::Token { .. } => {}
    }
}
