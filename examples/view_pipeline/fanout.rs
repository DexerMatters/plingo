//! A small keyed fan-out stage that observes three map views and emits two
//! maps plus a list. It is intentionally independent of parsing and trees so
//! consumers can test view ownership and exact-key dependencies directly.

use plingo::reactive::kind::{List, Map};
use reactive_macros::view;

#[view]
pub struct Names(Map<String, String>);

#[view]
pub struct Quantities(Map<String, i64>);

#[view]
pub struct Enabled(Map<String, bool>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Record {
    pub name: String,
    pub quantity: Option<i64>,
    pub enabled: bool,
}

#[view]
pub struct Records(Map<String, Record>);

#[view]
pub struct Scores(Map<String, i64>);

#[view]
pub struct Alerts(List<String, String>);


// ---------------------------------------------------------------------------
// Semantic digest (follow-up plan §4 item 1): complete public-view content,
// ID-erased and canonically ordered.
// ---------------------------------------------------------------------------

use plingo::reactive::digest::SemanticDigest;

fn render_record(record: &Record) -> String {
    let quantity = record
        .quantity
        .map(|value| format!("some({value})"))
        .unwrap_or_else(|| "none".to_owned());
    format!(
        "record{{name:{:?},quantity:{quantity},enabled:{}}}",
        record.name, record.enabled
    )
}

/// Captures every present entry of every public view of this family.
pub fn semantic_digest(snapshot: &plingo::reactive::Snapshot) -> SemanticDigest {
    use plingo::reactive::kind::ListKey;
    let mut digest = SemanticDigest::new();

    let mut names = snapshot.inputs::<Names>();
    names.sort();
    for key in names {
        let value = snapshot
            .observe::<Names>(key.clone())
            .map(|value| (*value).clone())
            .unwrap_or_default();
        digest.insert("names", &key, &format!("{value:?}"));
    }

    let mut quantities = snapshot.inputs::<Quantities>();
    quantities.sort();
    for key in quantities {
        let row = match snapshot.observe::<Quantities>(key.clone()).as_deref() {
            Some(value) => format!("some({value})"),
            None => "none".to_owned(),
        };
        digest.insert("quantities", &key, &row);
    }

    let mut enabled = snapshot.inputs::<Enabled>();
    enabled.sort();
    for key in enabled {
        let row = match snapshot.observe::<Enabled>(key.clone()).as_deref() {
            Some(value) => format!("{value}"),
            None => "missing".to_owned(),
        };
        digest.insert("enabled", &key, &row);
    }

    let mut records = snapshot.inputs::<Records>();
    records.sort();
    for key in records {
        let row = snapshot
            .observe::<Records>(key.clone())
            .as_deref()
            .map(render_record)
            .unwrap_or_else(|| "absent".to_owned());
        digest.insert("records", &key, &row);
    }

    let mut scores = snapshot.inputs::<Scores>();
    scores.sort();
    for key in scores {
        let row = match snapshot.observe::<Scores>(key.clone()).as_deref() {
            Some(value) => format!("{value}"),
            None => "absent".to_owned(),
        };
        digest.insert("scores", &key, &row);
    }

    let mut alert_keys: Vec<String> = snapshot
        .inputs::<Alerts>()
        .into_iter()
        .filter_map(|input| match input {
            ListKey::Slot(key, _) => Some(key),
            ListKey::Len(key) => Some(key),
        })
        .collect();
    alert_keys.sort();
    alert_keys.dedup();
    for key in alert_keys {
        let items = snapshot.list::<Alerts>(&key);
        let rendered: Vec<String> = items.iter().map(|item| format!("{item:?}")).collect();
        digest.insert("alerts", &key, &format!("[{}]", rendered.join(",")));
    }
    digest
}
/// Compatibility stage used by the phase-0 reaction oracle. New production
/// callers use the three first-class components in `fanout_components`; this
/// helper keeps the historical single-stage fixture available to the oracle.
pub fn fanout_one(key: String) -> plingo::Result<()> {
    use plingo::reactive::kind::{emit_view, observe_view};

    let name = observe_view::<Names>()?
        .get(&key)?
        .map(|value| (*value).clone())
        .unwrap_or_default();
    let quantity = observe_view::<Quantities>()?.get(&key)?.map(|value| *value);
    let enabled = observe_view::<Enabled>()?
        .get(&key)?
        .map(|value| *value)
        .unwrap_or(false);

    emit_view::<Records>()?.insert(
        key.clone(),
        Record {
            name,
            quantity,
            enabled,
        },
    )?;
    emit_view::<Scores>()?.insert(
        key.clone(),
        if enabled {
            quantity.unwrap_or_default()
        } else {
            0
        },
    )?;
    let alerts = match (enabled, quantity) {
        (true, Some(0)) => vec!["enabled item has no quantity".to_owned()],
        (true, None) => vec!["enabled item is missing a quantity".to_owned()],
        _ => Vec::new(),
    };
    emit_view::<Alerts>()?.replace(&key, alerts)
}
