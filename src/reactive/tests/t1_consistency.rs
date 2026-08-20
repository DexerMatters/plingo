//! T1 — Consistency (Acar): propagation from the epoch's initial state
//! converges to exactly the state a from-scratch re-evaluation of every
//! component would commit.

use crate::reactive::prelude::*;
use crate::reactive::tests::{Cells, Deps, Outcome, Source, Table, dump, run_scenario};

fn chain_commands() -> Vec<Vec<ExternalOp>> {
    vec![
        vec![
            ExternalOp::map_set::<Cells>(0, 0),
            ExternalOp::map_set::<Deps>(1, 3), // forward reference: 1 depends on 3
            ExternalOp::map_set::<Deps>(2, 1),
            ExternalOp::map_set::<Deps>(3, 0),
        ],
    ]
}

#[test]
fn incremental_converges_to_from_scratch() {
    // Incremental: cell 0 arrives first, the chain resolves across epochs.
    let incremental = run_scenario(1, &[
        vec![ExternalOp::map_set::<Cells>(0, 0)],
        vec![
            ExternalOp::map_set::<Deps>(1, 3),
            ExternalOp::map_set::<Deps>(2, 1),
            ExternalOp::map_set::<Deps>(3, 0),
        ],
    ]);
    // From scratch: the same final external state in one command.
    let from_scratch = run_scenario(1, &chain_commands());

    assert_eq!(from_scratch.dump, incremental.dump, "T1: propagated state must equal a from-scratch re-evaluation");
    assert!(
        from_scratch.dump.contains("cells=[1->Some(2),2->Some(3),3->Some(1),0->Some(0)]") ||
            from_scratch.dump.contains("cells=[0->Some(0),1->Some(2),2->Some(3),3->Some(1)]"),
        "forward references must resolve: {}",
        from_scratch.dump
    );
}

#[test]
fn cold_start_publishes_in_dependency_order() {
    // The chain resolves through the round iteration without an authored
    // loop (matrix 5): cells(1) = cells(3)+1 = 2, cells(2) = cells(1)+1 = 3,
    // cells(3) = cells(0)+1 = 1.
    let outcome = run_scenario(1, &chain_commands());
    assert_eq!(outcome.errors, Vec::<String>::new());
    let snapshot = outcome_dump_cells(&outcome);
    assert_eq!(snapshot.get(&0), Some(&0));
    assert_eq!(snapshot.get(&1), Some(&2));
    assert_eq!(snapshot.get(&2), Some(&3));
    assert_eq!(snapshot.get(&3), Some(&1));
}

fn outcome_dump_cells(outcome: &Outcome) -> std::collections::HashMap<u64, i64> {
    let mut map = std::collections::HashMap::new();
    // The dump renders cells=[k->Some(v),...] in rank order.
    let cells_part = outcome
        .dump
        .split("cells=[")
        .nth(1)
        .expect("cells section")
        .split(']')
        .next()
        .expect("cells close");
    if cells_part.is_empty() {
        return map;
    }
    for entry in cells_part.split(',') {
        let (key, value) = entry.split_once("->").expect("key->value");
        let key: u64 = key.parse().expect("key");
        let value: i64 = value
            .trim_start_matches("Some(")
            .trim_end_matches(')')
            .parse()
            .expect("value");
        map.insert(key, value);
    }
    map
}

#[test]
fn consistency_of_a_plain_derivation() {
    // A simple two-stage pipeline converges identically in one command and
    // incrementally: Source -> Output (*3) -> Half (/2).
    let one_shot = run_scenario(1, &[vec![ExternalOp::box_set::<Source>(9)]]);
    let incremental = run_scenario(1, &[
        vec![ExternalOp::box_set::<Source>(4)],
        vec![ExternalOp::box_set::<Source>(9)],
    ]);
    assert_eq!(one_shot.dump, incremental.dump);
    assert!(one_shot.dump.contains("output=Some(27)"), "{}", one_shot.dump);
    assert!(one_shot.dump.contains("half=Some(13)"), "{}", one_shot.dump);
}

#[test]
fn equal_command_produces_no_epoch_work() {
    let outcome = run_scenario(1, &[
        vec![ExternalOp::box_set::<Source>(5)],
        vec![ExternalOp::box_set::<Source>(5)], // equal final state
    ]);
    assert_eq!(outcome.rounds[1], 0, "equal final state ⇒ no epoch work");
    assert_eq!(outcome.epochs[1], 1, "no commit, no counter bump");
    assert_eq!(outcome.changes[1], Vec::<String>::new(), "no changed facts");
}

#[test]
fn table_pipeline_converges() {
    let one_shot = run_scenario(1, &[vec![
        ExternalOp::map_set::<Table>(1, 10),
        ExternalOp::map_set::<Table>(2, 20),
    ]]);
    let incremental = run_scenario(1, &[
        vec![ExternalOp::map_set::<Table>(1, 10)],
        vec![ExternalOp::map_set::<Table>(2, 20)],
    ]);
    assert_eq!(one_shot.dump, incremental.dump);
    assert!(one_shot.dump.contains("result=[1->Some(11),2->Some(21)]"), "{}", one_shot.dump);
}
