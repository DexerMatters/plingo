//! Scope graph coverage for plain effects and typed committed snapshots.

use std::sync::Arc;

use plingo::framework::scope::{
    PathExpr, PathOrder, ResolutionPath, ScopeDomain, ScopeGraph, ScopeNode, ScopeRequirements,
    declare, edge, partition_visible, scope, snapshot_declarations, snapshot_node, snapshot_nodes,
    snapshot_outgoing, snapshot_scope,
};
use plingo::reactive::component::EachKey;
use plingo::reactive::prelude::*;
use reactive_macros::{component, view};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LabelKind {
    Declare,
    Use,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Data {
    Module,
    Bound(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Request {
    Load(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Domain;

impl ScopeDomain for Domain {
    type ScopeData = Data;
    type Label = LabelKind;
    type Request = Request;
}

/// Cut C fixture trigger: one external element drives emitter components.
#[view]
struct FixtureTrigger(Map<(), ()>);

fn install_emitter(engine: &mut Engine) {
    emitter_component_install(engine).expect("install emitter");
    trigger(engine);
}

fn install_resolver(engine: &mut Engine) {
    resolver_component_install(engine).expect("install resolver");
    trigger(engine);
}

fn install_req_emitter(engine: &mut Engine) {
    req_emitter_component_install(engine).expect("install req emitter");
    trigger(engine);
}

fn trigger(engine: &mut Engine) {
    engine
        .command(|| emit_view::<FixtureTrigger>()?.insert((), ()))
        .expect("trigger fixture component");
}

#[component]
fn emitter_component(_key: EachKey<FixtureTrigger>) -> Result<()> {
    emitter(())
}

#[component]
fn resolver_component(_key: EachKey<FixtureTrigger>) -> Result<()> {
    resolver(())
}

#[component]
fn req_emitter_component(_key: EachKey<FixtureTrigger>) -> Result<()> {
    req_emitter(())
}

#[component]
fn emitter_second_component(_key: EachKey<FixtureTrigger>) -> Result<()> {
    emitter(())
}

#[test]
fn emission_and_typed_snapshot_round_trip() {
    let mut engine = Engine::new();
    install_emitter(&mut engine);
    let snapshot = engine.snapshot();
    let nodes = snapshot_nodes::<Domain>(&snapshot);

    assert_eq!(nodes.len(), 4, "module + lex scope + 2 declarations");
    let data: Vec<String> = nodes
        .iter()
        .filter_map(|node| snapshot_scope(&snapshot, node.clone()).map(|data| format!("{data:?}")))
        .collect();
    assert!(data.contains(&format!("{:?}", Data::Module)));
    assert!(nodes.iter().any(|node| {
        matches!(
            snapshot_node(&snapshot, node.clone()).as_deref(),
            Some(ScopeNode::Declaration(_))
        )
    }));
}

#[test]
fn identical_scope_construction_is_shared_across_roots() {
    let mut engine = Engine::new();
    // Cut C identity: each definition owns its own node copies (the
    // definition participates in automatic ids), so both owners coexist
    // and removal retracts exactly that owner's set.
    let first = emitter_component_install(&mut engine).expect("first install");
    let second = emitter_second_component_install(&mut engine).expect("second install");
    trigger(&mut engine);

    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot_nodes::<Domain>(&snapshot).len(),
        8,
        "two definitions each own their four nodes"
    );

    engine.remove_keyed(&first).expect("remove first root");
    assert_eq!(
        snapshot_nodes::<Domain>(&engine.snapshot()).len(),
        4,
        "survivor keeps exactly its own construction"
    );

    engine.remove_keyed(&second).expect("remove second root");
    assert!(snapshot_nodes::<Domain>(&engine.snapshot()).is_empty());
}

#[test]
fn typed_snapshot_reads_outgoing_and_declaration_buckets() {
    let mut engine = Engine::new();
    install_emitter(&mut engine);
    let snapshot = engine.snapshot();
    let nodes = snapshot_nodes::<Domain>(&snapshot);
    // Node enumeration is canonical, not first-created order; locate the
    // module scope structurally: the Scope(Module) node whose Declare
    // bucket owns exactly the document lexical scope.
    let module = nodes
        .iter()
        .cloned()
        .find(|node| {
            matches!(
                snapshot
                    .graph_node::<ScopeGraph<Domain>>(node.node())
                    .as_deref(),
                Some(ScopeNode::Scope(Data::Module))
            ) && snapshot_outgoing(&snapshot, node.clone(), &LabelKind::Declare).len() == 1
        })
        .expect("module scope present");
    let lex = snapshot_outgoing(&snapshot, module, &LabelKind::Declare)[0].clone();
    let declarations = snapshot_declarations(&snapshot, lex, &LabelKind::Declare);
    assert_eq!(declarations.len(), 2);
    let data: Vec<_> = declarations
        .iter()
        .filter_map(|node| snapshot_node(&snapshot, node.clone()))
        .filter_map(|node| match node.as_ref() {
            ScopeNode::Declaration(data) => Some(data.clone()),
            ScopeNode::Scope(_) | ScopeNode::Reference(_) => None,
        })
        .collect();
    assert_eq!(data, vec![Data::Bound("a".into()), Data::Bound("b".into())]);
}

#[test]
fn path_expr_algebra_is_unchanged() {
    let p = PathExpr::Label(LabelKind::Declare)
        .star()
        .then(PathExpr::Label(LabelKind::Use));
    assert!(PathExpr::Label(LabelKind::Declare).star().nullable());
    assert!(!p.nullable());
    let d = p.derivative(&LabelKind::Declare);
    assert_eq!(d.labels(), vec![LabelKind::Declare, LabelKind::Use]);
    let d2 = d.derivative(&LabelKind::Declare);
    assert!(!d2.nullable());
    assert_eq!(d2.labels(), vec![LabelKind::Declare, LabelKind::Use]);
    assert!(d2.derivative(&LabelKind::Use).nullable());
}

#[test]
fn partition_visible_honors_path_order() {
    let order = PathOrder::new().prefer(LabelKind::Use, LabelKind::Declare);
    let make = |labels: Vec<LabelKind>| ResolutionPath::<Domain> {
        scopes: Arc::from([]),
        labels: labels.into(),
        data: Data::Module,
    };
    let visible = make(vec![LabelKind::Use]);
    let shadowed = make(vec![LabelKind::Declare]);
    let set: std::collections::HashSet<_> = [visible.clone(), shadowed.clone()].into();
    let (visible_paths, dominated) = partition_visible(set, &order);
    assert_eq!(visible_paths, vec![visible]);
    assert_eq!(dominated.len(), 1);
    assert_eq!(dominated[0].0, shadowed);
}

#[test]
fn requirements_map_is_observable() {
    let mut engine = Engine::new();
    install_req_emitter(&mut engine);
    let request = engine
        .snapshot()
        .observe::<ScopeRequirements<Domain>>("a://doc".to_string());
    assert_eq!(
        request.map(|value| (*value).clone()),
        Some(vec![Request::Load("other".into())])
    );
}
#[test]
fn resolver_effect_reads_graph_during_a_reactive_run() {
    let mut engine = Engine::new();
    install_emitter(&mut engine);
    install_resolver(&mut engine);
    assert!(engine.snapshot().inputs::<ScopeGraph<Domain>>().len() >= 4);
}
fn emitter(_: ()) -> Result<()> {
    let module = scope::<Domain>(Data::Module)?;
    let lex = scope::<Domain>(Data::Module)?;
    edge(module.clone(), LabelKind::Declare, lex.clone())?;
    let a = declare(lex.clone(), LabelKind::Declare, Data::Bound("a".into()))?;
    let b = declare(lex.clone(), LabelKind::Declare, Data::Bound("b".into()))?;
    edge(lex.clone(), LabelKind::Use, a)?;
    edge(lex, LabelKind::Use, b)?;
    Ok(())
}

fn resolver(_: ()) -> Result<()> {
    let observe = plingo::reactive::kind::observe_view::<ScopeGraph<Domain>>()?;
    assert!(!observe.nodes()?.is_empty());
    Ok(())
}

fn req_emitter(_: ()) -> Result<()> {
    plingo::reactive::kind::emit_view::<ScopeRequirements<Domain>>()?
        .insert("a://doc".to_string(), vec![Request::Load("other".into())])
}
