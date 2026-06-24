use plingo::{
    Terminal,
    component::lex::{LexErrorInfo, LexMoment, WhenCx, WithCx},
};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scope_slots(
    depth: usize,
    pending: usize,
)]
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
    #[when(should_enter)]
    #[with(init_child)]
    ScopeStart,

    #[empty]
    #[exit]
    #[when(should_exit)]
    ScopeEnd,

    #[error]
    Error(LexErrorInfo),
}

fn should_enter(cx: &WhenCx<Tokens>) -> bool {
    cx.moment() == LexMoment::Normal
        && cx.get(Tokens::pending).copied().unwrap_or(0) > cx.get(Tokens::depth).copied().unwrap_or(0)
}

fn init_child(cx: &mut WithCx<Tokens>) {
    if let Some(&pending) = cx.source_get(Tokens::pending) {
        cx.set(Tokens::depth, pending);
    }
}

fn should_exit(cx: &WhenCx<Tokens>) -> bool {
    cx.moment() == LexMoment::Eof && cx.get(Tokens::depth).copied().unwrap_or(0) > 0
}

fn main() {}
