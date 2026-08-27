//! Fan-out rewritten as first-class components (follow-up plan §6.1, §24.3).
//!
//! Three definitions share ONLY key-membership lifecycle through an
//! `EachKey<Names>` driver: `record` reads the name text, `score` and
//! `alert` do not. Identity is `(definition marker, exact key)`; a payload
//! update reruns an instance only when its body records that read.

use plingo::reactive::component::{EachKey, Read, Write};
use plingo::reactive::prelude::*;
use reactive_macros::component;

use super::fanout::{Alerts, Enabled, Names, Quantities, Record, Records, Scores};

/// Evaluations per definition, for exact-reaction assertions.
pub static RECORD_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static SCORE_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static ALERT_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[component]
pub fn record(
    key: EachKey<Names>,
    names: Read<Names>,
    quantities: Read<Quantities>,
    enabled: Read<Enabled>,
    records: Write<Records>,
) -> Result<()> {
    RECORD_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let name = names
        .get(&key)?
        .map(|value| (*value).clone())
        .unwrap_or_default();
    let quantity = quantities.get(&key)?.map(|value| *value);
    let enabled = enabled.get(&key)?.map(|value| *value).unwrap_or(false);
    records.insert(
        key,
        Record {
            name,
            quantity,
            enabled,
        },
    )
}

#[component]
pub fn score(
    key: EachKey<Names>,
    quantities: Read<Quantities>,
    enabled: Read<Enabled>,
    scores: Write<Scores>,
) -> Result<()> {
    SCORE_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // Membership-only over `Names`: the driver's payload is never read
    // here, so a name-text edit must not wake this instance.
    let _never_read_names = &key;
    let quantity = quantities.get(&key)?.map(|value| *value);
    let enabled = enabled.get(&key)?.map(|value| *value).unwrap_or(false);
    let value = if enabled {
        quantity.unwrap_or_default()
    } else {
        0
    };
    scores.insert(key, value)
}

#[component]
pub fn alert(
    key: EachKey<Names>,
    quantities: Read<Quantities>,
    enabled: Read<Enabled>,
    alerts: Write<Alerts>,
) -> Result<()> {
    ALERT_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let quantity = quantities.get(&key)?.map(|value| *value);
    let enabled = enabled.get(&key)?.map(|value| *value).unwrap_or(false);
    let items = match (enabled, quantity) {
        (true, Some(0)) => vec!["enabled item has no quantity".to_owned()],
        (true, None) => vec!["enabled item is missing a quantity".to_owned()],
        _ => Vec::new(),
    };
    alerts_replace(&key, items)
}

fn alerts_replace(key: &str, items: Vec<String>) -> Result<()> {
    emit_view::<Alerts>()?.replace(&key.to_owned(), items)
}

/// Installs the three fan-out definitions in dependency-safe order.
pub fn install(engine: &mut plingo::reactive::Engine) -> plingo::Result<()> {
    record_install(engine)?;
    score_install(engine)?;
    alert_install(engine)?;
    Ok(())
}
