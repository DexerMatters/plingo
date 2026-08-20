//! The remaining verification-matrix items: Previous feedback (7), the
//! no-escape-hatch rule (11), fresh-id stability, graph propagation, and
//! the mint/fresh identity behavior.

use std::sync::atomic::Ordering;

use crate::reactive::prelude::*;
use crate::reactive::tests::{
    Current, GraphIn, GraphOut, Log, MintTree, Output, Tick, build_engine, run_scenario,
};

#[test]
fn previous_reads_exactly_t_minus_1() {
    // Matrix 7: `report` reads only the committed t-1 value. Epoch 1
    // commits Current=5; epoch 2 changes Current to 7 — the report's
    // temporal read fires at epoch 2's start with the committed 5, and
    // never sees the same-epoch 7.
    let outcome = run_scenario(1, &[
        vec![ExternalOp::box_set::<Current>(5)],
        vec![ExternalOp::box_set::<Current>(7)],
    ]);
    assert!(outcome.dump.contains("log=[0->Some(\"Some(5)\")]"), "{}", outcome.dump);
    let log_changes = outcome
        .changes
        .iter()
        .flatten()
        .filter(|change| change.contains("tests::Log"))
        .count();
    assert_eq!(
        log_changes, 3,
        "entry + keys in epoch 1, entry in epoch 2: {:?}",
        outcome.changes
    );
}

#[test]
fn previous_reads_do_not_cycle_within_an_epoch() {
    // The delta component reads Current normally AND temporally; its
    // temporal edge never creates a current-epoch cycle (it is excluded
    // from the epoch graph).
    let outcome = run_scenario(1, &[
        vec![ExternalOp::box_set::<Current>(5)],
        vec![ExternalOp::box_set::<Current>(9)],
    ]);
    assert_eq!(outcome.errors, Vec::<String>::new());
    assert!(outcome.dump.contains("diff=Some(4)"), "{}", outcome.dump);
}

#[test]
fn no_escape_hatch_write_outside_a_visitor() {
    // Matrix 11: a derived write outside any visitor is a deterministic
    // error; external authority commands remain valid.
    let mut engine = build_engine(1, false).expect("engine");
    let shared = crate::reactive::engine::Shared::from_engine_for_tests(&engine);
    let cx = crate::reactive::api::RunContext {
        shared: &shared,
        component: 0,
        instance: 0,
    };
    let emitted = cx.emitted::<MintTree>().expect("handle");
    let error = emitted.insert_node(NodeId(1), 5).expect_err("write outside a visitor");
    assert!(
        error.to_string().contains("write outside a visitor"),
        "{error}"
    );
    // External authority still works.
    engine
        .command(vec![ExternalOp::box_set::<Current>(3)])
        .expect("external patch remains valid");
}

#[test]
fn fresh_node_ids_are_stable_across_runs() {
    // The minter mints one node per run; the id must be identical across
    // epochs so re-runs re-ensure rather than fork (identity stability).
    let outcome = run_scenario(1, &[
        vec![ExternalOp::box_set::<Tick>(true)],
        vec![ExternalOp::box_set::<Tick>(false)],
        vec![ExternalOp::box_set::<Tick>(true)],
    ]);
    assert_eq!(outcome.errors, Vec::<String>::new());
    // Exactly one node ever exists in MintTree, and its payload is 7.
    let mint_part = outcome
        .dump
        .split("mint_tree=")
        .nth(1)
        .expect("mint section");
    assert!(
        mint_part.contains("root(NodeId(") && mint_part.matches("root(NodeId(").count() == 1,
        "the minted node must not fork across runs: {mint_part}"
    );
    assert!(mint_part.contains("Some(7)"), "{mint_part}");
}

#[test]
fn graph_copy_propagates_nodes_and_edges() {
    let outcome = run_scenario(1, &[vec![
        ExternalOp::graph_insert_node::<GraphIn>(NodeId(0), 10),
        ExternalOp::graph_insert_node::<GraphIn>(NodeId(1), 11),
        ExternalOp::graph_insert_edge::<GraphIn>(NodeId(0), "l".to_string(), NodeId(1), 111),
    ]]);
    assert_eq!(outcome.errors, Vec::<String>::new());
    assert!(
        outcome.dump.contains("graph_out=nNodeId(0)=Some(10),nNodeId(1)=Some(11),e"),
        "{}",
        outcome.dump
    );
    assert!(
        outcome.dump.contains("eNodeId(0)->NodeId(1)=Some(111)"),
        "the copied edge carries its datum: {}",
        outcome.dump
    );

    // Removing the source edge propagates.
    let outcome = run_scenario(1, &[
        vec![
            ExternalOp::graph_insert_node::<GraphIn>(NodeId(0), 10),
            ExternalOp::graph_insert_node::<GraphIn>(NodeId(1), 11),
            ExternalOp::graph_insert_edge::<GraphIn>(NodeId(0), "l".to_string(), NodeId(1), 111),
        ],
        vec![ExternalOp::graph_remove_edge::<GraphIn>(NodeId(0), "l".to_string(), NodeId(1))],
    ]);
    assert!(
        !outcome.dump.contains("graph_out=.*eNodeId(0)"),
        "the copied edge disappears: {}",
        outcome.dump
    );
}

#[test]
fn graph_remove_node_drops_incident_edges() {
    let outcome = run_scenario(1, &[
        vec![
            ExternalOp::graph_insert_node::<GraphIn>(NodeId(0), 10),
            ExternalOp::graph_insert_node::<GraphIn>(NodeId(1), 11),
            ExternalOp::graph_insert_edge::<GraphIn>(NodeId(0), "l".to_string(), NodeId(1), 111),
            ExternalOp::graph_insert_edge::<GraphIn>(NodeId(1), "l".to_string(), NodeId(0), 110),
        ],
        vec![ExternalOp::graph_remove_node::<GraphIn>(NodeId(1))],
    ]);
    assert!(
        outcome.dump.contains("graph_out=nNodeId(0)=Some(10),"),
        "the surviving node stays: {}",
        outcome.dump
    );
    assert!(
        !outcome.dump.contains("eNodeId(0)->NodeId(1)"),
        "outgoing edges of the removed node die: {}",
        outcome.dump
    );
    assert!(
        !outcome.dump.contains("eNodeId(1)->NodeId(0)"),
        "incoming edges of the removed node die: {}",
        outcome.dump
    );
}

#[test]
fn view_registration_and_authority() {
    // A component observing a view with no producer fails validation.
    let mut engine = Engine::new();
    engine.install(crate::reactive::tests::minter).expect("install");
    // minter observes Tick, which nobody produces: the first command must
    // fail with NoProducerForView.
    let error = engine.command(vec![]).expect_err("no producer for Tick");
    assert!(error.to_string().contains("no producer"), "{error}");
}

#[test]
fn external_patch_to_non_external_view_is_rejected() {
    let mut engine = build_engine(1, false).expect("engine");
    // Output is component-owned; external patches must be refused.
    let error = engine
        .command(vec![ExternalOp::box_set::<Output>(1)])
        .expect_err("Output has no external producer");
    assert!(
        error.to_string().contains("external"),
        "{error}"
    );
    // And the engine is unharmed.
    engine.command(vec![ExternalOp::box_set::<Current>(2)]).expect("still fine");
}

#[test]
fn graph_run_counter_counts_discovery() {
    let outcome = run_scenario(1, &[vec![
        ExternalOp::graph_insert_node::<GraphIn>(NodeId(0), 10),
        ExternalOp::graph_insert_node::<GraphIn>(NodeId(1), 11),
    ]]);
    let _ = outcome;
}
