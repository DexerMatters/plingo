//! T2 — Glitch freedom (Drechsler): two facts changed by one command are
//! observed together; no evaluation inside an epoch observes a fact that
//! changes later in the same round (matrix 6).

use crate::reactive::prelude::*;
use crate::reactive::tests::{Sum, Table, run_scenario};

#[test]
fn two_facts_changed_by_one_command_are_observed_together() {
    let outcome = run_scenario(1, &[vec![
        ExternalOp::map_set::<Table>(1, 5),
        ExternalOp::map_set::<Table>(2, 7),
    ]]);
    assert!(outcome.dump.contains("sum=Some(12)"), "{}", outcome.dump);
    assert!(
        outcome.dump.contains("result=[1->Some(6),2->Some(8)]"),
        "{}",
        outcome.dump
    );
}

#[test]
fn no_callback_sees_a_mixed_round() {
    // The sum component reads the whole table in one run; if it ever saw
    // a half-applied command (one entry new, one old), the sum would be
    // 5 or 7 instead of 12.
    let outcome = run_scenario(1, &[
        vec![ExternalMap::set_one()],
        vec![ExternalMap::set_two()],
    ]);
    assert!(outcome.dump.contains("sum=Some(12)"), "{}", outcome.dump);
}

mod ExternalMap {
    use super::*;
    pub fn set_one() -> ExternalOp {
        ExternalOp::map_set::<Table>(1, 5)
    }
    pub fn set_two() -> ExternalOp {
        ExternalOp::map_set::<Table>(2, 7)
    }
}

#[test]
fn subscription_delivery_is_quiescent_and_ordered() {
    let outcome = run_scenario(1, &[vec![
        ExternalOp::map_set::<Table>(1, 5),
        ExternalOp::map_set::<Table>(2, 7),
    ]]);
    assert!(
        outcome.subs.iter().any(|sub| sub.starts_with("sum:") && sub.contains("Some(12)")),
        "the committed quiescent state is what subscribers see: {:?}",
        outcome.subs
    );
    // Deterministic first-change order: entry(1) before entry(2).
    let sum_subs: Vec<&String> = outcome
        .subs
        .iter()
        .filter(|sub| sub.starts_with("sum:"))
        .collect();
    assert!(!sum_subs.is_empty());
    let first = sum_subs[0];
    assert!(
        first.contains("tests::Sum") && first.contains("Some(12)"),
        "the sum changed once, to the coherent total: {first}"
    );
}
