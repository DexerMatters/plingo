//! Integration tests for the STLC syntax and its incremental components.

use std::sync::Arc;

use plingo::{
    ComponentDiagnostics, Output, Workspace,
    component::{
        lex::LexerNode,
        parse::{ParseCandidates, ParseSnapshot, ParsedAst, ParserNode, grammar::Grammar},
        scope::{ScopeAllocations, ScopeStructure, SourceRequirements},
        source::{SourceEdit, SourceInput},
        structural::{StructureEdges, StructureEntries, StructureEntry, StructureNode},
    },
    scheme::node::{Graph, NodeError, ReadGraph},
    utils::{PrettyDisplay, Span},
    visual::{AstTree, ScopeGraph},
};

use super::{
    check::{StlcTypeDiagnostic, StlcTypeMode, TypeDocument, TypeOf, install_type_components},
    name_resolve::{NameDocument, StlcScope, StlcScopeData, StlcScopeKey, install_name_components},
    structural::{
        StlcLowered, StlcLoweredOrigin, StlcLoweredSummary, StlcLoweringDiagnostics, StlcNodeIndex,
        StlcNodeSummary, install_structural_pipeline,
    },
    syntax::{StlcDeclaration, StlcDocument, StlcToken},
};

/// Serializes every observable fact family of the STLC pipeline so two graphs
/// can be compared for equality.
fn observable_facts(
    graph: &Graph,
    uri: fluent_uri::Uri<&'static str>,
) -> anyhow::Result<Vec<String>> {
    let mut facts = Vec::new();
    let snapshot = graph
        .get::<ParseSnapshot<StlcToken>>(uri)
        .expect("parser snapshot");
    let mut keys: Vec<_> = snapshot.ast_keys().collect();
    keys.sort();
    for key in keys {
        facts.push(format!(
            "parsed:{key:?}={:?}",
            graph.get::<ParsedAst<StlcToken>>(key.clone())
        ));
        facts.push(format!(
            "index:{key:?}={:?}",
            graph.get::<StructureNode<StlcNodeIndex>>(key.clone())
        ));
        facts.push(format!(
            "summary:{key:?}={:?}",
            graph.get::<StructureNode<StlcNodeSummary>>(key.clone())
        ));
        facts.push(format!(
            "lowered:{key:?}={:?}",
            graph.get::<StructureNode<StlcLowered>>(key.clone())
        ));
        facts.push(format!(
            "origin:{key:?}={:?}",
            graph.get::<StlcLoweredOrigin>(key.clone())
        ));
        facts.push(format!(
            "lower-diag:{key:?}={:?}",
            graph.get::<StlcLoweringDiagnostics>(key.clone())
        ));
    }
    let mut allocations: Vec<_> = graph.scan_all::<ScopeAllocations<StlcScope>>();
    allocations.sort_by_key(|allocation| allocation.scope);
    for allocation in allocations {
        facts.push(format!(
            "scope-data:{:?}={:?}",
            allocation.scope,
            graph.get::<StructureNode<ScopeStructure<StlcScope>>>(allocation.scope)
        ));
    }
    let mut edges: Vec<_> = graph.scan_all::<StructureEdges<ScopeStructure<StlcScope>>>();
    edges.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.target.cmp(&right.target))
    });
    for edge in edges {
        facts.push(format!("scope-edge:{:?}->{:?}", edge.source, edge.target));
    }
    let mut requirements: Vec<_> = graph.scan_all::<SourceRequirements<StlcScope>>();
    requirements.sort();
    for requirement in requirements {
        facts.push(format!("require:{requirement:?}"));
    }
    facts.push(format!(
        "name-doc={:?}",
        graph.get::<Output<NameDocument>>(NameDocument { uri })
    ));
    facts.push(format!(
        "type-doc={:?}",
        graph.get::<Output<TypeDocument>>(TypeDocument { uri })
    ));
    facts.sort();
    Ok(facts)
}

fn graph_for_stlc() -> anyhow::Result<Graph> {
    let parser = Grammar::from_spec::<StlcDocument>().build_lr1::<StlcToken>();
    let mut graph = Graph::new();
    graph.install(LexerNode::<StlcToken>::new()?)?;
    graph.install(ParserNode::<StlcToken, StlcDocument>::from_parser(parser))?;
    ScopeStructure::<StlcScope>::install(&mut graph)?;
    install_name_components(&mut graph)?;
    install_type_components(&mut graph)?;
    install_structural_pipeline(&mut graph)?;
    Ok(graph)
}

fn load_document(
    graph: &mut Graph,
    uri: fluent_uri::Uri<&'static str>,
    text: &str,
) -> anyhow::Result<()> {
    graph.command(SourceInput::load(uri))?;
    graph.command(SourceInput::apply(SourceEdit::Insert {
        key: Span::point_uri(uri, 0).unwrap(),
        value: text.into(),
    }))?;
    graph.demand::<ParserNode<StlcToken, StlcDocument>>(uri)?;
    graph.request(NameDocument { uri })?;
    graph.request(TypeDocument { uri })?;
    Ok(())
}

#[test]
fn components_publish_scope_and_type_results() -> anyhow::Result<()> {
    let uri = Span::new("test://stlc-example", 0, 0).unwrap().uri;
    let mut graph = graph_for_stlc()?;
    load_document(&mut graph, uri, "f : Nat -> Nat := ()")?;

    let candidates = graph
        .get::<ParseCandidates<StlcToken, StlcDocument>>(uri)
        .ok_or_else(|| anyhow::anyhow!("parser did not publish candidates"))?;
    let snapshot = graph
        .get::<ParseSnapshot<StlcToken>>(uri)
        .ok_or_else(|| anyhow::anyhow!("parser did not publish snapshot"))?;
    let root = candidates[0].ast_box.key();
    let StlcDocument::Lines(declarations) = candidates[0].value.as_ref() else {
        anyhow::bail!("expected an STLC document");
    };
    let declaration_value = declarations[0]
        .resolve(&snapshot)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let StlcDeclaration::Value(_, _, Some(annotation), body) = &*declaration_value else {
        anyhow::bail!("expected an annotated value declaration");
    };

    let document_scope = graph
        .scan::<ScopeAllocations<StlcScope>>(StlcScopeKey::Document(uri))
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("name component did not allocate document scope"))?
        .scope;
    let lexical_scope = graph
        .scan::<ScopeAllocations<StlcScope>>(StlcScopeKey::Lexical(root.clone()))
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("name component did not allocate lexical scope"))?
        .scope;
    assert_eq!(
        graph
            .get::<StructureNode<ScopeStructure<StlcScope>>>(document_scope)
            .and_then(|artifact| artifact.deref::<StlcScopeData>()),
        Some(Arc::new(StlcScopeData::Document)),
    );
    assert_eq!(
        graph.scan::<StructureEntries<ScopeStructure<StlcScope>, _, ()>>(uri),
        vec![StructureEntry::new(uri, document_scope, ())],
    );

    let expected = super::name_resolve::StlcTypeValue::Arrow(
        Box::new(super::name_resolve::StlcTypeValue::Nat),
        Box::new(super::name_resolve::StlcTypeValue::Nat),
    );
    let annotation_key = annotation.key();
    let annotation_task = TypeOf {
        ast: annotation_key,
        incoming: lexical_scope,
        mode: StlcTypeMode::Infer,
    };
    assert_eq!(
        graph.get::<Output<TypeOf>>(annotation_task),
        Some(Some(expected.clone())),
    );

    let declaration_scope = graph
        .scan::<ScopeAllocations<StlcScope>>(StlcScopeKey::Lexical(declarations[0].key()))
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("name component did not allocate declaration scope"))?
        .scope;
    let body_task = TypeOf {
        ast: body.key(),
        incoming: declaration_scope,
        mode: StlcTypeMode::Check(expected.clone()),
    };
    let body_result = graph
        .get::<Output<TypeOf>>(body_task.clone())
        .ok_or_else(|| anyhow::anyhow!("type component did not publish body result"))?;
    assert!(body_result.is_none());
    assert!(matches!(
        graph
            .get::<ComponentDiagnostics<TypeOf, StlcTypeDiagnostic>>(body_task)
            .as_deref(),
        Some([StlcTypeDiagnostic { expression, error: super::name_resolve::StlcTypeError::Mismatch { expected: found_expected, found: super::name_resolve::StlcTypeValue::Unit } }])
            if *expression == body.key() && *found_expected == expected
    ));
    Ok(())
}

#[test]
fn structural_pipeline_and_components_retract_removed_roots() -> anyhow::Result<()> {
    let uri = Span::new("test://stlc-edit", 0, 0).unwrap().uri;
    let mut graph = graph_for_stlc()?;
    load_document(&mut graph, uri, "f : Nat := ()")?;
    let root = graph
        .get::<ParseCandidates<StlcToken, StlcDocument>>(uri)
        .expect("parser candidate")[0]
        .ast_box
        .key();
    assert_eq!(
        graph
            .get::<StructureNode<StlcNodeSummary>>(root.clone())
            .and_then(|artifact| artifact.deref::<String>()),
        Some(Arc::new("Document".to_owned())),
    );
    assert!(
        graph
            .get::<StructureNode<StlcLoweredSummary>>(root.clone())
            .is_some()
    );
    let scope = graph
        .scan::<ScopeAllocations<StlcScope>>(StlcScopeKey::Document(uri))
        .into_iter()
        .next()
        .expect("document scope")
        .scope;

    graph.command(SourceInput::apply(SourceEdit::Delete {
        key: Span::new_uri(uri, 0, 13).unwrap(),
    }))?;
    graph.demand::<ParserNode<StlcToken, StlcDocument>>(uri)?;
    graph.request(NameDocument { uri })?;
    graph.request(TypeDocument { uri })?;
    assert!(graph.get::<StructureNode<StlcNodeSummary>>(root).is_none());
    assert!(
        graph
            .get::<StructureNode<ScopeStructure<StlcScope>>>(scope)
            .is_none()
    );
    Ok(())
}

#[test]
fn parser_facts_retain_unchanged_ast_keys() -> anyhow::Result<()> {
    let uri = Span::new("test://stlc-parser-deltas", 0, 0).unwrap().uri;
    let mut graph = graph_for_stlc()?;
    load_document(&mut graph, uri, "x := 0\ny := 1")?;
    let candidates = graph
        .get::<ParseCandidates<StlcToken, StlcDocument>>(uri)
        .expect("parser candidate")
        .clone();
    let snapshot = graph
        .get::<ParseSnapshot<StlcToken>>(uri)
        .expect("snapshot");
    let document = candidates[0]
        .ast_box
        .resolve(&snapshot)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let StlcDocument::Lines(declarations) = &*document else {
        anyhow::bail!("expected lines");
    };
    let unchanged = declarations[1].key();
    let previous = graph
        .get::<ParsedAst<StlcToken>>(unchanged.clone())
        .expect("artifact");
    graph.command(SourceInput::apply_all(vec![
        SourceEdit::Delete {
            key: Span::new_uri(uri, 5, 6).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(uri, 5).unwrap(),
            value: "2".into(),
        },
    ]))?;
    graph.demand::<ParserNode<StlcToken, StlcDocument>>(uri)?;
    assert_eq!(graph.get::<ParsedAst<StlcToken>>(unchanged), Some(previous));
    Ok(())
}

#[test]
fn workspace_configures_the_graph_directly() -> anyhow::Result<()> {
    let uri = Span::new("test://stlc-workspace", 0, 0).unwrap().uri;
    let workspace = Workspace::build(|graph| {
        graph.install(
            LexerNode::<StlcToken>::new().map_err(|error| NodeError::message(error.to_string()))?,
        )?;
        graph.install(ParserNode::<StlcToken, StlcDocument>::from_parser(
            Grammar::from_spec::<StlcDocument>().build_lr1::<StlcToken>(),
        ))?;
        ScopeStructure::<StlcScope>::install(graph)?;
        install_name_components(graph)?;
        install_type_components(graph)?;
        Ok(())
    })?;
    let document = workspace.open::<ParserNode<StlcToken, StlcDocument>>(uri, "x := 0")?;
    assert!(
        document
            .artifact::<ParseSnapshot<StlcToken>>()?
            .ast_keys()
            .next()
            .is_some()
    );
    let before = workspace.revision();
    document.apply(SourceEdit::Insert {
        key: Span::point_uri(uri, 0).unwrap(),
        value: "\n".into(),
    })?;
    assert!(workspace.revision() > before);
    Ok(())
}

#[test]
fn structural_views_publish_all_downstream_products() -> anyhow::Result<()> {
    let uri = Span::new("test://stlc-structural", 0, 0).unwrap().uri;
    let mut graph = graph_for_stlc()?;
    load_document(&mut graph, uri, "id : Nat -> Nat := fun x -> x")?;
    let root = graph
        .get::<ParseCandidates<StlcToken, StlcDocument>>(uri)
        .expect("parser candidate")[0]
        .ast_box
        .key();
    assert_eq!(
        graph
            .get::<StructureNode<StlcLowered>>(root.clone())
            .and_then(|artifact| artifact.deref::<String>()),
        Some(Arc::new("untyped::Document".to_owned())),
    );
    assert_eq!(
        graph.get::<StlcLoweredOrigin>(root.clone()),
        Some(root.clone())
    );
    assert_eq!(
        graph.get::<StlcLoweringDiagnostics>(root.clone()),
        Some(Arc::from([]))
    );
    assert_eq!(
        graph
            .get::<StructureNode<StlcLoweredSummary>>(root)
            .and_then(|artifact| artifact.deref::<String>()),
        Some(Arc::new("summary:untyped::Document".to_owned())),
    );
    Ok(())
}

#[test]
fn one_worker_and_many_worker_runs_produce_equal_facts() -> anyhow::Result<()> {
    let build = |workers: usize| -> anyhow::Result<Graph> {
        let parser = Grammar::from_spec::<StlcDocument>().build_lr1::<StlcToken>();
        let mut graph = Graph::with_workers(workers);
        graph.install(LexerNode::<StlcToken>::new()?)?;
        graph.install(ParserNode::<StlcToken, StlcDocument>::from_parser(parser))?;
        ScopeStructure::<StlcScope>::install(&mut graph)?;
        install_name_components(&mut graph)?;
        install_type_components(&mut graph)?;
        install_structural_pipeline(&mut graph)?;
        Ok(graph)
    };

    let uri = Span::new("test://stlc-determinism", 0, 0).unwrap().uri;
    let text = "f : Nat -> Nat := fun x -> x\nn : Nat := 0";
    let mut single = build(1)?;
    let mut many = build(8)?;
    load_document(&mut single, uri, text)?;
    load_document(&mut many, uri, text)?;

    let single_facts = observable_facts(&single, uri)?;
    let many_facts = observable_facts(&many, uri)?;
    assert_eq!(
        single_facts, many_facts,
        "worker count must not change any committed fact",
    );

    // Edited graphs must agree with a fresh cold graph built from the edited
    // source: warm equals cold for equivalent computations.
    let edit = SourceEdit::Insert {
        key: Span::point_uri(uri, 40).unwrap(),
        value: "\ny : Bool := true".into(),
    };
    single.command(SourceInput::apply(edit.clone()))?;
    many.command(SourceInput::apply(edit.clone()))?;
    for graph in [&mut single, &mut many] {
        graph.demand::<ParserNode<StlcToken, StlcDocument>>(uri)?;
        graph.request(NameDocument { uri })?;
        graph.request(TypeDocument { uri })?;
    }

    let mut cold = build(8)?;
    load_document(&mut cold, uri, &format!("{text}\ny : Bool := true"))?;

    // The untouched declaration's typed result must equal its cold
    // counterpart. Both graphs place it second, and the warm graph retains
    // its AST identity across the edit.
    let untouched_type =
        |graph: &Graph| -> anyhow::Result<Option<super::name_resolve::StlcTypeValue>> {
            let candidates = graph
                .get::<ParseCandidates<StlcToken, StlcDocument>>(uri)
                .expect("parser candidate");
            let snapshot = graph
                .get::<ParseSnapshot<StlcToken>>(uri)
                .expect("snapshot");
            let document = candidates[0]
                .ast_box
                .resolve(&snapshot)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let StlcDocument::Lines(declarations) = &*document else {
                anyhow::bail!("expected lines");
            };
            let declaration = declarations[1];
            let StlcDeclaration::Value(_, _, _, body) = &*declaration
                .resolve(&snapshot)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
            else {
                anyhow::bail!("expected a value declaration");
            };
            let declaration_scope = graph
                .scan::<ScopeAllocations<StlcScope>>(StlcScopeKey::Lexical(declaration.key()))
                .into_iter()
                .next()
                .expect("declaration scope")
                .scope;
            Ok(graph
                .get::<Output<TypeOf>>(TypeOf {
                    ast: body.key(),
                    incoming: declaration_scope,
                    mode: StlcTypeMode::Infer,
                })
                .flatten())
        };

    let warm = untouched_type(&single)?;
    let cold = untouched_type(&cold)?;
    assert_eq!(
        warm, cold,
        "warm edited graph must equal cold for equivalent computations"
    );
    assert_eq!(
        untouched_type(&many)?,
        cold,
        "warm many-worker graph must equal cold",
    );
    Ok(())
}

#[test]
fn edit_invalidates_only_affected_components() -> anyhow::Result<()> {
    let uri = Span::new("test://stlc-edit-local", 0, 0).unwrap().uri;
    let mut graph = graph_for_stlc()?;
    load_document(&mut graph, uri, "x := 0\ny := 1")?;

    let candidates = graph
        .get::<ParseCandidates<StlcToken, StlcDocument>>(uri)
        .expect("parser candidate")
        .clone();
    let snapshot = graph
        .get::<ParseSnapshot<StlcToken>>(uri)
        .expect("snapshot");
    let document = candidates[0]
        .ast_box
        .resolve(&snapshot)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let StlcDocument::Lines(declarations) = &*document else {
        anyhow::bail!("expected lines");
    };
    let untouched = declarations[1].key();
    let untouched_type = graph.get::<StructureNode<StlcNodeIndex>>(untouched.clone());

    graph.command(SourceInput::apply_all(vec![
        SourceEdit::Delete {
            key: Span::new_uri(uri, 0, 5).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: "z := 9".into(),
        },
    ]))?;
    graph.demand::<ParserNode<StlcToken, StlcDocument>>(uri)?;
    graph.request(NameDocument { uri })?;
    graph.request(TypeDocument { uri })?;

    assert_eq!(
        graph.get::<StructureNode<StlcNodeIndex>>(untouched),
        untouched_type,
        "an untouched declaration's structural facts must survive the edit",
    );
    Ok(())
}

#[test]
fn prints_ast_and_final_scope_graph_for_let_and_function_code() -> anyhow::Result<()> {
    let uri = Span::new("test://stlc-print", 0, 0).unwrap().uri;
    let code = r##"
id : Nat -> Nat := fun x -> x
mul (x : Nat) (y : Nat) : Nat -> Nat -> Nat := case x of zero -> 0 | succ p -> y + mul p y
"##;
    let mut graph = graph_for_stlc()?;
    load_document(&mut graph, uri, code)?;
    let snapshot = graph
        .get::<ParseSnapshot<StlcToken>>(uri)
        .ok_or_else(|| anyhow::anyhow!("parser did not publish a snapshot"))?;
    let candidates = graph
        .get::<ParseCandidates<StlcToken, StlcDocument>>(uri)
        .ok_or_else(|| anyhow::anyhow!("parser did not publish candidates"))?;
    println!("\n=== STLC source ===\n{code}");
    for candidate in candidates.iter() {
        println!("{}", AstTree::new(candidate.ast_box).pretty(&snapshot));
    }
    let display = format!(
        "{}",
        ScopeGraph::<StlcScope>::from_graph(&graph).pretty(&())
    );
    println!("\n=== STLC scope graph ===\n{display}");
    let allocations = graph.scan_all::<ScopeAllocations<StlcScope>>();
    let data = allocations
        .iter()
        .filter_map(|allocation| {
            graph
                .get::<StructureNode<ScopeStructure<StlcScope>>>(allocation.scope)
                .and_then(|artifact| artifact.deref::<StlcScopeData>())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        data.len(),
        allocations.len(),
        "every explicit scope allocation must publish exactly one scope-data value"
    );
    let mul_ty = super::name_resolve::StlcTypeValue::Arrow(
        Box::new(super::name_resolve::StlcTypeValue::Nat),
        Box::new(super::name_resolve::StlcTypeValue::Arrow(
            Box::new(super::name_resolve::StlcTypeValue::Nat),
            Box::new(super::name_resolve::StlcTypeValue::Nat),
        )),
    );
    assert!(
        data.iter()
            .any(|datum| datum.as_ref() == &StlcScopeData::Type(mul_ty.clone()))
    );
    assert_eq!(
        data.iter()
            .filter(|datum| matches!(datum.as_ref(), StlcScopeData::Type(_)))
            .count(),
        6,
        "declarations and binders publish one owner-stable type each"
    );
    Ok(())
}
