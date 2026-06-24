use plingo::{Terminal, component::lex::{LexErrorInfo, LexerRoot, WithCx}};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(
    root {
        QuoteStart,
        QuoteStart,
    },
    string {
        StringText,
    },
)]
enum Tokens {
    #[regex(r#""+"#)]
    #[enter(string)]
    #[with(quote_key)]
    QuoteStart(String),

    #[regex(r#"[^"]+"#)]
    StringText(String),

    #[error]
    Error(LexErrorInfo),
}

fn quote_key<T: LexerRoot>(_: &mut WithCx<T>) {
}

fn main() {}
