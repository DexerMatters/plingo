use std::{any::TypeId, collections::HashSet};

use fluent_uri::Uri;

use crate::{
    component::{
        lex::{Lexer, LexerRoot},
        parse::{
            AstToken, IncrementalParseStats, ParseChange, ParsePath, Parser, ParserSessionState,
            data::{
                ast::AstBox,
                green::{ParseErrorInfo, TreeData},
                product::{ProductData, ProductId},
            },
        },
    },
    context_callable,
    scheme::{
        call::CallOutcome,
        context::Context,
        layer::{NonTopLayer, SnapshotLayer},
    },
    utils::Span,
};

impl<Root, Lower> Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<Change = ParseChange> + Send + Sync + 'static,
{
    #[context_callable]
    pub async fn get_node<'a>(
        &'a mut self,
        ctx: &'a Context,
        path: &'a ParsePath,
    ) -> CallOutcome<Self, Vec<ProductId>> {
        let state = self.state(ctx.snapshot()).unwrap_or(self.latest_state());
        CallOutcome::ok(self.products_at_path(state, path))
    }

    #[context_callable]
    pub async fn deref_ast_box<'a, T>(
        &'a mut self,
        _ctx: &'a Context,
        ast_box: &'a AstBox<T>,
    ) -> CallOutcome<Self, T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let Some(arenas) = self.session_arenas.get(&ast_box.uri) else {
            return CallOutcome::fail(super::ParseError::Build(
                super::grammar::BuildError::MissingProduct(0),
            ));
        };
        match arenas.ast.cloned::<T>(ast_box.id) {
            Some(v) => CallOutcome::ok(v),
            None => CallOutcome::fail(super::ParseError::Build(
                super::grammar::BuildError::TypeMismatch { product: 0 },
            )),
        }
    }

    #[context_callable]
    pub async fn deref_ast_token<'a, T>(
        &'a mut self,
        ctx: &'a Context,
        token: &'a AstToken<T>,
    ) -> CallOutcome<Self, T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let _ = self;
        let id = token.id;
        match ctx.call(Lexer::<Root, Self>::token_by_id::<T>, id).await {
            Ok(value) => CallOutcome::ok(value),
            Err(_) => CallOutcome::fail(super::ParseError::Build(
                super::grammar::BuildError::TypeMismatch { product: 0 },
            )),
        }
    }

    #[context_callable]
    pub async fn span_of_ast_box<'a, T>(
        &'a mut self,
        ctx: &'a Context,
        ast_box: &'a AstBox<T>,
    ) -> CallOutcome<Self, Span>
    where
        T: Send + Sync + 'static,
    {
        let Some(arenas) = self.session_arenas.get(&ast_box.uri) else {
            return CallOutcome::fail(super::ParseError::Build(
                super::grammar::BuildError::MissingAst(ast_box.id),
            ));
        };
        let Some(product) = arenas.ast.product_of(ast_box.id) else {
            return CallOutcome::fail(super::ParseError::Build(
                super::grammar::BuildError::MissingAst(ast_box.id),
            ));
        };

        let mut entries = Vec::new();
        if let Err(err) = self.collect_product_token_entries(arenas, product, &mut entries) {
            return CallOutcome::fail(super::ParseError::Build(err));
        }

        if entries.is_empty() {
            return match self.product_fallback_span(ast_box.uri, arenas, product) {
                Ok(span) => CallOutcome::ok(span),
                Err(err) => CallOutcome::fail(super::ParseError::Build(err)),
            };
        }

        let mut span: Option<Span> = None;
        for entry in entries {
            let next = match ctx
                .call(Lexer::<Root, Self>::span_of_token_entry, entry)
                .await
            {
                Ok(span) => span,
                Err(_) => {
                    return CallOutcome::fail(super::ParseError::Build(
                        super::grammar::BuildError::MissingToken(entry),
                    ));
                }
            };
            span = Some(match span {
                Some(current) => current.union(&next).unwrap_or(current),
                None => next,
            });
        }

        match span {
            Some(span) => CallOutcome::ok(span),
            None => CallOutcome::fail(super::ParseError::Build(
                super::grammar::BuildError::MissingAst(ast_box.id),
            )),
        }
    }

    #[context_callable]
    pub async fn span_of_ast_token<'a, T>(
        &'a mut self,
        ctx: &'a Context,
        token: &'a AstToken<T>,
    ) -> CallOutcome<Self, Span>
    where
        T: Send + Sync + 'static,
    {
        let _ = self;
        let id = token.id;
        match ctx.call(Lexer::<Root, Self>::span_of_token_entry, id).await {
            Ok(span) => CallOutcome::ok(span),
            Err(_) => CallOutcome::fail(super::ParseError::Build(
                super::grammar::BuildError::MissingToken(id),
            )),
        }
    }

    #[context_callable]
    pub async fn get_ast_tree<'a, T>(
        &'a mut self,
        ctx: &'a Context,
        path: &'a ParsePath,
    ) -> CallOutcome<Self, Vec<AstBox<T>>>
    where
        T: Send + Sync + 'static,
    {
        let state = self.state(ctx.snapshot()).unwrap_or(self.latest_state());
        CallOutcome::ok(self.ast_boxes_at_path::<T>(state, path))
    }

    #[context_callable]
    pub async fn get_ast_token<'a, T>(
        &'a mut self,
        ctx: &'a Context,
        path: &'a ParsePath,
    ) -> CallOutcome<Self, Vec<AstToken<T>>>
    where
        T: Send + Sync + 'static,
    {
        let state = self.state(ctx.snapshot()).unwrap_or(self.latest_state());
        CallOutcome::ok(self.ast_tokens_at_path::<T>(state, path))
    }

    #[context_callable]
    pub async fn get_root_ast_box<'a, T>(
        &'a mut self,
        ctx: &'a Context,
        uri: &'a Uri<&'static str>,
    ) -> CallOutcome<Self, Vec<AstBox<T>>>
    where
        T: Send + Sync + 'static,
    {
        let state = self.state(ctx.snapshot()).unwrap_or(self.latest_state());
        let Some(roots) = state.roots.get(uri) else {
            return CallOutcome::ok(Vec::new());
        };
        let Some(arenas) = self.session_arenas.get(uri) else {
            return CallOutcome::ok(Vec::new());
        };
        let target = TypeId::of::<T>();
        CallOutcome::ok(
            roots
                .iter()
                .filter_map(|&pid| match &arenas.products.get(pid)?.data {
                    ProductData::Node { ast, ty, .. } if *ty == target => {
                        Some(AstBox::new(*ast, *uri))
                    }
                    _ => None,
                })
                .collect(),
        )
    }

    #[context_callable]
    pub async fn describe_product<'a>(
        &'a mut self,
        _ctx: &'a Context,
        product: &'a (Uri<&'static str>, ProductId),
    ) -> CallOutcome<Self, String> {
        let Some(arenas) = self.session_arenas.get(&product.0) else {
            return CallOutcome::ok("(missing)".to_string());
        };
        let Some(product) = arenas.products.get(product.1) else {
            return CallOutcome::ok("(missing)".to_string());
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
        CallOutcome::ok(desc)
    }

    #[context_callable]
    pub async fn incremental_stats_for<'a>(
        &'a mut self,
        _ctx: &'a Context,
        uri: &'a Uri<&'static str>,
    ) -> CallOutcome<Self, Option<IncrementalParseStats>> {
        CallOutcome::ok(self.incremental_stats(*uri))
    }

    #[context_callable]
    pub async fn parse_diagnostics<'a>(
        &'a mut self,
        ctx: &'a Context,
        uri: &'a Uri<&'static str>,
    ) -> CallOutcome<Self, Vec<ParseErrorInfo>> {
        let state = self.state(ctx.snapshot()).unwrap_or(self.latest_state());
        let Some(session) = state.sessions.get(uri) else {
            return CallOutcome::ok(Vec::new());
        };
        let roots = state.roots.get(uri).map(Vec::as_slice).unwrap_or(&[]);
        let diagnostics = collect_parse_diagnostics(session, self.session_arenas.get(uri), roots);
        CallOutcome::ok(diagnostics)
    }

    fn collect_product_token_entries(
        &self,
        arenas: &super::SessionArenas,
        product_id: ProductId,
        entries: &mut Vec<usize>,
    ) -> Result<(), super::grammar::BuildError> {
        let Some(product) = arenas.products.get(product_id) else {
            return Err(super::grammar::BuildError::MissingProduct(product_id));
        };
        match &product.data {
            ProductData::Token { entry, .. } => entries.push(*entry),
            ProductData::Node { children, .. } | ProductData::Error { children } => {
                for &child in children {
                    self.collect_product_token_entries(arenas, child, entries)?;
                }
            }
        }
        Ok(())
    }

    fn product_fallback_span(
        &self,
        uri: Uri<&'static str>,
        arenas: &super::SessionArenas,
        product_id: ProductId,
    ) -> Result<Span, super::grammar::BuildError> {
        let product = arenas
            .products
            .get(product_id)
            .ok_or(super::grammar::BuildError::MissingProduct(product_id))?;
        let length = arenas
            .trees
            .get(product.green)
            .map_or(0, |tree| tree.length);
        Span::new_uri(uri, 0, length)
            .map_err(|_| super::grammar::BuildError::MissingProduct(product_id))
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
        ProductData::Error { .. } => {
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
