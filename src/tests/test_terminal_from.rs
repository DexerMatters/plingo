use crate::{
    Terminal,
    component::lex::{Entry, LexErrorInfo, LexErrorKind, Lexer, LexerState},
};

fn collect_entries<Root>(
    lexer: &mut Lexer<Root>,
    input: &str,
) -> Vec<(usize, Entry<Root>, usize, usize)>
where
    Root: crate::component::lex::LexerRoot + Clone,
{
    let token_ids: Vec<(usize, usize, usize)> = {
        let mut ids = Vec::new();
        lexer
            .lex_cont(
                LexerState::new(lexer.state_id_of::<Root>().unwrap()),
                input.to_string(),
                |token_id, _, start, end| {
                    ids.push((token_id, start, end));
                    true
                },
            )
            .unwrap();
        ids
    };

    token_ids
        .into_iter()
        .map(|(id, start, end)| (id, lexer.get(id).clone(), start, end))
        .collect()
}

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum FlatTokens {
    #[regex(r#"""#)]
    #[then_require(StringLiteral)]
    StringStart,

    #[from(FlatInner)]
    #[till(StringEnd)]
    StringLiteral(FlatInner),

    #[regex(r#"""#)]
    StringEnd,

    #[regex(r"\d+")]
    Number(usize),

    #[error]
    Error(LexErrorInfo),
}

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
enum FlatInner {
    #[regex(r#"a+"#)]
    A,

    #[regex(r#"b+"#)]
    B,

    #[error]
    Error(LexErrorInfo),
}

#[test]
fn flat_from_terminal_emits_wrapped_inner_tokens() {
    let mut lexer = Lexer::<FlatTokens>::new().unwrap();
    let entries = collect_entries(&mut lexer, r#""aabb""#);

    assert!(matches!(
        entries[0].1,
        Entry::Token {
            value: FlatTokens::StringStart,
            ..
        }
    ));
    assert!(matches!(
        entries[1].1,
        Entry::Token {
            value: FlatTokens::StringLiteral(FlatInner::A),
            ..
        }
    ));
    assert!(matches!(
        entries[2].1,
        Entry::Token {
            value: FlatTokens::StringLiteral(FlatInner::B),
            ..
        }
    ));
    assert!(matches!(
        entries[3].1,
        Entry::Token {
            value: FlatTokens::StringEnd,
            ..
        }
    ));
    assert!(matches!(entries[4].1, Entry::EOF));
}

#[test]
fn flat_from_terminal_keeps_nested_recovery_inside_inner_state() {
    let mut lexer = Lexer::<FlatTokens>::new().unwrap();
    let entries = collect_entries(&mut lexer, r#""aa$bb""#);

    assert_eq!(entries.len(), 6);
    assert!(matches!(
        entries[1].1,
        Entry::Token {
            value: FlatTokens::StringLiteral(FlatInner::A),
            ..
        }
    ));
    assert!(matches!(
        entries[2].1,
        Entry::Error {
            value: FlatTokens::StringLiteral(FlatInner::Error(LexErrorInfo {
                kind: LexErrorKind::UnexpectedInput,
                start: 3,
                end: 4,
            })),
            ..
        }
    ));
    assert!(matches!(
        entries[3].1,
        Entry::Token {
            value: FlatTokens::StringLiteral(FlatInner::B),
            ..
        }
    ));
    assert!(matches!(
        entries[4].1,
        Entry::Token {
            value: FlatTokens::StringEnd,
            ..
        }
    ));
}

#[test]
fn flat_from_terminal_reports_required_boundary_at_eof() {
    let mut lexer = Lexer::<FlatTokens>::new().unwrap();
    let entries = collect_entries(&mut lexer, r#""aa"#);

    assert_eq!(entries.len(), 4);
    assert!(matches!(
        entries[1].1,
        Entry::Token {
            value: FlatTokens::StringLiteral(FlatInner::A),
            ..
        }
    ));
    assert!(matches!(
        entries[2].1,
        Entry::Error {
            info: LexErrorInfo {
                kind: LexErrorKind::RequiredBoundary,
                ..
            },
            value: FlatTokens::Error(_),
            ..
        }
    ));
    assert!(matches!(entries[3].1, Entry::EOF));
}
