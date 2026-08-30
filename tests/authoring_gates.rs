//! Repository-wide authoring gates (follow-up plan Cut K §25):
//!
//! The final proof must show that no example or public test imports the
//! forbidden authoring surface — raw ordinary planning (`run`/`run_each_*`),
//! ordinal family installation (`install_keyed`), manual graph identities
//! (`Scope::anchored`), hidden fresh-ID APIs, or identity-carrier views —
//! except where the plan explicitly retains framework-internal and
//! transaction-test primitives.
//!
//! These gates scan source text at test time so a regression in any example
//! fails the suite instead of drifting into the release artifact.

use std::path::Path;

/// Every example file that must use only the first-class authoring surface
/// (Cut C exit gate): `#[component]` definitions, typed ports, generated
/// automatic identities, and raw effects only inside component bodies.
const COMPONENT_AUTHORED_FILES: &[&str] = &[
    "examples/tree_transform/lower.rs",
    "examples/tree_transform/view_harness.rs",
    "examples/view_pipeline/fanout_components.rs",
    "examples/view_pipeline/scope_lowering.rs",
    "examples/stlc/name_resolve.rs",
    "examples/stlc/check.rs",
    "examples/stlc/structural.rs",
];

/// Patterns that must never appear in component-authored example files.
const FORBIDDEN_IN_COMPONENT_FILES: &[(&str, &str)] = &[
    ("run_each_key", "raw run_each_key authoring"),
    ("run_each_child", "raw run_each_child authoring"),
    ("Scope::anchored", "manual anchored scope identity"),
    ("fresh_node_id", "hidden fresh-ID API"),
    ("automatic_effect_node_id", "hidden automatic-ID API"),
    ("install_keyed", "ordinal family installation"),
    ("NodeUris", "identity-carrier view"),
    ("ScopeRole", "identity-only role enum"),
    (
        "ScopeInput",
        "identity-carrier input struct (URI/parent carrier)",
    ),
    ("LowerInput", "identity-carrier input struct"),
    // Plan §9.5 negative gates: no raw tree ABI, raw handles, ports, or
    // generated installers in application-authored files.
    ("TreeKey", "encoded tree fact key"),
    ("TreeFact", "encoded tree fact value"),
    ("GraphKey<", "encoded graph fact key"),
    ("GraphFact<", "encoded graph fact value"),
    ("Node<V>", "raw runtime node handle"),
    ("view::Node", "raw runtime node handle"),
    ("emit_view", "raw emit handle"),
    ("observe_view", "raw observe handle"),
    ("emit_patch", "raw patch handle"),
    ("EachKey<", "legacy membership port"),
    ("fresh_node_id", "hidden node-id mint"),
    ("raw_id", "raw identity accessor"),
    ("_install(engine", "generated installer call"),
];

/// Files that must additionally be free of nested effectful `run`
/// recursion (Cut C: "Nested effectful run recursion becomes either a
/// named element component or a pure helper"). The STLC checker retains
/// its documented directional-cut recursion until Cut J lands.
const NO_NESTED_RUN_FILES: &[&str] = &[
    "examples/tree_transform/lower.rs",
    "examples/tree_transform/view_harness.rs",
    "examples/view_pipeline/fanout_components.rs",
    "examples/view_pipeline/scope_lowering.rs",
    "examples/stlc/name_resolve.rs",
    "examples/stlc/structural.rs",
];

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn component_authored_examples_avoid_forbidden_authoring_surface() {
    for relative in COMPONENT_AUTHORED_FILES {
        let path = repo_root().join(relative);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} unreadable: {error}", path.display()));
        for (pattern, reason) in FORBIDDEN_IN_COMPONENT_FILES {
            assert!(
                !source.contains(pattern),
                "{} uses {pattern:?} ({reason}); component-authored files must use \
                 typed ports and generated identities",
                relative
            );
        }
        if NO_NESTED_RUN_FILES.contains(&relative) {
            assert!(
                !source.contains("run("),
                "{} uses nested effectful run recursion; convert it to a named \
                 element component or a pure helper",
                relative
            );
        }
    }
}

/// The scope-pipeline source keeps one named component per semantic stage:
/// recursive tree lowering, scope projection, resolution, analysis, and
/// summary.  The test deliberately checks the public component definitions,
/// not generated installer names or runtime topology.
#[test]
fn scope_lowering_installs_element_components() {
    let path = repo_root().join("examples/view_pipeline/scope_lowering.rs");
    let source = std::fs::read_to_string(&path).expect("scope lowering readable");
    for definition in [
        "pub fn lower_node(",
        "pub fn emit_document_scope(",
        "pub fn emit_node_scope(",
        "pub fn publish_candidate(",
        "pub fn resolve_pass(",
        "pub fn analysis_label(",
        "pub fn analysis_origin(",
        "pub fn analysis_scope_presence(",
        "pub fn analysis_diagnostics(",
        "pub fn join_analyses(",
        "pub fn node_summary(",
        "pub fn document_summary(",
    ] {
        assert!(
            source.contains(definition),
            "scope lowering missing element component {definition:?}"
        );
    }
    // The old recursive walkers are gone; per-node components read only
    // their exact children summaries.
    for old_walker in ["fn summarize_node(", "fn scope_node(", "fn analyze_node("] {
        assert!(
            !source.contains(old_walker),
            "recursive walker {old_walker:?} remains in scope lowering"
        );
    }
}

/// The tree-transform lowering must keep its recursive per-node components
/// (plan §8 Cut G): one component per source node kind, visibly recursive
/// through component calls.
#[test]
fn tree_transform_keeps_recursive_lowering_components() {
    let path = repo_root().join("examples/tree_transform/lower.rs");
    let source = std::fs::read_to_string(&path).expect("lower.rs readable");
    for definition in [
        "pub fn lower_document(",
        "pub fn lower_declaration(",
        "pub fn lower_expr(",
    ] {
        assert!(
            source.contains(definition),
            "tree transform missing projection component {definition:?}"
        );
    }
    // The lowering visibly recurses by calling components, never by manual
    // child enumeration into identity maps.
    assert!(
        source.contains("lower_expr(add.left()?)"),
        "lower_expr must recurse through component calls"
    );
}

/// Positive gates (plan §9.5): lowered tree enums visibly use `AstBox`
/// children and lowering components visibly recurse by calling components.
#[test]
fn lowered_trees_use_ast_box_children_and_recursive_calls() {
    for (path, marker) in [
        ("examples/tree_transform/lower.rs", "AstBox<LoweredExpr>"),
        (
            "examples/tree_transform/view_harness.rs",
            "AstBox<CoreExpr>",
        ),
        (
            "examples/view_pipeline/scope_lowering.rs",
            "AstBox<LoweredNode>",
        ),
    ] {
        let source = std::fs::read_to_string(repo_root().join(path))
            .unwrap_or_else(|error| panic!("{path} readable: {error}"));
        assert!(
            source.contains(marker),
            "{path} does not declare {marker:?} children"
        );
    }
    let scope =
        std::fs::read_to_string(repo_root().join("examples/view_pipeline/scope_lowering.rs"))
            .expect("scope lowering readable");
    assert!(
        scope.contains("node_summary(node: AstBox<LoweredNode>)"),
        "summaries must flow through per-node components reading child summaries"
    );
}
