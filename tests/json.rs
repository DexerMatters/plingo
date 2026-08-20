mod common;

use std::sync::Arc;

use common::json::{JsonDocument, JsonToken};
use plingo::framework::Workspace;
use plingo::framework::lex::{TokenVec, Tokens, install_lexer};
use plingo::framework::parse::{ParseDiagnostics, ParseStatus, ParseUnits, install_parser};
use plingo::utils::Span;

fn uri(name: &str) -> fluent_uri::Uri<&'static str> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn build(workers: usize) -> Workspace {
    Workspace::build_with(workers, |engine| {
        install_lexer::<JsonToken>(engine)?;
        install_parser::<JsonToken, JsonDocument>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

#[test]
fn json_syntax_builds_with_macro_grammar() {
    // The derive macros build the pure grammar; `install_parser`
    // constructs the LR(1) parser inside, so reaching a committed parse
    // proves the Terminal/NonTerminal derives wired to the framework paths.
    let mut ws = build(1);
    ws.open(uri("macro"), r#"{"a": 1}"#).unwrap();
    let snap = ws.snapshot();
    assert!(snap.map_view::<Tokens<JsonToken>>().contains(&"test://macro".to_string()));
    assert_eq!(JsonToken::Null.to_string(), "Null");
}

#[test]
fn whitespace_is_skipped_and_lex_errors_are_reported() {
    let mut ws = build(1);
    ws.open(uri("ws"), "  {  \"a\" : 1  }  ").unwrap();
    let snap = ws.snapshot();
    let tokens: Arc<TokenVec<JsonToken>> = snap
        .map_view::<Tokens<JsonToken>>()
        .get(&"test://ws".to_string())
        .expect("tokens");
    // Lex errors are empty for clean input.
    assert!(tokens.errors.is_empty());
    // Token values never carry the skipped whitespace terminal.
    assert!(tokens
        .tokens
        .iter()
        .all(|t| !t.value.to_string().contains("Whitespace")));

    // Unterminated string: an error token is present with correct offsets.
    ws.open(uri("err"), r#"{"a": "unterminated}"#).unwrap();
    let tokens: Arc<TokenVec<JsonToken>> = ws
        .snapshot()
        .map_view::<Tokens<JsonToken>>()
        .get(&"test://err".to_string())
        .expect("tokens");
    assert!(!tokens.errors.is_empty());
    let err = tokens.errors[0];
    assert!(err.start < err.end);
    assert!(err.end <= r#"{"a": "unterminated}"#.len());
}

#[test]
fn nested_parse_publishes_units_and_clean_status() {
    let mut ws = build(1);
    let text = r#"{"a": [1, true, {"b": null}]}"#;
    ws.open(uri("nested"), text).unwrap();
    let snap = ws.snapshot();
    let unit = snap
        .map_view::<ParseUnits<JsonDocument>>()
        .get(&"test://nested".to_string())
        .expect("parse unit");
    assert_eq!(unit.status, ParseStatus::Clean);
    // The root node id is a real derived id (never the sentinel).
    assert_ne!(unit.root, plingo::reactive::NodeId(u64::MAX));
}

#[test]
fn recovery_publishes_diagnostics() {
    let mut ws = build(1);
    // Missing closing brace: the parser recovers and reports diagnostics.
    ws.open(uri("recover"), r#"{"a": 1"#).unwrap();
    let snap = ws.snapshot();
    let diags = snap
        .map_view::<ParseDiagnostics>()
        .get(&"test://recover".to_string());
    assert!(matches!(diags, Some(d) if !d.is_empty()));
}

#[test]
fn equal_edit_produces_no_token_or_unit_change() {
    let mut ws = build(1);
    ws.open(uri("eq"), "12").unwrap();
    let before: Arc<TokenVec<JsonToken>> = ws
        .snapshot()
        .map_view::<Tokens<JsonToken>>()
        .get(&"test://eq".to_string())
        .unwrap();
    // Re-opening with identical text is a no-op text delta.
    ws.open(uri("eq"), "12").unwrap();
    let after: Arc<TokenVec<JsonToken>> = ws
        .snapshot()
        .map_view::<Tokens<JsonToken>>()
        .get(&"test://eq".to_string())
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn one_worker_and_many_workers_commit_identical_state() {
    let text = r#"{"k": [1, 2, "x"]}"#;
    let mut single = build(1);
    let mut many = build(4);
    single.open(uri("det"), text).unwrap();
    many.open(uri("det"), text).unwrap();
    let ts = single.snapshot().map_view::<Tokens<JsonToken>>();
    let tm = many.snapshot().map_view::<Tokens<JsonToken>>();
    assert_eq!(
        ts.get(&"test://det".to_string()),
        tm.get(&"test://det".to_string())
    );
}

#[test]
fn editing_document_a_does_not_change_document_b() {
    // Per-uri isolation (matrix 1): an edit to A never re-runs B's lexer
    // child, so B's committed tokens are bit-identical.
    let mut ws = build(1);
    ws.open(uri("a"), r#"{"a": 1}"#).unwrap();
    ws.open(uri("b"), r#"{"b": 2}"#).unwrap();
    let before: Arc<TokenVec<JsonToken>> = ws
        .snapshot()
        .map_view::<Tokens<JsonToken>>()
        .get(&"test://b".to_string())
        .unwrap();
    ws.edit(vec![plingo::framework::SourceEdit::Insert {
        key: Span::point_uri(uri("a"), 6).unwrap(),
        value: "0".into(),
    }])
    .expect("edit applies");
    let after: Arc<TokenVec<JsonToken>> = ws
        .snapshot()
        .map_view::<Tokens<JsonToken>>()
        .get(&"test://b".to_string())
        .unwrap();
    assert_eq!(before, after);
}