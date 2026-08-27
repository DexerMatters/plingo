//! Exact-key dependency scenarios for the map/list fan-out example.

use plingo::reactive::kind::emit_view;
use plingo::reactive::{Engine, Snapshot};

use super::fanout::{Alerts, Enabled, Names, Quantities, Record, Records, Scores};
use super::fanout_components;

fn install(engine: &mut Engine) {
    fanout_components::install(engine).expect("install fan-out components");
}

fn record(snapshot: &Snapshot, key: &str) -> Option<Record> {
    snapshot
        .observe::<Records>(key.to_owned())
        .map(|record| (*record).clone())
}

#[test]
fn observing_three_maps_emits_two_maps_and_one_list_per_key() {
    let _counter_guard = super::fanout_components_test::COUNTER_LOCK.lock();
    let mut engine = Engine::new();
    install(&mut engine);
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "alpha".into())?;
            emit_view::<Quantities>()?.insert("a".into(), 4)?;
            emit_view::<Enabled>()?.insert("a".into(), true)
        })
        .expect("seed inputs");

    let snapshot = engine.snapshot();
    assert_eq!(
        record(&snapshot, "a"),
        Some(Record {
            name: "alpha".into(),
            quantity: Some(4),
            enabled: true,
        })
    );
    assert_eq!(snapshot.observe::<Scores>("a".into()).as_deref(), Some(&4));
    assert!(snapshot.list::<Alerts>(&"a".to_owned()).is_empty());
}

#[test]
fn one_key_update_does_not_change_another_keys_fanout_outputs() {
    let _counter_guard = super::fanout_components_test::COUNTER_LOCK.lock();
    let mut engine = Engine::new();
    install(&mut engine);
    engine
        .command(|| {
            for (key, name, quantity) in [("a", "alpha", 3), ("b", "beta", 8)] {
                emit_view::<Names>()?.insert(key.into(), name.into())?;
                emit_view::<Quantities>()?.insert(key.into(), quantity)?;
                emit_view::<Enabled>()?.insert(key.into(), true)?;
            }
            Ok(())
        })
        .expect("seed inputs");
    let before = engine.snapshot();
    let b_record = record(&before, "b");
    let b_score = before.observe::<Scores>("b".into());

    engine
        .command(|| emit_view::<Quantities>()?.insert("a".into(), 0))
        .expect("update only A");
    let after = engine.snapshot();
    assert_eq!(record(&after, "a").unwrap().quantity, Some(0));
    assert_eq!(
        after
            .list::<Alerts>(&"a".to_owned())
            .into_iter()
            .map(|alert| (*alert).clone())
            .collect::<Vec<_>>(),
        vec!["enabled item has no quantity".to_owned()]
    );
    assert_eq!(record(&after, "b"), b_record);
    assert_eq!(after.observe::<Scores>("b".into()), b_score);
}

#[test]
fn disabling_or_removing_a_name_updates_only_its_owned_outputs() {
    let _counter_guard = super::fanout_components_test::COUNTER_LOCK.lock();
    let mut engine = Engine::new();
    install(&mut engine);
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "alpha".into())?;
            emit_view::<Enabled>()?.insert("a".into(), true)
        })
        .expect("seed incomplete item");
    assert_eq!(
        engine
            .snapshot()
            .list::<Alerts>(&"a".to_owned())
            .into_iter()
            .map(|alert| (*alert).clone())
            .collect::<Vec<_>>(),
        vec!["enabled item is missing a quantity".to_owned()]
    );

    engine
        .command(|| emit_view::<Enabled>()?.insert("a".into(), false))
        .expect("disable item");
    let disabled = engine.snapshot();
    assert_eq!(disabled.observe::<Scores>("a".into()).as_deref(), Some(&0));
    assert!(disabled.list::<Alerts>(&"a".to_owned()).is_empty());

    engine
        .command(|| emit_view::<Names>()?.remove("a".into()))
        .expect("remove owner key");
    let removed = engine.snapshot();
    assert!(record(&removed, "a").is_none());
    assert!(removed.observe::<Scores>("a".into()).is_none());
    assert!(removed.list::<Alerts>(&"a".to_owned()).is_empty());
}

// ---------------------------------------------------------------------------
// Phase 0 oracles (follow-up plan §4): canonical fixture, cold equivalence,
// and reversible traces with expected keyed deltas.
// ---------------------------------------------------------------------------

use plingo::reactive::digest::{FamilyState, render_diff};

use super::fanout::semantic_digest;

fn seed_standard(engine: &mut Engine) {
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "alpha".into())?;
            emit_view::<Names>()?.insert("b".into(), "beta".into())?;
            emit_view::<Quantities>()?.insert("a".into(), 2)?;
            emit_view::<Enabled>()?.insert("a".into(), true)?;
            emit_view::<Enabled>()?.insert("b".into(), false)?;
            Ok(())
        })
        .expect("seed standard membership");
}

fn state_of(engine: &Engine) -> FamilyState {
    let snapshot = engine.snapshot();
    FamilyState::capture(semantic_digest(&snapshot), &snapshot)
}

/// Canonical empty/single-root fixture: hand-authored complete public-view
/// content (plan §4 item 13). A warm and cold implementation sharing the
/// same extra/orphan output must still fail this.
#[test]
fn canonical_fixture_matches_hand_authored_rows() {
    let _counter_guard = super::fanout_components_test::COUNTER_LOCK.lock();
    let mut engine = Engine::new();
    install(&mut engine);
    seed_standard(&mut engine);
    let digest = semantic_digest(&engine.snapshot());

    let expected: &[(&str, &str, &str)] = &[
        ("alerts", "a", "[]"),
        ("alerts", "b", "[]"),
        ("enabled", "a", "true"),
        ("enabled", "b", "false"),
        ("names", "a", "\"alpha\""),
        ("names", "b", "\"beta\""),
        ("quantities", "a", "some(2)"),
        (
            "records",
            "a",
            "record{name:\"alpha\",quantity:some(2),enabled:true}",
        ),
        (
            "records",
            "b",
            "record{name:\"beta\",quantity:none,enabled:false}",
        ),
        ("scores", "a", "2"),
        ("scores", "b", "0"),
    ];
    // The complete domain: every recorded row matches the hand-authored
    // table and the table covers every recorded row.
    assert_eq!(digest.len(), expected.len(), "{}", digest.render());
    for (view, key, value) in expected {
        let actual = digest
            .rows_of(view)
            .iter()
            .find(|(row_key, _)| row_key.strip_prefix(&format!("{view}::")) == Some(*key))
            .map(|(_, value)| *value);
        assert_eq!(actual.as_deref(), Some(*value), "row {view}::{key}");
    }
}

/// The full reversible edit matrix (plan §4 item 4, fan-out rows): every
/// forward step asserts its exact keyed delta; every reverse restores the
/// initial digest, per-view counts, and live-fact count exactly; a fresh
/// engine replaying the same final inputs matches the warm digest.
#[test]
fn reversible_edit_matrix_restores_exact_state() {
    let _counter_guard = super::fanout_components_test::COUNTER_LOCK.lock();
    let mut engine = Engine::new();
    install(&mut engine);
    seed_standard(&mut engine);
    let initial = state_of(&engine);

    // quantity edit: exactly records.a/scores.a/alerts.a may move.
    engine
        .command(|| emit_view::<Quantities>()?.insert("a".into(), 5))
        .expect("quantity edit");
    let after_quantity = state_of(&engine);
    let quantity_row = after_quantity
        .digest
        .rows_of("quantities")
        .iter()
        .find(|(key, _)| *key == "quantities::a")
        .map(|(_, value)| *value)
        .unwrap_or("absent");
    assert_eq!(quantity_row, "some(5)");
    let changed_scores = render_diff(&initial.digest, &after_quantity.digest)
        .matches("scores::")
        .count();
    assert!(
        changed_scores <= 1,
        "quantity edit must move at most one score row"
    );
    assert_eq!(
        after_quantity.digest.rows_of("scores").len(),
        initial.digest.rows_of("scores").len()
    );
    assert_ne!(
        after_quantity.digest.fingerprint(),
        initial.digest.fingerprint()
    );

    // name text edit touches only names/records of that key.
    engine
        .command(|| emit_view::<Names>()?.insert("a".into(), "alpha2".into()))
        .expect("name text edit");
    let after_name = state_of(&engine);
    let diff_name = render_diff(&after_quantity.digest, &after_name.digest);
    assert!(diff_name.contains("names::a"), "{diff_name}");
    assert!(diff_name.contains("records::a"), "{diff_name}");
    assert!(!diff_name.contains("records::b"), "{diff_name}");
    assert!(!diff_name.contains("scores::"), "{diff_name}");

    // Enabled toggle: score stays 0 (absent quantity defaults to 0 in both
    // states), the missing-quantity alert appears, records refresh — all
    // confined to key b.
    engine
        .command(|| emit_view::<Enabled>()?.insert("b".into(), true))
        .expect("enable b");
    let after_enable = state_of(&engine);
    let diff_enable = render_diff(&after_name.digest, &after_enable.digest);
    assert!(diff_enable.contains("enabled::b"), "{diff_enable}");
    assert!(diff_enable.contains("alerts::b"), "{diff_enable}");
    assert!(diff_enable.contains("records::b"), "{diff_enable}");
    assert!(!diff_enable.contains("scores::"), "{diff_enable}");
    assert!(!diff_enable.contains("::a"), "{diff_enable}");

    // Reverse everything back to the baseline membership.
    engine
        .command(|| emit_view::<Enabled>()?.insert("b".into(), false))
        .expect("reverse enable");
    engine
        .command(|| emit_view::<Names>()?.insert("a".into(), "alpha".into()))
        .expect("reverse name");
    engine
        .command(|| emit_view::<Quantities>()?.insert("a".into(), 2))
        .expect("reverse quantity");
    let restored = state_of(&engine);
    assert_eq!(
        restored.digest,
        initial.digest,
        "reverse digest mismatch:\n{}",
        render_diff(&initial.digest, &restored.digest)
    );
    assert_eq!(restored.live_facts, initial.live_facts);

    // Cold oracle: a fresh engine with the identical final membership.
    let mut cold = Engine::new();
    install(&mut cold);
    seed_standard(&mut cold);
    let cold_state = state_of(&cold);
    assert_eq!(
        restored.digest,
        cold_state.digest,
        "warm/cold mismatch:\n{}",
        render_diff(&restored.digest, &cold_state.digest)
    );
}

/// Key insertion/removal plus the missing-optional-input trace: absence is
/// an exact dependency, removal retracts the whole owned set once, and
/// reinsertion restores the exact initial graph.
#[test]
fn optional_input_and_membership_traces_are_reversible() {
    let _counter_guard = super::fanout_components_test::COUNTER_LOCK.lock();
    let mut engine = Engine::new();
    install(&mut engine);
    seed_standard(&mut engine);
    // Give b the missing optional quantity, then remove it again.
    let baseline = state_of(&engine);
    engine
        .command(|| emit_view::<Quantities>()?.insert("b".into(), 7))
        .expect("insert optional");
    let with_optional = state_of(&engine);
    let diff = render_diff(&baseline.digest, &with_optional.digest);
    assert!(diff.contains("quantities::b"), "{diff}");
    assert!(diff.contains("records::b"), "{diff}");
    assert!(!diff.contains("::a"), "{diff}");
    engine
        .command(|| emit_view::<Quantities>()?.remove("b".into()))
        .expect("remove optional");
    let restored = state_of(&engine);
    assert_eq!(restored.digest, baseline.digest);
    assert_eq!(restored.live_facts, baseline.live_facts);

    // Owner-key removal retires all three outputs; unrelated keys stay cold.
    engine
        .command(|| emit_view::<Names>()?.remove("b".into()))
        .expect("remove owner b");
    let without_b = state_of(&engine);
    let removal_diff = render_diff(&baseline.digest, &without_b.digest);
    for leaked in ["::a"] {
        assert!(!removal_diff.contains(leaked), "{removal_diff}");
    }
    assert!(removal_diff.contains("names::b"));
    assert!(removal_diff.contains("records::b"));
    assert!(removal_diff.contains("scores::b"));

    // Reinsertion restores the exact pre-removal state (same logical node).
    engine
        .command(|| {
            emit_view::<Names>()?.insert("b".into(), "beta".into())?;
            emit_view::<Enabled>()?.insert("b".into(), false)?;
            Ok(())
        })
        .expect("reinsert owner b");
    let reopened = state_of(&engine);
    assert_eq!(
        reopened.digest,
        baseline.digest,
        "reopen mismatch:\n{}",
        render_diff(&baseline.digest, &reopened.digest)
    );
    assert_eq!(reopened.live_facts, baseline.live_facts);
}
