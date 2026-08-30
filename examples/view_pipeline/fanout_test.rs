//! Exact-key dependency scenarios for the map/list fan-out example.

use plingo::prelude::*;
use plingo::reactive::Snapshot;

use super::fanout::{Alerts, Enabled, Names, Quantities, Record, Records, Scores};
use super::fanout_components::{alert, record as record_component, score};

fn install() -> Workspace {
    Workspace::builder()
        .mount::<record_component::Component, _>(Names::entries())
        .mount::<score::Component, _>(Names::entries())
        .mount::<alert::Component, _>(Names::entries())
        .build()
        .expect("workspace builds")
}

fn record(snapshot: &Snapshot, key: &str) -> Option<Record> {
    snapshot
        .observe::<Records>(key.to_owned())
        .map(|record| (*record).clone())
}

fn set_all(engine: &mut Engine, key: &str, name: &str, quantity: Option<i64>, enabled: bool) {
    engine
        .command(|| {
            (
                Names::set(key.to_owned(), name.to_owned()),
                Enabled::set(key.to_owned(), enabled),
            )
                .__apply()?;
            match quantity {
                Some(value) => Quantities::set(key.to_owned(), value).__apply(),
                None => Quantities::remove(key.to_owned()).__apply(),
            }
        })
        .expect("set inputs");
}

#[test]
fn observing_three_maps_emits_two_maps_and_one_list_per_key() {
    let _counter_guard = super::fanout_components_test::COUNTER_LOCK.lock();
    let mut workspace = install();
    let engine = workspace.engine_mut();
    set_all(engine, "a", "alpha", Some(4), true);

    let snapshot = engine.snapshot();
    assert_eq!(
        record(&snapshot, "a"),
        Some(Record {
            name: "alpha".into(),
            quantity: Some(4),
            enabled: true
        })
    );
    assert_eq!(snapshot.observe::<Scores>("a".into()).as_deref(), Some(&4));
    assert!(snapshot.list::<Alerts>(&"a".to_owned()).is_empty());
}

#[test]
fn one_key_update_does_not_change_another_keys_fanout_outputs() {
    let _counter_guard = super::fanout_components_test::COUNTER_LOCK.lock();
    let mut workspace = install();
    let engine = workspace.engine_mut();
    set_all(engine, "a", "alpha", Some(3), true);
    set_all(engine, "b", "beta", Some(8), true);
    let before = engine.snapshot();
    let b_record = record(&before, "b");
    let b_score = before.observe::<Scores>("b".into());

    engine
        .command(|| Quantities::set("a".into(), 0).__apply())
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
    let mut workspace = install();
    let engine = workspace.engine_mut();
    set_all(engine, "a", "alpha", None, true);
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
        .command(|| Enabled::set("a".into(), false).__apply())
        .expect("disable item");
    let disabled = engine.snapshot();
    assert_eq!(disabled.observe::<Scores>("a".into()).as_deref(), Some(&0));
    assert!(disabled.list::<Alerts>(&"a".to_owned()).is_empty());

    engine
        .command(|| Names::remove("a".into()).__apply())
        .expect("remove owner key");
    let removed = engine.snapshot();
    assert!(record(&removed, "a").is_none());
    assert!(removed.observe::<Scores>("a".into()).is_none());
    assert!(removed.list::<Alerts>(&"a".to_owned()).is_empty());
}

use super::fanout::semantic_digest;
use plingo::reactive::digest::{FamilyState, render_diff};

fn seed_standard(engine: &mut Engine) {
    engine
        .command(|| {
            (
                Names::set("a".into(), "alpha".into()),
                Names::set("b".into(), "beta".into()),
                Quantities::set("a".into(), 2),
                Enabled::set("a".into(), true),
                Enabled::set("b".into(), false),
            )
                .__apply()
        })
        .expect("seed standard membership");
}

fn state_of(engine: &Engine) -> FamilyState {
    let snapshot = engine.snapshot();
    FamilyState::capture(semantic_digest(&snapshot), &snapshot)
}

#[test]
fn canonical_fixture_matches_hand_authored_rows() {
    let _counter_guard = super::fanout_components_test::COUNTER_LOCK.lock();
    let mut workspace = install();
    let engine = workspace.engine_mut();
    seed_standard(engine);
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

#[test]
fn reversible_edit_matrix_restores_exact_state() {
    let _counter_guard = super::fanout_components_test::COUNTER_LOCK.lock();
    let mut workspace = install();
    let engine = workspace.engine_mut();
    seed_standard(engine);
    let initial = state_of(&engine);

    engine
        .command(|| Quantities::set("a".into(), 5).__apply())
        .expect("quantity edit");
    let after_quantity = state_of(&engine);
    assert_eq!(
        after_quantity
            .digest
            .rows_of("quantities")
            .iter()
            .find(|(key, _)| *key == "quantities::a")
            .map(|(_, value)| *value),
        Some("some(5)")
    );
    assert!(
        render_diff(&initial.digest, &after_quantity.digest)
            .matches("scores::")
            .count()
            <= 1
    );

    engine
        .command(|| Names::set("a".into(), "alpha2".into()).__apply())
        .expect("name text edit");
    let after_name = state_of(&engine);
    let diff_name = render_diff(&after_quantity.digest, &after_name.digest);
    assert!(diff_name.contains("names::a"));
    assert!(diff_name.contains("records::a"));
    assert!(!diff_name.contains("records::b"));
    assert!(!diff_name.contains("scores::"));

    engine
        .command(|| Enabled::set("b".into(), true).__apply())
        .expect("enable b");
    let after_enable = state_of(&engine);
    let diff_enable = render_diff(&after_name.digest, &after_enable.digest);
    assert!(diff_enable.contains("enabled::b"));
    assert!(diff_enable.contains("alerts::b"));
    assert!(diff_enable.contains("records::b"));
    assert!(!diff_enable.contains("scores::"));
    assert!(!diff_enable.contains("::a"));

    engine
        .command(|| Enabled::set("b".into(), false).__apply())
        .expect("reverse enable");
    engine
        .command(|| Names::set("a".into(), "alpha".into()).__apply())
        .expect("reverse name");
    engine
        .command(|| Quantities::set("a".into(), 2).__apply())
        .expect("reverse quantity");
    let restored = state_of(&engine);
    assert_eq!(restored.digest, initial.digest, "reverse digest mismatch");
    assert_eq!(restored.live_facts, initial.live_facts);

    let mut cold_workspace = install();
    let cold = cold_workspace.engine_mut();
    seed_standard(cold);
    assert_eq!(
        restored.digest,
        state_of(&cold).digest,
        "warm/cold mismatch"
    );
}
