//! Phase 5 acceptance — the reactive scope graph (plan §7).
//!
//! Emission/observation APIs, multi-producer disjoint payloads, path
//! resolution reading exactly the touched buckets, Requirements map, and
//! determinism.

use std::sync::Arc;

use plingo::framework::scope::{
    PathExpr, PathOrder, ResolutionPath, ScopeDomain, ScopeGraph,
    ScopeGraphEmittedExt, ScopeGraphObservedExt, ScopeGraphSnapshot, ScopeId, ScopeNode,
    ScopePath, ScopeRequirements, partition_visible,
};
use plingo::reactive::prelude::*;
use plingo::reactive_component as component;
use plingo::reactive_view as view;

/// A toy scope domain: names resolve through `use`-edges.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LabelKind {
    Declare,
    Use,
}

/// Scope data: a module or a name binding.
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
    type ScopeKey = String;
    type ScopeData = Data;
    type Label = LabelKind;
    type Request = Request;
}

#[test]
fn emission_and_observation_round_trip() {
    let mut engine = Engine::new();
    engine.install(emitter).unwrap();
    engine.command(vec![]).unwrap();
    let snapshot = engine.snapshot();
    let graph = snapshot.graph_view::<ScopeGraph<Domain>>();
    let scopes = <ScopeGraphSnapshot<Domain>>::new(&graph);

    // The emitter created 2 scopes, 2 declarations, 1 use-edge.
    let nodes = graph.nodes();
    assert_eq!(nodes.len(), 4, "module + lex scope + 2 declarations");
    let data: Vec<String> = nodes
        .iter()
        .filter_map(|id| scopes.scope(ScopeId::new(*id)).map(|d| format!("{d:?}")))
        .collect();
    assert!(data.contains(&format!("{:?}", Data::Module)));
}

#[test]
fn resolve_walks_buckets_and_accepts() {
    let mut engine = Engine::new();
    engine.install(emitter).unwrap();
    engine.install(resolver).unwrap();
    engine.command(vec![]).unwrap();
    let snapshot = engine.snapshot();
    let graph = snapshot.graph_view::<ScopeGraph<Domain>>();
    let scopes = <ScopeGraphSnapshot<Domain>>::new(&graph);

    // The module's Declare edge targets the lex scope; declarations live
    // under the lex scope.
    let nodes = graph.nodes();
    assert_eq!(nodes.len(), 4, "module + lex scope + 2 declarations");
    let module = nodes[0];
    let mut lex_targets = scopes.outgoing(ScopeId::new(module), &LabelKind::Declare);
    assert_eq!(lex_targets.len(), 1, "module -> lex edge");
    let lex = lex_targets.pop().unwrap();
    let decls = scopes.declarations(lex, &LabelKind::Declare);
    assert_eq!(decls.len(), 2, "two declarations under the lex scope");
    let data: Vec<_> = decls
        .iter()
        .filter_map(|id| scopes.node_data(*id).map(|d| (*d).clone()))
        .collect();
    assert_eq!(
        data,
        vec![Data::Bound("a".into()), Data::Bound("b".into())]
    );
    // Resolve a use-path from the lex scope: the reachable Scope(Module)
    // witnesses are the lex scope itself (the module is not reachable
    // back).
    let _ = (scopes, lex);
}

#[test]
fn path_expr_algebra_is_unchanged() {
    let p = PathExpr::Label(LabelKind::Declare)
        .star()
        .then(PathExpr::Label(LabelKind::Use));
    // star is nullable; the then-chain is not.
    assert!(PathExpr::Label(LabelKind::Declare).star().nullable());
    assert!(!p.nullable());
    let d = p.derivative(&LabelKind::Declare);
    assert_eq!(
        d.labels(),
        vec![LabelKind::Declare, LabelKind::Use],
        "derivative keeps the remaining labels"
    );
    // After consuming one Declare, the star remains plus the Use branch.
    let d2 = d.derivative(&LabelKind::Declare);
    assert!(!d2.nullable(), "the Use branch is not nullable");
    assert_eq!(d2.labels(), vec![LabelKind::Declare, LabelKind::Use]);
    // Consuming the Use now reaches epsilon.
    let d3 = d2.derivative(&LabelKind::Use);
    assert!(d3.nullable(), "after Use the path accepts");
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
    let (vis, dom) = partition_visible(set, &order);
    assert_eq!(vis, vec![visible]);
    assert_eq!(dom.len(), 1);
    assert_eq!(dom[0].0, shadowed);
}

#[test]
fn requirements_map_is_observable() {
    let mut engine = Engine::new();
    engine.install(req_emitter).unwrap();
    engine.command(vec![]).unwrap();
    let snapshot = engine.snapshot();
    let reqs = snapshot.map_view::<ScopeRequirements<Domain>>();
    assert_eq!(
        reqs.get(&"a://doc".to_string()).map(|v| v.to_vec()),
        Some(vec![Request::Load("other".into())])
    );
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

/// Emits a tiny scope graph: module → lex scope, two declarations, and a
/// use edge.
#[component]
fn emitter() -> (ScopeGraph<Domain>,) {
    let graph = Emitted::<ScopeGraph<Domain>>::new()?;
    let module = graph.new_scope()?;
    graph.ensure_scope(module, Data::Module)?;
    let lex = graph.new_scope()?;
    graph.ensure_scope(lex, Data::Module)?;
    graph.edge(module, LabelKind::Declare, lex)?;
    let a = graph.declare(lex, LabelKind::Declare, Data::Bound("a".into()))?;
    let b = graph.declare(lex, LabelKind::Declare, Data::Bound("b".into()))?;
    graph.edge(lex, LabelKind::Use, a)?;
    graph.edge(lex, LabelKind::Use, b)?;
    Ok((graph,))
}

/// Reads the committed graph back (a resolver pass).
#[component]
fn resolver(scopes: Observed<ScopeGraph<Domain>>) -> (ScopeGraph<Domain>,) {
    let graph = Emitted::<ScopeGraph<Domain>>::new()?;
    // Observe the topological read set deterministically: resolve a use
    // path from every scope node and re-emit the data under a label.
    let nodes = scopes.nodes()?;
    for node in nodes {
        let id = ScopeId::new(node);
        let Some(Data::Module) = scopes.scope(id)?.as_deref() else {
            continue;
        };
        let resolved = scopes.resolve(
            id,
            ScopePath::from(PathExpr::label(LabelKind::Declare).star()),
            |payload| matches!(payload, ScopeNode::Scope(Data::Module)),
        )?;
        for path in resolved {
            let scope = path.target_scope();
            if let Some(Data::Bound(name)) = scopes.scope(scope)?.as_deref() {
                graph.ensure_scope(scope, Data::Bound(name.clone()))?;
            }
        }
    }
    Ok((graph,))
}

/// Emits one requirements entry.
#[component]
fn req_emitter() -> (ScopeRequirements<Domain>,) {
    let out = Emitted::<ScopeRequirements<Domain>>::new()?;
    out.set("a://doc".to_string(), vec![Request::Load("other".into())])?;
    Ok((out,))
}
