//! Public parser vocabulary and snapshot values shared by all parser subsystems.

use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use fluent_uri::Uri;

use crate::{
    component::parse::{
        data::{
            ast::{AstBox, AstId, TokenEntryId},
            green::TreeArena,
            gss::GssArena,
            product::{ProductArena, ProductId},
        },
        grammar::TerminalId,
        identity::TokenFingerprint,
        parsing::ParserSessionState,
    },
    scheme::change::{AddressChange, ChangeSet, FlowUnit},
    utils::RangeOrPoint,
};

use super::data::ast::AstArena;

#[derive(Debug, Clone)]
pub struct ParserConfig {
    pub error_recovery: bool,
    pub error_recovery_timeout: Duration,
    /// Maximum planned replay tokens; `None` always reparses toward convergence.
    pub incremental_reparse_limit: Option<usize>,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            error_recovery: true,
            error_recovery_timeout: Duration::from_millis(100),
            incremental_reparse_limit: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParsePath {
    pub uri: Uri<&'static str>,
    pub path: Vec<usize>,
    pub range: RangeOrPoint<usize>,
}

impl fmt::Display for ParsePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.uri)?;
        for child in &self.path {
            write!(f, "/{child}")?;
        }
        write!(f, "@{}", self.range)
    }
}

#[derive(Clone, Debug)]
pub struct ParseForest {
    pub roots: Vec<ProductId>,
}

/// One typed AST value in an [`AstView`].
///
/// The value is shared with the parser's AST arena, so constructing a view does
/// not require `T: Clone` and the value remains valid after later revisions are
/// parsed.
pub struct AstViewEntry<T> {
    pub ast_box: AstBox<T>,
    pub product: ProductId,
    pub value: Arc<T>,
}

impl<T> Clone for AstViewEntry<T> {
    fn clone(&self) -> Self {
        Self {
            ast_box: self.ast_box,
            product: self.product,
            value: Arc::clone(&self.value),
        }
    }
}

impl<T> AstViewEntry<T> {
    pub fn owner(&self) -> ProductId {
        self.product
    }
}

/// An immutable, typed view of the AST reachable at one parser revision.
///
/// Views contain only nodes reachable from the selected snapshot's roots for
/// `uri`. Arena values allocated by older or newer revisions are deliberately
/// excluded.
pub struct AstView<T> {
    uri: Uri<&'static str>,
    roots: Arc<[AstBox<T>]>,
    entries: Arc<[AstViewEntry<T>]>,
    by_ast: Arc<HashMap<AstId, usize>>,
    by_product: Arc<HashMap<ProductId, usize>>,
}

impl<T> Clone for AstView<T> {
    fn clone(&self) -> Self {
        Self {
            uri: self.uri,
            roots: Arc::clone(&self.roots),
            entries: Arc::clone(&self.entries),
            by_ast: Arc::clone(&self.by_ast),
            by_product: Arc::clone(&self.by_product),
        }
    }
}

impl<T> AstView<T> {
    pub(crate) fn new(
        uri: Uri<&'static str>,
        roots: Vec<AstBox<T>>,
        entries: Vec<AstViewEntry<T>>,
    ) -> Self {
        let by_ast = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.ast_box.id, index))
            .collect();
        let by_product = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.product, index))
            .collect();
        Self {
            uri,
            roots: roots.into(),
            entries: entries.into(),
            by_ast: Arc::new(by_ast),
            by_product: Arc::new(by_product),
        }
    }

    pub(crate) fn empty(uri: Uri<&'static str>) -> Self {
        Self::new(uri, Vec::new(), Vec::new())
    }

    pub fn uri(&self) -> Uri<&'static str> {
        self.uri
    }

    pub fn roots(&self) -> &[AstBox<T>] {
        &self.roots
    }

    pub fn entries(&self) -> &[AstViewEntry<T>] {
        &self.entries
    }

    pub fn get(&self, ast_box: AstBox<T>) -> Option<&T> {
        self.entry(ast_box).map(|entry| entry.value.as_ref())
    }

    pub fn entry(&self, ast_box: AstBox<T>) -> Option<&AstViewEntry<T>> {
        if ast_box.uri != self.uri {
            return None;
        }
        self.by_ast
            .get(&ast_box.id)
            .and_then(|&index| self.entries.get(index))
    }

    pub fn owner(&self, ast_box: AstBox<T>) -> Option<ProductId> {
        self.entry(ast_box).map(|entry| entry.product)
    }

    pub fn box_for_product(&self, product: ProductId) -> Option<AstBox<T>> {
        self.by_product
            .get(&product)
            .and_then(|&index| self.entries.get(index))
            .map(|entry| entry.ast_box)
    }

    pub fn contains_product(&self, product: ProductId) -> bool {
        self.by_product.contains_key(&product)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParseAddress {
    pub uri: Uri<&'static str>,
    pub parent_path: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseUnit {
    pub product: ProductId,
}

impl FlowUnit for ParseUnit {
    fn extent(&self) -> usize {
        1
    }
}

pub type ParseChange = AddressChange<ParseAddress, ParseUnit>;
pub type ParseChanges = ChangeSet<ParseAddress, ParseUnit>;

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

impl FlowUnit for TokenData {
    fn extent(&self) -> usize {
        1
    }
}

#[derive(Clone, Default)]
pub struct ParserSnapshotState {
    pub sessions: HashMap<Uri<&'static str>, Arc<ParserSessionState>>,
    pub roots: HashMap<Uri<&'static str>, Arc<Vec<ProductId>>>,
    pub(crate) tokens: HashMap<Uri<&'static str>, Arc<Vec<TokenData>>>,
    pub(crate) incremental_stats: HashMap<Uri<&'static str>, IncrementalParseStats>,
}

pub(crate) struct SessionArenas {
    pub trees: TreeArena,
    pub products: ProductArena,
    pub ast: AstArena,
    pub gss: GssArena,
}
