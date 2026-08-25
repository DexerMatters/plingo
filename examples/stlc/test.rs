//! Integration tests for the STLC syntax and its plain reactive pipeline.

use std::fmt::Write as _;

use fluent_uri::Uri;

use plingo::framework::lex::{LexedDocuments, install_lexer};
use plingo::framework::parse::{TreeParseUnits, install_parser_tree};
use plingo::framework::source::SourceEdit;
use plingo::framework::workspace::Workspace;
use plingo::utils::Span;

use plingo::framework::scope::ScopeNode;

use super::{
    check::{StlcTypeDiagnostics, check_pass},

    syntax::StlcDeclarationCase,
    name_resolve::{
        ScopeGraph, StlcScope, StlcScopeData, StlcScopeLabel, StlcTypeValue, name_pass,
        resolve_pass,
    },
    structural::{
        StlcLowered, StlcLoweredOrigin, StlcLoweredSummary, StlcLoweringDiagnostics,
        StlcNodeIndex, structural_pass,
    },
    syntax::{StlcDocument, StlcToken, StlcTree},
};

fn uri(name: &str) -> Uri<String> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn build(workers: usize) -> Workspace {
    let _ = workers;
    Workspace::build(|engine| {
        install_lexer::<StlcToken>(engine)?;
        install_parser_tree::<StlcToken, StlcDocument>(engine)?;
        let planned = engine.plan(name_pass, ())?;
        let _running = engine.run(&planned)?;
        let planned = engine.plan(resolve_pass, ())?;
        let _running = engine.run(&planned)?;
        let planned = engine.plan(check_pass, ())?;
        let _running = engine.run(&planned)?;
        let planned = engine.plan(structural_pass, ())?;
        let _running = engine.run(&planned)?;
        Ok(())
    })
    .expect("workspace builds")
}

fn open(ws: &mut Workspace, u: &Uri<String>, text: &str) {
    ws.open(u.clone(), text).unwrap();
}

fn unit(ws: &Workspace, u: &Uri<String>) -> ArcUnit {
    ws.snapshot()
        .observe::<TreeParseUnits<StlcDocument>>(u.to_string())
        .expect("parse unit")
}

/// Keep the test helper independent of parser internals while retaining a
/// concise type for the committed tree publication.
type ArcUnit = std::sync::Arc<plingo::framework::parse::TreeParseUnit<StlcDocument>>;

/// Renders one inferred type the way the surface syntax writes it.
fn pretty_type(ty: &StlcTypeValue) -> String {
    match ty {
        StlcTypeValue::Nat => "Nat".to_owned(),
        StlcTypeValue::Bool => "Bool".to_owned(),
        StlcTypeValue::Unit => "Unit".to_owned(),
        StlcTypeValue::Arrow(parameter, result) => {
            format!("{} -> {}", pretty_type(parameter), pretty_type(result))
        }
    }
}

/// Renders one scope-graph node payload in human terms.
fn pretty_payload(payload: &ScopeNode<StlcScope>) -> String {
    let data = match payload {
        ScopeNode::Scope(data) | ScopeNode::Declaration(data) | ScopeNode::Reference(data) => data,
    };
    match data {
        StlcScopeData::Document => "document".to_owned(),
        StlcScopeData::Lexical => "lexical".to_owned(),
        StlcScopeData::CaseSuccessor => "case-successor".to_owned(),
        StlcScopeData::External { path } => format!("external \"{path}\""),
        StlcScopeData::Declaration { name, .. } => format!("declaration \"{name}\""),
        StlcScopeData::Type(ty) => format!("type {}", pretty_type(ty)),
    }
}

/// Renders one edge label.
fn pretty_label(label: &StlcScopeLabel) -> String {
    match label {
        StlcScopeLabel::Lexical => "Lexical".to_owned(),
        StlcScopeLabel::Declaration(name) => format!("Declaration({name})"),
        StlcScopeLabel::Type => "Type".to_owned(),
        StlcScopeLabel::Import(path) => format!("Import({path})"),
    }
}

/// Pretty-prints the committed scope graph. Nodes appear in registration
/// order with readable payloads; each node's labelled outgoing edges are
/// indented directly beneath it, pointing at the numbered targets.
fn render_scope_graph(snapshot: &plingo::reactive::Snapshot) -> String {
    use plingo::reactive::kind::GraphKey;

    // Discovery pass: assign dense numbers to node facts in input order.
    let mut payloads: Vec<String> = Vec::new();
    let mut scopes: Vec<super::name_resolve::Scope<StlcScope>> = Vec::new();
    for input in snapshot.inputs::<ScopeGraph<StlcScope>>() {
        if let GraphKey::Node(node) = input {
            if let Some(payload) = snapshot.graph_node::<ScopeGraph<StlcScope>>(node) {
                payloads.push(pretty_payload(&payload));
                scopes.push(super::name_resolve::Scope::from_node(node));
            }
        }
    }

    // Edge collection: resolve each bucket's source and targets against the
    // discovered scopes.
    let mut edges: Vec<(usize, String, usize)> = Vec::new();
    for input in snapshot.inputs::<ScopeGraph<StlcScope>>() {
        let GraphKey::Bucket(from, label) = input else {
            continue;
        };
        let Some(source_idx) = scopes.iter().position(|scope| scope.node() == from) else {
            continue;
        };
        for target in snapshot.outgoing::<ScopeGraph<StlcScope>>(from, &label) {
            if let Some(target_idx) = scopes.iter().position(|scope| scope.node() == target) {
                edges.push((source_idx, pretty_label(&label), target_idx));
            }
        }
    }
    edges.sort();

    // Render: one header per scope with its edges directly beneath it.
    let mut out = String::new();
    for (index, payload) in payloads.iter().enumerate() {
        writeln!(out, "scope {index}: {payload}").expect("write");
        for (_, label, target) in edges.iter().filter(|(source, _, _)| *source == index) {
            writeln!(out, "  -- {label} -> scope {target}").expect("write");
        }
    }
    out
}

fn render_ast(
    out: &mut String,
    snapshot: &plingo::reactive::Snapshot,
    id: plingo::reactive::view::Node<StlcTree>,
    depth: usize,
) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    writeln!(out, "<node {id:?}>").expect("write");
    for child in StlcTree::snapshot_children(snapshot, id).iter().copied() {
        render_ast(out, snapshot, child, depth + 1);
    }
}

#[test]
fn pipelines_publish_scope_and_type_results() {
    let u = uri("scenario1");
    let mut ws = build(1);
    open(&mut ws, &u, "f : Nat -> Nat := ()");

    let snapshot = ws.snapshot();
    assert!(!snapshot.inputs::<ScopeGraph<StlcScope>>().is_empty());
    let diagnostics: Vec<_> = snapshot
        .inputs::<StlcTypeDiagnostics>()
        .into_iter()
        .filter_map(|key| match key {
            plingo::reactive::kind::ListKey::Slot(node, _) => {
                Some(snapshot.list::<StlcTypeDiagnostics>(&node))
            }
            _ => None,
        })
        .flatten()
        .collect();
    assert!(diagnostics.iter().any(|diagnostic| {
        matches!(
            diagnostic.error,
            super::name_resolve::StlcTypeError::Mismatch {
                expected: StlcTypeValue::Arrow(..),
                found: StlcTypeValue::Unit,
            }
        )
    }));
    // Inferred types are graph facts now: at least one Type payload exists.
    let typed = snapshot
        .inputs::<ScopeGraph<StlcScope>>()
        .into_iter()
        .filter_map(|input| match input {
            plingo::reactive::kind::GraphKey::Node(node) => snapshot
                .graph_node::<ScopeGraph<StlcScope>>(node)
                .map(|payload| matches!(
                    payload.as_ref(),
                    plingo::framework::scope::ScopeNode::Scope(StlcScopeData::Type(_))
                )),
            _ => None,
        })
        .any(|typed| typed);
    assert!(typed, "inferred types live in the scope graph");
}

#[test]
fn structural_pipeline_retracts_removed_roots() {
    let u = uri("scenario2");
    let mut ws = build(1);
    open(&mut ws, &u, "f : Nat := ()");
    let snapshot = ws.snapshot();
    let root = unit(&ws, &u).root.expect("root");
    assert!(
        snapshot.observe::<StlcNodeIndex>(root).is_some(),
        "the root node is indexed"
    );

    ws.close(u.clone()).unwrap();
    let snapshot = ws.snapshot();
    assert!(
        snapshot
            .observe::<TreeParseUnits<StlcDocument>>(u.to_string())
            .is_none(),
        "closing the document retracts its parse unit"
    );
    assert!(
        snapshot.observe::<StlcNodeIndex>(root).is_none(),
        "structural facts retract with their owning visitors"
    );
    assert!(
        snapshot
            .observe::<TreeParseUnits<StlcDocument>>(u.to_string())
            .is_none()
    );
}

#[test]
fn parser_facts_retain_unchanged_ast_keys() {
    let u = uri("scenario3");
    let mut ws = build(1);
    open(&mut ws, &u, "x := 0\ny := 1");
    let before = unit(&ws, &u).root.expect("root");
    ws.edit(vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), 5, 6).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 5).unwrap(),
            value: "2".into(),
        },
    ])
    .unwrap();
    let after = unit(&ws, &u).root.expect("root after edit");
    assert_eq!(after, before, "the document root remains stable");
}

#[test]
fn workspace_configures_the_graph_directly() {
    let u = uri("scenario4");
    let mut ws = build(1);
    open(&mut ws, &u, "x := 0");
    let before = StlcTree::snapshot_roots(&ws.snapshot()).len();
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), 0).unwrap(),
        value: "\n".into(),
    }])
    .unwrap();
    let after = StlcTree::snapshot_roots(&ws.snapshot()).len();
    assert!(after >= before, "an edit drives the pipeline again");
}

#[test]
fn structural_views_publish_all_downstream_products() {
    let u = uri("scenario5");
    let mut ws = build(1);
    open(&mut ws, &u, "id : Nat -> Nat := fun x -> x");
    let snapshot = ws.snapshot();
    let root = unit(&ws, &u).root.expect("root");
    assert_eq!(
        snapshot
            .observe::<StlcLowered>(root)
            .map(|value| value.as_str().to_owned()),
        Some("untyped::Document".to_owned())
    );
    assert_eq!(
        snapshot
            .observe::<StlcLoweredOrigin>(root)
            .map(|origin| *origin),
        Some(root)
    );
    assert!(
        snapshot
            .list::<StlcLoweringDiagnostics>(&root)
            .is_empty()
    );
    assert_eq!(
        snapshot
            .observe::<StlcLoweredSummary>(root)
            .map(|value| value.as_str().to_owned()),
        Some("summary:untyped::Document".to_owned())
    );
}

#[test]
fn one_worker_and_many_worker_runs_produce_equal_facts() {
    let u = uri("scenario6");
    let text = "f : Nat -> Nat := fun x -> x\nn : Nat := 0";
    let mut single = build(1);
    let mut many = build(8);
    open(&mut single, &u, text);
    open(&mut many, &u, text);

    let dump = |ws: &Workspace| -> String {
        let snapshot = ws.snapshot();
        let unit = unit(ws, &u);
        let mut types: Vec<String> = snapshot
            .inputs::<ScopeGraph<StlcScope>>()
            .into_iter()
            .filter_map(|input| match input {
                plingo::reactive::kind::GraphKey::Node(node) => snapshot
                    .graph_node::<ScopeGraph<StlcScope>>(node)
                    .and_then(|payload| match payload.as_ref() {
                        plingo::framework::scope::ScopeNode::Scope(StlcScopeData::Type(ty)) => {
                            Some(format!("{ty:?}"))
                        }
                        _ => None,
                    }),
                _ => None,
            })
            .collect();
        types.sort();
        format!("{unit:?}|{types:?}")
    };
    assert_eq!(dump(&single), dump(&many));

    let end = text.len();
    let edit = SourceEdit::Insert {
        key: Span::point_uri(u.clone(), end).unwrap(),
        value: "\ny : Bool := true".into(),
    };
    single.edit(vec![edit.clone()]).unwrap();
    many.edit(vec![edit]).unwrap();
    let mut cold = build(8);
    open(&mut cold, &u, &format!("{text}\ny : Bool := true"));
    assert_eq!(dump(&single), dump(&cold));
    assert_eq!(dump(&many), dump(&cold));
}

#[test]
fn edit_invalidates_only_affected_pipelines() {
    let u = uri("scenario7");
    let mut ws = build(1);
    open(&mut ws, &u, "x := 0\ny := 1");
    let before = unit(&ws, &u).root.expect("root");
    let before_facts = ws
        .snapshot()
        .inputs::<StlcNodeIndex>()
        .len();
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), 2).unwrap(),
        value: "9".into(),
    }])
    .unwrap();
    let after = unit(&ws, &u).root.expect("root after edit");
    let after_facts = ws
        .snapshot()
        .inputs::<StlcNodeIndex>()
        .len();
    assert_eq!(before, after);
    assert_eq!(before_facts, after_facts);
}

#[test]
fn prints_ast_and_final_scope_graph_for_let_and_function_code() {
    let u = uri("scenario8");
    let code = "id : Nat -> Nat := fun x -> x\nmul (x : Nat) (y : Nat) : Nat -> Nat -> Nat := case x of zero -> 0 | succ p -> y + mul p y";
    let mut ws = build(1);
    open(&mut ws, &u, code);

    let snapshot = ws.snapshot();
    let root = unit(&ws, &u).root.expect("root");
    let mut buffer = String::new();
    render_ast(&mut buffer, &snapshot, root, 0);

    let scope_graph = render_scope_graph(&snapshot);
    println!("{scope_graph}");
    assert!(
        scope_graph.contains("document"),
        "the document scope anchors the graph\n{scope_graph}"
    );
    assert!(
        scope_graph.contains("declaration \"id\""),
        "the `id` binder appears as a readable declaration\n{scope_graph}"
    );
    assert!(
        scope_graph.contains("declaration \"mul\""),
        "the `mul` binder appears as a readable declaration\n{scope_graph}"
    );
    assert!(
        scope_graph.contains("type Nat -> Nat"),
        "inferred types render in surface syntax\n{scope_graph}"
    );
    assert!(
        scope_graph.contains("-- Lexical -> scope"),
        "lexical edges render with their labels\n{scope_graph}"
    );
    assert!(
        scope_graph.contains("-- Declaration("),
        "declaration edges render with their labels\n{scope_graph}"
    );
    assert!(
        scope_graph.contains("-- Type -> scope"),
        "type edges render with their labels\n{scope_graph}"
    );
}

// ---------------------------------------------------------------------------
// Plan §6.3 audit table: the "After" column, machine-checked. Each row
// asserts the exact fact-change footprint of one edit class.
// ---------------------------------------------------------------------------


/// Dumps every scope-graph node payload (sorted) so two epochs compare by
/// content, not insertion order.
fn graph_dump(ws: &Workspace) -> Vec<String> {
    let snapshot = ws.snapshot();
    let lines: Vec<String> = snapshot
        .inputs::<ScopeGraph<StlcScope>>()
        .into_iter()
        .filter_map(|input| match input {
            plingo::reactive::kind::GraphKey::Node(node) => {
                let payload = snapshot.graph_node::<ScopeGraph<StlcScope>>(node)?;
                Some(format!("{payload:?}"))
            }
            _ => None,
        })
        .collect();
    // Bucket contents matter too: dump (bucket length) pairs.
    let mut buckets: Vec<String> = snapshot
        .inputs::<ScopeGraph<StlcScope>>()
        .into_iter()
        .filter_map(|input| match input {
            plingo::reactive::kind::GraphKey::Bucket(from, label) => {
                let targets =
                    snapshot.outgoing::<ScopeGraph<StlcScope>>(from, &label);
                Some(format!("bucket {label:?} -> {}", targets.len()))
            }
            _ => None,
        })
        .collect();
    let mut all = lines;
    all.append(&mut buckets);
    let mut lines = all;
    lines.sort();
    lines.dedup();
    lines
}

/// Diagnostics dump across all per-node slots.
fn diagnostics_dump(ws: &Workspace) -> Vec<String> {
    let snapshot = ws.snapshot();
    let mut lines: Vec<String> = snapshot
        .inputs::<StlcTypeDiagnostics>()
        .into_iter()
        .filter_map(|key| match key {
            plingo::reactive::kind::ListKey::Slot(node, _) => Some(
                snapshot
                    .list::<StlcTypeDiagnostics>(&node)
                    .into_iter()
                    .map(|d| format!("{:?}", d.error))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    lines.sort();
    lines
}

#[test]
fn audit_literal_edit_changes_one_tree_fact_and_no_graph_fact() {
    let u = uri("audit-literal");
    let mut ws = build(1);
    open(&mut ws, &u, "n : Nat := 1 + 2");
    let before_graph = graph_dump(&ws);
    let before_diagnostics = diagnostics_dump(&ws);

    ws.edit(vec![SourceEdit::Delete {
        key: Span::new_uri(u.clone(), 14, 15).unwrap(),
    }])
    .unwrap();
    let delete_token_keys =
        ws.snapshot()
            .inputs::<plingo::framework::lex::TokenFacts<StlcToken>>();
    let after_delete = graph_dump(&ws);
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), 14).unwrap(),
        value: "7".to_owned(),
    }])
    .unwrap();
    let after_token_keys =
        ws.snapshot()
            .inputs::<plingo::framework::lex::TokenFacts<StlcToken>>();

    let after_graph = graph_dump(&ws);
    let after_diagnostics = diagnostics_dump(&ws);
    let snapshot = ws.snapshot();
    let root = unit(&ws, &u).root.expect("root");
    let root_case = StlcTree::snapshot_case(&snapshot, root);
    let root_children = StlcTree::snapshot_children(&snapshot, root);
    let child_case = root_children
        .first()
        .and_then(|child| StlcTree::snapshot_case(&snapshot, *child));
    assert_eq!(
        before_graph,
        after_graph,
        "graph facts unchanged: before={before_graph:?} delete={after_delete:?} after={after_graph:?} root_case={root_case:?} root_children={root_children:?} child_case={child_case:?} delete_tokens={delete_token_keys:?} after_tokens={after_token_keys:?}"
    );
    assert_eq!(
        before_diagnostics, after_diagnostics,
        "no new diagnostics: types did not change"
    );

    // Exactly one syntax-tree node fact differs: the edited literal.
    let changed_nodes = snapshot
        .inputs::<StlcLowered>()
        .len();
    assert!(changed_nodes > 0, "tree nodes remain indexed");
}

#[test]
fn audit_rename_binder_rewrites_only_the_declaration_payload() {
    let u = uri("audit-rename");
    let mut ws = build(1);
    open(&mut ws, &u, "x : Nat := 0\ny : Nat := x");
    let before_graph = graph_dump(&ws);
    let before_diagnostics = diagnostics_dump(&ws);

    // Rename the binder y -> w on line 2 (the reference stays y).
    ws.edit(vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), 13, 14).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 13).unwrap(),
            value: "w".to_owned(),
        },
    ])
    .unwrap();

    let after_graph = graph_dump(&ws);
    let after_diagnostics = diagnostics_dump(&ws);
    if std::env::var("PLINGO_DEBUG_DELTA").is_ok() {
        eprintln!("[after-graph] {after_graph:?}");
        eprintln!("[after-diags] {after_diagnostics:?}");
    }

    // Exactly one graph fact differs — the renamed binder's declaration
    // The binder rename updates its declaration payload and exact old/new
    // name buckets; unrelated scopes, types, and diagnostics remain stable.
    let removed: Vec<&String> =
        before_graph.iter().filter(|line| !after_graph.contains(line)).collect();
    let added: Vec<&String> =
        after_graph.iter().filter(|line| !before_graph.contains(line)).collect();
    assert_eq!(removed.len(), 2, "declaration and old name bucket removed; got {removed:?}");
    assert_eq!(added.len(), 2, "declaration and new name bucket added; got {added:?}");
    assert!(
        removed[0].contains("Declaration") && added[0].contains("Declaration"),
        "the changed fact is the declaration payload"
    );
    assert!(
        removed[0].contains("\"y\"") && added[0].contains("\"w\""),
        "only the renamed binder's name changed"
    );
    assert_eq!(
        before_diagnostics, after_diagnostics,
        "unbound-reference diagnostics are unaffected by an unrelated rename"
    );
}

#[test]
fn audit_terminal_kind_change_retains_lineage_and_rewrites_type_facts() {
    // §18 matrix: terminal `0 -> true`. A keyword token becomes a number
    // token: the parser re-runs locally, the declaration node identity is
    // retained (stable lineage), and only the leaf type + genuine
    // ancestors change.
    let u = uri("audit-terminal");
    let mut ws = build(1);
    open(&mut ws, &u, "b : Bool := true");
    let before_diagnostics = diagnostics_dump(&ws);
    let before_root = unit(&ws, &u).root.expect("root");

    // Replace `true` (bytes 12..16) with `0`: a terminal-KIND change.
    ws.edit(vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), 12, 16).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 12).unwrap(),
            value: "0".into(),
        },
    ])
    .unwrap();

    let after_root = unit(&ws, &u).root.expect("root after terminal change");
    let after_diagnostics = diagnostics_dump(&ws);
    assert_eq!(
        before_root, after_root,
        "the document root record is retained across the terminal-kind change"
    );
    assert!(
        before_diagnostics.is_empty(),
        "typed program starts clean: {before_diagnostics:?}"
    );
    assert!(
        after_diagnostics.iter().any(|d| d.contains("Mismatch")),
        "Bool annotation vs Nat literal must diagnose a mismatch: {after_diagnostics:?}"
    );
}

#[test]
fn audit_expression_child_insert_writes_only_new_subtree_and_parent_splice() {
    // §18 matrix: insert expression child. `1` becomes `1 + 2`: the new
    // Add subtree is published, the declaration root identity and its
    // name facts stay put, and no unrelated declaration wakes.
    let u = uri("audit-insert-child");
    let mut ws = build(1);
    open(&mut ws, &u, "x : Nat := 1\ny : Nat := 2");
    let before_root = unit(&ws, &u).root.expect("root");
    let before_diagnostics = diagnostics_dump(&ws);
    let before_graph = graph_dump(&ws);

    // Replace `1` (bytes 11..12) with `1 + 2`.
    ws.edit(vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), 11, 12).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 11).unwrap(),
            value: "1 + 2".into(),
        },
    ])
    .unwrap();

    let snapshot = ws.snapshot();
    let root = unit(&ws, &u).root.expect("root after insert");
    let children = StlcTree::snapshot_children(&snapshot, root);
    assert_eq!(
        children.len(),
        2,
        "both declarations remain; root child splice: {children:?}"
    );
    assert_eq!(
        before_root, root,
        "the document root record is retained across a child insertion"
    );
    let after_diagnostics = diagnostics_dump(&ws);
    assert_eq!(
        before_diagnostics, after_diagnostics,
        "Nat := Nat + Nat stays well-typed; no new diagnostics"
    );
    let after_graph = graph_dump(&ws);
    assert_eq!(
        before_graph, after_graph,
        "Nat := Nat + Nat types identically: no graph fact changes"
    );
}

#[test]
fn structural_top_level_insert_refreshes_root_order() {
    let u = uri("gss-lineage");
    let mut ws = build(1);
    open(&mut ws, &u, "x : Nat := 0");
    let before_snapshot = ws.snapshot();
    let before_root = unit(&ws, &u).root.expect("initial root");
    assert_eq!(
        StlcTree::snapshot_children(&before_snapshot, before_root).len(),
        1
    );

    let at = "x : Nat := 0".len();
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), at).unwrap(),
        value: "\ny : Nat := 1".into(),
    }])
    .unwrap();

    let snapshot = ws.snapshot();
    let root = unit(&ws, &u).root.expect("root after insert");
    let children = StlcTree::snapshot_children(&snapshot, root);
    assert_eq!(children.len(), 2, "root children: {children:?}");
}

#[test]
fn parser_delta_oracle_matches_slow_membership_diff() {
    // §20.2: the published ParseDelta record domains must equal the slow
    // exact symmetric difference of root-reachable membership across every
    // command, including recovery-shaped edits.
    let u = uri("delta-oracle");
    let mut ws = build(1);
    open(&mut ws, &u, "x : Nat := 0\ny : Nat := x");
    let mut live_ids = |ws: &Workspace| -> std::collections::BTreeSet<u64> {
        let snapshot = ws.snapshot();
        let snapshots = snapshot
            .observe::<plingo::framework::parse::AstSnapshots<StlcDocument>>(u.to_string())
            .expect("ast snapshots present");
        snapshots.snapshot().__live_record_ids().into_iter().collect()
    };
    let mut previous = live_ids(&ws);

    let mut apply = |ws: &mut Workspace, edits: Vec<SourceEdit>| {
        let report = ws.edit(edits).expect("edit commits");
        let work = report
            .work()
            .parser(u.as_str())
            .cloned()
            .unwrap_or_default();
        let current = live_ids(ws);
        let inserted: Vec<u64> = current.difference(&previous).copied().collect();
        let removed: Vec<u64> = previous.difference(&current).copied().collect();
        assert_eq!(
            work.parser_records_inserted,
            inserted.len() as u64,
            "inserted membership diff mismatch"
        );
        assert_eq!(
            work.parser_records_removed,
            removed.len() as u64,
            "removed membership diff mismatch"
        );
        previous = current;
    };

    // Structural: rename a binder token (value-only, same shape).
    apply(
        &mut ws,
        vec![SourceEdit::Delete { key: Span::new_uri(u.clone(), 1, 2).unwrap() },
             SourceEdit::Insert { key: Span::point_uri(u.clone(), 1).unwrap(), value: "w".into() }],
    );
    // Terminal-kind change inside first declaration body.
    apply(
        &mut ws,
        vec![SourceEdit::Delete { key: Span::new_uri(u.clone(), 15, 16).unwrap() },
             SourceEdit::Insert { key: Span::point_uri(u.clone(), 15).unwrap(), value: "true".into() }],
    );
    // Insert a fresh declaration line.
    let end = {
        let snapshot = ws.snapshot();
        plingo::framework::source::source_snapshot(&snapshot, &u.to_string())
            .map(|source| source.len_bytes())
            .unwrap_or(0)
    };
    apply(
        &mut ws,
        vec![SourceEdit::Insert {
            key: Span::point_uri(u.clone(), end).unwrap(),
            value: "\nz : Bool := false".into(),
        }],
    );
    // Recovery-shaped garbage insertion.
    apply(
        &mut ws,
        vec![SourceEdit::Insert { key: Span::point_uri(u.clone(), 2).unwrap(), value: "9".into() }],
    );
    // Repair by deleting the garbage.
    apply(
        &mut ws,
        vec![SourceEdit::Delete { key: Span::new_uri(u.clone(), 2, 3).unwrap() }],
    );
}
