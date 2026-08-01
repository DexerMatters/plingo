use std::fmt;

use plingo_macros::Terminal;

use super::*;
use crate::{
    component::{
        lex::LexErrorInfo,
        source::{SourceEdit, SourceInput},
    },
    scheme::node::{Graph, ReadGraph, ViewUpdate},
    utils::Span,
};

#[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
#[scopes(root { Word })]
enum TestTokens {
    #[regex(r"[a-z]+")]
    Word(String),
    #[error]
    Error(LexErrorInfo),
}

impl fmt::Display for TestTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[test]
fn lexer_node_observes_source_without_a_lower_layer() {
    let uri = Span::new("test://node-lexer", 0, 0).unwrap().uri;
    let mut graph = Graph::new();
    graph
        .install(LexerNode::<TestTokens>::new().unwrap())
        .unwrap();
    graph.command(SourceInput::load(uri)).unwrap();

    let _demand = graph.demand::<LexerNode<TestTokens>>(uri).unwrap();
    let subscription = graph.subscribe::<TokenOrder<TestTokens>>(uri).unwrap();
    assert!(matches!(
        subscription.recv().unwrap(),
        ViewUpdate::Initial { .. }
    ));

    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri, 0).unwrap(),
            value: "hello".into(),
        }))
        .unwrap();
    let ViewUpdate::Changed { value, .. } = subscription.recv().unwrap() else {
        panic!("source edit must publish a committed token update");
    };
    assert_eq!(value.len(), 2, "one word plus synthetic EOF");
    assert_eq!(
        graph
            .get::<TokenArtifact<TestTokens>>(value[0].clone())
            .expect("the ordered token must be materialized")
            .length,
        5
    );
}
