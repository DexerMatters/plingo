//! T4 — Minimum delta (DBSP's zero-delta rule at fact granularity) plus
//! identity preservation: only reverse-reachable visitors execute; equal
//! derived output emits no downstream fact; unchanged facts retain `Arc`,
//! revision, and causal identity (matrix 1, 2, 3, 4, 13).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::reactive::prelude::*;
use crate::reactive::tests::{
    BranchA, BranchB, CHILD_RUNS, DISCOVERY_RUNS, HALF_MOD_RUNS, MIRROR_RUNS, MODDER_RUNS,
    Output, ResultTree, Source, SourceTree, Switch, Table, build_engine, run_scenario,
};

#[test]
fn change_one_fact_only_reverse_reachable_visitors_execute() {
    // Matrix 1: the mod pipeline (Source -> Mod -> HalfMod) runs; an
    // unrelated external change runs nobody.
    let outcome = crate::reactive::tests::with_counters(|| {
        let outcome = run_scenario(1, &[
            vec![ExternalOp::box_set::<Source>(6)],
            vec![ExternalOp::box_set::<BranchA>(99)],
        ]);
        assert!(outcome.dump.contains("mod=Some(0)"), "{}", outcome.dump);
        // The second command changed only BranchA; the branch reads B
        // (switch off), so nothing may derive from it.
        assert!(
            !outcome.changes[1]
                .iter()
                .any(|change| change.contains("tests::Mod") || change.contains("tests::HalfMod")),
            "no propagation from an unobserved fact: {:?}",
            outcome.changes
        );
        // The modder ran once (cold start, deduped with the delta0
        // schedule); the half_modder ran at cold start and once more when
        // Mod appeared.
        assert_eq!(MODDER_RUNS.load(Ordering::SeqCst), 1);
        assert_eq!(HALF_MOD_RUNS.load(Ordering::SeqCst), 2);
        outcome
    });
    let _ = outcome;
}

#[test]
fn equal_derived_output_emits_no_downstream_fact() {
    // Matrix 1/T4: Source 4 and 6 both map to Mod 0. The second source
    // change must run modder but NOT half_mod: zero delta, zero work.
    let outcome = crate::reactive::tests::with_counters(|| {
        let outcome = run_scenario(1, &[
            vec![ExternalOp::box_set::<Source>(4)],
            vec![ExternalOp::box_set::<Source>(6)],
        ]);
        assert!(outcome.dump.contains("mod=Some(0)"), "{}", outcome.dump);
        assert!(outcome.dump.contains("half_mod=Some(0)"), "{}", outcome.dump);
        assert_eq!(MODDER_RUNS.load(Ordering::SeqCst), 2, "modder saw both changes");
        assert_eq!(
            HALF_MOD_RUNS.load(Ordering::SeqCst),
            2,
            "half_mod ran at cold start and once more when Mod appeared; the equal re-emission must not reschedule it"
        );
        // The second command's changed sequence contains no HalfMod fact.
        assert!(
            !outcome.changes[1]
                .iter()
                .any(|change| change.contains("tests::HalfMod")),
            "{:?}",
            outcome.changes
        );
        outcome
    });
    let _ = outcome;
}

#[test]
fn dynamic_read_set_switches_with_the_branch() {
    // Matrix 2: switch=true reads A; switch=false reads B. After a flip,
    // changes to the unread side must not visit the branch.
    let outcome = run_scenario(1, &[
        vec![
            ExternalOp::box_set::<Switch>(true),
            ExternalOp::box_set::<BranchA>(10),
            ExternalOp::box_set::<BranchB>(20),
        ],
        vec![ExternalOp::box_set::<Switch>(false)],
        vec![ExternalOp::box_set::<BranchA>(99)],
    ]);
    assert!(
        outcome.dump.contains("branch_out=Some(20)"),
        "after the flip the branch reads B: {}",
        outcome.dump
    );
    let branch_out_changes = outcome
        .changes
        .iter()
        .flatten()
        .filter(|change| change.contains("tests::BranchOut"))
        .count();
    assert_eq!(
        branch_out_changes, 2,
        "BranchOut changed exactly twice (10, then 20), never from A's edit: {:?}",
        outcome.changes
    );
    assert_eq!(
        outcome.changes[2],
        vec!["plingo::reactive::tests::BranchA Value: Some(10) -> Some(99)".to_string()],
        "the last command's only change is A's edit: {:?}",
        outcome.changes
    );
}

#[test]
fn map_discovery_is_exact() {
    // Matrix 3: insert/update/remove/rekey create, retain, and retire
    // exactly the corresponding child visitor.
    let outcome = crate::reactive::tests::with_counters(|| {
        run_scenario(1, &[
            vec![ExternalOp::map_set::<Table>(5, 1)],
            vec![ExternalOp::map_set::<Table>(5, 2)],  // entry update
            vec![ExternalOp::map_set::<Table>(6, 9)],  // new key
            vec![ExternalOp::map_remove::<Table>(5)],  // retirement
            vec![ExternalOp::map_rekey::<Table>(6, 7)], // rank retained
        ])
    });
    assert!(
        outcome.dump.contains("result=[7->Some(10)]"),
        "value and rank retained across rekey: {}",
        outcome.dump
    );
    assert!(outcome.dump.contains("table=[7->Some(9)]"), "{}", outcome.dump);
    // Discovery (the root's keys read) ran for cold start, the two key
    // additions, the removal, and the rekey: four runs.
    assert_eq!(
        DISCOVERY_RUNS.load(Ordering::SeqCst),
        4,
        "discovery runs only when the key set changes"
    );
    // Entry children ran: cmd1 insert (1), cmd2 update (2), cmd3 both
    // children (4), cmd4 the doomed child(5) plus child(6) (6), cmd5 the
    // doomed child(6) plus child(7) (8).
    assert_eq!(CHILD_RUNS.load(Ordering::SeqCst), 8);
}

#[test]
fn nested_traversal_updates_exactly_once() {
    // Matrix 4: a changed tree child does not visit siblings;
    // child-sequence change updates discovery exactly once.
    let root = NodeId(0);
    let setup = vec![
        ExternalOp::tree_insert_node::<SourceTree>(root, 1),
        ExternalOp::tree_insert_node::<SourceTree>(NodeId(1), 2),
        ExternalOp::tree_insert_node::<SourceTree>(NodeId(2), 3),
        ExternalOp::tree_move_node::<SourceTree>(NodeId(1), root),
        ExternalOp::tree_move_node::<SourceTree>(NodeId(2), root),
    ];
    let outcome = crate::reactive::tests::with_counters(|| run_scenario(1, &[setup.clone()]));
    assert!(
        outcome.dump.contains("result_tree=root(NodeId(0)=Some(1),kids=[NodeId(1), NodeId(2)])"),
        "mirror: {}",
        outcome.dump
    );
    assert_eq!(MIRROR_RUNS.load(Ordering::SeqCst), 3, "root + two children ran at cold start");

    // Update one child: only its mirror visitor re-runs.
    let outcome = crate::reactive::tests::with_counters(|| {
        run_scenario(1, &[setup.clone(), vec![ExternalOp::tree_update_node::<SourceTree>(NodeId(1), 20)]])
    });
    assert!(
        outcome
            .dump
            .contains("result_tree=root(NodeId(0)=Some(1),kids=[NodeId(1), NodeId(2)])"),
        "the mirror preserves the tree: {}",
        outcome.dump
    );
    assert_eq!(
        MIRROR_RUNS.load(Ordering::SeqCst),
        4,
        "only the changed child's visitor re-ran"
    );

    // Reorder: the root's discovery re-runs once and re-executes both
    // children; the result order follows.
    let outcome = crate::reactive::tests::with_counters(|| {
        run_scenario(1, &[
            setup.clone(),
            vec![ExternalOp::tree_reorder_children::<SourceTree>(root, vec![NodeId(2), NodeId(1)])],
        ])
    });
    assert!(
        outcome
            .dump
            .contains("result_tree=root(NodeId(0)=Some(1),kids=[NodeId(2), NodeId(1)])"),
        "the new order is mirrored: {}",
        outcome.dump
    );
    assert_eq!(
        MIRROR_RUNS.load(Ordering::SeqCst),
        6,
        "discovery re-ran once and re-executed both children"
    );
}

#[test]
fn unchanged_facts_keep_arc_revision_and_identity() {
    // Matrix 13: equal inputs are no-ops; changed facts get a new Arc and
    // a bumped revision; the causal identity (fact ordinal) is retained.
    let mut engine = build_engine(1, false).expect("engine");
    engine.command(vec![ExternalOp::box_set::<Source>(5)]).expect("1");
    let after_first = engine.snapshot().box_view::<Output>().get().expect("output");
    let rev_first = engine.debug_revision_of::<Output>(&crate::reactive::view::BoxFactKey::Value);

    engine.command(vec![ExternalOp::box_set::<Source>(7)]).expect("2");
    let after_second = engine.snapshot().box_view::<Output>().get().expect("output");
    assert!(!Arc::ptr_eq(&after_first, &after_second), "a changed fact gets a new Arc");

    let report = engine.command(vec![ExternalOp::box_set::<Source>(7)]).expect("3 equal");
    assert_eq!(report.rounds, 0, "equal final state ⇒ no epoch work");
    let after_equal = engine.snapshot().box_view::<Output>().get().expect("output");
    assert!(Arc::ptr_eq(&after_second, &after_equal), "the equal run retains the Arc");
    let rev_equal = engine.debug_revision_of::<Output>(&crate::reactive::view::BoxFactKey::Value);
    assert_eq!(rev_equal, rev_first.map(|r| r + 1), "the revision is retained on equality");

    engine.command(vec![ExternalOp::box_set::<Source>(9)]).expect("4");
    let after_fourth = engine.snapshot().box_view::<Output>().get().expect("output");
    assert!(!Arc::ptr_eq(&after_equal, &after_fourth));
    let rev_fourth = engine.debug_revision_of::<Output>(&crate::reactive::view::BoxFactKey::Value);
    assert_eq!(rev_fourth, rev_first.map(|r| r + 2));
    let _ = after_fourth;
}
