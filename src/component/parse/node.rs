//! Parser provider for the node graph runtime.
//!
//! Parsing observes a token stream and materializes an immutable typed AST view.
//! It has no downstream type or pass method.

use std::{marker::PhantomData, sync::Arc, time::Instant};

use fluent_uri::Uri;

use crate::{
    component::{
        lex::{LexerNode, LexerRoot, node::TokenChanges},
        parse::{
            AstView, ParseErrorInfo, Parser,
            data::{ast::AstBox, product::ProductId},
        },
    },
    scheme::node::{ComponentState, DeriveCx, Node, NodeError, ReclaimCx, View},
};

/// A typed, immutable parser result for one document.
struct ParsedDocument<Ast> {
    pub roots: Arc<[ProductId]>,
    pub ast: Arc<AstView<Ast>>,
}

impl<Ast> Clone for ParsedDocument<Ast> {
    fn clone(&self) -> Self {
        Self {
            roots: Arc::clone(&self.roots),
            ast: Arc::clone(&self.ast),
        }
    }
}

impl<Ast> PartialEq for ParsedDocument<Ast> {
    fn eq(&self, other: &Self) -> bool {
        self.roots == other.roots
            && self.ast.uri() == other.ast.uri()
            && self
                .ast
                .roots()
                .iter()
                .map(|root| (root.id, root.uri))
                .eq(other.ast.roots().iter().map(|root| (root.id, root.uri)))
            && self.ast.entries().iter().map(ast_identity).eq(other
                .ast
                .entries()
                .iter()
                .map(ast_identity))
    }
}

impl<Ast> Eq for ParsedDocument<Ast> {}

fn ast_identity<Ast>(
    entry: &crate::component::parse::AstViewEntry<Ast>,
) -> (usize, Uri<&'static str>, ProductId) {
    (entry.ast_box.id, entry.ast_box.uri, entry.product)
}

/// Stable identity of one reachable typed AST value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AstKey {
    pub uri: Uri<&'static str>,
    pub id: usize,
}

/// One independently observable typed AST artifact.
pub struct ParsedAst<Root, Ast>(PhantomData<fn() -> (Root, Ast)>);

/// The payload deliberately compares parser identity rather than requiring an
/// application AST type to implement `PartialEq`.
pub struct AstArtifact<Ast> {
    pub ast_box: AstBox<Ast>,
    pub product: ProductId,
    pub value: Arc<Ast>,
}

impl<Ast> Clone for AstArtifact<Ast> {
    fn clone(&self) -> Self {
        Self {
            ast_box: self.ast_box,
            product: self.product,
            value: Arc::clone(&self.value),
        }
    }
}

impl<Ast> PartialEq for AstArtifact<Ast> {
    fn eq(&self, other: &Self) -> bool {
        self.ast_box.id == other.ast_box.id
            && self.ast_box.uri == other.ast_box.uri
            && self.product == other.product
    }
}

impl<Ast> Eq for AstArtifact<Ast> {}

impl<Root, Ast> View for ParsedAst<Root, Ast>
where
    Root: LexerRoot,
    Ast: Send + Sync + 'static,
{
    type Key = AstKey;
    type Value = AstArtifact<Ast>;
}

/// The ordered typed-AST roots of one document.
pub struct ParseRoots<Root, Ast>(PhantomData<fn() -> (Root, Ast)>);

/// Parser diagnostics materialized for one document snapshot.
pub struct ParseDiagnostics<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for ParseDiagnostics<Root> {
    type Key = Uri<&'static str>;
    type Value = Arc<[ParseErrorInfo]>;
}

/// Materialized parser state for editor-facing consumers. Recovered input
/// commits partial roots and diagnostics; an unrecoverable revision commits an
/// empty root set rather than preserving stale syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseStatus {
    Clean,
    Recovered { diagnostics: usize },
    Unrecoverable { diagnostics: usize },
}

pub struct ParseStatusView<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for ParseStatusView<Root> {
    type Key = Uri<&'static str>;
    type Value = ParseStatus;
}

/// Incremental replay work performed for the current token revision.
pub struct ParseStats<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for ParseStats<Root> {
    type Key = Uri<&'static str>;
    type Value = crate::component::parse::IncrementalParseStats;
}

impl<Root, Ast> View for ParseRoots<Root, Ast>
where
    Root: LexerRoot,
    Ast: Send + Sync + 'static,
{
    type Key = Uri<&'static str>;
    type Value = Arc<[AstKey]>;
}

/// Incremental parser node. Its mutable replay arenas are implementation
/// storage; callers observe keyed root and AST artifacts.
pub struct ParserNode<Root, Ast>
where
    Root: LexerRoot + Clone,
    Ast: Send + Sync + 'static,
{
    parser: ComponentState<Parser<Root>>,
    _ast: PhantomData<fn() -> Ast>,
}

impl<Root, Ast> ParserNode<Root, Ast>
where
    Root: LexerRoot + Clone,
    Ast: Send + Sync + 'static,
{
    pub fn from_parser(parser: Parser<Root>) -> Self {
        Self {
            parser: ComponentState::new(parser),
            _ast: PhantomData,
        }
    }
}

impl<Root, Ast> Node for ParserNode<Root, Ast>
where
    Root: LexerRoot + Clone,
    Ast: Send + Sync + 'static,
{
    type Key = Uri<&'static str>;
    type Output = ParseRoots<Root, Ast>;

    fn derive(
        &self,
        cx: &mut DeriveCx<'_, '_>,
        uri: Self::Key,
    ) -> Result<Arc<[AstKey]>, NodeError> {
        let total_start = Instant::now();
        let inputs_start = Instant::now();
        let _ = cx.require::<LexerNode<Root>>(uri)?;
        let token_delta = cx.observe::<TokenChanges<Root>>(uri)?;
        let inputs_elapsed = inputs_start.elapsed();
        let changed_tokens = token_delta
            .changes
            .iter()
            .map(|change| {
                change
                    .splices
                    .iter()
                    .map(|splice| splice.removed.len() + splice.inserted.len())
                    .sum::<usize>()
            })
            .sum::<usize>();

        let (parsed, diagnostics, stats, replay_elapsed, ast_elapsed, diagnostics_elapsed) = {
            let parser = cx.state_mut(&self.parser)?;
            let replay_start = Instant::now();
            let roots = parser
                .derive_changes(uri, &token_delta.changes)
                .map_err(|error| NodeError::message(error.to_string()))?;
            let replay_elapsed = replay_start.elapsed();
            let ast_start = Instant::now();
            let ast = parser
                .ast_view::<Ast>(&parser.latest, uri)
                .map_err(|error| NodeError::message(error.to_string()))?;
            let ast_elapsed = ast_start.elapsed();
            let diagnostics_start = Instant::now();
            let diagnostics: Arc<[ParseErrorInfo]> = parser.latest_parse_diagnostics(uri).into();
            let diagnostics_elapsed = diagnostics_start.elapsed();
            let stats = parser.incremental_stats(uri).unwrap_or_default();
            (
                ParsedDocument {
                    roots: roots.into(),
                    ast: Arc::new(ast),
                },
                diagnostics,
                stats,
                replay_elapsed,
                ast_elapsed,
                diagnostics_elapsed,
            )
        };

        let emit_start = Instant::now();
        let roots: Arc<[AstKey]> = parsed
            .ast
            .roots()
            .iter()
            .map(|root| AstKey {
                uri: root.uri,
                id: root.id,
            })
            .collect::<Vec<_>>()
            .into();
        for entry in parsed.ast.entries() {
            cx.emit::<ParsedAst<Root, Ast>>(
                AstKey {
                    uri: entry.ast_box.uri,
                    id: entry.ast_box.id,
                },
                AstArtifact {
                    ast_box: entry.ast_box,
                    product: entry.product,
                    value: Arc::clone(&entry.value),
                },
            )?;
        }
        let status = if diagnostics.is_empty() {
            ParseStatus::Clean
        } else if roots.is_empty() {
            ParseStatus::Unrecoverable {
                diagnostics: diagnostics.len(),
            }
        } else {
            ParseStatus::Recovered {
                diagnostics: diagnostics.len(),
            }
        };
        cx.emit::<ParseStats<Root>>(uri, stats)?;
        cx.emit::<ParseStatusView<Root>>(uri, status)?;
        cx.emit::<ParseDiagnostics<Root>>(uri, diagnostics)?;
        let emit_elapsed = emit_start.elapsed();
        eprintln!(
            "[parse-node] uri={uri} total={:?} inputs={:?} replay={:?} ast_view={:?} diagnostics={:?} emit={:?} token_changes={} changed_tokens={} roots={}",
            total_start.elapsed(),
            inputs_elapsed,
            replay_elapsed,
            ast_elapsed,
            diagnostics_elapsed,
            emit_elapsed,
            token_delta.changes.len(),
            changed_tokens,
            roots.len(),
        );
        Ok(roots)
    }

    fn reclaim(&self, cx: &mut ReclaimCx<'_, '_>, uri: Self::Key) -> Result<(), NodeError> {
        let has_other_documents = cx.has_materialized::<Self>();
        let parser = cx.state_mut(&self.parser)?;
        if has_other_documents {
            parser.forget_document(uri);
        } else {
            parser.reset_documents();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use plingo_macros::{NonTerminal, Terminal};

    use super::*;
    use crate::{
        component::{
            lex::{LexErrorInfo, LexerNode},
            parse::{AstToken, grammar::Grammar},
            source::{SourceEdit, SourceNode},
        },
        scheme::node::Graph,
        utils::Span,
    };

    #[derive(Terminal, Debug, Clone, PartialEq, Eq, Hash)]
    #[scopes(root { Number })]
    enum Tokens {
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

    #[test]
    fn parser_node_derives_an_ast_view_from_the_token_view() {
        let uri = Span::new("test://node-parser", 0, 0).unwrap().uri;
        let parser = Grammar::from_spec::<Value>().build_lr1::<Tokens>();
        let mut graph = Graph::new();
        graph.install(LexerNode::<Tokens>::new().unwrap()).unwrap();
        graph
            .install(ParserNode::<Tokens, Value>::from_parser(parser))
            .unwrap();
        graph.command(SourceNode::load(uri)).unwrap();
        graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: "42".into(),
            }))
            .unwrap();

        let roots = graph.request::<ParserNode<Tokens, Value>>(uri).unwrap();
        assert_eq!(roots.len(), 1);
        let artifact = graph
            .read::<ParsedAst<Tokens, Value>>(roots[0].clone())
            .expect("the root AST artifact must be materialized");
        assert_eq!(artifact.ast_box.id, roots[0].id);
        assert_eq!(artifact.ast_box.uri, uri);
    }
}
