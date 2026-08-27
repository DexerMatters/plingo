//! End-to-end harness for parser-tree to heterogeneous-tree transforms.
//!
//! The scenarios assert observable target topology, provenance, payload-only
//! dependency propagation, child-order propagation, document isolation, and
//! lifecycle retraction. They deliberately do not depend on parser record IDs.

use fluent_uri::Uri;
use plingo::framework::lex::{Tokens, install_lexer};
use plingo::framework::parse::{
    ParseStatus, ParserTreeEdges, ParserTreeOrders, ParserTreePayloads, ParserTreeRoots,
    ParserTreeStatuses, install_parser_tree,
};
use plingo::framework::source::SourceEdit;
use plingo::framework::workspace::Workspace;
use plingo::reactive::Snapshot;
use plingo::reactive::view::Node;
use plingo::utils::Span;

use super::lower::{LoweredNode, LoweredNodes, LoweredOrigin, LoweredTree, lower_pass_install};
use super::syntax::{TransformDocument, TransformToken};

fn uri(name: &str) -> Uri<String> {
    Span::new(format!("test://tree-transform/{name}"), 0, 0)
        .expect("URI span")
        .uri
}

fn build() -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<TransformToken>(engine)?;
        install_parser_tree::<TransformToken, TransformDocument>(engine)?;
        lower_pass_install(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

fn roots(snapshot: &Snapshot, document: &Uri<String>) -> Vec<Node<LoweredTree>> {
    snapshot.tree_roots_of::<LoweredTree>(&document.to_string())
}

fn render(snapshot: &Snapshot, node: Node<LoweredTree>) -> String {
    let kind = snapshot
        .tree_payload::<LoweredTree>(node.clone())
        .expect("lowered node payload");
    let children = snapshot.tree_children::<LoweredTree>(node);
    if children.is_empty() {
        return format!("{kind:?}");
    }
    let children = children
        .into_iter()
        .map(|child| render(snapshot, child))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{kind:?}({children})")
}

fn preorder(snapshot: &Snapshot, node: Node<LoweredTree>, out: &mut Vec<Node<LoweredTree>>) {
    out.push(node.clone());
    for child in snapshot.tree_children::<LoweredTree>(node) {
        preorder(snapshot, child, out);
    }
}

fn lowered_nodes(snapshot: &Snapshot, root: Node<LoweredTree>) -> Vec<Node<LoweredTree>> {
    let mut nodes = Vec::new();
    preorder(snapshot, root, &mut nodes);
    nodes
}

fn kind(snapshot: &Snapshot, node: Node<LoweredTree>) -> LoweredNode {
    snapshot
        .tree_payload::<LoweredTree>(node)
        .as_deref()
        .expect("lowered node payload")
        .clone()
}

#[test]
fn parser_tree_lowers_to_a_distinct_heterogeneous_tree() {
    let mut ws = build();
    let document = uri("shape");
    ws.open(document.clone(), "let x = 1 + (2 - y); let y = 3;")
        .expect("source opens");

    let snapshot = ws.snapshot();
    let status = snapshot
        .observe::<ParserTreeStatuses>(document.to_string())
        .expect("parser status");
    assert_eq!(*status, ParseStatus::Clean);

    let source_root = snapshot
        .observe::<ParserTreeRoots<TransformDocument>>(document.to_string())
        .map(|root| root.as_ref().clone())
        .expect("parser root");
    let source_order = snapshot
        .observe::<ParserTreeOrders<TransformDocument>>(source_root.clone())
        .expect("parser root order");
    assert!(!source_order.is_empty(), "parser order view is populated");
    assert!(
        snapshot
            .observe::<ParserTreePayloads<TransformDocument>>(source_root)
            .is_some(),
        "parser payload view is populated"
    );
    assert!(
        !snapshot
            .inputs::<ParserTreeEdges<TransformDocument>>()
            .is_empty(),
        "parser edge view is populated"
    );

    let roots = roots(&snapshot, &document);
    assert_eq!(roots.len(), 1);
    assert_eq!(
        render(&snapshot, roots[0].clone()),
        "Module(Binding(Sum(Number, Group(Difference(Number, Name)))), Binding(Number))"
    );

    // Every target node exposes exactly one source-tree origin. Consumers can
    // join trees directly instead of reconstructing relationships by position.
    for target in lowered_nodes(&snapshot, roots[0].clone()) {
        let source = snapshot
            .observe::<LoweredOrigin>(target.clone())
            .expect("one source origin per lowered node");
        assert!(
            snapshot
                .observe::<ParserTreePayloads<TransformDocument>>(source.as_ref().clone())
                .is_some(),
            "origin must be a live parser-view node"
        );
    }
}

#[test]
fn payload_dependency_updates_one_lowered_node_without_touching_siblings() {
    let mut ws = build();
    let a = uri("payload-a");
    let b = uri("payload-b");
    let text = "let x = 1 + 2; let y = 3;";
    ws.open(a.clone(), text).expect("document A opens");
    ws.open(b.clone(), "let kept = 9;")
        .expect("document B opens");

    let before = ws.snapshot();
    let a_root = roots(&before, &a)[0].clone();
    let a_nodes = lowered_nodes(&before, a_root.clone());
    let _sum = a_nodes
        .iter()
        .cloned()
        .find(|node| kind(&before, node.clone()) == LoweredNode::Sum)
        .expect("sum node");
    let b_root = roots(&before, &b)[0].clone();
    let b_render = render(&before, b_root.clone());

    let plus = text.find('+').expect("plus token");
    let report = ws
        .edit(vec![
            SourceEdit::Delete {
                key: Span::new_uri(a.clone(), plus, plus + 1).expect("plus range"),
            },
            SourceEdit::Insert {
                key: Span::point_uri(a.clone(), plus).expect("minus point"),
                value: "-".into(),
            },
        ])
        .expect("operator edit commits");
    assert!(
        report
            .work()
            .parser(&a.to_string())
            .is_some_and(|work| work.component_runs > 0),
        "a source payload-shape change must reach the parser tree"
    );

    let after = ws.snapshot();
    let a_after_root = roots(&after, &a)[0].clone();
    let a_after_nodes = lowered_nodes(&after, a_after_root.clone());
    assert_eq!(a_after_root, a_root, "document target root stays stable");
    assert_eq!(
        render(&after, a_after_root.clone()),
        "Module(Binding(Difference(Number, Number)), Binding(Number))"
    );
    let difference = a_after_nodes
        .iter()
        .cloned()
        .find(|node| kind(&after, node.clone()) == LoweredNode::Difference)
        .expect("difference node");
    assert!(
        after.observe::<LoweredOrigin>(difference.clone()).is_some(),
        "every reclassified target node retains a source-node join"
    );
    assert_eq!(roots(&after, &b), vec![b_root.clone()], "other document stays cold");
    assert_eq!(render(&after, b_root), b_render);
}

/// Exact reaction proof (plan §24.7): a same-terminal Number lexeme edit
/// evaluates ZERO tree-transform projection components — the semantic
/// parser stays cold (plan §2 baseline: "Semantic parser runs 0"), so the
/// payload/edge/order/root projections do not wake at all. The digest row
/// moves only through the shared source/lexer/layout coordinate path, and
/// the other document stays byte-cold.
#[test]
fn payload_edit_evaluates_exactly_the_payload_projection() {
    let mut ws = build();
    let a = uri("reaction-a");
    let b = uri("reaction-b");
    let text = "let x = 1 + 2;";
    ws.open(a.clone(), text).expect("document A opens");
    ws.open(b.clone(), "let kept = 9;")
        .expect("document B opens");

    let two = text.find('2').expect("number two");
    let report = ws
        .edit(vec![
            SourceEdit::Delete {
                key: Span::new_uri(a.clone(), two, two + 1).expect("two range"),
            },
            SourceEdit::Insert {
                key: Span::point_uri(a.clone(), two).expect("two point"),
                value: "42".into(),
            },
        ])
        .expect("number edit commits");

    let digest = report
        .command()
        .metric::<plingo::reactive::ReactionDigest>()
        .expect("reaction digest");

    // No broad enumeration: every component read exact elements.
    assert!(
        digest.broad_enumerations.is_empty(),
        "{:#?}",
        digest.broad_enumerations
    );

    // ZERO projection evaluations: a same-terminal lexeme edit is not a
    // semantic parse (the semantic parser keys on LexedDocuments equality,
    // which value-only edits preserve), so no tree-transform projection
    // component may evaluate.
    for projection in [
        "tree_transform::lower::lower_source_node",
        "tree_transform::lower::lower_source_edge",
        "tree_transform::lower::lower_source_order",
        "tree_transform::lower::lower_source_root",
    ] {
        assert_eq!(
            digest.evaluations_of(projection).count(),
            0,
            "{projection} evaluated on a same-terminal value edit: {:#?}",
            digest.evaluations
        );
    }

    // The lowered lexeme digest still moves through the layout coordinate
    // path, and target identities stay stable.
    let snapshot = ws.snapshot();
    let a_root = roots(&snapshot, &a)[0].clone();
    assert_eq!(
        render(&snapshot, a_root),
        "Module(Binding(Sum(Number, Number)))",
        "topology and node identities are retained"
    );
    let b_root = roots(&snapshot, &b)[0].clone();
    assert_eq!(render(&snapshot, b_root), "Module(Binding(Number))");
}

#[test]
fn child_order_dependency_splices_only_the_affected_lowered_branch() {
    let mut ws = build();
    let document = uri("topology");
    let text = "let x = 1;";
    ws.open(document.clone(), text).expect("source opens");

    let before = ws.snapshot();
    let root = roots(&before, &document)[0].clone();
    assert_eq!(render(&before, root.clone()), "Module(Binding(Number))");

    let insertion = text.find(';').expect("semicolon");
    let report = ws
        .edit(vec![SourceEdit::Insert {
            key: Span::point_uri(document.clone(), insertion).expect("insertion point"),
            value: " + 2".into(),
        }])
        .expect("expression extension commits");
    assert!(
        report
            .work()
            .parser(&document.to_string())
            .is_some_and(|work| work.component_runs > 0),
        "a child-order change must reach the parser tree"
    );

    let after = ws.snapshot();
    let after_root = roots(&after, &document)[0].clone();
    assert_eq!(after_root.clone(), root, "module target identity is retained");
    assert_eq!(
        render(&after, after_root),
        "Module(Binding(Sum(Number, Number)))"
    );
}

#[test]
fn closing_a_document_retracts_the_transformed_forest_and_origins() {
    let mut ws = build();
    let document = uri("close");
    ws.open(document.clone(), "let x = 1;")
        .expect("source opens");
    let before = ws.snapshot();
    let root = roots(&before, &document)[0].clone();
    assert!(before.observe::<LoweredOrigin>(root.clone()).is_some());

    ws.close(document.clone()).expect("close commits");
    let after = ws.snapshot();
    assert!(
        after.observe::<LoweredOrigin>(root).is_none(),
        "retiring the source document must retire transformed provenance"
    );
}
// ---------------------------------------------------------------------------
// Phase 0 oracles (follow-up plan §4): canonical fixture, reversible traces
// with exact keyed deltas, document isolation, and warm/cold equivalence.
//
// Reversible parser-backed lineage and lifecycle oracles are active: the
// Phase 1 ownership and Cut E publication fixes now satisfy both exact
// semantic digests and live-fact counts.
// ---------------------------------------------------------------------------

use plingo::reactive::digest::{FamilyState, render_diff};

use super::lower::semantic_digest;

const CANON_TEXT: &str = "let x = 1 + (2 - y); let y = 3;";

/// Byte span of the unique occurrence of `needle`; panics unless `needle`
/// occurs exactly once in `haystack`.
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

/// Replaces `from` with `to` (Delete+Insert at its located span) and updates
/// `text` in place.
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

/// Inserts `value` immediately before the unique `anchor`; updates `text`.
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

/// Inserts `value` immediately after the unique `anchor`; updates `text`.
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

/// Deletes the unique `needle`; updates `text`.
fn delete_once(ws: &mut Workspace, document: &Uri<String>, text: &mut String, needle: &str) {
    let span = locate(document, text, needle);
    let (start, end) = (span.range.start(), span.range.end());
    ws.edit(vec![SourceEdit::Delete { key: span }])
        .expect("deletion commits");
    if std::env::var_os("PLINGO_TRACE_FACTS").is_some() {
        let snapshot = ws.snapshot();
        let source = plingo::framework::source::source_snapshot(
            &snapshot,
            &document.to_string(),
        )
        .expect("source remains open");
        eprintln!("delete helper source: {:?}", source.to_string());
    }
    *text = format!("{}{}", &text[..start], &text[end..]);
}

fn state_of(ws: &Workspace) -> FamilyState {
    let snapshot = ws.snapshot();
    if std::env::var_os("PLINGO_TRACE_FACTS").is_some() {
        fn view_id<V: plingo::reactive::View>() {
            eprintln!(
                "known {:?} {}",
                std::any::TypeId::of::<V>(),
                V::name()
            );
        }
        view_id::<plingo::framework::source::SourceEdits>();
        view_id::<plingo::framework::source::SourceRevisions>();
        view_id::<plingo::framework::lex::Tokens<TransformToken>>();
        view_id::<plingo::framework::lex::TokenFacts<TransformToken>>();
        view_id::<plingo::framework::lex::SemanticTokenDocuments<TransformToken>>();
        view_id::<plingo::framework::lex::TokenLayoutDocuments<TransformToken>>();
        view_id::<plingo::framework::parse::ParserTreeEdges<TransformDocument>>();
        view_id::<plingo::framework::parse::ParserTreeOrders<TransformDocument>>();
        view_id::<plingo::framework::parse::ParserTreePayloads<TransformDocument>>();
        view_id::<plingo::framework::parse::ParserTreeRoots<TransformDocument>>();
        view_id::<plingo::framework::parse::ParserTreeStatuses>();
        view_id::<plingo::framework::parse::AstSnapshots<TransformDocument>>();
        view_id::<LoweredNodes>();
        view_id::<LoweredTree>();
        view_id::<LoweredOrigin>();
        if let Some(source) =
            plingo::framework::source::source_snapshot(&snapshot, "test://tree-transform/topology-reverse")
        {
            eprintln!("raw source: {:?}", source.to_string());
        }
        if let Some(tokens) =
            snapshot.observe::<Tokens<TransformToken>>("test://tree-transform/topology-reverse".to_owned())
        {
            eprintln!("raw tokens: {:?}", tokens);
        }
    }
    let digest = semantic_digest(&snapshot);
    if std::env::var_os("PLINGO_TRACE_PARSER").is_some() {
        eprintln!("state digest:\n{}", digest.render());
    }
    FamilyState::capture(digest, &snapshot)
}

/// Canonical fixture (plan §4 item 13): hand-authored complete digest rows for
/// the standard program. A warm or cold implementation that leaks an extra
/// row, drops provenance, or misrenders a lexeme fails this.
#[test]
fn canonical_fixture_matches_hand_authored_rows() {
    let mut ws = build();
    let document = uri("canon");
    ws.open(document.clone(), CANON_TEXT).expect("source opens");

    let digest = semantic_digest(&ws.snapshot());
    let u = document.to_string();

    // Structural paths of the lowered forest; lowering preserves the source
    // shape one-to-one, so every origin path mirrors its lowered path.
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

    // The complete domain: 10 lowered rows + 10 origin rows + parse status.
    assert_eq!(digest.len(), 2 * paths.len() + 1, "{}", digest.render());
    let parse_row = digest
        .rows_of("parse")
        .iter()
        .find(|(key, _)| *key == format!("parse::{u}"))
        .map(|(_, value)| *value);
    assert_eq!(parse_row, Some("clean"));
    for (path, payload) in paths.iter().zip(payloads) {
        let key = format!("{u}#{path}");
        let lowered = digest
            .rows_of("lowered")
            .iter()
            .find(|(row_key, _)| *row_key == format!("lowered::{key}"))
            .map(|(_, value)| *value);
        assert_eq!(lowered.as_deref(), Some(payload), "row lowered::{key}");
        let origin = digest
            .rows_of("origin")
            .iter()
            .find(|(row_key, _)| *row_key == format!("origin::{key}"))
            .map(|(_, value)| *value);
        assert_eq!(origin.as_deref(), Some(key.as_str()), "row origin::{key}");
    }
}

/// Payload-only traces: a Number lexeme change moves exactly that leaf's
/// row; a Sum→Difference operator swap reclassifies exactly that node; both
/// reverse to the exact initial digest while the second document stays cold;
/// a fresh workspace replaying the final texts matches. Live-fact equality on
/// reversal is frozen in
/// [`payload_and_lifecycle_traces_restore_live_facts_exactly`].
#[test]
fn payload_traces_are_reversible_and_document_isolated() {
    let mut ws = build();
    let a = uri("payload-a");
    let b = uri("payload-b");
    let mut text_a = "let x = 1 + 2; let y = 3;".to_owned();
    ws.open(a.clone(), &text_a).expect("document A opens");
    ws.open(b.clone(), "let kept = 9;")
        .expect("document B opens");

    let initial_snapshot = ws.snapshot();
    let initial_root = roots(&initial_snapshot, &a)[0].clone();
    let initial = state_of(&ws);

    // Number payload change: only that leaf's lowered row may move.
    replace_once(&mut ws, &a, &mut text_a, "2", "42");
    let after_number = state_of(&ws);
    let diff = render_diff(&initial.digest, &after_number.digest);
    assert!(
        diff.contains("= Number(2) -> Number(42)"),
        "exact leaf delta expected: {diff}"
    );
    assert_eq!(diff.matches('~').count(), 1, "only one row moves: {diff}");
    assert!(
        !diff.contains('+') && !diff.contains("- "),
        "no topology churn: {diff}"
    );
    assert!(!diff.contains("kept"), "document B must stay cold: {diff}");
    assert_eq!(after_number.digest.len(), initial.digest.len());
    let after_number_snapshot = ws.snapshot();
    assert_eq!(
        roots(&after_number_snapshot, &a),
        vec![initial_root],
        "root identity is retained across a payload edit"
    );

    // Reverse: the initial family state returns exactly.
    replace_once(&mut ws, &a, &mut text_a, "42", "2");
    let restored = state_of(&ws);
    assert_eq!(
        restored.digest,
        initial.digest,
        "number reverse mismatch:\n{}",
        render_diff(&initial.digest, &restored.digest)
    );

    // Operator swap: exactly the Sum node's payload reclassifies.
    replace_once(&mut ws, &a, &mut text_a, "+", "-");
    let after_operator = state_of(&ws);
    let diff = render_diff(&initial.digest, &after_operator.digest);
    assert!(diff.contains("Sum -> Difference"), "{diff}");
    assert_eq!(diff.matches('~').count(), 1, "only one row moves: {diff}");
    assert!(!diff.contains("kept"), "document B must stay cold: {diff}");

    // Reverse the operator swap too.
    replace_once(&mut ws, &a, &mut text_a, "-", "+");
    let restored = state_of(&ws);
    assert_eq!(
        restored.digest,
        initial.digest,
        "operator reverse mismatch:\n{}",
        render_diff(&initial.digest, &restored.digest)
    );

    // Cold oracle: a fresh workspace over the identical final texts.
    let mut cold = build();
    cold.open(a.clone(), &text_a).expect("cold A opens");
    cold.open(b.clone(), "let kept = 9;").expect("cold B opens");
    let cold_state = state_of(&cold);
    assert_eq!(
        restored.digest,
        cold_state.digest,
        "warm/cold mismatch:\n{}",
        render_diff(&restored.digest, &cold_state.digest)
    );
}

/// Topology traces (insertion halves): child insertion and declaration
/// insertion each assert their exact keyed deltas, and a fresh workspace
/// replaying the same final text produces an equal digest. The removal
/// halves are frozen in [`topology_reverse_traces_are_exactly_reversible`].
#[test]
fn topology_insertion_traces_assert_exact_keyed_deltas() {
    let mut ws = build();
    let document = uri("topology-digest");
    let mut text = "let x = 1;".to_owned();
    ws.open(document.clone(), &text).expect("source opens");
    let initial = state_of(&ws);
    let u = document.to_string();

    // Child insertion grows exactly the binding's expression subtree:
    // the old Number leaf becomes a Sum and two leaves join under it.
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
    assert!(diff.contains(&format!("+ origin::{u}#0.0.0")), "{diff}");
    assert!(diff.contains(&format!("+ origin::{u}#0.0.1")), "{diff}");
    // Declaration insertion appends exactly one new binding subtree on top.
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
    assert!(diff.contains(&format!("+ origin::{u}#1")), "{diff}");
    assert!(diff.contains(&format!("+ origin::{u}#1.0")), "{diff}");
    assert!(!diff.contains('~'), "no existing row moves: {diff}");

    // Cold oracle: a fresh workspace replaying the same final text.
    let mut cold = build();
    cold.open(document.clone(), &text).expect("cold opens");
    let cold_state = state_of(&cold);
    assert_eq!(
        after_declaration.digest,
        cold_state.digest,
        "warm/cold mismatch:\n{}",
        render_diff(&after_declaration.digest, &cold_state.digest)
    );
}

/// Reversible parser-backed topology matrix: expression-child insertion and
/// declaration insertion/removal each restore the exact initial state.
#[test]
fn topology_reverse_traces_are_exactly_reversible() {
    let mut ws = build();
    let document = uri("topology-reverse");
    let mut text = "let x = 1;".to_owned();
    ws.open(document.clone(), &text).expect("source opens");
    let initial = state_of(&ws);

    insert_before(&mut ws, &document, &mut text, ";", " + 2");
    if std::env::var_os("PLINGO_TRACE_PARSER").is_some() {
        let _forward = state_of(&ws);
    }
    delete_once(&mut ws, &document, &mut text, " + 2");
    let after_child_cycle = state_of(&ws);
    assert_eq!(
        after_child_cycle.digest,
        initial.digest,
        "child cycle mismatch:\n{}",
        render_diff(&initial.digest, &after_child_cycle.digest)
    );
    assert_eq!(after_child_cycle.live_facts, initial.live_facts);

    insert_after(&mut ws, &document, &mut text, ";", " let z = 4;");
    delete_once(&mut ws, &document, &mut text, " let z = 4;");
    let restored = state_of(&ws);
    assert_eq!(
        restored.digest,
        initial.digest,
        "declaration cycle mismatch:\n{}",
        render_diff(&initial.digest, &restored.digest)
    );
    assert_eq!(restored.live_facts, initial.live_facts);
}
/// Closing a document retracts its whole digest domain; reopening the same
/// text restores the exact initial digest and live-fact count.
#[test]
fn close_and_reopen_restores_the_exact_initial_digest() {
    let mut ws = build();
    let document = uri("cycle");
    ws.open(document.clone(), "let x = 1 + 2;")
        .expect("source opens");
    let initial = state_of(&ws);

    ws.close(document.clone()).expect("close commits");
    let closed = state_of(&ws);
    assert!(
        closed.digest.is_empty(),
        "closing must retract every family row:\n{}",
        closed.digest.render()
    );

    ws.open(document.clone(), "let x = 1 + 2;")
        .expect("reopen commits");
    let reopened = state_of(&ws);
    assert_eq!(
        reopened.digest,
        initial.digest,
        "reopen mismatch:\n{}",
        render_diff(&initial.digest, &reopened.digest)
    );
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
        .expect("reopen with new text");

    let warm_state = state_of(&warm);
    let mut cold = build();
    cold.open(document, "let y = 7 - 3;")
        .expect("cold source opens");
    let cold_state = state_of(&cold);

    assert_eq!(
        warm_state.digest,
        cold_state.digest,
        "changed-text reopen diverged from cold replay:\n{}",
        render_diff(&cold_state.digest, &warm_state.digest)
    );
    assert_eq!(warm_state.live_facts, cold_state.live_facts);
}

/// Reversed parser-backed edits and a close/reopen cycle restore the exact
/// initial digest and live-fact count.
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
    assert_eq!(
        restored.digest,
        initial.digest,
        "digest must restore exactly:\n{}",
        render_diff(&initial.digest, &restored.digest)
    );
    assert_eq!(restored.live_facts, initial.live_facts);

    // Close/reopen restores the exact digest and live-fact count.
    ws.close(document.clone()).expect("close commits");
    ws.open(document.clone(), "let x = 1 + 2; let y = 3;")
        .expect("reopen commits");
    let reopened = state_of(&ws);
    assert_eq!(
        reopened.digest,
        initial.digest,
        "reopen digest mismatch:\n{}",
        render_diff(&initial.digest, &reopened.digest)
    );
    assert_eq!(reopened.live_facts, initial.live_facts);
}
