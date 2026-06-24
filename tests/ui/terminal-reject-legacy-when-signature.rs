use plingo::{Terminal, component::lex::{LexErrorInfo, LexMoment}};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum Tokens {
    #[regex("[a-z]+")]
    #[when(old_when)]
    Word(String),

    #[error]
    Error(LexErrorInfo),
}

fn old_when(_: &str, _: Option<&str>, _: LexMoment) -> bool {
    true
}

fn main() {}
