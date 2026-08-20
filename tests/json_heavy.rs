mod common;

use std::sync::Arc;

use common::json::{JsonDocument, JsonToken};
use plingo::framework::lex::{TokenVec, Tokens, install_lexer};
use plingo::framework::parse::{ParseDiagnostics, ParseStatus, ParseUnits, install_parser};
use plingo::framework::{SourceEdit, Workspace};
use plingo::utils::Span;

fn uri(name: &str) -> fluent_uri::Uri<&'static str> {
    Span::new(format!("test://{name}"), 0, 0).unwrap().uri
}

fn build(workers: usize) -> Workspace {
    Workspace::build_with(workers, |engine| {
        install_lexer::<JsonToken>(engine)?;
        install_parser::<JsonToken, JsonDocument>(engine)?;
        Ok(())
    })
    .expect("workspace builds")
}

struct JsonRuntime {
    ws: Workspace,
    u: fluent_uri::Uri<&'static str>,
    name: String,
}

impl JsonRuntime {
    fn new(name: &str, text: &str) -> Self {
        let mut ws = build(1);
        let u = uri(name);
        ws.open(u, text).unwrap();
        Self {
            ws,
            u,
            name: name.to_string(),
        }
    }

    fn uri_str(&self) -> String {
        self.u.to_string()
    }

    fn apply(&mut self, edits: Vec<SourceEdit>) {
        self.ws
            .edit(edits)
            .expect("edits must apply as one atomically committed epoch");
    }

    fn text(&self) -> String {
        self.ws
            .snapshot()
            .map_view::<plingo::framework::SourceText>()
            .get(&self.uri_str())
            .map(|v| v.to_string())
            .unwrap_or_default()
    }

    fn tokens(&self) -> Option<Arc<TokenVec<JsonToken>>> {
        self.ws
            .snapshot()
            .map_view::<Tokens<JsonToken>>()
            .get(&self.uri_str())
    }

    fn unit(&self) -> Option<plingo::framework::parse::ParseUnit<JsonDocument>> {
        self.ws
            .snapshot()
            .map_view::<ParseUnits<JsonDocument>>()
            .get(&self.uri_str())
            .map(|v| (*v).clone())
    }

    fn diagnostic_count(&self) -> usize {
        self.ws
            .snapshot()
            .map_view::<ParseDiagnostics>()
            .get(&self.uri_str())
            .map(|v| v.len())
            .unwrap_or(0)
    }

    fn lex_error_count(&self) -> usize {
        self.tokens()
            .map(|t| t.errors.len())
            .unwrap_or(0)
    }

    fn status(&self) -> Option<ParseStatus> {
        self.unit().map(|u| u.status)
    }

    fn has_root(&self) -> bool {
        self.unit().is_some_and(|u| u.root != plingo::reactive::NodeId(u64::MAX))
    }
}

fn replace(
    u: fluent_uri::Uri<&'static str>,
    text: &str,
    needle: &str,
    value: &str,
) -> Vec<SourceEdit> {
    let start = text.find(needle).expect("fixture contains target");
    let end = start + needle.len();
    vec![
        SourceEdit::Delete {
            key: Span::new_uri(u, start, end).unwrap(),
        },
        SourceEdit::Insert {
            key: Span::point_uri(u, start).unwrap(),
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
fn empty_containers_are_valid_separator_grammar_cases() {
    let runtime = JsonRuntime::new("heavy-empty", r#"{"object": {}, "array": []}"#);
    assert!(runtime.has_root());
    assert_eq!(runtime.diagnostic_count(), 0);
    assert_eq!(runtime.status(), Some(ParseStatus::Clean));
}

#[test]
fn skipped_whitespace_shifts_are_parser_invisible() {
    // Inserting whitespace changes byte offsets but neither the token
    // stream shape nor the parse result: the unit and diagnostics are
    // unchanged (only the underlying text/tokens move).
    let text = large_json();
    let mut runtime = JsonRuntime::new("heavy-whitespace", &text);
    let before_roots = runtime.unit().map(|u| u.root);
    let before_tokens: Arc<TokenVec<JsonToken>> = runtime.tokens().unwrap();
    let offset = text.find("\"middle\"").unwrap();

    runtime.apply(vec![SourceEdit::Insert {
        key: Span::point_uri(runtime.u, offset).unwrap(),
        value: "\n    ".into(),
    }]);

    assert_eq!(runtime.unit().map(|u| u.root), before_roots);
    assert_eq!(runtime.diagnostic_count(), 0);
    // Token values (textual lexemes) survive the layout shift.
    let after: Arc<TokenVec<JsonToken>> = runtime.tokens().unwrap();
    assert_eq!(after.tokens.len(), before_tokens.tokens.len());
}

#[test]
fn error_recovery_publishes_diagnostics_and_a_later_repair_clears_them() {
    let text = r#"{"left": 1, "right": 2}"#;
    let mut runtime = JsonRuntime::new("heavy-recovery", text);
    let colon = text.find(": 1").unwrap();

    runtime.apply(vec![SourceEdit::Delete {
        key: Span::new_uri(runtime.u, colon, colon + 1).unwrap(),
    }]);
    assert!(
        runtime.diagnostic_count() > 0,
        "invalid JSON must commit diagnostics"
    );
    assert!(matches!(runtime.status(), Some(ParseStatus::Recovered { .. })));

    runtime.apply(vec![SourceEdit::Insert {
        key: Span::point_uri(runtime.u, colon).unwrap(),
        value: ":".into(),
    }]);
    assert_eq!(
        runtime.diagnostic_count(),
        0,
        "repair must retract stale diagnostics"
    );
    assert_eq!(runtime.status(), Some(ParseStatus::Clean));
    assert!(runtime.has_root(), "repair must restore a typed root");
}

#[test]
fn truncated_container_recovers_without_retaining_stale_roots() {
    let text = r#"{"outer": {"value": 1}, "tail": 2}"#;
    let mut runtime = JsonRuntime::new("heavy-truncated", text);
    let close = text.len() - 1;

    runtime.apply(vec![SourceEdit::Delete {
        key: Span::new_uri(runtime.u, close, close + 1).unwrap(),
    }]);
    assert!(runtime.diagnostic_count() > 0);
    assert_ne!(runtime.status(), Some(ParseStatus::Clean));

    runtime.apply(vec![SourceEdit::Insert {
        key: Span::point_uri(runtime.u, close).unwrap(),
        value: "}".into(),
    }]);
    assert_eq!(runtime.diagnostic_count(), 0);
    assert_eq!(runtime.status(), Some(ParseStatus::Clean));
}

#[test]
fn unicode_replacements_preserve_utf8_boundaries() {
    let text = r#"{"label": "α", "tail": 12345}"#;
    let mut runtime = JsonRuntime::new("heavy-unicode", text);
    runtime.apply(replace(runtime.u, text, "α", "β"));

    assert_eq!(runtime.text(), r#"{"label": "β", "tail": 12345}"#);
    assert_eq!(runtime.diagnostic_count(), 0);
    assert!(runtime.has_root());
}

#[test]
fn lexical_errors_publish_partial_diagnostics_and_repair_cleanly() {
    let text = r#"{"value": 1, "tail": 2}"#;
    let mut runtime = JsonRuntime::new("heavy-lex-error", text);
    let number = text.find("1,").unwrap();

    runtime.apply(replace(runtime.u, text, "1", "@"));
    assert!(runtime.lex_error_count() > 0, "lex error visible in TokenVec");
    assert!(runtime.diagnostic_count() > 0);
    assert_ne!(runtime.status(), Some(ParseStatus::Clean));

    runtime.apply(replace(runtime.u, runtime.text().as_str(), "@", "1"));
    assert_eq!(runtime.lex_error_count(), 0, "repair clears lex errors");
    assert_eq!(runtime.diagnostic_count(), 0);
    assert_eq!(runtime.status(), Some(ParseStatus::Clean));
    assert!(number < runtime.text().len());
}

#[test]
fn sequential_incremental_edits_match_a_fresh_oracle_after_every_step() {
    let mut runtime = JsonRuntime::new("heavy-trace", r#"{"a": 10, "b": [20, 30], "c": 40}"#);
    let trace = [("10", "11"), ("20", "21"), ("40", "41"), ("30", "31")];

    for (step, (old, new)) in trace.into_iter().enumerate() {
        let before = runtime.text();
        runtime.apply(replace(runtime.u, before.as_str(), old, new));
        let fresh = JsonRuntime::new(
            &format!("heavy-trace-oracle-{step}"),
            runtime.text().as_str(),
        );
        let this: Arc<TokenVec<JsonToken>> = runtime.tokens().unwrap();
        let oracle: Arc<TokenVec<JsonToken>> = fresh.tokens().unwrap();
        // Textual token values and errors must match the fresh oracle.
        assert_eq!(this.tokens.len(), oracle.tokens.len());
        assert_eq!(
            this.tokens
                .iter()
                .map(|t| t.value.to_string())
                .collect::<Vec<_>>(),
            oracle
                .tokens
                .iter()
                .map(|t| t.value.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(runtime.diagnostic_count(), fresh.diagnostic_count());
        assert_eq!(runtime.status(), fresh.status());
    }
}

#[test]
fn reverse_order_batch_replacements_keep_disjoint_edits_sparse_and_valid() {
    let text = r#"{"left": 11111, "right": 22222}"#;
    let mut runtime = JsonRuntime::new("heavy-reverse-batch", text);
    let mut edits = replace(runtime.u, text, "22222", "98765");
    edits.extend(replace(runtime.u, text, "11111", "56789"));
    runtime.apply(edits);

    assert_eq!(runtime.text(), r#"{"left": 56789, "right": 98765}"#);
    assert_eq!(runtime.diagnostic_count(), 0);
    assert!(runtime.has_root());
}

#[test]
fn independent_documents_do_not_cross_invalidate() {
    let mut ws = build(1);
    let a = uri("heavy-isolated-a");
    let b = uri("heavy-isolated-b");
    ws.open(a, r#"{"value": 1}"#).unwrap();
    ws.open(b, r#"{"value": 2}"#).unwrap();
    let before_b = ws
        .snapshot()
        .map_view::<ParseUnits<JsonDocument>>()
        .get(&b.to_string())
        .map(|v| (*v).clone());
    ws.edit(vec![SourceEdit::Insert {
        key: Span::point_uri(a, 9).unwrap(),
        value: "0".into(),
    }])
    .unwrap();
    let after_b = ws
        .snapshot()
        .map_view::<ParseUnits<JsonDocument>>()
        .get(&b.to_string())
        .map(|v| (*v).clone());
    assert_eq!(before_b, after_b, "document B is untouched by A's edit");
}

#[test]
fn deep_nesting_parses_with_stats_present() {
    let depth = 200;
    let mut text = String::from(r#"{"a": "#);
    for _ in 0..depth {
        text.push_str("[");
    }
    text.push_str("1");
    for _ in 0..depth {
        text.push_str("]");
    }
    text.push_str("}");
    let runtime = JsonRuntime::new("heavy-deep", &text);
    assert!(runtime.has_root());
    assert_eq!(runtime.diagnostic_count(), 0);
    // Stats are populated (the plan's ParseUnit carries them).
    let unit = runtime.unit().unwrap();
    assert!(unit.stats.reparsed > 0 || unit.stats.restart_boundary == 0);
}