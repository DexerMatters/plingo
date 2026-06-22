use plingo::Terminal;

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum RootToken {
    #[regex(r"[a-z]+")]
    Text,
}

fn main() {}
