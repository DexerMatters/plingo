//! Cut D runtime proofs (follow-up plan §6.2 item 13 / §16.8): one
//! component instance owns a large patch-key domain and touches one key;
//! journal candidate work is independent of the owned domain.

use crate::reactive::kind::{emit_patch, emit_view, observe_view};
use crate::reactive::prelude::*;
use crate::reactive::{Engine, Error};
use reactive_macros::{component, view};

#[view]
struct Trigger(Map<(), u64>);

#[view]
struct BigOwned(Map<u64, u64>);

const DOMAIN: u64 = 20_000;

thread_local! {
    static SEEDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static POISON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[component]
fn owner(_key: EachKey<Trigger>) -> Result<()> {
    // Read the driver FIRST so every evaluation records an exact dependency
    // on the trigger element (membership alone schedules; payload changes
    // must wake through this row).
    let step = observe_view::<Trigger>()?
        .get(&())?
        .map(|value| *value)
        .unwrap_or(0);
    if !SEEDED.with(|flag| flag.get()) {
        let patch = emit_patch::<BigOwned>()?;
        for index in 0..DOMAIN {
            patch.upsert(index, index)?;
        }
        SEEDED.with(|flag| flag.set(true));
        return Ok(());
    }
    if POISON.with(|flag| flag.get()) {
        // Touch first (ownership mutates), then abort the evaluation: the
        // command must roll the staged writes back.
        let patch = emit_patch::<BigOwned>()?;
        patch.upsert(0, 999_999)?;
        POISON.with(|flag| flag.set(false));
        return Err(Error::Internal("cut_d injected failure".into()));
    }
    let index = step.wrapping_mul(7919) % DOMAIN;
    let patch = emit_patch::<BigOwned>()?;
    patch.upsert(index, 1_000_000 + step)
}

fn fact_writes_of(report: &crate::reactive::CommandReport) -> u64 {
    report.engine_work().fact_writes
}

#[test]
fn touching_one_key_journals_bounded_candidates_over_a_large_domain() {
    let mut engine = Engine::new();
    owner_install(&mut engine).expect("install owner");

    // Seed: the instance owns DOMAIN keys in one evaluation.
    engine
        .command(|| emit_view::<Trigger>()?.insert((), 0))
        .expect("seed trigger");
    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot.inputs::<BigOwned>().len() as u64,
        DOMAIN,
        "the whole owned domain must be live"
    );

    // Warm re-evaluation with no authored ops: zero journal candidates
    // beyond the trigger write itself.
    let before = {
        let report = engine
            .command(|| emit_view::<Trigger>()?.insert((), 1))
            .expect("no-op retouch");
        fact_writes_of(&report)
    };

    // Touch exactly one key.
    let report = engine
        .command(|| emit_view::<Trigger>()?.insert((), 2))
        .expect("touch one");
    let touched_writes = fact_writes_of(&report);

    assert!(
        touched_writes <= 4,
        "one-key touch must journal O(touched) candidates, got {touched_writes} (no-op baseline {before})"
    );

    // Retained ownership: every key still present, only index(2*7919%N)
    // changed value.
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.inputs::<BigOwned>().len() as u64, DOMAIN);
    let changed = (2u64.wrapping_mul(7919)) % DOMAIN;
    assert_eq!(
        snapshot.observe::<BigOwned>(changed).as_deref(),
        Some(&(1_000_000 + 2))
    );
    assert_eq!(snapshot.observe::<BigOwned>(7).as_deref(), Some(&7));

    // Liveness audit stays clean over the big owned set.
    let violations = engine.__liveness_audit();
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn failed_touch_rolls_the_owned_domain_back() {
    let mut engine = Engine::new();
    owner_install(&mut engine).expect("install owner");
    engine
        .command(|| emit_view::<Trigger>()?.insert((), 0))
        .expect("seed");
    let before = engine.snapshot();

    POISON.with(|flag| flag.set(true));
    let error = engine
        .command(|| emit_view::<Trigger>()?.insert((), 3))
        .expect_err("poisoned command must fail");
    assert!(matches!(error, Error::Internal(_)), "{error:?}");

    let after = engine.snapshot();
    assert_eq!(after.live_fact_count(), before.live_fact_count());
    assert_eq!(
        after.observe::<BigOwned>(0).as_deref(),
        Some(&0),
        "the poisoned upsert must not survive"
    );
    assert!(
        engine.__liveness_audit().is_empty(),
        "{:?}",
        engine.__liveness_audit()
    );
}
