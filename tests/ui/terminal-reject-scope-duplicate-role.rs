use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(
    root {
        QuoteRun => enter(string, quote_key),
        QuoteRun,
    },
    string {
        QuoteRun => exit(quote_matches),
        StringText,
    },
)]
enum Tokens {
    #[regex(r#""+"#)]
    QuoteRun(String),

    #[regex(r#"[^"]+"#)]
    StringText(String),

    #[error]
    Error(LexErrorInfo),
}

fn quote_key(token: &Tokens) -> Option<String> {
    match token {
        Tokens::QuoteRun(value) => Some(value.clone()),
        _ => None,
    }
}

fn quote_matches(token: &Tokens, key: &str) -> bool {
    matches!(token, Tokens::QuoteRun(value) if value == key)
}

fn main() {}
