use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum RootToken {
    #[regex(r#"""#)]
    #[leave]
    End,

    #[error]
    Error(LexErrorInfo),
}

fn main() {}
