//! Phase 1 acceptance — the §4 authoring sugar.
//!
//! `#[component]` with bare observed args, bare return tuples, `Previous`
//! args, and sink components; `#[view]` for all five shapes; duplicate-view
//! rejection is a compile error (trybuild-style fixtures live in
//! `tests/compile/`).

use crate::reactive::prelude::*;
use crate::reactive::tests::{Current, Diff, Half, Output, Source, Sum};
use crate::reactive_component as component;

// ---------------------------------------------------------------------------
// #[view] — every shape, including a generic view
// ---------------------------------------------------------------------------

#[crate::reactive_view(box, value = i32)]
pub struct Config;

#[crate::reactive_view(map, key = u32, value = String)]
pub struct StringMap;

#[crate::reactive_view(tree, value = u64)]
pub struct NumTree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode(pub u64);
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdge(pub u64);
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GraphLabel(pub String);

#[crate::reactive_view(graph, value = GraphNode, edge = GraphEdge, label = GraphLabel)]
pub struct GraphView;

#[crate::reactive_view(map, key = String, value = u32)]
pub struct GenericMap {
    pub _marker: std::marker::PhantomData<u32>,
}

// ---------------------------------------------------------------------------
// Sugar components
// ---------------------------------------------------------------------------

/// Bare observed arg, bare tuple return — the §4.1 check shape.
#[component]
pub fn sugar_doubler(source: Source) -> (Output,) {
    let out = Emitted::<Output>::new()?;
    let value = source.get()?;
    out.set(value.map(|v| *v).unwrap_or(0) * 2)?;
    Ok((out,))
}

/// Previous arg + bare return.
#[component]
pub fn sugar_delta(current: Current, prev: Previous<Current>) -> (Diff,) {
    let out = Emitted::<Diff>::new()?;
    let current = current.get()?.map(|v| *v).unwrap_or(0);
    let previous = prev.get()?.map(|v| *v).unwrap_or(0);
    out.set(current - previous)?;
    Ok((out,))
}

/// A sink component: observes and emits nothing.
#[component]
pub fn sugar_sink(source: Source) -> () {
    let value = source.get()?;
    let _ = value;
    Ok(())
}

/// Multi-emit return.
#[component]
pub fn sugar_multi(source: Source) -> (Sum, Half) {
    let sum = Emitted::<Sum>::new()?;
    let half = Emitted::<Half>::new()?;
    let value = source.get()?.map(|v| *v).unwrap_or(0);
    sum.set(value * 3)?;
    half.set(value / 2)?;
    Ok((sum, half))
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

#[test]
fn sugar_components_run_identically_to_explicit_handles() {
    let mut engine = Engine::new();
    engine.external::<Source>().unwrap();
    engine.install(sugar_doubler).unwrap();
    engine.install(sugar_multi).unwrap();

    let report = engine
        .command(vec![ExternalOp::box_set::<Source>(21)])
        .unwrap();
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.box_view::<Output>().get().map(|v| *v), Some(42));
    assert_eq!(snapshot.box_view::<Sum>().get().map(|v| *v), Some(63));
    assert_eq!(snapshot.box_view::<Half>().get().map(|v| *v), Some(10));
    // No epoch work on an equal re-command.
    let report2 = engine
        .command(vec![ExternalOp::box_set::<Source>(21)])
        .unwrap();
    assert_eq!(report2.runs, 0);
    let _ = report;
}

#[test]
fn sugar_sink_observes_without_emitting() {
    let mut engine = Engine::new();
    engine.external::<Source>().unwrap();
    engine.install(sugar_sink).unwrap();
    engine
        .command(vec![ExternalOp::box_set::<Source>(1)])
        .unwrap();
    let snapshot = engine.snapshot();
    // The sink wrote nothing; the external source is the only fact.
    assert_eq!(snapshot.box_view::<Source>().get().map(|v| *v), Some(1));
}

#[test]
fn sugar_previous_reads_committed_state() {
    let mut engine = Engine::new();
    engine.external::<Current>().unwrap();
    engine.install(sugar_delta).unwrap();
    engine
        .command(vec![ExternalOp::box_set::<Current>(10)])
        .unwrap();
    engine
        .command(vec![ExternalOp::box_set::<Current>(13)])
        .unwrap();
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.box_view::<Diff>().get().map(|v| *v), Some(3));
}