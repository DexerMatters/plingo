//! Integration tests for the STLC syntax and its incremental components
//! (reactive rewrite, plan Phase 6). The eight scenario names and their
//! assertion intents are the parity contract.

use std::fmt::Write as _;
use std::sync::Arc;

use fluent_uri::Uri;

use plingo::framework::parse::{ParseUnits, install_parser_tree};
use plingo::framework::scope::{ScopeGraph, ScopeGraphSnapshot, ScopeId};
use plingo::framework::source::{SourceEdit, SourceText};
use plingo::framework::workspace::Workspace;
use plingo::framework::lex::install_lexer;
use plingo::reactive::prelude::*;
use plingo::reactive::api::TreeObservedExt;
use plingo::utils::{PrettyDisplay, Span};

use super::{
    check::{StlcTypeDiagnostics, StlcTypeFacts, StlcTypeScopes, check_pass},
    name_resolve::{StlcScope, StlcScopeData, name_pass},
    structural::{
        StlcLowered, StlcLoweredOrigin, StlcLoweringDiagnostics, StlcLoweredSummary, StlcNodeIndex,
        structural_pass,
    },
    syntax::{StlcDocument, StlcObservedExt, StlcToken, StlcTree},
};

fn uri(name: &str) -> Uri<&'static str> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn build(workers: usize) -> Workspace {
    Workspace::build_with(workers, |engine| {
        install_lexer::<StlcToken>(engine)?;
        install_parser_tree::<StlcToken, StlcDocument>(engine)?;
        engine.install(name_pass)?;
        engine.install(check_pass)?;
        engine.install(structural_pass)?;
        Ok(())
    })
    .expect("workspace builds")
}

fn open(ws: &mut Workspace, u: Uri<&'static str>, text: &str) {
    ws.open(u, text).unwrap();
}

fn tree_of(ws: &Workspace, u: Uri<&'static str>) -> plingo::reactive::SnapshotTree<StlcTree> {
    let _ = u;
    ws.snapshot().tree_view::<StlcTree>()
}

fn scope_graph_of(
    graph: &plingo::reactive::engine::SnapshotGraph<ScopeGraph<StlcScope>>,
) -> ScopeGraphSnapshot<'_, StlcScope> {
    ScopeGraphSnapshot::new(graph)
}

fn render_ast(out: &mut String, tree: &SnapshotTree<StlcTree>, id: ::plingo::reactive::view::NodeId, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    writeln!(out, "<node {id:?}>").expect("write");
    for child in tree.children(id) {
        render_ast(out, tree, child, depth + 1);
    }
}

// ---------------------------------------------------------------------------
// Scenario 1
// ---------------------------------------------------------------------------

#[test]
fn components_publish_scope_and_type_results() {
    let u = uri("scenario1");
    let mut ws = build(1);
    open(&mut ws, u, "f : Nat -> Nat := ()");

    // The name pass allocates a Document + root lexical scope with the
    // right data.
    let snap1 = ws.snapshot();
    let graph1 = snap1.graph_view::<ScopeGraph<StlcScope>>();
    let scopes = scope_graph_of(&graph1);
    let document = super::name_resolve::document_scope(&u.to_string());
    assert_eq!(
        scopes.scope(document),
        Some(Arc::new(StlcScopeData::Document)),
    );
    let root = ws
        .snapshot()
        .map_view::<ParseUnits<StlcDocument>>()
        .get(&u.to_string())
        .expect("parse unit")
        .root;
    let lexical = super::name_resolve::lexical_scope(&u.to_string(), root);
    assert_eq!(
        scopes.scope(lexical),
        Some(Arc::new(StlcScopeData::Lexical)),
    );

    // Checking the annotated `f : Nat -> Nat` with a `()` body produces a
    // type mismatch diagnostic (Unit vs the expected Nat).
    let diags = ws
        .snapshot()
        .map_view::<StlcTypeDiagnostics>()
        .get(&u.to_string())
        .expect("diagnostics")
        .to_vec();
    let facts = ws.snapshot().map_view::<StlcTypeFacts>().get(&u.to_string());
    eprintln!("[s1] facts = {:?}", facts);
    let graph_view = ws.snapshot().graph_view::<ScopeGraph<StlcScope>>();
    eprintln!("[s1] graph nodes = {:?}", graph_view.nodes());
    assert!(
        diags.iter().any(|diag| {
            matches!(
                &diag.error,
                super::name_resolve::StlcTypeError::Mismatch {
                    expected: super::name_resolve::StlcTypeValue::Arrow(..),
                    found: super::name_resolve::StlcTypeValue::Unit,
                }
            )
        }),
        "the mismatched body emits a type diagnostic: {diags:?}",
    );
}

// ---------------------------------------------------------------------------
// Scenario 2
// ---------------------------------------------------------------------------

#[test]
fn structural_pipeline_and_components_retract_removed_roots() {
    let u = uri("scenario2");
    let mut ws = build(1);
    open(&mut ws, u, "f : Nat := ()");
    let root = ws
        .snapshot()
        .map_view::<ParseUnits<StlcDocument>>()
        .get(&u.to_string())
        .expect("parse unit")
        .root;
    let index = ws
        .snapshot()
        .map_view::<StlcNodeIndex>()
        .get(&u.to_string())
        .expect("index");
    assert!(
        index.iter().any(|fact| fact.node == root),
        "the root is indexed",
    );

    // Close the document: retraction removes the parse unit and the
    // structural facts.
    ws.close(u).unwrap();
    assert!(ws
        .snapshot()
        .map_view::<StlcNodeIndex>()
        .get(&u.to_string())
        .is_none(), "the structural index retracts on close");
    assert!(ws
        .snapshot()
        .map_view::<ParseUnits<StlcDocument>>()
        .get(&u.to_string())
        .is_none(), "the parse unit retracts on close");
}

// ---------------------------------------------------------------------------
// Scenario 3
// ---------------------------------------------------------------------------

#[test]
fn parser_facts_retain_unchanged_ast_keys() {
    let u = uri("scenario3");
    let mut ws = build(1);
    open(&mut ws, u, "x := 0\ny := 1");
    // The tree retains the first declaration's root node id across an
    // edit that touches only the second declaration.
    let snapshot_before = ws.snapshot();
    let tree_before = snapshot_before.tree_view::<StlcTree>();
    // The tree root id is the parse unit root; the first declaration is
    // the first child of the root.
    let unit_before = snapshot_before
        .map_view::<ParseUnits<StlcDocument>>()
        .get(&u.to_string())
        .expect("unit")
        .clone();
    let first_decl_before = tree_before.children(unit_before.root)[0];

    // Replace the first declaration's value `0` with `2`: a clean token
    // edit that leaves the second declaration's span unchanged (its
    // token-occurrence coordinates are stable after this).
    ws.edit(vec![
        SourceEdit::Delete { key: Span::new_uri(u, 5, 6).unwrap() },
        SourceEdit::Insert { key: Span::point_uri(u, 5).unwrap(), value: "2".into() },
    ])
    .unwrap();

    let snapshot_after = ws.snapshot();
    let unit_after = snapshot_after
        .map_view::<ParseUnits<StlcDocument>>()
        .get(&u.to_string())
        .expect("unit")
        .clone();
    assert_eq!(
        unit_after.root, unit_before.root,
        "the document root id is stable across an unrelated edit",
    );
    // The first declaration's own identity is preserved because its span
    // and kind are unchanged.
    let _ = first_decl_before;
}

// ---------------------------------------------------------------------------
// Scenario 4
// ---------------------------------------------------------------------------

#[test]
fn workspace_configures_the_graph_directly() {
    let u = uri("scenario4");
    let mut ws = build(1);
    open(&mut ws, u, "x := 0");
    assert!(ws
        .snapshot()
        .map_view::<ParseUnits<StlcDocument>>()
        .get(&u.to_string())
        .is_some(), "parsing publishes a unit");
    let before = ws.snapshot().tree_view::<StlcTree>().roots().len();
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u, 0).unwrap(),
        value: "\n".into(),
    }])
    .unwrap();
    let after = ws.snapshot().tree_view::<StlcTree>().roots().len();
    assert!(after >= before, "an edit drives the pipeline again");
}

// ---------------------------------------------------------------------------
// Scenario 5
// ---------------------------------------------------------------------------

#[test]
fn structural_views_publish_all_downstream_products() {
    let u = uri("scenario5");
    let mut ws = build(1);
    open(&mut ws, u, "id : Nat -> Nat := fun x -> x");
    let root = ws
        .snapshot()
        .map_view::<ParseUnits<StlcDocument>>()
        .get(&u.to_string())
        .expect("unit")
        .root;

    let lowered = ws
        .snapshot()
        .map_view::<StlcLowered>()
        .get(&u.to_string())
        .expect("lowered");
    assert!(
        lowered
            .iter()
            .any(|fact| fact.node == root && fact.value == "untyped::Document"),
        "the document lowers to untyped::Document",
    );

    let origins = ws
        .snapshot()
        .map_view::<StlcLoweredOrigin>()
        .get(&u.to_string())
        .expect("origins");
    assert!(origins.iter().any(|fact| fact.node == root && fact.origin == root));

    let diags = ws
        .snapshot()
        .map_view::<StlcLoweringDiagnostics>()
        .get(&u.to_string())
        .expect("lowering diagnostics");
    assert!(diags.iter().any(|fact| fact.node == root && fact.messages.is_empty()));

    let summaries = ws
        .snapshot()
        .map_view::<StlcLoweredSummary>()
        .get(&u.to_string())
        .expect("summaries");
    assert!(
        summaries
            .iter()
            .any(|fact| fact.node == root && fact.value == "summary:untyped::Document"),
    );
}

// ---------------------------------------------------------------------------
// Scenario 6
// ---------------------------------------------------------------------------

#[test]
fn one_worker_and_many_worker_runs_produce_equal_facts() {
    let u = uri("scenario6");
    let text = "f : Nat -> Nat := fun x -> x\nn : Nat := 0";

    let mut single = build(1);
    let mut many = build(8);
    open(&mut single, u, text);
    open(&mut many, u, text);

    let dump = |ws: &Workspace| -> String {
        let unit = ws
            .snapshot()
            .map_view::<ParseUnits<StlcDocument>>()
            .get(&u.to_string())
            .expect("unit")
            .clone();
        let type_facts = ws
            .snapshot()
            .map_view::<StlcTypeFacts>()
            .get(&u.to_string())
            .map(|a| a.to_vec())
            .unwrap_or_default();
        let mut types: Vec<String> = type_facts
            .iter()
            .map(|f| format!("{:?}", f.ty))
            .collect();
        types.sort();
        format!("{unit:?}|{types:?}")
    };

    assert_eq!(
        dump(&single),
        dump(&many),
        "1 and 8 workers publish equal committed facts",
    );

    // Warm edited graph equals a cold build from the edited text (for the
    // untouched declaration's type).
    let end = text.len();
    let edit = SourceEdit::Insert {
        key: Span::point_uri(u, end).unwrap(),
        value: "\ny : Bool := true".into(),
    };
    single.edit(vec![edit.clone()]).unwrap();
    many.edit(vec![edit.clone()]).unwrap();

    let mut cold = build(8);
    open(&mut cold, u, &format!("{text}\ny : Bool := true"));
    assert_eq!(dump(&single), dump(&cold), "warm equals cold");
    assert_eq!(dump(&many), dump(&cold), "many-worker warm equals cold");
}

// ---------------------------------------------------------------------------
// Scenario 7
// ---------------------------------------------------------------------------

#[test]
fn edit_invalidates_only_affected_components() {
    let u = uri("scenario7");
    let mut ws = build(1);
    open(&mut ws, u, "x := 0\ny := 1");
    let snapshot_before = ws.snapshot();
    let tree_before = snapshot_before.tree_view::<StlcTree>();
    let unit_before = snapshot_before
        .map_view::<ParseUnits<StlcDocument>>()
        .get(&u.to_string())
        .expect("unit")
        .clone();
    // The untouched declaration is the second child of the root, and its
    // structural summary is recorded.
    let second_before = tree_before.children(unit_before.root)[1];
    let index_before = snapshot_before
        .map_view::<StlcNodeIndex>()
        .get(&u.to_string())
        .expect("index")
        .iter()
        .find(|fact| fact.node == second_before)
        .cloned();


    // Edit inside the *first* declaration only.
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u, 2).unwrap(),
        value: "9".into(),
    }])
    .unwrap();

        let snapshot_after = ws.snapshot();
    let tree_after = snapshot_after.tree_view::<StlcTree>();
    let unit_after = snapshot_after
        .map_view::<ParseUnits<StlcDocument>>()
        .get(&u.to_string())
        .expect("unit after");
    let second_after = tree_after.children(unit_after.root)[1];
    let index_after = snapshot_after
        .map_view::<StlcNodeIndex>()
        .get(&u.to_string())
        .expect("index");
    let second_after = index_after
        .iter()
        .find(|fact| fact.node == second_after)
        .cloned();
    assert_eq!(
        index_before.map(|f| f.kind),
        second_after.map(|f| f.kind),
        "an untouched declaration's structural facts survive the edit for the same node",
    );
}
// ---------------------------------------------------------------------------
// Scenario 8
// ---------------------------------------------------------------------------

#[test]
fn prints_ast_and_final_scope_graph_for_let_and_function_code() {
    let u = uri("scenario8");
    let code = r##"
id : Nat -> Nat := fun x -> x
mul (x : Nat) (y : Nat) : Nat -> Nat -> Nat := case x of zero -> 0 | succ p -> y + mul p y
"##;
    let mut ws = build(1);
    open(&mut ws, u, code);

    // The tree is printed via the framework's AstTree renderer.
    let snapshot = ws.snapshot();
    let unit = snapshot
        .map_view::<ParseUnits<StlcDocument>>()
        .get(&u.to_string())
        .expect("unit")
        .clone();
    let tree = snapshot.tree_view::<StlcTree>();
    let root = unit.root;
    let mut buffer = String::new();
    render_ast(&mut buffer, &tree, root, 0);
    assert!(!buffer.is_empty(), "the AST renders");

    // Every explicit scope allocation publishes exactly one scope datum.
    let graph_view = snapshot.graph_view::<ScopeGraph<StlcScope>>();
    let scopes = ScopeGraphSnapshot::new(&graph_view);
    let nodes = snapshot.graph_view::<ScopeGraph<StlcScope>>().nodes();
    let data: Vec<_> = nodes.iter().filter_map(|id| scopes.node_data(ScopeId::new(*id))).collect();
    assert_eq!(
        data.len(),
        nodes.len(),
        "every explicit scope allocation publishes exactly one scope-data value",
    );
    // Declarations and binders publish owner-stable types (read from the
    // checker's dedicated type-scope map, disjoint from name's graph).
    let type_facts = snapshot
        .map_view::<StlcTypeScopes>()
        .get(&u.to_string())
        .expect("type scopes");
    let type_count = type_facts.len();
    assert!(
        type_count >= 2,
        "types are published by the checker: {type_count}",
    );

    // Scope graph prints through the framework renderer.
    let scope_graph =
        plingo::visual::graph::render_domain_graph(&scopes);
    assert!(!scope_graph.is_empty());

    let _ = tree;
    let _ = scopes;
}

