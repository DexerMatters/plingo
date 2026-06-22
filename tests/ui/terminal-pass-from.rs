use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum RootToken {
    #[regex(r#"""#)]
    #[then_require(Content)]
    Start,

    #[from(InnerToken)]
    #[till(End)]
    Content(InnerToken),

    #[regex(r#"""#)]
    End,

    #[error]
    Error(LexErrorInfo),
}

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum InnerToken {
    #[regex(r"[a-z]+")]
    Text,

    #[error]
    Error(LexErrorInfo),
}

fn main() {}
