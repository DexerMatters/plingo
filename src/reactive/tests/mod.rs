//! Shared fixtures for the theorem tests: view types, test components,
//! and the determinism harness (T1–T6, verification matrix §8).

#![allow(dead_code)]

mod matrix;
mod sugar;
mod t1_consistency;
mod t2_glitch;
mod t3_determinism;
mod t4_min_delta;
mod t5_ownership;
mod t6_cycles_rollback;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::reactive::prelude::*;
use crate::reactive_component as component;

// ---------------------------------------------------------------------------
// View types
// ---------------------------------------------------------------------------

macro_rules! box_view {
    ($name:ident, $value:ty) => {
        pub struct $name;
        impl ViewSpec for $name {
            type Shape = BoxShape;
            type Key = ();
            type Value = $value;
            type Edge = ();
            type Label = ();
        }
    };
}

macro_rules! map_view {
    ($name:ident, $key:ty, $value:ty) => {
        pub struct $name;
        impl ViewSpec for $name {
            type Shape = MapShape;
            type Key = $key;
            type Value = $value;
            type Edge = ();
            type Label = ();
        }
    };
}

macro_rules! tree_view {
    ($name:ident, $value:ty) => {
        pub struct $name;
        impl ViewSpec for $name {
            type Shape = TreeShape;
            type Key = ();
            type Value = $value;
            type Edge = ();
            type Label = ();
        }
    };
}

macro_rules! graph_view {
    ($name:ident, $value:ty, $edge:ty, $label:ty) => {
        pub struct $name;
        impl ViewSpec for $name {
            type Shape = GraphShape;
            type Key = $label;
            type Value = $value;
            type Edge = $edge;
            type Label = $label;
        }
    };
}

box_view!(Source, i64);
box_view!(Output, i64);
box_view!(Mod, i64);
box_view!(HalfMod, i64);
box_view!(Half, i64);
box_view!(Sum, i64);
box_view!(Switch, bool);
box_view!(BranchA, i64);
box_view!(BranchB, i64);
box_view!(BranchOut, i64);
box_view!(Tick, bool);
box_view!(Current, i64);
box_view!(Diff, i64);

map_view!(Table, u32, i64);
map_view!(ResultMap, u32, i64);
map_view!(Shared, u32, String);
map_view!(Cells, u64, i64);
map_view!(Deps, u64, u64);
map_view!(Log, u32, String);

tree_view!(SourceTree, i64);
tree_view!(ResultTree, i64);
tree_view!(MintTree, i64);

graph_view!(GraphIn, i64, i64, String);
graph_view!(GraphOut, i64, i64, String);

// ---------------------------------------------------------------------------
// Run counters (one static per component; tests reset before scenarios)
// ---------------------------------------------------------------------------

static COUNTER_LOCK: parking_lot::ReentrantMutex<()> = parking_lot::ReentrantMutex::new(());

pub static TRIPLE_RUNS: AtomicUsize = AtomicUsize::new(0);
pub static MODDER_RUNS: AtomicUsize = AtomicUsize::new(0);
pub static HALF_MOD_RUNS: AtomicUsize = AtomicUsize::new(0);
pub static HALVER_RUNS: AtomicUsize = AtomicUsize::new(0);
pub static DISCOVERY_RUNS: AtomicUsize = AtomicUsize::new(0);
pub static CHILD_RUNS: AtomicUsize = AtomicUsize::new(0);
pub static BRANCH_RUNS: AtomicUsize = AtomicUsize::new(0);
pub static CHAIN_RUNS: AtomicUsize = AtomicUsize::new(0);
pub static MIRROR_RUNS: AtomicUsize = AtomicUsize::new(0);
pub static GRAPH_RUNS: AtomicUsize = AtomicUsize::new(0);

/// Serializes scenarios that assert on the shared run counters: tests run
/// in parallel, and the statics are global.
pub fn with_counters<T>(f: impl FnOnce() -> T) -> T {
    let _guard = COUNTER_LOCK.lock();
    reset_counters();
    f()
}

/// A counter-lock guard for tests that drive engines directly.
pub fn counter_guard() -> parking_lot::ReentrantMutexGuard<'static, ()> {
    COUNTER_LOCK.lock()
}

pub fn reset_counters() {
    for counter in [
        &TRIPLE_RUNS,
        &MODDER_RUNS,
        &HALF_MOD_RUNS,
        &HALVER_RUNS,
        &DISCOVERY_RUNS,
        &CHILD_RUNS,
        &BRANCH_RUNS,
        &CHAIN_RUNS,
        &MIRROR_RUNS,
        &GRAPH_RUNS,
    ] {
        counter.store(0, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

/// Source → Output = value * 3.
#[component]
pub fn counted_triple(source: Observed<Source>) -> Result<(Emitted<Output>,)> {
    let out = Emitted::<Output>::new()?;
    TRIPLE_RUNS.fetch_add(1, Ordering::SeqCst);
    let value = source.get()?;
    out.set(value.map(|v| *v).unwrap_or(0) * 3)?;
    Ok((out,))
}

/// Source → Mod = value % 2 (non-injective, for zero-delta tests).
#[component]
pub fn modder(source: Observed<Source>) -> Result<(Emitted<Mod>,)> {
    let out = Emitted::<Mod>::new()?;
    MODDER_RUNS.fetch_add(1, Ordering::SeqCst);
    let value = source.get()?;
    out.set(value.map(|v| *v).unwrap_or(0) % 2)?;
    Ok((out,))
}

/// Mod → HalfMod = value / 2 (downstream of modder).
#[component]
pub fn half_modder(modded: Observed<Mod>) -> Result<(Emitted<HalfMod>,)> {
    let out = Emitted::<HalfMod>::new()?;
    HALF_MOD_RUNS.fetch_add(1, Ordering::SeqCst);
    let value = modded.get()?;
    out.set(value.map(|v| *v).unwrap_or(0) / 2)?;
    Ok((out,))
}

/// Output → Half = value / 2 (downstream of counted_triple).
#[component]
pub fn halver(output: Observed<Output>) -> Result<(Emitted<Half>,)> {
    let out = Emitted::<Half>::new()?;
    HALVER_RUNS.fetch_add(1, Ordering::SeqCst);
    let value = output.get()?;
    out.set(value.map(|v| *v).unwrap_or(0) / 2)?;
    Ok((out,))
}

/// Table → ResultMap = value + 1. The root counts key discovery; each
/// entry child counts separately.
#[component]
pub fn counted_plus_one(table: Observed<Table>) -> Result<(Emitted<ResultMap>,)> {
    let out = Emitted::<ResultMap>::new()?;
    DISCOVERY_RUNS.fetch_add(1, Ordering::SeqCst);
    let keys = table.keys()?;
    for key in keys {
        let out = out.clone();
        table.visit(key, move |key, value| -> Result<()> {
            CHILD_RUNS.fetch_add(1, Ordering::SeqCst);
            out.set(key, value.map(|v| *v).unwrap_or(0) + 1)?;
            Ok(())
        })?;
    }
    Ok((out,))
}

/// Table → Sum = the sum of all entries read in one coherent snapshot.
#[component]
pub fn summer(table: Observed<Table>) -> Result<(Emitted<Sum>,)> {
    let out = Emitted::<Sum>::new()?;
    let mut total = 0i64;
    for key in table.keys()? {
        total += table.get(&key)?.map(|v| *v).unwrap_or(0);
    }
    out.set(total)?;
    Ok((out,))
}

/// Dynamic branching: reads A when the switch is on, B when it is off.
#[component]
pub fn counted_branch(
    switch: Observed<Switch>,
    a: Observed<BranchA>,
    b: Observed<BranchB>,
) -> Result<(Emitted<BranchOut>,)> {
    let out = Emitted::<BranchOut>::new()?;
    BRANCH_RUNS.fetch_add(1, Ordering::SeqCst);
    if switch.get()?.map(|v| *v) == Some(true) {
        out.set(a.get()?.map(|v| *v).unwrap_or(0))?;
    } else {
        out.set(b.get()?.map(|v| *v).unwrap_or(0))?;
    }
    Ok((out,))
}

/// Forward references: cells(k) = cells(deps(k)) + 1, published
/// provisionally (check-mode semantics) so mutual references manifest as
/// fact cycles instead of silent stalls.
#[component]
pub fn counted_chain(deps: Observed<Deps>, cells: Observed<Cells>) -> Result<(Emitted<Cells>,)> {
    let cells_emit = Emitted::<Cells>::new()?;
    let emit = cells_emit.clone();
    deps.visit_each(move |key, dep| -> Result<()> {
        CHAIN_RUNS.fetch_add(1, Ordering::SeqCst);
        let prev = cells.get(&dep.map(|v| *v).unwrap_or(0))?;
        emit.set(key, prev.map(|v| *v).unwrap_or(0) + 1)?;
        Ok(())
    })?;
    Ok((cells_emit,))
}

/// Multi-producer: writes the odd keys of the shared map.
#[component]
pub fn producer_a(tick: Observed<Tick>) -> Result<(Emitted<Shared>,)> {
    let out = Emitted::<Shared>::new()?;
    tick.get()?;
    out.set(1, "a1".to_string())?;
    out.set(3, "a3".to_string())?;
    Ok((out,))
}

/// Multi-producer: writes the even key of the shared map.
#[component]
pub fn producer_b(tick: Observed<Tick>) -> Result<(Emitted<Shared>,)> {
    let out = Emitted::<Shared>::new()?;
    tick.get()?;
    out.set(2, "b2".to_string())?;
    Ok((out,))
}

/// Multi-producer violation: writes a key owned by producer_a.
#[component]
pub fn producer_overlap(tick: Observed<Tick>) -> Result<(Emitted<Shared>,)> {
    let out = Emitted::<Shared>::new()?;
    tick.get()?;
    out.set(1, "overlap".to_string())?;
    Ok((out,))
}

/// Previous feedback: Diff = Current(t) - Current(t-1).
#[component]
pub fn delta(current: Observed<Current>, prev: Previous<Current>) -> Result<(Emitted<Diff>,)> {
    let out = Emitted::<Diff>::new()?;
    let cur = current.get()?;
    let before = prev.get()?;
    out.set(match (before, cur) {
        (Some(p), Some(c)) => *c - *p,
        (None, Some(c)) => *c,
        _ => 0,
    })?;
    Ok((out,))
}

/// Temporal-only reader: logs the committed value from the previous epoch.
#[component]
pub fn report(prev: Previous<Current>) -> Result<(Emitted<Log>,)> {
    let log = Emitted::<Log>::new()?;
    let before = prev.get()?;
    log.set(0, format!("{before:?}"))?;
    Ok((log,))
}

/// Mints one fresh node id per run; the id must be stable across runs.
#[component]
pub fn minter(trigger: Observed<Tick>) -> Result<(Emitted<MintTree>,)> {
    let tree = Emitted::<MintTree>::new()?;
    trigger.get()?;
    let id = tree.fresh_node_id()?;
    tree.insert_node(id, 7)?;
    Ok((tree,))
}

/// Mirrors a source tree into a result tree (nested visitors).
#[component]
pub fn counted_mirror(source: Observed<SourceTree>) -> Result<(Emitted<ResultTree>,)> {
    let out = Emitted::<ResultTree>::new()?;
    let source_outer = source.clone();
    let out_outer = out.clone();
    source_outer.visit_roots_each(move |root| -> Result<()> {
        MIRROR_RUNS.fetch_add(1, Ordering::SeqCst);
        let kids = source.children(root)?;
        if let Some(value) = source.node(root)? {
            out_outer.insert_node(root, *value)?;
        }
        let receiver = source.clone();
        let source_child = source.clone();
        let out_child = out_outer.clone();
        receiver.visit_children_each(root, move |child| -> Result<()> {
            MIRROR_RUNS.fetch_add(1, Ordering::SeqCst);
            if let Some(value) = source_child.node(child)? {
                out_child.insert_node(child, *value)?;
            }
            out_child.move_node(child, root)?;
            Ok(())
        })?;
        out_outer.reorder_children(root, kids)?;
        Ok(())
    })?;
    Ok((out,))
}

/// Copies a graph's nodes and its "l" edges.
#[component]
pub fn graph_copy(source: Observed<GraphIn>) -> Result<(Emitted<GraphOut>,)> {
    let out = Emitted::<GraphOut>::new()?;
    GRAPH_RUNS.fetch_add(1, Ordering::SeqCst);
    let source_nodes = source.clone();
    let out_nodes = out.clone();
    source_nodes.visit_nodes_each(move |id, data| -> Result<()> {
        if let Some(data) = data {
            out_nodes.insert_node(id, *data)?;
        }
        Ok(())
    })?;
    let source_edges = source.clone();
    let out_edges = out.clone();
    source_edges.visit_outgoing_each(NodeId(0), "l".to_string(), move |edge, data| -> Result<()> {
        if let Some(data) = data {
            out_edges.insert_edge(edge.source, edge.label, edge.target, *data)?;
        }
        Ok(())
    })?;
    Ok((out,))
}

/// Errors when tick is true; used for authored-error rollback tests.
#[component]
pub fn failer(tick: Observed<Tick>) -> Result<(Emitted<Shared>,)> {
    if tick.get()?.map(|v| *v) == Some(true) {
        return Err(Error::authored(std::io::Error::other("boom")));
    }
    Ok((Emitted::<Shared>::new()?,))
}

/// Panics; used for panic-rollback tests.
#[component]
pub fn panicker(tick: Observed<Tick>) -> Result<(Emitted<Shared>,)> {
    tick.get()?;
    panic!("kaboom");
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The full fixture engine: every component and external view. The
/// `include_overlap` flag adds the ownership-violating producer.
pub fn build_engine(workers: usize, include_overlap: bool) -> Result<Engine> {
    let mut engine = Engine::with_workers(workers);
    for external in [
        Engine::external::<Source>,
        Engine::external::<Switch>,
        Engine::external::<BranchA>,
        Engine::external::<BranchB>,
        Engine::external::<Table>,
        Engine::external::<Tick>,
        Engine::external::<Deps>,
        Engine::external::<Cells>,
        Engine::external::<SourceTree>,
        Engine::external::<GraphIn>,
        Engine::external::<Current>,
    ] {
        external(&mut engine)?;
    }
    engine.install(counted_triple)?;
    engine.install(modder)?;
    engine.install(half_modder)?;
    engine.install(halver)?;
    engine.install(counted_plus_one)?;
    engine.install(summer)?;
    engine.install(counted_branch)?;
    engine.install(counted_chain)?;
    engine.install(producer_a)?;
    engine.install(producer_b)?;
    if include_overlap {
        engine.install(producer_overlap)?;
    }
    engine.install(delta)?;
    engine.install(report)?;
    engine.install(minter)?;
    engine.install(counted_mirror)?;
    engine.install(graph_copy)?;
    Ok(engine)
}

pub fn subscribe_named<V: ViewSpec>(
    engine: &mut Engine,
    name: &'static str,
    log: &Arc<Mutex<Vec<String>>>,
) -> Result<()> {
    let log = Arc::clone(log);
    engine.subscribe::<V>(Box::new(move |changes| {
        let mut log = log.lock();
        for change in changes {
            log.push(format!("{name}: {}", change.describe()));
        }
    }))
}

/// The committed-state dump used by the determinism harness.
pub fn dump(engine: &Engine) -> String {
    let snap = engine.snapshot();
    fn map<K, F>(keys: Vec<K>, view: &F) -> String
    where
        K: std::fmt::Display + std::fmt::Debug + Copy,
        F: Fn(K) -> String,
    {
        keys.iter()
            .map(|key| format!("{key}->{}", view(*key)))
            .collect::<Vec<_>>()
            .join(",")
    }
    let table_keys = snap.map_view::<Table>().keys();
    let result_keys = snap.map_view::<ResultMap>().keys();
    let shared_keys = snap.map_view::<Shared>().keys();
    let cells_keys = snap.map_view::<Cells>().keys();
    let deps_keys = snap.map_view::<Deps>().keys();
    let log_keys = snap.map_view::<Log>().keys();
    let tree = |view: &crate::reactive::engine::SnapshotTree<SourceTree>| -> String {
        let mut out = String::new();
        for root in view.roots() {
            out.push_str(&format!(
                "root({root:?}={:?},kids={:?})",
                view.node(root),
                view.children(root)
            ));
        }
        out
    };
    let result_tree = |view: &crate::reactive::engine::SnapshotTree<ResultTree>| -> String {
        let mut out = String::new();
        for root in view.roots() {
            out.push_str(&format!(
                "root({root:?}={:?},kids={:?})",
                view.node(root),
                view.children(root)
            ));
        }
        out
    };
    let mint_tree = |view: &crate::reactive::engine::SnapshotTree<MintTree>| -> String {
        let mut out = String::new();
        for root in view.roots() {
            out.push_str(&format!("root({root:?}={:?})", view.node(root)));
        }
        out
    };
    let graph = |view: &crate::reactive::engine::SnapshotGraph<GraphIn>| -> String {
        let mut out = String::new();
        for node in view.nodes() {
            out.push_str(&format!("n{node:?}={:?},", view.node(node)));
        }
        for edge in view.outgoing(NodeId(0), &"l".to_string()) {
            out.push_str(&format!(
                "e{:?}->{:?}={:?},",
                edge.source,
                edge.target,
                view.edge(edge.source, &edge.label, edge.target)
            ));
        }
        out
    };
    let graph_out = |view: &crate::reactive::engine::SnapshotGraph<GraphOut>| -> String {
        let mut out = String::new();
        for node in view.nodes() {
            out.push_str(&format!("n{node:?}={:?},", view.node(node)));
        }
        for edge in view.outgoing(NodeId(0), &"l".to_string()) {
            out.push_str(&format!(
                "e{:?}->{:?}={:?},",
                edge.source,
                edge.target,
                view.edge(edge.source, &edge.label, edge.target)
            ));
        }
        out
    };
    format!(
        "source={:?} output={:?} mod={:?} half_mod={:?} half={:?} sum={:?} switch={:?} branch_a={:?} \
         branch_b={:?} branch_out={:?} table=[{}] result=[{}] shared=[{}] cells=[{}] deps=[{}] \
         log=[{}] tick={:?} current={:?} diff={:?} source_tree={} result_tree={} mint_tree={} \
         graph_in={} graph_out={}",
        snap.box_view::<Source>().get(),
        snap.box_view::<Output>().get(),
        snap.box_view::<Mod>().get(),
        snap.box_view::<HalfMod>().get(),
        snap.box_view::<Half>().get(),
        snap.box_view::<Sum>().get(),
        snap.box_view::<Switch>().get(),
        snap.box_view::<BranchA>().get(),
        snap.box_view::<BranchB>().get(),
        snap.box_view::<BranchOut>().get(),
        map(table_keys, &|k| format!("{:?}", snap.map_view::<Table>().get(&k))),
        map(result_keys, &|k| {
            format!("{:?}", snap.map_view::<ResultMap>().get(&k))
        }),
        map(shared_keys, &|k| {
            format!("{:?}", snap.map_view::<Shared>().get(&k))
        }),
        map(cells_keys, &|k| {
            format!("{:?}", snap.map_view::<Cells>().get(&k))
        }),
        map(deps_keys, &|k| format!("{:?}", snap.map_view::<Deps>().get(&k))),
        map(log_keys, &|k| format!("{:?}", snap.map_view::<Log>().get(&k))),
        snap.box_view::<Tick>().get(),
        snap.box_view::<Current>().get(),
        snap.box_view::<Diff>().get(),
        tree(&snap.tree_view::<SourceTree>()),
        result_tree(&snap.tree_view::<ResultTree>()),
        mint_tree(&snap.tree_view::<MintTree>()),
        graph(&snap.graph_view::<GraphIn>()),
        graph_out(&snap.graph_view::<GraphOut>()),
    )
}

/// The observable outcome of a scenario: committed dump, changed-fact
/// sequences, subscription sequences, errors, and logical counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub dump: String,
    /// Per-command changed-fact sequences (deterministic order).
    pub changes: Vec<Vec<String>>,
    pub subs: Vec<String>,
    pub errors: Vec<String>,
    pub epochs: Vec<u64>,
    pub rounds: Vec<u32>,
    pub runs: Vec<u64>,
}

/// Runs one scenario on a fresh engine: applies each command in order
/// (tolerating expected failures) and records every observable.
pub fn run_scenario(workers: usize, commands: &[Vec<ExternalOp>]) -> Outcome {
    run_scenario_engine(workers, commands, false)
}

pub fn run_scenario_engine(
    workers: usize,
    commands: &[Vec<ExternalOp>],
    include_overlap: bool,
) -> Outcome {
    let _guard = COUNTER_LOCK.lock();
    let mut engine = build_engine(workers, include_overlap).expect("fixture engine");
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    subscribe_named::<Output>(&mut engine, "output", &log).unwrap();
    subscribe_named::<ResultMap>(&mut engine, "result", &log).unwrap();
    subscribe_named::<Shared>(&mut engine, "shared", &log).unwrap();
    subscribe_named::<Log>(&mut engine, "log", &log).unwrap();
    subscribe_named::<Sum>(&mut engine, "sum", &log).unwrap();
    subscribe_named::<Diff>(&mut engine, "diff", &log).unwrap();
    subscribe_named::<Half>(&mut engine, "half", &log).unwrap();
    subscribe_named::<ResultTree>(&mut engine, "result_tree", &log).unwrap();
    subscribe_named::<GraphOut>(&mut engine, "graph_out", &log).unwrap();
    subscribe_named::<MintTree>(&mut engine, "mint", &log).unwrap();

    let mut changes: Vec<Vec<String>> = Vec::new();
    let mut errors = Vec::new();
    let mut epochs = Vec::new();
    let mut rounds = Vec::new();
    let mut runs = Vec::new();
    for ops in commands {
        match engine.command(ops.to_vec()) {
            Ok(cmd) => {
                changes.push(cmd.changed().iter().map(|c| c.describe()).collect());
                epochs.push(cmd.epoch);
                rounds.push(cmd.rounds);
                runs.push(cmd.runs);
            }
            Err(error) => {
                changes.push(Vec::new());
                errors.push(error.to_string());
            }
        }
    }
    Outcome {
        dump: dump(&engine),
        changes,
        subs: log.lock().clone(),
        errors,
        epochs,
        rounds,
        runs,
    }
}
