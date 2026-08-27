//! T2 — nested runs propagate only committed child results.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::reactive::kind::Map;
use crate::reactive::prelude::*;
use crate::view;

#[view]
struct T2Source(Map<u64, i64>);

#[view]
struct T2Middle(Map<u64, i64>);

#[view]
struct T2Output(Map<u64, i64>);

static MIDDLE_RUNS: AtomicUsize = AtomicUsize::new(0);

fn middle(input: u64) -> Result<i64> {
    MIDDLE_RUNS.fetch_add(1, Ordering::SeqCst);
    let value = observe_view::<T2Source>()?
        .get(&input)?
        .map(|value| *value)
        .unwrap_or_default();
    let result = value * 2;
    emit_view::<T2Middle>()?.insert(input, result)?;
    Ok(result)
}

fn root(input: u64) -> Result<i64> {
    let value = run(middle, input)?;
    let result = value + 1;
    emit_view::<T2Output>()?.insert(input, result)?;
    Ok(result)
}

#[test]
fn child_update_reaches_parent_after_child_commit() {
    MIDDLE_RUNS.store(0, Ordering::SeqCst);
    let mut engine = Engine::new();
    let plan = engine.plan(root, 3).expect("plan");
    let running = engine.run(&plan).expect("run");
    assert_eq!(*running.output(), 1);

    engine
        .command(|| {
            emit_view::<T2Source>()?.insert(3, 4)?;
            Ok(())
        })
        .expect("source command");
    assert_eq!(*running.output(), 9);
    assert_eq!(
        engine.snapshot().observe::<T2Middle>(3).as_deref(),
        Some(&8)
    );
    assert_eq!(
        engine.snapshot().observe::<T2Output>(3).as_deref(),
        Some(&9)
    );
    assert!(MIDDLE_RUNS.load(Ordering::SeqCst) >= 2);
}

#[test]
fn nested_run_is_rejected_by_external_commands() {
    let mut engine = Engine::new();
    let error = engine.command(|| {
        let _ = run(middle, 1)?;
        Ok(())
    });
    assert!(matches!(error, Err(Error::InvalidCommandEffect { .. })));
}
