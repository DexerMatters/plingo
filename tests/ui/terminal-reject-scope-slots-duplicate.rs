use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scope_slots(
    indent: usize,
    indent: String,
)]
enum Tokens {
    #[regex("[a-z]+")]
    Word(String),

    #[error]
    Error(LexErrorInfo),
}

fn main() {}
