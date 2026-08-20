//! T6 — Termination and full rollback: a fact cycle aborts the epoch with
//! a cycle listing; any failure leaves every store, ownership entry,
//! counter, and subscription exactly as before the epoch (matrix 9, 10).

use crate::reactive::prelude::*;
use crate::reactive::tests::{
    Cells, Deps, Outcome, Output, Shared, Source, Tick, build_engine, dump, run_scenario,
    run_scenario_engine,
};

fn mutual_commands() -> Vec<Vec<ExternalOp>> {
    vec![vec![
        ExternalOp::map_set::<Cells>(0, 0),
        ExternalOp::map_set::<Deps>(1, 0),
        ExternalOp::map_set::<Deps>(2, 3), // mutual: 2 -> 3 -> 2
        ExternalOp::map_set::<Deps>(3, 2),
    ]]
}

#[test]
fn mutual_fact_cycle_is_rejected_with_a_listing() {
    let outcome = run_scenario(1, &mutual_commands());
    assert_eq!(outcome.errors.len(), 1, "the epoch must be rejected");
    let error = &outcome.errors[0];
    assert!(error.contains("fact cycle"), "cycle listing expected: {error}");
    assert!(outcome.dump.contains("cells=[]"), "full rollback: {}", outcome.dump);
    assert!(outcome.dump.contains("deps=[]"), "external facts also rolled back: {}", outcome.dump);
    assert_eq!(outcome.subs, Vec::<String>::new(), "no subscription may fire");
}

#[test]
fn self_read_write_cycle_is_rejected() {
    // A visitor that reads the exact fact it writes is a 1-cycle
    // (fact-level self-observation). cells(1) depends on itself via
    // deps(1) = 1 and publishes provisionally, so the cycle manifests.
    let outcome = run_scenario(1, &[vec![
        ExternalOp::map_set::<Cells>(0, 0),
        ExternalOp::map_set::<Deps>(1, 1), // cells(1) depends on itself
        ExternalOp::map_set::<Deps>(2, 0),
    ]]);
    assert!(
        outcome.errors.iter().any(|e| e.contains("fact cycle")),
        "self-dependency must be a cycle: {:?}",
        outcome.errors
    );
    assert!(outcome.dump.contains("cells=[]"), "{}", outcome.dump);
}

#[test]
fn ownership_violation_aborts_and_rolls_back() {
    // producer_overlap writes key 1 which producer_a owns: deterministic
    // validation error, epoch aborted, nothing committed.
    let outcome = run_scenario_engine(1, &[vec![ExternalOp::box_set::<Tick>(true)]], true);
    assert_eq!(outcome.errors.len(), 1);
    assert!(
        outcome.errors[0].contains("ownership violation"),
        "{}",
        outcome.errors[0]
    );
    assert!(outcome.dump.contains("shared=[]"), "full rollback: {}", outcome.dump);
    assert_eq!(outcome.subs, Vec::<String>::new());

    // The engine stays consistent after the rollback: a later good command
    // commits normally (without the overlap producer).
    let good = run_scenario(1, &[
        vec![ExternalOp::box_set::<Tick>(true)],
        vec![ExternalOp::box_set::<Tick>(false)],
    ]);
    assert!(good.dump.contains("shared=[1->Some(\"a1\"),2->Some(\"b2\"),3->Some(\"a3\")]")
        || good
            .dump
            .contains("shared=[1->Some(\"a1\"),3->Some(\"a3\"),2->Some(\"b2\")]"),
        "{}",
        good.dump
    );
}

#[test]
fn authored_error_aborts_and_rolls_back() {
    let _guard = crate::reactive::tests::counter_guard();
    let mut engine = build_engine(1, false).expect("engine");
    engine.install(crate::reactive::tests::failer).expect("failer");
    let log = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    crate::reactive::tests::subscribe_named::<Output>(&mut engine, "output", &log).unwrap();

    // First command: the failer sees tick=false and stays quiet.
    engine
        .command(vec![
            ExternalOp::box_set::<Source>(5),
            ExternalOp::box_set::<Tick>(false),
        ])
        .expect("benign");
    let before = dump(&engine);

    // Second command: the failer errors; the epoch aborts and rolls back
    // everything, including the Source edit it accompanied.
    let report = engine.command(vec![
        ExternalOp::box_set::<Source>(7),
        ExternalOp::box_set::<Tick>(true),
    ]);
    let error = report.expect_err("the failer's error must abort the epoch");
    assert!(error.to_string().contains("boom"), "{error}");

    assert_eq!(dump(&engine), before, "rollback restores every store");
    assert_eq!(log.lock().len(), 1, "the first command's deliveries stay; the failed epoch fires none");
    assert_eq!(engine.shared.counters.lock().epoch, 1, "counter unchanged");

    // The engine recovers: the failer is quiet again and the edit commits.
    let report = engine
        .command(vec![ExternalOp::box_set::<Source>(6)])
        .expect("recovers");
    assert_eq!(report.epoch, 2);
    assert!(dump(&engine).contains("output=Some(18)"), "{}", dump(&engine));
}

#[test]
fn panic_aborts_and_rolls_back() {
    let _guard = crate::reactive::tests::counter_guard();
    let mut engine = build_engine(1, false).expect("engine");
    engine
        .install(crate::reactive::tests::panicker)
        .expect("panicker");
    let before = dump(&engine);
    let error = engine
        .command(vec![ExternalOp::box_set::<Source>(3)])
        .expect_err("panic must abort the epoch");
    assert!(error.to_string().contains("panic"), "{error}");
    assert_eq!(dump(&engine), before);
    assert_eq!(engine.shared.counters.lock().epoch, 0);
}

#[test]
fn cycle_rejection_is_deterministic_under_workers() {
    let one = run_scenario(1, &mutual_commands());
    let many = run_scenario(0, &mutual_commands());
    assert_eq!(one, many, "T3/T6: rejection outcome identical under any worker count");
}

#[test]
fn acyclic_chains_terminate_in_bounded_rounds() {
    let outcome = run_scenario(1, &[vec![
        ExternalOp::map_set::<Cells>(0, 0),
        ExternalOp::map_set::<Deps>(1, 3),
        ExternalOp::map_set::<Deps>(2, 1),
        ExternalOp::map_set::<Deps>(3, 0),
    ]]);
    assert_eq!(outcome.errors, Vec::<String>::new());
    let rounds = outcome.rounds[0];
    assert!(rounds <= 6, "bounded by the dependency chain depth: {rounds}");
    assert!(rounds >= 3, "the fixed point really iterates: {rounds}");
}
