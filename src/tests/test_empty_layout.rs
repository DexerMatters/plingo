use crate::{
    Terminal,
    component::lex::{LexErrorInfo, LexMoment, Lexer, LexerState, WhenCx, WithCx},
};

fn collect_values<Root>(lexer: &mut Lexer<Root>, input: &str) -> Vec<Root>
where
    Root: crate::component::lex::LexerRoot + Clone,
{
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

    ids.into_iter()
        .map(|id| lexer.token(id).unwrap().value.clone())
        .collect()
}

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(root { EnterLoop }, loop_state { ExitLoop })]
enum EmptyCycleToken {
    #[empty]
    #[enter(loop_state)]
    #[when(always)]
    EnterLoop,

    #[empty]
    #[exit]
    #[when(always)]
    ExitLoop,

    #[error]
    Error(LexErrorInfo),
}

fn always(_: &WhenCx<EmptyCycleToken>) -> bool {
    true
}

#[test]
fn empty_transition_cycles_are_rejected() {
    let mut lexer = Lexer::<EmptyCycleToken>::new().unwrap();
    let error = lexer
        .lex_cont(
            LexerState::new(lexer.state_id_of::<EmptyCycleToken>().unwrap()),
            String::new(),
            |_, _, _, _| true,
        )
        .unwrap_err();
    assert!(error.to_string().contains("empty token cycle"));
}

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scope_slots(
    current_indent: usize,
    pending_indent: usize,
)]
#[scopes(
    root {
        LineIndent,
        Identifier,
        ScopeStart,
    },
    block {
        LineIndent,
        Identifier,
        ScopeStart,
        ScopeEnd,
    },
)]
enum LayoutToken {
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Identifier(String),

    #[regex(r"\n *")]
    #[skip]
    #[with(record_indent)]
    LineIndent,

    #[empty]
    #[enter(block)]
    #[when(should_indent)]
    #[with(push_indent)]
    ScopeStart,

    #[empty]
    #[exit]
    #[when(should_dedent)]
    #[with(sync_parent_indent)]
    ScopeEnd,

    #[error]
    Error(LexErrorInfo),
}

fn line_indent(lexeme: &str) -> usize {
    lexeme.chars().skip(1).take_while(|ch| *ch == ' ').count()
}

fn record_indent(cx: &mut WithCx<LayoutToken>) {
    cx.set(LayoutToken::pending_indent, line_indent(cx.lexeme()));
}

fn should_indent(cx: &WhenCx<LayoutToken>) -> bool {
    if cx.moment() != LexMoment::Normal {
        return false;
    }
    let pending = cx.get(LayoutToken::pending_indent).copied().unwrap_or(0);
    let current = cx.get(LayoutToken::current_indent).copied().unwrap_or(0);
    pending > current
}

fn push_indent(cx: &mut WithCx<LayoutToken>) {
    if let Some(&pending) = cx.source_get(LayoutToken::pending_indent) {
        cx.set(LayoutToken::current_indent, pending);
        cx.remove(LayoutToken::pending_indent);
    }
}

fn should_dedent(cx: &WhenCx<LayoutToken>) -> bool {
    let current = cx.get(LayoutToken::current_indent).copied().unwrap_or(0);
    if current == 0 {
        return false;
    }
    if cx.moment() == LexMoment::Eof {
        return true;
    }
    let pending = cx
        .get(LayoutToken::pending_indent)
        .copied()
        .unwrap_or(current);
    pending < current
}

fn sync_parent_indent(cx: &mut WithCx<LayoutToken>) {
    if let Some(&pending) = cx.source_get(LayoutToken::pending_indent) {
        cx.set(LayoutToken::pending_indent, pending);
    }
}

#[test]
fn empty_scope_tokens_represent_simple_indentation() {
    let mut lexer = Lexer::<LayoutToken>::new().unwrap();
    let values = collect_values(
        &mut lexer,
        r#"
    a
        aa
        aaa
    "#,
    );

    for value in &values {
        println!("{:?}", value);
    }
}

#[test]
fn empty_scope_tokens_drain_nested_scopes_at_eof() {
    let mut lexer = Lexer::<LayoutToken>::new().unwrap();
    let values = collect_values(&mut lexer, "aaa\n   bbb\n      ccc");

    assert_eq!(
        values,
        vec![
            LayoutToken::Identifier("aaa".to_string()),
            LayoutToken::ScopeStart,
            LayoutToken::Identifier("bbb".to_string()),
            LayoutToken::ScopeStart,
            LayoutToken::Identifier("ccc".to_string()),
            LayoutToken::ScopeEnd,
            LayoutToken::ScopeEnd,
        ]
    );
}

#[test]
fn same_indentation_emits_no_empty_scope_tokens() {
    let mut lexer = Lexer::<LayoutToken>::new().unwrap();
    let values = collect_values(&mut lexer, "aaa\nbbb");

    assert_eq!(
        values,
        vec![
            LayoutToken::Identifier("aaa".to_string()),
            LayoutToken::Identifier("bbb".to_string()),
        ]
    );
}
