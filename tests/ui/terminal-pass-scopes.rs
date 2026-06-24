use plingo::{
    Terminal,
    component::lex::{LexErrorInfo, WhenCx, WithCx},
};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(
    root {
        QuoteStart,
        Number,
    },
    string {
        QuoteEnd,
        StringText,
    },
)]
enum Tokens {
    #[regex(r#""+"#)]
    #[enter(string)]
    #[with(quote_key)]
    QuoteStart(String),

    #[regex(r#""+"#)]
    #[exit]
    #[when(quote_matches)]
    QuoteEnd(String),

    #[regex(r#"[^"]+"#)]
    StringText(String),

    #[regex(r"\d+")]
    Number(usize),

    #[error]
    Error(LexErrorInfo),
}

fn quote_key(cx: &mut WithCx<Tokens>) {
    cx.set(Tokens::scope_key, cx.lexeme().to_string());
}

fn quote_matches(cx: &WhenCx<Tokens>) -> bool {
    cx.get(Tokens::scope_key)
        .is_some_and(|key| key == cx.lexeme())
}

fn main() {}
