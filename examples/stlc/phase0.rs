//! Phase-0 semantic oracles for the public STLC authoring surface.

use plingo::framework::Workspace;
use plingo::framework::parse::{ParseStatus, ParserTreeStatuses};
use plingo::framework::source::SourceEdit;
use plingo::prelude::*;
use plingo::reactive::digest::{FamilyState, render_diff};
use plingo::utils::Span;

use super::digest::stlc_digest;
use super::name_resolve::{
    name_declaration, name_document, name_expr, name_param, name_path, name_type, name_type_atom,
    resolve_expr,
};
use super::syntax::{
    StlcDeclaration, StlcDocument, StlcExpr, StlcParam, StlcPath, StlcToken, StlcTree, StlcType,
    StlcTypeAtom,
};

pub(crate) const BASELINE: &str = "x : Nat := 1";

pub(crate) fn uri(name: &str) -> fluent_uri::Uri<String> {
    Span::new(format!("test://{name}"), 0, 0)
        .expect("uri parses")
        .uri
}

pub(crate) fn build() -> Workspace {
    Workspace::builder()
        .lexer::<StlcToken>()
        .parser::<StlcDocument>()
        .mount::<name_document::Component, _>(StlcDocument::nodes())
        .mount::<name_declaration::Component, _>(StlcDeclaration::nodes())
        .mount::<name_path::Component, _>(StlcPath::nodes())
        .mount::<name_param::Component, _>(StlcParam::nodes())
        .mount::<name_type::Component, _>(StlcType::nodes())
        .mount::<name_type_atom::Component, _>(StlcTypeAtom::nodes())
        .mount::<name_expr::Component, _>(StlcExpr::nodes())
        .mount::<resolve_expr::Component, _>(StlcExpr::nodes())
        .mount::<super::check::synthesize_expr::Component, _>(StlcExpr::nodes())
        .mount::<super::check::synthesize_type::Component, _>(StlcType::nodes())
        .mount::<super::check::synthesize_type_atom::Component, _>(StlcTypeAtom::nodes())
        .mount::<super::check::synthesize_param::Component, _>(StlcParam::nodes())
        .mount::<super::check::synthesize_declaration::Component, _>(StlcDeclaration::nodes())
        .mount::<super::check::publish_expr::Component, _>(StlcExpr::nodes())
        .mount::<super::check::publish_param::Component, _>(StlcParam::nodes())
        .mount::<super::check::publish_declaration::Component, _>(StlcDeclaration::nodes())
        .mount::<super::structural::structural_document::Component, _>(StlcDocument::nodes())
        .mount::<super::structural::structural_declaration::Component, _>(StlcDeclaration::nodes())
        .mount::<super::structural::structural_expression::Component, _>(StlcExpr::nodes())
        .mount::<super::structural::structural_path::Component, _>(StlcPath::nodes())
        .mount::<super::structural::structural_parameter::Component, _>(StlcParam::nodes())
        .mount::<super::structural::structural_type::Component, _>(StlcType::nodes())
        .mount::<super::structural::structural_type_atom::Component, _>(StlcTypeAtom::nodes())
        .build()
        .expect("workspace builds")
}

fn state(workspace: &Workspace) -> FamilyState {
    let snapshot = workspace.snapshot();
    FamilyState::capture(stlc_digest(&snapshot), &snapshot)
}

fn replace_once(
    uri: &fluent_uri::Uri<String>,
    source: &str,
    old: &str,
    new: &str,
) -> Vec<SourceEdit> {
    let start = source.find(old).expect("source fragment");
    vec![
        SourceEdit::Delete {
            key: Span::new_uri(uri.clone(), start, start + old.len()).expect("delete span"),
        },
        SourceEdit::Insert {
            key: Span::point_uri(uri.clone(), start).expect("insert point"),
            value: new.into(),
        },
    ]
}

#[test]
fn canonical_fixture_uses_only_semantic_views() {
    let uri = uri("fixture");
    let mut workspace = build();
    workspace.open(uri.clone(), BASELINE).expect("open");
    let snapshot = workspace.snapshot();
    let digest = stlc_digest(&snapshot);

    let status = snapshot
        .observe::<ParserTreeStatuses>(uri.to_string())
        .expect("parser status");
    assert!(matches!(status.as_ref(), ParseStatus::Clean));
    assert_eq!(
        snapshot.tree::<StlcTree>().roots(&uri.to_string()).count(),
        1
    );
    assert!(
        digest.rows_in("tree") >= 5,
        "tree rows: {}",
        digest.render()
    );
    assert!(digest.rows_in("tokens") == 1);
    assert!(digest.rows_in("incoming") >= 1);
    assert!(digest.rows_in("node-index") >= 5);
    assert!(
        !digest.render().contains("Node("),
        "raw graph identity leaked"
    );
    assert!(workspace.__liveness_audit().is_empty());
}

#[test]
fn value_edit_changes_semantics_and_reverses_exactly() {
    let uri = uri("roundtrip");
    let mut workspace = build();
    workspace.open(uri.clone(), BASELINE).expect("open");
    let before = state(&workspace);

    workspace
        .edit(replace_once(&uri, BASELINE, "1", "2"))
        .expect("forward edit");
    let after = state(&workspace);
    assert_ne!(after.digest, before.digest);
    assert!(render_diff(&before.digest, &after.digest).contains("tokens"));

    workspace
        .edit(replace_once(&uri, "x : Nat := 2", "2", "1"))
        .expect("reverse edit");
    let restored = state(&workspace);
    assert_eq!(restored, before);
    assert!(workspace.__liveness_audit().is_empty());
}

#[test]
fn warm_and_cold_snapshots_have_equal_public_digest() {
    let uri = uri("warm-cold");
    let mut warm = build();
    warm.open(uri.clone(), "id : Nat := 1").expect("open");
    warm.edit(replace_once(&uri, "id : Nat := 1", "1", "2"))
        .expect("edit");
    let warm_state = state(&warm);

    let mut cold = build();
    cold.open(uri, "id : Nat := 2").expect("open");
    let cold_state = state(&cold);
    assert_eq!(warm_state.digest, cold_state.digest);
}
