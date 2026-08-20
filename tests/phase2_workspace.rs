//! Phase 2 acceptance — workspace + source (plan §8.1, §8.4).
//!
//! Load/edit/close round-trips, equal-edit no-op, delta exactness,
//! `Previous<SourceText>` correctness, and 1-vs-N determinism.

use plingo::framework::{SourceEdit, SourceEdits, SourceText, Workspace};
use plingo::reactive::prelude::*;
use plingo::reactive_component as component;
use plingo::reactive_view as view;
use plingo::utils::Span;

fn uri(name: &str) -> fluent_uri::Uri<&'static str> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn ws_build<F: FnOnce(&mut Engine) -> plingo::reactive::Result<()>>(
    workers: usize,
    f: F,
) -> Workspace {
    Workspace::build_with(workers, f).unwrap()
}

fn text_of(ws: &Workspace, u: fluent_uri::Uri<&'static str>) -> Option<String> {
    ws.snapshot()
        .map_view::<SourceText>()
        .get(&u.to_string())
        .map(|value| value.to_string())
}

// ---------------------------------------------------------------------------
// The Previous<SourceText> logger (installed in the Previous test)
// ---------------------------------------------------------------------------

/// Logs each committed document's text from the previous epoch.
#[view(map, key = String, value = String)]
pub struct TextLog;

#[component]
pub fn text_logger(text: Previous<SourceText>) -> (TextLog,) {
    let out = Emitted::<TextLog>::new()?;
    for key in text.keys()? {
        let value = text.get(&key)?.map(|v| (*v).to_string()).unwrap_or_default();
        out.set(key.clone(), value)?;
    }
    Ok((out,))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn load_edit_close_round_trip() {
    let u = uri("roundtrip");
    let mut ws = ws_build(1, |_| Ok(()));
    ws.open(u, "hello").unwrap();
    assert_eq!(text_of(&ws, u).as_deref(), Some("hello"));

    ws.edit(vec![SourceEdit::Insert {
        key: Span::new_uri(u, 3, 3).unwrap(),
        value: ", world".into(),
    }])
    .unwrap();
    assert_eq!(text_of(&ws, u).as_deref(), Some("hel, worldlo"));

    ws.close(u).unwrap();
    assert_eq!(text_of(&ws, u), None, "closing retracts the text entry");
}

#[test]
fn equal_edit_is_a_no_op() {
    let u = uri("equal");
    let mut ws = ws_build(1, |_| Ok(()));
    ws.open(u, "abc").unwrap();

    // This edit deletes "b" and re-inserts it: the folded text equals the
    // committed text, so the fold must propagate zero facts.
    let no_op_edit = vec![
        SourceEdit::Delete {
            key: Span::new_uri(u, 1, 2).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u, 1).unwrap(),
            value: "b".into(),
        },
    ];
    ws.edit(no_op_edit.clone()).unwrap();
    assert_eq!(text_of(&ws, u).as_deref(), Some("abc"));

    // Re-applying the equal edit keeps the epoch silently text-wise: the
    // SourceText entry is unchanged. (The fold may run; it publishes
    // nothing — T4.)
    ws.engine_mut()
        .subscribe::<SourceText>(Box::new(|changes| {
            panic!("no SourceText changes expected after an equal edit: {changes:?}");
        }))
        .unwrap();
    ws.edit(no_op_edit).unwrap();
    assert_eq!(text_of(&ws, u).as_deref(), Some("abc"));
}

#[test]
fn per_entry_changed_facts_isolate_documents() {
    let a = uri("iso-a");
    let b = uri("iso-b");
    let mut ws = ws_build(1, |_| Ok(()));
    ws.open(a, "aaa").unwrap();
    ws.open(b, "bbb").unwrap();

    let mut engine = ws.engine_mut();
    // Drive one more edit through the engine to read the report.
    let report = engine
        .command(vec![ExternalOp::map_set::<SourceEdits>(
            a.to_string(),
            plingo::framework::SourceDelta {
                replace: false,
                splices: std::sync::Arc::from([plingo::framework::SourceSplice {
                    old_range: 1..1,
                    new_range: 1..2,
                    removed: "".into(),
                    inserted: "X".into(),
                }]),
            },
        )])
        .unwrap();
    let changed_names: Vec<&str> = report.changed().iter().map(|c| c.view_name).collect();
    assert!(
        changed_names.iter().any(|n| n.contains("SourceText")),
        "the target document's text changed"
    );
    // No change touches document B's key.
    for change in report.changed() {
        let key = format!("{:?}", change.key);
        assert!(
            !key.contains("iso-b"),
            "document B must be untouched: {key}"
        );
    }
    let _ = engine;
    let _ = ws;
}

#[test]
fn previous_source_text_reads_committed_state() {
    let u = uri("previous");
    let mut ws = ws_build(1, |engine| {
        engine.install(text_logger)?;
        Ok(())
    });
    ws.open(u, "abc").unwrap();
    // The logger's Previous readers run at the start of the next epoch,
    // against the epoch-1 committed text.
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u, 1).unwrap(),
        value: "XY".into(),
    }])
    .unwrap();
    assert_eq!(text_of(&ws, u).as_deref(), Some("aXYbc"));
    let log = ws
        .snapshot()
        .map_view::<TextLog>()
        .get(&u.to_string())
        .map(|v| (*v).clone());
    assert_eq!(
        log.as_deref(),
        Some("abc"),
        "the logger saw the committed pre-edit text"
    );
}

#[test]
fn one_worker_and_many_workers_commit_equal_state() {
    let scenario = |workers: usize| -> (Vec<String>, String) {
        let u = uri("determinism");
        let mut ws = ws_build(workers, |engine| {
            engine.install(text_logger)?;
            Ok(())
        });
        let mut facts = Vec::new();
        ws.open(u, "x := 0\ny := 1").unwrap();
        facts.push(format!("{:?}", ws.snapshot().map_view::<SourceText>().keys()));
        ws.edit(vec![SourceEdit::Insert {
            key: Span::point_uri(u, 6).unwrap(),
            value: "extra\n".into(),
        }])
        .unwrap();
        facts.push(format!("{:?}", ws.snapshot().map_view::<SourceText>().keys()));
        ws.close(u).unwrap();
        facts.push(format!("{:?}", ws.snapshot().map_view::<SourceText>().keys()));
        let snapshot_dump = format!("{:?}", ws.snapshot().map_view::<SourceText>().keys());
        (facts, snapshot_dump)
    };

    let (facts_1, dump_1) = scenario(1);
    let (facts_8, dump_8) = scenario(8);
    assert_eq!(facts_1, facts_8, "1 and N workers publish equal state");
    assert_eq!(dump_1, dump_8);
}

#[test]
fn edit_delta_is_exact_and_single_text_change() {
    let u = uri("delta");
    let mut ws = ws_build(1, |_| Ok(()));
    ws.open(u, "0123456789").unwrap();
    ws.edit(vec![
        SourceEdit::Delete {
            key: Span::new_uri(u, 2, 4).unwrap(), // "23"
        },
        SourceEdit::Insert {
            key: Span::point_uri(u, 7).unwrap(),
            value: "zz".into(),
        },
    ])
    .unwrap();
    // Delete 2..4 then insert at original point 7 (after '6'), which
    // shifted left by 2 → index 5 in the folded document.
    assert_eq!(text_of(&ws, u).as_deref(), Some("01456zz789"));
    let delta = ws
        .snapshot()
        .map_view::<SourceEdits>()
        .get(&u.to_string())
        .expect("delta entry");
    assert_eq!(delta.splices.len(), 2, "one exact splice per edit");
    assert_eq!(delta.splices[0].old_range, 2..4);
    assert_eq!(delta.splices[1].old_range, 5..5);
    assert!(!delta.replace, "edits are spliced, not replaced");
}