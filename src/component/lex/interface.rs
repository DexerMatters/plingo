use crate::{
    component::{
        lex::{IncrementalLexStats, LexInterrupt, LexToken, Lexer, LexerRoot},
        parse::TokenData,
    },
    context_callable,
    scheme::{call::CallOutcome, context::Context, layer::NonTopLayer},
    utils::{PrettyDisplay, Span},
};

impl<Root, Lower> Lexer<Root, Lower>
where
    Root: LexerRoot + Clone,
    Lower: NonTopLayer<
            Address = fluent_uri::Uri<&'static str>,
            Unit = crate::component::parse::TokenData,
        > + Send
        + Sync
        + 'static,
{
    #[context_callable]
    pub async fn tokens<'a>(
        &'a mut self,
        ctx: &'a Context,
        span: &'a Span,
    ) -> CallOutcome<Self, Vec<LexToken<Root>>> {
        match self.tokens_in_span_snapshot(ctx.snapshot(), *span) {
            Ok(tokens) => CallOutcome::ok(tokens),
            Err(err) => CallOutcome::fail(err),
        }
    }

    #[context_callable]
    pub async fn parse_tokens<'a>(
        &'a mut self,
        ctx: &'a Context,
        span: &'a Span,
    ) -> CallOutcome<Self, Vec<TokenData>> {
        match self.token_data_in_span(ctx.snapshot(), *span) {
            Ok(tokens) => CallOutcome::ok(tokens),
            Err(err) => CallOutcome::fail(err),
        }
    }

    #[context_callable]
    pub async fn incremental_stats_for<'a>(
        &'a mut self,
        ctx: &'a Context,
        uri: &'a fluent_uri::Uri<&'static str>,
    ) -> CallOutcome<Self, Option<IncrementalLexStats>> {
        match self.snapshot_state(ctx.snapshot()) {
            Ok(state) => CallOutcome::ok(state.incremental_stats.get(uri).copied()),
            Err(err) => CallOutcome::fail(err),
        }
    }

    #[context_callable]
    pub async fn pretty<'a, T>(
        &'a mut self,
        _ctx: &'a Context,
        value: &'a T,
    ) -> CallOutcome<Self, String>
    where
        T: PrettyDisplay<Lexer<Root, Lower>> + Send + Sync + Clone + 'static,
    {
        let value = value.clone();
        CallOutcome::ok(value.pretty(self).to_string())
    }

    #[context_callable]
    pub async fn token_by_id<'a, T>(
        &'a mut self,
        ctx: &'a Context,
        id: &'a usize,
    ) -> CallOutcome<Self, T>
    where
        Root: 'static,
        T: Clone + Send + Sync + 'static,
    {
        if let Err(err) = self.snapshot_state(ctx.snapshot()) {
            return CallOutcome::fail(err);
        }
        let id = *id;
        match self.token(id) {
            Some(token) => {
                let p: &dyn std::any::Any = &token.value;
                match p.downcast_ref::<T>() {
                    Some(v) => CallOutcome::ok(v.clone()),
                    None => CallOutcome::fail(LexInterrupt::InternalError(format!(
                        "token_by_id downcast failed: T={}",
                        std::any::type_name::<T>()
                    ))),
                }
            }
            None => CallOutcome::fail(LexInterrupt::InternalError(format!(
                "token_by_id: id {id} out of range (arena size {})",
                self.arena.len()
            ))),
        }
    }

    #[context_callable]
    pub async fn span_of_token<'a>(
        &'a mut self,
        ctx: &'a Context,
        id: &'a usize,
    ) -> CallOutcome<Self, Span> {
        let id = *id;
        match self.token_span(ctx.snapshot(), id) {
            Ok(Some(span)) => CallOutcome::ok(span),
            Ok(None) => CallOutcome::fail(LexInterrupt::InternalError(format!(
                "span_of_token: id {id} not found in current lexer snapshot"
            ))),
            Err(err) => CallOutcome::fail(err),
        }
    }
}
