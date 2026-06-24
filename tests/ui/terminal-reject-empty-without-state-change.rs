use plingo::{Terminal, component::lex::{LexErrorInfo, LexerRoot, WhenCx}};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum Tokens {
    #[empty]
    #[when(always)]
    Marker,

    #[regex("[a-z]+")]
    Word(String),

    #[error]
    Error(LexErrorInfo),
}

fn always<T: LexerRoot>(_: &WhenCx<T>) -> bool {
    true
}

fn main() {}
