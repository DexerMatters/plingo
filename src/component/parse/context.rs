//! Borrowed parser-view operations over a component [`Context`].

use std::{marker::PhantomData, sync::Arc};

use fluent_uri::Uri;

use crate::{
    component::{
        api::{Component, Context, ContextView, Error},
        lex::{LexerRoot, TokenEntryKey, TokenLexeme},
        parse::{
            AstArtifact, AstKey, AstLocation, AstSnapshot, AstToken, IncrementalParseStats,
            ParseCandidate, ParseCandidates, ParseDiagnostics, ParseEntries, ParseSnapshot,
            ParseStats, ParseStatus, ParseStatusView, ParsedAst, ResolvedAst,
            data::{AstBox, ProductId},
        },
        structural::StructureEntry,
    },
    scheme::node::{NodeError, NodeKey},
    utils::Span,
};

/// The accepted interpretation of one document, selected by the parser's
/// canonical acceptance policy.
pub struct Accepted<Ast> {
    key: AstKey,
    value: Arc<Ast>,
}

impl<Ast> Accepted<Ast> {
    /// The root AST identity of the accepted document.
    pub fn key(&self) -> AstKey {
        self.key.clone()
    }

    /// The accepted document value.
    pub fn value(&self) -> &Arc<Ast> {
        &self.value
    }
}

/// Borrowed access to parser snapshots and their structural projections.
///
/// Every read retains the parser as a child of the current component, so the
/// parser stays materialized while any component observes its facts.
pub struct ParsedView<'cx, 'tx, C: Component, Root: LexerRoot + Clone, Ast: Send + Sync + 'static> {
    cx: &'cx mut Context<'tx, C>,
    _root: PhantomData<fn() -> (Root, Ast)>,
}

impl<'cx, 'tx, C: Component, Root: LexerRoot + Clone, Ast: Send + Sync + 'static>
    ParsedView<'cx, 'tx, C, Root, Ast>
{
    pub(crate) fn open(cx: &'cx mut Context<'tx, C>) -> Self {
        Self {
            cx,
            _root: PhantomData,
        }
    }

    fn retain_parser(&mut self, uri: Uri<&'static str>) {
        self.cx
            .retain_provider::<crate::component::parse::ParserNode<Root, Ast>>(uri);
    }

    /// Reads one parser artifact and recovers its original payload arc.
    pub fn artifact<T: Send + Sync + 'static>(&mut self, key: AstKey) -> Option<Arc<T>> {
        self.cx
            .get::<ParsedAst<Root>>(key)
            .and_then(|artifact| artifact.deref::<T>())
    }

    /// Reads the complete erased artifact, including its product identity.
    pub fn raw_artifact(&mut self, key: AstKey) -> Option<AstArtifact> {
        self.cx.get::<ParsedAst<Root>>(key)
    }

    /// Reads the parser's immutable document snapshot, retaining the parser.
    pub fn snapshot(&mut self, uri: Uri<&'static str>) -> Option<Arc<AstSnapshot>> {
        self.retain_parser(uri);
        self.cx.get::<ParseSnapshot<Root>>(uri)
    }

    /// Resolves a typed AST box against the current immutable snapshot.
    pub fn resolve<T: Send + Sync + 'static>(
        &mut self,
        uri: Uri<&'static str>,
        node: AstBox<T>,
    ) -> Result<ResolvedAst<T>, Error> {
        let snapshot = self
            .snapshot(uri)
            .ok_or_else(NodeError::missing_view::<ParseSnapshot<Root>>)?;
        snapshot
            .resolve(node)
            .map_err(|error| NodeError::message(error.to_string()).into())
    }

    /// Reads stable source text for one typed AST token.
    pub fn token_text(
        &mut self,
        uri: Uri<&'static str>,
        token: AstToken<Root>,
    ) -> Option<Arc<str>> {
        self.cx
            .get::<TokenLexeme<Root>>(TokenEntryKey { uri, id: token.id })
    }

    /// Reads one AST location projection.
    pub fn location(&mut self, key: AstKey) -> Option<Span> {
        self.cx.get::<AstLocation<Root>>(key)
    }

    /// Reads the source text covered by one AST box.
    pub fn source_text<T: Send + Sync + 'static>(
        &mut self,
        uri: Uri<&'static str>,
        node: AstBox<T>,
    ) -> Result<String, Error> {
        let resolved = self.resolve(uri, node)?;
        let snapshot = self
            .snapshot(uri)
            .ok_or_else(NodeError::missing_view::<ParseSnapshot<Root>>)?;
        Ok(snapshot.source_text(resolved.span()))
    }

    /// Reads all parser discovery entries for one document.
    pub fn entries(
        &mut self,
        uri: Uri<&'static str>,
    ) -> Vec<StructureEntry<AstKey, Uri<&'static str>, ProductId>> {
        self.retain_parser(uri);
        crate::scheme::node::ReadGraph::scan::<ParseEntries<Root>>(&self.cx.derive, uri)
    }

    /// Reads complete accepted interpretations for one document.
    pub fn candidates<A: Send + Sync + 'static>(
        &mut self,
        uri: Uri<&'static str>,
    ) -> Option<Arc<[ParseCandidate<A>]>> {
        self.retain_parser(uri);
        self.cx.get::<ParseCandidates<Root, A>>(uri)
    }

    /// Reads parser diagnostics for one document.
    pub fn diagnostics(
        &mut self,
        uri: Uri<&'static str>,
    ) -> Option<Arc<[crate::component::parse::ParseErrorInfo]>> {
        self.retain_parser(uri);
        self.cx.get::<ParseDiagnostics<Root>>(uri)
    }

    /// Reads parser status for one document.
    pub fn status(&mut self, uri: Uri<&'static str>) -> Option<ParseStatus> {
        self.retain_parser(uri);
        self.cx.get::<ParseStatusView<Root>>(uri)
    }

    /// Reads parser incremental replay statistics for one document.
    pub fn stats(&mut self, uri: Uri<&'static str>) -> Option<IncrementalParseStats> {
        self.retain_parser(uri);
        self.cx.get::<ParseStats<Root>>(uri)
    }

    /// Selects the accepted document under the canonical parser policy.
    ///
    /// A legitimately rejected parse (ambiguous or unrecoverable) yields
    /// `Ok(None)`. An unavailable parser producer suspends the current
    /// component and reruns it after the parser publishes.
    pub fn accepted(&mut self, uri: Uri<&'static str>) -> Result<Option<Accepted<Ast>>, Error> {
        self.retain_parser(uri);
        let snapshot = self.cx.get::<ParseSnapshot<Root>>(uri);
        let candidates = self.cx.get::<ParseCandidates<Root, Ast>>(uri);
        let status = self.cx.get::<ParseStatusView<Root>>(uri);
        let (Some(snapshot), Some(candidates)) = (snapshot, candidates) else {
            self.cx.awaiting = true;
            return Err(Error::suspended());
        };
        if candidates.len() != 1 || matches!(status, Some(ParseStatus::Unrecoverable { .. })) {
            return Ok(None);
        }
        let candidate = candidates.first().expect("one parser candidate");
        if candidate.ast_box.resolve(&snapshot).is_err() {
            return Ok(None);
        }
        Ok(Some(Accepted {
            key: candidate.ast_box.key(),
            value: Arc::clone(&candidate.value),
        }))
    }
}

impl<C: Component, Root: LexerRoot + Clone, Ast: Send + Sync + 'static> ContextView<C>
    for crate::component::api::Parsed<Root, Ast>
{
    type Access<'cx, 'tx>
        = ParsedView<'cx, 'tx, C, Root, Ast>
    where
        Self: 'cx,
        'tx: 'cx;

    fn open<'cx, 'tx>(cx: &'cx mut Context<'tx, C>) -> Self::Access<'cx, 'tx> {
        ParsedView::open(cx)
    }
}

// Keep `NodeKey` in the module prelude for entry-key bounds.
#[allow(unused_imports)]
use NodeKey as _;
