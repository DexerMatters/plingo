//! Parser provider for the node graph runtime.
//!
//! One parser derivation publishes an immutable [`AstSnapshot`]. Typed AST
//! and location views are independent keyed projections of that snapshot.

use std::{marker::PhantomData, sync::Arc, time::Instant};

use fluent_uri::Uri;

use crate::{
    component::{
        lex::{node::TokenRevision, LexerNode, LexerRoot},
        parse::{
            data::{ast::AstBox, product::ProductId},
            AstKey, AstSnapshot, ParseErrorInfo, ParseStatus, Parser,
        },
    },
    scheme::node::{ComponentState, DeriveCx, Node, NodeError, ReclaimCx, View},
    utils::Span,
};

/// One independently observable semantic AST artifact. Locations are exposed
/// separately so a span-only edit does not invalidate semantic consumers.
pub struct ParsedAst<Root, Ast>(PhantomData<fn() -> (Root, Ast)>);

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

/// Typed AST keys reachable from one document snapshot. It is the manifest
/// used by document-level reconcilers; each key has its own semantic/location
/// view.
pub struct ParseRoots<Root, Ast>(PhantomData<fn() -> (Root, Ast)>);

impl<Root, Ast> View for ParseRoots<Root, Ast>
where
    Root: LexerRoot,
    Ast: Send + Sync + 'static,
{
    type Key = Uri<&'static str>;
    type Value = Arc<[AstKey]>;
}

/// Immutable AST dereference boundary for one document publication.
pub struct ParseSnapshot<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for ParseSnapshot<Root> {
    type Key = Uri<&'static str>;
    type Value = Arc<AstSnapshot>;
}

/// Source location for one typed semantic AST artifact. Consumers that only
/// need semantics should not observe this view.
pub struct AstLocation<Root, Ast>(PhantomData<fn() -> (Root, Ast)>);

impl<Root, Ast> View for AstLocation<Root, Ast>
where
    Root: LexerRoot,
    Ast: Send + Sync + 'static,
{
    type Key = AstKey;
    type Value = Span;
}

pub struct ParseDiagnostics<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for ParseDiagnostics<Root> {
    type Key = Uri<&'static str>;
    type Value = Arc<[ParseErrorInfo]>;
}

pub struct ParseStatusView<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for ParseStatusView<Root> {
    type Key = Uri<&'static str>;
    type Value = ParseStatus;
}

pub struct ParseStats<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for ParseStats<Root> {
    type Key = Uri<&'static str>;
    type Value = crate::component::parse::IncrementalParseStats;
}

/// Incremental parser node. Mutable replay state remains local; graph-owned
/// keyed facts provide semantic and location-level invalidation.
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
    type Output = ParseSnapshot<Root>;

    fn derive(
        &self,
        cx: &mut DeriveCx<'_, '_>,
        uri: Self::Key,
    ) -> Result<Arc<AstSnapshot>, NodeError> {
        let total_start = Instant::now();
        let _ = cx.require::<LexerNode<Root>>(uri)?;
        // The lexer publishes exact replay splices and final source/token
        // coordinates as one fact. This prevents a parser task from seeing a
        // new source together with a prior lexer delta in the same graph
        // transaction.
        let token_revision = cx.observe::<TokenRevision<Root>>(uri)?;

        let (snapshot, diagnostics, stats) = {
            let parser = cx.state_mut(&self.parser)?;
            let snapshot = parser
                .derive_changes(uri, &token_revision.changes)
                .and_then(|_| {
                    parser.commit_snapshot(
                        uri,
                        Arc::clone(&token_revision.source),
                        &token_revision.tokens,
                    )
                })
                .map_err(|error| NodeError::message(error.to_string()))?;
            let diagnostics: Arc<[ParseErrorInfo]> = parser.latest_parse_diagnostics(uri).into();
            (
                snapshot,
                diagnostics,
                parser.incremental_stats(uri).unwrap_or_default(),
            )
        };
        let status = if diagnostics.is_empty() {
            ParseStatus::Clean
        } else if snapshot.ast_keys().next().is_none() {
            ParseStatus::Unrecoverable {
                diagnostics: diagnostics.len(),
            }
        } else {
            ParseStatus::Recovered {
                diagnostics: diagnostics.len(),
            }
        };

        let artifacts = snapshot
            .ast_keys()
            .filter_map(|key| {
                let ast_box = AstBox::<Ast>::new(key.id, key.uri);
                ast_box.resolve(&snapshot).ok().map(|resolved| {
                    (
                        key,
                        AstArtifact {
                            ast_box,
                            product: resolved.product(),
                            value: resolved.arc(),
                        },
                        resolved.span(),
                    )
                })
            })
            .collect::<Vec<_>>();
        let roots: Arc<[AstKey]> = artifacts
            .iter()
            .map(|(key, _, _)| key.clone())
            .collect::<Vec<_>>()
            .into();

        for (key, artifact, span) in artifacts {
            cx.emit::<ParsedAst<Root, Ast>>(key.clone(), artifact)?;
            cx.emit::<AstLocation<Root, Ast>>(key, span)?;
        }
        cx.emit::<ParseRoots<Root, Ast>>(uri, Arc::clone(&roots))?;
        cx.emit::<ParseStats<Root>>(uri, stats)?;
        cx.emit::<ParseStatusView<Root>>(uri, status)?;
        cx.emit::<ParseDiagnostics<Root>>(uri, diagnostics)?;
        eprintln!(
            "[parse-node] uri={uri} total={:?} token_changes={} roots={}",
            total_start.elapsed(),
            token_revision.changes.len(),
            roots.len(),
        );
        Ok(snapshot)
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
            parse::{grammar::Grammar, AstToken},
            source::{SourceEdit, SourceNode},
        },
        scheme::node::{Graph, ViewUpdate},
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

    #[test]
    fn parser_revision_resolves_ast_boxes_and_tracks_span_only_updates() {
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

        let snapshot = graph.request::<ParserNode<Tokens, Value>>(uri).unwrap();
        let roots = graph.read::<ParseRoots<Tokens, Value>>(uri).unwrap();
        let root = roots[0].clone();
        let artifact = graph
            .read::<ParsedAst<Tokens, Value>>(root.clone())
            .expect("the root AST artifact must be materialized");
        let semantic = graph
            .subscribe_view::<ParsedAst<Tokens, Value>>(root.clone())
            .unwrap();
        let location = graph
            .subscribe_view::<AstLocation<Tokens, Value>>(root.clone())
            .unwrap();
        let _ = semantic.recv().unwrap();
        let _ = location.recv().unwrap();

        let resolved = artifact.ast_box.resolve(&snapshot).unwrap();
        assert!(matches!(&*resolved, Value::Number(_)));
        assert_eq!(resolved.span(), Span::new_uri(uri, 0, 2).unwrap());
        assert_eq!(
            resolved.span().to_line_col(snapshot.source()),
            RangeOrPoint::Range((0, 0), (0, 2))
        );

        graph
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 0).unwrap(),
                value: "\n".into(),
            }))
            .unwrap();
        let shifted = graph.read::<ParseSnapshot<Tokens>>(uri).unwrap();
        assert_eq!(
            artifact
                .ast_box
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
            .command(SourceNode::apply(SourceEdit::Insert {
                key: Span::point_uri(uri, 1).unwrap(),
                value: "\u{2003}".into(),
            }))
            .unwrap();
        let unicode_shifted = graph.read::<ParseSnapshot<Tokens>>(uri).unwrap();
        assert_eq!(
            artifact.ast_box.span(&unicode_shifted).unwrap(),
            Span::new_uri(uri, 4, 6).unwrap(),
        );
        assert_eq!(
            artifact
                .ast_box
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
            artifact.ast_box.span(&snapshot).unwrap(),
            Span::new_uri(uri, 0, 2).unwrap(),
            "held historical snapshots retain their original source coordinates"
        );

        graph
            .command(SourceNode::apply_all(vec![
                SourceEdit::Delete {
                    key: Span::new_uri(uri, 4, 6).unwrap(),
                },
                SourceEdit::Insert {
                    key: Span::point_uri(uri, 4).unwrap(),
                    value: "7".into(),
                },
            ]))
            .unwrap();
        let replaced = graph.read::<ParseSnapshot<Tokens>>(uri).unwrap();
        assert!(matches!(
            semantic.recv().unwrap(),
            ViewUpdate::Removed { .. }
        ));
        assert!(matches!(
            location.recv().unwrap(),
            ViewUpdate::Removed { .. }
        ));
        assert!(matches!(
            artifact.ast_box.resolve(&replaced),
            Err(crate::component::parse::AstLookupError::Deleted { .. })
        ));
    }
}
