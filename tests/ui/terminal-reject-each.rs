use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum RootToken {
    #[each(InnerToken)]
    Content(Vec<InnerToken>),

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
