//! Integration tests for the generated STLC tree readers and semantic passes.

use plingo::framework::parse::ParserTreeStatuses;
use plingo::reactive::Snapshot;
use plingo::reactive::abstract_tree::AstBox;

use super::check::{StlcSynthesizedTypes, StlcTypeResult, StlcTypeValue};
use super::name_resolve::{StlcReferenceCandidates, StlcResolution, StlcResolvedReferences};
use super::phase0::{BASELINE, build, uri};
use super::structural::{StlcNodeIndex, StlcNodeKind};
use super::syntax::{StlcDeclarationView, StlcDocument, StlcDocumentView, StlcExprView, StlcTree};

fn root(snapshot: &Snapshot, uri: &str) -> AstBox<StlcDocument> {
    snapshot
        .tree::<StlcTree>()
        .roots(&uri.to_owned())
        .next()
        .expect("accepted document root")
}

#[test]
fn generated_views_expose_typed_children() {
    let uri = uri("typed-readers");
    let mut workspace = build();
    workspace.open(uri.clone(), BASELINE).expect("open");
    let snapshot = workspace.snapshot();
    let tree = snapshot.tree::<StlcTree>();
    let document = root(&snapshot, &uri.to_string());
    let StlcDocumentView::Lines(lines) = tree.view(document).expect("document view") else {
        panic!("baseline should parse as lines");
    };
    let declaration = lines
        .declarations()
        .expect("declarations")
        .get(0)
        .expect("one declaration");
    let StlcDeclarationView::Value(value) = tree.view(declaration).expect("declaration view")
    else {
        panic!("baseline should parse as value");
    };
    assert!(value.annotation().expect("annotation").is_some());
    assert_eq!(value.parameters().expect("parameters").len(), 0);
    assert!(matches!(
        tree.view(value.body().expect("body"))
            .expect("expression view"),
        StlcExprView::Number(_)
    ));
}

#[test]
fn semantic_components_publish_exact_node_keys() {
    let uri = uri("semantic-keys");
    let mut workspace = build();
    workspace.open(uri.clone(), BASELINE).expect("open");
    let snapshot = workspace.snapshot();
    let document = root(&snapshot, &uri.to_string());
    let lines = match snapshot
        .tree::<StlcTree>()
        .view(document)
        .expect("document view")
    {
        StlcDocumentView::Lines(lines) => lines,
        StlcDocumentView::Error(_) => panic!("baseline parse failed"),
    };
    let declaration = lines
        .declarations()
        .expect("declarations")
        .get(0)
        .expect("declaration");
    let body = match snapshot
        .tree::<StlcTree>()
        .view(declaration)
        .expect("declaration view")
    {
        StlcDeclarationView::Value(value) => value.body().expect("body"),
        _ => panic!("baseline declaration failed"),
    };
    assert!(snapshot.observe::<StlcNodeIndex>(body.erased()).is_some());
    assert_eq!(
        snapshot
            .observe::<StlcNodeIndex>(body.erased())
            .expect("node index")
            .as_ref(),
        &StlcNodeKind::Expression
    );
    assert_eq!(
        snapshot
            .observe::<StlcSynthesizedTypes>(body.erased())
            .expect("synthesized type")
            .as_ref(),
        &StlcTypeResult::Known(StlcTypeValue::Nat)
    );
}

#[test]
fn variable_resolution_tracks_reference_membership() {
    let uri = uri("resolution");
    let mut workspace = build();
    workspace.open(uri.clone(), "id : Nat := x").expect("open");
    let snapshot = workspace.snapshot();
    let document = root(&snapshot, &uri.to_string());
    let declaration = match snapshot
        .tree::<StlcTree>()
        .view(document)
        .expect("document")
    {
        StlcDocumentView::Lines(lines) => lines
            .declarations()
            .expect("declarations")
            .get(0)
            .expect("declaration"),
        _ => panic!("parse failed"),
    };
    let expression = match snapshot
        .tree::<StlcTree>()
        .view(declaration)
        .expect("declaration")
    {
        StlcDeclarationView::Value(value) => value.body().expect("body"),
        _ => panic!("parse failed"),
    };
    assert!(
        snapshot
            .observe::<StlcReferenceCandidates>(expression.erased())
            .is_some()
    );
    assert!(matches!(
        snapshot
            .observe::<StlcResolvedReferences>(expression.erased())
            .map(|value| value.as_ref().clone()),
        Some(StlcResolution::Unbound { .. })
    ));
}

#[test]
fn parser_root_retracts_when_document_closes() {
    let uri = uri("close");
    let mut workspace = build();
    workspace.open(uri.clone(), BASELINE).expect("open");
    assert!(
        workspace
            .snapshot()
            .observe::<ParserTreeStatuses>(uri.to_string())
            .is_some()
    );
    workspace.close(uri).expect("close");
    assert!(
        workspace
            .snapshot()
            .inputs::<ParserTreeStatuses>()
            .is_empty()
    );
}
