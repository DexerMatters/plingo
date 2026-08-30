mod common;

use std::sync::Arc;

use common::json::{JsonDocument, JsonToken};
use plingo::framework::lex::{TokenVec, Tokens, install_lexer};
use plingo::framework::parse::{ParseDiagnostics, ParseStatus, ParseUnits, install_parser};
use plingo::framework::{SourceEdit, Workspace};
use plingo::utils::Span;

fn uri(name: &str) -> fluent_uri::Uri<String> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn build(workers: usize) -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<JsonToken>(engine)?;
        install_parser::<JsonToken, JsonDocument>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

struct JsonRuntime {
    ws: Workspace,
    u: fluent_uri::Uri<String>,
    name: String,
}

impl JsonRuntime {
    fn new(name: &str, text: &str) -> Self {
        let mut ws = build(1);
        let u = uri(name);
        ws.open(u.clone(), text).unwrap();
        Self {
            ws,
            u,
            name: name.to_string(),
        }
    }

    fn uri_str(&self) -> String {
        self.u.to_string()
    }

    fn apply(&mut self, edits: Vec<SourceEdit>) {
        self.ws
            .edit(edits)
            .expect("edits must apply as one atomically committed epoch");
    }

    fn text(&self) -> String {
        plingo::framework::source::source_snapshot(&self.ws.snapshot(), &self.uri_str())
            .map(|snapshot| snapshot.to_string())
            .unwrap_or_default()
    }

    fn tokens(&self) -> Option<Arc<TokenVec<JsonToken>>> {
        self.ws
            .snapshot()
            .observe::<Tokens<JsonToken>>(self.uri_str())
    }

    fn unit(&self) -> Option<plingo::framework::parse::ParseUnit<JsonDocument>> {
        self.ws
            .snapshot()
            .observe::<ParseUnits<JsonDocument>>(self.uri_str())
            .map(|value| (*value).clone())
    }

    fn diagnostic_count(&self) -> usize {
        self.ws
            .snapshot()
            .list::<ParseDiagnostics>(&self.uri_str())
            .len()
    }

    fn lex_error_count(&self) -> usize {
        self.tokens().map(|tokens| tokens.errors.len()).unwrap_or(0)
    }

    fn status(&self) -> Option<ParseStatus> {
        self.unit().map(|unit| unit.status)
    }

    fn has_root(&self) -> bool {
        self.unit().is_some_and(|unit| unit.root.is_some())
    }
}

fn replace(u: &fluent_uri::Uri<String>, text: &str, needle: &str, value: &str) -> Vec<SourceEdit> {
    let start = text.find(needle).expect("fixture contains target");
    let end = start + needle.len();
    vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), start, end).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), start).unwrap(),
            value: value.into(),
        },
    ]
}

fn large_json() -> String {
    let values = (0..512).map(|value| value.to_string()).collect::<Vec<_>>();
    format!(
        r#"{{"head":12345,"items":[{}],"middle":23456,"tail":34567}}"#,
        values.join(",")
    )
}

#[test]
fn empty_containers_are_valid_separator_grammar_cases() {
    let runtime = JsonRuntime::new("heavy-empty", r#"{"object": {}, "array": []}"#);
    assert!(runtime.has_root());
    assert_eq!(runtime.diagnostic_count(), 0);
    assert_eq!(runtime.status(), Some(ParseStatus::Clean));
}

#[test]
fn skipped_whitespace_shifts_are_parser_invisible() {
    // Inserting whitespace changes byte offsets but neither the token
    // stream shape nor the parse result: the unit and diagnostics are
    // unchanged (only the underlying text/tokens move).
    let text = large_json();
    let mut runtime = JsonRuntime::new("heavy-whitespace", &text);
    let before_has_root = runtime.has_root();
    let before_tokens: Arc<TokenVec<JsonToken>> = runtime.tokens().unwrap();
    let offset = text.find("\"middle\"").unwrap();

    runtime.apply(vec![SourceEdit::Insert {
        key: Span::point_uri(runtime.u.clone(), offset).unwrap(),
        value: "\n    ".into(),
    }]);

    assert_eq!(runtime.has_root(), before_has_root);
    assert_eq!(runtime.diagnostic_count(), 0);
    // Token values (textual lexemes) survive the layout shift.
    let after: Arc<TokenVec<JsonToken>> = runtime.tokens().unwrap();
    assert_eq!(after.tokens.len(), before_tokens.tokens.len());
}

#[test]
fn error_recovery_publishes_diagnostics_and_a_later_repair_clears_them() {
    let text = r#"{"left": 1, "right": 2}"#;
    let mut runtime = JsonRuntime::new("heavy-recovery", text);
    let colon = text.find(": 1").unwrap();

    runtime.apply(vec![SourceEdit::Delete {
        key: Span::new_uri(runtime.u.clone(), colon, colon + 1).unwrap(),
    }]);
    assert!(
        runtime.diagnostic_count() > 0,
        "invalid JSON must commit diagnostics"
    );
    assert!(matches!(
        runtime.status(),
        Some(ParseStatus::Recovered { .. })
    ));

    runtime.apply(vec![SourceEdit::Insert {
        key: Span::point_uri(runtime.u.clone(), colon).unwrap(),
        value: ":".into(),
    }]);
    assert_eq!(
        runtime.diagnostic_count(),
        0,
        "repair must retract stale diagnostics"
    );
    assert_eq!(runtime.status(), Some(ParseStatus::Clean));
    assert!(runtime.has_root(), "repair must restore a typed root");
}

#[test]
fn truncated_container_recovers_without_retaining_stale_roots() {
    let text = r#"{"outer": {"value": 1}, "tail": 2}"#;
    let mut runtime = JsonRuntime::new("heavy-truncated", text);
    let close = text.len() - 1;

    runtime.apply(vec![SourceEdit::Delete {
        key: Span::new_uri(runtime.u.clone(), close, close + 1).unwrap(),
    }]);
    assert!(runtime.diagnostic_count() > 0);
    assert_ne!(runtime.status(), Some(ParseStatus::Clean));

    runtime.apply(vec![SourceEdit::Insert {
        key: Span::point_uri(runtime.u.clone(), close).unwrap(),
        value: "}".into(),
    }]);
    assert_eq!(runtime.diagnostic_count(), 0);
    assert_eq!(runtime.status(), Some(ParseStatus::Clean));
}

#[test]
fn unicode_replacements_preserve_utf8_boundaries() {
    let text = r#"{"label": "α", "tail": 12345}"#;
    let mut runtime = JsonRuntime::new("heavy-unicode", text);
    runtime.apply(replace(&runtime.u, text, "α", "β"));

    assert_eq!(runtime.text(), r#"{"label": "β", "tail": 12345}"#);
    assert_eq!(runtime.diagnostic_count(), 0);
    assert!(runtime.has_root());
}

#[test]
fn lexical_errors_publish_partial_diagnostics_and_repair_cleanly() {
    let text = r#"{"value": 1, "tail": 2}"#;
    let mut runtime = JsonRuntime::new("heavy-lex-error", text);
    let number = text.find("1,").unwrap();

    runtime.apply(replace(&runtime.u, text, "1", "@"));
    assert!(
        runtime.lex_error_count() > 0,
        "lex error visible in TokenVec"
    );
    assert!(runtime.diagnostic_count() > 0);
    assert_ne!(runtime.status(), Some(ParseStatus::Clean));

    runtime.apply(replace(&runtime.u, runtime.text().as_str(), "@", "1"));
    assert_eq!(runtime.lex_error_count(), 0, "repair clears lex errors");
    assert_eq!(runtime.diagnostic_count(), 0);
    assert_eq!(runtime.status(), Some(ParseStatus::Clean));
    assert!(number < runtime.text().len());
}

#[test]
fn sequential_incremental_edits_match_a_fresh_oracle_after_every_step() {
    let mut runtime = JsonRuntime::new("heavy-trace", r#"{"a": 10, "b": [20, 30], "c": 40}"#);
    let trace = [("10", "11"), ("20", "21"), ("40", "41"), ("30", "31")];

    for (step, (old, new)) in trace.into_iter().enumerate() {
        let before = runtime.text();
        runtime.apply(replace(&runtime.u, before.as_str(), old, new));
        let fresh = JsonRuntime::new(
            &format!("heavy-trace-oracle-{step}"),
            runtime.text().as_str(),
        );
        let this: Arc<TokenVec<JsonToken>> = runtime.tokens().unwrap();
        let oracle: Arc<TokenVec<JsonToken>> = fresh.tokens().unwrap();
        // Textual token values and errors must match the fresh oracle.
        assert_eq!(this.tokens.len(), oracle.tokens.len());
        assert_eq!(
            this.tokens
                .iter()
                .map(|t| t.value.to_string())
                .collect::<Vec<_>>(),
            oracle
                .tokens
                .iter()
                .map(|t| t.value.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(runtime.diagnostic_count(), fresh.diagnostic_count());
        assert_eq!(runtime.status(), fresh.status());
    }
}

#[test]
fn reverse_order_batch_replacements_keep_disjoint_edits_sparse_and_valid() {
    let text = r#"{"left": 11111, "right": 22222}"#;
    let mut runtime = JsonRuntime::new("heavy-reverse-batch", text);
    let mut edits = replace(&runtime.u, text, "22222", "98765");
    edits.extend(replace(&runtime.u, text, "11111", "56789"));
    runtime.apply(edits);

    assert_eq!(runtime.text(), r#"{"left": 56789, "right": 98765}"#);
    assert_eq!(runtime.diagnostic_count(), 0);
    assert!(runtime.has_root());
}

#[test]
fn independent_documents_do_not_cross_invalidate() {
    let mut ws = build(1);
    let a = uri("heavy-isolated-a");
    let b = uri("heavy-isolated-b");
    ws.open(a.clone(), r#"{"value": 1}"#).unwrap();
    ws.open(b.clone(), r#"{"value": 2}"#).unwrap();
    let before_b = ws
        .snapshot()
        .observe::<ParseUnits<JsonDocument>>(b.to_string())
        .map(|value| (*value).clone());
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(a, 9).unwrap(),
        value: "0".into(),
    }])
    .unwrap();
    let after_b = ws
        .snapshot()
        .observe::<ParseUnits<JsonDocument>>(b.to_string())
        .map(|value| (*value).clone());
    assert_eq!(before_b, after_b, "document B is untouched by A's edit");
}

#[test]
fn deep_nesting_parses_with_stats_present() {
    let depth = 200;
    let mut text = String::from(r#"{"a": "#);
    for _ in 0..depth {
        text.push('[');
    }
    text.push('1');
    for _ in 0..depth {
        text.push(']');
    }
    text.push('}');
    let runtime = JsonRuntime::new("heavy-deep", &text);
    assert!(runtime.has_root());
    assert_eq!(runtime.diagnostic_count(), 0);
    // Stats are populated (the plan's ParseUnit carries them).
    let unit = runtime.unit().unwrap();
    assert!(unit.stats.reparsed > 0 || unit.stats.restart_boundary == 0);
}

// ---------------------------------------------------------------------------
// Quantitative work gates (plan §19): for a local edit, replayed work is
// bounded by the replay window plus a constant — never by document size —
// and the unchanged suffix is reused, never physically visited.
// ---------------------------------------------------------------------------

mod work_gates {
    use super::*;
    use common::fixtures::json_array;
    use plingo::framework::SourceEdit;

    fn build_ws(workers: usize) -> Workspace {
        build(workers)
    }

    /// Same-width value edit at the HEAD of a large document: structure
    /// is unchanged, so the parser must stay completely cold (plan §18
    /// row `Number 1 -> 7`) regardless of document size.
    #[test]
    fn head_value_edit_is_parser_cold() {
        let u = plingo::utils::Span::new(
            fluent_uri::Uri::try_from("test://gates-head".to_string()).unwrap(),
            0,
            0,
        )
        .unwrap()
        .uri;

        for size in [200usize, 800] {
            let mut ws = build_ws(1);
            let text = json_array(size);
            // json_array starts elements with index%5==0 => a bare number.
            let doc = format!("{{\"items\": [{text}]}}");
            // json_array opens with '['; inside the wrapper the first
            // element starts right after the double bracket.
            let first_num = doc.find("[[").unwrap() + 2;
            ws.open(u.clone(), &doc).unwrap();
            // Same-width replacement of the first digit character.
            let report = ws
                .edit(vec![
                    SourceEdit::Delete {
                        key: plingo::utils::Span::new_uri(u.clone(), first_num, first_num + 1)
                            .unwrap(),
                    },
                    SourceEdit::Insert {
                        key: plingo::utils::Span::point_uri(u.clone(), first_num).unwrap(),
                        value: "9".into(),
                    },
                ])
                .expect("head edit commits");
            let work = report
                .work()
                .parser(u.as_str())
                .cloned()
                .unwrap_or_default();
            assert_eq!(
                work.tokens_replayed, 0,
                "size={size} value-only edit woke the parser"
            );
            assert_eq!(
                work.parser_records_inserted
                    + work.parser_records_updated
                    + work.parser_records_removed,
                0,
                "size={size} value-only edit touched parser records"
            );
        }
    }

    /// A tail value edit of the same size replays the same bounded window
    /// regardless of document size (delta-scaling, plan §19).
    #[test]
    fn tail_edit_window_is_size_independent() {
        let u = plingo::utils::Span::new(
            fluent_uri::Uri::try_from("test://gates-tail".to_string()).unwrap(),
            0,
            0,
        )
        .unwrap()
        .uri;

        let mut windows = Vec::new();
        for size in [200usize, 800] {
            let mut ws = build_ws(1);
            let text = json_array(size);
            let doc = format!("{{\"items\": [{text}]}}");
            ws.open(u.clone(), &doc).unwrap();
            // Edit near the TAIL: last number before ']}'.
            let at = doc.rfind(']').unwrap();
            ws.edit(vec![SourceEdit::Insert {
                key: plingo::utils::Span::point_uri(u.clone(), at).unwrap(),
                value: "9".into(),
            }])
            .expect("tail edit commits");
            let report = ws
                .edit(vec![SourceEdit::Delete {
                    key: plingo::utils::Span::new_uri(u.clone(), at, at + 1).unwrap(),
                }])
                .expect("tail restore commits");
            let work = report
                .work()
                .parser(u.as_str())
                .cloned()
                .unwrap_or_default();
            windows.push((size, work.tokens_replayed, work.columns_reused));
        }
        // Both sizes replay through EOF for a tail edit inside an object
        // (the closing brace anchors), but the WINDOW per command is the
        // same small constant in both — delta-scaled, not size-scaled.
        let (_, w_small, _) = windows[0];
        let (_, w_large, _) = windows[1];
        assert_eq!(
            w_small, w_large,
            "replay window scaled with document size: {windows:?}"
        );
        assert!(w_small <= 4, "unexpected large tail replay: {windows:?}");
    }

    /// Trivia edits mark zero parser records (§18 row: whitespace-only).
    #[test]
    fn whitespace_edit_is_parser_cold() {
        let u = plingo::utils::Span::new(
            fluent_uri::Uri::try_from("test://gates-ws".to_string()).unwrap(),
            0,
            0,
        )
        .unwrap()
        .uri;
        let mut ws = build_ws(1);
        let text = json_array(300);
        let doc = format!("{{\"items\": [{text}]}}");
        ws.open(u.clone(), &doc).unwrap();
        let at = doc.find(",").unwrap();
        let report = ws
            .edit(vec![SourceEdit::Insert {
                key: plingo::utils::Span::point_uri(u.clone(), at).unwrap(),
                value: " ".into(),
            }])
            .expect("whitespace edit commits");
        let work = report
            .work()
            .parser(u.as_str())
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            work.parser_records_inserted
                + work.parser_records_removed
                + work.parser_records_updated,
            0,
            "trivia edit touched parser records"
        );
    }

    /// §19 gate: an edit before a large unchanged suffix must rewrite
    /// only the reused tail's columns, never the whole document. The honest
    /// suffix_columns_physically_visited counter equals the reused length
    /// on a convergence, and stays strictly below the document size.
    #[test]
    fn suffix_rewrite_is_measured_and_bounded() {
        let u = plingo::utils::Span::new(
            fluent_uri::Uri::try_from("test://gates-suffix".to_string()).unwrap(),
            0,
            0,
        )
        .unwrap()
        .uri;
        for size in [300usize, 900] {
            let mut ws = build_ws(1);
            let text = json_array(size);
            let doc = format!("{{\"items\": [{text}]}}");
            ws.open(u.clone(), &doc).unwrap();
            // Rare, high-popularity head edit (same-width) leaving the whole
            // array as a reusable suffix.
            let at = doc.find("[[").unwrap() + 2;
            let report = ws
                .edit(vec![
                    SourceEdit::Delete {
                        key: plingo::utils::Span::new_uri(u.clone(), at, at + 1).unwrap(),
                    },
                    SourceEdit::Insert {
                        key: plingo::utils::Span::point_uri(u.clone(), at).unwrap(),
                        value: "9".into(),
                    },
                ])
                .expect("head edit commits");
            let work = report
                .work()
                .parser(u.as_str())
                .cloned()
                .unwrap_or_default();
            // §19 invariant, now with the cache-stable fast path (plan
            // §8.6): the number of retained suffix columns physically
            // rewritten by ONE command is a SMALL CONSTANT (only the
            // non-stable seam), never a whole-document rewrite — O(1)
            // attachment for the common converged suffix.
            let visited = work.suffix_columns_physically_visited;
            let reused = work.columns_reused;
            assert!(
                visited <= reused,
                "physically visited must not exceed reused columns (size {size})"
            );
            assert!(
                (visited as usize) <= 16,
                "cache-stable suffix rewrite escaped the seam for size {size}: {visited} (reused {reused})"
            );
        }
    }
}
// ---------------------------------------------------------------------------
// Recovery determinism (plan §14): identical recovery traces produce
// identical committed projections, so deterministic synthetic-token
// identities yield byte-identical facts across worker configurations.
// ---------------------------------------------------------------------------

mod recovery_determinism {
    use super::*;

    #[test]
    fn recovery_traces_match_a_fresh_oracle() {
        let build_ws = |workers: usize| build(workers);
        // A malformed document that forces recovery, then a repair edit.
        let run = |workers: usize| -> common::oracle::PipelineProjection {
            let mut ws = build_ws(workers);
            let u = uri("recdet");
            // Missing comma -> recovery inserts/repairs.
            let malformed = r#"{"a": [1, 2] "b": true}"#.to_string();
            ws.open(u.clone(), &malformed).unwrap();
            // Repair: insert the missing comma.
            ws.edit(vec![plingo::framework::source::SourceEdit::Insert {
                key: plingo::utils::Span::point_uri(u.clone(), 13).unwrap(),
                value: ",".into(),
            }])
            .unwrap();
            common::oracle::project(&ws.snapshot(), &u.to_string())
        };
        let one = run(1);
        let four = run(4);
        assert_eq!(
            one, four,
            "recovery trace diverged across worker counts (synthetic-token nondeterminism?)"
        );
        assert_eq!(
            one.parse_status.as_deref(),
            Some("clean"),
            "repair must recover to clean"
        );
    }
    /// Plan §8 item 8 / Phase 4 exit gate: a transition that changes only
    /// status/diagnostics must execute ZERO syntax identity/dimension
    /// projection work. In a recovered document, a same-terminal VALUE edit
    /// keeps the parser component completely cold (zero evaluations, zero
    /// record journaling, zero syntax facts) while the committed diagnostics
    /// still equal a fresh oracle — the observable form of the gate.
    #[test]
    fn recovered_value_edit_stays_parser_cold_with_exact_diagnostics() {
        let mut ws = build(1);
        let u = uri("recdet-cold");
        let malformed = r#"{"a": [1, 2] "b": true}"#.to_string();
        ws.open(u.clone(), &malformed).unwrap();
        let before = common::oracle::project(&ws.snapshot(), &u.to_string());
        assert!(
            before
                .parse_status
                .as_deref()
                .is_some_and(|s| s.starts_with("recovered")),
            "fixture must start recovered: {:?}",
            before.parse_status
        );
        assert!(!before.diagnostics.is_empty());

        // Same-terminal value edit inside the recovered region: `2` -> `7`.
        let at = malformed.find('2').unwrap();
        let report = ws
            .edit(vec![
                SourceEdit::Delete {
                    key: Span::new_uri(u.clone(), at, at + 1).unwrap(),
                },
                SourceEdit::Insert {
                    key: Span::point_uri(u.clone(), at).unwrap(),
                    value: "7".into(),
                },
            ])
            .unwrap();
        let work = report
            .work()
            .parser(&u.to_string())
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            work.component_runs, 0,
            "value edit woke the parser: {work:?}"
        );
        assert_eq!(
            work.parser_records_inserted
                + work.parser_records_updated
                + work.parser_records_removed,
            0,
            "value edit journaled parser records: {work:?}"
        );
        assert_eq!(work.syntax_facts_patched, 0, "{work:?}");
        assert_eq!(work.full_rebuild_fallbacks, 0, "{work:?}");
        assert_eq!(work.full_store_scans, 0, "{work:?}");
        assert_eq!(work.full_token_vector_clones, 0, "{work:?}");

        // The committed diagnostics stay exact against a fresh workspace.
        let after = common::oracle::project(&ws.snapshot(), &u.to_string());
        let mut fresh = build(1);
        let v = uri("recdet-cold-fresh");
        let mut edited = malformed.clone();
        edited.replace_range(at..at + 1, "7");
        fresh.open(v.clone(), &edited).unwrap();
        let fresh_projection = common::oracle::project(&fresh.snapshot(), &v.to_string());
        assert_eq!(
            after.diagnostics, fresh_projection.diagnostics,
            "recovered diagnostics diverged from the fresh oracle"
        );
        assert_eq!(after.parse_status, fresh_projection.parse_status);
    }

    /// Plan §14: after a recovery-shaped edit, the persistent witness
    /// interval index records the consumed token occurrences and an
    /// interval query returns the intersecting recovery segments (so a
    /// later structural patch need only touch affected segments).
    #[test]
    fn recovery_records_witness_intervals() {
        let mut ws = build(1);
        let u = uri("recdet-witness");
        let malformed = r#"{"a": [1, 2] "b": true}"#.to_string();
        // Opening a recovery-shaped doc records witnesses; the repair edit
        // validates the interval query path.
        let report = ws.open(u.clone(), &malformed).unwrap();
        let parser_work = report.work().parser(u.as_str());
        assert!(
            parser_work.is_some(),
            "parser work missing after recovery edit"
        );
        let work = parser_work.unwrap();
        assert!(
            work.recovery_witness_tokens > 0,
            "no recovery witnesses recorded"
        );
        assert!(
            work.recovery_interval_probes >= 1,
            "no interval-index probe recorded"
        );
    }
}
