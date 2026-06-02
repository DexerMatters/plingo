use std::marker::PhantomData;

use plingo_macros::resolve_action;

use crate::{
    component::{
        common::Pretty,
        lex::{Entry, GetVisibleTokenBatch, LexInterrupt, Lexer, LexerRoot, VisibleTokenBatch},
        parse::{GetParseTokens, TokenData},
    },
    scheme::{Context, NonTopLayer, Outcome, Resolve},
    utils::{PrettyDisplay, Span},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GetTokens(pub Span);

#[resolve_action]
impl<Root, Lower> Resolve<GetTokens> for Lexer<Root, Lower>
where
    Root: LexerRoot + Clone,
    Lower: NonTopLayer<_Key = Span, _Value = usize>,
{
    type Output = Vec<Entry<Root>>;

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a GetTokens,
    ) -> impl Future<Output = Outcome<GetTokens, Self>> + Send + 'a {
        async move { Outcome::ok(self.entries_in_span(ctx.snapshot(), action.0)) }
    }
}

#[resolve_action]
impl<Root, Lower, T> Resolve<Pretty<T>> for Lexer<Root, Lower>
where
    Root: LexerRoot + Clone,
    Lower: NonTopLayer<_Key = Span, _Value = usize>,
    T: PrettyDisplay<Lexer<Root, Lower>> + Send + Sync,
{
    type Output = String;

    fn resolve<'a>(
        &'a mut self,
        _ctx: &'a Context,
        action: &'a Pretty<T>,
    ) -> impl Future<Output = Outcome<Pretty<T>, Self>> + Send + 'a {
        async move { Outcome::ok(action.0.pretty(self).to_string()) }
    }
}

#[resolve_action]
impl<Root, Lower> Resolve<GetParseTokens> for Lexer<Root, Lower>
where
    Root: LexerRoot + Clone,
    Lower: NonTopLayer<_Key = Span, _Value = usize>,
{
    type Output = Vec<TokenData>;

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a GetParseTokens,
    ) -> impl Future<Output = Outcome<GetParseTokens, Self>> + Send + 'a {
        async move { Outcome::ok(self.token_data_in_span(ctx.snapshot(), action.0)) }
    }
}

#[resolve_action]
impl<Root, Lower> Resolve<GetVisibleTokenBatch> for Lexer<Root, Lower>
where
    Root: LexerRoot + Clone,
    Lower: NonTopLayer<_Key = Span, _Value = usize>,
{
    type Output = Option<VisibleTokenBatch>;

    fn resolve<'a>(
        &'a mut self,
        ctx: &'a Context,
        action: &'a GetVisibleTokenBatch,
    ) -> impl Future<Output = Outcome<GetVisibleTokenBatch, Self>> + Send + 'a {
        async move { Outcome::ok(self.visible_batch(ctx.snapshot(), action.0)) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GetTokenById<T>(pub usize, pub PhantomData<T>);

#[resolve_action]
impl<Root, Lower, T> Resolve<GetTokenById<T>> for Lexer<Root, Lower>
where
    Root: LexerRoot + Clone + 'static,
    Lower: NonTopLayer<_Key = Span, _Value = usize>,
    T: Clone + Send + Sync + 'static,
{
    type Output = T;

    fn resolve<'a>(
        &'a mut self,
        _ctx: &'a Context,
        action: &'a GetTokenById<T>,
    ) -> impl Future<Output = Outcome<GetTokenById<T>, Self>> + Send + 'a {
        let id = action.0;
        let entry = self.arena.get(id).cloned();
        async move {
            match entry {
                Some(Entry::Token { value, .. }) => {
                    let p: &dyn std::any::Any = &value;
                    match p.downcast_ref::<T>() {
                        Some(v) => Outcome::ok(v.clone()),
                        None => Outcome::fail(LexInterrupt::InternalError(format!(
                            "GetTokenById downcast failed: T={}",
                            std::any::type_name::<T>()
                        ))),
                    }
                }
                Some(_) => Outcome::fail(LexInterrupt::InternalError(
                    "entry exists but is not a Token variant".into(),
                )),
                None => Outcome::fail(LexInterrupt::InternalError(format!(
                    "GetTokenById: id {id} out of range (arena size {})",
                    self.arena.len()
                ))),
            }
        }
    }
}
