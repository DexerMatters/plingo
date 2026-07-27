//! Node-graph lexer provider.
//!
//! `LexerNode` has no downstream type.  It observes the source document view
//! and materializes a token-stream view for the same URI.

use std::{marker::PhantomData, sync::Arc};

use fluent_uri::Uri;

use crate::{
    component::{
        lex::{Lexer, LexerConfig, LexerCreationError, LexerRoot},
        parse::TokenData,
        source::{DocumentText, node::DocumentChange},
    },
    scheme::{
        change::AddressChange,
        node::{ComponentState, DeriveCx, Node, NodeError, ReclaimCx, View},
    },
};

/// Stable key for one token occurrence in a document.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TokenKey {
    pub uri: Uri<&'static str>,
    pub occurrence: usize,
}

/// One independently observable token artifact.
pub struct TokenArtifact<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for TokenArtifact<Root> {
    type Key = TokenKey;
    type Value = TokenData;
}

/// The ordered token-occurrence manifest for a document.
pub struct TokenOrder<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for TokenOrder<Root> {
    type Key = Uri<&'static str>;
    type Value = Arc<[TokenKey]>;
}

/// Exact token changes emitted for one source revision. Parser replay consumes
/// this typed revision directly instead of rediscovering a broad token diff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TokenDelta {
    pub changes: Arc<[AddressChange<Uri<&'static str>, TokenData>]>,
}

/// The token delta corresponding to the current [`TokenOrder`] revision.
pub(crate) struct TokenChanges<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for TokenChanges<Root> {
    type Key = Uri<&'static str>;
    type Value = Arc<TokenDelta>;
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
/// individual tokens rather than the document-wide compatibility stream.
pub struct LexerNode<Root: LexerRoot + Clone> {
    lexer: ComponentState<Lexer<Root>>,
}

impl<Root: LexerRoot + Clone> LexerNode<Root> {
    pub fn new() -> Result<Self, LexerCreationError> {
        Ok(Self {
            lexer: ComponentState::new(Lexer::new()?),
        })
    }

    pub fn with_config(config: LexerConfig) -> Result<Self, LexerCreationError> {
        let mut lexer = Lexer::new()?;
        lexer.config = config;
        Ok(Self {
            lexer: ComponentState::new(lexer),
        })
    }
}

impl<Root: LexerRoot + Clone> Node for LexerNode<Root> {
    type Key = Uri<&'static str>;
    type Output = TokenOrder<Root>;

    fn derive(
        &self,
        cx: &mut DeriveCx<'_, '_>,
        uri: Self::Key,
    ) -> Result<Arc<[TokenKey]>, NodeError> {
        let source = cx.observe::<DocumentText>(uri)?;
        let source_change = cx.observe::<DocumentChange>(uri)?;
        let (tokens, changes, diagnostics, stats) = {
            let lexer = cx.state_mut(&self.lexer)?;
            let document = lexer
                .derive_document(uri, source, &source_change)
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
        }
        cx.emit::<TokenChanges<Root>>(
            uri,
            Arc::new(TokenDelta {
                changes: changes.into(),
            }),
        )?;
        cx.emit::<LexStats<Root>>(uri, stats)?;
        cx.emit::<LexDiagnostics<Root>>(uri, diagnostics)?;
        let order: Arc<[TokenKey]> = order.into();
        Ok(order)
    }

    fn reclaim(&self, cx: &mut ReclaimCx<'_, '_>, uri: Self::Key) -> Result<(), NodeError> {
        let has_other_documents = cx.has_materialized::<Self>();
        let lexer = cx.state_mut(&self.lexer)?;
        if has_other_documents {
            lexer.forget_document(uri);
        } else {
            lexer.reset_documents();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use plingo_macros::Terminal;

    use super::*;
    use crate::{
        component::{
            lex::LexErrorInfo,
            source::{SourceEdit, SourceNode},
        },
        scheme::node::{Graph, ViewUpdate},
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
        graph.command(SourceNode::load(uri)).unwrap();

        let subscription = graph.subscribe::<LexerNode<TestTokens>>(uri).unwrap();
        assert!(matches!(
            subscription.recv().unwrap(),
            ViewUpdate::Initial { .. }
        ));

        graph
            .command(SourceNode::apply(SourceEdit::Insert {
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
                .read::<TokenArtifact<TestTokens>>(value[0].clone())
                .expect("the ordered token must be materialized")
                .length,
            5
        );
    }
}
