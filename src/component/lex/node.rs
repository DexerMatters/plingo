//! Node-graph lexer provider.
//!
//! `LexerNode` has no downstream type.  It observes the source document view
//! and materializes a token-stream view for the same URI.

use std::{marker::PhantomData, sync::Arc};

use fluent_uri::Uri;

use crate::{
    component::{
        lex::{Lexer, LexerCreationError, LexerRoot},
        parse::TokenData,
        source::{DocumentText, node::DocumentChange},
    },
    scheme::{
        change::AddressChange,
        node::{DeriveCx, NodeError, NodeProvider, ProviderState, ReadGraph, ReclaimCx, View},
    },
};

/// Stable key for one token occurrence in a document.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TokenKey {
    pub uri: Uri<&'static str>,
    pub occurrence: usize,
}

/// Stable parser-facing identity for one token entry. Unlike occurrence
/// coordinates, this key lets typed AST token fields retrieve their semantic
/// lexeme without observing span-only movement.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TokenEntryKey {
    pub uri: Uri<&'static str>,
    pub id: usize,
}

/// One independently observable token artifact.
pub struct TokenArtifact<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for TokenArtifact<Root> {
    type Key = TokenKey;
    type Value = TokenData;
}

/// Semantic source text for one stable token entry. Its value remains equal
/// when edits merely shift the token's byte coordinates.
pub struct TokenLexeme<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for TokenLexeme<Root> {
    type Key = TokenEntryKey;
    type Value = Arc<str>;
}

/// The ordered token-occurrence manifest for a document.
pub struct TokenOrder<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for TokenOrder<Root> {
    type Key = Uri<&'static str>;
    type Value = Arc<[TokenKey]>;
}

/// One atomic lexer publication. It keeps exact parser replay splices and the
/// final coordinate/source state together, including edits that only shift
/// skipped whitespace and therefore need no grammar replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TokenRevisionData {
    pub changes: Arc<[AddressChange<Uri<&'static str>, TokenData>]>,
    pub tokens: Arc<[TokenData]>,
    pub source: Arc<str>,
}

pub(crate) struct TokenRevision<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for TokenRevision<Root> {
    type Key = Uri<&'static str>;
    type Value = Arc<TokenRevisionData>;
}

/// Incremental lexer work performed for the current source revision.
pub struct LexStats<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for LexStats<Root> {
    type Key = Uri<&'static str>;
    type Value = crate::component::lex::IncrementalLexStats;
}

/// Lexer diagnostics materialized alongside the token stream.
pub struct LexDiagnostics<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for LexDiagnostics<Root> {
    type Key = Uri<&'static str>;
    type Value = Arc<[crate::component::lex::LexErrorInfo]>;
}

/// Incremental token provider for one language root.
///
/// Its mutable lexer cache is transactionally staged.  It additionally emits
/// stable occurrence-keyed token artifacts, allowing consumers to depend on
/// individual tokens rather than the document-wide token stream.
pub struct LexerNode<Root: LexerRoot + Clone> {
    lexer: ProviderState<Lexer<Root>>,
}

impl<Root: LexerRoot + Clone> LexerNode<Root> {
    pub fn new() -> Result<Self, LexerCreationError> {
        Ok(Self {
            lexer: ProviderState::new(Lexer::new()?),
        })
    }
}

impl<Root: LexerRoot + Clone> NodeProvider for LexerNode<Root> {
    type Key = Uri<&'static str>;

    fn schema() -> crate::scheme::node::NodeSchema {
        use crate::scheme::node::PortDeclaration;
        crate::scheme::node::NodeSchema::new(
            std::any::type_name::<Self>(),
            vec![
                PortDeclaration::map::<TokenOrder<Root>>(),
                PortDeclaration::map::<TokenArtifact<Root>>(),
                PortDeclaration::map::<TokenLexeme<Root>>(),
                PortDeclaration::map::<TokenRevision<Root>>(),
                PortDeclaration::map::<LexStats<Root>>(),
                PortDeclaration::map::<LexDiagnostics<Root>>(),
            ],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_>, uri: Self::Key) -> Result<(), NodeError> {
        let source = cx
            .get::<DocumentText>(uri)
            .ok_or_else(NodeError::missing_view::<DocumentText>)?;
        let source_change = cx
            .get::<DocumentChange>(uri)
            .ok_or_else(NodeError::missing_view::<DocumentChange>)?;
        let (tokens, changes, diagnostics, stats) = {
            let lexer = cx.state_mut(&self.lexer)?;
            let document = lexer
                .derive_document(uri, Arc::clone(&source), &source_change)
                .map_err(|error| NodeError::message(error.to_string()))?;
            let diagnostics = document
                .tokens
                .iter()
                .filter_map(|token| lexer.token(token.id).and_then(|token| token.error))
                .collect::<Vec<_>>()
                .into();
            (
                document.tokens,
                document.changes,
                diagnostics,
                lexer.incremental_stats(uri).unwrap_or_default(),
            )
        };
        let order = tokens
            .iter()
            .map(|token| TokenKey {
                uri,
                occurrence: token.column,
            })
            .collect::<Vec<_>>();
        for token in &tokens {
            cx.emit::<TokenArtifact<Root>>(
                TokenKey {
                    uri,
                    occurrence: token.column,
                },
                *token,
            )?;
            let end = token.start.saturating_add(token.length);
            let lexeme: Arc<str> = source.get(token.start..end).unwrap_or_default().into();
            cx.emit::<TokenLexeme<Root>>(TokenEntryKey { uri, id: token.id }, lexeme)?;
        }
        let changes: Arc<[AddressChange<Uri<&'static str>, TokenData>]> = changes.into();
        cx.emit::<TokenRevision<Root>>(
            uri,
            Arc::new(TokenRevisionData {
                changes,
                tokens: tokens.clone().into(),
                source: Arc::clone(&source),
            }),
        )?;
        cx.emit::<LexStats<Root>>(uri, stats)?;
        cx.emit::<LexDiagnostics<Root>>(uri, diagnostics)?;
        let order: Arc<[TokenKey]> = order.into();
        cx.emit::<TokenOrder<Root>>(uri, order)?;
        Ok(())
    }

    fn reclaim(&self, cx: &mut ReclaimCx<'_>, uri: Self::Key) -> Result<(), NodeError> {
        let has_other_documents = cx.has_materialized::<Self>();
        let lexer = cx.state_mut(&self.lexer)?;
        if has_other_documents {
            lexer.forget_document(uri);
        } else {
            lexer.reset_documents();
        }
        Ok(())
    }

    fn uses_state() -> bool {
        true
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/component_lex_node.rs"]
mod tests;
