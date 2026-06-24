use plingo::{Terminal, component::lex::{LexErrorInfo, LexerRoot, WhenCx}};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum Tokens {
    #[empty]
    #[enter(block)]
    #[when(always)]
    ScopeStart(String),

    #[regex("[a-z]+")]
    Word(String),

    #[error]
    Error(LexErrorInfo),
}

fn always<T: LexerRoot>(_: &WhenCx<T>) -> bool {
    true
}

fn main() {}
