//! Public parser vocabulary and snapshot values shared by all parser subsystems.

use std::{any::TypeId, collections::HashMap, fmt, ops::Deref, sync::Arc};

use fluent_uri::Uri;
use ropey::Rope;

use crate::utils::Span;

use crate::component::parse::{
    data::{
        ast::{AstBox, AstId, TokenEntryId},
        green::TreeArena,
        gss::GssArena,
        product::{ProductArena, ProductId},
    },
    grammar::TerminalId,
    identity::TokenFingerprint,
    parsing::ParserSessionState,
};

use super::data::ast::AstArena;

#[derive(Debug, Clone)]
pub struct ParserConfig {
    /// Recovery is part of normal incremental replay and publishes partial
    /// error products plus diagnostics rather than triggering a rebuild.
    pub error_recovery: bool,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            error_recovery: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ParseForest {
    pub roots: Vec<ProductId>,
}

/// Stable identity of a reachable AST value across parser publications.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AstKey {
    pub uri: Uri<&'static str>,
    pub id: AstId,
}

pub type ParseSnapshotId = u64;

/// Metadata and source span for one AST value. The span is mandatory: it was
/// anchored when the value's shift/reduction created its product.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstSnapshotEntry {
    pub product: ProductId,
    pub type_id: TypeId,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AstLookupError {
    WrongDocument,
    Deleted { id: AstId },
    TypeMismatch { id: AstId },
}

impl fmt::Display for AstLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongDocument => write!(f, "AST box belongs to a different document"),
            Self::Deleted { id } => write!(f, "AST node #{id} is not live in this snapshot"),
            Self::TypeMismatch { id } => write!(f, "AST node #{id} has a different runtime type"),
        }
    }
}

/// Snapshot-bound AST access. The contained value dereferences to `T`, while
/// [`ResolvedAst::span`] returns the exact source extent from the same parser
/// revision.
pub struct ResolvedAst<T> {
    value: Arc<T>,
    product: ProductId,
    span: Span,
}

impl<T> Deref for ResolvedAst<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> ResolvedAst<T> {
    pub fn arc(&self) -> Arc<T> {
        Arc::clone(&self.value)
    }

    pub fn product(&self) -> ProductId {
        self.product
    }

    pub fn span(&self) -> Span {
        self.span
    }
}

/// Immutable parser result for one committed document revision. It owns the
/// Rope used by consumers that want line/column formatting, so historical
/// snapshots keep their original coordinate system.
#[derive(Clone)]
pub struct AstSnapshot {
    id: ParseSnapshotId,
    uri: Uri<&'static str>,
    source: Arc<Rope>,
    entries: Arc<HashMap<AstId, AstSnapshotEntry>>,
    values: Arc<HashMap<AstId, Arc<dyn std::any::Any + Send + Sync>>>,
}

impl AstSnapshot {
    pub(crate) fn new(
        id: ParseSnapshotId,
        uri: Uri<&'static str>,
        source: Arc<str>,
        entries: HashMap<AstId, AstSnapshotEntry>,
        values: HashMap<AstId, Arc<dyn std::any::Any + Send + Sync>>,
    ) -> Self {
        Self {
            id,
            uri,
            source: Arc::new(Rope::from_str(&source)),
            entries: Arc::new(entries),
            values: Arc::new(values),
        }
    }

    pub fn id(&self) -> ParseSnapshotId {
        self.id
    }

    pub fn uri(&self) -> Uri<&'static str> {
        self.uri
    }

    pub fn source(&self) -> &Rope {
        &self.source
    }

    pub fn ast_keys(&self) -> impl Iterator<Item = AstKey> + '_ {
        self.entries
            .keys()
            .copied()
            .map(|id| AstKey { uri: self.uri, id })
    }

    pub fn resolve<T>(&self, node: AstBox<T>) -> Result<ResolvedAst<T>, AstLookupError>
    where
        T: Send + Sync + 'static,
    {
        if node.uri != self.uri {
            return Err(AstLookupError::WrongDocument);
        }
        let entry = self
            .entries
            .get(&node.id)
            .ok_or(AstLookupError::Deleted { id: node.id })?;
        if entry.type_id != TypeId::of::<T>() {
            return Err(AstLookupError::TypeMismatch { id: node.id });
        }
        let value = Arc::clone(
            self.values
                .get(&node.id)
                .ok_or(AstLookupError::Deleted { id: node.id })?,
        )
        .downcast::<T>()
        .map_err(|_| AstLookupError::TypeMismatch { id: node.id })?;
        Ok(ResolvedAst {
            value,
            product: entry.product,
            span: entry.span,
        })
    }

    /// Convenience for consumers that only need ownership of the value.
    /// Snapshot-bound `AstBox::resolve` remains the richer API.
    pub fn get<T>(&self, node: AstBox<T>) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.resolve(node).ok().map(|resolved| resolved.arc())
    }
}

impl<T> AstBox<T>
where
    T: Send + Sync + 'static,
{
    pub fn resolve(self, snapshot: &AstSnapshot) -> Result<ResolvedAst<T>, AstLookupError> {
        snapshot.resolve(self)
    }

    pub fn span(self, snapshot: &AstSnapshot) -> Result<Span, AstLookupError> {
        Ok(self.resolve(snapshot)?.span())
    }
}

impl PartialEq for AstSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for AstSnapshot {}

/// Materialized parser state for editor-facing consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseStatus {
    Clean,
    Recovered { diagnostics: usize },
    Unrecoverable { diagnostics: usize },
}

pub type TokenOccurrenceId = usize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncrementalParseStats {
    pub restart_boundary: usize,
    pub reconverged_new_boundary: Option<usize>,
    pub reconverged_old_boundary: Option<usize>,
    pub convergence_checks: usize,
    pub checkpoint_matches: usize,
    pub frontier_matches: usize,
    pub reparsed: usize,
    pub reused: usize,
    pub recovery_columns: usize,
    pub frontier_converged: bool,
}

impl fmt::Display for ParseForest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} parse roots", self.roots.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenData {
    pub id: TokenEntryId,
    pub terminal: Option<TerminalId>,
    pub start: usize,
    pub length: usize,
    /// Stable occurrence identity; it is independent of byte and token positions.
    pub column: TokenOccurrenceId,
    pub fingerprint: TokenFingerprint,
}

#[derive(Clone, Default)]
pub struct ParserSnapshotState {
    pub sessions: HashMap<Uri<&'static str>, Arc<ParserSessionState>>,
    pub roots: HashMap<Uri<&'static str>, Arc<Vec<ProductId>>>,
    pub(crate) tokens: HashMap<Uri<&'static str>, Arc<Vec<TokenData>>>,
    pub(crate) incremental_stats: HashMap<Uri<&'static str>, IncrementalParseStats>,
}

#[derive(Clone)]
pub(crate) struct SessionArenas {
    pub trees: TreeArena,
    pub products: ProductArena,
    pub ast: AstArena,
    pub gss: GssArena,
}
