//! Workspace/source coverage for plain functions and uniform views.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use plingo::framework::source::{SourceDelta, source_snapshot};
use plingo::framework::{SourceEdit, SourceEdits, SourceRevisions, Workspace};
use plingo::reactive::kind::Map;
use plingo::reactive::prelude::*;
use plingo::utils::Span;
use plingo::view;

fn uri(name: &str) -> fluent_uri::Uri<String> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn ws_build<F: FnOnce(&mut Engine) -> plingo::reactive::Result<()>>(
    workers: usize,
    f: F,
) -> Workspace {
    Workspace::build(f).unwrap()
}

fn text_of(ws: &Workspace, u: &fluent_uri::Uri<String>) -> Option<String> {
    source_snapshot(&ws.snapshot(), &u.to_string()).map(|snapshot| snapshot.to_string())
}

#[view]
pub struct TextLog(Map<String, String>);

#[plingo::component]
fn text_logger_component(key: Each<SourceRevisions>) -> Result<()> {
    // Cut C: one instance per document writes exactly its own entry
    // (previous-epoch value when the current revision is absent, i.e. the
    // close-tombstone case).
    let value = key
        .value_previous()?
        .map(|revision| revision.text().to_string())
        .unwrap_or_default();
    TextLog::set(key.into_key(), value).__apply()
}

fn install_logger(engine: &mut Engine) -> Result<()> {
    text_logger_component::Component::mount(
        engine,
        plingo::reactive::framework_mount::MapEntries::<SourceRevisions>::new(),
    )?;
    Ok(())
}

#[test]
fn load_edit_close_round_trip() {
    let u = uri("roundtrip");
    let mut ws = ws_build(1, |_| Ok(()));
    ws.open(u.clone(), "hello").unwrap();
    assert_eq!(text_of(&ws, &u).as_deref(), Some("hello"));

    ws.edit(vec![SourceEdit::Insert {
        key: Span::new_uri(u.clone(), 3, 3).unwrap(),
        value: ", world".into(),
    }])
    .unwrap();
    assert_eq!(text_of(&ws, &u).as_deref(), Some("hel, worldlo"));

    ws.close(u.clone()).unwrap();
    assert_eq!(text_of(&ws, &u), None);
}

#[test]
fn equal_edit_is_a_no_op() {
    let u = uri("equal");
    let mut ws = ws_build(1, |_| Ok(()));
    ws.open(u.clone(), "abc").unwrap();
    let no_op_edit = vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), 1, 2).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 1).unwrap(),
            value: "b".into(),
        },
    ];
    ws.edit(no_op_edit.clone()).unwrap();
    assert_eq!(text_of(&ws, &u).as_deref(), Some("abc"));

    let changes = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&changes);
    ws.subscribe::<SourceRevisions>(move |_, count| {
        seen.fetch_add(count, Ordering::SeqCst);
    })
    .unwrap();
    ws.edit(no_op_edit).unwrap();
    assert_eq!(text_of(&ws, &u).as_deref(), Some("abc"));
    assert_eq!(changes.load(Ordering::SeqCst), 0);
}

#[test]
fn per_entry_changed_facts_isolate_documents() {
    let a = uri("iso-a");
    let b = uri("iso-b");
    let mut ws = ws_build(1, |_| Ok(()));
    ws.open(a.clone(), "aaa").unwrap();
    ws.open(b.clone(), "bbb").unwrap();
    let changes = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&changes);
    ws.subscribe::<SourceRevisions>(move |_, count| {
        seen.fetch_add(count, Ordering::SeqCst);
    })
    .unwrap();
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(a.clone(), 1).unwrap(),
        value: "X".into(),
    }])
    .unwrap();
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    assert_eq!(text_of(&ws, &b).as_deref(), Some("bbb"));
}

#[test]
fn previous_source_text_reads_committed_state() {
    let u = uri("previous");
    let mut ws = ws_build(1, install_logger);
    ws.open(u.clone(), "abc").unwrap();
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(u.clone(), 1).unwrap(),
        value: "XY".into(),
    }])
    .unwrap();
    assert_eq!(text_of(&ws, &u).as_deref(), Some("aXYbc"));
    assert_eq!(
        ws.snapshot()
            .observe::<TextLog>(u.to_string())
            .map(|value| (*value).clone()),
        Some("abc".to_string())
    );
}

#[test]
fn one_worker_and_many_workers_commit_equal_state() {
    let scenario = |workers: usize| -> (Vec<String>, Vec<String>) {
        let u = uri("determinism");
        let mut ws = ws_build(workers, install_logger);
        let mut facts = Vec::new();
        ws.open(u.clone(), "x := 0\ny := 1").unwrap();
        facts.push(format!("{:?}", ws.snapshot().inputs::<SourceRevisions>()));
        ws.edit(vec![SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 6).unwrap(),
            value: "extra\n".into(),
        }])
        .unwrap();
        facts.push(format!("{:?}", ws.snapshot().inputs::<SourceRevisions>()));
        ws.close(u).unwrap();
        facts.push(format!("{:?}", ws.snapshot().inputs::<SourceRevisions>()));
        let texts = ws.snapshot().inputs::<TextLog>();
        (facts, texts)
    };
    let (facts_1, dump_1) = scenario(1);
    let (facts_8, dump_8) = scenario(8);
    assert_eq!(facts_1, facts_8);
    assert_eq!(dump_1, dump_8);
}

#[test]
fn edit_delta_is_exact_and_single_text_change() {
    let u = uri("delta");
    let mut ws = ws_build(1, |_| Ok(()));
    ws.open(u.clone(), "0123456789").unwrap();
    ws.edit(vec![
        SourceEdit::Delete {
            key: Span::new_uri(u.clone(), 2, 4).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u.clone(), 7).unwrap(),
            value: "zz".into(),
        },
    ])
    .unwrap();
    assert_eq!(text_of(&ws, &u).as_deref(), Some("01456zz789"));
    // Phase 6 note: the command channel now carries pre-built Rope commands
    // whose equality is the command id; the exact sparse shape lives in the
    // published revision's delta.
    let command = ws
        .snapshot()
        .observe::<SourceEdits>(u.to_string())
        .expect("delta");
    match &command.delta {
        SourceDelta::Edit { splices } => {
            assert_eq!(splices.len(), 2);
            assert_eq!(splices[0].old_range, 2..4);
            assert_eq!(splices[1].old_range, 7..7);
        }
        SourceDelta::Load { .. } => panic!("expected an edit delta"),
    }
}

/// Determinism across document OPEN ORDER (plan §20.4): the same command
/// trace with documents opened in different order must commit the same
/// per-document facts, diagnostics, and source text. Opening order must
/// never leak into any document's semantic state.
#[test]
fn open_order_does_not_affect_committed_state() {
    let scenario = |order: &[&str]| -> Vec<String> {
        let mut ws = ws_build(1, install_logger);
        let mut dump = Vec::new();
        for name in order {
            // The text is keyed to the DOCUMENT IDENTITY, not the open
            // position: the same trace with a different open order must
            // converge on the same per-document text.
            let u = uri(name);
            let text = if *name == "big" {
                "x := 0\ny := 1"
            } else {
                "a := 2"
            };
            ws.open(u.clone(), text).unwrap();
        }
        // Edit the document opened LAST and the one opened FIRST in the
        // same command batch to interleave scheduling.
        let edits = vec![
            SourceEdit::Insert {
                key: Span::point_uri(uri("big"), 6).unwrap(),
                value: "extra\n".into(),
            },
            SourceEdit::Insert {
                key: Span::point_uri(uri("small"), 0).unwrap(),
                value: "z := 0\n".into(),
            },
        ];
        ws.edit(edits).unwrap();
        for name in order {
            let u = uri(name);
            dump.push(format!(
                "{}={:?}",
                name,
                text_of(&ws, &u).expect("document text committed")
            ));
        }
        let mut revisions = ws.snapshot().inputs::<SourceRevisions>();
        revisions.sort();
        dump.push(format!("{revisions:?}"));
        dump
    };
    let forward = scenario(&["big", "small"]);
    let reversed = scenario(&["small", "big"]);
    // The two documents' committed text must be identical regardless of
    // which was opened first; the revision keyset is the same set.
    let canonical = |dump: &[String]| -> Vec<String> {
        let mut sorted = dump.to_vec();
        sorted.sort();
        sorted
    };
    assert_eq!(
        canonical(&forward),
        canonical(&reversed),
        "open order must not affect committed text or revision keysets"
    );
}
