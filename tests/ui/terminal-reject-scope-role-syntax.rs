use plingo::{Terminal, component::lex::{LexErrorInfo, LexerRoot, WhenCx, WithCx}};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(
    root {
        QuoteStart => enter(string, quote_key),
        StringText,
    },
    string {
        QuoteEnd,
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

    #[error]
    Error(LexErrorInfo),
}

fn quote_key<T: LexerRoot>(_: &mut WithCx<T>) {
}

fn quote_matches<T: LexerRoot>(_: &WhenCx<T>) -> bool {
    true
}

fn main() {}
