//! Granularity checks for the public-view tree transform harness.

use plingo::reactive::kind::emit_view;
use plingo::reactive::view::Node;
use plingo::reactive::{Engine, Snapshot};

use super::view_harness::{
    CoreNode, CoreOrigin, CoreTree, SurfaceProgram, SurfacePrograms, SurfaceTree,
    build_surface_pass_install, lower_view_pass_install,
};

fn install(engine: &mut Engine) {
    build_surface_pass_install(engine).expect("install source stage");
    lower_view_pass_install(engine).expect("install target stage");
}

fn set_program(engine: &mut Engine, uri: &str, program: SurfaceProgram) {
    engine
        .command(|| emit_view::<SurfacePrograms>()?.insert(uri.to_owned(), program))
        .expect("program command");
}

fn core_nodes(snapshot: &Snapshot, root: Node<CoreTree>) -> Vec<Node<CoreTree>> {
    fn visit(snapshot: &Snapshot, node: Node<CoreTree>, nodes: &mut Vec<Node<CoreTree>>) {
        nodes.push(node.clone());
        for child in snapshot.tree_children::<CoreTree>(node) {
            visit(snapshot, child, nodes);
        }
    }

    let mut nodes = Vec::new();
    visit(snapshot, root, &mut nodes);
    nodes
}

fn core_payloads(snapshot: &Snapshot, root: Node<CoreTree>) -> Vec<CoreNode> {
    core_nodes(snapshot, root)
        .into_iter()
        .map(|node| {
            snapshot
                .tree_payload::<CoreTree>(node)
                .as_deref()
                .expect("core payload")
                .clone()
        })
        .collect()
}

#[test]
fn view_transform_builds_a_distinct_heterogeneous_tree_with_provenance() {
    let mut engine = Engine::new();
    install(&mut engine);
    set_program(
        &mut engine,
        "memory://shape",
        SurfaceProgram {
            left: 7,
            right_name: Some("answer".into()),
        },
    );

    let snapshot = engine.snapshot();
    let roots = snapshot.tree_roots_of::<CoreTree>(&"memory://shape".to_owned());
    assert_eq!(roots.len(), 1);
    assert_eq!(
        core_payloads(&snapshot, roots[0].clone()),
        vec![
            CoreNode::Module,
            CoreNode::LetBinding,
            CoreNode::ApplyAdd,
            CoreNode::Integer(7),
            CoreNode::Reference("answer".into()),
        ]
    );

    for target in core_nodes(&snapshot, roots[0].clone()) {
        let source = snapshot
            .observe::<CoreOrigin>(target)
            .expect("target provenance");
        assert!(
            snapshot.tree_payload::<SurfaceTree>(source.as_ref().clone()).is_some(),
            "every target node must join to a live source node"
        );
    }
}

#[test]
fn payload_updates_preserve_topology_and_leave_other_documents_cold() {
    let mut engine = Engine::new();
    install(&mut engine);
    set_program(
        &mut engine,
        "memory://a",
        SurfaceProgram {
            left: 1,
            right_name: None,
        },
    );
    set_program(
        &mut engine,
        "memory://b",
        SurfaceProgram {
            left: 9,
            right_name: Some("kept".into()),
        },
    );

    let before = engine.snapshot();
    let a_root = before.tree_roots_of::<CoreTree>(&"memory://a".to_owned())[0].clone();
    let a_nodes = core_nodes(&before, a_root.clone());
    let b_root = before.tree_roots_of::<CoreTree>(&"memory://b".to_owned())[0].clone();
    let b_payloads = core_payloads(&before, b_root.clone());

    set_program(
        &mut engine,
        "memory://a",
        SurfaceProgram {
            left: 2,
            right_name: None,
        },
    );

    let after = engine.snapshot();
    let a_after_root = after.tree_roots_of::<CoreTree>(&"memory://a".to_owned())[0].clone();
    assert_eq!(a_after_root.clone(), a_root, "root identity is source-root-derived");
    assert_eq!(
        core_nodes(&after, a_after_root.clone()),
        a_nodes,
        "a payload fact must not recreate target topology"
    );
    assert_eq!(
        core_payloads(&after, a_after_root.clone()),
        vec![
            CoreNode::Module,
            CoreNode::LetBinding,
            CoreNode::ApplyAdd,
            CoreNode::Integer(2),
        ]
    );
    assert_eq!(
        after.tree_roots_of::<CoreTree>(&"memory://b".to_owned()),
        vec![b_root.clone()],
        "a different source-tree domain stays cold"
    );
    assert_eq!(core_payloads(&after, b_root), b_payloads);
}

/// Exact reaction proof (plan §24.2, §24.7): a `left` value change
/// evaluates exactly the source bridge and the NUMBER payload producer.
/// The root/topology/payload components for every other node — and the
/// entire other document — stay cold, with zero broad enumerations.
#[test]
fn number_edit_evaluates_exactly_the_number_payload_producer() {
    let mut engine = Engine::new();
    install(&mut engine);
    set_program(
        &mut engine,
        "memory://a",
        SurfaceProgram {
            left: 1,
            right_name: None,
        },
    );
    set_program(
        &mut engine,
        "memory://b",
        SurfaceProgram {
            left: 9,
            right_name: Some("kept".into()),
        },
    );

    let report = set_program_report(
        &mut engine,
        "memory://a",
        SurfaceProgram {
            left: 2,
            right_name: None,
        },
    );
    let digest = report.metric::<plingo::reactive::ReactionDigest>().expect("digest");

    assert!(
        digest.broad_enumerations.is_empty(),
        "{:#?}",
        digest.broad_enumerations
    );
    assert!(
        digest.retirements.is_empty(),
        "{:#?}",
        digest.retirements
    );

    let mut definitions: Vec<String> = digest
        .evaluations
        .iter()
        .map(|evaluation| evaluation.definition.to_string())
        .collect();
    definitions.sort();
    definitions.dedup();
    assert_eq!(
        definitions,
        vec![
            "tree_transform::view_harness::lower_number_payload".to_owned(),
            "tree_transform::view_harness::split_surface_program".to_owned(),
        ],
        "{:#?}",
        digest.evaluations
    );

    // Exact driving elements: only document A's key.
    for evaluation in &digest.evaluations {
        assert_eq!(evaluation.driving_element, "\"memory://a\"");
    }
}

fn set_program_report(
    engine: &mut Engine,
    uri: &str,
    program: SurfaceProgram,
) -> plingo::reactive::CommandReport {
    engine
        .command(|| emit_view::<SurfacePrograms>()?.insert(uri.to_owned(), program.clone()))
        .expect("program command")
}

#[test]
fn child_order_update_adds_one_target_link_without_recreating_prefix_nodes() {
    let mut engine = Engine::new();
    install(&mut engine);
    set_program(
        &mut engine,
        "memory://topology",
        SurfaceProgram {
            left: 1,
            right_name: None,
        },
    );

    let before = engine.snapshot();
    let root = before.tree_roots_of::<CoreTree>(&"memory://topology".to_owned())[0].clone();
    let prefix = core_nodes(&before, root.clone());
    assert_eq!(prefix.len(), 4);

    set_program(
        &mut engine,
        "memory://topology",
        SurfaceProgram {
            left: 1,
            right_name: Some("later".into()),
        },
    );

    let after = engine.snapshot();
    let after_root =
        after.tree_roots_of::<CoreTree>(&"memory://topology".to_owned())[0].clone();
    let nodes = core_nodes(&after, after_root.clone());
    assert_eq!(after_root.clone(), root);
    assert_eq!(&nodes[..prefix.len()], prefix.as_slice());
    assert_eq!(
        core_payloads(&after, after_root.clone()),
        vec![
            CoreNode::Module,
            CoreNode::LetBinding,
            CoreNode::ApplyAdd,
            CoreNode::Integer(1),
            CoreNode::Reference("later".into()),
        ]
    );

    let add = nodes[2].clone();
    let children = after.tree_children::<CoreTree>(add);
    assert_eq!(
        children,
        nodes[3..].to_vec(),
        "only add's child order grows"
    );
}

#[test]
fn removing_the_source_program_retracts_the_target_forest() {
    let mut engine = Engine::new();
    install(&mut engine);
    let uri = "memory://remove";
    set_program(
        &mut engine,
        uri,
        SurfaceProgram {
            left: 5,
            right_name: None,
        },
    );
    let before = engine.snapshot();
    let root = before.tree_roots_of::<CoreTree>(&uri.to_owned())[0].clone();

    engine
        .command(|| emit_view::<SurfacePrograms>()?.remove(uri.to_owned()))
        .expect("remove program");
    let after = engine.snapshot();
    assert!(after.tree_roots_of::<CoreTree>(&uri.to_owned()).is_empty());
    assert!(after.observe::<CoreOrigin>(root).is_none());
}

// ---------------------------------------------------------------------------
// Phase 0 oracles (follow-up plan §4): canonical fixture, reversible traces
// with expected keyed deltas, document isolation, and cold equivalence.
// ---------------------------------------------------------------------------

use plingo::reactive::digest::{FamilyState, SemanticDigest, render_diff};

use super::view_harness::semantic_digest;

fn state_of(engine: &Engine) -> FamilyState {
    let snapshot = engine.snapshot();
    FamilyState::capture(semantic_digest(&snapshot), &snapshot)
}

/// The exact digest row value of one structural path in one view family.
fn row<'a>(digest: &'a SemanticDigest, view: &'a str, key: &str) -> &'a str {
    digest
        .rows_of(view)
        .iter()
        .find(|(row_key, _)| *row_key == format!("{view}::{key}"))
        .map(|(_, value)| *value)
        .unwrap_or("absent")
}

fn surf(doc: &str, path: &str) -> String {
    format!("surface:{doc}#{path}")
}

fn core(doc: &str, path: &str) -> String {
    format!("core:{doc}#{path}")
}

fn standard_program(left: i64) -> SurfaceProgram {
    SurfaceProgram {
        left,
        right_name: Some("answer".into()),
    }
}

fn seed_standard(engine: &mut Engine) {
    set_program(&mut *engine, "memory://answer", standard_program(7));
}

/// Canonical single-root fixture (plan §4 item 13): the complete public-view
/// content of the standard program is hand-authored. A warm and cold
/// implementation sharing the same extra/orphan output must still fail this.
#[test]
fn canonical_fixture_matches_hand_authored_rows() {
    let mut engine = Engine::new();
    install(&mut engine);
    seed_standard(&mut engine);
    let digest = semantic_digest(&engine.snapshot());

    let doc = "memory://answer";
    let s = |path: &str| surf(doc, path);
    let c = |path: &str| core(doc, path);
    let expected: &[(&str, String, String)] = &[
        (
            "programs",
            doc.into(),
            "program{left:7,right_name:some(\"answer\")}".into(),
        ),
        ("surface_roots_map", doc.into(), s("0")),
        ("surface_roots", doc.into(), format!("[{}]", s("0"))),
        ("surface_tree", s("0"), "Document".into()),
        ("surface_tree", s("0.0"), "Binding".into()),
        ("surface_tree", s("0.0.0"), "Add".into()),
        ("surface_tree", s("0.0.0.0"), "Number(7)".into()),
        ("surface_tree", s("0.0.0.1"), "Name(\"answer\")".into()),
        ("surface_parent", s("0"), "none".into()),
        ("surface_parent", s("0.0"), s("0")),
        ("surface_parent", s("0.0.0"), s("0.0")),
        ("surface_parent", s("0.0.0.0"), s("0.0.0")),
        ("surface_parent", s("0.0.0.1"), s("0.0.0")),
        ("core_roots", doc.into(), format!("[{}]", c("0"))),
        ("core_tree", c("0"), "Module".into()),
        ("core_tree", c("0.0"), "LetBinding".into()),
        ("core_tree", c("0.0.0"), "ApplyAdd".into()),
        ("core_tree", c("0.0.0.0"), "Integer(7)".into()),
        ("core_tree", c("0.0.0.1"), "Reference(\"answer\")".into()),
        ("core_parent", c("0"), "none".into()),
        ("core_parent", c("0.0"), c("0")),
        ("core_parent", c("0.0.0"), c("0.0")),
        ("core_parent", c("0.0.0.0"), c("0.0.0")),
        ("core_parent", c("0.0.0.1"), c("0.0.0")),
        ("core_origin", c("0"), s("0")),
        ("core_origin", c("0.0"), s("0.0")),
        ("core_origin", c("0.0.0"), s("0.0.0")),
        ("core_origin", c("0.0.0.0"), s("0.0.0.0")),
        ("core_origin", c("0.0.0.1"), s("0.0.0.1")),
    ];
    assert_eq!(digest.len(), expected.len(), "{}", digest.render());
    for (view, key, value) in expected {
        assert_eq!(row(&digest, view, key), value.as_str(), "row {view}::{key}");
    }
}

/// Plan §4 item 4 tree-transform number-payload trace: updating `left`
/// moves exactly the program row and the one Number/Integer payload row per
/// tree; a second document stays byte-identical; reversing restores the
/// exact initial state with an empty liveness audit; a cold replay of the
/// same membership matches the warm digest.
#[test]
fn left_payload_trace_is_exact_reversible_and_document_isolated() {
    let mut engine = Engine::new();
    install(&mut engine);
    seed_standard(&mut engine);
    set_program(
        &mut engine,
        "memory://other",
        SurfaceProgram {
            left: 1,
            right_name: None,
        },
    );
    let initial = state_of(&engine);
    set_program(&mut engine, "memory://answer", standard_program(9));
    let after = state_of(&engine);
    let diff = render_diff(&initial.digest, &after.digest);
    let lines: Vec<&str> = diff.lines().collect();
    assert_eq!(
        lines,
        vec![
            format!(
                "~ core_tree::{} = Integer(7) -> Integer(9)",
                core("memory://answer", "0.0.0.0")
            )
            .as_str(),
            format!(
                "~ programs::memory://answer = program{{left:7,right_name:some(\"answer\")}} -> program{{left:9,right_name:some(\"answer\")}}"
            )
            .as_str(),
            format!(
                "~ surface_tree::{} = Number(7) -> Number(9)",
                surf("memory://answer", "0.0.0.0")
            )
            .as_str(),
        ],
        "{diff}"
    );
    assert!(!diff.contains("memory://other"), "{diff}");

    // Reverse restores the exact initial family state.
    set_program(&mut engine, "memory://answer", standard_program(7));
    let restored = state_of(&engine);
    assert_eq!(
        restored,
        initial,
        "{}",
        render_diff(&initial.digest, &restored.digest)
    );
    assert!(engine.__liveness_audit().is_empty());

    // Cold replay of the identical final membership.
    let mut cold = Engine::new();
    install(&mut cold);
    set_program(&mut cold, "memory://answer", standard_program(7));
    set_program(
        &mut cold,
        "memory://other",
        SurfaceProgram {
            left: 1,
            right_name: None,
        },
    );
    let cold_state = state_of(&cold);
    assert_eq!(
        restored.digest,
        cold_state.digest,
        "{}",
        render_diff(&restored.digest, &cold_state.digest)
    );
}

/// Same-URI optional-child trace: `Some(name) -> None -> Some(name)`.
/// At the `None` step the optional source/target payloads, their origin
/// entries, and their parent links are absent while document roots and all
/// mandatory siblings stay byte-equal; reinsertion restores the exact
/// initial family state.
#[test]
fn optional_child_trace_restores_the_exact_initial_state() {
    let mut engine = Engine::new();
    install(&mut engine);
    seed_standard(&mut engine);
    let baseline = state_of(&engine);
    let doc = "memory://answer";

    // Forward: drop the optional name child.
    set_program(
        &mut engine,
        doc,
        SurfaceProgram {
            left: 7,
            right_name: None,
        },
    );
    let without = state_of(&engine);
    let diff = render_diff(&baseline.digest, &without.digest);
    let mut lines: Vec<&str> = diff.lines().collect();
    lines.sort_unstable();
    let expected_removed = [
        format!(
            "- surface_tree::{} = Name(\"answer\")",
            surf(doc, "0.0.0.1")
        ),
        format!(
            "- surface_parent::{} = {}",
            surf(doc, "0.0.0.1"),
            surf(doc, "0.0.0")
        ),
        format!(
            "- core_tree::{} = Reference(\"answer\")",
            core(doc, "0.0.0.1")
        ),
        format!(
            "- core_parent::{} = {}",
            core(doc, "0.0.0.1"),
            core(doc, "0.0.0")
        ),
        format!(
            "- core_origin::{} = {}",
            core(doc, "0.0.0.1"),
            surf(doc, "0.0.0.1")
        ),
    ];
    let expected_changed = format!(
        "~ programs::{doc} = program{{left:7,right_name:some(\"answer\")}} -> program{{left:7,right_name:none}}"
    );
    let mut expected_lines: Vec<String> = expected_removed.to_vec();
    expected_lines.push(expected_changed);
    let mut expected_sorted: Vec<&str> = expected_lines.iter().map(String::as_str).collect();
    expected_sorted.sort_unstable();
    assert_eq!(lines, expected_sorted, "{diff}");

    // Exact absence of every optional element at the None step.
    for view in [
        "surface_tree",
        "surface_parent",
        "core_tree",
        "core_parent",
        "core_origin",
    ] {
        let key_suffix = if view.starts_with("core") {
            core(doc, "0.0.0.1")
        } else {
            surf(doc, "0.0.0.1")
        };
        assert_eq!(
            row(&without.digest, view, &key_suffix),
            "absent",
            "{view} must not carry the optional child"
        );
    }

    // Document roots and mandatory siblings are byte-equal throughout.
    for (view, key) in [
        ("surface_roots_map", doc),
        ("surface_roots", doc),
        ("core_roots", doc),
    ] {
        assert_eq!(
            row(&without.digest, view, key),
            row(&baseline.digest, view, key),
            "document root row {view}::{key} must be byte-equal"
        );
    }
    for (view, key) in [
        ("surface_tree", surf(doc, "0")),
        ("surface_tree", surf(doc, "0.0")),
        ("surface_tree", surf(doc, "0.0.0")),
        ("surface_tree", surf(doc, "0.0.0.0")),
        ("core_tree", core(doc, "0")),
        ("core_tree", core(doc, "0.0")),
        ("core_tree", core(doc, "0.0.0")),
        ("core_tree", core(doc, "0.0.0.0")),
        ("core_parent", surf(doc, "0")),
        ("core_parent", core(doc, "0")),
        ("core_origin", core(doc, "0")),
        ("core_origin", core(doc, "0.0")),
        ("core_origin", core(doc, "0.0.0")),
        ("core_origin", core(doc, "0.0.0.0")),
    ] {
        assert_eq!(
            row(&without.digest, view, &key),
            row(&baseline.digest, view, &key),
            "mandatory sibling {view}::{key} must be byte-equal"
        );
    }

    // Reinsertion restores the exact initial family state.
    set_program(&mut engine, doc, standard_program(7));
    let reopened = state_of(&engine);
    assert_eq!(
        reopened,
        baseline,
        "{}",
        render_diff(&baseline.digest, &reopened.digest)
    );
    assert_eq!(reopened.live_facts, baseline.live_facts);
    assert!(engine.__liveness_audit().is_empty());

    // Cold replay of the reinserted membership matches the warm digest.
    let mut cold = Engine::new();
    install(&mut cold);
    seed_standard(&mut cold);
    let cold_state = state_of(&cold);
    assert_eq!(
        reopened.digest,
        cold_state.digest,
        "{}",
        render_diff(&reopened.digest, &cold_state.digest)
    );
}
