use plingo::{Terminal, component::lex::LexErrorInfo};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum RootToken {
    #[regex(r#"""#)]
    #[leave_when(always)]
    End,

    #[error]
    Error(LexErrorInfo),
}

fn always(_lexeme: &str, _key: Option<&str>) -> bool {
    true
}

fn main() {}
