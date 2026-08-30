//! T4 — exact fact reads avoid unrelated reruns.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::reactive::api::{run, run_each_child, run_each_child_of, run_each_key};
use crate::reactive::kind::{Map, emit_view, observe_view};
use crate::reactive::prelude::*;
use crate::view;

#[view]
struct T4Source(Map<u64, i64>);

#[view]
struct T4Derived(Map<u64, i64>);

static RUNS_ONE: AtomicUsize = AtomicUsize::new(0);
static RUNS_TWO: AtomicUsize = AtomicUsize::new(0);

fn derive_one(input: u64) -> Result<i64> {
    RUNS_ONE.fetch_add(1, Ordering::SeqCst);
    let value = observe_view::<T4Source>()?
        .get(&input)?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<T4Derived>()?.insert(input, value + 10)?;
    Ok(value + 10)
}

fn derive_two(input: u64) -> Result<i64> {
    RUNS_TWO.fetch_add(1, Ordering::SeqCst);
    let value = observe_view::<T4Source>()?
        .get(&input)?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<T4Derived>()?.insert(input, value + 20)?;
    Ok(value + 20)
}

#[test]
fn changing_one_fact_does_not_schedule_another_key() {
    RUNS_ONE.store(0, Ordering::SeqCst);
    RUNS_TWO.store(0, Ordering::SeqCst);
    let mut engine = Engine::new();
    let first_plan = engine.plan(derive_one, 1).expect("first plan");
    let first = engine.run(&first_plan).expect("first run");
    let second_plan = engine.plan(derive_two, 2).expect("second plan");
    let second = engine.run(&second_plan).expect("second run");

    engine
        .command(|| {
            emit_view::<T4Source>()?.insert(1, 7)?;
            Ok(())
        })
        .expect("key-one command");
    assert_eq!(*first.output(), 17);
    assert_eq!(*second.output(), 20);
    assert_eq!(RUNS_TWO.load(Ordering::SeqCst), 1);
}

#[test]
fn omitted_emissions_retract_the_owned_fact() {
    let mut engine = Engine::new();
    let plan = engine.plan(derive_one, 4).expect("plan");
    let running = engine.run(&plan).expect("run");
    engine
        .command(|| {
            emit_view::<T4Source>()?.insert(4, -10)?;
            Ok(())
        })
        .expect("source command");
    assert_eq!(
        engine.snapshot().observe::<T4Derived>(4).as_deref(),
        Some(&0)
    );
    assert_eq!(*running.output(), 0);
}

#[view]
struct AbbaSource(Map<u64, i64>);

#[view]
struct AbbaEcho(Map<u64, i64>);

#[test]
fn abba_round_trips_commit_zero_changes() {
    // Plan §5.1: A -> B -> A is cold to subscribers/snapshots even if
    // transient rounds were required. The journal compares first and final
    // values once per touched key.
    fn echo(_: ()) -> Result<()> {
        let value = observe_view::<AbbaSource>()?
            .get(&1)?
            .map(|value| *value)
            .unwrap_or(0);
        emit_view::<AbbaEcho>()?.insert(1, value)?;
        Ok(())
    }

    let mut engine = Engine::new();
    let plan = engine.plan(echo, ()).expect("plan");
    engine.run(&plan).expect("run");

    // Establish baseline A: source=3, echo=3.
    engine
        .command(|| {
            emit_view::<AbbaSource>()?.insert(1, 3)?;
            Ok(())
        })
        .expect("seed A");

    static ABBA_NOTIFIED: AtomicUsize = AtomicUsize::new(0);
    engine
        .subscribe::<AbbaEcho>(move |_snapshot, count| {
            ABBA_NOTIFIED.fetch_add(count, Ordering::SeqCst);
        })
        .expect("subscribe");

    // Both writes land in ONE command: two transient rounds, one journal,
    // identical first and final values.
    engine
        .command(|| {
            emit_view::<AbbaSource>()?.insert(1, 7)?;
            emit_view::<AbbaSource>()?.insert(1, 3)?;
            Ok(())
        })
        .expect("A -> B -> A in one epoch");

    // The echo fact returned to its original value: no committed change.
    assert_eq!(
        ABBA_NOTIFIED.load(Ordering::SeqCst),
        0,
        "A -> B -> A must not notify subscribers"
    );
    assert_eq!(
        engine.snapshot().observe::<AbbaEcho>(1).as_deref(),
        Some(&3)
    );
}
