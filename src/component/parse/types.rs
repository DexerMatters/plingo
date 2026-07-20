//! Public parser vocabulary and snapshot values shared by all parser subsystems.

use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use fluent_uri::Uri;

use crate::{
    component::parse::{
        data::{
            ast::TokenEntryId,
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
