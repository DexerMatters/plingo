//! Typed abstract-tree coverage for the plain reactive authoring surface.

use plingo::reactive::prelude::*;
use plingo::{abstract_tree, view};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LeafVal(pub u64);

#[abstract_tree(members(StlcExpr, StlcParam, StlcLit))]
pub enum StlcExpr {
    Lam {
        param: StlcParam,
        body: Box<StlcExpr>,
        span: u64,
    },
    App {
        fun: Box<StlcExpr>,
        arg: Box<StlcExpr>,
        span: u64,
    },
    Var {
        path: StlcLit,
        span: u64,
    },
    Num {
        value: u64,
        span: u64,
    },
}

#[abstract_tree(members(StlcExpr, StlcParam, StlcLit))]
pub enum StlcParam {
    Bare {
        name: StlcLit,
        note: Option<StlcLit>,
        span: u64,
    },
}

#[abstract_tree(members(StlcExpr, StlcParam, StlcLit))]
pub enum StlcLit {
    Text { value: LeafVal, span: u64 },
}

#[test]
fn tree_kinds_and_derived_values_are_stable() {
    let lam = StlcExpr::Lam {
        param: StlcParam::Bare {
            span: 0,
            name: StlcLit::Text {
                value: LeafVal(1),
                span: 0,
            },
            note: None,
        },
        body: Box::new(StlcExpr::Num { value: 0, span: 0 }),
        span: 0,
    };
    assert_eq!(lam.tree_kind(), 0);
    assert_eq!(
        StlcLit::Text {
            value: LeafVal(1),
            span: 0
        }
        .tree_kind(),
        0
    );
    assert_eq!(StlcExpr::Num { value: 0, span: 0 }.tree_kind(), 3);
}

fn upsert_emitter(_: ()) -> Result<()> {
    let root = StlcTree::emit_root(&StlcExpr::Lam {
        param: StlcParam::Bare {
            span: 4,
            name: StlcLit::Text {
                value: LeafVal(4),
                span: 2,
            },
            note: Some(StlcLit::Text {
                value: LeafVal(6),
                span: 3,
            }),
        },
        body: Box::new(StlcExpr::Var {
            path: StlcLit::Text {
                value: LeafVal(3),
                span: 1,
            },
            span: 10,
        }),
        span: 30,
    })?;
    StlcTree::emit_roots(vec![root])
}

fn run_emitter<F>(function: F) -> (Engine, Running<()>)
where
    F: Fn(()) -> Result<()> + Clone + Send + Sync + 'static,
{
    let mut engine = Engine::new();
    let plan = engine.plan(function, ()).expect("plan");
    let running = engine.run(&plan).expect("run");
    (engine, running)
}

#[test]
fn upsert_then_snapshot_case_reads_nested_typed_tree() {
    let (engine, _running) = run_emitter(upsert_emitter);
    let snapshot = engine.snapshot();
    let roots = StlcTree::snapshot_roots(&snapshot);
    assert_eq!(roots.len(), 1);
    let root = roots[0];
    let (param, body, span) = match StlcTree::snapshot_case(&snapshot, root) {
        Some(StlcCase::Expr(StlcExprCase::Lam { param, body, span })) => (param, body, span),
        other => panic!("expected Expr::Lam, got {other:?}"),
    };
    assert_eq!(span, 30);
    assert_ne!(param, body);
    assert!(matches!(
        StlcTree::snapshot_case(&snapshot, body),
        Some(StlcCase::Expr(StlcExprCase::Var { span: 10, .. }))
    ));
    assert!(matches!(
        StlcTree::snapshot_case(&snapshot, param),
        Some(StlcCase::Param(StlcParamCase::Bare { note: Some(_), .. }))
    ));
}

#[test]
fn snapshot_case_is_consistent_across_engines() {
    let (first, _) = run_emitter(upsert_emitter);
    let (second, _) = run_emitter(upsert_emitter);
    let first_snapshot = first.snapshot();
    let second_snapshot = second.snapshot();
    let first_root = StlcTree::snapshot_roots(&first_snapshot)[0];
    let second_root = StlcTree::snapshot_roots(&second_snapshot)[0];
    assert_eq!(first_root, second_root);
    assert_eq!(
        StlcTree::snapshot_case(&first_snapshot, first_root),
        StlcTree::snapshot_case(&second_snapshot, second_root)
    );
}

#[test]
fn absent_tree_has_no_roots() {
    let engine = Engine::new();
    assert!(StlcTree::snapshot_roots(&engine.snapshot()).is_empty());
}

#[test]
fn generated_snapshot_children_are_typed_and_filtered() {
    let (engine, _) = run_emitter(upsert_emitter);
    let snapshot = engine.snapshot();
    let root = StlcTree::snapshot_roots(&snapshot)[0];
    let children = StlcTree::snapshot_children(&snapshot, root);
    assert_eq!(children.len(), 2);
    assert!(matches!(
        StlcTree::snapshot_case(&snapshot, children[0]),
        Some(StlcCase::Param(_))
    ));
    assert!(matches!(
        StlcTree::snapshot_case(&snapshot, children[1]),
        Some(StlcCase::Expr(_))
    ));
}

#[allow(unused_imports)]
use view as _view_macro_anchor;

// ---------------------------------------------------------------------------
// Per-node granularity (plan §8 Phase 3): editing one leaf rewrites exactly
// that node's fact.
// ---------------------------------------------------------------------------

#[view]
struct GranularStep(Map<(), u64>);

fn granular_writer(_: ()) -> Result<()> {
    let step = observe_view::<GranularStep>()?
        .get(&())?
        .map(|step| *step)
        .unwrap_or(0);
    let body = StlcExpr::Var {
        path: StlcLit::Text {
            value: LeafVal(3),
            span: 1,
        },
        span: if step >= 1 { 11 } else { 10 },
    };
    let root = StlcTree::emit_root(&StlcExpr::Lam {
        param: StlcParam::Bare {
            span: 4,
            name: StlcLit::Text {
                value: LeafVal(4),
                span: 2,
            },
            note: None,
        },
        body: Box::new(body),
        span: 30,
    })?;
    // Total republication: every maintained fact is rewritten, but only the
    // genuinely changed units publish (T4).
    StlcTree::replace_roots_of(StlcTree::anonymous_key(), vec![root])
}

#[test]
fn editing_one_leaf_rewrites_exactly_one_node_fact() {
    let mut engine = Engine::new();
    engine
        .command(|| emit_view::<GranularStep>()?.insert((), 0))
        .expect("step seed");
    let plan = engine.plan(granular_writer, ()).expect("plan");
    engine.run(&plan).expect("run");

    let report = engine
        .command(|| emit_view::<GranularStep>()?.insert((), 1))
        .expect("leaf edit");
    assert_eq!(
        report.changed::<StlcTree>(),
        1,
        "one leaf edit must rewrite exactly one tree fact"
    );

    // An unrelated edit publishes nothing for the tree.
    let report = engine
        .command(|| emit_view::<GranularStep>()?.insert((), 1))
        .expect("equal step");
    assert_eq!(report.changed::<StlcTree>(), 0);
}
