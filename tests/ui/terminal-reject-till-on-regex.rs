use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum RootToken {
    #[regex(r"[a-z]+")]
    #[till(End)]
    Text,

    #[regex(r#"""#)]
    End,

    #[error]
    Error(LexErrorInfo),
}

fn main() {}
