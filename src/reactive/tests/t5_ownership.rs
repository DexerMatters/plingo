//! T5 — Ownership merge: two producers committing disjoint fact sets
//! commute and both commit; an overlapping write is a deterministic
//! validation error that aborts the epoch; equal re-emission is a no-op
//! (matrix 8).

use crate::reactive::prelude::*;
use crate::reactive::tests::{Shared, Tick, run_scenario, run_scenario_engine};

#[test]
fn disjoint_producer_facts_both_commit() {
    let outcome = run_scenario(1, &[vec![ExternalOp::box_set::<Tick>(true)]]);
    assert_eq!(outcome.errors, Vec::<String>::new());
    assert!(
        outcome.dump.contains("shared=[1->Some(\"a1\"),2->Some(\"b2\"),3->Some(\"a3\")]")
            || outcome
                .dump
                .contains("shared=[1->Some(\"a1\"),3->Some(\"a3\"),2->Some(\"b2\")]"),
        "{}",
        outcome.dump
    );
}

#[test]
fn overlapping_producer_write_is_a_deterministic_error() {
    let outcome = run_scenario_engine(1, &[vec![ExternalOp::box_set::<Tick>(true)]], true);
    assert_eq!(outcome.errors.len(), 1);
    assert!(
        outcome.errors[0].contains("ownership violation"),
        "{}",
        outcome.errors[0]
    );
    assert!(outcome.dump.contains("shared=[]"), "{}", outcome.dump);
}

#[test]
fn equal_re_emission_publishes_nothing() {
    // Every toggle re-runs the producers; re-emitting the same values
    // publishes nothing and enqueues nothing downstream (T4/T5).
    let outcome = run_scenario(1, &[
        vec![ExternalOp::box_set::<Tick>(true)],
        vec![ExternalOp::box_set::<Tick>(false)],
        vec![ExternalOp::box_set::<Tick>(true)],
    ]);
    // The shared map's facts appear in the changed sequence exactly once:
    // their first command's creation. Re-emissions are equal and silent.
    let shared_changes = outcome
        .changes
        .iter()
        .flatten()
        .filter(|change| change.contains("tests::Shared"))
        .count();
    // Three entry creations plus the Keys registry change, all in the
    // first command; re-emissions are equal and silent.
    assert_eq!(shared_changes, 4, "three entries + the keys registry: {:?}", outcome.changes);
    let tick_changes = outcome
        .changes
        .iter()
        .flatten()
        .filter(|change| change.contains("tests::Tick"))
        .count();
    assert_eq!(tick_changes, 3, "every toggle still changes Tick: {:?}", outcome.changes);
}
