use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum RootToken {
    #[regex(r#"""#)]
    #[then_require(Content)]
    Start,

    #[regex(r#"""#)]
    End,

    #[error]
    Error(LexErrorInfo),
}

fn main() {}
