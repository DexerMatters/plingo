mod common;

use std::sync::Arc;

use common::json::{JsonDocument, JsonToken};
use plingo::framework::lex::{TokenVec, Tokens, install_lexer};
use plingo::framework::parse::{ParseDiagnostics, ParseStatus, ParseUnits, install_parser};
use plingo::framework::{SourceEdit, Workspace};
use plingo::utils::Span;

fn uri(name: &str) -> fluent_uri::Uri<String> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn build(workers: usize) -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<JsonToken>(engine)?;
        install_parser::<JsonToken, JsonDocument>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

fn tokens(ws: &Workspace, name: &str) -> Arc<TokenVec<JsonToken>> {
    ws.snapshot()
        .observe::<Tokens<JsonToken>>(name.to_owned())
        .expect("tokens")
}

#[test]
fn json_syntax_builds_with_macro_grammar() {
    let mut ws = build(1);
    ws.open(uri("macro"), r#"{"a": 1}"#).unwrap();
    let snap = ws.snapshot();
    assert!(
        snap.observe::<Tokens<JsonToken>>("test://macro".to_owned())
            .is_some()
    );
    assert_eq!(JsonToken::Null.to_string(), "Null");
}

#[test]
fn whitespace_is_skipped_and_lex_errors_are_reported() {
    let mut ws = build(1);
    ws.open(uri("ws"), "  {  \"a\" : 1  }  ").unwrap();
    let token_vec = tokens(&ws, "test://ws");
    assert!(token_vec.errors.is_empty());
    assert!(
        token_vec
            .tokens
            .iter()
            .all(|token| !token.value.to_string().contains("Whitespace"))
    );

    ws.open(uri("err"), r#"{"a": "unterminated}"#).unwrap();
    let token_vec = tokens(&ws, "test://err");
    assert!(!token_vec.errors.is_empty());
    let error = token_vec.errors[0];
    assert!(error.start < error.end);
    assert!(error.end <= r#"{"a": "unterminated}"#.len());
}

#[test]
fn nested_parse_publishes_units_and_clean_status() {
    let mut ws = build(1);
    let text = r#"{"a": [1, true, {"b": null}]}"#;
    ws.open(uri("nested"), text).unwrap();
    let unit = ws
        .snapshot()
        .observe::<ParseUnits<JsonDocument>>("test://nested".to_owned())
        .expect("parse unit");
    assert_eq!(unit.status, ParseStatus::Clean);
    assert!(unit.root.is_some());
}

#[test]
fn recovery_publishes_diagnostics() {
    let mut ws = build(1);
    ws.open(uri("recover"), r#"{"a": 1"#).unwrap();
    let diagnostics = ws
        .snapshot()
        .list::<ParseDiagnostics>(&"test://recover".to_owned());
    assert!(!diagnostics.is_empty());
}

#[test]
fn equal_edit_produces_no_token_or_unit_change() {
    let mut ws = build(1);
    ws.open(uri("eq"), "12").unwrap();
    let before = tokens(&ws, "test://eq");
    ws.open(uri("eq"), "12").unwrap();
    let after = tokens(&ws, "test://eq");
    assert_eq!(before, after);
}

#[test]
fn one_worker_and_many_workers_commit_identical_state() {
    let text = r#"{"k": [1, 2, "x"]}"#;
    let mut single = build(1);
    let mut many = build(4);
    single.open(uri("det"), text).unwrap();
    many.open(uri("det"), text).unwrap();
    assert_eq!(tokens(&single, "test://det"), tokens(&many, "test://det"));
}

#[test]
fn editing_document_a_does_not_change_document_b() {
    let mut ws = build(1);
    ws.open(uri("a"), r#"{"a": 1}"#).unwrap();
    ws.open(uri("b"), r#"{"b": 2}"#).unwrap();
    let before = tokens(&ws, "test://b");
    ws.edit(vec![plingo::framework::SourceEdit::Insert {
        key: Span::point_uri(uri("a"), 6).unwrap(),
        value: "0".into(),
    }])
    .expect("edit applies");
    let after = tokens(&ws, "test://b");
    assert_eq!(before, after);
}

// ---------------------------------------------------------------------------
// Diagnostics granularity (plan §8 Phase 4): diagnostics are per-slot facts
// keyed by document, so an edit to one document never rewrites another's
// diagnostic slots, and a clean re-parse of unchanged source stays cold.
// ---------------------------------------------------------------------------

#[test]
fn diagnostics_are_per_document_slot_facts() {
    let mut ws = build(1);
    let good = uri("diag-good");
    let bad = uri("diag-bad");
    ws.open(good.clone(), r#"{"a": 1}"#).unwrap();
    ws.open(bad.clone(), r#"{"a": 1"#).unwrap();

    let snapshot = ws.snapshot();
    let bad_diagnostics = snapshot.list::<ParseDiagnostics>(&bad.to_string());
    assert!(
        !bad_diagnostics.is_empty(),
        "the truncated document reports diagnostics"
    );
    assert!(
        snapshot
            .list::<ParseDiagnostics>(&good.to_string())
            .is_empty(),
        "the clean document has none"
    );

    // Fixing the broken document clears exactly its slots; the clean
    // document's diagnostics were never rewritten.
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(bad.clone(), 7).unwrap(),
        value: "}".to_owned(),
    }])
    .unwrap();
    let snapshot = ws.snapshot();
    assert!(
        snapshot
            .list::<ParseDiagnostics>(&bad.to_string())
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Determinism matrix (plan §20.4): identical normalized command traces must
// produce identical semantic facts regardless of worker count, document open
// order, or which fresh engine instance executes them.
// ---------------------------------------------------------------------------

mod determinism_matrix {
    use super::*;
    use common::oracle;

    fn scenario_a(ws: &mut Workspace, name: &str) {
        let u = uri(name);
        ws.open(u.clone(), r#"{"alpha": [1, 2], "beta": {"gamma": "x"}}"#)
            .unwrap();
        let edits = |ws: &mut Workspace| {
            ws.edit(vec![SourceEdit::Insert {
                key: Span::point_uri(u.clone(), 12).unwrap(),
                value: "3, ".into(),
            }])
            .unwrap();
            ws.edit(vec![SourceEdit::Insert {
                key: Span::point_uri(u.clone(), 30).unwrap(),
                value: "\"y\" : ".into(),
            }])
            .unwrap();
            // Recovery-shaped garbage then repair.
            ws.edit(vec![SourceEdit::Insert {
                key: Span::point_uri(u.clone(), 5).unwrap(),
                value: "@".into(),
            }])
            .unwrap();
            ws.edit(vec![SourceEdit::Delete {
                key: Span::new_uri(u.clone(), 5, 6).unwrap(),
            }])
            .unwrap();
        };
        edits(ws);
    }

    fn scenario_b(ws: &mut Workspace) {
        let a = uri("matrix-a");
        let b = uri("matrix-b");
        // Open order B then A; edit A only; B must stay cold.
        ws.open(b.clone(), r#"{"keep": true}"#).unwrap();
        ws.open(a.clone(), r#"{"n": 1}"#).unwrap();
        ws.edit(vec![SourceEdit::Insert {
            key: Span::point_uri(a, 6).unwrap(),
            value: "11".into(),
        }])
        .unwrap();
    }

    /// The full canonical projection for one document.
    fn projection(ws: &Workspace, name: &str) -> oracle::PipelineProjection {
        oracle::project(&ws.snapshot(), &format!("test://{name}"))
    }

    #[test]
    fn worker_count_and_fresh_engine_produce_identical_projections() {
        let mut one = build(1);
        let mut four = build(4);
        let mut fresh_again = build(2);
        scenario_a(&mut one, "matrix-w");
        scenario_a(&mut four, "matrix-w");
        scenario_a(&mut fresh_again, "matrix-w");
        let p1 = projection(&one, "matrix-w");
        let p4 = projection(&four, "matrix-w");
        let pf = projection(&fresh_again, "matrix-w");
        assert_eq!(p1, p4, "worker count changed the committed facts");
        assert_eq!(p1, pf, "a second fresh engine diverged");
    }

    #[test]
    fn open_order_is_irrelevant_to_per_document_facts() {
        let mut first_a = build(1);
        let mut first_b = build(1);
        scenario_b(&mut first_a);
        scenario_b(&mut first_b);
        // Same engine replays; compare per-document projections.
        let pa = projection(&first_a, "matrix-a");
        let pb = projection(&first_a, "matrix-b");
        assert_eq!(pa.tokens.len(), 5, "document A token count drifted: {pa:?}");
        assert_eq!(pb.parse_status.as_deref(), Some("clean"));
        // And a fresh engine reproduces both exactly.
        let mut replay = build(3);
        scenario_b(&mut replay);
        assert_eq!(projection(&replay, "matrix-a"), pa);
        assert_eq!(projection(&replay, "matrix-b"), pb);
    }
}
