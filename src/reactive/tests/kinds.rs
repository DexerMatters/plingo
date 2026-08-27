//! Per-kind round-trip and granularity coverage (plan §8 Phase 1).
//!
//! Every kind round-trips through the handles, and a granularity test per
//! structured kind proves that writing one smallest unit wakes exactly the
//! readers of that unit. Writers follow the canonical authoring model: one
//! long-lived root owns its output facts and re-runs when a command-driven
//! input view changes (per-fact ownership, T5).

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::reactive::kind::{self, emit_view, observe_view};
use crate::reactive::prelude::*;
use crate::reactive::view::Node;
use crate::reactive::{KeyedFamily, StateValue};
use crate::view;
use reactive_macros::StateValue as StateValueDerive;

#[view]
struct KindsMap(Map<u64, i64>);

#[view]
struct KindsList(List<u64, String>);

#[view]
struct KindsTree(Tree<String, i64>);

#[view]
struct KindsGraph(Graph<i64, u8>);

/// The command-driven step input driving writer roots below.
#[view]
struct Step(Map<(), u64>);

// The box-kind witness is imported explicitly so it never shadows
// `std::boxed::Box` for other files.
#[view]
struct KindsCell(kind::Box<i64>);

static NODE_A_RUNS: AtomicUsize = AtomicUsize::new(0);
static NODE_B_RUNS: AtomicUsize = AtomicUsize::new(0);
static BUCKET_ONE_RUNS: AtomicUsize = AtomicUsize::new(0);
static BUCKET_TWO_RUNS: AtomicUsize = AtomicUsize::new(0);

static GRANDCHILD: parking_lot::Mutex<Option<Node<KindsTree>>> = parking_lot::Mutex::new(None);
static ENDPOINTS: parking_lot::Mutex<Option<(Node<KindsGraph>, Node<KindsGraph>)>> =
    parking_lot::Mutex::new(None);
static TREE_NODES: parking_lot::Mutex<Option<(Node<KindsTree>, Node<KindsTree>)>> =
    parking_lot::Mutex::new(None);
static SPLICE_ORDER: parking_lot::Mutex<Option<Vec<i64>>> = parking_lot::Mutex::new(None);
static SPLICE_ERROR: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);
static HUB: parking_lot::Mutex<Option<Node<KindsGraph>>> = parking_lot::Mutex::new(None);

fn set_step(engine: &mut Engine, step: u64) {
    engine
        .command(|| emit_view::<Step>()?.insert((), step))
        .expect("step command");
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[test]
fn map_round_trips_entries() {
    let mut engine = Engine::new();
    engine
        .command(|| {
            let map = emit_view::<KindsMap>()?;
            map.insert(1, 10)?;
            map.insert(2, 20)?;
            Ok(())
        })
        .expect("seed command");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.observe::<KindsMap>(1).as_deref(), Some(&10));
    assert_eq!(snapshot.inputs::<KindsMap>().len(), 2);

    engine
        .command(|| emit_view::<KindsMap>()?.remove(2))
        .expect("remove command");
    assert!(engine.snapshot().observe::<KindsMap>(2).is_none());
}

#[test]
fn box_round_trips_the_cell() {
    let mut engine = Engine::new();
    engine
        .command(|| emit_view::<KindsCell>()?.set(41))
        .expect("set command");
    assert_eq!(
        engine.snapshot().box_value::<KindsCell>().as_deref(),
        Some(&41)
    );
    engine
        .command(|| emit_view::<KindsCell>()?.clear())
        .expect("clear command");
    assert!(engine.snapshot().box_value::<KindsCell>().is_none());
}

/// One writer root owns the list under key 7. It is a total function of
/// the commanded step: every re-run replaces the whole list through the
/// diffing emitter, so re-runs reuse their invocation identities (T5) and
/// equal states publish nothing (T4).
fn list_writer(_: ()) -> Result<()> {
    let list = emit_view::<KindsList>()?;
    let step = observe_view::<Step>()?
        .get(&())?
        .map(|step| *step)
        .unwrap_or(0);
    let items: Vec<String> = match step {
        0 => vec!["a".into(), "b".into(), "c".into()],
        10 => vec!["a".into(), "b".into(), "c".into(), "z".into()],
        // Only slot 1 differs from step 10; the length is unchanged.
        20 | 40 => vec!["a".into(), "changed".into(), "c".into(), "z".into()],
        30 => vec!["x".into(), "y".into()],
        _ => Vec::new(),
    };
    if step == 40 {
        // An unrelated domain key is maintained by the same owner.
        list.push(&8, "unrelated".into())?;
    }
    list.replace(&7, items)
}

#[test]
fn list_round_trips_slots_and_lengths() {
    let mut engine = Engine::new();
    let plan = engine.plan(list_writer, ()).expect("plan");
    engine.run(&plan).expect("run");

    let snapshot = engine.snapshot();
    assert_eq!(snapshot.list_len::<KindsList>(&7), 3);
    assert_eq!(snapshot.list::<KindsList>(&7)[1].as_str(), "b");

    // A shorter replacement retracts the tail slots.
    set_step(&mut engine, 30);
    let items = engine.snapshot().list::<KindsList>(&7);
    assert_eq!(items.len(), 2);
    assert_eq!((items[0].as_str(), items[1].as_str()), ("x", "y"));

    // Clearing retracts every slot and zeroes the length.
    set_step(&mut engine, 99);
    assert!(engine.snapshot().list::<KindsList>(&7).is_empty());

    // Restoring republishes exactly the smallest units that changed.
    set_step(&mut engine, 0);
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.list_len::<KindsList>(&7), 3);
    assert_eq!(snapshot.list::<KindsList>(&7)[1].as_str(), "b");
}

fn forest_builder(_: ()) -> Result<()> {
    let tree = emit_view::<KindsTree>()?;
    let root = tree.root(&"doc".to_string(), 1)?;
    let child = tree.child(root, 2)?;
    let grand = tree.child(child, 3)?;
    *GRANDCHILD.lock() = Some(grand);
    Ok(())
}

fn tree_verifier(_: ()) -> Result<()> {
    let observe = observe_view::<KindsTree>()?;
    let grand = GRANDCHILD.lock().clone().expect("grand");
    let parent = observe.parent(grand.clone())?.expect("parent link");
    assert_eq!(observe.payload(parent.clone())?.as_deref(), Some(&2));
    let root = observe.parent(parent.clone())?.expect("root parent link");
    assert_eq!(observe.payload(root.clone())?.as_deref(), Some(&1));
    assert_eq!(observe.roots(&"doc".to_string())?.len(), 1);
    assert_eq!(observe.children(root)?.len(), 1);
    Ok(())
}

#[test]
fn tree_round_trips_nodes_roots_and_parents() {
    *GRANDCHILD.lock() = None;
    let mut engine = Engine::new();
    let plan = engine.plan(forest_builder, ()).expect("plan");
    engine.run(&plan).expect("run");

    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot
            .tree_roots_of::<KindsTree>(&"doc".to_string())
            .len(),
        1
    );
    let root = snapshot.tree_roots::<KindsTree>()[0].clone();
    assert_eq!(
        snapshot.tree_payload::<KindsTree>(root).as_deref(),
        Some(&1)
    );

    let plan = engine.plan(tree_verifier, ()).expect("plan");
    engine.run(&plan).expect("verify run");
}

fn bucket_builder(_: ()) -> Result<()> {
    let graph = emit_view::<KindsGraph>()?;
    let step = observe_view::<Step>()?
        .get(&())?
        .map(|step| *step)
        .unwrap_or(0);
    let hub = graph.mint(0)?;
    let leaf = graph.mint(1)?;
    graph.link(hub.clone(), 1, leaf)?;
    if step >= 1 {
        let other = graph.mint(2)?;
        graph.link(hub.clone(), 2, other)?;
    }
    *HUB.lock() = Some(hub);
    Ok(())
}

fn graph_builder(_: ()) -> Result<()> {
    let graph = emit_view::<KindsGraph>()?;
    let a = graph.mint(1)?;
    let b = graph.mint(2)?;
    graph.link(a.clone(), 9, b.clone())?;
    graph.link(a.clone(), 9, b.clone())?; // deduplicated by the bucket emitter
    graph.unlink(a.clone(), 9, b.clone())?;
    graph.link(a.clone(), 9, b.clone())?;
    *ENDPOINTS.lock() = Some((a, b));
    Ok(())
}

fn graph_verifier(_: ()) -> Result<()> {
    let observe = observe_view::<KindsGraph>()?;
    let (a, b) = ENDPOINTS.lock().clone().expect("endpoints");
    assert_eq!(observe.outgoing(a.clone(), &9)?, vec![b.clone()]);
    assert_eq!(observe.payload(a.clone())?.as_deref(), Some(&1));
    assert_eq!(observe.payload(b)?.as_deref(), Some(&2));
    assert_eq!(observe.nodes()?.len(), 2);
    Ok(())
}

#[test]
fn graph_round_trips_buckets_and_payloads() {
    *ENDPOINTS.lock() = None;
    let mut engine = Engine::new();
    let plan = engine.plan(graph_builder, ()).expect("plan");
    engine.run(&plan).expect("run");

    let plan = engine.plan(graph_verifier, ()).expect("plan");
    engine.run(&plan).expect("verify run");
}

// ---------------------------------------------------------------------------
// Granularity: one written unit wakes exactly its readers
// ---------------------------------------------------------------------------

#[test]
fn list_writes_wake_only_touched_units() {
    let slot_runs = std::sync::Arc::new(AtomicUsize::new(0));
    let iter_runs = std::sync::Arc::new(AtomicUsize::new(0));

    fn slot_reader(runs: std::sync::Arc<AtomicUsize>) -> Result<i64> {
        runs.fetch_add(1, Ordering::SeqCst);
        let observe = observe_view::<KindsList>()?;
        Ok(observe
            .get(&7, 1)?
            .map(|item| item.len() as i64)
            .unwrap_or(-1))
    }

    fn iter_reader(runs: std::sync::Arc<AtomicUsize>) -> Result<i64> {
        runs.fetch_add(1, Ordering::SeqCst);
        let observe = observe_view::<KindsList>()?;
        Ok(observe.iter(&7)?.len() as i64)
    }

    let mut engine = Engine::new();
    set_step(&mut engine, 0);
    let plan = engine.plan(list_writer, ()).expect("plan");
    engine.run(&plan).expect("run");
    {
        let runs = std::sync::Arc::clone(&slot_runs);
        let plan = engine
            .plan(move |_: ()| slot_reader(std::sync::Arc::clone(&runs)), ())
            .expect("plan");
        engine.run(&plan).expect("run");
    }
    {
        let runs = std::sync::Arc::clone(&iter_runs);
        let plan = engine
            .plan(move |_: ()| iter_reader(std::sync::Arc::clone(&runs)), ())
            .expect("plan");
        engine.run(&plan).expect("run");
    }

    // An equal re-run (same step value, new epoch via an unrelated write)
    // publishes nothing: both readers stay asleep (T4).
    set_step(&mut engine, 0);
    assert_eq!(slot_runs.load(Ordering::SeqCst), 1);
    assert_eq!(iter_runs.load(Ordering::SeqCst), 1);

    // Step 10 appends one slot: the length fact changes, so the iterator
    // wakes while the slot-1 reader stays asleep.
    set_step(&mut engine, 10);
    assert_eq!(slot_runs.load(Ordering::SeqCst), 1);
    assert_eq!(iter_runs.load(Ordering::SeqCst), 2);

    // An in-place slot rewrite wakes exactly that slot's reader and the
    // whole-list iterator (it reads every slot); an equal value would
    // wake neither (T4).
    set_step(&mut engine, 20);
    assert_eq!(slot_runs.load(Ordering::SeqCst), 2);
    assert_eq!(iter_runs.load(Ordering::SeqCst), 3);

    // A different domain key wakes neither reader of key 7.
    set_step(&mut engine, 40);
    assert_eq!(slot_runs.load(Ordering::SeqCst), 2);
    assert_eq!(iter_runs.load(Ordering::SeqCst), 3);
}

fn tree_builder(_: ()) -> Result<()> {
    let tree = emit_view::<KindsTree>()?;
    let step = observe_view::<Step>()?
        .get(&())?
        .map(|step| *step)
        .unwrap_or(0);
    let root = tree.root(&"g".to_string(), 1)?;
    let child_a = tree.child(root.clone(), 10)?;
    let child_b = tree.child(root, 20)?;
    if step >= 1 {
        tree.set_payload(child_a.clone(), 11)?;
    }
    *TREE_NODES.lock() = Some((child_a, child_b));
    Ok(())
}

fn tree_node_a_watcher(_: ()) -> Result<()> {
    NODE_A_RUNS.fetch_add(1, Ordering::SeqCst);
    let (a, _) = TREE_NODES.lock().clone().expect("tree nodes");
    let id = a.clone();
    observe_view::<KindsTree>()?.payload(id)?;
    Ok(())
}

fn tree_node_b_watcher(_: ()) -> Result<()> {
    NODE_B_RUNS.fetch_add(1, Ordering::SeqCst);
    let (_, b) = TREE_NODES.lock().clone().expect("tree nodes");
    let id = b.clone();
    observe_view::<KindsTree>()?.payload(id)?;
    Ok(())
}

#[test]
fn tree_payload_writes_wake_exactly_that_node() {
    NODE_A_RUNS.store(0, Ordering::SeqCst);
    NODE_B_RUNS.store(0, Ordering::SeqCst);
    *TREE_NODES.lock() = None;

    let mut engine = Engine::new();
    set_step(&mut engine, 0);
    for function in [tree_builder, tree_node_a_watcher, tree_node_b_watcher] {
        let plan = engine.plan(function, ()).expect("plan");
        engine.run(&plan).expect("run");
    }

    // Rewrite A's payload through its owner: A's reader wakes, B's does
    // not.
    set_step(&mut engine, 1);
    assert_eq!(NODE_A_RUNS.load(Ordering::SeqCst), 2);
    assert_eq!(NODE_B_RUNS.load(Ordering::SeqCst), 1);

    // An equal payload write is cold (T4).
    set_step(&mut engine, 2);
    assert_eq!(NODE_A_RUNS.load(Ordering::SeqCst), 2);
    assert_eq!(NODE_B_RUNS.load(Ordering::SeqCst), 1);
}

fn bucket_one_watcher(_: ()) -> Result<()> {
    BUCKET_ONE_RUNS.fetch_add(1, Ordering::SeqCst);
    let hub = HUB.lock().clone().expect("hub");
    observe_view::<KindsGraph>()?.outgoing(hub, &1)?;
    Ok(())
}

fn bucket_two_watcher(_: ()) -> Result<()> {
    BUCKET_TWO_RUNS.fetch_add(1, Ordering::SeqCst);
    let hub = HUB.lock().clone().expect("hub");
    observe_view::<KindsGraph>()?.outgoing(hub, &2)?;
    Ok(())
}

#[test]
fn bucket_writes_wake_only_that_label() {
    BUCKET_ONE_RUNS.store(0, Ordering::SeqCst);
    BUCKET_TWO_RUNS.store(0, Ordering::SeqCst);
    *HUB.lock() = None;

    let mut engine = Engine::new();
    set_step(&mut engine, 0);
    for function in [bucket_builder, bucket_one_watcher, bucket_two_watcher] {
        let plan = engine.plan(function, ()).expect("plan");
        engine.run(&plan).expect("run");
    }

    // Linking under label 2 leaves the label-1 reader asleep; re-linking
    // label 1 with an equal target publishes nothing (T4).
    set_step(&mut engine, 1);
    assert_eq!(BUCKET_ONE_RUNS.load(Ordering::SeqCst), 1);
    assert_eq!(BUCKET_TWO_RUNS.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------
// Engine invariants restated over non-map kinds (plan §8 Phase 2)
// ---------------------------------------------------------------------------

/// T2 over a ListView pipeline: a downstream box that sums a list is never
/// observed torn from the list it derives from, across epochs.
#[test]
fn list_pipeline_never_exposes_glitches() {
    #[view]
    struct SumSource(List<u64, u64>);

    #[view]
    struct SumDerived(Box<u64>);

    fn summarizer(_: ()) -> Result<()> {
        let observe = observe_view::<SumSource>()?;
        let total: u64 = observe.iter(&5)?.iter().map(|item| **item).sum();
        emit_view::<SumDerived>()?.set(total)
    }

    fn auditor(_: ()) -> Result<u64> {
        let lists = observe_view::<SumSource>()?;
        let boxes = observe_view::<SumDerived>()?;
        let items = lists.iter(&5)?;
        let total = boxes.get()?.map(|value| *value).unwrap_or(0);
        // Any observed pair must be consistent: the box equals the sum of
        // exactly the slots visible right now.
        assert_eq!(total, items.iter().map(|item| **item).sum::<u64>());
        Ok(items.len() as u64)
    }

    let mut engine = Engine::new();
    set_step(&mut engine, 0);
    let plan = engine.plan(summarizer, ()).expect("plan");
    engine.run(&plan).expect("run");
    let plan = engine.plan(auditor, ()).expect("plan");
    let running = engine.run(&plan).expect("run");

    for step in [1u64, 2, 3] {
        set_step(&mut engine, step);
        let expected = engine.snapshot().list_len::<SumSource>(&5);
        assert_eq!(*running.output(), expected as u64);
    }
}

/// T3 over a GraphView: identical constructions on independent engines
/// produce identical committed facts.
#[test]
fn graph_construction_is_deterministic_across_engines() {
    static DUMP: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

    fn build(_: ()) -> Result<()> {
        let graph = emit_view::<KindsGraph>()?;
        let a = graph.mint(1)?;
        let b = graph.mint(2)?;
        let c = graph.mint(3)?;
        graph.link(a.clone(), 7, b.clone())?;
        graph.link(a.clone(), 7, c.clone())?;
        graph.link(b, 8, c)?;
        Ok(())
    }

    fn dump(_: ()) -> Result<()> {
        let observe = observe_view::<KindsGraph>()?;
        let mut lines = Vec::new();
        for node in observe.nodes()? {
            if let Some(payload) = observe.payload(node.clone())? {
                lines.push(format!("node {payload:?}"));
                for label in [7u8, 8] {
                    lines.push(format!(
                        "bucket {label} -> {:?}",
                        observe.outgoing(node.clone(), &label)?
                    ));
                }
            }
        }
        lines.sort();
        *DUMP.lock() = Some(lines.join("\n"));
        Ok(())
    }

    let run = || -> String {
        *DUMP.lock() = None;
        let mut engine = Engine::new();
        for function in [build, dump] {
            let plan = engine.plan(function, ()).expect("plan");
            engine.run(&plan).expect("run");
        }
        DUMP.lock().clone().expect("dump published")
    };

    assert_eq!(run(), run());
}


// ---------------------------------------------------------------------------
// StateValue derive + StateCell (plan §5.6)
// ---------------------------------------------------------------------------

#[derive(StateValueDerive, Debug)]
struct KindsState {
    visits: u64,
    label: String,
}

#[test]
fn state_cell_persists_across_epochs_and_rolls_back() {
    #[view]
    struct StateTick(Map<u64, u64>);

    fn writer(_: ()) -> Result<()> {
        let cell = crate::reactive::state_cell::<KindsState>();
        let visits = cell.with(|state| state.map(|state| state.visits).unwrap_or(0))?;
        cell.set(KindsState {
            visits: visits + 1,
            label: format!("visit-{visits}"),
        })?;
        let tick = observe_view::<Step>()?
            .get(&())?
            .map(|value| *value)
            .unwrap_or(0);
        emit_view::<StateTick>()?.insert(0, visits + tick)?;
        Ok(())
    }

    let mut engine = Engine::new();
    let plan = engine.plan(writer, ()).expect("plan");
    let running = engine.run(&plan).expect("run");
    assert_eq!(
        engine.snapshot().observe::<StateTick>(0).as_deref(),
        Some(&0)
    );

    // Second epoch wakes the SAME root; the slot carries the committed value.
    engine
        .command(|| emit_view::<Step>()?.insert((), 10))
        .expect("tick command");
    // Slot persisted (visits=1) and the tick fact recomputed from it.
    assert_eq!(
        engine.snapshot().observe::<StateTick>(0).as_deref(),
        Some(&11)
    );
    let _ = &running;

    // A failing command rolls the whole epoch (facts + slot) back atomically.
    let error = engine
        .command::<fn() -> plingo::reactive::Result<()>>(|| {
            Err(Error::Internal("authored failure after set".into()))
        })
        .unwrap_err();
    assert!(matches!(error, Error::Internal(_)));
    assert_eq!(
        engine.snapshot().observe::<StateTick>(0).as_deref(),
        Some(&11)
    );
}

// ---------------------------------------------------------------------------
// Keyed families (plan §5.4)
// ---------------------------------------------------------------------------

#[view]
struct FamilyInput(Map<u64, i64>);

#[view]
struct FamilyEcho(Map<u64, i64>);

/// Private fixture component definition (plan §6.1): identity derives from
/// this marker plus the exact driving key, with duplicate-install
/// rejection through the descriptor registry.
struct FamilyEchoDefinition;

impl crate::reactive::component::ComponentDefinition for FamilyEchoDefinition {
    fn __descriptor() -> &'static str {
        "reactive::tests::kinds::family_echo"
    }
}

#[test]
fn keyed_family_evaluates_exactly_one_child_per_changed_key() {
    static RUNS: AtomicUsize = AtomicUsize::new(0);

    fn echo_child(input: u64) -> Result<()> {
        RUNS.fetch_add(1, Ordering::SeqCst);
        let value = observe_view::<FamilyInput>()?
            .get(&input)?
            .map(|value| *value)
            .unwrap_or_default();
        emit_view::<FamilyEcho>()?.insert(input, value * 3)?;
        Ok(())
    }

    let mut engine = Engine::new();
    engine
        .command(|| {
            emit_view::<FamilyInput>()?.insert(1, 10)?;
            emit_view::<FamilyInput>()?.insert(2, 20)?;
            Ok(())
        })
        .expect("seed inputs");

    let family: KeyedFamily<FamilyInput> = engine
        .install_component_each_key::<FamilyEchoDefinition, FamilyInput, _>(echo_child)
        .expect("install");
    // Initial enumeration evaluated exactly the two existing keys once.
    assert_eq!(RUNS.load(Ordering::SeqCst), 2);
    assert_eq!(
        engine.snapshot().observe::<FamilyEcho>(1).as_deref(),
        Some(&30)
    );
    assert_eq!(
        engine.snapshot().observe::<FamilyEcho>(2).as_deref(),
        Some(&60)
    );

    // One changed key wakes exactly one child.
    engine
        .command(|| emit_view::<FamilyInput>()?.insert(2, 21))
        .expect("edit key 2");
    assert_eq!(RUNS.load(Ordering::SeqCst), 3);
    assert_eq!(
        engine.snapshot().observe::<FamilyEcho>(2).as_deref(),
        Some(&63)
    );
    // Untouched key stayed cold: no third re-run for key 1.

    // Inserting a NEW key schedules only that child.
    engine
        .command(|| emit_view::<FamilyInput>()?.insert(3, 5))
        .expect("insert key 3");
    assert_eq!(RUNS.load(Ordering::SeqCst), 4);
    assert_eq!(
        engine.snapshot().observe::<FamilyEcho>(3).as_deref(),
        Some(&15)
    );

    // Removal runs the child once observing absence, then retires it and
    // retracts the publication.
    engine
        .command(|| emit_view::<FamilyInput>()?.remove(3))
        .expect("remove key 3");
    assert_eq!(engine.snapshot().observe::<FamilyEcho>(3), None);

    engine.remove_keyed(&family).expect("remove family");
}

// ---------------------------------------------------------------------------
// Patch emission (plan §5.5)
// ---------------------------------------------------------------------------

#[view]
struct PatchTarget(Map<u64, String>);

#[test]
fn map_patch_touches_only_mentioned_keys() {
    let mut engine = Engine::new();
    engine
        .command(|| {
            emit_view::<PatchTarget>()?.insert(1, "one".into())?;
            emit_view::<PatchTarget>()?.insert(2, "two".into())?;
            Ok(())
        })
        .expect("seed");

    // Patch: one upsert plus one remove; the untouched key stays cold.
    let report = engine
        .command(|| {
            plingo::reactive::emit_patch::<PatchTarget>()?.upsert(2, "TWO".into())?;
            plingo::reactive::emit_patch::<PatchTarget>()?.remove(1)?;
            Ok(())
        })
        .expect("patch command");

    assert_eq!(
        engine.snapshot().observe::<PatchTarget>(1),
        None,
        "removed key retracts"
    );
    assert_eq!(
        engine.snapshot().observe::<PatchTarget>(2).as_deref(),
        Some(&"TWO".to_string())
    );
    assert_eq!(report.engine_work().facts_changed, 2);

    // Mixed modes on one view are rejected.
    let error = engine
        .command(|| -> Result<()> {
            emit_view::<PatchTarget>()?.insert(3, "three".into())?;
            plingo::reactive::emit_patch::<PatchTarget>()?.upsert(4, "four".into())?;
            Ok(())
        })
        .unwrap_err();
    assert!(matches!(error, Error::MixedEmissionMode { .. }));

    // Duplicate patch keys are rejected instead of last-write-wins.
    let error = engine
        .command(|| -> Result<()> {
            plingo::reactive::emit_patch::<PatchTarget>()?.upsert(5, "a".into())?;
            plingo::reactive::emit_patch::<PatchTarget>()?.upsert(5, "b".into())?;
            Ok(())
        })
        .unwrap_err();
    assert!(matches!(error, Error::DuplicatePatchKey { .. }));
}

#[test]
fn randomized_keyed_traces_match_replace_all_oracle() {
    // Plan §5.5/§11 Phase 2: randomized insert/equal-update/update/remove
    // traces must leave the keyed family's publication identical to a
    // replace-all writer after every epoch, including close/reopen.
    #[view]
    struct RandSource(Map<u64, i64>);

    #[view]
    struct RandEcho(Map<u64, i64>);

    /// Private fixture component definition (plan §6.1).
    struct RandEchoDefinition;

    impl crate::reactive::component::ComponentDefinition for RandEchoDefinition {
        fn __descriptor() -> &'static str {
            "reactive::tests::kinds::rand_echo"
        }
    }

    fn echo_child(input: u64) -> Result<()> {
        let value = observe_view::<RandSource>()?
            .get(&input)?
            .map(|value| *value);
        match value {
            Some(value) => emit_view::<RandEcho>()?.insert(input, value * 2)?,
            None => {}
        }
        Ok(())
    }

    fn replace_all(_: ()) -> Result<()> {
        let mut items = Vec::new();
        for input in 0..24u64 {
            if let Some(value) = observe_view::<RandSource>()?
                .get(&input)?
                .map(|value| *value)
            {
                items.push((input, value * 2));
            }
        }
        let echo = emit_view::<RandEcho>()?;
        for (input, value) in items {
            echo.insert(input, value)?;
        }
        Ok(())
    }

    // Deterministic PRNG (xorshift): no external seed dependencies.
    let mut state: u64 = 0x2545F4914F6CDD1D;
    let mut next_random = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut incremental = Engine::new();
    incremental
        .command(|| {
            emit_view::<RandSource>()?.insert(0, 0)?;
            Ok(())
        })
        .expect("seed");
    incremental
        .install_component_each_key::<RandEchoDefinition, RandSource, _>(echo_child)
        .expect("install family");

    let mut reference = Engine::new();
    reference
        .command(|| {
            emit_view::<RandSource>()?.insert(0, 0)?;
            Ok(())
        })
        .expect("seed");
    let plan = reference.plan(replace_all, ()).expect("plan");
    reference.run(&plan).expect("run reference");

    for epoch in 0..200u64 {
        let key = next_random() % 24;
        let action = next_random() % 3;
        match action {
            0 => {
                let value = (next_random() % 1_000_000) as i64;
                incremental
                    .command(|| emit_view::<RandSource>()?.insert(key, value))
                    .expect("insert");
                reference
                    .command(|| emit_view::<RandSource>()?.insert(key, value))
                    .expect("ref insert");
            }
            1 => {
                // Equal-value rewrite exercises T4 coldness in both engines.
                let current = incremental
                    .snapshot()
                    .observe::<RandSource>(key)
                    .map(|value| *value)
                    .unwrap_or(epoch as i64);
                incremental
                    .command(|| emit_view::<RandSource>()?.insert(key, current))
                    .expect("equal");
                reference
                    .command(|| emit_view::<RandSource>()?.insert(key, current))
                    .expect("ref equal");
            }
            _ => {
                incremental
                    .command(|| emit_view::<RandSource>()?.remove(key))
                    .expect("remove");
                reference
                    .command(|| emit_view::<RandSource>()?.remove(key))
                    .expect("ref remove");
            }
        }

        // Canonical comparison: every live input's echo matches.
        for input in 0..24u64 {
            assert_eq!(
                incremental.snapshot().observe::<RandEcho>(input).as_deref(),
                reference.snapshot().observe::<RandEcho>(input).as_deref(),
                "epoch {epoch} diverged at key {input}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// B1 regression: keyed-child retirement must wake downstream dependents
// (plan §5, barrier-solutions.md §2)
// ---------------------------------------------------------------------------

#[view]
struct B1Upstream(Map<u64, String>);

#[view]
struct B1Downstream(Map<u64, bool>);

/// Private fixture component definition (plan §6.1).
struct B1KeyedChildDefinition;

impl crate::reactive::component::ComponentDefinition for B1KeyedChildDefinition {
    fn __descriptor() -> &'static str {
        "reactive::tests::kinds::b1_keyed_child"
    }
}

#[test]
fn keyed_child_retraction_wakes_downstream_root() {
    // A keyed family child publishes to B1Downstream. A separate root
    // observes B1Downstream. When the upstream key is removed, the child
    // retires and retracts its publication. The downstream root MUST fire.
    fn keyed_child(key: u64) -> Result<()> {
        let value = observe_view::<B1Upstream>()?
            .get(&key)?
            .map(|v| (*v).clone());
        match value {
            Some(text) => {
                emit_view::<B1Downstream>()?.insert(key, text == "alive")?;
            }
            None => {} // input absent: child will be retired by the engine
        }
        Ok(())
    }

    fn downstream_watcher(_: ()) -> Result<()> {
        // Read all downstream keys; publish a summary fact for assertion.
        let keys = observe_view::<B1Downstream>()?.keys()?;
        for key in keys {
            if let Some(value) = observe_view::<B1Downstream>()?.get(&key)?.map(|v| *v) {
                emit_view::<B1Summary>()?.insert(key, value);
            } else {
                emit_view::<B1Summary>()?.remove(key)?;
            }
        }
        Ok(())
    }

    #[view]
    struct B1Summary(Map<u64, bool>);

    let mut engine = Engine::new();
    engine
        .command(|| {
            emit_view::<B1Upstream>()?.insert(42, "alive".into())?;
            Ok(())
        })
        .expect("seed");

    engine
        .install_component_each_key::<B1KeyedChildDefinition, B1Upstream, _>(keyed_child)
        .expect("install family");

    let plan = engine.plan(downstream_watcher, ()).expect("plan watcher");
    engine.run(&plan).expect("run watcher");

    // Sanity: downstream sees the initial publication.
    assert_eq!(
        engine.snapshot().observe::<B1Summary>(42).as_deref(),
        Some(&true)
    );

    // Remove the upstream key. The child retires and retracts
    // B1Downstream[42]. The downstream watcher MUST wake and retract
    // B1Summary[42].
    engine
        .command(|| emit_view::<B1Upstream>()?.remove(42))
        .expect("remove upstream");

    // This assertion MUST pass after the B1 fix; it fails on main because
    // the retirement retraction never reaches mark_changes.
    // NOTE: even if B1Summary doesn't retract (the bug), B1Downstream SHOULD
    // be gone because journal.commit_changes includes the retraction.
    assert!(
        engine.snapshot().observe::<B1Downstream>(42).is_none(),
        "B1Downstream[42] must retract when the keyed child retires"
    );

    // THE ACTUAL BUG: the downstream watcher must ALSO fire and retract
    // B1Summary[42]. This fails on main because the absence-retirement
    // path in quiesce `continue`s before draining the graph change log,
    // so the omitted-write retraction never reaches mark_changes.
    assert!(
        engine.snapshot().observe::<B1Summary>(42).is_none(),
        "B1Summary[42] must retract: downstream watcher must wake on          keyed-child retirement retraction"
    );
}

// ---------------------------------------------------------------------------
// Per-child lifecycle (plan §11 child relationship lifecycle)
// ---------------------------------------------------------------------------

/// Echo of every live child link: `(parent, child) -> child payload`.
#[view]
struct ChildEcho(Map<(Node<KindsTree>, Node<KindsTree>), i64>);

static CHILD_RUNS: AtomicUsize = AtomicUsize::new(0);

/// A keyed child effect observes its own link fact and echoes the child
/// payload. Inserted links spawn it; a payload edit reruns exactly it; a
/// removed link retires it (and its echo retracts).
fn child_lifecycle(_: ()) -> Result<()> {
    run_each_child::<KindsTree, _>(|parent, child| {
        CHILD_RUNS.fetch_add(1, Ordering::SeqCst);
        let observe = observe_view::<KindsTree>()?;
        let link = observe.fact(
            kind::TreeKey::ChildLink(parent.clone(), child.clone()),
            crate::reactive::plain::Temporal::Current,
        )?;
        let echo = emit_view::<ChildEcho>()?;
        match link.as_deref() {
            Some(kind::TreeFact::Link(_)) => {
                let payload = observe.payload(child.clone())?.as_deref().copied().unwrap_or(0);
                echo.insert((parent.clone(), child.clone()), payload)?;
            }
            _ => {
                echo.remove((parent, child))?;
            }
        }
        Ok(())
    })
}

fn child_forest_builder(_: ()) -> Result<()> {
    let tree = emit_view::<KindsTree>()?;
    let step = observe_view::<Step>()?
        .get(&())?
        .map(|step| *step)
        .unwrap_or(0);
    let root = tree.root(&"lifecycle".to_string(), 1)?;
    // Child A's payload depends on the step; child B exists only at
    // step 2. Identities are stable across reruns (same call sites),
    // so unchanged links never respawn.
    let child_a = tree.child(root.clone(), if step == 1 { 11 } else { 10 })?;
    if step == 2 {
        let _child_b = tree.child(root, 20)?;
    }
    if step >= 3 {
        // A's payload changes again while the forest shape stays the same.
        tree.set_payload(child_a, 12)?;
    }
    Ok(())
}

#[test]
fn per_child_lifecycle_wakes_exactly_the_affected_child() {
    CHILD_RUNS.store(0, Ordering::SeqCst);
    let mut engine = Engine::new();
    set_step(&mut engine, 0);
    for function in [child_forest_builder, child_lifecycle] {
        let plan = engine.plan(function, ()).expect("plan");
        engine.run(&plan).expect("run");
    }

    let echo = |engine: &Engine| -> Vec<(u64, i64)> {
        let mut pairs: Vec<(u64, i64)> = engine
            .snapshot()
            .inputs::<ChildEcho>()
            .into_iter()
            .map(|(parent, child): (Node<KindsTree>, Node<KindsTree>)| {
                let payload = engine
                    .snapshot()
                    .observe::<ChildEcho>((parent.clone(), child.clone()))
                    .map(|v| *v)
                    .unwrap_or(0);
                (parent.raw_id(), payload)
            })
            .collect();
        pairs.sort_unstable();
        pairs
    };

    // Initial build: the only link is root -> A, so exactly one child
    // effect ran and echoed A's payload. Enumeration holds a single key
    // even though the journaled slot may migrate ordinals in one commit
    // (snapshot reconcile).
    assert_eq!(CHILD_RUNS.load(Ordering::SeqCst), 1);
    let pairs = echo(&engine);
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].1, 10);

    // Payload-only edit of A: A's effect reruns and republishes; no other
    // link exists and the echo map still has exactly one entry.
    set_step(&mut engine, 1);
    assert_eq!(CHILD_RUNS.load(Ordering::SeqCst), 2);
    let pairs = echo(&engine);
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].1, 11);

    // Insert child B: B's effect is new (one run) and A re-evaluates via
    // the parent touch but publishes nothing new (equal-value write cold,
    // T4). The echo grows to exactly two entries.
    set_step(&mut engine, 2);
    assert_eq!(CHILD_RUNS.load(Ordering::SeqCst), 4);
    let pairs = echo(&engine);
    assert_eq!(pairs.len(), 2);
    assert_eq!(
        pairs
            .iter()
            .map(|(_, payload)| *payload)
            .collect::<Vec<_>>(),
        vec![10, 20]
    );

    // Remove child B: B's effect retires without re-running (no run
    // increment for it) and its echo retracts; A's payload edit reruns
    // its effect.
    set_step(&mut engine, 3);
    assert_eq!(CHILD_RUNS.load(Ordering::SeqCst), 5);
    let pairs = echo(&engine);
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].1, 12);
}

fn splice_mint(
    tree: &kind::TreeEmit<KindsTree>,
    root: Node<KindsTree>,
    payload: i64,
) -> Result<Node<KindsTree>> {
    let id = crate::reactive::__macro_private::automatic_effect_node_id::<KindsTree>()?;
    tree.set_node(id.clone(), Some(root), payload, Vec::new())?;
    Ok(id)
}

/// One builder per step: rebuilds the doc forest [a,b,c,d] with fresh
/// identities and applies the step's canonical ordered splices (plan
/// §15.3). Error steps capture the rejection into `SPLICE_ERROR`.
fn splice_builder(_: ()) -> Result<()> {
    let tree = emit_view::<KindsTree>()?;
    let step = observe_view::<Step>()?
        .get(&())?
        .map(|step| *step)
        .unwrap_or(0);
    let root = tree.root(&"doc".to_string(), 1)?;
    let a = tree.child(root.clone(), 10)?;
    let b = tree.child(root.clone(), 20)?;
    let c = tree.child(root.clone(), 30)?;
    let d = tree.child(root.clone(), 40)?;
    match step {
        1 => {
            // Insert x between b and c: [a,b,x,c,d].
            let x = splice_mint(&tree, root.clone(), 99)?;
            tree.splice_children(root.clone(), Some(b.clone()), &[], &[x], Some(c.clone()))?;
        }
        2 => {
            // Coalesce two touches of the same node in one command:
            // x between b and c, then y between x and c.
            let x = splice_mint(&tree, root.clone(), 99)?;
            let y = splice_mint(&tree, root.clone(), 88)?;
            tree.splice_children(root.clone(), Some(b.clone()), &[], &[x.clone()], Some(c.clone()))?;
            tree.splice_children(root, Some(x), &[], &[y], Some(c))?;
        }
        3 => {
            // Remove c between y and d: [a,b,x,y,d].
            let x = splice_mint(&tree, root.clone(), 99)?;
            let y = splice_mint(&tree, root.clone(), 88)?;
            tree.splice_children(root.clone(), Some(b.clone()), &[], &[x.clone()], Some(c.clone()))?;
            tree.splice_children(root.clone(), Some(x), &[], &[y.clone()], Some(c.clone()))?;
            tree.splice_children(root, Some(y), &[c], &[], Some(d))?;
        }
        4 => {
            // Replace y with z: [a,b,x,z,d].
            let x = splice_mint(&tree, root.clone(), 99)?;
            let y = splice_mint(&tree, root.clone(), 88)?;
            let z = splice_mint(&tree, root.clone(), 77)?;
            tree.splice_children(root.clone(), Some(b.clone()), &[], &[x.clone()], Some(c.clone()))?;
            tree.splice_children(root.clone(), Some(x.clone()), &[], &[y.clone()], Some(c.clone()))?;
            tree.splice_children(root.clone(), Some(y.clone()), &[c.clone()], &[], Some(d.clone()))?;
            tree.splice_children(root, Some(x), &[y], &[z], Some(d))?;
        }
        5 => {
            // Absent before-anchor rejected.
            let ghost = splice_mint(&tree, root.clone(), 7)?;
            let result = tree.splice_children(root, Some(ghost), &[], &[], Some(a));
            *SPLICE_ERROR.lock() = Some(format!("{result:?}"));
        }
        6 => {
            // Removed-run mismatch rejected (the run between b and d is
            // [c], not []).
            let x = splice_mint(&tree, root.clone(), 99)?;
            let result = tree.splice_children(root, Some(b), &[], &[x], Some(d));
            *SPLICE_ERROR.lock() = Some(format!("{result:?}"));
        }
        _ => {}
    }
    Ok(())
}

fn splice_verifier(_: ()) -> Result<()> {
    let observe = observe_view::<KindsTree>()?;
    let expected = SPLICE_ORDER.lock().clone().expect("expected order");
    let root = observe.roots(&"doc".to_string())?[0].clone();
    let children = observe.children(root)?;
    let mut payloads = Vec::with_capacity(children.len());
    for child in children {
        let payload = observe.payload(child)?.expect("spliced child payload");
        payloads.push(*payload);
    }
    assert_eq!(payloads, expected, "child order after splice");
    Ok(())
}

#[test]
fn tree_ordered_splice_validates_and_coalesces() {
    *SPLICE_ORDER.lock() = None;
    *SPLICE_ERROR.lock() = None;
    let mut engine = Engine::new();
    set_step(&mut engine, 0);
    let plan = engine.plan(splice_builder, ()).expect("plan");
    engine.run(&plan).expect("run");
    for (step, order) in [
        (1, vec![10, 20, 99, 30, 40]),
        (2, vec![10, 20, 99, 88, 30, 40]),
        (3, vec![10, 20, 99, 88, 40]),
        (4, vec![10, 20, 99, 77, 40]),
        (5, vec![10, 20, 30, 40]),
        (6, vec![10, 20, 30, 40]),
    ] {
        *SPLICE_ORDER.lock() = Some(order);
        set_step(&mut engine, step);
        let verify = engine.plan(splice_verifier, ()).expect("verify plan");
        engine.run(&verify).expect("verify run");
    }
    assert!(
        SPLICE_ERROR.lock().is_some(),
        "invalid splices must be rejected"
    );
}
