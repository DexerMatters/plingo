//! STLC semantic snapshot digest (follow-up plan §4 items 1–2, 8, 13).
//!
//! `stlc_digest` enumerates the COMPLETE public-view domain of one
//! committed workspace state — tokens, parse status, structural tree,
//! diagnostics in explicit slot order, scope-graph payloads and labelled
//! edge buckets, resolutions, and the five structural products — as
//! ID-erased canonical rows. Raw node ordinals never appear: every
//! syntax-keyed view joins to a deterministic DFS path of the parsed tree
//! (`doc#0.2.1` style), so a warm workspace and a fresh cold build of the
//! same text are directly comparable.
//!
//! Edge triples render endpoint PAYLOADS, so equal-cardinality graph
//! buckets with different sources/targets produce different digests.
use std::collections::BTreeMap;
use std::sync::Arc;

use plingo::framework::lex::{TokenVec, Tokens};
use plingo::framework::parse::{
    AstSnapshot, AstSnapshots, AstToken, ParseDiagnostics, ParseStatus, TreeParseUnits,
};
use plingo::framework::scope::{ScopeGraph, ScopeNode};
use plingo::reactive::digest::SemanticDigest;
use plingo::reactive::kind::{GraphKey, ListKey};
use plingo::reactive::view::Node;
use plingo::reactive::{Snapshot, View};

use super::check::{StlcTypeDiagnostic, StlcTypeDiagnostics, StlcTypeValue};
use super::name_resolve::{
    StlcIncomingScopes, StlcReferenceCandidates, StlcResolution, StlcResolvedReferences, StlcScope,
    StlcScopeData, StlcScopeLabel,
};
use super::structural::{
    StlcLowered, StlcLoweredOrigin, StlcLoweredSummary, StlcLoweringDiagnostics, StlcNodeIndex,
    StlcNodeKind,
};
use super::syntax::{
    StlcCase, StlcDeclarationCase, StlcDocument, StlcDocumentCase, StlcExprCase, StlcParamCase,
    StlcPathCase, StlcToken, StlcTree, StlcTypeAtomCase, StlcTypeCase,
};

// ---------------------------------------------------------------------------
// Typed value renderers (semantic content only; no identities)
// ---------------------------------------------------------------------------

/// Renders an STLC type the way the surface writes it.
pub fn render_type(ty: &StlcTypeValue) -> String {
    match ty {
        StlcTypeValue::Nat => "Nat".into(),
        StlcTypeValue::Bool => "Bool".into(),
        StlcTypeValue::Unit => "Unit".into(),
StlcTypeValue::Function(function) => {
            let parameters = function
                .parameters()
                .iter()
                .map(render_type)
                .collect::<Vec<_>>()
                .join(" -> ");
            if parameters.is_empty() {
                render_type(function.result())
            } else {
                format!("({parameters} -> {})", render_type(function.result()))
            }
        }
    }
}

fn render_scope_data(data: &StlcScopeData) -> String {
    match data {
        StlcScopeData::Document => "document".into(),
        StlcScopeData::Lexical => "lexical".into(),
        // The `definition` field carries a raw syntax identity; the semantic
        // content of a declaration bucket is its name.
        StlcScopeData::Declaration { name, .. } => format!("decl({name})"),
        StlcScopeData::CaseSuccessor => "case-successor".into(),
        StlcScopeData::External { path } => format!("external({path})"),
    }
}

fn render_scope_node(node: &ScopeNode<StlcScope>) -> String {
    match node {
        ScopeNode::Scope(data) => format!("scope:{}", render_scope_data(data)),
        ScopeNode::Declaration(data) => format!("declaration:{}", render_scope_data(data)),
        ScopeNode::Reference(data) => format!("reference:{}", render_scope_data(data)),
    }
}

fn render_label(label: &StlcScopeLabel) -> String {
    match label {
        StlcScopeLabel::Lexical => "Lexical".into(),
        StlcScopeLabel::Declaration(name) => format!("Declaration({name})"),
        StlcScopeLabel::Import(path) => format!("Import({path})"),
    }
}

fn render_node_kind(kind: &StlcNodeKind) -> &'static str {
    match kind {
        StlcNodeKind::Document => "document",
        StlcNodeKind::Declaration => "declaration",
        StlcNodeKind::Expression => "expression",
        StlcNodeKind::Type => "type",
        StlcNodeKind::Other => "other",
    }
}

fn render_status(status: &ParseStatus) -> String {
    match status {
        ParseStatus::Clean => "clean".into(),
        ParseStatus::Recovered { diagnostics } => format!("recovered({diagnostics})"),
        ParseStatus::Unrecoverable { diagnostics } => format!("unrecoverable({diagnostics})"),
        other => format!("{other:?}"),
    }
}

/// Span-keyed lexeme table built once per document from the committed
/// token publication; `source_text` is crate-private, so lexemes join
/// through coordinates instead.
#[derive(Default)]
pub struct LexemeJoin {
    by_span: BTreeMap<(usize, usize), String>,
}

impl LexemeJoin {
    pub fn build(snapshot: &Snapshot, uri: &str) -> Option<Self> {
        let tokens = snapshot.observe::<Tokens<StlcToken>>(uri.to_string())?;
        let mut join = LexemeJoin::default();
        for token in tokens.tokens.iter() {
            join.by_span.insert(
                (token.start, token.start + token.length),
                render_token_value(&token.value),
            );
        }
        Some(join)
    }

    fn text(&self, span: &plingo::utils::Span) -> String {
        self.by_span
            .get(&(span.range.start(), span.range.end()))
            .cloned()
            .unwrap_or_else(|| "?".into())
    }
}

/// The semantic lexeme of one token value (identifier or number text).
fn render_token_value(value: &StlcToken) -> String {
    match value {
        StlcToken::Ident(text) => text.clone(),
        StlcToken::Number(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

fn resolve_lexeme<T: Send + Sync + 'static>(
    join: Option<&LexemeJoin>,
    ast: Option<&AstSnapshot>,
    token: AstToken<T>,
) -> String {
    let (Some(join), Some(ast)) = (join, ast) else {
        return "?".into();
    };
    match ast.token(token) {
        Some(entry) => join.text(&entry.span),
        None => "?".into(),
    }
}

/// Renders one node's case variant with resolved identifier lexemes.
fn render_case(join: Option<&LexemeJoin>, ast: Option<&AstSnapshot>, case: &StlcCase) -> String {
    match case {
        StlcCase::Document(StlcDocumentCase::Lines { .. }) => "Document::Lines".into(),
        StlcCase::Document(StlcDocumentCase::Error { .. }) => "Document::Error".into(),
        StlcCase::Declaration(StlcDeclarationCase::Value { f0, .. }) => {
            format!("Declaration::Value({})", resolve_lexeme(join, ast, *f0))
        }
        StlcCase::Declaration(StlcDeclarationCase::Import { .. }) => "Declaration::Import".into(),
        StlcCase::Declaration(StlcDeclarationCase::Export { .. }) => "Declaration::Export".into(),
        StlcCase::Declaration(StlcDeclarationCase::Error { .. }) => "Declaration::Error".into(),
        StlcCase::Path(StlcPathCase::Segments { .. }) => "Path::Segments".into(),
        StlcCase::Param(StlcParamCase::Bare { f0, .. }) => {
            format!("Param::Bare({})", resolve_lexeme(join, ast, *f0))
        }
        StlcCase::Param(StlcParamCase::Parenthesized { f0, .. }) => {
            format!("Param::Parenthesized({})", resolve_lexeme(join, ast, *f0))
        }
        StlcCase::Type(StlcTypeCase::Arrow { .. }) => "Type::Arrow".into(),
        StlcCase::Type(StlcTypeCase::Atom { .. }) => "Type::Atom".into(),
        StlcCase::Type(StlcTypeCase::Error { .. }) => "Type::Error".into(),
        StlcCase::TypeAtom(StlcTypeAtomCase::Nat { .. }) => "Atom::Nat".into(),
        StlcCase::TypeAtom(StlcTypeAtomCase::Unit { .. }) => "Atom::Unit".into(),
        StlcCase::TypeAtom(StlcTypeAtomCase::Bool { .. }) => "Atom::Bool".into(),
        StlcCase::TypeAtom(StlcTypeAtomCase::Parenthesized { .. }) => "Atom::Group".into(),
        StlcCase::Expr(expr) => render_expr_case(join, ast, expr),
        _ => "unknown-case".into(),
    }
}

fn render_expr_case(
    join: Option<&LexemeJoin>,
    ast: Option<&AstSnapshot>,
    expr: &StlcExprCase,
) -> String {
    match expr {
        StlcExprCase::If { .. } => "Expr::If".into(),
        StlcExprCase::Case { f2, .. } => {
            format!("Expr::Case(successor={})", resolve_lexeme(join, ast, *f2))
        }
        StlcExprCase::True { .. } => "Expr::True".into(),
        StlcExprCase::False { .. } => "Expr::False".into(),
        StlcExprCase::Let { f0, .. } => format!("Expr::Let({})", resolve_lexeme(join, ast, *f0)),
        StlcExprCase::Lambda { .. } => "Expr::Lambda".into(),
        StlcExprCase::Add { .. } => "Expr::Add".into(),
        StlcExprCase::Apply { .. } => "Expr::Apply".into(),
        StlcExprCase::Succ { .. } => "Expr::Succ".into(),
        StlcExprCase::Group { .. } => "Expr::Group".into(),
        StlcExprCase::Number { f0 } => format!("Expr::Number({})", resolve_lexeme(join, ast, *f0)),
        StlcExprCase::Variable { f0 } => {
            format!("Expr::Variable({})", resolve_lexeme(join, ast, *f0))
        }
        StlcExprCase::Unit { .. } => "Expr::Unit".into(),
        StlcExprCase::Error { .. } => "Expr::Error".into(),
        _ => "unknown-expr".into(),
    }
}

fn render_error(error: &super::check::StlcTypeError) -> String {
    use super::check::StlcTypeError;
    match error {
        StlcTypeError::Mismatch { expected, found } => format!(
            "mismatch{{expected:{},found:{}}}",
            render_type(expected),
            render_type(found)
        ),
        StlcTypeError::NonFunctionApplication { found } => {
            format!("non-function-application{{found:{}}}", render_type(found))
        }
        StlcTypeError::BranchMismatch { then_ty, else_ty } => format!(
            "branch-mismatch{{then:{},else:{}}}",
            render_type(then_ty),
            render_type(else_ty)
        ),
        StlcTypeError::UnboundVariable { name } => format!("unbound-variable{{{name}}}"),
        StlcTypeError::UnboundVariable { name } => format!("unbound-variable{{{name}}}"),
        StlcTypeError::MissingParameterAnnotation => "missing-parameter-annotation".into(),
    }
}

fn render_type_diagnostic(diagnostic: &StlcTypeDiagnostic) -> String {
    render_error(&diagnostic.error)
}

// ---------------------------------------------------------------------------
// Tree projection: stable DFS paths joined to every node-keyed view
// ---------------------------------------------------------------------------

/// One document's tree projection.
///
/// Paths are local: a root renders `{doc}#{root-ordinal}` and a child
/// appends `.position-among-siblings`, so an inserted subtree renumbers
/// nothing outside its own branch.
struct TreeProjection {
    /// Join key (deterministic per engine capture) → canonical path.
    paths: BTreeMap<u64, String>,
    /// Canonical path → rendered case row.
    cases: BTreeMap<String, String>,
    /// Roots in publication order.
    roots: Vec<String>,
}

impl TreeProjection {
    fn build(
        snapshot: &Snapshot,
        join: Option<&LexemeJoin>,
        ast: Option<&AstSnapshot>,
        uri: &str,
    ) -> Self {
        let mut projection = Self {
            paths: BTreeMap::new(),
            cases: BTreeMap::new(),
            roots: Vec::new(),
        };
        let doc = doc_prefix(uri);
        let key = uri.to_string();
        // Root-order facts exist only after an explicit root splice; the
        // initial document root handle rides in TreeParseUnits (plan
        // §19.6 removes this companion lifecycle in Phase 1).
        let mut roots: Vec<Node<StlcTree>> = StlcTree::snapshot_roots_of(snapshot, &key);
        if roots.is_empty() {
            if let Some(unit) = snapshot.observe::<TreeParseUnits<StlcDocument>>(uri.to_string())
                && let Some(root) = unit.root.clone()
            {
                roots.push(root);
            }
        }
        for root in roots {
            let root_path = format!("{doc}#{}", projection.roots.len());
            projection.roots.push(root_path.clone());
            projection.visit(snapshot, join, ast, root, &root_path);
        }
        projection
    }

    #[allow(clippy::too_many_arguments)]
    fn visit(
        &mut self,
        snapshot: &Snapshot,
        join: Option<&LexemeJoin>,
        ast: Option<&AstSnapshot>,
        id: Node<StlcTree>,
        path: &str,
    ) {
        let key = join_key(id.clone());
        if self.paths.contains_key(&key) {
            return;
        }
        self.paths.insert(key, path.to_owned());
        if let Some(case) = StlcTree::snapshot_case(snapshot, id.clone()) {
            let rendered = render_case(join, ast, &case);
            self.cases.insert(path.to_owned(), rendered);
        }
        for (position, child) in StlcTree::snapshot_children(snapshot, id.clone())
            .iter()
            .enumerate()
        {
            let child_path = format!("{path}.{position}");
            self.visit(snapshot, join, ast, child.clone(), &child_path);
        }
    }

    fn path_of(&self, id: Node<StlcTree>) -> Option<&str> {
        self.paths.get(&join_key(id)).map(|path| path.as_str())
    }

    fn len(&self) -> usize {
        self.paths.len()
    }
}

/// Internal per-capture join key; never rendered.
fn join_key(id: Node<StlcTree>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    std::hash::Hasher::finish(&hasher)
}

fn doc_prefix(uri: &str) -> String {
    uri.trim_start_matches("test://").to_owned()
}

// ---------------------------------------------------------------------------
// The digest
// ---------------------------------------------------------------------------

/// Captures every public view's complete committed content.
pub fn stlc_digest(snapshot: &Snapshot) -> SemanticDigest {
    let mut digest = SemanticDigest::new();

    // Document universe: union of token publications and parse units.
    let mut uris: Vec<String> = snapshot.inputs::<Tokens<StlcToken>>();
    uris.extend(snapshot.inputs::<TreeParseUnits<StlcDocument>>());
    uris.sort();
    uris.dedup();

    for uri in &uris {
        let prefix = doc_prefix(uri);

        // Ordered semantic tokens: source-stable coordinate rows.
        if let Some(tokens) = snapshot.observe::<Tokens<StlcToken>>(uri.clone()) {
            let TokenVec {
                tokens: list,
                errors,
            } = &*tokens;
            for (ordinal, token) in list.iter().enumerate() {
                digest.insert_domain(
                    &format!("{prefix}:tokens"),
                    ordinal,
                    &format!(
                        "{}..{}:{:?}",
                        token.start,
                        token.start + token.length,
                        token.value
                    ),
                );
            }
            digest.insert(
                &format!("{prefix}:lex"),
                "errors",
                &errors.len().to_string(),
            );
        }

        // Parse unit: status + full structural projection.
        let unit = snapshot.observe::<TreeParseUnits<StlcDocument>>(uri.clone());
        let ast = snapshot
            .observe::<AstSnapshots<StlcDocument>>(uri.clone())
            .map(|document| Arc::clone(document.arc()));
        let ast = ast.as_deref();
        let Some(unit) = unit else {
            continue;
        };
        digest.insert(
            &format!("{prefix}:parse"),
            "status",
            &render_status(&unit.status),
        );

        let join = LexemeJoin::build(snapshot, uri);
        let projection = TreeProjection::build(snapshot, join.as_ref(), ast, uri);
        digest.insert(
            &format!("{prefix}:tree"),
            "nodes",
            &projection.len().to_string(),
        );
        for (index, root) in projection.roots.iter().enumerate() {
            digest.insert(&format!("{prefix}:roots"), &index.to_string(), root);
        }
        for (path, case_row) in &projection.cases {
            digest.insert(&format!("{prefix}:cases"), path, case_row);
        }

        // Node-keyed semantic views joined to stable paths.
        for input in snapshot.inputs::<StlcIncomingScopes>() {
            let Some(path) = projection.path_of(input.clone()).map(|path| path.to_owned()) else {
                continue;
            };
            digest.insert(&format!("{prefix}:enclosing"), &path, "present");
        }
        for input in snapshot.inputs::<StlcReferenceCandidates>() {
            let Some(path) = projection.path_of(input.clone()).map(|path| path.to_owned()) else {
                continue;
            };
            digest.insert(&format!("{prefix}:reference-candidates"), &path, "present");
        }
        for input in snapshot.inputs::<StlcResolvedReferences>() {
            let Some(path) = projection.path_of(input.clone()).map(|path| path.to_owned()) else {
                continue;
            };
            let resolution = snapshot.observe::<StlcResolvedReferences>(input.clone());
            let row = match resolution.as_deref() {
                Some(StlcResolution::Resolved { declaration }) => snapshot
                    .graph_node::<ScopeGraph<StlcScope>>(declaration.node())
                    .as_deref()
                    .map(|payload| format!("resolved({})", render_scope_node(payload)))
                    .unwrap_or_else(|| "resolved(<absent>)".into()),
                Some(StlcResolution::Unbound { name }) => format!("unbound({name})"),
                None => continue,
            };
            digest.insert(&format!("{prefix}:resolutions"), &path, &row);
        }

        // Structural products.
        for input in snapshot.inputs::<StlcNodeIndex>() {
            let Some(path) = projection.path_of(input.clone()).map(|path| path.to_owned()) else {
                continue;
            };
            if let Some(kind) = snapshot.observe::<StlcNodeIndex>(input.clone()) {
                digest.insert(
                    &format!("{prefix}:node-index"),
                    &path,
                    render_node_kind(&kind),
                );
            }
        }
        for input in snapshot.inputs::<StlcLowered>() {
            let Some(path) = projection.path_of(input.clone()).map(|path| path.to_owned()) else {
                continue;
            };
            if let Some(lowered) = snapshot.observe::<StlcLowered>(input.clone()) {
                digest.insert(&format!("{prefix}:lowered"), &path, lowered.as_str());
            }
        }
        for input in snapshot.inputs::<StlcLoweredOrigin>() {
            let Some(path) = projection.path_of(input.clone()).map(|path| path.to_owned()) else {
                continue;
            };
            if let Some(origin) = snapshot.observe::<StlcLoweredOrigin>(input.clone()) {
                let row = projection
                    .path_of(origin.as_ref().clone())
                    .unwrap_or("<orphan>")
                    .to_owned();
                digest.insert(&format!("{prefix}:lowered-origin"), &path, &row);
            }
        }
        for input in snapshot.inputs::<StlcLoweredSummary>() {
            let Some(path) = projection.path_of(input.clone()).map(|path| path.to_owned()) else {
                continue;
            };
            if let Some(summary) = snapshot.observe::<StlcLoweredSummary>(input.clone()) {
                digest.insert(
                    &format!("{prefix}:lowered-summary"),
                    &path,
                    summary.as_str(),
                );
            }
        }

        // List-keyed diagnostics in explicit slot order.
        for input in snapshot.inputs::<ParseDiagnostics>() {
            let ListKey::Slot(domain, slot) = &input else {
                continue;
            };
            if domain != uri {
                continue;
            }
            if let Some(item) = snapshot.observe::<ParseDiagnostics>(input.clone()) {
                digest.insert(
                    &format!("{prefix}:parse-diagnostics"),
                    &slot.to_string(),
                    &format!("{item:?}"),
                );
            }
        }
        for input in snapshot.inputs::<StlcTypeDiagnostics>() {
            let ListKey::Slot(node, _slot) = &input else {
                continue;
            };
            let Some(path) = projection.path_of(node.clone()).map(|path| path.to_owned()) else {
                continue;
            };
            let ListKey::Slot(_, slot) = &input else {
                unreachable!()
            };
            let item = snapshot.observe::<StlcTypeDiagnostics>(input.clone());
            let Some(plingo::reactive::kind::ListFact::Item(diagnostic)) = item.as_deref() else {
                continue;
            };
            digest.insert(
                &format!("{prefix}:type-diagnostics"),
                &format!("{path}[{slot}]"),
                &render_type_diagnostic(diagnostic),
            );
        }
        for input in snapshot.inputs::<StlcLoweringDiagnostics>() {
            let ListKey::Slot(node, slot) = &input else {
                continue;
            };
            let Some(path) = projection.path_of(node.clone()).map(|path| path.to_owned()) else {
                continue;
            };
            let item = snapshot.observe::<StlcLoweringDiagnostics>(input.clone());
            let Some(plingo::reactive::kind::ListFact::Item(text)) = item.as_deref() else {
                continue;
            };
            digest.insert(
                &format!("{prefix}:lowering-diagnostics"),
                &format!("{path}[{slot}]"),
                text,
            );
        }
    }

    emit_scope_graph_rows(snapshot, &mut digest);
    digest
}

/// Scope graph: payload multiset plus exact edge triples.
///
/// Endpoints render by payload content, so equal-cardinality buckets whose
/// sources/targets differ produce different digests (plan §4 exit gate).
fn emit_scope_graph_rows(snapshot: &Snapshot, digest: &mut SemanticDigest) {
    let mut nodes: Vec<String> = Vec::new();
    let mut edges: Vec<String> = Vec::new();
    for input in snapshot.inputs::<ScopeGraph<StlcScope>>() {
        match input {
            GraphKey::Node(node) => {
                if let Some(payload) =
                    snapshot.graph_node::<ScopeGraph<StlcScope>>(node.clone())
                {
                    nodes.push(render_scope_node(&payload));
                }
            }
            GraphKey::Bucket(source, label) => {
                let label_row = render_label(&label);
                let source_payload = snapshot
                    .graph_node::<ScopeGraph<StlcScope>>(source.clone())
                    .map(|payload| render_scope_node(&payload))
                    .unwrap_or_else(|| "<absent>".into());
                for target in
                    snapshot.outgoing::<ScopeGraph<StlcScope>>(source.clone(), &label)
                {
                    let target_payload = snapshot
                        .graph_node::<ScopeGraph<StlcScope>>(target)
                        .map(|payload| render_scope_node(&payload))
                        .unwrap_or_else(|| "<absent>".into());
                    edges.push(format!(
                        "({source_payload})--{label_row}->({target_payload})"
                    ));
                }
            }
            _ => {}
        }
    }
    nodes.sort();
    edges.sort();
    for (ordinal, row) in nodes.iter().enumerate() {
        digest.insert_domain("graph:nodes", ordinal, row);
    }
    for (ordinal, row) in edges.iter().enumerate() {
        digest.insert_domain("graph:edges", ordinal, row);
    }
}
