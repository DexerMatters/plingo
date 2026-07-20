use std::fmt::Display;

use color_print::cwrite;
use plingo_macros::Terminal;

use crate::{
    component::lex::{LexErrorInfo, LexToken, Lexer, LexerState, WhenCx, WithCx},
    utils::PrettyDisplay,
};

fn collect_entries<Root>(lexer: &mut Lexer<Root>, input: &str) -> Vec<LexToken<Root>>
where
    Root: crate::component::lex::LexerRoot + Clone,
{
    let token_ids: Vec<usize> = {
        let mut ids = Vec::new();
        lexer
            .lex_cont(
                LexerState::new(lexer.state_id_of::<Root>().unwrap()),
                input.to_string(),
                |token_id, _, _, _| {
                    ids.push(token_id);
                    true
                },
            )
            .unwrap();
        ids
    };

    token_ids
        .into_iter()
        .map(|id| lexer.token(id).unwrap().clone())
        .collect()
}

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(
    root {
        Number,
        StringStart,
    },
    string {
        StringContent,
        StringEnd,
    },
)]
enum Tokens {
    #[regex(r#"\d+"#)]
    Number(usize),
    #[regex(r#""+"#)]
    #[enter(string)]
    #[with(string_start_key)]
    StringStart,
    #[regex(r#""+"#)]
    #[exit]
    #[when(string_end_matches)]
    StringEnd,
    #[regex(r#"[^"]+"#)]
    #[recover_when(string_end_recover)]
    StringContent(String),

    #[error]
    Error(LexErrorInfo),
}

impl Display for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tokens::Number(n) => cwrite!(f, "Number(<blue>{}</blue>)", n),
            Tokens::StringStart => cwrite!(f, "StringStart"),
            Tokens::StringContent(s) => cwrite!(f, "StringContent(<blue>{}</blue>)", s),
            Tokens::StringEnd => cwrite!(f, "StringEnd"),
            Tokens::Error(e) => cwrite!(f, "Error(<red>{:?}</red>)", e),
        }
    }
}

fn string_start_key(cx: &mut WithCx<Tokens>) {
    cx.set(Tokens::scope_key, cx.lexeme().to_string());
}

fn string_end_matches(cx: &WhenCx<Tokens>) -> bool {
    cx.get(Tokens::scope_key)
        .is_some_and(|key| key == cx.lexeme())
}

fn string_end_recover(rest: &str, key: Option<&str>) -> usize {
    let quotes = rest.chars().take_while(|c| *c == '"').count();
    let needed = key.map(str::len).unwrap_or(0);
    if quotes < needed { quotes } else { 0 }
}

#[test]
fn test_lexer() {
    let mut lexer = Lexer::<Tokens>::new().unwrap();

    let input = r#"""ad""""jac"ent""1234"""func(""hello"")""""#;
    let tokens = collect_entries(&mut lexer, input);
    for token in &tokens {
        println!("{}", token.pretty(&lexer));
    }
}
