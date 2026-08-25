//! Canonical oracle projection (plan §10.4).
//!
//! Fresh and incremental runs cannot share internal IDs, so every gate
//! compares this projection instead: ordered tokens as
//! `(terminal, semantic value, resolved span, error kind)`, parse status and
//! ordered roots as typed structural values, and diagnostics as
//! `(kind, expected, recovered, location)`. No digest is an oracle:
//! comparisons are exact structural equality.

use std::collections::HashMap;
use std::sync::Arc;

use plingo::framework::lex::{LexErrorInfo, LexToken, Tokens, TokenVec};
use plingo::framework::parse::data::AstBox;
use plingo::framework::parse::{
    AstSnapshot, AstSnapshots, DocumentSnapshot, ParseDiagnostics, ParseErrorInfo, ParseStatus,
    ParseUnits,
};
use plingo::framework::source::source_snapshot;
use plingo::framework::Workspace;
use plingo::reactive::Snapshot;

use super::json::{
    JsonArray, JsonDocument, JsonElements, JsonMember, JsonObject, JsonMembers, JsonToken,
    JsonValue,
};

// ---------------------------------------------------------------------------
// Canonical value (structure only; every ID is erased)
// ---------------------------------------------------------------------------

/// The ID-erased structural value of a parsed JSON document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Canonical {
    Str(String),
    Num(String),
    True,
    False,
    Null,
    Array(Vec<Canonical>),
    Object(Vec<(String, Canonical)>),
    /// A `#[parse_err]` region reached the root: an explicit error node.
    Error(String),
}

impl Canonical {
    /// Renders the canonical structure to a comparable string form.
    pub fn render(&self) -> String {
        match self {
            Canonical::Str(value) => format!("str({value:?})"),
            Canonical::Num(value) => format!("num({value})"),
            Canonical::True => "true".into(),
            Canonical::False => "false".into(),
            Canonical::Null => "null".into(),
            Canonical::Array(items) => {
                let inner = items
                    .iter()
                    .map(|item| item.render())
                    .collect::<Vec<_>>()
                    .join(",");
                format!("[{inner}]")
            }
            Canonical::Object(members) => {
                let inner = members
                    .iter()
                    .map(|(key, value)| format!("{key:?}:{}", value.render()))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{{{inner}}}")
            }
            Canonical::Error(description) => format!("error({description})"),
        }
    }
}

/// Payload text lookup: joins `AstToken` spans with committed token values.
///
/// The parser snapshot stores terminal plus resolved span; the semantic
/// payload lives in the token publication. The join is exact on span
/// coordinates from one committed revision pair.
pub struct ValueJoin {
    by_span: HashMap<(usize, usize), String>,
}

impl ValueJoin {
    fn new(tokens: &[LexToken<JsonToken>]) -> Self {
        Self {
            by_span: tokens
                .iter()
                .filter_map(|token| {
                    let value = match &token.value {
                        JsonToken::String(text) => text.clone(),
                        JsonToken::Number(text) => text.clone(),
                        _ => return None,
                    };
                    Some(((token.start, token.start + token.length), value))
                })
                .collect(),
        }
    }

    /// Resolves one AST token reference to its payload text.
    pub fn text<T: Send + Sync + 'static>(
        &self,
        snapshot: &AstSnapshot,
        token: &plingo::framework::parse::AstToken<T>,
    ) -> Option<String> {
        let entry = snapshot.token(*token)?;
        let range = &entry.span.range;
        self.by_span
            .get(&(range.start(), range.end()))
            .cloned()
    }
}

/// Converts a resolved JSON document into its canonical value. Resolution
/// goes through one committed `AstSnapshot`, so structure comes from a
/// single parser revision.
pub fn canonical_json(snapshot: &AstSnapshot, document: &JsonDocument, join: &ValueJoin) -> Canonical {
    match document {
        JsonDocument::Root(value) => canonical_value(snapshot, *value, join, 0),
        JsonDocument::Error(info) => Canonical::Error(error_projection(info)),
    }
}

fn canonical_value(
    snapshot: &AstSnapshot,
    boxed: AstBox<JsonValue>,
    join: &ValueJoin,
    depth: usize,
) -> Canonical {
    if depth > 10_000 {
        return Canonical::Error("depth limit exceeded (malformed fixture?)".into());
    }
    let Ok(value) = snapshot.resolve(boxed) else {
        return Canonical::Error("unresolvable".into());
    };
    match &*value {
        JsonValue::Object(inner) => canonical_object(snapshot, *inner, join, depth),
        JsonValue::Array(inner) => canonical_array(snapshot, *inner, join, depth),
        JsonValue::String(token) => Canonical::Str(join.text(snapshot, token).unwrap_or_default()),
        JsonValue::Number(token) => Canonical::Num(join.text(snapshot, token).unwrap_or_default()),
        JsonValue::True(_) => Canonical::True,
        JsonValue::False(_) => Canonical::False,
        JsonValue::Null(_) => Canonical::Null,
        JsonValue::Error(info) => Canonical::Error(error_projection(info)),
    }
}

fn canonical_object(
    snapshot: &AstSnapshot,
    boxed: AstBox<JsonObject>,
    join: &ValueJoin,
    depth: usize,
) -> Canonical {
    let Ok(object) = snapshot.resolve(boxed) else {
        return Canonical::Error("unresolvable object".into());
    };
    match &*object {
        JsonObject::Object(None) => Canonical::Object(Vec::new()),
        JsonObject::Object(Some(members)) => canonical_members(snapshot, *members, join, depth),
    }
}

fn canonical_members(
    snapshot: &AstSnapshot,
    boxed: AstBox<JsonMembers>,
    join: &ValueJoin,
    depth: usize,
) -> Canonical {
    let Ok(members) = snapshot.resolve(boxed) else {
        return Canonical::Error("unresolvable members".into());
    };
    let list = match &*members {
        JsonMembers::Members(list) => list,
    };
    let mut entries = Vec::new();
    for member in list.iter() {
        let Ok(member_value) = snapshot.resolve(*member) else {
            return Canonical::Error("unresolvable member".into());
        };
        match &*member_value {
            JsonMember::Member(key, val) => {
                entries.push((
                    join.text(snapshot, key).unwrap_or_default(),
                    canonical_value(snapshot, *val, join, depth + 1),
                ));
            }
        }
    }
    Canonical::Object(entries)
}

fn canonical_array(
    snapshot: &AstSnapshot,
    boxed: AstBox<JsonArray>,
    join: &ValueJoin,
    depth: usize,
) -> Canonical {
    let Ok(array) = snapshot.resolve(boxed) else {
        return Canonical::Error("unresolvable array".into());
    };
    let elements = match &*array {
        JsonArray::Array(None) => return Canonical::Array(Vec::new()),
        JsonArray::Array(Some(elements)) => *elements,
    };
    let Ok(resolved) = snapshot.resolve(elements) else {
        return Canonical::Error("unresolvable elements".into());
    };
    let list = match &*resolved {
        JsonElements::Elements(list) => list,
    };
    Canonical::Array(
        list.iter()
            .map(|element| canonical_value(snapshot, *element, join, depth + 1))
            .collect(),
    )
}

fn error_projection(info: &ParseErrorInfo) -> String {
    format!(
        "kind={:?} expected={:?} recovered={} location={:?}",
        info.kind, info.expected, info.recovered, info.location
    )
}

// ---------------------------------------------------------------------------
// Token / diagnostic projections
// ---------------------------------------------------------------------------

/// One projected token occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenProjection {
    pub terminal: String,
    pub value: String,
    pub span: (usize, usize),
    pub error_kind: Option<String>,
}

/// One projected parse diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticProjection {
    pub kind: String,
    pub node: usize,
    pub unexpected: Option<String>,
    pub expected: String,
    pub recovered: bool,
    pub location: Option<usize>,
}

/// The complete canonical pipeline projection for one document revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineProjection {
    pub source_len: usize,
    pub tokens: Vec<TokenProjection>,
    pub parse_status: Option<String>,
    pub roots: Vec<String>,
    pub diagnostics: Vec<DiagnosticProjection>,
}

fn project_token(token: &LexToken<JsonToken>) -> TokenProjection {
    let (terminal, value) = match &token.value {
        JsonToken::Whitespace => ("Whitespace", String::new()),
        JsonToken::LeftBrace => ("LeftBrace", String::new()),
        JsonToken::RightBrace => ("RightBrace", String::new()),
        JsonToken::LeftBracket => ("LeftBracket", String::new()),
        JsonToken::RightBracket => ("RightBracket", String::new()),
        JsonToken::Comma => ("Comma", String::new()),
        JsonToken::Colon => ("Colon", String::new()),
        JsonToken::String(text) => ("String", text.clone()),
        JsonToken::Number(text) => ("Number", text.clone()),
        JsonToken::True => ("True", String::new()),
        JsonToken::False => ("False", String::new()),
        JsonToken::Null => ("Null", String::new()),
        JsonToken::Error(_) => ("Error", String::new()),
    };
    TokenProjection {
        terminal: terminal.into(),
        value,
        span: (token.start, token.start + token.length),
        error_kind: token.error.as_ref().map(error_kind_label),
    }
}

fn error_kind_label(error: &LexErrorInfo) -> String {
    format!("{:?}", error.kind)
}

fn project_diagnostic(info: &ParseErrorInfo) -> DiagnosticProjection {
    DiagnosticProjection {
        kind: format!("{:?}", info.kind),
        node: info.node as usize,
        unexpected: info.unexpected.map(|symbol| format!("{symbol:?}")),
        expected: format!("{:?}", info.expected),
        recovered: info.recovered,
        location: info.location,
    }
}

/// Extracts the canonical projection from one committed engine snapshot.
pub fn project(ws_snapshot: &Snapshot, uri: &str) -> PipelineProjection {
    let source_len = source_snapshot(ws_snapshot, uri)
        .map(|snapshot| snapshot.len_bytes())
        .unwrap_or(0);

    let tokens_view = ws_snapshot.observe::<Tokens<JsonToken>>(uri.to_string());
    let tokens: Vec<TokenProjection> = match &tokens_view {
        Some(tokens_arc) => tokens_arc.tokens.iter().map(project_token).collect(),
        None => Vec::new(),
    };

    let unit = ws_snapshot
        .observe::<ParseUnits<JsonDocument>>(uri.to_string())
        .map(|value| (*value).clone());
    let parse_status = unit.as_ref().map(|unit| status_label(&unit.status));

    let snapshots = ws_snapshot.observe::<AstSnapshots<JsonDocument>>(uri.to_string());
    let roots = match (&unit, &snapshots) {
        (Some(unit), Some(document)) if unit.root.is_some() => {
            project_roots(document.arc(), &unit.root.expect("root checked"), &tokens_view)
        }
        _ => Vec::new(),
    };

    let diagnostics = ws_snapshot
        .list::<ParseDiagnostics>(&uri.to_string())
        .iter()
        .map(|info| project_diagnostic(info))
        .collect();

    PipelineProjection {
        source_len,
        tokens,
        parse_status,
        roots,
        diagnostics,
    }
}

fn status_label(status: &ParseStatus) -> String {
    match status {
        ParseStatus::Clean => "clean".into(),
        ParseStatus::Recovered { diagnostics } => format!("recovered({diagnostics})"),
        ParseStatus::Unrecoverable { diagnostics } => format!("unrecoverable({diagnostics})"),
    }
}

fn project_roots(
    snapshot: &AstSnapshot,
    root: &AstBox<JsonDocument>,
    tokens: &Option<Arc<TokenVec<JsonToken>>>,
) -> Vec<String> {
    let Some(document_arc) = snapshot.get(*root) else {
        return vec![];
    };
    let join = match tokens {
        Some(tokens_arc) => ValueJoin::new(&tokens_arc.tokens),
        None => ValueJoin { by_span: HashMap::new() },
    };
    vec![canonical_json(snapshot, &document_arc, &join).render()]
}

// ---------------------------------------------------------------------------
// Edit-trace runner with fresh-workspace oracle
// ---------------------------------------------------------------------------

/// Drives an incremental workspace through an edit trace and verifies the
/// canonical projection against a fresh workspace after every step.
pub struct TraceRunner {
    incremental: Workspace,
    fresh_builder: fn() -> Workspace,
    name: String,
    text: String,
    last_publication: Option<Arc<TokenVec<JsonToken>>>,
    steps: usize,
}

impl TraceRunner {
    /// Starts a runner over one opened document.
    pub fn open(builder: fn() -> Workspace, name: &str, text: &str) -> Self {
        let mut incremental = builder();
        let uri = super::uri(name);
        incremental.open(uri, text).expect("open commits");
        Self {
            incremental,
            fresh_builder: builder,
            name: name.to_string(),
            text: text.to_string(),
            last_publication: None,
            steps: 0,
        }
    }

    /// The underlying incremental workspace.
    pub fn workspace(&mut self) -> &mut Workspace {
        &mut self.incremental
    }

    /// Applies one edit batch and runs the full oracle comparison. Returns
    /// the committed workspace report for work-counter assertions.
    pub fn step(
        &mut self,
        edits: Vec<plingo::framework::source::SourceEdit>,
    ) -> plingo::framework::WorkspaceReport {
        let report = self.incremental.edit(edits).expect("edit commits");
        // The committed snapshot is the authoritative post-edit text.
        let uri_string = super::uri(&self.name).to_string();
        if let Some(snapshot) = source_snapshot(&self.incremental.snapshot(), &uri_string) {
            self.text = snapshot.to_string();
        } else {
            self.text.clear();
        }
        self.verify("after step");
        self.steps += 1;
        report
    }

    /// Runs the oracle comparison without applying an edit.
    pub fn verify(&self, context: &str) {
        let uri_string = super::uri(&self.name).to_string();
        let incremental_projection = project(&self.incremental.snapshot(), &uri_string);

        let mut fresh = (self.fresh_builder)();
        let fresh_uri = super::uri(&(format!("{}-fresh", self.name)));
        fresh.open(fresh_uri.clone(), &self.text).expect("fresh open commits");
        let fresh_projection = project(&fresh.snapshot(), &fresh_uri.to_string());

        assert_eq!(
            incremental_projection, fresh_projection,
            "{context} step {}: canonical projection diverged from fresh workspace",
            self.steps
        );
    }

    /// Asserts that an unchanged revision retains its publication by
    /// pointer identity (plan §10.4 identity retention).
    pub fn checkpoint_and_verify_retained_on_noop(&mut self) {
        let uri_string = super::uri(&self.name).to_string();
        let current = self
            .incremental
            .snapshot()
            .observe::<Tokens<JsonToken>>(uri_string);
        if let Some(previous) = &self.last_publication {
            assert!(
                current.as_ref().is_some_and(|current| Arc::ptr_eq(current, previous)),
                "unchanged revision must retain its publication Arc"
            );
        }
        self.last_publication = current;
    }

    /// The mirrored authoritative text.
    pub fn text(&self) -> &str {
        &self.text
    }
}
// ---------------------------------------------------------------------------
// Corruption detection support
// ---------------------------------------------------------------------------

/// Deliberately corrupts one projection field; used by gates to prove the
/// comparator detects divergence.
pub fn corrupt_tokens(projection: &mut PipelineProjection) {
    if let Some(token) = projection.tokens.first_mut() {
        token.value.push('!');
    } else {
        projection.source_len += 1;
    }
}

/// Deliberately corrupts the root structure.
pub fn corrupt_roots(projection: &mut PipelineProjection) {
    match projection.roots.first_mut() {
        Some(root) => root.insert(0, '!'),
        None => projection.roots.push("missing".into()),
    }
}

/// Deliberately corrupts diagnostics.
pub fn corrupt_diagnostics(projection: &mut PipelineProjection) {
    match projection.diagnostics.first_mut() {
        Some(diagnostic) => diagnostic.kind.push('!'),
        None => projection.parse_status = Some("corrupt".into()),
    }
}

/// Deliberately corrupts recovery state.
pub fn corrupt_recovery(projection: &mut PipelineProjection) {
    match projection.diagnostics.first_mut() {
        Some(diagnostic) => diagnostic.recovered = !diagnostic.recovered,
        None => projection.parse_status = Some("corrupt".into()),
    }
}
