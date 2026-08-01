//! Parser provider for the node graph runtime.
//!
//! One parser derivation publishes an immutable [`AstSnapshot`] as the
//! canonical parser publication. Typed AST and location views are read-only
//! keyed projections of that snapshot.

use std::{
    any::{Any, TypeId},
    marker::PhantomData,
    sync::Arc,
};

use fluent_uri::Uri;

use crate::{
    component::{
        lex::{LexerNode, LexerRoot, node::TokenRevision},
        parse::{
            AstKey, AstSnapshot, ParseErrorInfo, ParseStatus, Parser,
            data::{ast::AstBox, product::ProductId},
        },
    },
    scheme::node::{ComponentState, DeriveCx, NodeError, NodeProvider, ReadGraph, ReclaimCx, View},
    utils::Span,
};

/// One independently observable semantic AST artifact. It is erased only at
/// the graph boundary; [`AstArtifact::deref`] restores its concrete type.
/// Locations are a separate view, so span-only edits do not invalidate this
/// semantic fact.
pub struct ParsedAst<Root>(PhantomData<fn() -> Root>);

pub struct AstArtifact {
    pub key: AstKey,
    pub product: ProductId,
    type_id: TypeId,
    value: Arc<dyn Any + Send + Sync>,
}

impl AstArtifact {
    /// Restores the concrete immutable AST value when this artifact has type
    /// `Ast`. A mismatch means the requested `AstBox` is stale or mistyped.
    pub fn deref<Ast>(&self) -> Option<Arc<Ast>>
    where
        Ast: Send + Sync + 'static,
    {
        (self.type_id == TypeId::of::<Ast>())
            .then(|| Arc::clone(&self.value).downcast::<Ast>().ok())
            .flatten()
    }
}

impl Clone for AstArtifact {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            product: self.product,
            type_id: self.type_id,
            value: Arc::clone(&self.value),
        }
    }
}

impl PartialEq for AstArtifact {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.product == other.product && self.type_id == other.type_id
    }
}

impl Eq for AstArtifact {}

/// One complete interpretation accepted by the generalized parser.
///
/// Candidates share the document's immutable [`AstSnapshot`], but retain
/// distinct root products and AST identities. Downstream components can
/// therefore choose an interpretation without changing parser state or the
/// choice made by any other component.
pub struct ParseCandidate<Ast> {
    pub ast_box: AstBox<Ast>,
    pub product: ProductId,
    pub value: Arc<Ast>,
}

impl<Ast> Clone for ParseCandidate<Ast> {
    fn clone(&self) -> Self {
        Self {
            ast_box: self.ast_box,
            product: self.product,
            value: Arc::clone(&self.value),
        }
    }
}

impl<Ast> PartialEq for ParseCandidate<Ast> {
    fn eq(&self, other: &Self) -> bool {
        self.ast_box.id == other.ast_box.id
            && self.ast_box.uri == other.ast_box.uri
            && self.product == other.product
    }
}

impl<Ast> Eq for ParseCandidate<Ast> {}

impl<Root: LexerRoot> View for ParsedAst<Root> {
    type Key = AstKey;
    type Value = AstArtifact;
}

/// Complete accepted interpretations for one document.
///
/// Unlike [`ParseRoots`], which remains the manifest of every reachable typed
/// AST artifact, this view contains only accepted parser roots and preserves
/// their product identity.
pub struct ParseCandidates<Root, Ast>(PhantomData<fn() -> (Root, Ast)>);

impl<Root, Ast> View for ParseCandidates<Root, Ast>
where
    Root: LexerRoot,
    Ast: Send + Sync + 'static,
{
    type Key = Uri<&'static str>;
    type Value = Arc<[ParseCandidate<Ast>]>;
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
pub struct AstLocation<Root>(PhantomData<fn() -> Root>);

impl<Root: LexerRoot> View for AstLocation<Root> {
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

impl<Root, Ast> NodeProvider for ParserNode<Root, Ast>
where
    Root: LexerRoot + Clone,
    Ast: Send + Sync + 'static,
{
    type Key = Uri<&'static str>;

    fn schema() -> crate::scheme::node::NodeSchema {
        use crate::scheme::node::PortDeclaration;
        crate::scheme::node::NodeSchema::new(
            std::any::type_name::<Self>(),
            vec![
                PortDeclaration::map::<ParseSnapshot<Root>>(),
                PortDeclaration::map::<ParsedAst<Root>>(),
                PortDeclaration::map::<AstLocation<Root>>(),
                PortDeclaration::map::<ParseCandidates<Root, Ast>>(),
                PortDeclaration::map::<ParseRoots<Root, Ast>>(),
                PortDeclaration::map::<ParseStats<Root>>(),
                PortDeclaration::map::<ParseStatusView<Root>>(),
                PortDeclaration::map::<ParseDiagnostics<Root>>(),
            ],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, uri: Self::Key) -> Result<(), NodeError> {
        cx.materialize::<LexerNode<Root>>(uri)?;
        // The lexer publishes exact replay splices and final source/token
        // coordinates as one fact. This prevents a parser task from seeing a
        // new source together with a prior lexer delta in the same graph
        // transaction.
        let token_revision = cx
            .get::<TokenRevision<Root>>(uri)
            .ok_or_else(NodeError::missing_view::<TokenRevision<Root>>)?;

        let (snapshot, accepted, diagnostics, stats) = {
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
            let accepted = parser
                .latest
                .roots
                .get(&uri)
                .map(|roots| roots.as_ref().clone())
                .unwrap_or_default();
            let diagnostics: Arc<[ParseErrorInfo]> = parser.latest_parse_diagnostics(uri).into();
            (
                snapshot,
                accepted,
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
            .erased_entries()
            .map(|(key, entry, value)| {
                (
                    key.clone(),
                    AstArtifact {
                        key,
                        product: entry.product,
                        type_id: entry.type_id,
                        value,
                    },
                    entry.span,
                )
            })
            .collect::<Vec<_>>();
        let typed_artifacts = snapshot
            .ast_keys()
            .filter_map(|key| {
                let ast_box = AstBox::<Ast>::new(key.id, key.uri);
                ast_box
                    .resolve(&snapshot)
                    .ok()
                    .map(|resolved| (key, ast_box, resolved.product(), resolved.arc()))
            })
            .collect::<Vec<_>>();
        let candidates: Arc<[ParseCandidate<Ast>]> = accepted
            .iter()
            .filter_map(|product| {
                typed_artifacts
                    .iter()
                    .find(|(_, _, artifact_product, _)| artifact_product == product)
                    .map(|(_, ast_box, product, value)| ParseCandidate {
                        ast_box: *ast_box,
                        product: *product,
                        value: Arc::clone(value),
                    })
            })
            .collect::<Vec<_>>()
            .into();
        let roots: Arc<[AstKey]> = typed_artifacts
            .iter()
            .map(|(key, _, _, _)| key.clone())
            .collect::<Vec<_>>()
            .into();

        let candidates_changed = cx
            .peek::<ParseCandidates<Root, Ast>>(uri)
            .is_none_or(|previous| previous.as_ref() != candidates.as_ref());

        for (key, artifact, span) in artifacts {
            cx.emit::<ParsedAst<Root>>(key.clone(), artifact)?;
            cx.emit::<AstLocation<Root>>(key, span)?;
        }
        cx.emit::<ParseCandidates<Root, Ast>>(uri, Arc::clone(&candidates))?;
        cx.emit::<ParseRoots<Root, Ast>>(uri, Arc::clone(&roots))?;
        cx.emit::<ParseStats<Root>>(uri, stats)?;
        cx.emit::<ParseStatusView<Root>>(uri, status)?;
        cx.emit::<ParseDiagnostics<Root>>(uri, diagnostics)?;
        if candidates_changed {
            cx.defer_connected::<Self>(uri);
        }
        cx.emit::<ParseSnapshot<Root>>(uri, snapshot)?;
        Ok(())
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
#[path = "../../../tests/unit/component_parse_node.rs"]
mod tests;
