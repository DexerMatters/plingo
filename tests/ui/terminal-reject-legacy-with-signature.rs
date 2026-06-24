use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum Tokens {
    #[regex("[a-z]+")]
    #[with(old_with)]
    Word(String),

    #[error]
    Error(LexErrorInfo),
}

fn old_with(_: &str, _: Option<&str>) -> Option<String> {
    Some(String::new())
}

fn main() {}
