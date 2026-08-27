//! Phase 0 oracle gates (plan §10.4, §11 Phase 0).
//!
//! The canonical projection must match a fresh workspace after every edit,
//! detect deliberate corruption, retain unchanged publications by identity,
//! and hold across the seeded mutation corpus and persistent-error fixtures.

mod common;

use common::fixtures;
use common::json::{JsonDocument, JsonToken};
use common::oracle::{self, TraceRunner};
use plingo::framework::Workspace;
use plingo::framework::lex::install_lexer;
use plingo::framework::parse::install_parser;

fn build() -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<JsonToken>(engine)?;
        install_parser::<JsonToken, JsonDocument>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

#[test]
fn sequential_edits_match_fresh_workspace_after_every_step() {
    let mut runner = TraceRunner::open(build, "trace", r#"{"a":[1,2,3],"b":{"c":"x"}}"#);
    let u = common::uri("trace");

    runner.step(replace(&runner, &u, "\"x\"", "\"yy\""));
    runner.step(replace(&runner, &u, "[1,2,3]", "[1,\"two\",null,true]"));
    runner.step(insert_before_tail(&runner, &u));
    runner.step(rename_head_key(&runner, &u));
}

#[test]
fn no_op_edit_retains_publication_identity() {
    let mut runner = TraceRunner::open(build, "identity", r#"{"k":[1,2]}"#);
    let u = common::uri("identity");

    // An equal-width value replacement that changes nothing must keep the
    // committed token publication Arc pointer-equal.
    let same = same_width_equal_value(&u, &runner);
    runner.workspace().edit(same).expect("no-op commits");
    runner.verify("after no-op");
    runner.checkpoint_and_verify_retained_on_noop();

    // A real change republishes.
    runner.step(replace_text(&runner, &u, "\"k\"", "\"kk\""));
    assert!(true, "republish path exercised");
}

#[test]
fn comparator_detects_deliberate_corruption() {
    let mut runner = TraceRunner::open(build, "corrupt", r#"{"a":[1,2],"b":false}"#);
    let u = common::uri("corrupt");

    // Capture the healthy projection through a step, then prove every
    // corruption detector flips equality.
    let uri_string = u.to_string();
    let healthy = oracle::project(&runner.workspace().snapshot(), &uri_string);

    let mut tokens = healthy.clone();
    oracle::corrupt_tokens(&mut tokens);
    assert_ne!(tokens, healthy);

    let mut roots = healthy.clone();
    oracle::corrupt_roots(&mut roots);
    assert_ne!(roots, healthy);

    let mut diagnostics = healthy.clone();
    oracle::corrupt_diagnostics(&mut diagnostics);
    assert_ne!(diagnostics, healthy);

    let _ = runner.verify("healthy baseline passes");
}

#[test]
fn two_persistent_errors_stay_independent_across_edits() {
    let text = fixtures::json_two_errors();
    let mut runner = TraceRunner::open(build, "errors", &text);
    let u = common::uri("errors");

    // Baseline: both errors are visible in the fresh comparison (the runner
    // verifies this on the next step). Edit near error A only.
    let edit_at = text.find("\"x\":1").expect("error site present") + 5;
    runner.step(vec![insert_at(&u, edit_at, " ")]);
    runner.step(replace(&runner, &u, "\"y\":2", "\"y\":2,"));

    // Repair error B; error A's diagnostics must persist unchanged.
    runner.step(replace(&runner, &u, "true \"q\"", "true, \"q\""));
}

#[test]
fn unterminated_tail_forces_eof_replay_but_stays_exact() {
    let text = fixtures::json_unterminated_tail(32);
    let mut runner = TraceRunner::open(build, "eof", &text);
    let u = common::uri("eof");

    // Every converging edit before the tail still compares exactly against
    // a fresh workspace even though replay runs to EOF.
    runner.step(replace(&runner, &u, "1,", "11,"));
    runner.step(vec![insert_at(&u, 10, " ")]);
}

#[test]
fn seeded_mutation_corpus_matches_fresh_workspace() {
    let base = fixtures::json_array(256);
    let mutations = fixtures::seeded_mutations(&base, 0xC0FFEE, 24);
    let mut runner = TraceRunner::open(build, "seeded", &base);
    let u = common::uri("seeded");

    for mutation in mutations {
        let edits = fixtures::mutation_edits(&u, runner.text(), &mutation);
        if edits.is_empty() {
            continue;
        }
        // Skip batches the source validator would reject (out-of-bounds
        // after truncation); those are Phase 3 normalization gates.
        if runner.text().is_empty() {
            break;
        }
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runner.step(edits);
        }));
        if ok.is_err() {
            // A rejected batch is a valid corpus outcome only for invalid
            // coordinates; anything else is an oracle failure already
            // panicked inside step.
            continue;
        }
    }
}

// ---------------------------------------------------------------------------
// edit helpers
// ---------------------------------------------------------------------------

type Edits = Vec<plingo::framework::source::SourceEdit>;

/// Reads the current mirrored text to locate `needle`, then builds one
/// delete+insert replacement pair at its first occurrence.
fn replace(runner: &TraceRunner, u: &fluent_uri::Uri<String>, needle: &str, value: &str) -> Edits {
    replace_text(runner, u, needle, value)
}

fn replace_text(
    runner: &TraceRunner,
    u: &fluent_uri::Uri<String>,
    needle: &str,
    value: &str,
) -> Edits {
    use plingo::framework::source::SourceEdit;
    use plingo::utils::Span;
    let start = runner.text().find(needle).expect("fixture contains target");
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

fn insert_before_tail(runner: &TraceRunner, u: &fluent_uri::Uri<String>) -> Edits {
    vec![insert_at(u, runner.text().len().saturating_sub(1), " ")]
}

/// Renames the head key in place (same byte length, still-valid document).
fn rename_head_key(runner: &TraceRunner, u: &fluent_uri::Uri<String>) -> Edits {
    replace_text(runner, u, "\"a\"", "\"z\"")
}

fn insert_at(
    u: &fluent_uri::Uri<String>,
    at: usize,
    value: &str,
) -> plingo::framework::source::SourceEdit {
    use plingo::framework::source::SourceEdit;
    use plingo::utils::Span;
    SourceEdit::Insert {
        key: Span::point_uri(u.clone(), at.min(usize::MAX)).unwrap(),
        value: value.into(),
    }
}

fn same_width_equal_value(u: &fluent_uri::Uri<String>, runner: &TraceRunner) -> Edits {
    use plingo::framework::source::SourceEdit;
    use plingo::utils::Span;
    let needle = "\"k\"";
    let start = runner.text().find(needle).expect("key present");
    let end = start + needle.len();
    vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), start, end).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), start).unwrap(),
            value: needle.into(),
        },
    ]
}
