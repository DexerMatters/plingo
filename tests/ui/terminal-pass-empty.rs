use plingo::{
    Terminal,
    component::lex::{LexErrorInfo, LexMoment, WhenCx, WithCx},
};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(
    root {
        Word,
        ScopeStart,
    },
    block {
        Word,
        ScopeEnd,
    },
)]
enum Tokens {
    #[regex("[a-z]+")]
    Word(String),

    #[empty]
    #[enter(block)]
    #[when(always_normal)]
    #[with(set_key)]
    ScopeStart,

    #[empty]
    #[exit]
    #[when(always_eof)]
    #[with(clear_key)]
    ScopeEnd,

    #[error]
    Error(LexErrorInfo),
}

fn always_normal(cx: &WhenCx<Tokens>) -> bool {
    cx.moment() == LexMoment::Normal
}

fn always_eof(cx: &WhenCx<Tokens>) -> bool {
    cx.moment() == LexMoment::Eof
}

fn set_key(cx: &mut WithCx<Tokens>) {
    cx.set(Tokens::scope_key, "block".to_string());
}

fn clear_key(cx: &mut WithCx<Tokens>) {
    cx.set(Tokens::scope_key, "root".to_string());
}

fn main() {}
