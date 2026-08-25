//! Incremental pipeline benchmark (plan §10.2, §10.4).
//!
//! Custom runner (`harness = false`): deterministic fixtures, 10 warm-ups,
//! at least 50 measured commands per case, a counting global allocator reset
//! outside each measured region, and schema-versioned JSON output recording
//! medians, p95, allocations, and every deterministic work counter.
//!
//! Usage:
//! - `cargo bench --bench incremental_pipeline`
//! - `PLINGO_BENCH_OUT=path.json cargo bench --bench incremental_pipeline`

#![allow(dead_code)]

#[path = "../tests/common/fixtures.rs"]
mod fixtures;
#[path = "../tests/common/frozen.rs"]
mod frozen;
#[path = "../tests/common/json.rs"]
mod json;
use json::{JsonDocument, JsonToken};

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use plingo::framework::lex::install_lexer;
use plingo::framework::parse::install_parser_tree;
use plingo::framework::source::{SourceEdit, SourceRevisions};
use plingo::framework::Workspace;
use plingo::utils::Span;

#[path = "../examples/stlc/syntax.rs"]
mod syntax;
#[path = "../examples/stlc/structural.rs"]
mod structural;
#[path = "../examples/stlc/name_resolve.rs"]
mod name_resolve;
#[path = "../examples/stlc/check.rs"]
mod check;

use check::check_pass;
use name_resolve::{name_pass, resolve_pass};
use structural::structural_pass;
use syntax::{StlcDocument, StlcToken};
// ---------------------------------------------------------------------------

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static COUNTING_ENABLED: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING_ENABLED.load(Ordering::Relaxed) == 1 {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

struct AllocationProbe;

impl AllocationProbe {
    fn start() -> Self {
        ALLOC_COUNT.store(0, Ordering::Relaxed);
        ALLOC_BYTES.store(0, Ordering::Relaxed);
        COUNTING_ENABLED.store(1, Ordering::Relaxed);
        AllocationProbe
    }
}

impl Drop for AllocationProbe {
    fn drop(&mut self) {
        COUNTING_ENABLED.store(0, Ordering::Relaxed);
    }
}

fn allocation_totals() -> (u64, u64) {
    (
        ALLOC_COUNT.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// Workspace plumbing
// ---------------------------------------------------------------------------

const URI_NAME: &str = "bench";
fn uri() -> fluent_uri::Uri<String> {
    Span::new(format!("test://{URI_NAME}"), 0, 0)
        .expect("bench uri parses")
        .uri
}

fn build() -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<JsonToken>(engine)?;
        plingo::framework::parse::install_parser::<JsonToken, JsonDocument>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

fn build_stlc() -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<StlcToken>(engine)?;
        install_parser_tree::<StlcToken, StlcDocument>(engine)?;
        for pass in [
            name_pass as fn(()) -> plingo::reactive::Result<()>,
            resolve_pass,
            check_pass,
            structural_pass,
        ] {
            let planned = engine.plan(pass, ())?;
            let _running = engine.run(&planned)?;
        }
        Ok(())
    })
    .expect("STLC workspace builds")
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

struct Sample {
    elapsed: Duration,
    alloc_count: u64,
    alloc_bytes: u64,
}

/// Runs `operation` `warmups + samples` times, measuring only the sample
/// window. The allocator probe wraps each operation individually.
fn measure(mut operation: impl FnMut() -> plingo::framework::WorkspaceReport, warmups: usize, samples: usize) -> Vec<Sample> {
    for _ in 0..warmups {
        let _probe = AllocationProbe::start();
        let _ = operation();
    }
    let mut out = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let _probe = AllocationProbe::start();
        let report = operation();
        drop(_probe);
        let elapsed = start.elapsed();
        let (count, bytes) = allocation_totals();
        black_box(&report);
        out.push(Sample { elapsed, alloc_count: count, alloc_bytes: bytes });
    }
    out
}

fn black_box<T>(value: &T) {
    // Keep the compiler from eliding the work without std::hint instability.
    std::sync::atomic::fence(Ordering::SeqCst);
    let _ = value as *const T as usize;
    std::sync::atomic::fence(Ordering::SeqCst);
}

fn median(samples: &[Sample]) -> Duration {
    let mut values: Vec<Duration> = samples.iter().map(|s| s.elapsed).collect();
    values.sort();
    values[values.len() / 2]
}

fn p95(samples: &[Sample]) -> Duration {
    let mut values: Vec<Duration> = samples.iter().map(|s| s.elapsed).collect();
    values.sort();
    values[((values.len() as f64) * 0.95).ceil() as usize % values.len()]
}

fn total_allocs(samples: &[Sample]) -> u64 {
    samples.iter().map(|s| s.alloc_count).sum::<u64>() / samples.len().max(1) as u64
}

fn total_alloc_bytes(samples: &[Sample]) -> u64 {
    samples.iter().map(|s| s.alloc_bytes).sum::<u64>() / samples.len().max(1) as u64
}

/// One replacement of the last occurrence of `needle` in `text`.
#[allow(dead_code)]
fn replace_last(text: &str, needle: &str, value: &str) -> Vec<SourceEdit> {
    let u = uri();
    let start = text.rfind(needle).unwrap_or_else(|| panic!("rneedle {needle:?} absent"));
    let end = start + needle.len();
    vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), start, end).expect("range"),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u, start).expect("point"),
            value: value.into(),
        },
    ]
}

/// One replacement of the first occurrence of `needle` in `text`.
fn replace_first(text: &str, needle: &str, value: &str) -> Vec<SourceEdit> {
    let span = Span::new("test://bench", 0, 0).expect("bench span");
    replace_at(span.uri, text, needle, value)
}

/// One replacement against an explicit document URI.
fn replace_at(
    u: fluent_uri::Uri<String>,
    text: &str,
    needle: &str,
    value: &str,
) -> Vec<SourceEdit> {
    let start = text
        .find(needle)
        .unwrap_or_else(|| panic!("find {needle:?} absent in {} bytes", text.len()));
    let end = start + needle.len();
    vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), start, end).expect("range"),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u, start).expect("point"),
            value: value.into(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

struct CaseResult {
    name: String,
    tokens: usize,
    samples: usize,
    median_us: u128,
    p95_us: u128,
    alloc_count: u64,
    alloc_bytes: u64,
    work_json: String,
}

impl CaseResult {
    fn to_json(&self) -> String {
        format!(
            "{{\"name\": {:?}, \"tokens\": {}, \"samples\": {}, \"median_us\": {}, \"p95_us\": {}, \"avg_alloc_count\": {}, \"avg_alloc_bytes\": {}, \"work\": {}}}",
            self.name,
            self.tokens,
            self.samples,
            self.median_us,
            self.p95_us,
            self.alloc_count,
            self.alloc_bytes,
            if self.work_json.trim().starts_with('{') {
                self.work_json.clone()
            } else {
                format!("\"status\": {:?}", self.work_json)
            }
        )
    }

    fn from_json(line: &str) -> Self {
        let value: serde_json_value_stub::Value = serde_json_value_stub::parse(line);
        CaseResult {
            name: value.string("name"),
            tokens: value.usize("tokens"),
            samples: value.usize("samples"),
            median_us: value.u128("median_us"),
            p95_us: value.u128("p95_us"),
            alloc_count: value.u64("avg_alloc_count"),
            alloc_bytes: value.u64("avg_alloc_bytes"),
            work_json: value.raw("work"),
        }
    }
}

/// Minimal JSON field extractor for the machine-generated one-line records
/// above; benches cannot use dev-dependencies.
mod serde_json_value_stub {
    pub struct Value(String);

    pub fn parse(text: &str) -> Value {
        Value(text.trim().to_string())
    }

    impl Value {
        fn field(&self, key: &str) -> Option<String> {
            let needle = format!("\"{key}\":");
            let start = self.0.find(&needle)? + needle.len();
            let rest = self.0[start..].trim_start();
            if let Some(stripped) = rest.strip_prefix('"') {
                let end = stripped.find('"')?;
                return Some(stripped[..end].to_string());
            }
            if let Some(stripped) = rest.strip_prefix('{') {
                // Balanced-brace scan keeps nested counter objects intact.
                let mut depth = 1usize;
                let mut end = stripped.len();
                for (index, c) in stripped.char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = index + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                return Some(format!("{{{}", &stripped[..end]));
            }
            let end = rest
                .find(|c: char| c == ',' || c == '}')
                .unwrap_or(rest.len());
            Some(rest[..end].trim().to_string())
        }

        pub fn string(&self, key: &str) -> String {
            let raw = self.field(key).unwrap_or_default();
            raw.trim_matches('"').to_string()
        }

        pub fn raw(&self, key: &str) -> String {
            self.field(key).unwrap_or_else(|| "null".into())
        }

        pub fn usize(&self, key: &str) -> usize {
            self.field(key).and_then(|v| v.parse().ok()).unwrap_or(0)
        }

        pub fn u64(&self, key: &str) -> u64 {
            self.field(key).and_then(|v| v.parse().ok()).unwrap_or(0)
        }

        pub fn u128(&self, key: &str) -> u128 {
            self.field(key).and_then(|v| v.parse().ok()).unwrap_or(0)
        }
    }
}

fn run_case(
    name: &str,
    initial_text: &str,
    edit_builder: impl Fn(&str, bool) -> Vec<SourceEdit>,
    tokens: usize,
    warmups: usize,
    samples: usize,
) -> CaseResult {
    // Initial load measurement.
    let load_samples = measure(
        || {
            let mut ws = build();
            ws.open(uri(), initial_text).expect("open commits")
        },
        warmups,
        samples,
    );
    black_box(&load_samples);

    // Edit measurement: edits oscillate (a -> b -> a ...) so every sample
    // has the same local grammar effect and the needle always exists. The
    // mirror refreshes from the committed snapshot after every command.
    let mut ws = build();
    ws.open(uri(), initial_text).expect("open commits");
    let mut mirror = initial_text.to_string();
    let mut toggle = false;
    let mut edit_samples: Vec<Sample> = Vec::with_capacity(samples);
    for index in 0..warmups + samples {
        let per_edit = edit_builder(&mirror, toggle);
        toggle = !toggle;
        let report = if index < warmups {
            ws.edit(per_edit).expect("edit commits")
        } else {
            let start = Instant::now();
            let probe = AllocationProbe::start();
            let report = ws.edit(per_edit).expect("edit commits");
            drop(probe);
            let elapsed = start.elapsed();
            let (count, bytes) = allocation_totals();
            black_box(&report);
            edit_samples.push(Sample {
                elapsed,
                alloc_count: count,
                alloc_bytes: bytes,
            });
            report
        };
        mirror = ws
            .snapshot()
            .observe::<SourceRevisions>(uri().to_string())
            .map(|revision| revision.text().chunks().collect())
            .unwrap_or_else(String::new);
        if edit_samples.len() == samples {
            break;
        }
    }

    // Work counters from one final representative command.
    let final_report = ws
        .edit(edit_builder(&mirror, toggle))
        .expect("final counters commit");
    let work_json = work_counters_json(&final_report);

    CaseResult {
        name: name.to_string(),
        tokens,
        samples: edit_samples.len(),
        median_us: median(&edit_samples).as_micros(),
        p95_us: p95(&edit_samples).as_micros(),
        alloc_count: total_allocs(&edit_samples),
        alloc_bytes: total_alloc_bytes(&edit_samples),
        work_json,
    }
}

fn work_counters_doc(report: &plingo::framework::WorkspaceReport, uri_string: &str) -> String {
    work_counters_for(report, Some(uri_string))
}

fn work_counters_json(report: &plingo::framework::WorkspaceReport) -> String {
    work_counters_for(report, None)
}

fn work_counters_for(report: &plingo::framework::WorkspaceReport, doc_uri: Option<&str>) -> String {
    let engine = report.engine_work();
    let mut parts = vec![
        format!("\"rounds\":{}", report.rounds()),
        format!("\"fact_reads\":{}", engine.fact_reads),
        format!("\"fact_scan_steps\":{}", engine.fact_scan_steps),
        format!("\"fact_writes\":{}", engine.fact_writes),
        format!("\"fact_retractions\":{}", engine.fact_retractions),
        format!("\"facts_changed\":{}", engine.facts_changed),
        format!("\"candidate_writes\":{}", engine.fact_writes.saturating_add(engine.fact_retractions)),
        format!("\"committed_changes\":{}", engine.facts_changed),
        format!("\"patch_key_lookups\":{}", engine.patch_key_lookups),
        format!("\"patch_key_comparisons\":{}", engine.patch_key_comparisons),
        format!("\"patch_ops_coalesced\":{}", engine.patch_ops_coalesced),
        format!("\"ordered_splices_applied\":{}", engine.ordered_splices_applied),
        format!("\"forbidden_full_vector_scans\":{}", engine.full_patch_vector_scans),
        format!("\"invocation_scans\":{}", engine.invocation_scans),
        format!("\"state_diffs\":{}", engine.state_diffs),
        format!("\"diff_scan_steps\":{}", engine.diff_scan_steps),
    ];
    let uri_string = match doc_uri {
        Some(explicit) => explicit.to_string(),
        None => uri().to_string(),
    };
    if let Some(lexer) = report.work().lexer(&uri_string) {
        parts.push(format!("\"lexer_restart_bytes\":{}", lexer.restart_bytes));
        parts.push(format!("\"lexer_relexed\":{}", lexer.tokens_replayed));
        parts.push(format!("\"lexer_reused\":{}", lexer.tokens_reused));
        parts.push(format!("\"lexer_dfa_transitions\":{}", lexer.dfa_transitions));
        parts.push(format!("\"lexer_bytes_examined\":{}", lexer.source_bytes_examined));
        parts.push(format!("\"lexer_eof_replays\":{}", lexer.eof_replays));
    }
    if let Some(parser) = report.work().parser(&uri_string) {
        parts.push(format!("\"parser_restart_columns\":{}", parser.restart_columns));
        parts.push(format!("\"parser_reparsed\":{}", parser.columns_replayed));
        parts.push(format!("\"parser_reused\":{}", parser.columns_reused));
        parts.push(format!("\"parser_convergence_checks\":{}", parser.checkpoint_comparisons));
        parts.push(format!("\"parser_frontier_matches\":{}", parser.frontier_comparisons));
        parts.push(format!("\"parser_gss_created\":{}", parser.gss_records_created));
        parts.push(format!("\"parser_products_created\":{}", parser.product_records_created));
        parts.push(format!("\"parser_ast_created\":{}", parser.ast_records_created));
        parts.push(format!("\"parser_snapshot_entries\":{}", parser.snapshot_entries_changed));
        parts.push(format!("\"parser_recovery_searches\":{}", parser.recovery_searches));
        parts.push(format!("\"parser_eof_replays\":{}", parser.eof_replays));
    }
    format!("{{{}}}", parts.join(","))
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

const SCHEMA_VERSION: u32 = 1;

fn emit(results: &[CaseResult], path: Option<&str>) {
    let checksums = frozen::FixtureChecksums::current();
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(&format!("  \"schema_version\": {SCHEMA_VERSION},\n"));
    json.push_str(&format!(
        "  \"policy_version\": {},\n",
        frozen::RECOVERY_POLICY_VERSION
    ));
    json.push_str("  \"target\": {\n");
    json.push_str(&format!(
        "    \"os\": {:?},\n    \"arch\": {:?},\n    \"profile\": \"release\",\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    json.push_str(&format!(
        "    \"fixture_checksums\": {{\"json_512\": {}, \"json_10k\": {}, \"json_100k\": {}, \"stlc_1k\": {}}}\n",
        checksums.json_512, checksums.json_10k, checksums.json_100k, checksums.stlc_1k
    ));
    json.push_str("  },\n");
    json.push_str("  \"cases\": [\n");
    for (index, case) in results.iter().enumerate() {
        json.push_str("    {\n");
        json.push_str(&format!("      \"name\": {:?},\n", case.name));
        json.push_str(&format!("      \"tokens\": {},\n", case.tokens));
        json.push_str(&format!("      \"samples\": {},\n", case.samples));
        json.push_str(&format!("      \"median_us\": {},\n", case.median_us));
        json.push_str(&format!("      \"p95_us\": {},\n", case.p95_us));
        json.push_str(&format!("      \"avg_alloc_count\": {},\n", case.alloc_count));
        json.push_str(&format!("      \"avg_alloc_bytes\": {},\n", case.alloc_bytes));
        json.push_str(&format!("      \"work\": {}\n", case.work_json));
        json.push_str(if index + 1 == results.len() { "    }\n" } else { "    },\n" });
    }
    json.push_str("  ]\n}\n");

    match path {
        Some(path) => {
            std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap_or_else(|| std::path::Path::new(".")))
                .expect("baseline dir");
            std::fs::write(path, json).expect("baseline write");
            eprintln!("baseline written to {path}");
        }
        None => print!("{json}"),
    }
}

/// Stack-scale probe entry point: runs in a spawned child process so a
/// stack overflow abort cannot kill the harness (plan §11 Phase 0 step 3).
fn stack_probe_child(elements: usize) -> i32 {
    let text = fixtures::json_array(elements);
    let mut ws = build();
    let start = Instant::now();
    if ws.open(uri(), &text).is_err() {
        return 2;
    }
    eprintln!(
        "stack probe ok: {{\"elements\": {elements}, \"bytes\": {}, \"open_us\": {}}}",
        text.len(),
        start.elapsed().as_micros()
    );
    0
}

/// The registry of measured case names; each runs in its own process.
const CASE_NAMES: &[&str] = &[
    "json_512_replace_s1",
    "json_4096_replace_true",
    "json_12000_replace_null",
    "json_trivia_insert",
    "json_two_distant_edits",
    "two_documents_edit_a",
    "stlc_64_literal",
];

fn run_single_case(name: &str, warmups: usize, samples: usize) -> Option<CaseResult> {
    match name {
        "json_512_replace_s1" => Some(scale_case(512, "s1", "t9", false, name, warmups, samples)),
        "json_4096_replace_true" => Some(scale_case(4_096, "true", "\"mid\"", false, name, warmups, samples)),
        "stlc_64_literal" => Some(stlc_case(name, warmups, samples)),
        // Large JSON cases cap sample counts to keep the artifact bounded
        // while recording the actual measured sample count.
        "json_12000_replace_null" => Some(scale_case(
            12_000,
            "null",
            "\"z99\"",
            true,
            name,
            warmups.min(2),
            samples.min(8),
        )),
        // Heavy baseline cases cap their iteration counts for the same
        // arena-growth reason as the 12k scale case.
        "json_trivia_insert" => Some(trivia_case(name, warmups.min(2), samples.min(8))),
        "json_two_distant_edits" => Some(distant_case(name, warmups.min(2), samples.min(8))),
        "two_documents_edit_a" => Some(two_documents_case(warmups, samples)),
        _ => None,
    }
}

fn scale_case(
    tokens: usize,
    needle: &str,
    value: &str,
    last: bool,
    name: &str,
    warmups: usize,
    samples: usize,
) -> CaseResult {
    let text = fixtures::json_array(tokens);
    run_edit_case(
        name,
        &text,
        move |text, toggle| {
            if toggle {
                if last {
                    replace_last(text, value, needle)
                } else {
                    replace_first(text, value, needle)
                }
            } else if last {
                replace_last(text, needle, value)
            } else {
                replace_first(text, needle, value)
            }
        },
        tokens,
        warmups,
        samples,
    )
}

fn trivia_case(name: &str, warmups: usize, samples: usize) -> CaseResult {
    let text = fixtures::json_array(10_000);
    // Whitespace insertion at byte one shifts the suffix but never changes
    // a semantic token; the parser must stay cold.
    run_edit_case(
        name,
        &text,
        |text, toggle| {
            let at = if toggle { 2 } else { 1 };
            vec![SourceEdit::Insert {
                key: Span::point_uri(uri(), at).expect("point"),
                value: " ".into(),
            }]
        },
        10_000,
        warmups,
        samples,
    )
}

fn distant_case(name: &str, warmups: usize, samples: usize) -> CaseResult {
    let text = fixtures::json_array(10_000);
    run_edit_case(
        name,
        &text,
        |text, toggle| {
            let (head_from, head_to, tail_from, tail_to) = if toggle {
                ("\"u1\"", "\"s1\"", "false", "true")
            } else {
                ("\"s1\"", "\"u1\"", "true", "false")
            };
            let mut edits = replace_first(text, head_from, head_to);
            let second_at = text.rfind(tail_from).expect("tail target");
            edits.push(SourceEdit::Delete {
                key: Span::new_uri(uri(), second_at, second_at + tail_from.len()).unwrap(),
            });
            edits.push(SourceEdit::Insert {
                key: Span::point_uri(uri(), second_at).unwrap(),
                value: tail_to.into(),
            });
            edits
        },
        10_000,
        warmups,
        samples,
    )
}

fn two_documents_case(warmups: usize, samples: usize) -> CaseResult {
    let text_a = fixtures::json_array(4_096);
    let text_b = fixtures::json_array(4_096);
    let mut ws = build();
    let a_span = Span::new("test://doc-a", 0, 0).expect("doc-a span");
    let b_span = Span::new("test://doc-b", 0, 0).expect("doc-b span");
    ws.open(a_span.uri.clone(), &text_a).expect("open a");
    ws.open(b_span.uri.clone(), &text_b).expect("open b");
    // Equal-length replacement keeps every other byte offset stable.
    let at = text_a.find("\"s1\"").expect("sentinel present");
    let make_edits = |value: &str| {
        vec![
            SourceEdit::Delete {
                key: Span::new_uri(a_span.uri.clone(), at, at + 4).expect("range"),
            },
            SourceEdit::Insert {
                key: Span::point_uri(a_span.uri.clone(), at).expect("point"),
                value: value.into(),
            },
        ]
    };
    let mut toggle = false;
    let mut edit_samples: Vec<Sample> = Vec::with_capacity(samples);
    for index in 0..warmups + samples {
        let edits = if toggle {
            make_edits("\"s1\"")
        } else {
            make_edits("\"v7\"")
        };
        toggle = !toggle;
        let report = if index < warmups {
            ws.edit(edits).expect("edit a")
        } else {
            let start = Instant::now();
            let probe = AllocationProbe::start();
            let report = ws.edit(edits).expect("edit a");
            drop(probe);
            let elapsed = start.elapsed();
            let (count, bytes) = allocation_totals();
            black_box(&report);
            edit_samples.push(Sample { elapsed, alloc_count: count, alloc_bytes: bytes });
            report
        };
        black_box(&report);
        if edit_samples.len() == samples {
            break;
        }
    }
    // Apply the opposite of the last-applied value so the counters command
    // performs a real change.
    let final_report = ws.edit(make_edits(if toggle { "\"s1\"" } else { "\"v7\"" })).expect("counters");
    CaseResult {
        name: "two_documents_edit_a".into(),
        tokens: 4_096,
        samples: edit_samples.len(),
        median_us: median(&edit_samples).as_micros(),
        p95_us: p95(&edit_samples).as_micros(),
        alloc_count: total_allocs(&edit_samples),
        alloc_bytes: total_alloc_bytes(&edit_samples),
        work_json: work_counters_doc(&final_report, a_span.uri.to_string().as_str()),
    }
}

/// Shared edit-case driver: measures per-command latency/allocations while
/// oscillating equal-effect edits and refreshing the mirror from snapshots.
fn run_edit_case(
    name: &str,
    initial_text: &str,
    edit_builder: impl Fn(&str, bool) -> Vec<SourceEdit>,
    tokens: usize,
    warmups: usize,
    samples: usize,
) -> CaseResult {
    run_edit_case_with(build, name, initial_text, edit_builder, tokens, warmups, samples)
}

fn run_edit_case_with(
    build_workspace: fn() -> Workspace,
    name: &str,
    initial_text: &str,
    edit_builder: impl Fn(&str, bool) -> Vec<SourceEdit>,
    tokens: usize,
    warmups: usize,
    samples: usize,
) -> CaseResult {
    let load_samples = measure(
        || {
            let mut ws = build_workspace();
            ws.open(uri(), initial_text).expect("open commits")
        },
        warmups.min(3),
        samples.min(3),
    );
    black_box(&load_samples);

    let mut ws = build_workspace();
    ws.open(uri(), initial_text).expect("open commits");
    let mut mirror = initial_text.to_string();
    let mut toggle = false;
    let mut edit_samples: Vec<Sample> = Vec::with_capacity(samples);
    for index in 0..warmups + samples {
        let per_edit = edit_builder(&mirror, toggle);
        toggle = !toggle;
        let report = if index < warmups {
            ws.edit(per_edit).expect("edit commits")
        } else {
            let start = Instant::now();
            let probe = AllocationProbe::start();
            let report = ws.edit(per_edit).expect("edit commits");
            drop(probe);
            let elapsed = start.elapsed();
            let (count, bytes) = allocation_totals();
            black_box(&report);
            edit_samples.push(Sample { elapsed, alloc_count: count, alloc_bytes: bytes });
            report
        };
        black_box(&report);
        mirror = ws
            .snapshot()
            .observe::<SourceRevisions>(uri().to_string())
            .map(|revision| revision.text().chunks().collect())
            .unwrap_or_else(String::new);
        if edit_samples.len() == samples {
            break;
        }
    }

    let live_before = ws.snapshot().live_fact_count();
    let final_report = ws
        .edit(edit_builder(&mirror, toggle))
        .expect("final counters commit");
    let live_after = ws.snapshot().live_fact_count();

    CaseResult {
        name: name.to_string(),
        tokens,
        samples: edit_samples.len(),
        median_us: median(&edit_samples).as_micros(),
        p95_us: p95(&edit_samples).as_micros(),
        alloc_count: total_allocs(&edit_samples),
        alloc_bytes: total_alloc_bytes(&edit_samples),
        work_json: format!(
            "{{\"live_facts_before\":{live_before},\"live_facts_after\":{live_after},{}}}",
            work_counters_json(&final_report).trim_start_matches('{').trim_end_matches('}')
        ),
    }
}

fn stlc_case(name: &str, warmups: usize, samples: usize) -> CaseResult {
    let text = fixtures::stlc_program(64);
    run_edit_case_with(
        build_stlc,
        name,
        &text,
        |text, toggle| {
            if toggle {
                replace_first(text, " + 1", " + 0")
            } else {
                replace_first(text, " + 0", " + 1")
            }
        },
        64,
        warmups,
        samples,
    )
}

fn main() {
    // Child mode: PLINGO_STACK_PROBE_ELEMENTS runs only the stack probe.
    if let Ok(elements) = std::env::var("PLINGO_STACK_PROBE_ELEMENTS") {
        let parsed: usize = elements.parse().expect("probe element count");
        std::process::exit(stack_probe_child(parsed));
    }
    // Child mode: PLINGO_BENCH_CASE isolates one case in its own process so
    // baseline arena growth cannot accumulate across cases (plan §9
    // documents this pathology; Phase 9 removes it).
    if let Ok(case) = std::env::var("PLINGO_BENCH_CASE") {
        let warmups: usize = std::env::var("PLINGO_BENCH_WARMUPS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
        let samples: usize = std::env::var("PLINGO_BENCH_SAMPLES").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
        let result = run_single_case(&case, warmups, samples).unwrap_or_else(|| panic!("unknown case {case}"));
        println!("{}", result.to_json());
        return;
    }

    let warmups = std::env::var("PLINGO_BENCH_WARMUPS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    let samples = std::env::var("PLINGO_BENCH_SAMPLES").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
    let exe = std::env::current_exe().expect("current exe");

    let mut results = Vec::new();
    for name in CASE_NAMES {
        eprintln!("running case {name}");
        let output = std::process::Command::new(&exe)
            .env("PLINGO_BENCH_CASE", name)
            .env("PLINGO_BENCH_WARMUPS", warmups.to_string())
            .env("PLINGO_BENCH_SAMPLES", samples.to_string())
            .output()
            .expect("spawn case child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout.lines().last().unwrap_or("");
        if !output.status.success() || line.is_empty() {
            eprintln!("case {name} failed: {}\n{}", output.status, stdout);
            std::process::exit(1);
        }
        results.push(CaseResult::from_json(line));
    }

    // Stack-scale gate (item 16): attempt a far-larger flat document in a
    // subprocess so an abort cannot kill the harness. The recorded status is
    // the Phase 0 baseline evidence for the explicit stack-depth scale gate.
    let probe_elements = std::env::var("PLINGO_STACK_PROBE_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000usize);
    let probe_status = match std::process::Command::new(&exe)
        .env("PLINGO_STACK_PROBE_ELEMENTS", probe_elements.to_string())
        .output()
    {
        Ok(output) if output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            format!("{{\"status\": \"ok\", \"detail\": {stderr:?}}}")
        }
        Ok(output) => format!(
            "{{\"status\": \"aborted\", \"code\": {}}}",
            output.status.code().unwrap_or(-1)
        ),
        Err(error) => format!("{{\"status\": \"spawn_failed\", \"error\": {error:?}}}"),
    };
    results.push(CaseResult {
        name: "stack_scale_flat_json".into(),
        tokens: probe_elements,
        samples: 0,
        median_us: 0,
        p95_us: 0,
        alloc_count: 0,
        alloc_bytes: 0,
        work_json: probe_status,
    });

    emit(&results, std::env::var_os("PLINGO_BENCH_OUT").and_then(|p| p.into_string().ok()).as_deref());
}
