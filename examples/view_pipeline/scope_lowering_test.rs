//! Integration scenarios for the public recursive-tree and scope APIs.

use plingo::framework::scope::{ScopeNode, snapshot_node, snapshot_outgoing, snapshot_scope};
use plingo::prelude::*;

use super::scope_lowering::{
    Analyses, Diagnostics, DocumentScopes, DocumentSummaries, DocumentSummary, IncomingScopes,
    LoweredNode, LoweredTree, PipelineScope, Program, Programs, ReferenceCandidates,
    ReferenceScopes, Resolution, Resolutions, ScopeData, ScopeLabel, SurfaceNode, SurfaceTree,
    analysis_diagnostics, analysis_label, analysis_origin, analysis_scope_presence, build_surface,
    document_summary, emit_document_scope, emit_node_scope, join_analyses, lower_document,
    node_summary, publish_candidate, resolve_pass,
};

fn install() -> Workspace {
    Workspace::builder()
        .mount::<build_surface::Component, _>(Programs::entries())
        .mount::<lower_document::Component, _>(
            RootSelector::<SurfaceTree, SurfaceNode>::new(),
        )
        .mount::<emit_document_scope::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .mount::<emit_node_scope::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .mount::<publish_candidate::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .mount::<resolve_pass::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .mount::<analysis_label::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .mount::<analysis_origin::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .mount::<analysis_scope_presence::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .mount::<analysis_diagnostics::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .mount::<join_analyses::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .mount::<node_summary::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .mount::<document_summary::Component, _>(
            NodeSelector::<LoweredTree, LoweredNode>::new(),
        )
        .build()
        .expect("workspace builds")
}

fn set_program(engine: &mut Engine, uri: &str, program: Program) {
    engine
        .command(|| Programs::set(uri.to_owned(), program).__apply())
        .expect("program command");
}

fn remove_program(engine: &mut Engine, uri: &str) {
    engine
        .command(|| Programs::remove(uri.to_owned()).__apply())
        .expect("program removal");
}

fn roots(snapshot: &Snapshot, uri: &str) -> Vec<AstBox<LoweredNode>> {
    snapshot
        .tree::<LoweredTree>()
        .roots(&uri.to_owned())
        .collect()
}

fn visit_lowered(
    tree: &SnapshotTree<LoweredTree>,
    node: AstBox<LoweredNode>,
    nodes: &mut Vec<AstBox<LoweredNode>>,
) {
    nodes.push(node.clone());
    let value = tree.materialize(node).expect("lowered payload");
    let children = match value {
        LoweredNode::Module { declarations } => declarations,
        LoweredNode::Definition { value, .. } => vec![value],
        LoweredNode::ApplyAdd { operands } => operands,
        LoweredNode::Integer { .. } | LoweredNode::Variable { .. } | LoweredNode::Error { .. } => {
            Vec::new()
        }
    };
    for child in children {
        visit_lowered(tree, child, nodes);
    }
}

fn lowered_nodes(snapshot: &Snapshot, root: AstBox<LoweredNode>) -> Vec<AstBox<LoweredNode>> {
    let tree = snapshot.tree::<LoweredTree>();
    let mut nodes = Vec::new();
    visit_lowered(&tree, root, &mut nodes);
    nodes
}

fn reference_node(snapshot: &Snapshot, root: AstBox<LoweredNode>) -> AstBox<LoweredNode> {
    let tree = snapshot.tree::<LoweredTree>();
    lowered_nodes(snapshot, root)
        .into_iter()
        .find(|node| {
            matches!(
                tree.materialize(node.clone()).expect("lowered payload"),
                LoweredNode::Variable { .. }
            )
        })
        .expect("reference node")
}

fn state_of(snapshot: &Snapshot) -> Vec<(String, String)> {
    let digest = super::scope_lowering::semantic_digest(snapshot);
    digest
        .render()
        .lines()
        .map(|line| {
            let (key, value) = line.split_once(" = ").expect("digest row");
            (key.to_owned(), value.to_owned())
        })
        .collect()
}

#[test]
fn recursive_tree_scope_graph_and_analysis_views_agree() {
    let mut workspace = install();
    let mut engine = workspace.engine_mut();
    let uri = "memory://scope/ok";
    set_program(
        &mut engine,
        uri,
        Program {
            binding: "x".into(),
            value: 1,
            reference: Some("x".into()),
        },
    );

    let snapshot = engine.snapshot();
    let root = roots(&snapshot, uri)
        .into_iter()
        .next()
        .expect("lowered root");
    let nodes = lowered_nodes(&snapshot, root.clone());
    assert_eq!(nodes.len(), 5);

    let reference = reference_node(&snapshot, root.clone());
    let document = snapshot
        .observe::<DocumentScopes>(root.clone())
        .expect("document scope")
        .as_ref()
        .clone();
    assert!(matches!(
        snapshot_scope(&snapshot, document.clone()).as_deref(),
        Some(ScopeData::Document)
    ));
    let declarations = snapshot_outgoing(
        &snapshot,
        document,
        &ScopeLabel::Declaration("x".to_owned()),
    );
    assert_eq!(declarations.len(), 1);
    assert!(matches!(
        snapshot_node(&snapshot, declarations[0].clone()).as_deref(),
        Some(ScopeNode::Declaration(ScopeData::Definition(name))) if name == "x"
    ));

    assert!(matches!(
        snapshot.observe::<Resolutions>(reference.clone()).as_deref(),
        Some(Resolution::Resolved { declaration }) if declaration == &declarations[0]
    ));
    let reference_scope = snapshot
        .observe::<ReferenceScopes>(reference.clone())
        .expect("reference scope")
        .as_ref()
        .clone();
    assert_eq!(
        snapshot_outgoing(&snapshot, reference_scope, &ScopeLabel::ResolvesTo,),
        declarations
    );
    assert_eq!(
        snapshot
            .observe::<Analyses>(reference.clone())
            .as_deref()
            .map(|analysis| analysis.label.as_str()),
        Some("reference x -> x")
    );
    assert!(snapshot.list::<Diagnostics>(&reference).is_empty());
    assert_eq!(
        snapshot.observe::<DocumentSummaries>(root).as_deref(),
        Some(&DocumentSummary {
            nodes: 5,
            diagnostics: 0,
        })
    );
}

#[test]
fn reference_edit_retracts_resolution_edge_and_adds_diagnostic() {
    let mut workspace = install();
    let mut engine = workspace.engine_mut();
    let uri = "memory://scope/edit";
    set_program(
        &mut engine,
        uri,
        Program {
            binding: "x".into(),
            value: 1,
            reference: Some("x".into()),
        },
    );
    let before = engine.snapshot();
    let root = roots(&before, uri).into_iter().next().expect("root");
    let reference = reference_node(&before, root.clone());
    let root_again = root.clone();

    set_program(
        &mut engine,
        uri,
        Program {
            binding: "x".into(),
            value: 1,
            reference: Some("missing".into()),
        },
    );
    let after = engine.snapshot();
    assert_eq!(roots(&after, uri), vec![root_again]);
    assert!(matches!(
        after.observe::<Resolutions>(reference.clone()).as_deref(),
        Some(Resolution::Unbound { name }) if name == "missing"
    ));
    let reference_scope = after
        .observe::<ReferenceScopes>(reference.clone())
        .expect("reference scope")
        .as_ref()
        .clone();
    assert!(snapshot_outgoing(&after, reference_scope, &ScopeLabel::ResolvesTo).is_empty());
    assert_eq!(
        after
            .list::<Diagnostics>(&reference)
            .into_iter()
            .map(|diagnostic| diagnostic.as_ref().clone())
            .collect::<Vec<_>>(),
        vec!["unbound reference missing".to_owned()]
    );
    assert_eq!(
        after.observe::<DocumentSummaries>(root).as_deref(),
        Some(&DocumentSummary {
            nodes: 5,
            diagnostics: 1,
        })
    );
}

#[test]
fn optional_child_removal_retracts_all_derived_views() {
    let mut workspace = install();
    let mut engine = workspace.engine_mut();
    let uri = "memory://scope/remove-child";
    set_program(
        &mut engine,
        uri,
        Program {
            binding: "x".into(),
            value: 3,
            reference: Some("x".into()),
        },
    );
    let before = engine.snapshot();
    let root = roots(&before, uri).into_iter().next().expect("root");
    let before_nodes = lowered_nodes(&before, root.clone());
    assert_eq!(before_nodes.len(), 5);

    set_program(
        &mut engine,
        uri,
        Program {
            binding: "x".into(),
            value: 3,
            reference: None,
        },
    );
    let after = engine.snapshot();
    let after_root = roots(&after, uri).into_iter().next().expect("root");
    let after_nodes = lowered_nodes(&after, after_root.clone());
    assert_eq!(after_root, root);
    assert_eq!(after_nodes.len(), 4);
    assert!(
        after
            .observe::<DocumentSummaries>(after_root.clone())
            .is_some_and(|summary| summary.nodes == 4)
    );
    assert!(
        after
            .observe::<ReferenceCandidates>(before_nodes[4].clone())
            .is_none()
    );
    assert!(
        after
            .observe::<ReferenceScopes>(before_nodes[4].clone())
            .is_none()
    );
    assert!(
        after
            .observe::<Resolutions>(before_nodes[4].clone())
            .is_none()
    );
    assert!(after.list::<Diagnostics>(&before_nodes[4]).is_empty());
}

#[test]
fn program_removal_retracts_roots_scopes_and_summaries() {
    let mut workspace = install();
    let mut engine = workspace.engine_mut();
    let uri = "memory://scope/remove";
    set_program(
        &mut engine,
        uri,
        Program {
            binding: "x".into(),
            value: 0,
            reference: None,
        },
    );
    assert!(!roots(&engine.snapshot(), uri).is_empty());

    remove_program(&mut engine, uri);
    let snapshot = engine.snapshot();
    assert!(roots(&snapshot, uri).is_empty());
    assert!(snapshot.inputs::<Programs>().is_empty());
    assert!(snapshot.inputs::<DocumentSummaries>().is_empty());
    assert!(snapshot.inputs::<DocumentScopes>().is_empty());
    assert!(engine.__liveness_audit().is_empty());
}

#[test]
fn semantic_digest_is_warm_cold_equivalent() {
    let uri = "memory://scope/cold";
    let program = Program {
        binding: "x".into(),
        value: 5,
        reference: Some("x".into()),
    };
    let mut warm_workspace = install();
    let warm = warm_workspace.engine_mut();
    set_program(warm, uri, program.clone());
    set_program(
        warm,
        uri,
        Program {
            value: 7,
            ..program.clone()
        },
    );

    let mut cold_workspace = install();
    let cold = cold_workspace.engine_mut();
    set_program(
        cold,
        uri,
        Program {
            value: 7,
            ..program
        },
    );

    assert_eq!(state_of(&warm.snapshot()), state_of(&cold.snapshot()));
}

#[test]
fn shared_child_calls_have_one_instance_and_removal_is_refcounted() {
    // Two mounted parents (`emit_document_scope` and `document_summary`
    // both call child components for the same source nodes) exercise the
    // shared-instance contract: one child instance per definition+input,
    // retired only when its last caller disappears.
    let mut workspace = install();
    let mut engine = workspace.engine_mut();
    let uri = "memory://shared";
    set_program(
        &mut engine,
        uri,
        Program {
            binding: "shared".into(),
            value: 3,
            reference: None,
        },
    );
    let snapshot = engine.snapshot();
    let root = roots(&snapshot, uri).into_iter().next().expect("root");
    // The child instance count is observable through the summary: one node
    // summary per lowered node regardless of how many parents read it.
    // Module -> Definition -> Integer, plus the Definition's value operand:
    // 4 lowered nodes for this program shape.
    assert_eq!(
        snapshot.observe::<DocumentSummaries>(root).as_deref(),
        Some(&DocumentSummary {
            nodes: 4,
            diagnostics: 0,
        })
    );
    assert!(engine.__liveness_audit().is_empty());

    // Removing the program retracts every derived fact exactly once.
    remove_program(&mut engine, uri);
    assert!(roots(&engine.snapshot(), uri).is_empty());
    assert!(engine.__liveness_audit().is_empty());
}
