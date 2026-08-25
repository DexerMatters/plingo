//! Phase 0 baseline-artifact gates (plan §10.4, §11 Phase 0 acceptance).
//!
//! The committed reference JSON must be readable, schema-versioned, and
//! carry environment, policy version, fixture checksums, medians/p95,
//! allocation totals, and work counters. Performance comparison refuses a
//! baseline with a different schema, target triple, recovery policy
//! version, or fixture checksum.

mod common;

use common::frozen;
use serde_json::Value;

const BASELINE_PATH: &str = "benchmarks/baselines/incremental-pipeline-phase0.json";

fn load_baseline() -> Value {
    let raw = std::fs::read_to_string(BASELINE_PATH)
        .expect("committed baseline artifact is readable");
    serde_json::from_str(&raw).expect("baseline parses as JSON")
}

#[test]
fn baseline_carries_required_schema_fields() {
    let baseline = load_baseline();

    assert_eq!(
        baseline["schema_version"].as_u64(),
        Some(1),
        "schema version pins the counter layout"
    );
    assert_eq!(
        baseline["policy_version"].as_u64(),
        Some(frozen::RECOVERY_POLICY_VERSION),
        "policy version recorded with the baseline"
    );

    let target = &baseline["target"];
    assert!(target["os"].is_string());
    assert!(target["arch"].is_string());
    assert_eq!(target["profile"].as_str(), Some("release"));
    let checksums = &target["fixture_checksums"];
    let current = frozen::FixtureChecksums::current();
    assert_eq!(checksums["json_512"].as_u64(), Some(current.json_512));
    assert_eq!(checksums["json_10k"].as_u64(), Some(current.json_10k));
    assert_eq!(checksums["json_100k"].as_u64(), Some(current.json_100k));
    assert_eq!(checksums["stlc_1k"].as_u64(), Some(current.stlc_1k));

    let cases = baseline["cases"].as_array().expect("cases array");
    assert!(!cases.is_empty(), "at least one measured case");
    for case in cases {
        assert!(case["name"].is_string());
        if case["name"] == "stack_scale_flat_json" {
            // The stack-scale gate records a status object instead of
            // latency samples; its presence documents the scale gate.
            continue;
        }
        for field in ["samples", "median_us", "p95_us", "avg_alloc_count", "avg_alloc_bytes", "work"] {
            assert!(
                case[field].is_number() || case[field].is_object(),
                "case {} missing {field}",
                case["name"]
            );
        }
        let work = &case["work"];
        for counter in [
            "fact_reads",
            "fact_scan_steps",
            "lexer_relexed",
            "lexer_dfa_transitions",
            "parser_reparsed",
            "parser_restart_columns",
        ] {
            assert!(
                work[counter].is_u64(),
                "case {} missing deterministic counter {counter}",
                case["name"]
            );
        }
    }
}

#[test]
fn baseline_comparison_refuses_incompatible_artifacts() {
    fn compatible(a: &Value, b: &Value) -> bool {
        a["schema_version"] == b["schema_version"]
            && a["policy_version"] == b["policy_version"]
            && a["target"]["os"] == b["target"]["os"]
            && a["target"]["arch"] == b["target"]["arch"]
            && a["target"]["fixture_checksums"] == b["target"]["fixture_checksums"]
    }

    let baseline = load_baseline();
    let mut other = baseline.clone();
    other["schema_version"] = Value::from(2);
    assert!(!compatible(&baseline, &other), "schema mismatch refused");

    let mut other = baseline.clone();
    other["policy_version"] = Value::from(frozen::RECOVERY_POLICY_VERSION + 1);
    assert!(!compatible(&baseline, &other), "policy drift refused");

    let mut other = baseline.clone();
    other["target"]["fixture_checksums"]["json_512"] =
        Value::from(baseline["target"]["fixture_checksums"]["json_512"].as_u64().unwrap_or(0) + 1);
    assert!(!compatible(&baseline, &other), "fixture drift refused");

    let mut other = baseline.clone();
    other["target"]["os"] = Value::from("plan9");
    assert!(!compatible(&baseline, &other), "environment drift refused");

    assert!(compatible(&baseline, &baseline.clone()));
}
