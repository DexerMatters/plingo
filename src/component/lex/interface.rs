use crate::{
    component::{
        lex::{Entry, LexInterrupt, Lexer, LexerRoot, TokenChange},
        parse::TokenData,
    },
    context_callable,
    scheme::{call::CallOutcome, context::Context, layer::NonTopLayer},
    utils::{PrettyDisplay, Span},
};

impl<Root, Lower> Lexer<Root, Lower>
where
    Root: LexerRoot + Clone,
    Lower: NonTopLayer<Change = TokenChange> + Send + Sync + 'static,
{
    #[context_callable]
    pub async fn tokens<'a>(
        &'a mut self,
        ctx: &'a Context,
        span: &'a Span,
    ) -> CallOutcome<Self, Vec<Entry<Root>>> {
        CallOutcome::ok(self.entries_in_span(ctx.snapshot(), *span))
    }

    #[context_callable]
    pub async fn parse_tokens<'a>(
        &'a mut self,
        ctx: &'a Context,
        span: &'a Span,
    ) -> CallOutcome<Self, Vec<TokenData>> {
        CallOutcome::ok(self.token_data_in_span(ctx.snapshot(), *span))
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
        _ctx: &'a Context,
        id: &'a usize,
    ) -> CallOutcome<Self, T>
    where
        Root: 'static,
        T: Clone + Send + Sync + 'static,
    {
        let id = *id;
        let entry = self.arena.get(id).cloned();
        match entry {
            Some(Entry::Token { value, .. }) => {
                let p: &dyn std::any::Any = &value;
                match p.downcast_ref::<T>() {
                    Some(v) => CallOutcome::ok(v.clone()),
                    None => CallOutcome::fail(LexInterrupt::InternalError(format!(
                        "token_by_id downcast failed: T={}",
                        std::any::type_name::<T>()
                    ))),
                }
            }
            Some(_) => CallOutcome::fail(LexInterrupt::InternalError(
                "entry exists but is not a Token variant".into(),
            )),
            None => CallOutcome::fail(LexInterrupt::InternalError(format!(
                "token_by_id: id {id} out of range (arena size {})",
                self.arena.len()
            ))),
        }
    }

    #[context_callable]
    pub async fn span_of_token_entry<'a>(
        &'a mut self,
        ctx: &'a Context,
        id: &'a usize,
    ) -> CallOutcome<Self, Span> {
        let id = *id;
        match self.entry_span(ctx.snapshot(), id) {
            Some(span) => CallOutcome::ok(span),
            None => CallOutcome::fail(LexInterrupt::InternalError(format!(
                "span_of_token_entry: id {id} not found in current lexer snapshot"
            ))),
        }
    }
}
