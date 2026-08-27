//! T6 — cycles and authored failures leave committed state untouched.

use crate::reactive::kind::Map;
use crate::reactive::prelude::*;
use crate::view;

#[view]
struct T6A(Map<(), i64>);

#[view]
struct T6B(Map<(), i64>);

#[view]
struct T6Source(Map<u64, i64>);

#[view]
struct T6Derived(Map<u64, i64>);

fn cycle_a(_: ()) -> Result<i64> {
    let value = observe_view::<T6B>()?
        .get(&())?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<T6A>()?.insert((), value + 1)?;
    Ok(value + 1)
}

fn cycle_b(_: ()) -> Result<i64> {
    let value = observe_view::<T6A>()?
        .get(&())?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<T6B>()?.insert((), value + 1)?;
    Ok(value + 1)
}

fn failing(input: u64) -> Result<i64> {
    let value = observe_view::<T6Source>()?
        .get(&input)?
        .map(|value| *value)
        .unwrap_or_default();
    emit_view::<T6Derived>()?.insert(input, value)?;
    if value == 9 {
        return Err(Error::authored(std::io::Error::other("rerun")));
    }
    Ok(value)
}

#[test]
fn dependency_cycles_are_rejected_transactionally() {
    let mut engine = Engine::new();
    let first_plan = engine.plan(cycle_a, ()).expect("first plan");
    let _first = engine.run(&first_plan).expect("first run");
    let second_plan = engine.plan(cycle_b, ()).expect("second plan");
    let error = match engine.run(&second_plan) {
        Ok(_) => panic!("cycle must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::DependencyCycle { .. }));
    assert_eq!(engine.snapshot().observe::<T6A>(()).as_deref(), Some(&1));
    assert!(engine.snapshot().observe::<T6B>(()).is_none());
}

#[test]
fn command_and_rerun_failures_restore_the_previous_snapshot() {
    let mut engine = Engine::new();
    let plan = engine.plan(failing, 4).expect("plan");
    let running = engine.run(&plan).expect("run");
    engine
        .command(|| {
            emit_view::<T6Source>()?.insert(4, 1)?;
            Ok(())
        })
        .expect("successful command");
    assert_eq!(
        engine.snapshot().observe::<T6Derived>(4).as_deref(),
        Some(&1)
    );

    let error = engine.command(|| {
        emit_view::<T6Source>()?.insert(4, 9)?;
        Ok(())
    });
    assert!(matches!(error, Err(Error::Authored(_))));
    assert_eq!(
        engine.snapshot().observe::<T6Source>(4).as_deref(),
        Some(&1)
    );
    assert_eq!(
        engine.snapshot().observe::<T6Derived>(4).as_deref(),
        Some(&1)
    );
    assert_eq!(*running.output(), 1);

    let panic_result = engine.command(|| -> Result<()> {
        emit_view::<T6Source>()?.insert(4, 7)?;
        panic!("command panic");
    });
    assert!(matches!(panic_result, Err(Error::Panic(_))));
    assert_eq!(
        engine.snapshot().observe::<T6Source>(4).as_deref(),
        Some(&1)
    );
}
