use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum Tokens {
    #[empty]
    #[enter(block)]
    ScopeStart,

    #[regex("[a-z]+")]
    Word(String),

    #[error]
    Error(LexErrorInfo),
}

fn main() {}
