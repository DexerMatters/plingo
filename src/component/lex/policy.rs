use plingo_macros::resolve_action;

use crate::{
    component::lex::{Entry, Lexer, LexerRoot},
    scheme::{Context, NonTopLayer, Outcome, Resolve},
    utils::{PrettyDisplay, Span},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GetTokens(pub Span);

#[derive(Debug)]
pub struct Pretty<T>(pub T);

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
