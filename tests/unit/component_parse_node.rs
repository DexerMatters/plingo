use std::fmt;

use plingo_macros::{NonTerminal, Terminal};

use super::*;
use crate::{
    component::{
        lex::{LexErrorInfo, LexerNode},
        parse::{AstToken, data::AstBox, grammar::Grammar},
        source::{SourceEdit, SourceInput},
    },
    scheme::node::{Graph, ReadGraph, ViewUpdate},
    utils::{RangeOrPoint, Span},
};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(root { Whitespace, Number })]
enum Tokens {
    #[regex(r"\s+")]
    #[skip]
    Whitespace,
    #[regex(r"[0-9]+")]
    Number(usize),
    #[error]
    Error(LexErrorInfo),
}

impl fmt::Display for Tokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[allow(dead_code)]
#[derive(NonTerminal, Debug, Clone)]
enum Value {
    #[rule(Tokens::Number)]
    Number(#[from(0)] AstToken<Tokens>),
}

#[allow(dead_code)]
#[derive(NonTerminal, Debug, Clone)]
enum AmbiguousValue {
    #[rule(Tokens::Number)]
    Decimal(#[from(0)] AstToken<Tokens>),
    #[rule(Tokens::Number)]
    Natural(#[from(0)] AstToken<Tokens>),
}

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(root { Whitespace, Number, Plus, Star, Power, Minus })]
enum OperatorTokens {
    #[regex(r"\s+")]
    #[skip]
    Whitespace,
    #[regex(r"[0-9]+")]
    Number(usize),
    #[regex(r"\+")]
    Plus,
    #[regex(r"\*")]
    Star,
    #[regex(r"\^")]
    Power,
    #[regex(r"-")]
    Minus,
    #[error]
    Error(LexErrorInfo),
}

impl fmt::Display for OperatorTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

/// Binding powers live on occurrences of the same nonterminal.  The
/// compact left-recursive spelling is used for `Add` and `Multiply`; the
/// power and prefix rules use the explicit output form.
#[allow(dead_code)]
#[derive(NonTerminal, Debug, Clone)]
enum TierExpr {
    #[rule(TierExpr:10, OperatorTokens::Plus, TierExpr:20)]
    Add(
        #[from(0)] crate::component::parse::data::AstBox<TierExpr>,
        #[from(1)] AstToken<OperatorTokens>,
        #[from(2)] crate::component::parse::data::AstBox<TierExpr>,
    ),
    #[rule(TierExpr:20, OperatorTokens::Star, TierExpr:30)]
    Multiply(
        #[from(0)] crate::component::parse::data::AstBox<TierExpr>,
        #[from(1)] AstToken<OperatorTokens>,
        #[from(2)] crate::component::parse::data::AstBox<TierExpr>,
    ),
    #[rule(TierExpr:30 <- TierExpr:31, OperatorTokens::Power, TierExpr:30)]
    Power(
        #[from(0)] crate::component::parse::data::AstBox<TierExpr>,
        #[from(1)] AstToken<OperatorTokens>,
        #[from(2)] crate::component::parse::data::AstBox<TierExpr>,
    ),
    #[rule(TierExpr:40 <- OperatorTokens::Minus, TierExpr:40)]
    Negate(
        #[from(0)] AstToken<OperatorTokens>,
        #[from(1)] crate::component::parse::data::AstBox<TierExpr>,
    ),
    #[rule(TierExpr:50 <- OperatorTokens::Number)]
    Number(#[from(0)] AstToken<OperatorTokens>),
}

#[test]
fn parser_node_publishes_every_accepted_interpretation() {
    let uri = Span::new("test://node-parser-ambiguity", 0, 0).unwrap().uri;
    let parser = Grammar::from_spec::<AmbiguousValue>().build_lr1::<Tokens>();
    assert!(
        !parser.conflicts.is_empty(),
        "fixture must retain a reduce/reduce conflict"
    );

    let mut graph = Graph::new();
    graph.install(LexerNode::<Tokens>::new().unwrap()).unwrap();
    graph
        .install(ParserNode::<Tokens, AmbiguousValue>::from_parser(parser))
        .unwrap();
    graph.command(SourceInput::load(uri)).unwrap();
    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: "42".into(),
        }))
        .unwrap();

    let _demand = graph
        .demand::<ParserNode<Tokens, AmbiguousValue>>(uri)
        .unwrap();
    let snapshot = graph.get::<ParseSnapshot<Tokens>>(uri).unwrap();
    let candidates = graph
        .get::<ParseCandidates<Tokens, AmbiguousValue>>(uri)
        .unwrap();
    let entries = graph.scan::<crate::component::parse::ParseEntries<Tokens>>(uri);

    assert_eq!(candidates.len(), 2);
    let mut entry_products = entries
        .iter()
        .map(|entry| entry.metadata)
        .collect::<Vec<_>>();
    let mut candidate_products = candidates
        .iter()
        .map(|candidate| candidate.product)
        .collect::<Vec<_>>();
    entry_products.sort_unstable();
    candidate_products.sort_unstable();
    assert_eq!(entry_products, candidate_products);
    assert_ne!(candidates[0].product, candidates[1].product);
    assert!(
        candidates
            .iter()
            .any(|candidate| matches!(candidate.value.as_ref(), AmbiguousValue::Decimal(_)))
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| matches!(candidate.value.as_ref(), AmbiguousValue::Natural(_)))
    );
    for candidate in candidates.iter() {
        assert_eq!(
            candidate.ast_box.resolve(&snapshot).unwrap().product(),
            candidate.product
        );
    }
}

#[test]
fn tiered_nonterminal_rules_encode_precedence_and_associativity_structurally() {
    let uri = Span::new("test://node-tiered-nonterminal", 0, 0)
        .unwrap()
        .uri;
    let parser = Grammar::from_spec::<TierExpr>().build_lr1::<OperatorTokens>();
    assert!(
        parser.conflicts.is_empty(),
        "tier lowering should make operator grouping unambiguous before parsing"
    );

    let mut graph = Graph::new();
    graph
        .install(LexerNode::<OperatorTokens>::new().unwrap())
        .unwrap();
    graph
        .install(ParserNode::<OperatorTokens, TierExpr>::from_parser(parser))
        .unwrap();
    graph.command(SourceInput::load(uri)).unwrap();
    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: "-1+2*3^4^5".into(),
        }))
        .unwrap();

    let _demand = graph
        .demand::<ParserNode<OperatorTokens, TierExpr>>(uri)
        .unwrap();
    let snapshot = graph.get::<ParseSnapshot<OperatorTokens>>(uri).unwrap();
    let candidates = graph
        .get::<ParseCandidates<OperatorTokens, TierExpr>>(uri)
        .unwrap();
    assert_eq!(
        candidates.len(),
        1,
        "the grammar, not a later chooser, grouped it"
    );

    let root = candidates[0].ast_box.resolve(&snapshot).unwrap();
    let TierExpr::Add(left, _, right) = &*root else {
        panic!("expected addition at the loosest declared binding power")
    };
    let negate = left.resolve(&snapshot).unwrap();
    let TierExpr::Negate(_, operand) = &*negate else {
        panic!("the explicit prefix rule must build a unary expression")
    };
    assert!(matches!(
        &*operand.resolve(&snapshot).unwrap(),
        TierExpr::Number(_)
    ));
    let multiply = right.resolve(&snapshot).unwrap();
    let TierExpr::Multiply(left, _, right) = &*multiply else {
        panic!("multiplication must bind more tightly than addition")
    };
    assert!(matches!(
        &*left.resolve(&snapshot).unwrap(),
        TierExpr::Number(_)
    ));
    let outer_power = right.resolve(&snapshot).unwrap();
    let TierExpr::Power(left, _, right) = &*outer_power else {
        panic!("expected exponentiation under multiplication")
    };
    assert!(matches!(
        &*left.resolve(&snapshot).unwrap(),
        TierExpr::Number(_)
    ));
    let inner_power = right.resolve(&snapshot).unwrap();
    assert!(
        matches!(&*inner_power, TierExpr::Power(_, _, _)),
        "power is right-associative"
    );
}

#[test]
fn parser_revision_resolves_ast_boxes_and_tracks_span_only_updates() {
    let uri = Span::new("test://node-parser", 0, 0).unwrap().uri;
    let parser = Grammar::from_spec::<Value>().build_lr1::<Tokens>();
    let mut graph = Graph::new();
    graph.install(LexerNode::<Tokens>::new().unwrap()).unwrap();
    graph
        .install(ParserNode::<Tokens, Value>::from_parser(parser))
        .unwrap();
    graph.command(SourceInput::load(uri)).unwrap();
    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: "42".into(),
        }))
        .unwrap();

    let _demand = graph.demand::<ParserNode<Tokens, Value>>(uri).unwrap();
    let snapshot = graph.get::<ParseSnapshot<Tokens>>(uri).unwrap();
    let entries = graph.scan::<ParseEntries<Tokens>>(uri);
    let root = entries[0].node.clone();
    let artifact = graph
        .get::<ParsedAst<Tokens>>(root.clone())
        .expect("the root AST artifact must be materialized");
    let semantic = graph.subscribe::<ParsedAst<Tokens>>(root.clone()).unwrap();
    let location = graph
        .subscribe::<AstLocation<Tokens>>(root.clone())
        .unwrap();
    let _ = semantic.recv().unwrap();
    let _ = location.recv().unwrap();

    let ast_box = AstBox::<Value>::new(root.id, root.uri);
    assert!(matches!(
        &*artifact.deref::<Value>().unwrap(),
        Value::Number(_)
    ));
    let resolved = ast_box.resolve(&snapshot).unwrap();
    assert_eq!(resolved.span(), Span::new_uri(uri, 0, 2).unwrap());
    assert_eq!(
        resolved.span().to_line_col(snapshot.source()),
        RangeOrPoint::Range((0, 0), (0, 2))
    );

    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: "\n".into(),
        }))
        .unwrap();
    let shifted = graph.get::<ParseSnapshot<Tokens>>(uri).unwrap();
    assert_eq!(
        ast_box
            .span(&shifted)
            .unwrap()
            .to_line_col(shifted.source()),
        RangeOrPoint::Range((1, 0), (1, 2)),
    );
    assert!(
        semantic.try_recv().is_err(),
        "span-only edits do not invalidate semantic AST facts"
    );
    assert!(matches!(
        location.recv().unwrap(),
        ViewUpdate::Changed { .. }
    ));

    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 1).unwrap(),
            value: "\u{2003}".into(),
        }))
        .unwrap();
    let unicode_shifted = graph.get::<ParseSnapshot<Tokens>>(uri).unwrap();
    assert_eq!(
        ast_box.span(&unicode_shifted).unwrap(),
        Span::new_uri(uri, 4, 6).unwrap(),
    );
    assert_eq!(
        ast_box
            .span(&unicode_shifted)
            .unwrap()
            .to_line_col(unicode_shifted.source()),
        RangeOrPoint::Range((1, 1), (1, 3)),
        "Rope conversion reports character columns rather than UTF-8 bytes"
    );
    assert!(semantic.try_recv().is_err());
    assert!(matches!(
        location.recv().unwrap(),
        ViewUpdate::Changed { .. }
    ));
    assert_eq!(
        ast_box.span(&snapshot).unwrap(),
        Span::new_uri(uri, 0, 2).unwrap(),
        "held historical snapshots retain their original source coordinates"
    );

    graph
        .command(SourceInput::apply_all(vec![
            SourceEdit::Delete {
                key: Span::new_uri(uri, 4, 6).unwrap(),
            },
            SourceEdit::Insert {
                key: Span::point_uri(uri, 4).unwrap(),
                value: "7".into(),
            },
        ]))
        .unwrap();
    let replaced = graph.get::<ParseSnapshot<Tokens>>(uri).unwrap();
    assert!(matches!(
        semantic.recv().unwrap(),
        ViewUpdate::Removed { .. }
    ));
    assert!(matches!(
        location.recv().unwrap(),
        ViewUpdate::Removed { .. }
    ));
    assert!(matches!(
        ast_box.resolve(&replaced),
        Err(crate::component::parse::AstLookupError::Deleted { .. })
    ));
}
