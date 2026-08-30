//! Keyed fan-out components using semantic inputs and returned effects.
//!
//! Membership is driven by `Names`, while each body records only the payload
//! reads it needs. Returned effects own the derived rows and retract them when
//! an evaluation omits a value.

use plingo::prelude::*;

use super::fanout::{Alerts, Enabled, Names, Quantities, Record, Records, Scores};

/// Evaluations per definition, for exact-reaction assertions.
pub static RECORD_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static SCORE_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static ALERT_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[component]
pub fn record(key: Each<Names>) -> Result<Set<Records>> {
    RECORD_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let name = Names::get(key.key())?
        .map(|value| (*value).clone())
        .unwrap_or_default();
    let quantity = Quantities::get(key.key())?.map(|value| *value);
    let enabled = Enabled::get(key.key())?
        .map(|value| *value)
        .unwrap_or(false);
    let key = key.into_key();
    Ok(Records::set(
        key,
        Record {
            name,
            quantity,
            enabled,
        },
    ))
}

#[component]
pub fn score(key: Each<Names>) -> Result<Set<Scores>> {
    SCORE_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // The membership key is intentionally not read through `Names::get`.
    let quantity = Quantities::get(key.key())?.map(|value| *value);
    let enabled = Enabled::get(key.key())?
        .map(|value| *value)
        .unwrap_or(false);
    let value = if enabled {
        quantity.unwrap_or_default()
    } else {
        0
    };
    Ok(Scores::set(key.into_key(), value))
}

#[component]
pub fn alert(key: Each<Names>) -> Result<Replace<Alerts>> {
    ALERT_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let quantity = Quantities::get(key.key())?.map(|value| *value);
    let enabled = Enabled::get(key.key())?
        .map(|value| *value)
        .unwrap_or(false);
    let items = match (enabled, quantity) {
        (true, Some(0)) => vec!["enabled item has no quantity".to_owned()],
        (true, None) => vec!["enabled item is missing a quantity".to_owned()],
        _ => Vec::new(),
    };
    Ok(Alerts::replace(key.into_key(), items))
}
