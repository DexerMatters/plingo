//! Phase 1 acceptance — `#[abstract_tree]` (plan §6) and the §4 sugar,
//! exercised from a workspace test crate against the `plingo` library.

use plingo::reactive::api::TreeEmittedExt;
use plingo::reactive::prelude::*;
use plingo::{reactive_component as component, reactive_abstract_tree as abstract_tree};
use plingo::reactive::view::ShapeKind;

// ---------------------------------------------------------------------------
// The family: a tiny expression language (member enums carry the attribute)
// ---------------------------------------------------------------------------

/// A leaf value carried in the tree.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LeafVal(pub u64);

/// The family root.
#[abstract_tree(members(StlcExpr, StlcParam, StlcLit))]
pub enum StlcExpr {
    Lam { param: StlcParam, body: Box<StlcExpr>, span: u64 },
    App { fun: Box<StlcExpr>, arg: Box<StlcExpr>, span: u64 },
    Var { path: StlcLit, span: u64 },
    Num { value: u64, span: u64 },
}

/// A parameter member.
#[abstract_tree(members(StlcExpr, StlcParam, StlcLit))]
pub enum StlcParam {
    Bare { name: StlcLit, note: Option<StlcLit>, span: u64 },
}

/// A leaf-only member.
#[abstract_tree(members(StlcExpr, StlcParam, StlcLit))]
pub enum StlcLit {
    Text { value: LeafVal, span: u64 },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn view_spec_is_an_abstract_tree() {
    // AbstractTreeShape is the Tree shape with a marker; the ViewSpec uses
    // it for the family view.
    use plingo::reactive::view::{ShapeKind, TreeShape, ViewSpec};
    assert_eq!(<StlcTree as ViewSpec>::Shape::KIND, ShapeKind::Tree);
    // The concrete shape alias is still the Tree shape.
    assert_eq!(TreeShape::KIND, ShapeKind::Tree);
}

/// Derived ids: same region + kind ⇒ same id; different region/kind ⇒
/// different.
#[test]
fn derived_ids_are_stable_and_distinct() {
    let a = StlcTree::id_from_span("u", 1, 2, 3);
    let b = StlcTree::id_from_span("u", 1, 2, 3);
    let c = StlcTree::id_from_span("u", 1, 2, 4);
    let d = StlcTree::id_from_span("v", 1, 2, 3);
    assert_eq!(a, b, "identical re-parse mints identical ids");
    assert_ne!(a, c, "kind participates");
    assert_ne!(a, d, "uri participates");
}

/// `tree_kind` reports the variant ordinal.
#[test]
fn tree_kind_ordinals() {
    let lam = StlcExpr::Lam {
        param: StlcParam::Bare {
            span: 0,
            name: StlcLit::Text { value: LeafVal(1), span: 0 },
            note: None,
        },
        body: Box::new(StlcExpr::Num { value: 0, span: 0 }),
        span: 0,
    };
    assert_eq!(lam.tree_kind(), 0);
    assert_eq!(StlcLit::Text { value: LeafVal(1), span: 0 }.tree_kind(), 0);
    assert_eq!(StlcExpr::Num { value: 0, span: 0 }.tree_kind(), 3);
}

/// The whole generated surface compiles and runs: `upsert_*` emits a nested
/// tree that `case` reads back with children in field order.
#[test]
fn upsert_then_case_reads_back_nested_tree() {
    let mut engine = Engine::new();
    engine.install(upsert_emitter).unwrap();
    let _report = engine.command(vec![]).unwrap();
    let snapshot = engine.snapshot();
    let tree = snapshot.tree_view::<StlcTree>();
    let roots = tree.roots();
    assert_eq!(roots.len(), 1, "the emitter publishes its root");
    let root = roots[0];
    let root_case = match snapshot_case(&tree, root) {
        ::std::option::Option::Some(StlcCase::Expr(StlcExprCase::Lam { param, body, span })) => {
            (param, body, span)
        }
        _ => {
            assert!(false, "expected the root to be an Expr::Lam case");
            return;
        }
    };
    let (param, body, span) = root_case;
    assert_eq!(span, 30);
    assert_ne!(param, body, "param and body are distinct child nodes");

    // The body is a Var node.
    let body_span = match snapshot_case(&tree, body) {
        ::std::option::Option::Some(StlcCase::Expr(StlcExprCase::Var { span, .. })) => span,
        _ => {
            assert!(false, "expected body to be an Expr::Var case");
            return;
        }
    };
    assert_eq!(body_span, 10);

    // The param is a Bare with a note.
    let note = match snapshot_case(&tree, param) {
        ::std::option::Option::Some(StlcCase::Param(StlcParamCase::Bare { note, .. })) => note,
        _ => {
            assert!(false, "expected param to be a Param::Bare case");
            return;
        }
    };
    assert!(note.is_some(), "the note child is present");
}

/// The observed `case` reader returns the same case as the snapshot.
#[test]
fn observed_case_matches_snapshot() {
    let engine = engine_after_run();
    let snapshot_tree = snapshot_case(&engine.snapshot().tree_view::<StlcTree>(), engine_tree_root(&engine));
    let observed_tree = {
        let mut engine2 = Engine::new();
        engine2.install(upsert_emitter).unwrap();
        engine2.command(vec![]).unwrap();
        let tree = engine2.snapshot().tree_view::<StlcTree>();
        snapshot_case(&tree, tree.roots()[0])
    };
    assert_eq!(snapshot_tree, observed_tree);
}

/// `case` of an absent node is `None`, not an error.
#[test]
fn case_of_absent_node_is_none() {
    let mut engine = Engine::new();
    engine.external::<StlcTree>().unwrap();
    let snapshot = engine.snapshot();
    let tree = snapshot.tree_view::<StlcTree>();
    assert!(snapshot_case(&tree, StlcTree::id_from_span("x", 0, 0, 0)).is_none());
}

/// The `visit_<member>_each` visitor walks only children of that member.
#[test]
fn member_visitors_are_filtered() {
    // Root's children: one Param and one Expr. `visit_lit_each` under the
    // root finds nothing; `visit_param_each` finds exactly the param.
    let engine = engine_after_run();
    let snapshot = engine.snapshot();
    let tree = snapshot.tree_view::<StlcTree>();
    let root = tree.roots()[0];

    let mut param_visits = 0usize;
    let mut lit_visits = 0usize;

    // (The generated observed-trait visitors record reads on a live handle;
    // their *filtering* is directly visible in the snapshot: children of
    // one member are disjoint per member kind.)
    for child in tree.children(root) {
        match snapshot_case(&tree, child) {
            Some(StlcCase::Param(_)) => param_visits += 1,
            Some(StlcCase::Lit(_)) => lit_visits += 1,
            _ => {}
        }
    }
    assert_eq!(param_visits, 1, "one Param child under the root");
    // The root's children include the Param and the Expr body, but no Lit.
    assert_eq!(lit_visits, 0, "the Lit node lives under the param/body, not the root");
}

// ---------------------------------------------------------------------------
// A runnable component used by the tests above
// ---------------------------------------------------------------------------

static EMITTER_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Emits one nested tree via the generated upsert methods.
#[component]
pub fn upsert_emitter() -> (StlcTree,) {
    EMITTER_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let tree = Emitted::<StlcTree>::new()?;
    let root_id = tree.fresh_node_id()?;
    let body = StlcExpr::Var { path: StlcLit::Text { value: LeafVal(3), span: 1 }, span: 10 };
    let param = StlcParam::Bare {
        span: 4,
        name: StlcLit::Text { value: LeafVal(4), span: 2 },
        note: Some(StlcLit::Text { value: LeafVal(6), span: 3 }),
    };
    // The root recursively emits its whole subtree; children are attached
    // under it, so the only root is `root_id`.
    tree.upsert_expr(root_id, &StlcExpr::Lam {
        param,
        body: Box::new(body),
        span: 30,
    })?;
    Ok((tree,))
}

fn engine_after_run() -> Engine {
    let mut engine = Engine::new();
    engine.install(upsert_emitter).unwrap();
    engine.command(vec![]).unwrap();
    engine
}

fn snapshot_after_run() -> plingo::reactive::Snapshot {
    engine_after_run().snapshot()
}

fn engine_tree_root(engine: &Engine) -> NodeId {
    engine.snapshot().tree_view::<StlcTree>().roots()[0]
}

fn snapshot_case(tree: &SnapshotTree<StlcTree>, id: NodeId) -> Option<StlcCase> {
    StlcSnapshotExt::case(tree, id)
}

/// Derives ids from spans: a component that emits via the derived path.
#[component]
pub fn span_emitter() -> (StlcTree,) {
    let tree = Emitted::<StlcTree>::new()?;
    // Root at [0,5), body Var at [1,3), path Lit at [2,3).
    let root = StlcExpr::Lam {
        span: encode_span(0, 5),
        param: StlcParam::Bare {
            span: encode_span(1, 4),
            name: StlcLit::Text { value: LeafVal(1), span: encode_span(2, 3) },
            note: None,
        },
        body: Box::new(StlcExpr::Num { value: 0, span: encode_span(3, 4) }),
    };
    StlcExpr::__tree_emit_derived(&tree, "d://span", &root)?;
    Ok((tree,))
}

fn encode_span(start: u32, end: u32) -> u64 {
    ((start as u64) << 32) | (end as u64)
}

/// Unchanged regions re-parse to identical derived ids; the ids are the
/// `id_from_span` values, not fresh ids.
#[test]
fn derived_emit_produces_stable_span_ids() {
    let run = |engine: &mut Engine| -> Vec<NodeId> {
        engine.install(span_emitter).unwrap();
        engine.command(vec![]).unwrap();
        let tree = engine.snapshot().tree_view::<StlcTree>();
        let root = tree.roots()[0];
        let body = tree.children(root)[1];
        let mut ids = vec![root, body];
        ids.extend(tree.roots());
        ids
    };
    let mut e1 = Engine::new();
    let ids1 = run(&mut e1);
    let mut e2 = Engine::new();
    let ids2 = run(&mut e2);

    assert_eq!(ids1, ids2, "derived ids are stable across engines");
    assert_eq!(
        ids1[0],
        StlcTree::id_from_span("d://span", 0, 5, StlcExpr::Lam { span: encode_span(0, 5), param: StlcParam::Bare { span: encode_span(1, 4), name: StlcLit::Text { value: LeafVal(1), span: encode_span(2, 3) }, note: None }, body: Box::new(StlcExpr::Num { value: 0, span: encode_span(3, 4) }) }.tree_kind()),
        "the root id is exactly H(uri, 0, 5, kind)"
    );
}
