//! Exact-reaction scenarios for the semantic keyed fan-out components.

use parking_lot::Mutex;
use plingo::prelude::*;
use plingo::reactive::ReactionDigest;
use std::sync::atomic::Ordering;

use super::fanout::{Alerts, Enabled, Names, Quantities, Records, Scores};
use super::fanout_components::{alert, record, score};

/// Serializes tests that touch the process-global run counters.
pub(crate) static COUNTER_LOCK: Mutex<()> = Mutex::new(());

fn install() -> Workspace {
    Workspace::builder()
        .mount::<record::Component, _>(Names::entries())
        .mount::<score::Component, _>(Names::entries())
        .mount::<alert::Component, _>(Names::entries())
        .build()
        .expect("workspace builds")
}

fn set_all(engine: &mut Engine, key: &str, name: &str, quantity: i64, enabled: bool) {
    engine
        .command(|| {
            (
                Names::set(key.to_owned(), name.to_owned()),
                Quantities::set(key.to_owned(), quantity),
                Enabled::set(key.to_owned(), enabled),
            )
                .__apply()
        })
        .expect("seed");
}

fn counts() -> (usize, usize, usize) {
    (
        super::fanout_components::RECORD_RUNS.load(Ordering::SeqCst),
        super::fanout_components::SCORE_RUNS.load(Ordering::SeqCst),
        super::fanout_components::ALERT_RUNS.load(Ordering::SeqCst),
    )
}

fn reset_counts() {
    super::fanout_components::RECORD_RUNS.store(0, Ordering::SeqCst);
    super::fanout_components::SCORE_RUNS.store(0, Ordering::SeqCst);
    super::fanout_components::ALERT_RUNS.store(0, Ordering::SeqCst);
}

#[test]
fn duplicate_mount_is_rejected_before_mutation() {
    let _guard = COUNTER_LOCK.lock();
    let result = Workspace::builder()
        .mount::<record::Component, _>(Names::entries())
        .mount::<record::Component, _>(Names::entries())
        .build();
    let error = result.expect_err("second mount must fail");
    assert!(
        matches!(error, plingo::reactive::Error::DuplicateComponent { ref descriptor }
            if descriptor.contains("record")),
        "{error:?}"
    );
}

#[test]
fn name_text_edit_wakes_only_the_reading_component() {
    let _guard = COUNTER_LOCK.lock();
    let mut workspace = install();
    let engine = workspace.engine_mut();
    set_all(engine, "a", "alpha", 2, true);

    reset_counts();
    let report = engine
        .command(|| Names::set("a".into(), "alpha2".into()).__apply())
        .expect("name text edit");
    let digest = report.metric::<ReactionDigest>().expect("digest");
    let scored: Vec<&str> = digest
        .evaluations
        .iter()
        .map(|evaluation| evaluation.definition)
        .collect();
    assert_eq!(scored.len(), 1, "{scored:?}");
    assert!(scored[0].ends_with("::record"), "{scored:?}");
    let (records, scores, alerts) = counts();
    assert_eq!(records, 1);
    assert_eq!(scores, 0, "membership-only component stayed cold");
    assert_eq!(alerts, 0);
    assert!(
        digest.evaluations[0]
            .reads
            .iter()
            .any(|read| read.view == Names::name())
    );
    assert_eq!(digest.evaluations[0].driving_element, "\"a\"");
    assert_eq!(
        engine
            .snapshot()
            .observe::<Records>("a".into())
            .unwrap()
            .name,
        "alpha2"
    );
}

#[test]
fn input_edit_wakes_exactly_the_three_instances_of_that_key() {
    let _guard = COUNTER_LOCK.lock();
    let mut workspace = install();
    let engine = workspace.engine_mut();
    set_all(engine, "a", "alpha", 1, true);
    set_all(engine, "b", "beta", 1, true);

    reset_counts();
    let report = engine
        .command(|| Quantities::set("a".into(), 5).__apply())
        .expect("quantity edit");
    let (records, scores, alerts) = counts();
    assert_eq!((records, scores, alerts), (1, 1, 1));
    assert_eq!(
        report.metric::<ReactionDigest>().unwrap().evaluations.len(),
        3
    );
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.observe::<Scores>("a".into()).as_deref(), Some(&5));
    assert_eq!(
        snapshot.observe::<Records>("a".into()).unwrap().quantity,
        Some(5)
    );
    assert_eq!(snapshot.observe::<Scores>("b".into()).as_deref(), Some(&1));
    assert!(engine.__liveness_audit().is_empty());
}

#[test]
fn membership_removal_retires_every_owned_output() {
    let _guard = COUNTER_LOCK.lock();
    let mut workspace = install();
    let engine = workspace.engine_mut();
    set_all(engine, "a", "alpha", 3, false);

    engine
        .command(|| Names::remove("a".into()).__apply())
        .expect("remove owner key");
    let snapshot = engine.snapshot();
    assert!(snapshot.observe::<Records>("a".into()).is_none());
    assert!(snapshot.observe::<Scores>("a".into()).is_none());
    assert!(snapshot.list::<Alerts>(&"a".to_owned()).is_empty());
    assert!(engine.__liveness_audit().is_empty());

    set_all(engine, "a", "alpha", 3, false);
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.observe::<Scores>("a".into()).as_deref(), Some(&0));
    assert!(engine.__liveness_audit().is_empty());
}
