//! T1 — incremental propagation equals a from-scratch installation.

use std::sync::Arc;

use crate::reactive::kind::Map;
use crate::reactive::prelude::*;
use crate::view;

#[view]
struct T1Source(Map<u64, i64>);

#[view]
struct T1Derived(Map<u64, i64>);

fn derive(input: u64) -> Result<i64> {
    let value = observe_view::<T1Source>()?.get(&input)?
        .map(|value| *value)
        .unwrap_or_default();
    let result = value + 1;
    emit_view::<T1Derived>()?.insert(input, result)?;
    Ok(result)
}

fn install_with_source(value: i64) -> (i64, Option<Arc<i64>>)
where
    i64: 'static,
{
    let mut engine = Engine::new();
    let plan = engine.plan(derive, 0).expect("plan");
    let running = engine.run(&plan).expect("run");
    engine
        .command(|| {
            emit_view::<T1Source>()?.insert(0, value)?;
            Ok(())
        })
        .expect("source command");
    (*running.output(), engine.snapshot().observe::<T1Derived>(0))
}

#[test]
fn incremental_and_from_scratch_state_match() {
    let (incremental, incremental_fact) = install_with_source(4);
    let (from_scratch, from_scratch_fact) = install_with_source(4);
    assert_eq!(incremental, 5);
    assert_eq!(incremental, from_scratch);
    assert_eq!(incremental_fact.as_deref(), Some(&5));
    assert_eq!(incremental_fact, from_scratch_fact);
}

#[test]
fn equal_external_writes_are_zero_work() {
    let mut engine = Engine::new();
    let plan = engine.plan(derive, 0).expect("plan");
    let _running = engine.run(&plan).expect("run");
    engine
        .command(|| {
            emit_view::<T1Source>()?.insert(0, 2)?;
            Ok(())
        })
        .expect("first command");
    let report = engine
        .command(|| {
            emit_view::<T1Source>()?.insert(0, 2)?;
            Ok(())
        })
        .expect("equal command");
    assert_eq!(report.rounds, 0);
    assert_eq!(report.changed::<T1Source>(), 0);
}
