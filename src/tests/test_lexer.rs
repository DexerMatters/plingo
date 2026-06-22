use plingo_macros::Terminal;

use crate::component::lex::{LexErrorInfo, LexToken, Lexer, LexerState};

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
        StringStart => enter(string, string_start_key),
    },
    string {
        StringContent,
        StringEnd => exit(string_end_matches),
    },
)]
enum Tokens {
    #[regex(r#"\d+"#)]  Number(usize),
    #[regex(r#""+"#)]   StringStart(String),
    #[regex(r#""+"#)]   StringEnd(String),
    #[regex(r#"[^"]+"#)]
    #[recover_when(string_end_recover)]
                        StringContent(String),

    #[error]            Error(LexErrorInfo),
}

fn string_start_key(token: &Tokens) -> Option<String> {
    match token {
        Tokens::StringStart(value) => Some(value.clone()),
        _ => None,
    }
}

fn string_end_matches(token: &Tokens, key: &str) -> bool {
    matches!(token, Tokens::StringEnd(value) if value == key)
}

fn string_end_recover(rest: &str, key: Option<&str>) -> usize {
    let quotes = rest.chars().take_while(|c| *c == '"').count();
    let needed = key.map(str::len).unwrap_or(0);
    if quotes < needed {
        quotes
    } else {
        0
    }
}

#[test]
fn test_lexer() {
    let mut lexer = Lexer::<Tokens>::new().unwrap();

    let input = r#"""ad""""jac"ent"""#;
    let tokens = collect_entries(&mut lexer, input);

    let values = tokens.into_iter().map(|token| token.value).collect::<Vec<_>>();
    for value in values {
        println!("{:?}", value);
    }
}
