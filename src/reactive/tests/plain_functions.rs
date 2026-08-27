use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::reactive::kind::Map;
use crate::reactive::prelude::*;
use crate::view;

#[view]
struct PlainSource(Map<u64, i64>);

#[view]
struct PlainDerived(Map<u64, i64>);

#[view]
struct Deps(Map<u64, u64>);

#[view]
struct Cells(Map<u64, i64>);

#[view]
struct PlainOther(Map<u64, i64>);

#[view]
struct DomainResult(Map<(), i64>);

#[view]
struct CycleA(Map<(), i64>);

#[view]
struct CycleB(Map<(), i64>);

static DOUBLE_RUNS: AtomicUsize = AtomicUsize::new(0);
static LEAF_RUNS: AtomicUsize = AtomicUsize::new(0);

fn double(cell: u64) -> Result<i64> {
    DOUBLE_RUNS.fetch_add(1, Ordering::SeqCst);
    let value = observe_view::<PlainSource>()?
        .get(&cell)?
        .map(|value| *value)
        .unwrap_or_default();
    let result = value * 2;
    emit_view::<PlainDerived>()?.insert(cell, result)?;
    Ok(result)
}

fn derive(cell: u64) -> Result<i64> {
    LEAF_RUNS.fetch_add(1, Ordering::SeqCst);
    let dependency = observe_view::<Deps>()?.get(&cell)?;
    let value = match dependency {
        Some(next) => run(derive, *next)? + 1,
        None => observe_view::<Cells>()?
            .get(&cell)?
            .map(|value| *value)
            .unwrap_or_default(),
    };
    emit_view::<PlainDerived>()?.insert(cell, value)?;
    Ok(value)
}

fn cycle(cell: u64) -> Result<i64> {
    run(cycle, cell)
}

fn previous_source(cell: u64) -> Result<i64> {
    let value = observe_view::<PlainSource>()?
        .get_previous(&cell)?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<PlainDerived>()?.insert(cell, value)?;
    Ok(value)
}

fn domain_count(_: ()) -> Result<i64> {
    let count = observe_view::<PlainSource>()?.keys()?.len() as i64;
    emit_view::<DomainResult>()?.insert((), count)?;
    Ok(count)
}

fn optional_output(cell: u64) -> Result<i64> {
    let value = observe_view::<PlainSource>()?.get(&cell)?;
    if let Some(value) = value {
        emit_view::<PlainDerived>()?.insert(cell, *value)?;
        Ok(*value)
    } else {
        Ok(0)
    }
}

fn fail_after_emit(cell: u64) -> Result<i64> {
    let value = observe_view::<PlainSource>()?
        .get(&cell)?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<PlainDerived>()?.insert(cell, value)?;
    if value == 9 {
        return Err(Error::authored(std::io::Error::other("rerun failure")));
    }
    Ok(value)
}

fn cycle_a(_: ()) -> Result<i64> {
    let value = observe_view::<CycleB>()?
        .get(&())?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<CycleA>()?.insert((), value + 1)?;
    Ok(value + 1)
}

fn cycle_b(_: ()) -> Result<i64> {
    let value = observe_view::<CycleA>()?
        .get(&())?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<CycleB>()?.insert((), value + 1)?;
    Ok(value + 1)
}

fn seed_source(_: ()) -> Result<()> {
    emit_view::<PlainSource>()?.insert(7, 3)?;
    Ok(())
}
#[test]
fn plain_plan_isolated_then_reacts_and_removes() {
    DOUBLE_RUNS.store(0, Ordering::SeqCst);
    let mut engine = Engine::new();
    let planned = engine.plan(double, 7).expect("plan");
    assert_eq!(*planned.output(), 0);
    assert!(engine.snapshot().observe::<PlainDerived>(7).is_none());
    assert!(engine.snapshot().inputs::<PlainSource>().is_empty());

    let running = engine.run(&planned).expect("run");
    assert_eq!(*running.output(), 0);
    assert!(engine.run(&planned).is_err());
    engine
        .command(|| {
            emit_view::<PlainSource>()?.insert(7, 3)?;
            Ok(())
        })
        .expect("source command");
    assert_eq!(*running.output(), 6);
    assert_eq!(
        engine.snapshot().observe::<PlainDerived>(7).as_deref(),
        Some(&6)
    );

    engine
        .command(|| {
            emit_view::<PlainSource>()?.remove(7)?;
            Ok(())
        })
        .expect("retract command");
    assert_eq!(*running.output(), 0);
    assert!(DOUBLE_RUNS.load(Ordering::SeqCst) >= 3);
    engine.remove(&running).expect("remove");
    engine.remove(&running).expect("idempotent remove");
    assert!(engine.snapshot().observe::<PlainDerived>(7).is_none());
}

#[test]
fn stale_plan_recaptures_and_wrong_engine_is_rejected() {
    let mut engine = Engine::new();
    let planned = engine.plan(double, 4).expect("plan");
    engine
        .command(|| {
            emit_view::<PlainSource>()?.insert(4, 9)?;
            Ok(())
        })
        .expect("source command");
    assert_eq!(*planned.output(), 0);
    let mut other = Engine::new();
    assert!(matches!(
        other.run(&planned),
        Err(Error::PlanForDifferentEngine)
    ));
    let running = engine.run(&planned).expect("recaptured run");
    assert_eq!(*running.output(), 18);
}

#[test]
fn failed_stale_plan_run_restores_preview_and_capture() {
    let mut engine = Engine::new();
    let planned = engine.plan(double, 7).expect("plan");
    engine
        .command(|| {
            emit_view::<PlainDerived>()?.insert(7, 99)?;
            Ok(())
        })
        .expect("conflicting external write");

    assert!(matches!(
        engine.run(&planned),
        Err(Error::ConflictingWrites { .. })
    ));
    assert_eq!(*planned.output(), 0);
    assert!(engine.snapshot().observe::<PlainSource>(7).is_none());

    engine
        .command(|| {
            emit_view::<PlainDerived>()?.remove(7)?;
            Ok(())
        })
        .expect("retract conflicting write");
    let running = engine.run(&planned).expect("retry");
    assert_eq!(*running.output(), 0);
}

#[test]
fn recursive_children_reuse_and_cycles_fail_without_deadlock() {
    LEAF_RUNS.store(0, Ordering::SeqCst);
    let mut engine = Engine::new();
    engine
        .command(|| {
            emit_view::<Cells>()?.insert(1, 4)?;
            emit_view::<Deps>()?.insert(2, 1)?;
            Ok(())
        })
        .expect("seed command");
    let planned = engine.plan(derive, 2).expect("recursive plan");
    let running = engine.run(&planned).expect("recursive run");

    assert_eq!(*running.output(), 5);
    let before = LEAF_RUNS.load(Ordering::SeqCst);
    engine
        .command(|| {
            emit_view::<Cells>()?.insert(1, 8)?;
            Ok(())
        })
        .expect("leaf command");
    assert_eq!(*running.output(), 9);
    assert!(LEAF_RUNS.load(Ordering::SeqCst) > before);

    let mut cycle_engine = Engine::new();
    assert!(matches!(
        cycle_engine.plan(cycle, 0),
        Err(Error::ComputationCycle { .. })
    ));
}
#[test]
fn temporal_reads_wait_for_the_next_epoch_and_domains_are_precise() {
    let mut engine = Engine::new();
    let planned = engine.plan(previous_source, 3).expect("plan");
    let running = engine.run(&planned).expect("run");
    engine
        .command(|| {
            emit_view::<PlainSource>()?.insert(3, 7)?;
            Ok(())
        })
        .expect("source command");
    assert_eq!(*running.output(), 0);
    engine
        .command(|| {
            emit_view::<PlainOther>()?.insert(0, 1)?;
            Ok(())
        })
        .expect("next epoch");
    assert_eq!(*running.output(), 7);

    let domain_plan = engine.plan(domain_count, ()).expect("domain plan");
    let domain_running = engine.run(&domain_plan).expect("domain run");
    engine
        .command(|| {
            emit_view::<PlainSource>()?.insert(1, 1)?;
            emit_view::<PlainSource>()?.insert(2, 2)?;
            Ok(())
        })
        .expect("domain command");
    assert_eq!(*domain_running.output(), 3);
    assert_eq!(engine.snapshot().inputs::<PlainSource>(), vec![3, 1, 2]);
}

#[test]
fn omitted_writes_retract_and_failed_reruns_keep_committed_state() {
    let mut engine = Engine::new();
    let optional = engine.plan(optional_output, 5).expect("optional plan");
    let optional_running = engine.run(&optional).expect("optional run");
    engine
        .command(|| {
            emit_view::<PlainSource>()?.insert(5, 4)?;
            Ok(())
        })
        .expect("insert");
    assert_eq!(
        engine.snapshot().observe::<PlainDerived>(5).as_deref(),
        Some(&4)
    );
    engine
        .command(|| {
            emit_view::<PlainSource>()?.remove(5)?;
            Ok(())
        })
        .expect("omission");
    assert!(engine.snapshot().observe::<PlainDerived>(5).is_none());
    assert_eq!(*optional_running.output(), 0);

    let failing = engine.plan(fail_after_emit, 6).expect("failure plan");
    let failing_running = engine.run(&failing).expect("failure run");
    engine
        .command(|| {
            emit_view::<PlainSource>()?.insert(6, 1)?;
            Ok(())
        })
        .expect("successful rerun");
    assert_eq!(
        engine.snapshot().observe::<PlainDerived>(6).as_deref(),
        Some(&1)
    );
    let error = engine.command(|| {
        emit_view::<PlainSource>()?.insert(6, 9)?;
        Ok(())
    });
    assert!(matches!(error, Err(Error::Authored(_))));
    assert_eq!(
        engine.snapshot().observe::<PlainDerived>(6).as_deref(),
        Some(&1)
    );
    assert_eq!(*failing_running.output(), 1);
}

#[test]
fn view_dependency_cycles_are_rejected_transactionally() {
    let mut engine = Engine::new();
    let first = engine.plan(cycle_a, ()).expect("first plan");
    let second = engine.plan(cycle_b, ()).expect("second plan");
    engine.run(&first).expect("first root");
    assert!(matches!(
        engine.run(&second),
        Err(Error::DependencyCycle { .. })
    ));
}

#[test]
fn planned_root_writes_recompute_existing_roots() {
    let mut engine = Engine::new();
    let derived = engine.plan(double, 7).expect("derived plan");
    let derived = engine.run(&derived).expect("derived root");
    assert_eq!(*derived.output(), 0);

    let source = engine.plan(seed_source, ()).expect("source plan");
    let source_running = engine.run(&source).expect("source root");
    assert_eq!(
        engine.snapshot().observe::<PlainDerived>(7).as_deref(),
        Some(&6)
    );
    assert_eq!(*derived.output(), 6);
    engine.remove(&source_running).expect("remove source root");
    assert_eq!(engine.snapshot().observe::<PlainSource>(7), None);
    assert_eq!(
        engine.snapshot().observe::<PlainDerived>(7).as_deref(),
        Some(&0)
    );
    assert_eq!(*derived.output(), 0);
}

#[test]
fn command_effect_rules_and_typed_subscriptions() {
    let mut engine = Engine::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    engine
        .subscribe::<PlainSource>(move |snapshot, count| {
            assert_eq!(count, 1);
            assert_eq!(snapshot.observe::<PlainSource>(9).as_deref(), Some(&2));
            seen.fetch_add(1, Ordering::SeqCst);
        })
        .expect("subscribe");

    let observe_error = engine.command(|| {
        let _ = observe_view::<PlainSource>()?.get(&9)?;
        Ok(())
    });
    assert!(matches!(
        observe_error,
        Err(Error::InvalidCommandEffect { .. })
    ));
    let run_error = engine.command(|| {
        let _ = run(double, 9)?;
        Ok(())
    });
    assert!(matches!(run_error, Err(Error::InvalidCommandEffect { .. })));
    let panic_result = engine.command(|| -> Result<()> {
        emit_view::<PlainSource>()?.insert(9, 2)?;
        panic!("command panic")
    });
    assert!(matches!(panic_result, Err(Error::Panic(_))));
    assert!(engine.snapshot().observe::<PlainSource>(9).is_none());

    engine
        .command(|| {
            emit_view::<PlainSource>()?.insert(9, 2)?;
            Ok(())
        })
        .expect("successful command");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
