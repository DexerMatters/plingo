//! End-to-end tests for parser-backed recursive abstract-tree lowering.

use fluent_uri::Uri;
use plingo::framework::parse::{AstSnapshots, ParseStatus, ParserTreeStatuses};
use plingo::framework::source::SourceEdit;
use plingo::framework::workspace::Workspace;
use plingo::reactive::Snapshot;
use plingo::reactive::abstract_tree::AstBox;
use plingo::reactive::digest::{FamilyState, render_diff};
use plingo::utils::Span;

use super::lower::{
    LoweredDeclaration, LoweredDocument, LoweredExpr, LoweredTree, semantic_digest,
};
use super::syntax::{TransformDocument, TransformToken, TransformTree};

fn uri(name: &str) -> Uri<String> {
    Span::new(format!("test://tree-transform/{name}"), 0, 0)
        .expect("URI span")
        .uri
}

fn build() -> Workspace {
    Workspace::builder()
        .lexer::<TransformToken>()
        .parser::<TransformDocument>()
        .mount::<super::lower::lower_document::Component, _>(TransformDocument::roots())
        .build()
        .expect("workspace builds")
}

fn lowered_roots(snapshot: &Snapshot, document: &Uri<String>) -> Vec<AstBox<LoweredDocument>> {
    snapshot
        .tree::<LoweredTree>()
        .roots(&document.to_string())
        .collect()
}

fn render_expr(snapshot: &Snapshot, node: AstBox<LoweredExpr>) -> String {
    let tree = snapshot.tree::<LoweredTree>();
    match tree.materialize(node).expect("lowered expression") {
        LoweredExpr::Add { left, right } => {
            format!(
                "Sum({}, {})",
                render_expr(snapshot, left),
                render_expr(snapshot, right)
            )
        }
        LoweredExpr::Subtract { left, right } => format!(
            "Difference({}, {})",
            render_expr(snapshot, left),
            render_expr(snapshot, right)
        ),
        LoweredExpr::Group { expression } => {
            format!("Group({})", render_expr(snapshot, expression))
        }
        LoweredExpr::Number => "Number".to_owned(),
        LoweredExpr::Name => "Name".to_owned(),
        LoweredExpr::Error { .. } => "ParseError".to_owned(),
    }
}

fn render_declaration(snapshot: &Snapshot, node: AstBox<LoweredDeclaration>) -> String {
    let tree = snapshot.tree::<LoweredTree>();
    match tree.materialize(node).expect("lowered declaration") {
        LoweredDeclaration::Binding { value } => {
            format!("Binding({})", render_expr(snapshot, value))
        }
        LoweredDeclaration::Error { .. } => "ParseError".to_owned(),
    }
}

fn render(snapshot: &Snapshot, node: AstBox<LoweredDocument>) -> String {
    let tree = snapshot.tree::<LoweredTree>();
    match tree.materialize(node).expect("lowered document") {
        LoweredDocument::Module { declarations } => format!(
            "Module({})",
            declarations
                .into_iter()
                .map(|declaration| render_declaration(snapshot, declaration))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        LoweredDocument::Error { .. } => "ParseError".to_owned(),
    }
}

fn state_of(ws: &Workspace) -> FamilyState {
    let snapshot = ws.snapshot();
    FamilyState::capture(semantic_digest(&snapshot), &snapshot)
}

#[test]
fn parser_tree_lowers_to_a_distinct_heterogeneous_tree() {
    let mut ws = build();
    let document = uri("shape");
    ws.open(document.clone(), "let x = 1 + (2 - y); let y = 3;")
        .expect("source opens");

    let snapshot = ws.snapshot();
    assert_eq!(
        *snapshot
            .observe::<ParserTreeStatuses>(document.to_string())
            .expect("parser status"),
        ParseStatus::Clean
    );
    let roots = lowered_roots(&snapshot, &document);
    assert_eq!(roots.len(), 1);
    assert_eq!(
        render(&snapshot, roots[0].clone()),
        "Module(Binding(Sum(Number, Group(Difference(Number, Name)))), Binding(Number))"
    );

    let source_root = snapshot
        .tree::<TransformTree>()
        .roots(&document.to_string())
        .next()
        .expect("parser root");
    assert!(
        snapshot
            .observe::<AstSnapshots<TransformDocument>>(document.to_string())
            .is_some(),
        "parser snapshot remains available for source resolution"
    );
}

#[test]
fn payload_dependency_updates_one_lowered_document_without_touching_siblings() {
    let mut ws = build();
    let a = uri("payload-a");
    let b = uri("payload-b");
    ws.open(a.clone(), "let x = 1 + 2; let y = 3;")
        .expect("document A opens");
    ws.open(b.clone(), "let kept = 9;")
        .expect("document B opens");

    let before = ws.snapshot();
    let a_root = lowered_roots(&before, &a)[0].clone();
    let b_root = lowered_roots(&before, &b)[0].clone();
    let b_render = render(&before, b_root.clone());

    let plus = "let x = 1 + 2; let y = 3;".find('+').expect("plus token");
    ws.edit(vec![
        SourceEdit::Delete {
            key: Span::new_uri(a.clone(), plus, plus + 1).expect("plus range"),
        },
        SourceEdit::Insert {
            key: Span::point_uri(a.clone(), plus).expect("minus point"),
            value: "-".into(),
        },
    ])
    .expect("operator edit commits");

    let after = ws.snapshot();
    assert_eq!(lowered_roots(&after, &a)[0], a_root);
    assert_eq!(
        render(&after, a_root.clone()),
        "Module(Binding(Difference(Number, Number)), Binding(Number))"
    );
    assert_eq!(lowered_roots(&after, &b), vec![b_root.clone()]);
    assert_eq!(render(&after, b_root), b_render);
}

#[test]
fn payload_edit_keeps_target_topology_and_has_no_broad_enumeration() {
    let mut ws = build();
    let document = uri("reaction");
    let text = "let x = 1 + 2;";
    ws.open(document.clone(), text).expect("source opens");
    let root = lowered_roots(&ws.snapshot(), &document)[0].clone();
    let two = text.find('2').expect("number two");
    let report = ws
        .edit(vec![
            SourceEdit::Delete {
                key: Span::new_uri(document.clone(), two, two + 1).expect("two range"),
            },
            SourceEdit::Insert {
                key: Span::point_uri(document.clone(), two).expect("two point"),
                value: "42".into(),
            },
        ])
        .expect("number edit commits");
    let reaction = report
        .command()
        .metric::<plingo::reactive::ReactionDigest>()
        .expect("reaction digest");
    assert!(reaction.broad_enumerations.is_empty(), "{reaction:#?}");
    let snapshot = ws.snapshot();
    assert_eq!(lowered_roots(&snapshot, &document), vec![root.clone()]);
    assert_eq!(
        render(&snapshot, root),
        "Module(Binding(Sum(Number, Number)))"
    );
}

#[test]
fn child_order_dependency_splices_only_the_affected_lowered_branch() {
    let mut ws = build();
    let document = uri("topology");
    let text = "let x = 1;";
    ws.open(document.clone(), text).expect("source opens");
    let before = ws.snapshot();
    let root = lowered_roots(&before, &document)[0].clone();
    assert_eq!(render(&before, root.clone()), "Module(Binding(Number))");

    let insertion = text.find(';').expect("semicolon");
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(document.clone(), insertion).expect("insertion point"),
        value: " + 2".into(),
    }])
    .expect("expression extension commits");
    let after = ws.snapshot();
    assert_eq!(lowered_roots(&after, &document), vec![root.clone()]);
    assert_eq!(render(&after, root), "Module(Binding(Sum(Number, Number)))");
}

#[test]
fn closing_a_document_retracts_the_transformed_forest() {
    let mut ws = build();
    let document = uri("close");
    ws.open(document.clone(), "let x = 1;")
        .expect("source opens");
    assert_eq!(lowered_roots(&ws.snapshot(), &document).len(), 1);
    ws.close(document.clone()).expect("close commits");
    assert!(lowered_roots(&ws.snapshot(), &document).is_empty());
}

const CANON_TEXT: &str = "let x = 1 + (2 - y); let y = 3;";

fn locate(document: &Uri<String>, haystack: &str, needle: &str) -> Span {
    let start = haystack
        .find(needle)
        .unwrap_or_else(|| panic!("needle {needle:?} absent from {haystack:?}"));
    assert!(
        !haystack[start + 1..].contains(needle),
        "needle {needle:?} is ambiguous in {haystack:?}"
    );
    Span::new_uri(document.clone(), start, start + needle.len()).expect("needle span")
}

fn replace_once(
    ws: &mut Workspace,
    document: &Uri<String>,
    text: &mut String,
    from: &str,
    to: &str,
) {
    let span = locate(document, text, from);
    let (start, end) = (span.range.start(), span.range.end());
    ws.edit(vec![
        SourceEdit::Delete { key: span },
        SourceEdit::Insert {
            key: Span::point_uri(document.clone(), start).expect("edit point"),
            value: to.to_owned(),
        },
    ])
    .expect("replace commits");
    *text = format!("{}{to}{}", &text[..start], &text[end..]);
}

fn insert_before(
    ws: &mut Workspace,
    document: &Uri<String>,
    text: &mut String,
    anchor: &str,
    value: &str,
) {
    let point = locate(document, text, anchor).range.start();
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(document.clone(), point).expect("insertion point"),
        value: value.to_owned(),
    }])
    .expect("insertion commits");
    *text = format!("{}{value}{}", &text[..point], &text[point..]);
}

fn insert_after(
    ws: &mut Workspace,
    document: &Uri<String>,
    text: &mut String,
    anchor: &str,
    value: &str,
) {
    let point = locate(document, text, anchor).range.end();
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(document.clone(), point).expect("insertion point"),
        value: value.to_owned(),
    }])
    .expect("insertion commits");
    *text = format!("{}{value}{}", &text[..point], &text[point..]);
}

fn delete_once(ws: &mut Workspace, document: &Uri<String>, text: &mut String, needle: &str) {
    let span = locate(document, text, needle);
    let (start, end) = (span.range.start(), span.range.end());
    ws.edit(vec![SourceEdit::Delete { key: span }])
        .expect("deletion commits");
    *text = format!("{}{}", &text[..start], &text[end..]);
}

fn state_of_text(ws: &Workspace) -> FamilyState {
    state_of(ws)
}

#[test]
fn canonical_fixture_matches_hand_authored_rows() {
    let mut ws = build();
    let document = uri("canon");
    ws.open(document.clone(), CANON_TEXT).expect("source opens");
    let digest = semantic_digest(&ws.snapshot());
    let u = document.to_string();
    let paths = [
        "",
        "0",
        "0.0",
        "0.0.0",
        "0.0.1",
        "0.0.1.0",
        "0.0.1.0.0",
        "0.0.1.0.1",
        "1",
        "1.0",
    ];
    let payloads = [
        "Module",
        "Binding",
        "Sum",
        "Number(1)",
        "Group",
        "Difference",
        "Number(2)",
        "Name(y)",
        "Binding",
        "Number(3)",
    ];
    assert_eq!(digest.len(), 2 * paths.len() + 1, "{}", digest.render());
    assert_eq!(
        digest
            .rows_of("parse")
            .iter()
            .find(|(key, _)| *key == format!("parse::{u}"))
            .map(|(_, value)| *value),
        Some("clean")
    );
    for (path, payload) in paths.iter().zip(payloads) {
        let key = format!("{u}#{path}");
        assert_eq!(
            digest
                .rows_of("lowered")
                .iter()
                .find(|(row_key, _)| *row_key == format!("lowered::{key}"))
                .map(|(_, value)| *value),
            Some(payload),
            "row lowered::{key}"
        );
        assert_eq!(
            digest
                .rows_of("origin")
                .iter()
                .find(|(row_key, _)| *row_key == format!("origin::{key}"))
                .map(|(_, value)| *value),
            Some(key.as_str()),
            "row origin::{key}"
        );
    }
}

#[test]
fn payload_traces_are_reversible_and_document_isolated() {
    let mut ws = build();
    let a = uri("payload-cycle-a");
    let b = uri("payload-cycle-b");
    let mut text_a = CANON_TEXT.to_owned();
    ws.open(a.clone(), &text_a).expect("document A opens");
    ws.open(b.clone(), "let kept = 9;")
        .expect("document B opens");
    let initial = state_of_text(&ws);

    replace_once(&mut ws, &a, &mut text_a, "2", "42");
    let changed = state_of_text(&ws);
    let diff = render_diff(&initial.digest, &changed.digest);
    assert!(diff.contains("Number(2) -> Number(42)"), "{diff}");
    assert_eq!(diff.matches('~').count(), 1, "{diff}");
    assert!(!diff.contains("kept"), "{diff}");
    replace_once(&mut ws, &a, &mut text_a, "42", "2");
    let restored = state_of_text(&ws);
    assert_eq!(restored.digest, initial.digest);
    assert_eq!(restored.live_facts, initial.live_facts);
}

#[test]
fn topology_insertion_traces_assert_exact_keyed_deltas() {
    let mut ws = build();
    let document = uri("topology-digest");
    let mut text = "let x = 1;".to_owned();
    ws.open(document.clone(), &text).expect("source opens");
    let initial = state_of(&ws);
    let u = document.to_string();

    insert_before(&mut ws, &document, &mut text, ";", " + 2");
    let after_child = state_of(&ws);
    let diff = render_diff(&initial.digest, &after_child.digest);
    assert!(
        diff.contains(&format!("~ lowered::{u}#0.0 = Number(1) -> Sum")),
        "{diff}"
    );
    assert!(
        diff.contains(&format!("+ lowered::{u}#0.0.0 = Number(1)")),
        "{diff}"
    );
    assert!(
        diff.contains(&format!("+ lowered::{u}#0.0.1 = Number(2)")),
        "{diff}"
    );

    insert_after(&mut ws, &document, &mut text, ";", " let z = 4;");
    let after_declaration = state_of(&ws);
    let diff = render_diff(&after_child.digest, &after_declaration.digest);
    assert!(
        diff.contains(&format!("+ lowered::{u}#1 = Binding")),
        "{diff}"
    );
    assert!(
        diff.contains(&format!("+ lowered::{u}#1.0 = Number(4)")),
        "{diff}"
    );
    assert!(!diff.contains('~'), "{diff}");

    let mut cold = build();
    cold.open(document, &text).expect("cold opens");
    assert_eq!(after_declaration.digest, state_of(&cold).digest);
}

#[test]
fn topology_reverse_traces_are_exactly_reversible() {
    let mut ws = build();
    let document = uri("topology-reverse");
    let mut text = "let x = 1;".to_owned();
    ws.open(document.clone(), &text).expect("source opens");
    let initial = state_of(&ws);
    insert_before(&mut ws, &document, &mut text, ";", " + 2");
    delete_once(&mut ws, &document, &mut text, " + 2");
    let after_child_cycle = state_of(&ws);
    assert_eq!(after_child_cycle.digest, initial.digest);
    assert_eq!(after_child_cycle.live_facts, initial.live_facts);
    insert_after(&mut ws, &document, &mut text, ";", " let z = 4;");
    delete_once(&mut ws, &document, &mut text, " let z = 4;");
    let restored = state_of(&ws);
    assert_eq!(restored.digest, initial.digest);
    assert_eq!(restored.live_facts, initial.live_facts);
}

#[test]
fn close_and_reopen_restores_the_exact_initial_digest() {
    let mut ws = build();
    let document = uri("cycle");
    ws.open(document.clone(), "let x = 1 + 2;")
        .expect("source opens");
    let initial = state_of(&ws);
    ws.close(document.clone()).expect("close commits");
    assert!(state_of(&ws).digest.is_empty());
    ws.open(document, "let x = 1 + 2;").expect("reopen commits");
    let reopened = state_of(&ws);
    assert_eq!(reopened.digest, initial.digest);
    assert_eq!(reopened.live_facts, initial.live_facts);
}

#[test]
fn close_and_reopen_with_new_text_matches_cold_replay() {
    let document = uri("cycle-new-text");
    let mut warm = build();
    warm.open(document.clone(), "let x = 1 + 2;")
        .expect("source opens");
    warm.close(document.clone()).expect("close commits");
    warm.open(document.clone(), "let y = 7 - 3;")
        .expect("reopen commits");
    let warm_state = state_of(&warm);
    let mut cold = build();
    cold.open(document, "let y = 7 - 3;").expect("cold opens");
    let cold_state = state_of(&cold);
    assert_eq!(warm_state.digest, cold_state.digest);
    assert_eq!(warm_state.live_facts, cold_state.live_facts);
}

#[test]
fn payload_and_lifecycle_traces_restore_live_facts_exactly() {
    let mut ws = build();
    let document = uri("facts-cycle");
    let mut text = "let x = 1 + 2; let y = 3;".to_owned();
    ws.open(document.clone(), &text).expect("source opens");
    let initial = state_of(&ws);
    replace_once(&mut ws, &document, &mut text, "2", "42");
    replace_once(&mut ws, &document, &mut text, "42", "2");
    replace_once(&mut ws, &document, &mut text, "+", "-");
    replace_once(&mut ws, &document, &mut text, "-", "+");
    let restored = state_of(&ws);
    assert_eq!(restored.digest, initial.digest);
    assert_eq!(restored.live_facts, initial.live_facts);
    ws.close(document.clone()).expect("close commits");
    ws.open(document, &text).expect("reopen commits");
    let reopened = state_of(&ws);
    assert_eq!(reopened.digest, initial.digest);
    assert_eq!(reopened.live_facts, initial.live_facts);
}
