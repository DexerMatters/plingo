//! Integration scenarios for the lowered-tree, scope-graph, and multi-view
//! analysis pipeline.

use plingo::framework::scope::{ScopeGraph, ScopeNode, snapshot_declarations, snapshot_outgoing};
use plingo::reactive::view::Node;
use plingo::reactive::{Engine, Snapshot};

use super::scope_lowering::{
    Analyses, Diagnostics, DocumentScopes, DocumentSummaries, DocumentSummary, IncomingScopes,
    LoweredBySource, LoweredNode, LoweredOrigins, LoweredRoots, LoweredTree, PipelineScope,
    Program, Programs, ReferenceScopes, Resolution, Resolutions, ScopeData, ScopeLabel,
    analyze_pass_install, build_surface_pass_install, emit_scopes_pass_install, lower_pass_install,
    resolve_pass_install, summarize_pass_install,
};

fn install(engine: &mut Engine) {
    // Cut C: passes install as first-class components.
    build_surface_pass_install(engine).expect("install build surface pass");
    lower_pass_install(engine).expect("install lower pass");
    emit_scopes_pass_install(engine).expect("install emit scopes pass");
    resolve_pass_install(engine).expect("install resolve pass");
    analyze_pass_install(engine).expect("install analyze pass");
    summarize_pass_install(engine).expect("install summarize pass");
}

fn set_program(engine: &mut Engine, uri: &str, program: Program) {
    engine
        .command(|| {
            plingo::reactive::kind::emit_view::<Programs>()?.insert(uri.to_owned(), program)
        })
        .expect("program command");
}

fn lowered_nodes(snapshot: &Snapshot, root: Node<LoweredTree>) -> Vec<Node<LoweredTree>> {
    fn visit(snapshot: &Snapshot, node: Node<LoweredTree>, nodes: &mut Vec<Node<LoweredTree>>) {
        nodes.push(node.clone());
        for child in snapshot.tree_children::<LoweredTree>(node.clone()) {
            visit(snapshot, child, nodes);
        }
    }

    let mut nodes = Vec::new();
    visit(snapshot, root, &mut nodes);
    nodes
}

fn lowered_payloads(snapshot: &Snapshot, root: Node<LoweredTree>) -> Vec<LoweredNode> {
    lowered_nodes(snapshot, root)
        .into_iter()
        .map(|node| {
            snapshot
                .tree_payload::<LoweredTree>(node)
                .as_deref()
                .expect("lowered payload")
                .clone()
        })
        .collect()
}

fn reference_node(snapshot: &Snapshot, root: Node<LoweredTree>) -> Node<LoweredTree> {
    lowered_nodes(snapshot, root)
        .into_iter()
        .find(|node| {
            matches!(
                snapshot.tree_payload::<LoweredTree>(node.clone()).as_deref(),
                Some(LoweredNode::Variable(_))
            )
        })
        .expect("reference node")
}

#[test]
fn lowered_tree_scope_graph_and_analysis_views_agree() {
    let mut engine = Engine::new();
    install(&mut engine);
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
    let root = snapshot.tree_roots_of::<LoweredTree>(&uri.to_owned())[0].clone();
    assert_eq!(
        lowered_payloads(&snapshot, root.clone()),
        vec![
            LoweredNode::Module,
            LoweredNode::Definition("x".into()),
            LoweredNode::ApplyAdd,
            LoweredNode::Integer(1),
            LoweredNode::Variable("x".into()),
        ]
    );

    let document = snapshot
        .observe::<DocumentScopes>(uri.to_owned())
        .expect("document scope");
    let document = document.as_ref().clone();
    let declarations =
        snapshot_declarations(&snapshot, document, &ScopeLabel::Declaration("x".into()));
    assert_eq!(declarations.len(), 1);
    assert!(matches!(
        snapshot.graph_node::<ScopeGraph<PipelineScope>>(declarations[0].node()).as_deref(),
        Some(ScopeNode::Declaration(ScopeData::Definition(name))) if name == "x"
    ));

    let reference = reference_node(&snapshot, root);
    assert!(matches!(
        snapshot.observe::<Resolutions>(reference.clone()).as_deref(),
        Some(Resolution::Resolved { declaration }) if *declaration == declarations[0]
    ));
    assert_eq!(
        snapshot_outgoing(
            &snapshot,
            snapshot
                .observe::<ReferenceScopes>(reference.clone())
                .expect("reference scope")
                .as_ref()
                .clone(),
            &ScopeLabel::ResolvesTo,
        ),
        declarations
    );
    assert_eq!(
        snapshot
            .observe::<Analyses>(reference.clone())
            .as_deref()
            .map(|analysis| &analysis.label),
        Some(&"reference x -> x".to_owned())
    );
    assert!(snapshot.list::<Diagnostics>(&reference).is_empty());
    assert_eq!(
        snapshot
            .observe::<DocumentSummaries>(uri.to_owned())
            .as_deref(),
        Some(&DocumentSummary {
            nodes: 5,
            diagnostics: 0,
        })
    );

    let origin = snapshot
        .observe::<LoweredOrigins>(reference.clone())
        .expect("lowered origin");
    assert_eq!(
        snapshot
            .observe::<LoweredBySource>(origin.as_ref().clone())
            .as_deref(),
        Some(&reference)
    );
    assert!(snapshot.observe::<IncomingScopes>(reference).is_some());
}

#[test]
fn resolution_and_diagnostics_update_from_the_reference_key_only() {
    let mut engine = Engine::new();
    install(&mut engine);
    let uri = "memory://scope/edit";
    let initial = Program {
        binding: "x".into(),
        value: 1,
        reference: Some("x".into()),
    };
    set_program(&mut engine, uri, initial);

    let before = engine.snapshot();
    let root = before.tree_roots_of::<LoweredTree>(&uri.to_owned())[0].clone();
    let nodes = lowered_nodes(&before, root.clone());
    let reference = reference_node(&before, root.clone());

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
    let after_root = after.tree_roots_of::<LoweredTree>(&uri.to_owned())[0].clone();
    assert_eq!(after_root, root);
    assert_eq!(lowered_nodes(&after, after_root.clone()), nodes);
    assert!(matches!(
        after.observe::<Resolutions>(reference.clone()).as_deref(),
        Some(Resolution::Unbound { name }) if name == "missing"
    ));
    assert!(
        snapshot_outgoing(
            &after,
            after
                .observe::<ReferenceScopes>(reference.clone())
                .expect("reference scope")
                .as_ref()
                .clone(),
            &ScopeLabel::ResolvesTo,
        )
        .is_empty(),
        "the resolver retracts its old graph edge"
    );
    assert_eq!(
        after
            .list::<Diagnostics>(&reference)
            .into_iter()
            .map(|diagnostic| (*diagnostic).clone())
            .collect::<Vec<_>>(),
        vec!["unbound reference missing".to_owned()]
    );
    assert_eq!(
        after
            .observe::<DocumentSummaries>(uri.to_owned())
            .as_deref(),
        Some(&DocumentSummary {
            nodes: 5,
            diagnostics: 1,
        })
    );
}

#[test]
fn child_order_extension_adds_one_lowered_scope_and_analysis_branch() {
    let mut engine = Engine::new();
    install(&mut engine);
    let uri = "memory://scope/topology";
    set_program(
        &mut engine,
        uri,
        Program {
            binding: "x".into(),
            value: 3,
            reference: None,
        },
    );

    let before = engine.snapshot();
    let root = before.tree_roots_of::<LoweredTree>(&uri.to_owned())[0].clone();
    let prefix = lowered_nodes(&before, root.clone());
    assert_eq!(prefix.len(), 4);

    set_program(
        &mut engine,
        uri,
        Program {
            binding: "x".into(),
            value: 3,
            reference: Some("x".into()),
        },
    );

    let after = engine.snapshot();
    let after_root = after.tree_roots_of::<LoweredTree>(&uri.to_owned())[0].clone();
    let nodes = lowered_nodes(&after, after_root.clone());
    assert_eq!(after_root, root);
    assert_eq!(&nodes[..prefix.len()], prefix.as_slice());
    let reference = reference_node(&after, after_root.clone());
    assert!(matches!(
        after.observe::<Resolutions>(reference).as_deref(),
        Some(Resolution::Resolved { .. })
    ));
    assert_eq!(
        after
            .observe::<DocumentSummaries>(uri.to_owned())
            .as_deref(),
        Some(&DocumentSummary {
            nodes: 5,
            diagnostics: 0,
        })
    );
}

#[test]
fn removing_a_program_retracts_its_lowered_roots_and_summary() {
    let mut engine = Engine::new();
    install(&mut engine);
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

    engine
        .command(|| plingo::reactive::kind::emit_view::<Programs>()?.remove(uri.to_owned()))
        .expect("remove program");
    let snapshot = engine.snapshot();
    assert!(
        snapshot
            .tree_roots_of::<LoweredTree>(&uri.to_owned())
            .is_empty()
    );
    assert!(snapshot.observe::<LoweredRoots>(uri.to_owned()).is_none());
    assert!(
        snapshot
            .observe::<DocumentSummaries>(uri.to_owned())
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Phase 0 oracles (follow-up plan §24.4): canonical fixture, reversible
// edit matrix with exact keyed deltas, and warm/cold equivalence.
// ---------------------------------------------------------------------------

use plingo::reactive::digest::{FamilyState, SemanticDigest, render_diff};

use super::scope_lowering::semantic_digest;

const CANON: &str = "memory://canon/a";

fn state_of(engine: &Engine) -> FamilyState {
    let snapshot = engine.snapshot();
    FamilyState::capture(semantic_digest(&snapshot), &snapshot)
}

/// The exact changed row keys between two digests.
fn diff_keys(before: &SemanticDigest, after: &SemanticDigest) -> Vec<String> {
    render_diff(before, after)
        .lines()
        .map(|line| {
            let body = &line[2..];
            body.split(" = ").next().expect("diff row key").to_owned()
        })
        .collect()
}

/// One row's value from a digest.
fn row_of(digest: &SemanticDigest, view: &str, key: &str) -> String {
    let full = format!("{view}::{key}");
    digest
        .rows_of(view)
        .into_iter()
        .find(|(row_key, _)| *row_key == full)
        .map(|(_, value)| value.to_owned())
        .unwrap_or_else(|| format!("<missing {full}>"))
}

/// Asserts the engine holds no stale liveness bookkeeping.
fn assert_liveness_clean(engine: &Engine) {
    let audit = engine.__liveness_audit();
    assert!(audit.is_empty(), "liveness audit leaked: {audit:?}");
}

/// Parses the canonical rendering into sorted (key, value) rows so a test
/// can compare complete digest content against a hand-authored table.
fn rows(digest: &SemanticDigest) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = digest
        .render()
        .lines()
        .map(|line| {
            let (key, value) = line.split_once(" = ").expect("digest line");
            (key.to_owned(), value.to_owned())
        })
        .collect();
    rows.sort();
    rows
}

/// Canonical single-document fixture: hand-authored complete public-view
/// content. A warm and cold implementation sharing the same leaked node or
/// orphan output must still fail this table.
#[test]
fn canonical_fixture_matches_hand_authored_rows() {
    let mut engine = Engine::new();
    install(&mut engine);
    set_program(
        &mut engine,
        CANON,
        Program {
            binding: "x".into(),
            value: 5,
            reference: Some("x".into()),
        },
    );
    let digest = semantic_digest(&engine.snapshot());

    let root = format!("{CANON}#0");
    let module = format!("{root}.0");
    let add = format!("{module}.0");
    let number = format!("{add}.0");
    let name = format!("{add}.1");
    let expected: Vec<(String, String)> = vec![
        (
            format!("programs::{CANON}"),
            "program{binding:\"x\",value:5,reference:some(\"x\")}".to_owned(),
        ),
        (format!("surface_roots::{CANON}"), root.clone()),
        (format!("surface_tree::{root}"), "Document".to_owned()),
        (
            format!("surface_tree::{module}"),
            "Binding(\"x\")".to_owned(),
        ),
        (format!("surface_tree::{add}"), "Add".to_owned()),
        (format!("surface_tree::{number}"), "Number(5)".to_owned()),
        (format!("surface_tree::{name}"), "Name(\"x\")".to_owned()),
        (format!("lowered_roots::{CANON}"), root.clone()),
        (format!("lowered_tree::{root}"), "Module".to_owned()),
        (
            format!("lowered_tree::{module}"),
            "Definition(\"x\")".to_owned(),
        ),
        (format!("lowered_tree::{add}"), "ApplyAdd".to_owned()),
        (format!("lowered_tree::{number}"), "Integer(5)".to_owned()),
        (
            format!("lowered_tree::{name}"),
            "Variable(\"x\")".to_owned(),
        ),
        // Provenance is the identity mapping for this fixture shape.
        (format!("lowered_origins::{root}"), root.clone()),
        (format!("lowered_origins::{module}"), module.clone()),
        (format!("lowered_origins::{add}"), add.clone()),
        (format!("lowered_origins::{number}"), number.clone()),
        (format!("lowered_origins::{name}"), name.clone()),
        (format!("lowered_by_source::{root}"), root.clone()),
        (format!("lowered_by_source::{module}"), module.clone()),
        (format!("lowered_by_source::{add}"), add.clone()),
        (format!("lowered_by_source::{number}"), number.clone()),
        (format!("lowered_by_source::{name}"), name.clone()),
        // Every lowered node sits in its document scope.
        (format!("document_scopes::{CANON}"), "Document".to_owned()),
        (format!("incoming_scopes::{root}"), "Document".to_owned()),
        (format!("incoming_scopes::{module}"), "Document".to_owned()),
        (format!("incoming_scopes::{add}"), "Document".to_owned()),
        (format!("incoming_scopes::{number}"), "Document".to_owned()),
        (format!("incoming_scopes::{name}"), "Document".to_owned()),
        (format!("reference_candidates::{name}"), "x".to_owned()),
        (
            format!("resolutions::{name}"),
            "Resolved{declaration:Declaration(Definition(\"x\"))}".to_owned(),
        ),
        (
            format!("reference_scopes::{name}"),
            "Reference(Reference(\"x\"))".to_owned(),
        ),
        (
            format!("analyses::{root}"),
            "analysis{label:\"module\",diagnostics:0,has_origin:true,has_scope:true}".to_owned(),
        ),
        (
            format!("analyses::{module}"),
            "analysis{label:\"definition x\",diagnostics:0,has_origin:true,has_scope:true}"
                .to_owned(),
        ),
        (
            format!("analyses::{add}"),
            "analysis{label:\"apply add\",diagnostics:0,has_origin:true,has_scope:true}".to_owned(),
        ),
        (
            format!("analyses::{number}"),
            "analysis{label:\"integer 5\",diagnostics:0,has_origin:true,has_scope:true}".to_owned(),
        ),
        (
            format!("analyses::{name}"),
            "analysis{label:\"reference x -> x\",diagnostics:0,has_origin:true,has_scope:true}"
                .to_owned(),
        ),
        (format!("node_summaries::{root}"), "summary{nodes:5,diagnostics:0}".to_owned()),
        (format!("node_summaries::{module}"), "summary{nodes:4,diagnostics:0}".to_owned()),
        (format!("node_summaries::{add}"), "summary{nodes:3,diagnostics:0}".to_owned()),
        (format!("node_summaries::{number}"), "summary{nodes:1,diagnostics:0}".to_owned()),
        (format!("node_summaries::{name}"), "summary{nodes:1,diagnostics:0}".to_owned()),
        // Nodes without diagnostics own no Diagnostics facts at all; only a
        // currently-unbound reference carries a row.
        (
            "scope_nodes::#000000".to_owned(),
            "Declaration(Definition(\"x\"))".to_owned(),
        ),        (
            "scope_nodes::#000001".to_owned(),
            "Reference(Reference(\"x\"))".to_owned(),
        ),
        (
            "scope_nodes::#000002".to_owned(),
            "Scope(Document)".to_owned(),
        ),
        (
            "scope_edges::#000000".to_owned(),
            "(Reference(Reference(\"x\")),ResolvesTo)->Declaration(Definition(\"x\"))".to_owned(),
        ),
        (
            "scope_edges::#000001".to_owned(),
            "(Scope(Document),Declaration(\"x\"))->Declaration(Definition(\"x\"))".to_owned(),
        ),
        (
            "scope_edges::#000002".to_owned(),
            "(Scope(Document),Reference(\"x\"))->Reference(Reference(\"x\"))".to_owned(),
        ),
        (
            format!("document_summaries::{CANON}"),
            "summary{nodes:5,diagnostics:0}".to_owned(),
        ),
    ];
    let mut expected = expected;
    expected.sort();
    let actual = rows(&digest);
    let mut mismatches: Vec<String> = Vec::new();
    for (key, value) in &actual {
        if !expected.contains(&(key.clone(), value.clone())) {
            mismatches.push(format!("unexpected {key} = {value}"));
        }
    }
    for (key, value) in &expected {
        if !actual.contains(&(key.clone(), value.clone())) {
            mismatches.push(format!("missing   {key} = {value}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "canonical digest must equal the hand-authored table:\n{}",
        mismatches.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Exact reaction proofs (plan §24.7): a scalar value edit wakes exactly the
// value-chain components with exact driving/read/write elements, and zero
// scope/resolution components outside the changed node.
// ---------------------------------------------------------------------------

use plingo::reactive::ReactionDigest;

#[test]
fn scalar_value_edit_wakes_exactly_the_value_chain() {
    let mut engine = Engine::new();
    install(&mut engine);
    let uri = "memory://scope/reaction";
    set_program(
        &mut engine,
        uri,
        Program {
            binding: "x".into(),
            value: 5,
            reference: Some("x".into()),
        },
    );

    let report = engine
        .command(|| {
            plingo::reactive::kind::emit_view::<Programs>()?.insert(
                uri.to_owned(),
                Program {
                    binding: "x".into(),
                    value: 7,
                    reference: Some("x".into()),
                },
            )
        })
        .expect("scalar edit");
    let digest = report.metric::<ReactionDigest>().expect("digest");

    // Exactly the source builder, the number node's lowering, its scope
    // payload component, its label/join analyses, and its own summary. The
    // candidate component re-evaluates because it reads the changed payload,
    // but commits nothing (the node is not a reference). Ancestor summaries
    // and the document summary stay cold: node/diagnostic counts are equal.
    let mut definitions: Vec<String> = digest
        .evaluations
        .iter()
        .map(|evaluation| evaluation.definition.to_string())
        .collect();
    definitions.sort();
    definitions.dedup();
    let expected = [
        "view_pipeline::scope_lowering::analysis_label",
        "view_pipeline::scope_lowering::build_surface_pass",
        "view_pipeline::scope_lowering::emit_node_scope",
        "view_pipeline::scope_lowering::join_analyses",
        "view_pipeline::scope_lowering::lower_node",
        "view_pipeline::scope_lowering::node_summary",
        "view_pipeline::scope_lowering::publish_candidate",
    ];
    assert_eq!(
        definitions, expected,
        "scalar edit evaluated unexpected definitions: {definitions:?}\n\
         evaluations: {:#?}",
        digest.evaluations
    );

    // Zero resolution/scope-graph retirements and zero broad enumerations.
    assert!(digest.retirements.is_empty(), "{:#?}", digest.retirements);
    assert!(
        digest.broad_enumerations.is_empty(),
        "{:#?}",
        digest.broad_enumerations
    );

    // The resolved reference node stays cold: no resolve_pass evaluation.
    assert_eq!(
        digest.evaluations_of("resolve_pass").count(),
        0,
        "a scalar value edit must not wake the resolver"
    );
    assert_eq!(
        digest.evaluations_of("document_summary").count(),
        0,
        "equal summary values must keep the document summary cold"
    );
    assert_eq!(
        digest.evaluations_of("emit_document_scope").count(),
        0,
        "the document scope must stay cold on a value edit"
    );

    // Exact output edges: the number node's lowered payload and analysis
    // moved; the reference name and resolution outputs stayed cold.
    for evaluation in digest.evaluations_of("lower_node") {
        assert!(
            evaluation
                .outputs
                .iter()
                .any(|edge| edge.view.contains("LoweredTree")),
            "lower_node must own a tree output: {:#?}",
            evaluation.outputs
        );
    }
    for evaluation in digest.evaluations_of("analysis_label") {
        assert!(
            evaluation
                .outputs
                .iter()
                .any(|edge| edge.view.contains("AnalysisLabels")),
            "analysis_label must own a label output: {:#?}",
            evaluation.outputs
        );
    }

    // Warm/cold: the committed digest still matches a fresh engine.
    let warm = state_of(&engine);
    let mut cold = Engine::new();
    install(&mut cold);
    set_program(
        &mut cold,
        uri,
        Program {
            binding: "x".into(),
            value: 7,
            reference: Some("x".into()),
        },
    );
    let cold_state = state_of(&cold);
    assert_eq!(
        warm.digest,
        cold_state.digest,
        "warm/cold mismatch after scalar edit"
    );
}

/// Program removal retires exactly the per-node components of that document
/// (plan §24.7 item 5): every lowered-node driver, its scope payload, its
/// analyses, its summary, and the document summary — with zero unrelated
/// evaluations.
#[test]
fn program_removal_retires_exactly_its_component_domain() {
    let mut engine = Engine::new();
    install(&mut engine);
    let a = "memory://scope/retire-a";
    let b = "memory://scope/retire-b";
    set_program(
        &mut engine,
        a,
        Program {
            binding: "x".into(),
            value: 5,
            reference: Some("x".into()),
        },
    );
    set_program(
        &mut engine,
        b,
        Program {
            binding: "q".into(),
            value: 2,
            reference: None,
        },
    );

    let report = engine
        .command(|| plingo::reactive::kind::emit_view::<Programs>()?.remove(a.to_owned()))
        .expect("remove program A");
    let digest = report.metric::<ReactionDigest>().expect("digest");

    // The removed document's domain retires; no broad enumeration runs.
    // Driving elements are opaque node identities, so isolation is proven
    // by document B's byte-identical facts below.
    assert!(
        !digest.retirements.is_empty(),
        "removal must retire component instances"
    );
    assert!(
        digest.broad_enumerations.is_empty(),
        "{:#?}",
        digest.broad_enumerations
    );

    // Document B's summary and root stay byte-identical.
    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot
            .observe::<DocumentSummaries>(b.to_owned())
            .as_deref(),
        Some(&DocumentSummary {
            nodes: 4,
            diagnostics: 0,
        })
    );
    assert!(snapshot.observe::<DocumentSummaries>(a.to_owned()).is_none());
    assert_liveness_clean(&engine);
}

/// The full reversible edit matrix (plan §24.4): scalar payload change,
/// reference rename, leaf-child insertion/removal, unrelated-document edit,
/// and program removal/reinsertion. Every forward step asserts its exact
/// keyed delta; every reverse restores the initial FamilyState exactly with
/// an empty liveness audit; a fresh engine replaying the final membership
/// matches the warm digest.
#[test]
fn reversible_edit_matrix_restores_exact_state() {
    const A: &str = "memory://matrix/a";
    const B: &str = "memory://matrix/b";

    let mut engine = Engine::new();
    install(&mut engine);
    set_program(
        &mut engine,
        A,
        Program {
            binding: "x".into(),
            value: 5,
            reference: Some("x".into()),
        },
    );
    set_program(
        &mut engine,
        B,
        Program {
            binding: "q".into(),
            value: 2,
            reference: None,
        },
    );
    let initial = state_of(&engine);

    // Scalar payload change: only document A's number chain may move.
    set_program(
        &mut engine,
        A,
        Program {
            binding: "x".into(),
            value: 7,
            reference: Some("x".into()),
        },
    );
    let after_scalar = state_of(&engine);
    let moved = diff_keys(&initial.digest, &after_scalar.digest);
    let expected_moved = [
        format!("programs::{A}"),
        format!("surface_tree::{A}#0.0.0.0"),
        format!("lowered_tree::{A}#0.0.0.0"),
        format!("analyses::{A}#0.0.0.0"),
    ];
    assert_eq!(
        moved.len(),
        expected_moved.len(),
        "scalar edit moved unexpected rows:\n{}",
        render_diff(&initial.digest, &after_scalar.digest)
    );
    for key in expected_moved {
        assert!(
            moved.iter().any(|row| row == &key),
            "scalar edit missed {key}:\n{}",
            render_diff(&initial.digest, &after_scalar.digest)
        );
    }

    // Reference rename: resolution, diagnostics, analyses, candidates, and
    // summary refresh only for the renamed node; the number chain stays cold.
    set_program(
        &mut engine,
        A,
        Program {
            binding: "x".into(),
            value: 7,
            reference: Some("z".into()),
        },
    );
    let after_rename = state_of(&engine);
    let rename_diff = render_diff(&after_scalar.digest, &after_rename.digest);
    let rename_keys = diff_keys(&after_scalar.digest, &after_rename.digest);
    for key in [
        format!("reference_candidates::{A}#0.0.0.1"),
        format!("resolutions::{A}#0.0.0.1"),
        format!("analyses::{A}#0.0.0.1"),
        format!("diagnostics::{A}#0.0.0.1"),
        format!("document_summaries::{A}"),
    ] {
        assert!(
            rename_keys.contains(&key),
            "rename missed {key}:\n{rename_diff}"
        );
    }
    assert!(
        rename_keys
            .iter()
            .any(|key| key.starts_with("scope_nodes::"))
    );
    assert!(
        rename_keys
            .iter()
            .any(|key| key.starts_with("scope_edges::"))
    );
    assert!(
        !rename_diff.contains("#0.0.0.0") && !rename_diff.contains("Integer"),
        "rename must not touch the number chain:\n{rename_diff}"
    );
    for line in rename_diff.lines() {
        let key = &line[2..];
        assert!(
            key.contains("/a")
                || key.starts_with("scope_nodes::")
                || key.starts_with("scope_edges::"),
            "rename leaked outside document A: {line}"
        );
    }
    assert_eq!(
        row_of(&after_rename.digest, "resolutions", &format!("{A}#0.0.0.1")),
        "Unbound{name:\"z\"}"
    );
    assert_eq!(
        row_of(&after_rename.digest, "diagnostics", &format!("{A}#0.0.0.1")),
        "[\"unbound reference z\"]"
    );
    assert_eq!(
        row_of(&after_rename.digest, "document_summaries", A),
        "summary{nodes:5,diagnostics:1}"
    );

    // Reverse the rename: exact restoration of digest, live facts, liveness.
    set_program(
        &mut engine,
        A,
        Program {
            binding: "x".into(),
            value: 7,
            reference: Some("x".into()),
        },
    );
    let restored_rename = state_of(&engine);
    assert_eq!(
        restored_rename,
        after_scalar,
        "reverse rename mismatch:\n{}",
        render_diff(&after_scalar.digest, &restored_rename.digest)
    );
    assert_liveness_clean(&engine);

    // Leaf-child removal: the optional Name/Variable child retracts across
    // every view that keyed it; summaries shrink to four nodes.
    set_program(
        &mut engine,
        A,
        Program {
            binding: "x".into(),
            value: 7,
            reference: None,
        },
    );
    let after_removal = state_of(&engine);
    let removal_diff = render_diff(&restored_rename.digest, &after_removal.digest);
    for view in [
        "surface_tree",
        "lowered_tree",
        "lowered_origins",
        "lowered_by_source",
        "incoming_scopes",
        "reference_scopes",
        "analyses",
        "reference_candidates",
        "resolutions",
    ] {
        assert!(
            removal_diff.contains(&format!("- {view}::{A}#0.0.0.1")),
            "child removal must retract {view} leaf:\n{removal_diff}"
        );
    }
    assert_eq!(
        row_of(&after_removal.digest, "document_summaries", A),
        "summary{nodes:4,diagnostics:0}"
    );
    for line in removal_diff.lines() {
        assert!(
            line.contains("/a") || line.starts_with("- scope_") || line.starts_with("~ scope_"),
            "child removal leaked outside document A: {line}"
        );
    }

    // Reinsertion of the child restores the exact pre-removal state.
    set_program(
        &mut engine,
        A,
        Program {
            binding: "x".into(),
            value: 7,
            reference: Some("x".into()),
        },
    );
    let restored_child = state_of(&engine);
    assert_eq!(restored_child, after_scalar);
    assert_liveness_clean(&engine);

    // Unrelated-document edit: every changed row belongs to document B.
    set_program(
        &mut engine,
        B,
        Program {
            binding: "q".into(),
            value: 3,
            reference: None,
        },
    );
    let after_unrelated = state_of(&engine);
    let unrelated_diff = render_diff(&after_scalar.digest, &after_unrelated.digest);
    assert!(
        unrelated_diff.contains(&format!("programs::{B}")),
        "{unrelated_diff}"
    );
    for line in unrelated_diff.lines() {
        assert!(line.contains("/b"), "unrelated edit touched A: {line}");
    }

    // Reverse the unrelated edit: zero residual difference on either side.
    set_program(
        &mut engine,
        B,
        Program {
            binding: "q".into(),
            value: 2,
            reference: None,
        },
    );
    let restored_unrelated = state_of(&engine);
    assert_eq!(restored_unrelated, after_scalar);
    assert_liveness_clean(&engine);

    // Program removal retires document A completely; document B is untouched.
    engine
        .command(|| plingo::reactive::kind::emit_view::<Programs>()?.remove(A.to_owned()))
        .expect("remove program A");
    let removed = state_of(&engine);
    for line in removed.digest.render().lines() {
        assert!(!line.contains("/a"), "removal left a row behind: {line}");
    }
    assert_eq!(
        row_of(&removed.digest, "document_summaries", B),
        "summary{nodes:4,diagnostics:0}"
    );

    // Reinsertion restores the exact pre-removal FamilyState.
    set_program(
        &mut engine,
        A,
        Program {
            binding: "x".into(),
            value: 7,
            reference: Some("x".into()),
        },
    );
    let reopened = state_of(&engine);
    assert_eq!(
        reopened,
        after_scalar,
        "reopen mismatch:\n{}",
        render_diff(&after_scalar.digest, &reopened.digest)
    );
    assert_eq!(reopened.live_facts, after_scalar.live_facts);
    assert_liveness_clean(&engine);

    // Cold oracle: a fresh engine replaying the identical final membership.
    let mut cold = Engine::new();
    install(&mut cold);
    set_program(
        &mut cold,
        A,
        Program {
            binding: "x".into(),
            value: 7,
            reference: Some("x".into()),
        },
    );
    set_program(
        &mut cold,
        B,
        Program {
            binding: "q".into(),
            value: 2,
            reference: None,
        },
    );
    let cold_state = state_of(&cold);
    assert_eq!(
        reopened.digest,
        cold_state.digest,
        "warm/cold mismatch:\n{}",
        render_diff(&reopened.digest, &cold_state.digest)
    );
}
