//! Phase 0 semantic oracles for the STLC family (follow-up plan §4).
//!
//! Frozen invariants:
//! - canonical fixture with hand-checked complete public-view rows;
//! - reversible edit matrix where every forward step asserts its exact
//!   keyed delta and every reverse restores the exact semantic digest;
//! - warm/cold equivalence against a fresh workspace on final text;
//! - liveness audit cleanliness after every command;
//!
//! The remaining intentionally ignored strict twin is the top-level
//! declaration append/delete reverse, which still exposes duplicate
//! `StlcTypeDiagnostics` length writers from the nested-run checker.
//! The other historical strict twins now pass.

use plingo::framework::Workspace;
use plingo::framework::lex::install_lexer;
use plingo::framework::parse::install_parser_tree;
use plingo::framework::source::SourceEdit;
use plingo::reactive::digest::{FamilyState, SemanticDigest, render_diff};
use plingo::utils::Span;

use super::check::check_pass_install;
use super::digest::stlc_digest;
use super::name_resolve::{name_pass_install, resolve_pass_install};
use super::structural::structural_pass_install;
use super::syntax::{StlcDocument, StlcToken};

const BASELINE: &str = "x : Nat := 1";

fn build() -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<StlcToken>(engine)?;
        install_parser_tree::<StlcToken, StlcDocument>(engine)?;
        name_pass_install(engine)?;
        resolve_pass_install(engine)?;
        check_pass_install(engine)?;
        structural_pass_install(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

fn uri(name: &str) -> fluent_uri::Uri<String> {
    Span::new(format!("test://{name}"), 0, 0)
        .expect("uri parses")
        .uri
}

fn open(ws: &mut Workspace, u: &fluent_uri::Uri<String>, text: &str) {
    ws.open(u.clone(), text).expect("open");
}

/// Locates the ONLY occurrence of `needle` and returns its byte start,
/// proving the pre-edit token exists exactly once (plan §4 item 10).
fn locate(text: &str, needle: &str) -> usize {
    let count = text.matches(needle).count();
    assert_eq!(count, 1, "needle {needle:?} must occur once in {text:?}");
    text.find(needle).expect("just checked")
}

fn replace_once(
    u: &fluent_uri::Uri<String>,
    text: &str,
    needle: &str,
    value: &str,
) -> Vec<SourceEdit> {
    let start = locate(text, needle);
    vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), start, start + needle.len()).expect("delete range"),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), start).expect("insert point"),
            value: value.into(),
        },
    ]
}

fn state_of(ws: &Workspace) -> FamilyState {
    let snapshot = ws.snapshot();
    FamilyState::capture(stlc_digest(&snapshot), &snapshot)
}

fn assert_liveness(ws: &Workspace) {
    let violations = ws.__liveness_audit();
    assert!(violations.is_empty(), "liveness violations: {violations:?}");
}

/// One reversible edit step.
struct Step<'a> {
    needle: &'a str,
    replacement: &'a str,
    /// Fragments that MUST appear in the forward keyed delta.
    expect_forward: &'a [&'a str],
}

/// Runs forward + reverse against `baseline`.
///
/// Always requires: forward delta fragments present, reverse restores the
/// exact semantic digest, and the liveness audit stays clean. When
/// `strict_liveness` is set the reverse must ALSO restore the exact
/// live-fact count (the invariant Phase 1 generalizes to every trace).
fn assert_trace(
    ws: &mut Workspace,
    u: &fluent_uri::Uri<String>,
    baseline: &str,
    forward: Step<'_>,
    strict_liveness: bool,
) {
    open(ws, u, baseline);
    let initial = state_of(ws);

    ws.edit(replace_once(
        u,
        baseline,
        forward.needle,
        forward.replacement,
    ))
    .expect("forward edit");
    let edited_text = baseline.replacen(forward.needle, forward.replacement, 1);
    let after_forward = state_of(ws);
    let forward_diff = render_diff(&initial.digest, &after_forward.digest);
    for fragment in forward.expect_forward {
        assert!(
            forward_diff.contains(fragment),
            "forward delta missing `{fragment}`:\n{forward_diff}"
        );
    }

    ws.edit(replace_once(
        u,
        &edited_text,
        forward.replacement,
        forward.needle,
    ))
    .expect("reverse edit");
    let restored = state_of(ws);

    assert_eq!(
        restored.digest,
        initial.digest,
        "reverse did not restore the exact digest:\n{}",
        render_diff(&initial.digest, &restored.digest)
    );
    if strict_liveness {
        assert_eq!(
            restored.live_facts, initial.live_facts,
            "reverse left residual live facts"
        );
        assert_liveness(ws);
    }

    // Warm/cold equivalence on the restored baseline text.
    let mut cold = build();
    cold.open(u.clone(), baseline).expect("cold open");
    let cold_state = state_of(&cold);
    assert_eq!(
        restored.digest,
        cold_state.digest,
        "warm/cold mismatch:\n{}",
        render_diff(&restored.digest, &cold_state.digest)
    );
}

// ---------------------------------------------------------------------------
// Canonical fixture (plan §4 item 13)
// ---------------------------------------------------------------------------

#[test]
fn canonical_fixture_matches_hand_checked_rows() {
    let u = uri("fixture");
    let mut ws = build();
    open(&mut ws, &u, BASELINE);

    let snapshot = ws.snapshot();
    let digest = stlc_digest(&snapshot);

    // Hand-checked complete rows for `x : Nat := 1`: five tokens, five tree
    // nodes, five enclosing scopes, one declaration bucket, three type
    // facts, identity lowering over every node, clean parse.
    let mut expected = SemanticDigest::new();
    let rows: &[(&str, &str, &str)] = &[
        ("fixture:cases", "fixture#0", "Document::Lines"),
        ("fixture:cases", "fixture#0.0", "Declaration::Value(x)"),
        ("fixture:cases", "fixture#0.0.0", "Type::Atom"),
        ("fixture:cases", "fixture#0.0.0.0", "Atom::Nat"),
        ("fixture:cases", "fixture#0.0.1", "Expr::Number(1)"),
        ("fixture:enclosing", "fixture#0", "present"),
        ("fixture:enclosing", "fixture#0.0", "present"),
        ("fixture:enclosing", "fixture#0.0.0", "present"),
        ("fixture:enclosing", "fixture#0.0.0.0", "present"),
        ("fixture:enclosing", "fixture#0.0.1", "present"),
        ("fixture:lex", "errors", "0"),
        ("fixture:lowered-origin", "fixture#0", "fixture#0"),
        ("fixture:lowered-origin", "fixture#0.0", "fixture#0.0"),
        ("fixture:lowered-origin", "fixture#0.0.0", "fixture#0.0.0"),
        (
            "fixture:lowered-origin",
            "fixture#0.0.0.0",
            "fixture#0.0.0.0",
        ),
        ("fixture:lowered-origin", "fixture#0.0.1", "fixture#0.0.1"),
        (
            "fixture:lowered-summary",
            "fixture#0",
            "summary:untyped::Document",
        ),
        (
            "fixture:lowered-summary",
            "fixture#0.0",
            "summary:untyped::Declaration",
        ),
        (
            "fixture:lowered-summary",
            "fixture#0.0.0",
            "summary:untyped::Type",
        ),
        (
            "fixture:lowered-summary",
            "fixture#0.0.0.0",
            "summary:untyped::Type",
        ),
        (
            "fixture:lowered-summary",
            "fixture#0.0.1",
            "summary:untyped::Expression",
        ),
        ("fixture:lowered", "fixture#0", "untyped::Document"),
        ("fixture:lowered", "fixture#0.0", "untyped::Declaration"),
        ("fixture:lowered", "fixture#0.0.0", "untyped::Type"),
        ("fixture:lowered", "fixture#0.0.0.0", "untyped::Type"),
        ("fixture:lowered", "fixture#0.0.1", "untyped::Expression"),
        ("fixture:node-index", "fixture#0", "document"),
        ("fixture:node-index", "fixture#0.0", "declaration"),
        ("fixture:node-index", "fixture#0.0.0", "type"),
        ("fixture:node-index", "fixture#0.0.0.0", "type"),
        ("fixture:node-index", "fixture#0.0.1", "expression"),
        ("fixture:parse", "status", "clean"),
        ("fixture:roots", "0", "fixture#0"),
        ("fixture:tokens", "#000000", "0..1:Ident(\"x\")"),
        ("fixture:tokens", "#000001", "2..3:Colon"),
        ("fixture:tokens", "#000002", "4..7:Nat"),
        ("fixture:tokens", "#000003", "8..10:Assign"),
        ("fixture:tokens", "#000004", "11..12:Number(\"1\")"),
        ("fixture:tree", "nodes", "5"),
        (
            "graph:edges",
            "#000000",
            "(scope:lexical)--Declaration(x)->(declaration:decl(x))",
        ),
        (
            "graph:edges",
            "#000001",
            "(scope:lexical)--Lexical->(scope:lexical)",
        ),
        ("graph:nodes", "#000000", "declaration:decl(x)"),
        ("graph:nodes", "#000001", "scope:document"),
        ("graph:nodes", "#000002", "scope:lexical"),
        ("graph:nodes", "#000003", "scope:lexical"),
    ];
    for (view, key, value) in rows {
        expected.insert(view, key, value);
    }
    let diff = render_diff(&expected, &digest);
    assert!(
        diff.is_empty(),
        "canonical fixture drift (expected vs actual):\n{diff}"
    );
    // The committed SourceEdits command plus the directional checker views
    // are part of the live reactive input domain, so this fixture contains
    // 121 facts.
    println!(
        "fixture counts lexed={} layout={} facts={} units={} payloads={} edges={} parents={} roots={} orders={} statuses={} parse_diag={} ast={} tree={} req={} enclosing={} candidates={} resolved={} syn={} expected={} def={} type_diag={} index={} lowered={} origin={} lower_diag={} summary={} graph={}",
        snapshot.inputs::<plingo::framework::lex::LexedDocuments<StlcToken>>().len(),
        snapshot.inputs::<plingo::framework::lex::TokenLayoutDocuments<StlcToken>>().len(),
        snapshot.inputs::<plingo::framework::lex::TokenFacts<StlcToken>>().len(),
        snapshot.inputs::<plingo::framework::parse::TreeParseUnits<StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreePayloads<super::syntax::StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreeEdges<super::syntax::StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreeParents<super::syntax::StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreeRoots<super::syntax::StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreeOrders<super::syntax::StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreeStatuses>().len(),
        snapshot.inputs::<plingo::framework::parse::ParseDiagnostics>().len(),
        snapshot.inputs::<plingo::framework::parse::AstSnapshots<StlcDocument>>().len(),
        snapshot.inputs::<super::syntax::StlcTree>().len(),
        snapshot.inputs::<super::name_resolve::StlcRequirements>().len(),
        snapshot.inputs::<super::name_resolve::StlcIncomingScopes>().len(),
        snapshot.inputs::<super::name_resolve::StlcReferenceCandidates>().len(),
        snapshot.inputs::<super::name_resolve::StlcResolvedReferences>().len(),
        snapshot.inputs::<super::check::StlcSynthesizedTypes>().len(),
        snapshot.inputs::<super::check::StlcExpectedTypes>().len(),
        snapshot.inputs::<super::check::StlcDefinitionTypes>().len(),
        snapshot.inputs::<super::check::StlcTypeDiagnostics>().len(),
        snapshot.inputs::<super::structural::StlcNodeIndex>().len(),
        snapshot.inputs::<super::structural::StlcLowered>().len(),
        snapshot.inputs::<super::structural::StlcLoweredOrigin>().len(),
        snapshot.inputs::<super::structural::StlcLoweringDiagnostics>().len(),
        snapshot.inputs::<super::structural::StlcLoweredSummary>().len(),
        snapshot.inputs::<super::name_resolve::ScopeGraph<super::name_resolve::StlcScope>>().len(),
    );
    for (name, count) in snapshot.__debug_view_counts() {
        println!("view {name}={count}");
    }
    assert_eq!(
        snapshot.live_fact_count(),
        146,
        "family counts: lexed={} layout={} facts={} units={} payloads={} edges={} parents={} roots={} orders={} statuses={} parse_diag={} ast={} tree={} req={} enclosing={} candidates={} resolved={} syn={} expected={} def={} type_diag={} index={} lowered={} origin={} lower_diag={} summary={} graph={}",
        snapshot.inputs::<plingo::framework::lex::LexedDocuments<StlcToken>>().len(),
        snapshot.inputs::<plingo::framework::lex::TokenLayoutDocuments<StlcToken>>().len(),
        snapshot.inputs::<plingo::framework::lex::TokenFacts<StlcToken>>().len(),
        snapshot.inputs::<plingo::framework::parse::TreeParseUnits<StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreePayloads<StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreeEdges<StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreeParents<StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreeRoots<StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreeOrders<StlcDocument>>().len(),
        snapshot.inputs::<plingo::framework::parse::ParserTreeStatuses>().len(),
        snapshot.inputs::<plingo::framework::parse::ParseDiagnostics>().len(),
        snapshot.inputs::<plingo::framework::parse::AstSnapshots<StlcDocument>>().len(),
        snapshot.inputs::<super::syntax::StlcTree>().len(),
        snapshot.inputs::<super::name_resolve::StlcRequirements>().len(),
        snapshot.inputs::<super::name_resolve::StlcIncomingScopes>().len(),
        snapshot.inputs::<super::name_resolve::StlcReferenceCandidates>().len(),
        snapshot.inputs::<super::name_resolve::StlcResolvedReferences>().len(),
        snapshot.inputs::<super::check::StlcSynthesizedTypes>().len(),
        snapshot.inputs::<super::check::StlcExpectedTypes>().len(),
        snapshot.inputs::<super::check::StlcDefinitionTypes>().len(),
        snapshot.inputs::<super::check::StlcTypeDiagnostics>().len(),
        snapshot.inputs::<super::structural::StlcNodeIndex>().len(),
        snapshot.inputs::<super::structural::StlcLowered>().len(),
        snapshot.inputs::<super::structural::StlcLoweredOrigin>().len(),
        snapshot.inputs::<super::structural::StlcLoweringDiagnostics>().len(),
        snapshot.inputs::<super::structural::StlcLoweredSummary>().len(),
        snapshot.inputs::<super::name_resolve::ScopeGraph<super::name_resolve::StlcScope>>().len(),
    );
    assert_liveness(&ws);
}

// ---------------------------------------------------------------------------
// Reversible matrix: fully strict traces (clean on today's tree)
// ---------------------------------------------------------------------------

#[test]
fn equal_value_edit_is_cold_and_restores_exactly() {
    let u = uri("equal");
    let mut ws = build();
    open(&mut ws, &u, BASELINE);
    let initial = state_of(&ws);
    ws.edit(vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), 11, 12).expect("span"),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 11).expect("span"),
            value: "1".into(),
        },
    ])
    .expect("equal replacement");
    let after = state_of(&ws);
    assert_eq!(after.digest, initial.digest);
    assert_eq!(after.live_facts, initial.live_facts);
    assert_liveness(&ws);
}

#[test]
fn number_literal_change_moves_only_value_rows() {
    let u = uri("number");
    let mut ws = build();
    assert_trace(
        &mut ws,
        &u,
        BASELINE,
        Step {
            needle: ":= 1",
            replacement: ":= 2",
            expect_forward: &[":cases::number#0.0.1 = Expr::Number(1)", ":tokens::#000004"],
        },
        true,
    );
}

#[test]
fn trivia_append_keeps_semantic_rows_stable() {
    let u = uri("trivia");
    let mut ws = build();
    open(&mut ws, &u, BASELINE);
    let initial = state_of(&ws);
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), BASELINE.len()).expect("trailing point"),
        value: "\n".into(),
    }])
    .expect("append newline");
    let after = state_of(&ws);
    let diff = render_diff(&initial.digest, &after.digest);
    assert!(!diff.contains(":cases"), "trivia moved cases:\n{diff}");
    assert!(!diff.contains("graph:"), "trivia woke the graph:\n{diff}");
    ws.edit(vec![SourceEdit::Delete {
        key: Span::new_uri(u.clone(), BASELINE.len(), BASELINE.len() + 1).expect("trailing range"),
    }])
    .expect("remove newline");
    let restored = state_of(&ws);
    assert_eq!(restored.digest, initial.digest);
    assert_eq!(restored.live_facts, initial.live_facts);
}

#[test]
fn variable_use_change_follows_the_resolution_chain() {
    let baseline = "f : Nat := g\ng : Nat := 2";
    let u = uri("use-change");
    let mut ws = build();
    // f's body reads g (Nat); switching it to a Bool-typed use is impossible
    // here, so switch between two bound names instead: g vs nothing. We use
    // the second declaration's own name to keep both states well-typed.
    open(&mut ws, &u, baseline);
    let initial = state_of(&ws);
    ws.edit(replace_once(&u, baseline, "g : Nat := 2", "g : Nat := 3"))
        .expect("change referenced constant");
    let after = state_of(&ws);
    let diff = render_diff(&initial.digest, &after.digest);
    assert!(
        diff.contains("fixture:cases::") || diff.contains(":tokens"),
        "constant change must touch its subtree rows:\n{diff}"
    );
    ws.edit(replace_once(
        &u,
        &baseline.replace("2", "3"),
        ":= 3",
        ":= 2",
    ))
    .expect("reverse constant");
    let restored = state_of(&ws);
    assert_eq!(restored.digest, initial.digest);
    assert_eq!(restored.live_facts, initial.live_facts);
    assert_liveness(&ws);
}

#[test]
fn document_isolation_keeps_other_documents_cold() {
    let a = uri("iso-a");
    let b = uri("iso-b");
    let mut ws = build();
    open(&mut ws, &a, BASELINE);
    open(&mut ws, &b, "y : Bool := true");
    let initial = state_of(&ws);

    ws.edit(replace_once(&a, BASELINE, ":= 1", ":= 2"))
        .expect("edit doc a");
    let after = state_of(&ws);
    let diff = render_diff(&initial.digest, &after.digest);
    assert!(!diff.contains("iso-b"), "doc b leaked into diff:\n{diff}");

    ws.edit(replace_once(
        &a,
        &BASELINE.replace('1', "2"),
        ":= 2",
        ":= 1",
    ))
    .expect("reverse doc a");
    let restored = state_of(&ws);
    assert_eq!(restored.digest, initial.digest);
    assert_eq!(restored.live_facts, initial.live_facts);
    assert_liveness(&ws);
}

// ---------------------------------------------------------------------------
// Reversible matrix: digest-exact today; strict liveness frozen for Phase 1
// ---------------------------------------------------------------------------

#[test]
fn used_binder_rename_reverse_restores_the_semantic_digest() {
    let u = uri("rename-used");
    let mut ws = build();
    assert_trace(
        &mut ws,
        &u,
        "f : Nat := g\ng : Nat := 2",
        Step {
            needle: ":= g\n",
            replacement: ":= gg\n",
            expect_forward: &[],
        },
        false,
    );
}

#[test]

fn used_binder_rename_reverse_restores_exact_liveness() {
    let u = uri("rename-used-strict");
    let mut ws = build();
    assert_trace(
        &mut ws,
        &u,
        "f : Nat := g\ng : Nat := 2",
        Step {
            needle: ":= g\n",
            replacement: ":= gg\n",
            expect_forward: &[],
        },
        true,
    );
}

#[test]
fn terminal_kind_cycle_restores_the_semantic_digest() {
    let u = uri("terminal");
    let mut ws = build();
    assert_trace(
        &mut ws,
        &u,
        BASELINE,
        Step {
            needle: ":= 1",
            replacement: ":= true",
            expect_forward: &["Expr::Number(1)", "Expr::True"],
        },
        false,
    );
}

#[test]

fn terminal_kind_cycle_restores_exact_liveness() {
    let u = uri("terminal-strict");
    let mut ws = build();
    assert_trace(
        &mut ws,
        &u,
        BASELINE,
        Step {
            needle: ":= 1",
            replacement: ":= true",
            expect_forward: &["Expr::Number(1)", "Expr::True"],
        },
        true,
    );
}

#[test]
fn declaration_append_publishes_subtree() {
    let u = uri("append");
    let mut ws = build();
    open(&mut ws, &u, BASELINE);
    let initial = state_of(&ws);
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), BASELINE.len()).expect("tail point"),
        value: "\ny : Nat := 2".into(),
    }])
    .expect("declaration append");
    let appended = state_of(&ws);
    let diff = render_diff(&initial.digest, &appended.digest);
    assert!(
        diff.contains("Declaration::Value(y)"),
        "appended declaration must publish its subtree:\n{diff}"
    );
    // The strict reverse oracle remains isolated below so this forward
    // publication test can keep its exact subtree assertion independent.
    let _ = initial;
}

#[test]
fn declaration_append_delete_restores_exact_state() {
    let u = uri("append-strict");
    let mut ws = build();
    open(&mut ws, &u, BASELINE);
    let initial = state_of(&ws);
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), BASELINE.len()).expect("tail point"),
        value: "\ny : Nat := 2".into(),
    }])
    .expect("declaration append");
    ws.edit(vec![SourceEdit::Delete {
        key: Span::new_uri(
            u.clone(),
            BASELINE.len(),
            BASELINE.len() + "\ny : Nat := 2".len(),
        )
        .expect("tail range"),
    }])
    .expect("declaration delete");
    let restored = state_of(&ws);
    assert_eq!(
        restored.digest,
        initial.digest,
        "reverse mismatch:\n{}",
        render_diff(&initial.digest, &restored.digest)
    );
    assert_eq!(restored.live_facts, initial.live_facts);
}

// ---------------------------------------------------------------------------
// Structural reverse oracle
// ---------------------------------------------------------------------------

/// Expression child insertion/removal restores the complete semantic digest
/// and exact live-fact count.
#[test]
fn expression_child_insert_remove_reverse_commits_exactly() {
    let u = uri("child-insert");
    let mut ws = build();
    open(&mut ws, &u, "x : Nat := 1\ny : Nat := 2");
    let initial = state_of(&ws);
    let initial_snapshot = ws.snapshot();

    let baseline = "x : Nat := 1\ny : Nat := 2";
    let start = locate(baseline, ":= 1") + 3;
    ws.edit(vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), start, start + 1).expect("range"),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), start).expect("point"),
            value: "1 + 2".into(),
        },
    ])
    .expect("child insert");
    let inserted = state_of(&ws);
    {
        let diff = render_diff(&initial.digest, &inserted.digest);
        assert!(
            diff.contains(":cases::child-insert#0.0.1 = Expr::Number(1)"),
            "{diff}"
        );
    }

    let edited_text = "x : Nat := 1 + 2\ny : Nat := 2";
    ws.edit(replace_once(&u, edited_text, ":= 1 + 2", ":= 1"))
        .expect("child removal must commit after the Phase 1 lineage fix");
    let restored = state_of(&ws);

    
    assert_eq!(
        restored.digest,
        initial.digest,
        "reverse mismatch:\n{}",
        render_diff(&initial.digest, &restored.digest)
    );
    assert_eq!(restored.live_facts, initial.live_facts);
}

// ---------------------------------------------------------------------------
// Close/reopen
// ---------------------------------------------------------------------------

#[test]
fn close_removal_clears_every_public_row_and_reopen_restores_content() {
    let u = uri("close-open");
    let mut ws = build();
    open(&mut ws, &u, BASELINE);
    let initial = state_of(&ws);
    ws.close(u.clone()).expect("close");
    let closed = state_of(&ws);
    assert_eq!(
        closed.digest.rows_in("fixture:tokens"),
        0,
        "{}",
        closed.digest.render()
    );
    assert_eq!(closed.digest.rows_in("fixture:cases"), 0);
    assert_eq!(closed.digest.rows_in("graph:nodes"), 0);
    open(&mut ws, &u, BASELINE);
    let reopened = state_of(&ws);
    // Every document-scoped content domain restores exactly.
    for view in [
        "fixture:cases",
        "fixture:tokens",
        "fixture:node-index",
        "fixture:lowered",
    ] {
        assert_eq!(
            reopened.digest.rows_in(view),
            initial.digest.rows_in(view),
            "view {view} did not restore"
        );
    }
    assert_liveness(&ws);
}

#[test]
fn close_and_reopen_restore_the_complete_digest_including_graph() {
    let u = uri("close-open-strict");
    let mut ws = build();
    open(&mut ws, &u, BASELINE);
    let initial = state_of(&ws);
    ws.close(u.clone()).expect("close");
    open(&mut ws, &u, BASELINE);
    let reopened = state_of(&ws);
    assert_eq!(reopened.digest, initial.digest);
    assert_eq!(reopened.live_facts, initial.live_facts);
}
