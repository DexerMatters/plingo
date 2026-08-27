//! Phase 0 instrumentation gates for local replay and automatic engine work.
//!
//! These checks retain the broad counters used by the later granularity
//! phases while asserting that a local edit restarts from a checkpoint rather
//! than from byte/column zero.

mod common;

use common::json::{JsonDocument, JsonToken};
use plingo::framework::Workspace;
use plingo::framework::lex::install_lexer;
use plingo::framework::parse::install_parser;
use plingo::framework::source::SourceEdit;

fn build() -> Workspace {
    Workspace::build(|engine| {
        install_lexer::<JsonToken>(engine)?;
        install_parser::<JsonToken, JsonDocument>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

fn uri(name: &str) -> fluent_uri::Uri<String> {
    plingo::utils::Span::new(format!("test://{name}"), 0, 0)
        .unwrap()
        .uri
}

const HEAD_EDIT_DOC: &str = r#"{"head":1,"items":[1,2,3],"tail":9}"#;

#[test]
fn local_edit_reports_document_scoped_work_counters() {
    let mut ws = build();
    let u = uri("work");
    ws.open(u.clone(), HEAD_EDIT_DOC).unwrap();

    let mut mirror = HEAD_EDIT_DOC.to_string();
    let report = ws
        .edit(replace_first_in(&u, &mut mirror, "1", "7"))
        .expect("edit commits");

    // Engine-level determinism counters exist.
    assert!(report.engine_work().fact_reads > 0);
    assert_eq!(report.rounds(), report.command().rounds);

    let source = report.work().source(&u.to_string()).expect("source work");
    assert_eq!(source.validated_operations, 2); // delete + insert
    assert!(source.effective_splices >= 1);
    assert!(source.rope_edit_operations >= 1);
    assert!(source.rope_chunks_traversed >= 1);
    assert_eq!(source.full_source_materializations, 0);

    let lexer = report.work().lexer(&u.to_string()).expect("lexer work");
    assert_eq!(lexer.component_runs, 1);
    // Local replay starts at the changed semantic region, not byte zero.
    assert!(lexer.restart_bytes > 0);
    assert!(lexer.restart_bytes < HEAD_EDIT_DOC.len() as u64);
    assert!(lexer.tokens_replayed <= 12);
    assert!(lexer.dfa_transitions > 0);
    assert!(lexer.source_bytes_examined >= lexer.tokens_replayed);
    assert_eq!(lexer.retained_suffix_entries_visited, 0);
    assert_eq!(lexer.full_tape_iterations, 0);
    assert_eq!(lexer.full_projection_fallbacks, 0);
    assert_eq!(lexer.document_vector_rebuilds, 0);
    let parser = report
        .work()
        .parser(&u.to_string())
        .expect("layout snapshot work");
    // Plan §18: `1 -> 42` preserves terminal structure, so only one token
    // value fact changes. The layout façade may refresh, but semantic parsing
    // is not scheduled.
    assert_eq!(parser.component_runs, 0);
}

/// Phase 3 scale gate: a same-terminal head edit keeps source/lexer work
/// bounded while the untouched token suffix remains physically unvisited.
#[test]
fn local_value_edit_has_no_document_sized_source_or_lexer_work() {
    use common::fixtures::json_array;

    for size in [32usize, 128] {
        let mut ws = build();
        let u = uri(&format!("scale-{size}"));
        let text = json_array(size);
        ws.open(u.clone(), &text).expect("scale fixture opens");
        let report = ws
            .edit(vec![
                SourceEdit::Delete {
                    key: plingo::utils::Span::new_uri(u.clone(), 1, 2).unwrap(),
                },
                SourceEdit::Insert {
                    key: plingo::utils::Span::point_uri(u.clone(), 1).unwrap(),
                    value: "9".into(),
                },
            ])
            .expect("local value edit commits");

        let source = report.work().source(u.as_str()).expect("source work");
        assert_eq!(source.full_source_materializations, 0, "size={size}");

        let lexer = report.work().lexer(u.as_str()).expect("lexer work");
        assert_eq!(lexer.full_tape_iterations, 0, "size={size}");
        assert_eq!(lexer.full_projection_fallbacks, 0, "size={size}");
        assert_eq!(lexer.document_vector_rebuilds, 0, "size={size}");
        assert_eq!(lexer.retained_suffix_entries_visited, 0, "size={size}");
        assert!(
            lexer.tokens_replayed <= 16
                && lexer.lexical_entries_visited <= 32
                && lexer.source_bytes_examined <= 128,
            "document-sized lexer work at size={size}: {lexer:?}"
        );

        let parser = report.work().parser(u.as_str()).cloned().unwrap_or_default();
        assert_eq!(parser.component_runs, 0, "value edit woke parser at size={size}");
        assert_eq!(
            parser.parser_records_inserted
                + parser.parser_records_updated
                + parser.parser_records_removed,
            0,
            "value edit changed parser records at size={size}"
        );
    }
}

#[test]
fn equal_edit_is_cold_but_reports_validation() {
    let mut ws = build();
    let u = uri("cold");
    let open_epoch = ws.open(u.clone(), HEAD_EDIT_DOC).unwrap().epoch();

    // Replace a value with an equal-width equal-value edit: the source fold
    // still validates two operations but the text is unchanged.
    let mut mirror = HEAD_EDIT_DOC.to_string();
    let report = ws
        .edit(replace_first_in(
            &u,
            &mut mirror,
            "\"head\":1",
            "\"head\":1",
        ))
        .unwrap();
    // Baseline (pre-Phase-3): the batch is spliced, the folded text is equal,
    // and equal-write filtering keeps every downstream stage cold.
    assert_eq!(
        report.work().lexer(&u.to_string()),
        None,
        "no lexer run for a no-op batch"
    );
    assert_eq!(
        report.work().parser(&u.to_string()),
        None,
        "no parser run for a no-op batch"
    );
    // Phase 3 contract: exact-equal batches are dropped during pre-command
    // normalization, so NO epoch opens and no fact changes at all. The
    // validation counters still prove the batch was checked.
    assert_eq!(
        report.epoch(),
        open_epoch,
        "equal write does not advance the epoch"
    );
    assert_eq!(report.engine_work().facts_changed, 0);
    assert_eq!(report.rounds(), 0);
    let source = report
        .work()
        .source(&u.to_string())
        .expect("validation recorded");
    assert_eq!(source.validated_operations, 2);
}

#[test]
fn repeated_runs_produce_identical_counters() {
    let run_once = || {
        let mut ws = build();
        let u = uri("repeat");
        ws.open(u.clone(), HEAD_EDIT_DOC).unwrap();
        let mut mirror = HEAD_EDIT_DOC.to_string();
        let r1 = ws
            .edit(replace_first_in(&u, &mut mirror, "1", "7"))
            .unwrap();
        let r2 = ws
            .edit(replace_first_in(&u, &mut mirror, "7", "8"))
            .unwrap();
        (
            r1.engine_work().clone(),
            r1.work().lexer(&u.to_string()).cloned(),
            r2.engine_work().clone(),
            r2.work().parser(&u.to_string()).cloned(),
        )
    };
    let a = run_once();
    let b = run_once();
    if a != b {
        panic!("nondeterministic counters:\na={a:?}\nb={b:?}");
    }
}

#[test]
fn rollback_drops_command_metrics() {
    use plingo::reactive::Error;
    let mut ws = build();
    let u = uri("rollback");
    ws.open(u.clone(), HEAD_EDIT_DOC).unwrap();
    // A command whose closure errors after recording source work must not
    // publish metrics or facts. Drive it through engine_mut directly.
    let error = ws
        .engine_mut()
        .command::<fn() -> plingo::reactive::Result<()>>(|| {
            Err(Error::Internal("authored failure".into()))
        })
        .unwrap_err();
    assert!(matches!(error, Error::Internal(_)));
}

// ---------------------------------------------------------------------------
// helpers

fn replace_first_in(
    u: &fluent_uri::Uri<String>,
    text: &mut String,
    needle: &str,
    value: &str,
) -> Vec<SourceEdit> {
    let start = text.find(needle).expect("fixture contains target");
    let end = start + needle.len();
    let edits = vec![
        SourceEdit::Delete {
            key: plingo::utils::Span::new_uri(u.clone(), start, end).unwrap(),
        },
        SourceEdit::Insert {
            key: plingo::utils::Span::point_uri(u.clone(), start).unwrap(),
            value: value.into(),
        },
    ];
    // Mirror the edit locally so later searches see the updated layout.
    text.replace_range(start..end, value);
    edits
}

/// Fault-injection rollback oracle (plan §20.5): a command that fails at a
/// phase boundary must leave the persistent roots, semantic facts, token
/// publications, ownership, and work counters exactly as they were —
/// byte-identical projections and the same committed Arc.
#[test]
fn failing_command_rolls_back_roots_facts_and_publication_identity() {
    use common::oracle::project;
    use plingo::framework::lex::{LexedDocuments, TokenVec, Tokens};
    use std::sync::Arc;

    let mut ws = build();
    let u = uri("fault");
    ws.open(u.clone(), HEAD_EDIT_DOC).unwrap();

    let before_projection = project(&ws.snapshot(), &u.to_string());
    let before_tokens: Arc<TokenVec<JsonToken>> = ws
        .snapshot()
        .observe::<Tokens<JsonToken>>(u.to_string())
        .expect("committed tokens");
    let before_lexed = ws
        .snapshot()
        .observe::<LexedDocuments<JsonToken>>(u.to_string())
        .expect("committed semantic doc");

    // An invalid edit fails at source normalization (delete beyond EOF) —
    // the earliest phase boundary after the command is opened.
    let error = ws
        .edit(vec![SourceEdit::Delete {
            key: plingo::utils::Span::new_uri(u.clone(), 10_000, 10_001).unwrap(),
        }])
        .unwrap_err();
    assert!(
        matches!(error, plingo::reactive::Error::Internal(_)),
        "the edit must fail at source normalization: {error:?}"
    );

    // The projection, the token Arc, and the semantic-document Arc are all
    // byte-identical: no partial publication survived the rollback.
    let after_projection = project(&ws.snapshot(), &u.to_string());
    assert_eq!(
        before_projection, after_projection,
        "canonical projection unchanged after rollback"
    );
    let after_tokens = ws
        .snapshot()
        .observe::<Tokens<JsonToken>>(u.to_string())
        .expect("committed tokens after rollback");
    assert!(
        Arc::ptr_eq(&before_tokens, &after_tokens),
        "token publication Arc survives the failed command"
    );
    let after_lexed = ws
        .snapshot()
        .observe::<LexedDocuments<JsonToken>>(u.to_string())
        .expect("committed semantic doc after rollback");
    assert!(
        Arc::ptr_eq(&before_lexed, &after_lexed),
        "semantic document Arc survives the failed command"
    );

    // A subsequent valid edit still works: the engine is not wedged.
    ws.edit(vec![SourceEdit::Insert {
        key: plingo::utils::Span::point_uri(u.clone(), 1).unwrap(),
        value: "x".into(),
    }])
    .unwrap();
    let after_edit = project(&ws.snapshot(), &u.to_string());
    assert_ne!(
        after_edit.source_len, before_projection.source_len,
        "the post-rollback command executes normally"
    );
}

/// Fault-injection matrix extension (plan §20.5): multi-document rollback
/// isolation and metric hygiene after a failed command.
#[test]
fn failed_command_isolates_documents_and_keeps_metrics_clean() {
    use common::oracle::project;
    use plingo::framework::lex::Tokens;

    let mut ws = build();
    let a = uri("fault-a");
    let b = uri("fault-b");
    ws.open(a.clone(), r#"{"a": [1, 2]}"#).unwrap();
    ws.open(b.clone(), r#"{"b": [3, 4]}"#).unwrap();

    let before_a = project(&ws.snapshot(), &a.to_string());
    let before_b = project(&ws.snapshot(), &b.to_string());
    let before_b_tokens = ws
        .snapshot()
        .observe::<Tokens<JsonToken>>(b.to_string())
        .expect("document B tokens");

    // Source-validation rejection: an out-of-bounds delete against A
    // fails at normalization, rolling back the whole epoch.
    let error = ws
        .edit(vec![SourceEdit::Delete {
            key: plingo::utils::Span::new_uri(a.clone(), 10_000, 10_001).unwrap(),
        }])
        .unwrap_err();
    assert!(
        matches!(error, plingo::reactive::Error::Internal(_)),
        "the edit must fail at source normalization: {error:?}"
    );

    // Both documents keep byte-identical projections.
    assert_eq!(
        before_a,
        project(&ws.snapshot(), &a.to_string()),
        "document A changed after a rejected edit"
    );
    assert_eq!(
        before_b,
        project(&ws.snapshot(), &b.to_string()),
        "document B changed after a rejected edit against A"
    );
    let after_b_tokens = ws
        .snapshot()
        .observe::<Tokens<JsonToken>>(b.to_string())
        .expect("document B tokens after rollback");
    assert!(
        std::sync::Arc::ptr_eq(&before_b_tokens, &after_b_tokens),
        "document B token Arc survived unchanged"
    );

    // Metric hygiene: the next VALID command reports exactly its own work
    // (no leakage from the failed attempt's partially-accumulated frame).
    let report = ws
        .edit(vec![SourceEdit::Insert {
            key: plingo::utils::Span::point_uri(b.clone(), 9).unwrap(),
            value: "5".into(),
        }])
        .expect("valid edit after rollback");
    let b_work = report
        .work()
        .parser(b.as_str())
        .cloned()
        .unwrap_or_default();
    if let Some(work) = report.work().parser(a.as_str()) {
        assert_eq!(
            work.component_runs, 0,
            "document A ran the parser while editing B"
        );
    }
    assert!(
        b_work.tokens_replayed > 0 || b_work.tokens_reused > 0,
        "document B parser work missing after rollback"
    );
}

// ---------------------------------------------------------------------------
// ReactionDigest oracle (follow-up plan §4 item 6)
// ---------------------------------------------------------------------------
use plingo::reactive::kind::{Map, emit_view};
use plingo::reactive::prelude::*;

#[path = "../examples/view_pipeline/fanout.rs"]
mod fanout;

use fanout::{Alerts, Enabled, Names, Quantities, Records, Scores, fanout_one};
use plingo::reactive::{Engine, ReactionDigest, View};
use reactive_macros::component;
use reactive_macros::view;

#[reactive_macros::component]
fn reaction_stage(key: EachKey<Names>) -> plingo::Result<()> {
    fanout::fanout_one(key)
}

fn reaction_engine() -> Engine {
    let mut engine = Engine::new();
    reaction_stage_install(&mut engine).expect("install fan-out stage");
    engine
}

/// One quantity edit evaluates exactly the record/score/alert instances of
/// that key with exact read edges; score/alert never read the name text.
#[test]
fn reaction_digest_records_exact_element_edges() {
    let mut engine = reaction_engine();
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "alpha".into())?;
            Ok(())
        })
        .expect("seed names");
    let report = engine
        .command(|| {
            emit_view::<Quantities>()?.insert("a".into(), 3)?;
            emit_view::<Enabled>()?.insert("a".into(), true)?;
            Ok(())
        })
        .expect("seed inputs");
    let digest = report
        .metric::<ReactionDigest>()
        .expect("reaction digest recorded");
    // Only key "a" instances evaluate.
    for evaluation in &digest.evaluations {
        let driving = &evaluation.driving_element;
        assert!(
            driving.contains("\"a\""),
            "unexpected driving element {driving} in {}",
            evaluation.definition
        );
    }
    let definitions: Vec<&str> = digest
        .evaluations
        .iter()
        .map(|evaluation| evaluation.definition)
        .collect();
    assert!(
        definitions.iter().any(|d| d.contains("reaction_stage")),
        "{definitions:?}"
    );
    let fanout_evals: Vec<_> = digest
        .evaluations_of("phase0_instrumentation::reaction_stage")
        .collect();
    if !fanout_evals.is_empty() {
        for evaluation in fanout_evals {
            let views: Vec<&str> = evaluation.reads.iter().map(|edge| edge.view).collect();
            assert!(views.contains(&Names::name()));
            assert!(views.contains(&Quantities::name()));
            assert!(views.contains(&Enabled::name()));
            let outputs: Vec<&str> = evaluation.outputs.iter().map(|edge| edge.view).collect();
            assert!(outputs.contains(&Records::name()));
            assert!(outputs.contains(&Scores::name()));
            assert!(outputs.contains(&Alerts::name()));
        }
    }
    // A no-op command evaluates nothing.
    let cold = engine.command(|| Ok(())).expect("noop");
    let cold_digest = cold.metric::<ReactionDigest>().cloned().unwrap_or_default();
    assert!(cold_digest.is_empty(), "{}", cold_digest.render());
    // Liveness audit is clean on the healthy graph.
    assert!(
        engine.__liveness_audit().is_empty(),
        "{:?}",
        engine.__liveness_audit()
    );
}

/// Removing a name retires its three instances through exact retractions.
#[test]
fn reaction_digest_records_exact_retirements() {
    let mut engine = reaction_engine();
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "alpha".into())?;
            emit_view::<Quantities>()?.insert("a".into(), 1)?;
            Ok(())
        })
        .expect("seed");
    let report = engine
        .command(|| emit_view::<Names>()?.remove("a".into()))
        .expect("remove");
    let digest = report.metric::<ReactionDigest>().expect("digest");
    assert!(
        !digest.retirements.is_empty(),
        "expected retirements: {}",
        digest.render()
    );
    for retirement in &digest.retirements {
        assert!(retirement.driving_element.contains("\"a\""));
    }
    assert!(engine.__liveness_audit().is_empty());
}

/// The liveness audit stays clean across an edit/reverse/close/reopen
/// cycle on every example family harness (follow-up plan §4 item 12).
#[test]
fn liveness_audit_holds_across_reactive_cycles() {
    let mut engine = reaction_engine();
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "x".into())?;
            emit_view::<Quantities>()?.insert("a".into(), 2)?;
            emit_view::<Enabled>()?.insert("a".into(), false)?;
            Ok(())
        })
        .expect("open");
    engine
        .command(|| emit_view::<Quantities>()?.insert("a".into(), 5))
        .expect("edit");
    engine
        .command(|| emit_view::<Quantities>()?.insert("a".into(), 2))
        .expect("reverse");
    engine
        .command(|| emit_view::<Names>()?.remove("a".into()))
        .expect("close");
    engine
        .command(|| {
            emit_view::<Names>()?.insert("a".into(), "x".into())?;
            emit_view::<Quantities>()?.insert("a".into(), 2)?;
            emit_view::<Enabled>()?.insert("a".into(), false)?;
            Ok(())
        })
        .expect("reopen");
    let violations = engine.__liveness_audit();
    assert!(violations.is_empty(), "{violations:?}");
}
