mod common;

use std::{sync::Arc, sync::mpsc::TryRecvError};

use common::json::{JsonDocument, JsonToken};
use plingo::{
    DemandLease, Graph, ReadGraph, Subscription,
    component::{
        lex::{LexDiagnostics, LexStats, LexerNode, TokenArtifact, TokenKey, TokenOrder},
        parse::{
            ParseDiagnostics, ParseRoots, ParseSnapshot, ParseStats, ParseStatus, ParseStatusView,
            ParserNode, grammar::Grammar,
        },
        source::{DocumentText, SourceEdit, SourceInput},
    },
    utils::Span,
};

struct JsonRuntime {
    graph: Graph,
    uri: fluent_uri::Uri<&'static str>,
    _demand: DemandLease,
    subscription: Subscription<ParseSnapshot<JsonToken>>,
}

impl JsonRuntime {
    fn new(name: &str, text: &str) -> Self {
        let uri = Span::new(format!("test://{name}"), 0, 0).unwrap().uri;
        let parser = Grammar::from_spec::<JsonDocument>().build_lr1::<JsonToken>();
        let mut graph = Graph::new();
        graph
            .install(LexerNode::<JsonToken>::new().unwrap())
            .unwrap();
        graph
            .install(ParserNode::<JsonToken, JsonDocument>::from_parser(parser))
            .unwrap();
        graph.command(SourceInput::load(uri)).unwrap();
        graph
            .command(SourceInput::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: text.into(),
            }))
            .unwrap();
        let demand = graph
            .demand::<ParserNode<JsonToken, JsonDocument>>(uri)
            .unwrap();
        let subscription = graph.subscribe::<ParseSnapshot<JsonToken>>(uri).unwrap();
        let _ = subscription.recv().unwrap();
        Self {
            graph,
            uri,
            _demand: demand,
            subscription,
        }
    }

    fn apply(&mut self, edits: Vec<SourceEdit>) {
        self.graph.command(SourceInput::apply_all(edits)).unwrap();
        while self.subscription.try_recv().is_ok() {}
    }

    fn text(&self) -> Arc<str> {
        self.graph.get::<DocumentText>(self.uri).unwrap()
    }

    fn roots(&self) -> Arc<[plingo::component::parse::AstKey]> {
        self.graph
            .get::<ParseRoots<JsonToken, JsonDocument>>(self.uri)
            .unwrap()
    }

    fn token_keys(&self) -> Arc<[TokenKey]> {
        self.graph.get::<TokenOrder<JsonToken>>(self.uri).unwrap()
    }

    fn token_shape(
        &self,
    ) -> Vec<(
        Option<plingo::component::parse::grammar::TerminalId>,
        usize,
        u64,
    )> {
        self.token_keys()
            .iter()
            .map(|key| {
                let token = self
                    .graph
                    .get::<TokenArtifact<JsonToken>>(key.clone())
                    .unwrap();
                (token.terminal, token.length, token.fingerprint)
            })
            .collect()
    }

    fn lex_stats(&self) -> plingo::component::lex::IncrementalLexStats {
        self.graph.get::<LexStats<JsonToken>>(self.uri).unwrap()
    }

    fn parse_stats(&self) -> plingo::component::parse::IncrementalParseStats {
        self.graph.get::<ParseStats<JsonToken>>(self.uri).unwrap()
    }

    fn diagnostic_count(&self) -> usize {
        self.graph
            .get::<ParseDiagnostics<JsonToken>>(self.uri)
            .unwrap()
            .len()
    }

    fn status(&self) -> ParseStatus {
        self.graph
            .get::<ParseStatusView<JsonToken>>(self.uri)
            .unwrap()
    }

    fn lex_diagnostic_count(&self) -> usize {
        self.graph
            .get::<LexDiagnostics<JsonToken>>(self.uri)
            .unwrap()
            .len()
    }
}

fn replace(
    uri: fluent_uri::Uri<&'static str>,
    text: &str,
    needle: &str,
    value: &str,
) -> Vec<SourceEdit> {
    let start = text.find(needle).expect("fixture contains target");
    let end = start + needle.len();
    vec![
        SourceEdit::Delete {
            key: Span::new_uri(uri, start, end).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(uri, start).unwrap(),
            value: value.into(),
        },
    ]
}

fn large_json() -> String {
    let values = (0..512).map(|value| value.to_string()).collect::<Vec<_>>();
    format!(
        r#"{{"head":12345,"items":[{}],"middle":23456,"tail":34567}}"#,
        values.join(",")
    )
}

#[test]
fn distant_replacements_replay_locally_and_reuse_the_suffix() {
    let text = large_json();
    let mut runtime = JsonRuntime::new("heavy-distant", &text);
    let initial_tokens = runtime.token_keys();
    let initial_roots = runtime.roots();
    let retained_middle = initial_tokens[20..30].to_vec();

    let mut edits = replace(runtime.uri, &text, "12345", "54321");
    edits.extend(replace(runtime.uri, &text, "34567", "76543"));
    runtime.apply(edits);

    let lex = runtime.lex_stats();
    let parse = runtime.parse_stats();
    assert!(
        lex.reused > 0,
        "lexer must converge and retain a suffix: {lex:?}"
    );
    assert!(
        parse.reused > 0,
        "parser must converge and retain a suffix: {parse:?}"
    );
    assert!(
        lex.relexed < initial_tokens.len(),
        "edit must not relex the document"
    );
    assert!(
        parse.reparsed < initial_tokens.len(),
        "edit must not reparse the document"
    );
    assert_ne!(
        runtime.roots(),
        initial_roots,
        "changed JSON roots must be replaced"
    );
    assert_eq!(runtime.diagnostic_count(), 0);
    let current_keys = runtime.token_keys();
    assert!(retained_middle.iter().all(|key| current_keys.contains(key)));

    let fresh = JsonRuntime::new("heavy-distant-fresh", runtime.text().as_ref());
    assert_eq!(runtime.token_shape(), fresh.token_shape());
    assert_eq!(runtime.diagnostic_count(), fresh.diagnostic_count());
}

#[test]
fn empty_containers_are_valid_separator_grammar_cases() {
    let runtime = JsonRuntime::new("heavy-empty", r#"{"object": {}, "array": []}"#);
    assert!(!runtime.roots().is_empty());
    assert_eq!(runtime.diagnostic_count(), 0);
    assert_eq!(runtime.status(), ParseStatus::Clean);
}

#[test]
fn skipped_whitespace_shifts_spans_without_reparsing_json() {
    let text = large_json();
    let mut runtime = JsonRuntime::new("heavy-whitespace", &text);
    let roots = runtime.roots();
    let tokens = runtime.token_keys();
    let offset = text.find("\"middle\"").unwrap();

    runtime.apply(vec![SourceEdit::Insert {
        key: Span::point_uri(runtime.uri, offset).unwrap(),
        value: "\n    ".into(),
    }]);

    assert_eq!(runtime.roots(), roots, "layout is parser-invisible");
    assert_eq!(
        runtime.token_keys(),
        tokens,
        "occurrence identities survive layout shifts"
    );
    assert_eq!(runtime.diagnostic_count(), 0);
}

#[test]
fn error_recovery_publishes_diagnostics_and_a_later_repair_clears_them() {
    let text = r#"{"left": 1, "right": 2}"#;
    let mut runtime = JsonRuntime::new("heavy-recovery", text);
    let colon = text.find(": 1").unwrap();

    runtime.apply(vec![SourceEdit::Delete {
        key: Span::new_uri(runtime.uri, colon, colon + 1).unwrap(),
    }]);
    assert!(
        runtime.diagnostic_count() > 0,
        "invalid JSON must commit diagnostics"
    );
    assert!(matches!(runtime.status(), ParseStatus::Recovered { .. }));

    runtime.apply(vec![SourceEdit::Insert {
        key: Span::point_uri(runtime.uri, colon).unwrap(),
        value: ":".into(),
    }]);
    assert_eq!(
        runtime.diagnostic_count(),
        0,
        "repair must retract stale diagnostics"
    );
    assert_eq!(runtime.status(), ParseStatus::Clean);
    assert!(
        !runtime.roots().is_empty(),
        "repair must restore a typed root"
    );
}

#[test]
fn truncated_container_recovers_without_retaining_stale_roots() {
    let text = r#"{"outer": {"value": 1}, "tail": 2}"#;
    let mut runtime = JsonRuntime::new("heavy-truncated", text);
    let close = text.len() - 1;

    runtime.apply(vec![SourceEdit::Delete {
        key: Span::new_uri(runtime.uri, close, close + 1).unwrap(),
    }]);
    assert!(runtime.diagnostic_count() > 0);
    assert_ne!(runtime.status(), ParseStatus::Clean);

    runtime.apply(vec![SourceEdit::Insert {
        key: Span::point_uri(runtime.uri, close).unwrap(),
        value: "}".into(),
    }]);
    assert_eq!(runtime.diagnostic_count(), 0);
    assert_eq!(runtime.status(), ParseStatus::Clean);
}

#[test]
fn unicode_replacements_preserve_utf8_boundaries_and_incremental_suffixes() {
    let text = r#"{"label": "α", "tail": 12345}"#;
    let mut runtime = JsonRuntime::new("heavy-unicode", text);
    runtime.apply(replace(runtime.uri, text, "α", "β"));

    assert_eq!(runtime.text().as_ref(), r#"{"label": "β", "tail": 12345}"#);
    assert_eq!(runtime.diagnostic_count(), 0);
    assert!(runtime.lex_stats().reused > 0);
    assert!(runtime.parse_stats().reused > 0);
}

#[test]
fn lexical_errors_publish_partial_diagnostics_and_repair_cleanly() {
    let text = r#"{"value": 1, "tail": 2}"#;
    let mut runtime = JsonRuntime::new("heavy-lex-error", text);
    let number = text.find("1,").unwrap();

    runtime.apply(replace(runtime.uri, text, "1", "@"));
    assert!(runtime.lex_diagnostic_count() > 0);
    assert!(runtime.diagnostic_count() > 0);
    assert_ne!(runtime.status(), ParseStatus::Clean);

    runtime.apply(replace(runtime.uri, runtime.text().as_ref(), "@", "1"));
    assert_eq!(runtime.lex_diagnostic_count(), 0);
    assert_eq!(runtime.diagnostic_count(), 0);
    assert_eq!(runtime.status(), ParseStatus::Clean);
    assert!(number < runtime.text().len());
}

#[test]
fn sequential_incremental_edits_match_a_fresh_oracle_after_every_step() {
    let mut runtime = JsonRuntime::new("heavy-trace", r#"{"a": 10, "b": [20, 30], "c": 40}"#);
    let trace = [("10", "11"), ("20", "21"), ("40", "41"), ("30", "31")];

    for (step, (old, new)) in trace.into_iter().enumerate() {
        let before = runtime.text();
        runtime.apply(replace(runtime.uri, before.as_ref(), old, new));
        let fresh = JsonRuntime::new(
            &format!("heavy-trace-oracle-{step}"),
            runtime.text().as_ref(),
        );
        assert_eq!(runtime.token_shape(), fresh.token_shape());
        assert_eq!(runtime.diagnostic_count(), fresh.diagnostic_count());
        assert_eq!(runtime.status(), fresh.status());
    }
}

#[test]
fn reverse_order_batch_replacements_keep_disjoint_edits_sparse_and_valid() {
    let text = r#"{"left": 11111, "right": 22222}"#;
    let mut runtime = JsonRuntime::new("heavy-reverse-batch", text);
    let mut edits = replace(runtime.uri, text, "22222", "98765");
    edits.extend(replace(runtime.uri, text, "11111", "56789"));
    runtime.apply(edits);

    assert_eq!(
        runtime.text().as_ref(),
        r#"{"left": 56789, "right": 98765}"#
    );
    assert_eq!(runtime.diagnostic_count(), 0);
    assert!(runtime.lex_stats().reused > 0);
    assert!(runtime.parse_stats().reused > 0);
}

#[test]
fn released_document_caches_reinitialize_without_replaying_stale_deltas() {
    let runtime = JsonRuntime::new("heavy-reclaim", r#"{"value": 1}"#);
    let JsonRuntime {
        mut graph,
        uri,
        _demand,
        subscription,
    } = runtime;
    drop(subscription);
    drop(_demand);
    graph.collect_garbage().unwrap();
    assert!(
        graph
            .get::<ParseRoots<JsonToken, JsonDocument>>(uri)
            .is_none()
    );

    let _rematerialized = graph
        .demand::<ParserNode<JsonToken, JsonDocument>>(uri)
        .unwrap();
    assert!(
        graph
            .get::<ParseSnapshot<JsonToken>>(uri)
            .unwrap()
            .ast_keys()
            .next()
            .is_some()
    );
    assert_eq!(
        graph.get::<ParseDiagnostics<JsonToken>>(uri).unwrap().len(),
        0
    );
}

#[test]
fn independent_documents_do_not_cross_invalidate() {
    let uri_a = Span::new("test://heavy-isolated-a", 0, 0).unwrap().uri;
    let uri_b = Span::new("test://heavy-isolated-b", 0, 0).unwrap().uri;
    let parser = Grammar::from_spec::<JsonDocument>().build_lr1::<JsonToken>();
    let mut graph = Graph::new();
    graph
        .install(LexerNode::<JsonToken>::new().unwrap())
        .unwrap();
    graph
        .install(ParserNode::<JsonToken, JsonDocument>::from_parser(parser))
        .unwrap();
    for (uri, text) in [(uri_a, r#"{"value": 1}"#), (uri_b, r#"{"value": 2}"#)] {
        graph.command(SourceInput::load(uri)).unwrap();
        graph
            .command(SourceInput::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: text.into(),
            }))
            .unwrap();
    }
    let _demand_a = graph
        .demand::<ParserNode<JsonToken, JsonDocument>>(uri_a)
        .unwrap();
    let _demand_b = graph
        .demand::<ParserNode<JsonToken, JsonDocument>>(uri_b)
        .unwrap();
    let subscription_a = graph.subscribe::<ParseSnapshot<JsonToken>>(uri_a).unwrap();
    let subscription_b = graph.subscribe::<ParseSnapshot<JsonToken>>(uri_b).unwrap();
    let _ = subscription_a.recv().unwrap();
    let _ = subscription_b.recv().unwrap();
    let roots_b = graph
        .get::<ParseRoots<JsonToken, JsonDocument>>(uri_b)
        .unwrap();

    graph
        .command(SourceInput::apply(SourceEdit::Insert {
            key: Span::point_uri(uri_a, 1).unwrap(),
            value: " ".into(),
        }))
        .unwrap();
    assert!(
        matches!(subscription_b.try_recv(), Err(TryRecvError::Empty)),
        "an edit to URI A must not publish a URI B parser update"
    );
    assert_eq!(
        graph
            .get::<ParseRoots<JsonToken, JsonDocument>>(uri_b)
            .unwrap(),
        roots_b
    );
}

#[test]
fn atomic_batches_reject_mixed_documents_without_publishing_a_partial_revision() {
    let text = r#"{"value": 1}"#;
    let mut runtime = JsonRuntime::new("heavy-atomic", text);
    let before = runtime.text();
    let foreign = Span::new("test://foreign", 0, 0).unwrap().uri;

    let error = runtime
        .graph
        .command(SourceInput::apply_all(vec![
            SourceEdit::Insert {
                key: Span::point_uri(runtime.uri, 1).unwrap(),
                value: " ".into(),
            },
            SourceEdit::Insert {
                key: Span::point_uri(foreign, 0).unwrap(),
                value: "x".into(),
            },
        ]))
        .expect_err("a batch cannot span documents");
    assert!(error.to_string().contains("one document"));
    assert_eq!(runtime.text(), before);
    assert_eq!(runtime.diagnostic_count(), 0);
}
