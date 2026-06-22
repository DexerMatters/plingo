use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum RootToken {
    #[regex(r#"""#)]
    #[enter(string)]
    Start,

    #[error]
    Error(LexErrorInfo),
}

fn main() {}
