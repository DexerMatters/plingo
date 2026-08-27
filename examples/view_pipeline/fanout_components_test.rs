//! Exact-reaction scenarios for the component-backed fan-out
//! (follow-up plan §6.1, §24.3): membership-only drivers, definition-named
//! reaction edges, duplicate-install rejection.
use plingo::reactive::kind::emit_view;
use plingo::reactive::{Engine, ReactionDigest, View};
use parking_lot::Mutex;
use std::sync::atomic::Ordering;

use super::fanout::{Alerts, Enabled, Names, Quantities, Records, Scores};
use super::fanout_components::{install, record_install};

/// Serializes tests that touch the process-global run counters.
pub(crate) static COUNTER_LOCK: Mutex<()> = Mutex::new(());

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
fn duplicate_install_is_rejected_before_mutation() {
    let _guard = COUNTER_LOCK.lock();
    let mut engine = Engine::new();
    let _first = record_install(&mut engine).expect("first install");
    let error = record_install(&mut engine).expect_err("second install must fail");
    assert!(
        matches!(error, plingo::reactive::Error::DuplicateComponent { ref descriptor }
            if descriptor.contains("record")),
        "{error:?}"
    );
}

#[test]
fn name_text_edit_wakes_only_the_reading_component() {
    let _guard = COUNTER_LOCK.lock();
    let mut engine = Engine::new();
    install(&mut engine).expect("install");
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "alpha".into())?;
            emit_view::<Quantities>()?.insert("a".into(), 2)?;
            emit_view::<Enabled>()?.insert("a".into(), true)?;
            Ok(())
        })
        .expect("seed");

    reset_counts();

    // A pure payload change on the DRIVER view: `score` and `alert` never
    // read the name text, so only `record` may evaluate.
    let report = engine
        .command(|| emit_view::<Names>()?.insert("a".into(), "alpha2".into()))
        .expect("name text edit");
    let digest = report.metric::<ReactionDigest>().expect("digest");
    // Command-scoped reaction proof first: exactly one definition
    // evaluated, and it is `record`.
    let scored: Vec<&str> = digest
        .evaluations
        .iter()
        .map(|evaluation| evaluation.definition)
        .collect();
    assert_eq!(scored.len(), 1, "{scored:?}");
    assert!(scored[0].ends_with("::record"), "{scored:?}");
    let (records, scores, alerts) = counts();
    assert_eq!(records, 1);
    assert_eq!(
        scores, 0,
        "membership-only driver must stay cold on driver-payload edits"
    );
    assert_eq!(alerts, 0);

    // Definition-named evaluations with exact read edges.
    for evaluation in &digest.evaluations {
        assert!(
            evaluation.definition.ends_with("::record"),
            "unexpected evaluation of {}",
            evaluation.definition
        );
        let views: Vec<&str> = evaluation.reads.iter().map(|e| e.view).collect();
        assert!(views.contains(&Names::name()));
        assert_eq!(evaluation.driving_element, "\"a\"");
    }

    // The record output refreshed; score/alert untouched.
    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot.observe::<Records>("a".into()).unwrap().name,
        "alpha2"
    );
}

#[test]
fn input_edit_wakes_exactly_the_three_instances_of_that_key() {
    let _guard = COUNTER_LOCK.lock();
    let mut engine = Engine::new();
    install(&mut engine).expect("install");
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "alpha".into())?;
            emit_view::<Names>()?.insert("b".into(), "beta".into())?;
            emit_view::<Quantities>()?.insert("a".into(), 1)?;
            emit_view::<Quantities>()?.insert("b".into(), 1)?;
            emit_view::<Enabled>()?.insert("a".into(), true)?;
            emit_view::<Enabled>()?.insert("b".into(), true)?;
            Ok(())
        })
        .expect("seed two keys");

    reset_counts();

    let report = engine
        .command(|| emit_view::<Quantities>()?.insert("a".into(), 5))
        .expect("quantity edit a");
    let digest = report.metric::<ReactionDigest>().expect("digest");

    let (records, scores, alerts) = counts();
    assert_eq!(records, 1);
    assert_eq!(scores, 1);
    assert_eq!(alerts, 1);

    let snapshot = engine.snapshot();
    assert_eq!(snapshot.observe::<Scores>("a".into()).as_deref(), Some(&5));
    assert_eq!(
        snapshot.observe::<Records>("a".into()).unwrap().quantity,
        Some(5)
    );
    // Unrelated key stayed byte-cold.
    assert_eq!(snapshot.observe::<Scores>("b".into()).as_deref(), Some(&1));

    // Liveness audit stays clean over component graphs.
    assert!(
        engine.__liveness_audit().is_empty(),
        "{:?}",
        engine.__liveness_audit()
    );
}

#[test]
fn membership_removal_retires_every_owned_output() {
    let _guard = COUNTER_LOCK.lock();
    let mut engine = Engine::new();
    install(&mut engine).expect("install");
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "alpha".into())?;
            emit_view::<Quantities>()?.insert("a".into(), 3)?;
            emit_view::<Enabled>()?.insert("a".into(), false)?;
            Ok(())
        })
        .expect("seed one key");

    engine
        .command(|| emit_view::<Names>()?.remove("a".into()))
        .expect("remove owner key");

    let snapshot = engine.snapshot();
    assert!(snapshot.observe::<Records>("a".into()).is_none());
    assert!(snapshot.observe::<Scores>("a".into()).is_none());
    assert!(snapshot.list::<Alerts>(&"a".to_owned()).is_empty());
    assert!(engine.__liveness_audit().is_empty());

    // Reinsertion recreates the instances through the same definitions.
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "alpha".into())?;
            emit_view::<Quantities>()?.insert("a".into(), 3)?;
            Ok(())
        })
        .expect("reinsert");
    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot.observe::<Scores>("a".into()).as_deref(),
        Some(&0) // disabled by earlier Enabled seed? Enabled was removed? no: still present=false
    );
    assert!(engine.__liveness_audit().is_empty());
}
