//! Uniform view/effect contract coverage.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::reactive::api::{run, run_each_child, run_each_child_of, run_each_key};
use crate::reactive::kind::{Map, emit_view, observe_view};
use crate::reactive::prelude::*;
use crate::view;

#[view]
struct Source(Map<u64, i64>);

#[view]
struct Doubled(Map<u64, i64>);

#[view]
struct Conditional(Map<u64, i64>);

#[view]
struct Delta(Map<u64, i64>);

#[view]
struct Singleton(Map<(), i64>);

#[view]
struct DomainCount(Map<(), i64>);

fn double(input: u64) -> Result<i64> {
    let value = observe_view::<Source>()?
        .get(&input)?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<Doubled>()?.insert(input, value * 2)?;
    Ok(value * 2)
}

fn conditional(input: u64) -> Result<i64> {
    let Some(value) = observe_view::<Source>()?.get(&input)? else {
        return Ok(0);
    };
    emit_view::<Conditional>()?.insert(input, *value)?;
    Ok(*value)
}

fn previous(input: u64) -> Result<i64> {
    let prior = observe_view::<Source>()?
        .get_previous(&input)?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<Delta>()?.insert(input, prior)?;
    Ok(prior)
}

fn singleton_count(_: ()) -> Result<i64> {
    let count = observe_view::<Singleton>()?.keys()?.len() as i64;
    emit_view::<DomainCount>()?.insert((), count)?;
    Ok(count)
}

#[test]
fn owned_outputs_and_deterministic_domains() {
    let mut engine = Engine::new();
    let planned = engine.plan(double, 7).expect("plan");
    let running = engine.run(&planned).expect("run");
    assert_eq!(*running.output(), 0);

    engine
        .command(|| {
            emit_view::<Source>()?.insert(7, 3)?;
            emit_view::<Source>()?.insert(2, 8)?;
            Ok(())
        })
        .expect("source command");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.observe::<Doubled>(7).as_deref(), Some(&6));
    assert_eq!(snapshot.inputs::<Source>(), vec![7, 2]);
    assert_eq!(*running.output(), 6);
}

#[test]
fn singleton_absence_and_presence_are_distinct() {
    let mut engine = Engine::new();
    let planned = engine.plan(singleton_count, ()).expect("plan");
    let running = engine.run(&planned).expect("run");
    assert_eq!(*running.output(), 0);
    assert!(engine.snapshot().inputs::<Singleton>().is_empty());

    engine
        .command(|| {
            emit_view::<Singleton>()?.insert((), 9)?;
            Ok(())
        })
        .expect("singleton command");
    assert_eq!(*running.output(), 1);
    assert_eq!(engine.snapshot().inputs::<Singleton>(), vec![()]);
}

#[test]
fn current_previous_and_omitted_writes_use_one_contract() {
    let mut engine = Engine::new();
    let previous_plan = engine.plan(previous, 4).expect("previous plan");
    let previous_running = engine.run(&previous_plan).expect("previous run");
    let conditional_plan = engine.plan(conditional, 4).expect("conditional plan");
    let conditional_running = engine.run(&conditional_plan).expect("conditional run");

    engine
        .command(|| {
            emit_view::<Source>()?.insert(4, 6)?;
            Ok(())
        })
        .expect("first source command");
    assert_eq!(*conditional_running.output(), 6);
    assert_eq!(*previous_running.output(), 0);

    engine
        .command(|| {
            emit_view::<Source>()?.insert(9, 1)?;
            Ok(())
        })
        .expect("next epoch");
    assert_eq!(*previous_running.output(), 6);

    engine
        .command(|| {
            emit_view::<Source>()?.remove(4)?;
            Ok(())
        })
        .expect("retraction command");
    assert!(engine.snapshot().observe::<Conditional>(4).is_none());
    assert_eq!(*previous_running.output(), 6);
}

#[test]
fn command_panic_rolls_back_and_subscriptions_read_snapshots() {
    let mut engine = Engine::new();
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_subscription = Arc::clone(&seen);
    engine
        .subscribe::<Source>(move |snapshot, count| {
            assert_eq!(count, 1);
            assert_eq!(snapshot.observe::<Source>(3).as_deref(), Some(&5));
            seen_subscription.fetch_add(1, Ordering::SeqCst);
        })
        .expect("subscription");

    let panic_result = engine.command(|| -> Result<()> {
        emit_view::<Source>()?.insert(3, 4)?;
        panic!("rollback");
    });
    assert!(matches!(panic_result, Err(Error::Panic(_))));
    assert!(engine.snapshot().observe::<Source>(3).is_none());

    engine
        .command(|| {
            emit_view::<Source>()?.insert(3, 5)?;
            Ok(())
        })
        .expect("source command");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[test]
fn an_empty_domain_observation_wakes_when_a_writer_appears() {
    let mut engine = Engine::new();
    let planned = engine.plan(singleton_count, ()).expect("plan");
    let running = engine.run(&planned).expect("run");
    engine
        .command(|| {
            emit_view::<Singleton>()?.insert((), 1)?;
            Ok(())
        })
        .expect("late writer");
    assert_eq!(*running.output(), 1);
}

#[test]
fn equal_writes_are_cold_and_do_not_run_subscribers() {
    let runs = std::sync::Arc::new(AtomicUsize::new(0));
    let counted = {
        let runs = std::sync::Arc::clone(&runs);
        move |input: u64| -> Result<i64> {
            runs.fetch_add(1, Ordering::SeqCst);
            double(input)
        }
    };
    let mut engine = Engine::new();
    let planned = engine.plan(counted, 2).expect("plan");
    let _running = engine.run(&planned).expect("run");
    engine
        .command(|| {
            emit_view::<Source>()?.insert(2, 3)?;
            Ok(())
        })
        .expect("first write");
    let before = runs.load(Ordering::SeqCst);
    let report = engine
        .command(|| {
            emit_view::<Source>()?.insert(2, 3)?;
            Ok(())
        })
        .expect("equal write");
    assert_eq!(report.rounds, 0);
    assert_eq!(report.changed::<Source>(), 0);
    assert_eq!(runs.load(Ordering::SeqCst), before);
}
