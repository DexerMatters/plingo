use super::*;
use crate::{
    Graph,
    scheme::node::{ReadGraph, ViewUpdate},
    utils::Span,
};

#[test]
fn source_edits_reject_non_boundary_offsets() {
    let uri = Span::new("test://node-source-boundary", 0, 0).unwrap().uri;
    let mut graph = Graph::new();
    graph.command(SourceInput::load(uri)).unwrap();
    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: "α".into(),
        }))
        .unwrap();

    let error = graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 1).unwrap(),
            value: "x".into(),
        }))
        .expect_err("a byte inside a UTF-8 code point must not be rounded");
    assert!(error.to_string().contains("UTF-8 boundary"));
    assert_eq!(graph.get::<DocumentText>(uri).as_deref(), Some("α"));
}

#[test]
fn batched_replacements_publish_disjoint_revision_splices() {
    let uri = Span::new("test://node-source-splices", 0, 0).unwrap().uri;
    let base = "head=11111;middle;tail=22222";
    let mut graph = Graph::new();
    graph.command(SourceInput::load(uri)).unwrap();
    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: base.into(),
        }))
        .unwrap();
    let first = base.find("11111").unwrap();
    let second = base.find("22222").unwrap();
    graph
        .command(SourceInput::apply_all(vec![
            SourceEdit::Delete {
                key: Span::new_uri(uri, first, first + 5).unwrap(),
            },
            SourceEdit::Insert {
                key: Span::point_uri(uri, first).unwrap(),
                value: "54321".into(),
            },
            SourceEdit::Delete {
                key: Span::new_uri(uri, second, second + 5).unwrap(),
            },
            SourceEdit::Insert {
                key: Span::point_uri(uri, second).unwrap(),
                value: "76543".into(),
            },
        ]))
        .unwrap();

    let delta = graph.get::<DocumentChange>(uri).unwrap();
    assert_eq!(delta.splices.len(), 2);
    assert_eq!(delta.splices[0].old_range, first..first + 5);
    assert_eq!(delta.splices[0].inserted.as_ref(), "54321");
    assert_eq!(delta.splices[1].old_range, second..second + 5);
    assert_eq!(delta.splices[1].inserted.as_ref(), "76543");
    assert_eq!(
        graph.get::<DocumentText>(uri).as_deref(),
        Some("head=54321;middle;tail=76543")
    );
}

#[test]
fn input_node_schema_declares_document_ports() {
    let mut graph = Graph::new();
    graph.install_input::<SourceInput>().unwrap();
    let schema = graph
        .schemas()
        .into_iter()
        .find(|schema| schema.provider == std::any::type_name::<SourceInput>())
        .expect("source input schema is installed");
    assert!(schema.declares_map::<DocumentText>());
    assert!(schema.declares_map::<DocumentChange>());
}

#[test]
fn source_node_commands_are_versioned_and_subscribable() {
    let uri = Span::new("test://node-source", 0, 0).unwrap().uri;
    let mut graph = Graph::new();
    graph.command(SourceInput::load(uri)).unwrap();
    assert_eq!(graph.get::<DocumentText>(uri).as_deref(), Some(""));

    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: "α".into(),
        }))
        .unwrap();
    let snapshot = graph.snapshot();
    let subscription = graph
        .subscribe::<DocumentText>(uri)
        .expect("loaded source is materialized");
    assert!(matches!(
        subscription.recv().unwrap(),
        ViewUpdate::Initial { .. }
    ));

    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, "α".len()).unwrap(),
            value: "β".into(),
        }))
        .unwrap();
    assert_eq!(
        subscription.recv().unwrap(),
        ViewUpdate::Changed {
            snapshot: graph.revision(),
            value: Arc::from("αβ"),
        }
    );
    assert_eq!(snapshot.get::<DocumentText>(uri).as_deref(), Some("α"));
}
