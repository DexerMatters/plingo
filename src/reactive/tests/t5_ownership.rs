//! T5 — concrete facts have one writer.

use crate::reactive::api::{run, run_each_child, run_each_child_of, run_each_key};
use crate::reactive::kind::{Map, emit_view, observe_view};
use crate::reactive::prelude::*;
use crate::view;

#[view]
struct T5Fact(Map<u64, i64>);

fn first(_: ()) -> Result<i64> {
    emit_view::<T5Fact>()?.insert(1, 10)?;
    Ok(10)
}

fn second(_: ()) -> Result<i64> {
    emit_view::<T5Fact>()?.insert(1, 20)?;
    Ok(20)
}

fn disjoint(_: ()) -> Result<i64> {
    emit_view::<T5Fact>()?.insert(2, 30)?;
    Ok(30)
}

#[test]
fn overlapping_root_writes_fail_without_partial_commit() {
    let mut engine = Engine::new();
    let first_plan = engine.plan(first, ()).expect("first plan");
    let second_plan = engine.plan(second, ()).expect("second plan");
    let first = engine.run(&first_plan).expect("first run");
    let error = match engine.run(&second_plan) {
        Ok(_) => panic!("overlap must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::ConflictingWrites { .. }));
    assert_eq!(engine.snapshot().observe::<T5Fact>(1).as_deref(), Some(&10));
    assert_eq!(*first.output(), 10);
    assert!(engine.run(&second_plan).is_err());
}

#[test]
fn disjoint_writes_remain_valid() {
    let mut engine = Engine::new();
    let first_plan = engine.plan(first, ()).expect("first plan");
    let _first = engine.run(&first_plan).expect("first run");
    let disjoint_plan = engine.plan(disjoint, ()).expect("disjoint plan");
    let disjoint = engine.run(&disjoint_plan).expect("disjoint run");
    assert_eq!(*disjoint.output(), 30);
    assert_eq!(engine.snapshot().observe::<T5Fact>(2).as_deref(), Some(&30));
}
