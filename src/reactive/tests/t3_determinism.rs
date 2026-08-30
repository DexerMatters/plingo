//! T3 — worker choice does not affect committed values or reports.

use crate::reactive::api::{run, run_each_child, run_each_child_of, run_each_key};
use crate::reactive::kind::{Map, emit_view, observe_view};
use crate::reactive::prelude::*;
use crate::view;

#[view]
struct T3Source(Map<u64, i64>);

#[view]
struct T3Total(Map<(), i64>);

fn total(_: ()) -> Result<i64> {
    let mut sum = 0;
    for input in observe_view::<T3Source>()?.keys()? {
        sum += observe_view::<T3Source>()?
            .get(&input)?
            .map(|value| *value)
            .unwrap_or_default();
    }
    emit_view::<T3Total>()?.insert((), sum)?;
    Ok(sum)
}

fn run_trace() -> (i64, u64, u32, usize) {
    let mut engine = Engine::new();
    let plan = engine.plan(total, ()).expect("plan");
    let running = engine.run(&plan).expect("run");
    let report = engine
        .command(|| {
            for (input, value) in [(3, 8), (1, 2), (2, 4)] {
                emit_view::<T3Source>()?.insert(input, value)?;
            }
            Ok(())
        })
        .expect("source command");
    (
        *running.output(),
        report.epoch,
        report.rounds,
        report.changed::<T3Total>(),
    )
}

#[test]
fn repeated_identical_traces_produce_identical_reports() {
    // Determinism contract after the worker-parameter removal (plan §5.4):
    // repeated identical traces produce byte-identical reports and state.
    let one = run_trace();
    let many = run_trace();
    assert_eq!(one, many);
    assert_eq!(one.0, 14);
    assert_eq!(one.3, 1);
}
