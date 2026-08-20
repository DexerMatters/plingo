//! T3 — Determinism: same initial state and same ordered command sequence
//! yield identical committed states, changed-fact sequences, subscription
//! deliveries, and logical counters under any worker count (matrix 12).
//!
//! Every scenario below runs once with one worker and once with
//! `available_parallelism()` workers; the full observable outcome
//! (snapshot dump, changed sequence, subscription sequence, errors,
//! epoch/round/run counters) must be identical.

use crate::reactive::prelude::*;
use crate::reactive::tests::{Cells, Deps, Source, SourceTree, Table, Tick, run_scenario};

fn available_workers() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2)
}

fn assert_deterministic(commands: &[Vec<ExternalOp>]) {
    let one = run_scenario(1, commands);
    let many = run_scenario(0, commands);
    assert!(
        many.runs.iter().any(|r| *r > 0),
        "the many-worker run must actually execute work"
    );
    assert_eq!(
        one, many,
        "one worker and available_parallelism() workers must agree on every observable"
    );
    let _ = available_workers();
}

#[test]
fn determinism_cold_start_and_box_pipeline() {
    assert_deterministic(&[
        vec![ExternalOp::box_set::<Source>(6)],
        vec![ExternalOp::box_set::<Source>(8)],
        vec![ExternalOp::box_set::<Source>(8)], // equal no-op
    ]);
}

#[test]
fn determinism_map_lifecycle() {
    assert_deterministic(&[
        vec![ExternalOp::map_set::<Table>(1, 10)],
        vec![
            ExternalOp::map_set::<Table>(2, 20),
            ExternalOp::map_set::<Table>(3, 30),
        ],
        vec![ExternalOp::map_set::<Table>(2, 21)],
        vec![ExternalOp::map_remove::<Table>(1)],
        vec![ExternalOp::map_rekey::<Table>(3, 9)],
    ]);
}

#[test]
fn determinism_glitch_fan_in() {
    assert_deterministic(&[vec![
        ExternalOp::map_set::<Table>(1, 5),
        ExternalOp::map_set::<Table>(2, 7),
        ExternalOp::map_set::<Table>(3, 9),
    ]]);
}

#[test]
fn determinism_forward_reference_fixed_point() {
    assert_deterministic(&[
        vec![
            ExternalOp::map_set::<Cells>(0, 0),
            ExternalOp::map_set::<Deps>(1, 3),
            ExternalOp::map_set::<Deps>(2, 1),
            ExternalOp::map_set::<Deps>(3, 0),
        ],
        vec![ExternalOp::map_set::<Deps>(2, 0)], // restructure the chain
    ]);
}

#[test]
fn determinism_tree_and_nested_visitors() {
    let root = NodeId(0);
    assert_deterministic(&[
        vec![
            ExternalOp::tree_insert_node::<SourceTree>(root, 1),
            ExternalOp::tree_insert_node::<SourceTree>(NodeId(1), 2),
            ExternalOp::tree_insert_node::<SourceTree>(NodeId(2), 3),
            ExternalOp::tree_move_node::<SourceTree>(NodeId(1), root),
            ExternalOp::tree_move_node::<SourceTree>(NodeId(2), root),
        ],
        vec![ExternalOp::tree_update_node::<SourceTree>(NodeId(2), 30)],
        vec![ExternalOp::tree_reorder_children::<SourceTree>(root, vec![NodeId(2), NodeId(1)])],
        vec![ExternalOp::tree_remove_node::<SourceTree>(NodeId(1))],
    ]);
}

#[test]
fn determinism_multi_producer() {
    assert_deterministic(&[
        vec![ExternalOp::box_set::<Tick>(true)],
        vec![ExternalOp::box_set::<Tick>(false)],
    ]);
}

#[test]
fn determinism_feedback_and_dynamic_branch() {
    assert_deterministic(&[
        vec![ExternalOp::box_set::<crate::reactive::tests::Current>(5)],
        vec![ExternalOp::box_set::<crate::reactive::tests::Current>(9)],
        vec![
            ExternalOp::box_set::<crate::reactive::tests::Switch>(true),
            ExternalOp::box_set::<crate::reactive::tests::BranchA>(1),
            ExternalOp::box_set::<crate::reactive::tests::BranchB>(2),
        ],
        vec![ExternalOp::box_set::<crate::reactive::tests::Switch>(false)],
        vec![ExternalOp::box_set::<crate::reactive::tests::BranchA>(99)],
    ]);
}

#[test]
fn determinism_cycle_rejection_and_rollback() {
    // The mutual-dependency command must fail identically under both
    // worker counts, and the rolled-back state must be identical.
    let commands = [vec![
        ExternalOp::map_set::<Cells>(0, 0),
        ExternalOp::map_set::<Deps>(1, 0),
        ExternalOp::map_set::<Deps>(2, 3),
        ExternalOp::map_set::<Deps>(3, 2),
    ]];
    let one = run_scenario(1, &commands);
    let many = run_scenario(0, &commands);
    assert_eq!(one.errors.len(), 1);
    assert_eq!(one, many);
}
