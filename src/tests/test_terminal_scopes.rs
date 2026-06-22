use crate::{
    Terminal,
    component::lex::{LexErrorInfo, LexToken, Lexer, LexerState},
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
        QuoteRun => enter(string, quote_key),
        Number,
    },
    string {
        QuoteRun => exit(quote_matches),
        StringText,
    },
)]
enum ScopedTokens {
    #[regex(r#""+"#)]
    QuoteRun(String),

    #[regex(r#"[^"]+"#)]
    StringText(String),

    #[regex(r"\d+")]
    Number(usize),

    #[error]
    Error(LexErrorInfo),
}

fn quote_key(token: &ScopedTokens) -> Option<String> {
    match token {
        ScopedTokens::QuoteRun(value) => Some(value.clone()),
        _ => None,
    }
}

fn quote_matches(token: &ScopedTokens, key: &str) -> bool {
    matches!(token, ScopedTokens::QuoteRun(value) if value == key)
}

#[test]
fn scope_terminal_splits_adjacent_quote_runs_into_close_then_open() {
    let mut lexer = Lexer::<ScopedTokens>::new().unwrap();
    let entries = collect_entries(&mut lexer, r#"12""hello""""world"""#);

    let values = entries.into_iter().map(|token| token.value).collect::<Vec<_>>();
    assert_eq!(
        values,
        vec![
            ScopedTokens::Number(12),
            ScopedTokens::QuoteRun("\"\"".to_string()),
            ScopedTokens::StringText("hello".to_string()),
            ScopedTokens::QuoteRun("\"\"".to_string()),
            ScopedTokens::QuoteRun("\"\"".to_string()),
            ScopedTokens::StringText("world".to_string()),
            ScopedTokens::QuoteRun("\"\"".to_string()),
        ]
    );
}
