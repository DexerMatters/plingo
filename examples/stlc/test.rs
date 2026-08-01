//! Integration tests for the STLC syntax and its single heterogeneous elaborator.

use plingo::{
    component::{
        lex::LexerNode,
        parse::{ParseCandidates, ParseSnapshot, ParserNode, grammar::Grammar},
        scope::{ScopeAllocations, ScopeData, ScopeEdges, ScopeLifecycles},
        semantic::elaborators,
        source::{SourceEdit, SourceInput},
    },
    scheme::node::{Graph, ReadGraph},
    utils::{PrettyDisplay, Span},
    visual::{AstTree, ScopeGraph},
};

use super::{
    check::{StlcTypes, stlc_type_rules},
    name_resolve::{
        StlcNames, StlcScope, StlcScopeData, StlcScopeLabel, StlcTypeValue, stlc_name_rules,
    },
    syntax::{StlcDocument, StlcToken},
};

#[test]
fn elaborates_nested_let_and_lambda_tasks() -> anyhow::Result<()> {
    let uri = Span::new("test://stlc-example", 0, 0).unwrap().uri;
    let parser = Grammar::from_spec::<StlcDocument>().build_lr1::<StlcToken>();
    let mut graph = Graph::new();
    graph.install(LexerNode::<StlcToken>::new()?)?;
    graph.install(ParserNode::<StlcToken, StlcDocument>::from_parser(parser))?;
    elaborators::<StlcScope>()
        .rule::<StlcTypes, _>(stlc_type_rules())
        .rule::<StlcNames, _>(stlc_name_rules())
        .install(&mut graph)?;
    graph.command(SourceInput::load(uri))?;
    graph.command(SourceInput::apply(SourceEdit::Insert {
        key: Span::point_uri(uri, 0).unwrap(),
        value: "f := case succ 0 of zero -> 1 | succ x -> x".into(),
    }))?;

    let _parser = graph.demand::<ParserNode<StlcToken, StlcDocument>>(uri)?;

    let snapshot = graph
        .get::<ParseSnapshot<StlcToken>>(uri)
        .ok_or_else(|| anyhow::anyhow!("parser did not publish a snapshot"))?;
    let candidates = graph
        .get::<ParseCandidates<StlcToken, StlcDocument>>(uri)
        .ok_or_else(|| anyhow::anyhow!("parser did not publish candidates"))?;
    println!("\n=== STLC AST ===");
    for candidate in candidates.iter() {
        println!("{}", AstTree::new(candidate.ast_box).pretty(&snapshot));
    }
    println!(
        "\n=== STLC scope graph ===\n{}",
        ScopeGraph::<StlcScope>::from_graph(&graph).pretty(&())
    );

    let allocations = graph.scan_all::<ScopeAllocations<StlcScope>>();
    let data = allocations
        .iter()
        .filter_map(|allocation| graph.get::<ScopeData<StlcScope>>(allocation.scope))
        .collect::<Vec<_>>();
    assert_eq!(
        data.len(),
        allocations.len(),
        "every explicit scope allocation must publish exactly one scope-data value"
    );
    for name in ["f", "x"] {
        assert!(data.iter().any(|datum| {
            matches!(
                datum,
                StlcScopeData::Declaration { name: bound, .. } if bound.as_ref() == name
            )
        }));
    }
    assert!(
        data.iter()
            .any(|datum| matches!(datum, StlcScopeData::Type(StlcTypeValue::Nat)))
    );
    let edges = graph.scan_all::<ScopeEdges<StlcScope>>();
    assert!(
        edges
            .iter()
            .any(|edge| edge.label == StlcScopeLabel::Declaration)
    );
    let lifecycles = graph.scan_all::<ScopeLifecycles<StlcScope>>();
    assert!(!lifecycles.is_empty());
    assert!(lifecycles.iter().all(|state| state.is_closed()));
    assert!(edges.iter().any(|edge| edge.label == StlcScopeLabel::Type));
    Ok(())
}

#[test]
fn prints_ast_and_final_scope_graph_for_let_and_function_code() -> anyhow::Result<()> {
    let uri = Span::new("test://stlc-print", 0, 0).unwrap().uri;
    let code = r##"
id : Nat -> Nat := fun x -> x
mul (x : Nat) (y : Nat) : Nat -> Nat -> Nat := case x of zero -> 0 | succ p -> y + mul p y
"##;
    let parser = Grammar::from_spec::<StlcDocument>().build_lr1::<StlcToken>();
    let mut graph = Graph::new();
    graph.install(LexerNode::<StlcToken>::new()?)?;
    graph.install(ParserNode::<StlcToken, StlcDocument>::from_parser(parser))?;
    elaborators::<StlcScope>()
        .rule::<StlcTypes, _>(stlc_type_rules())
        .rule::<StlcNames, _>(stlc_name_rules())
        .install(&mut graph)?;
    graph.command(SourceInput::load(uri))?;
    graph.command(SourceInput::apply(SourceEdit::Insert {
        key: Span::point_uri(uri, 0).unwrap(),
        value: code.into(),
    }))?;

    let _parser = graph.demand::<ParserNode<StlcToken, StlcDocument>>(uri)?;
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
        .filter_map(|allocation| graph.get::<ScopeData<StlcScope>>(allocation.scope))
        .collect::<Vec<_>>();
    assert_eq!(data.len(), allocations.len());
    let mul_ty = StlcTypeValue::Arrow(
        Box::new(StlcTypeValue::Nat),
        Box::new(StlcTypeValue::Arrow(
            Box::new(StlcTypeValue::Nat),
            Box::new(StlcTypeValue::Nat),
        )),
    );
    assert!(data.contains(&StlcScopeData::Type(mul_ty)));
    assert_eq!(
        data.iter()
            .filter(|datum| matches!(datum, StlcScopeData::Type(_)))
            .count(),
        6,
        "declarations and binders publish one owner-stable type each"
    );
    Ok(())
}
