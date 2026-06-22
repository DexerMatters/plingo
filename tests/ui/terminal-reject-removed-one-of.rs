use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum RootToken {
    #[one_of(InnerToken)]
    Content(InnerToken),

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
