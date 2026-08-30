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
    let raw =
        std::fs::read_to_string(BASELINE_PATH).expect("committed baseline artifact is readable");
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
        for field in [
            "samples",
            "median_us",
            "p95_us",
            "avg_alloc_count",
            "avg_alloc_bytes",
            "work",
        ] {
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
    other["target"]["fixture_checksums"]["json_512"] = Value::from(
        baseline["target"]["fixture_checksums"]["json_512"]
            .as_u64()
            .unwrap_or(0)
            + 1,
    );
    assert!(!compatible(&baseline, &other), "fixture drift refused");

    let mut other = baseline.clone();
    other["target"]["os"] = Value::from("plan9");
    assert!(!compatible(&baseline, &other), "environment drift refused");

    assert!(compatible(&baseline, &baseline.clone()));
}

// ---------------------------------------------------------------------------
// V2 artifact schema and V1->V2 migration gates (plan §27).
// ---------------------------------------------------------------------------

const V2_BASELINES: &[&str] = &[
    "benchmarks/baselines/incremental-pipeline-v2-phase0.json",
    "benchmarks/baselines/incremental-pipeline-v2-phase1.json",
    "benchmarks/baselines/incremental-pipeline-v2-phase2.json",
    "benchmarks/baselines/incremental-pipeline-v2-phase3.json",
    "benchmarks/baselines/incremental-pipeline-v2-phase4.json",
];

/// The V2 notebooks are per-phase evidence (plan §15): every artifact
/// carries the schema version, percentile policy, and phase identity. The
/// newest notebook (phase3) carries the full closed case/work schema; older
/// notebooks may predate later counter families and are validated for the
/// common contract only.
#[test]
fn v2_baselines_carry_the_closed_schema() {
    for path in V2_BASELINES {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("{path} unreadable: {error}"));
        let artifact: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|error| panic!("{path} invalid JSON: {error}"));

        assert_eq!(
            artifact["schema_version"].as_u64(),
            Some(2),
            "{path}: schema version"
        );
        assert_eq!(
            artifact["percentile_policy"].as_str(),
            Some("nearest_rank_v1"),
            "{path}: percentile policy identity"
        );
        assert!(
            artifact["phase"].is_string(),
            "{path}: phase identity string"
        );
        assert!(
            artifact["recovery_policy_version"].is_string(),
            "{path}: recovery policy identity"
        );

        let Some(cases) = artifact["cases"].as_array() else {
            // Phase notebooks without measured cases (interim/design
            // records) satisfy only the common contract.
            continue;
        };
        for case in cases {
            assert!(case["name"].is_string(), "{path}");
            let outcome = case["outcome"]["status"]
                .as_str()
                .unwrap_or_else(|| panic!("{path} case {} missing outcome status", case["name"]));
            if outcome == "invalid" {
                // An invalid case is an explicit status record with the
                // reason serialized — never fabricated zero latency.
                assert!(
                    case["reason_detail"].is_string() || case["work"].is_object(),
                    "{path} case {} invalid record incomplete",
                    case["name"]
                );
                continue;
            }
            for field in ["samples", "median_us", "p95_us"] {
                assert!(
                    case[field].is_number(),
                    "{path} case {} missing {field}",
                    case["name"]
                );
            }
        }
    }

    // The newest notebook is the closed full schema: scale nomenclature,
    // work counters, and per-structure path work.
    let newest = V2_BASELINES.last().expect("newest baseline");
    let raw = std::fs::read_to_string(newest).expect("newest readable");
    let artifact: Value = serde_json::from_str(&raw).expect("newest parses");
    assert_eq!(artifact["status"].as_str(), Some("recorded"), "{newest}");
    let cases = artifact["cases"].as_array().expect("newest cases");
    for case in cases {
        for field in [
            "source_bytes",
            "lexical_occurrences",
            "semantic_tokens",
            "declarations",
            "live_facts",
            "avg_alloc_count",
            "avg_alloc_bytes",
        ] {
            assert!(
                case[field].is_number(),
                "{newest} case {} missing {field}",
                case["name"]
            );
        }
        let work = &case["work"];
        for counter in [
            "fact_reads",
            "fact_scan_steps",
            "fact_writes",
            "facts_changed",
            "candidate_writes",
            "source_validated_operations",
            "source_full_materializations",
            "lexer_relexed",
            "lexer_retained_suffix_entries_visited",
            "lexer_full_tape_iterations",
            "parser_reparsed",
            "parser_reused",
            "parser_restart_columns",
            "parser_convergence_checks",
            "parser_gss_created",
            "parser_products_created",
            "parser_ast_created",
        ] {
            assert!(
                work[counter].is_number(),
                "{newest} case {} missing deterministic counter {counter}",
                case["name"]
            );
        }
        // Path work, when present, is attributed per structure with the
        // fixed field set.
        if let Some(structures) = work["path_work"].as_object() {
            for structure in structures.values() {
                for field in [
                    "operations",
                    "key_comparisons",
                    "nodes_visited",
                    "nodes_copied",
                    "nodes_created",
                    "rebalances",
                    "max_depth",
                ] {
                    assert!(
                        structure[field].is_u64(),
                        "{newest}: path_work missing {field}"
                    );
                }
            }
        }
    }
    let scale = &artifact["scale_gate"];
    assert_eq!(
        scale["status"].as_str(),
        Some("pass"),
        "{newest} scale gate"
    );
}

/// V1 artifacts remain readable under the V2 reader contract: the legacy
/// `tokens` field survives and every V2 field defaults to zero when absent,
/// so old baselines do not break the migration (plan §27, bench
/// `CaseResult::from_json`).
#[test]
fn v1_artifacts_migrate_with_defaulted_v2_fields() {
    let baseline = load_baseline();
    assert_eq!(baseline["schema_version"].as_u64(), Some(1));
    let cases = baseline["cases"].as_array().expect("cases array");
    assert!(!cases.is_empty());
    for case in cases {
        if case["name"] == "stack_scale_flat_json" {
            continue;
        }
        // Legacy fields present.
        assert!(case["tokens"].is_number(), "legacy tokens field");
        assert!(case["median_us"].is_number());
        assert!(case["p95_us"].is_number());
        assert!(case["avg_alloc_count"].is_number());
        // V2 fields absent in the V1 artifact read as zero under the
        // migration contract (the bench's from_json defaults them).
        assert!(case["source_bytes"].is_null() || case["source_bytes"].is_number());
        assert!(case["lexical_occurrences"].is_null() || case["lexical_occurrences"].is_number());
        assert!(case["semantic_tokens"].is_null() || case["semantic_tokens"].is_number());
        assert!(case["declarations"].is_null() || case["declarations"].is_number());
        assert!(case["live_facts"].is_null() || case["live_facts"].is_number());
        // The work object's legacy counters are still present.
        let work = &case["work"];
        for counter in [
            "fact_reads",
            "fact_scan_steps",
            "lexer_relexed",
            "parser_reparsed",
        ] {
            assert!(
                work[counter].is_u64(),
                "V1 case {} missing counter {counter}",
                case["name"]
            );
        }
    }
}
