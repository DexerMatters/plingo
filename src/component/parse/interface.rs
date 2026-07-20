use std::any::TypeId;

use fluent_uri::Uri;

use crate::{
    component::{
        lex::{Lexer, LexerRoot},
        parse::{
            AstToken, IncrementalParseStats, ParseAddress, ParsePath, ParseUnit, Parser,
            data::{
                ast::AstBox,
                green::ParseErrorInfo,
                product::{ProductData, ProductId},
            },
            diagnostics::collect_parse_diagnostics,
            types::SessionArenas,
        },
    },
    context_callable,
    scheme::{call::CallOutcome, context::Context, layer::NonTopLayer},
    utils::Span,
};

impl<Root, Lower> Parser<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<Address = ParseAddress, Unit = ParseUnit> + Send + Sync + 'static,
{
    #[context_callable]
    pub async fn get_node<'a>(
        &'a mut self,
        ctx: &'a Context,
        path: &'a ParsePath,
    ) -> CallOutcome<Self, Vec<ProductId>> {
        match self.snapshot_state(ctx.snapshot()) {
            Ok(state) => CallOutcome::ok(self.products_at_path(state, path)),
            Err(err) => CallOutcome::fail(err),
        }
    }

    #[context_callable]
    pub async fn deref_ast_box<'a, T>(
        &'a mut self,
        ctx: &'a Context,
        ast_box: &'a AstBox<T>,
    ) -> CallOutcome<Self, T>
    where
        T: Clone + Send + Sync + 'static,
    {
        if let Err(err) = self.snapshot_state(ctx.snapshot()) {
            return CallOutcome::fail(err);
        }
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
        if let Err(err) = self.snapshot_state(ctx.snapshot()) {
            return CallOutcome::fail(err);
        }
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
        if let Err(err) = self.snapshot_state(ctx.snapshot()) {
            return CallOutcome::fail(err);
        }
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
            let next = match ctx.call(Lexer::<Root, Self>::span_of_token, entry).await {
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
        match ctx.call(Lexer::<Root, Self>::span_of_token, id).await {
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
        match self.snapshot_state(ctx.snapshot()) {
            Ok(state) => CallOutcome::ok(self.ast_boxes_at_path::<T>(state, path)),
            Err(err) => CallOutcome::fail(err),
        }
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
        match self.snapshot_state(ctx.snapshot()) {
            Ok(state) => CallOutcome::ok(self.ast_tokens_at_path::<T>(state, path)),
            Err(err) => CallOutcome::fail(err),
        }
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
        let state = match self.snapshot_state(ctx.snapshot()) {
            Ok(state) => state,
            Err(err) => return CallOutcome::fail(err),
        };
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
        ctx: &'a Context,
        product: &'a (Uri<&'static str>, ProductId),
    ) -> CallOutcome<Self, String> {
        if let Err(err) = self.snapshot_state(ctx.snapshot()) {
            return CallOutcome::fail(err);
        }
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
        ctx: &'a Context,
        uri: &'a Uri<&'static str>,
    ) -> CallOutcome<Self, Option<IncrementalParseStats>> {
        match self.snapshot_state(ctx.snapshot()) {
            Ok(state) => CallOutcome::ok(state.incremental_stats.get(uri).copied()),
            Err(err) => CallOutcome::fail(err),
        }
    }

    #[context_callable]
    pub async fn parse_diagnostics<'a>(
        &'a mut self,
        ctx: &'a Context,
        uri: &'a Uri<&'static str>,
    ) -> CallOutcome<Self, Vec<ParseErrorInfo>> {
        let state = match self.snapshot_state(ctx.snapshot()) {
            Ok(state) => state,
            Err(err) => return CallOutcome::fail(err),
        };
        let Some(session) = state.sessions.get(uri) else {
            return CallOutcome::ok(Vec::new());
        };
        let roots = state
            .roots
            .get(uri)
            .map(|roots| roots.as_slice())
            .unwrap_or(&[]);
        let diagnostics = collect_parse_diagnostics(session, self.session_arenas.get(uri), roots);
        CallOutcome::ok(diagnostics)
    }

    fn collect_product_token_entries(
        &self,
        arenas: &SessionArenas,
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
        arenas: &SessionArenas,
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
