//! Reactive lexer scenarios (ported from `tests/unit/component_lex_node.rs`):
//! lex errors, per-uri isolation, and deterministic token order, driven
//! through the `Workspace` + `install_lexer` API (plan §8.2, matrix 1).

use std::{fmt, sync::Arc};

use plingo::framework::lex::{LexErrorInfo, TokenVec, Tokens, install_lexer};
use plingo::framework::{SourceEdit, Workspace};
use plingo::utils::Span;
use plingo_macros::Terminal;

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(root { Word })]
enum TestTokens {
    #[regex(r"[a-z]+")]
    Word(String),
    #[error]
    Error(LexErrorInfo),
}

impl fmt::Display for TestTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

fn uri(name: &str) -> fluent_uri::Uri<&'static str> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn build(workers: usize) -> Workspace {
    Workspace::build_with(workers, |engine| {
        install_lexer::<TestTokens>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

#[test]
fn lexer_observes_source_without_a_lower_layer() {
    let mut ws = build(1);
    ws.open(uri("lex"), "hello").unwrap();
    let tokens: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .map_view::<Tokens<TestTokens>>()
        .get(&"test://lex".to_string())
        .expect("committed tokens");
    // Deterministic order: the word token(s) in source order, then the
    // synthetic EOF token.
    assert_eq!(tokens.tokens.len(), 1, "one word, no EOF in public tokens");
    assert_eq!(tokens.tokens[0].value, TestTokens::Word("hello".into()));
    assert_eq!(tokens.tokens[0].length, 5);
    assert!(tokens.errors.is_empty());
}

#[test]
fn lex_errors_are_published_with_offsets() {
    let mut ws = build(1);
    // `@` is not a word character: it becomes an error token.
    ws.open(uri("err"), "a@b").unwrap();
    let tokens: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .map_view::<Tokens<TestTokens>>()
        .get(&"test://err".to_string())
        .expect("committed tokens");
    assert!(!tokens.errors.is_empty(), "error token must be materialized");
    let error = tokens.errors[0];
    assert!(error.start < error.end);
    assert!(error.end <= "a@b".len());
    // The error token also appears in the ordered token list in source
    // position.
    assert!(tokens.tokens.iter().any(|t| t.error.is_some()));
}

#[test]
fn per_uri_isolation_keeps_sibling_document_untouched() {
    let mut ws = build(1);
    ws.open(uri("a"), "alpha").unwrap();
    ws.open(uri("b"), "beta").unwrap();
    let before_b: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .map_view::<Tokens<TestTokens>>()
        .get(&"test://b".to_string())
        .unwrap();
    // Edit document A only.
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(uri("a"), 0).unwrap(),
        value: "x".into(),
    }])
    .unwrap();
    let after_b: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .map_view::<Tokens<TestTokens>>()
        .get(&"test://b".to_string())
        .unwrap();
    assert_eq!(before_b, after_b, "B's lexer child never re-ran");
    // A changed.
    let after_a: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .map_view::<Tokens<TestTokens>>()
        .get(&"test://a".to_string())
        .unwrap();
    assert_eq!(after_a.tokens[0].value, TestTokens::Word("xalpha".into()));
}

#[test]
fn deterministic_order_is_stable_across_worker_counts() {
    let text = "one two three";
    let mut single = build(1);
    let mut many = build(4);
    single.open(uri("det"), text).unwrap();
    many.open(uri("det"), text).unwrap();
    let s = single.snapshot().map_view::<Tokens<TestTokens>>();
    let m = many.snapshot().map_view::<Tokens<TestTokens>>();
    assert_eq!(
        s.get(&"test://det".to_string()),
        m.get(&"test://det".to_string()),
        "1-worker and N-worker commits are identical"
    );
}

#[test]
fn closing_a_document_retracts_its_tokens() {
    let mut ws = build(1);
    ws.open(uri("gone"), "hello").unwrap();
    assert!(ws
        .snapshot()
        .map_view::<Tokens<TestTokens>>()
        .get(&"test://gone".to_string())
        .is_some());
    ws.close(uri("gone")).unwrap();
    assert!(ws
        .snapshot()
        .map_view::<Tokens<TestTokens>>()
        .get(&"test://gone".to_string())
        .is_none());
}

#[test]
fn equal_text_edit_is_a_no_op_past_the_lexer() {
    let mut ws = build(1);
    ws.open(uri("eq"), "same").unwrap();
    let before: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .map_view::<Tokens<TestTokens>>()
        .get(&"test://eq".to_string())
        .unwrap();
    ws.open(uri("eq"), "same").unwrap();
    let after: Arc<TokenVec<TestTokens>> = ws
        .snapshot()
        .map_view::<Tokens<TestTokens>>()
        .get(&"test://eq".to_string())
        .unwrap();
    assert_eq!(before, after, "text equality short-circuits the lexer");
}