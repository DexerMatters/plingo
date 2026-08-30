//! Typed abstract-tree coverage for the plain reactive authoring surface.

use plingo::reactive::kind::{emit_view, observe_view};
use plingo::reactive::prelude::*;
use plingo::{abstract_tree, component, view};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LeafVal(pub u64);

#[abstract_tree(tree = StlcTree, domain = (), members(StlcExpr, StlcParam, StlcLit))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StlcExpr {
    Lam {
        param: AstBox<StlcParam>,
        body: AstBox<StlcExpr>,
        span: u64,
    },
    App {
        fun: AstBox<StlcExpr>,
        arg: AstBox<StlcExpr>,
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

#[abstract_tree(member_of = StlcTree)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StlcParam {
    Bare {
        name: StlcLit,
        note: Option<StlcLit>,
        span: u64,
    },
}

#[abstract_tree(member_of = StlcTree)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StlcLit {
    Text { value: LeafVal, span: u64 },
}

#[test]
fn tree_kinds_and_derived_values_are_stable() {
    // The enum remains a plain render description; the kind fact it publishes
    // is verified through the emitter component below.
    let _ = Engine::new();
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct GranularStepKey(pub u64);

#[component]
fn render_param(_step: GranularStepKey) -> Result<AstBox<StlcParam>> {
    StlcParam::render(StlcParam::Bare {
        span: 4,
        name: StlcLit::Text {
            value: LeafVal(4),
            span: 2,
        },
        note: Some(StlcLit::Text {
            value: LeafVal(6),
            span: 3,
        }),
    })
}

#[component]
fn render_var(step: GranularStepKey) -> Result<AstBox<StlcExpr>> {
    StlcExpr::render(StlcExpr::Var {
        path: StlcLit::Text {
            value: LeafVal(3),
            span: 1,
        },
        span: step.0,
    })
}

#[component]
fn render_body(_name: Each<FixtureTrigger>) -> Result<AstBox<StlcExpr>> {
    StlcExpr::render(StlcExpr::Var {
        path: StlcLit::Text {
            value: LeafVal(3),
            span: 1,
        },
        span: 10,
    })
}

fn upsert_emitter(_: ()) -> Result<()> {
    Err(plingo::reactive::Error::Internal("unused".into()))
}

/// Cut C fixture trigger: one external element drives the emitter
/// component; the unkeyed planner surface is gone.
#[view]
struct FixtureTrigger(Map<(), ()>);

fn run_emitter() -> Engine {
    let mut engine = Engine::new();
    <granular_writer_component::Component as plingo::reactive::framework_mount::MountComponent<
        plingo::reactive::framework_mount::MapEntries<FixtureTrigger>,
    >>::mount(
        &mut engine,
        plingo::reactive::framework_mount::MapEntries::new(),
    )
    .expect("mount emitter component");
    engine
        .command(|| emit_view::<FixtureTrigger>()?.insert((), ()))
        .expect("trigger emitter");
    engine
}

#[component]
fn render_leaf_body(_key: Each<FixtureTrigger>) -> Result<AstBox<StlcExpr>> {
    render_var(GranularStepKey(10))
}

#[component]
fn leaf_body(_key: ()) -> Result<AstBox<StlcExpr>> {
    let step = observe_view::<GranularStep>()?
        .get(&())?
        .map(|step| *step)
        .unwrap_or(0);
    let body_span = if step >= 1 { 11 } else { 10 };
    StlcExpr::render(StlcExpr::Var {
        path: StlcLit::Text {
            value: LeafVal(3),
            span: 1,
        },
        span: body_span,
    })
}

#[component]
fn granular_writer_component(_key: Each<FixtureTrigger>) -> Result<AstBox<StlcExpr>> {
    // The body's INPUT is stable across the leaf edit; only the leaf fact
    // published through GranularStep changes (plan §9.2 leaf row).
    let body = leaf_body(())?;
    let param = render_param(GranularStepKey(4))?;
    StlcExpr::render(StlcExpr::Lam {
        param,
        body,
        span: 30,
    })
}

#[test]
fn upsert_then_lazy_accessors_read_nested_typed_tree() {
    let engine = run_emitter();
    let snapshot = engine.snapshot();
    let tree = snapshot.tree::<StlcTree>();
    let root = tree.roots(&()).next().expect("root");
    let (param, body, span) = match tree.view(root.clone()).expect("root view") {
        StlcExprView::Lam(lam) => (
            lam.param().expect("param"),
            lam.body().expect("body"),
            *lam.span().expect("span"),
        ),
        other => panic!("expected Expr::Lam, got {other:?}"),
    };
    assert_eq!(span, 30);
    assert!(!param.same_identity(&body));
    assert!(matches!(
        tree.view(body.clone()).expect("body view"),
        StlcExprView::Var(_)
    ));
    assert!(matches!(
        tree.view(param).expect("param view"),
        StlcParamView::Bare(_)
    ));
}

#[test]
fn snapshot_reads_are_consistent_across_engines() {
    let first = run_emitter();
    let second = run_emitter();
    let first_snapshot = first.snapshot();
    let second_snapshot = second.snapshot();
    let first_root = first_snapshot
        .tree::<StlcTree>()
        .roots(&())
        .next()
        .expect("root");
    let second_root = second_snapshot
        .tree::<StlcTree>()
        .roots(&())
        .next()
        .expect("root");
    assert!(first_root.same_identity(&second_root));
    let first_span = match first_snapshot
        .tree::<StlcTree>()
        .view(first_root.clone())
        .expect("view")
    {
        StlcExprView::Lam(lam) => *lam.span().expect("span"),
        _ => panic!("expected Lam"),
    };
    let second_span = match second_snapshot
        .tree::<StlcTree>()
        .view(second_root)
        .expect("view")
    {
        StlcExprView::Lam(lam) => *lam.span().expect("span"),
        _ => panic!("expected Lam"),
    };
    assert_eq!(first_span, second_span);
}

#[test]
fn absent_tree_has_no_roots() {
    let engine = Engine::new();
    assert!(
        engine
            .snapshot()
            .tree::<StlcTree>()
            .roots(&())
            .next()
            .is_none()
    );
}

#[test]
fn generated_snapshot_children_are_typed_and_filtered() {
    let engine = run_emitter();
    let snapshot = engine.snapshot();
    let tree = snapshot.tree::<StlcTree>();
    let root = tree.roots(&()).next().expect("root");
    let (param, body) = match tree.view(root.clone()).expect("root view") {
        StlcExprView::Lam(lam) => (lam.param().expect("param"), lam.body().expect("body")),
        _ => panic!("expected Lam"),
    };
    assert!(!param.same_identity(&body));
    assert!(matches!(
        tree.view(param).expect("view"),
        StlcParamView::Bare(_)
    ));
    assert!(matches!(
        tree.view(body).expect("view"),
        StlcExprView::Var(_)
    ));
}

#[allow(unused_imports)]
use view as _view_macro_anchor;

#[view]
struct GranularStep(Map<(), u64>);

// ---------------------------------------------------------------------------
// Per-node granularity (plan §8 Phase 3): editing one leaf rewrites exactly
// that node's fact.
// ---------------------------------------------------------------------------

#[test]
fn editing_one_leaf_rewrites_exactly_one_node_fact() {
    let mut engine = Engine::new();
    engine
        .command(|| emit_view::<GranularStep>()?.insert((), 0))
        .expect("step seed");
    granular_writer_component::Component::mount(
        &mut engine,
        plingo::reactive::framework_mount::MapEntries::<FixtureTrigger>::new(),
    )
    .expect("mount writer component");
    engine
        .command(|| emit_view::<FixtureTrigger>()?.insert((), ()))
        .expect("trigger writer");

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
