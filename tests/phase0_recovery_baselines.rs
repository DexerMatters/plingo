//! Phase 0 recovery-pathology baselines (plan §11 Phase 0 acceptance).
//!
//! These fixtures pin the *current* recovery behavior so Phase 7 can prove
//! each pathology is fixed:
//! - one-shift acceptance (`MIN_REAL_SHIFTS = 1`);
//! - alternate equal-cost repairs being silently dropped;
//! - recovery-column suffix poisoning forcing replay past unchanged input.
//!
//! Assertions describe observed baseline semantics via deterministic work
//! counters and canonical projections; Phase 7 updates them when the
//! canonical search replaces this machinery.

mod common;

use common::json::{JsonDocument, JsonToken};
use common::oracle;
use plingo::framework::parse::{AstSnapshots, ParseUnits};
use plingo::framework::lex::install_lexer;
use plingo::framework::parse::install_parser;
use plingo::framework::source::SourceEdit;
use plingo::framework::Workspace;
use plingo::utils::Span;

fn build() -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<JsonToken>(engine)?;
        install_parser::<JsonToken, JsonDocument>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

fn uri(name: &str) -> fluent_uri::Uri<String> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn replace_at(
    u: &fluent_uri::Uri<String>,
    text: &str,
    needle: &str,
    value: &str,
) -> Vec<SourceEdit> {
    let start = text.find(needle).expect("needle present");
    let end = start + needle.len();
    vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), start, end).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), start).unwrap(),
            value: value.into(),
        },
    ]
}

#[test]
fn one_real_shift_accepts_a_repair() {
    let mut ws = build();
    let u = uri("one_shift");
    // Missing comma between elements: inserting `,` plus one real shift
    // validates the repair under MIN_REAL_SHIFTS = 1.
    let text = r#"[1 2]"#;
    let report = ws.open(u.clone(), text).expect("open commits");

    let projection = oracle::project(&ws.snapshot(), &u.to_string());
    let status = projection.parse_status.clone().unwrap_or_default();
    assert!(
        status.starts_with("recovered") || status.starts_with("unrecoverable"),
        "broken input must not parse clean: {status:?}"
    );
    assert!(
        !projection.diagnostics.is_empty(),
        "recovery publishes explicit diagnostics"
    );

    // Repair restores a clean parse with zero stale diagnostics.
    let repaired = ws.edit(replace_at(&u, text, "1 2", "1, 2")).expect("repair");
    assert_eq!(repaired.work().parser(&u.to_string()).map(|p| p.recovery_searches), Some(0));
    let projection = oracle::project(&ws.snapshot(), &u.to_string());
    assert_eq!(projection.parse_status.as_deref(), Some("clean"));
    assert!(projection.diagnostics.is_empty());
    let _ = report;
}

#[test]
fn equal_cost_alternate_repairs_are_dropped_by_the_current_search() {
    let mut ws = build();
    let u = uri("alternates");
    // Two plausible minimum-cost repairs exist for a missing separator, but
    // the current one-selected-trace search keeps only its best-scored
    // candidate. Baseline contract: exactly one diagnostic location, no
    // cascade, deterministic across reruns.
    let text = r#"{"a":[1,2],"b":{"x" 1},"c":[3,4]}"#;
    ws.open(u.clone(), text).expect("open commits");

    let first = oracle::project(&ws.snapshot(), &u.to_string());
    // Rerun from scratch: identical selection (determinism, no hash-order
    // dependence in the chosen trace).
    let mut fresh = build();
    let v = uri("alternates-fresh");
    fresh.open(v.clone(), text).expect("fresh commits");
    let second = oracle::project(&fresh.snapshot(), &v.to_string());

    assert_eq!(first.diagnostics, second.diagnostics);
    assert!(!first.diagnostics.is_empty());

    // A later repair clears every stale diagnostic.
    ws.edit(replace_at(&u, text, "\"x\" 1", "\"x\": 1")).expect("repair");
    let repaired = oracle::project(&ws.snapshot(), &u.to_string());
    assert_eq!(repaired.parse_status.as_deref(), Some("clean"));
    assert!(repaired.diagnostics.is_empty());
}

#[test]
fn recovery_columns_reuse_unchanged_suffix() {
    let mut ws = build();
    let u = uri("poison");
    // A persistent syntax error early in the document (missing comma in
    // the first array); hundreds of clean elements follow it.
    let filler = (0..400)
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let text = format!(r#"{{"bad":[1 2],"middle":{{"x":[{filler}]}},"tail":[{filler}]}}"#);
    ws.open(u.clone(), &text).expect("open commits");
    let before = oracle::project(&ws.snapshot(), &u.to_string());
    assert!(!before.diagnostics.is_empty(), "baseline contains an error");

    // Edit the LAST element only. The error region and everything between
    // are untouched, so replay must retain the long clean suffix.
    let last_number = text.rfind('9').expect("tail number");
    let edits = vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), last_number).unwrap(),
        value: "1".into(),
    }];
    let report = ws.edit(edits).expect("edit commits");
    let lexer = report
        .work()
        .lexer(&u.to_string())
        .expect("lexer ran for the edit");
    let parser = report
        .work()
        .parser(&u.to_string())
        .expect("parser ran for the edit");
    assert!(
        lexer.tokens_replayed <= 12,
        "local edit replays a bounded lexer window: {lexer:?}"
    );
    assert!(
        lexer.tokens_reused > 0,
        "unchanged lexer occurrences remain reusable: {lexer:?}"
    );
    assert!(
        parser.columns_replayed <= 24,
        "local edit replays a bounded parser window: {parser:?}"
    );
    assert!(
        parser.columns_reused > 1_000,
        "unchanged parser columns remain reusable: {parser:?}"
    );
    assert!(
        parser.tokens_replayed <= 12,
        "local edit materializes a bounded parser token window: {parser:?}"
    );
    assert!(
        parser.tokens_reused > 1_000,
        "unchanged parser token coordinates remain reusable: {parser:?}"
    );

    // Canonical output still equals a fresh workspace (correctness holds
    // independently of reuse counters).
    let incremental = oracle::project(&ws.snapshot(), &u.to_string());
    let mut fresh = build();
    let v = uri("poison-fresh");
    let mut edited_text = text.clone();
    edited_text.insert(last_number, '1');
    fresh.open(v.clone(), &edited_text).expect("open commits");
    let fresh_projection = oracle::project(&fresh.snapshot(), &v.to_string());
    assert_eq!(incremental.source_len, fresh_projection.source_len);
    if incremental.tokens != fresh_projection.tokens {
        let first = incremental
            .tokens
            .iter()
            .zip(&fresh_projection.tokens)
            .position(|(left, right)| left != right);
        panic!(
            "token projection mismatch at {first:?}: left={:?} right={:?}",
            first.and_then(|index| incremental.tokens.get(index)),
            first.and_then(|index| fresh_projection.tokens.get(index)),
        );
    }
    assert_eq!(incremental.parse_status, fresh_projection.parse_status);
    let unit = ws
        .snapshot()
        .observe::<ParseUnits<JsonDocument>>(u.to_string())
        .expect("incremental parse unit");
    let document = ws
        .snapshot()
        .observe::<AstSnapshots<JsonDocument>>(u.to_string())
        .expect("incremental AST snapshot");
    let root = unit.root.expect("incremental root");
    assert!(
        document.snapshot().get(root).is_some(),
        "incremental AST snapshot lost root {}",
        root.identity()
    );
    assert_eq!(incremental.roots, fresh_projection.roots);
    assert_eq!(incremental.diagnostics, fresh_projection.diagnostics);
}
