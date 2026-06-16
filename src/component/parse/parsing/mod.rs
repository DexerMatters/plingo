use std::{collections::HashMap, fmt, time::Duration};

use indexmap::IndexSet;

use crate::component::parse::{TokenOccurrenceId, recovery};
use crate::component::parse::{
    build::ActionSet,
    checkpoint::ColumnCheckpointCache,
    data::{
        ast::{AstArena, TokenEntryId},
        green::{ParseErrorInfo, TreeArena},
        gss::{GssArena, GssNodeId},
        product::{ProductArena, ProductId},
    },
    grammar::{BuildError, Grammar, TerminalId},
    identity::TokenFingerprint,
};

mod incremental;
mod session;
mod state;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ParseToken {
    pub(crate) entry: TokenEntryId,
    pub(crate) column: TokenOccurrenceId,
    pub(crate) start: usize,
    pub(crate) terminal: TerminalId,
    pub(crate) length: usize,
    pub(crate) fingerprint: TokenFingerprint,
    pub(crate) merge_source_terminal: Option<TerminalId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReductionPath {
    predecessor: GssNodeId,
    products: Vec<ProductId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReductionKey {
    production: u32,
    children: Vec<ProductId>,
}

#[derive(Debug)]
pub enum ParseError {
    MissingGoto { state: usize, non_terminal: u32 },
    NoActiveStacks { column: Option<TokenOccurrenceId> },
    MissingGssNode { node: GssNodeId },
    Build(BuildError),
    RecoveryTimeout { elapsed: Duration },
    Recovered { product: ProductId },
}

impl From<BuildError> for ParseError {
    fn from(value: BuildError) -> Self {
        Self::Build(value)
    }
}

impl From<recovery::RecoveryError> for ParseError {
    fn from(value: recovery::RecoveryError) -> Self {
        match value {
            recovery::RecoveryError::Timeout { elapsed } => Self::RecoveryTimeout { elapsed },
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingGoto {
                state,
                non_terminal,
            } => {
                write!(
                    f,
                    "missing goto from state {state} on nonterminal {non_terminal}"
                )
            }
            Self::NoActiveStacks { column } => match column {
                Some(column) => write!(f, "no active parse stacks at token column {column}"),
                None => write!(f, "no active parse stacks"),
            },
            Self::MissingGssNode { node } => write!(f, "missing GSS node {node}"),
            Self::Build(error) => write!(f, "build error: {error:?}"),
            Self::RecoveryTimeout { elapsed } => {
                write!(f, "recovery search timed out after {elapsed:?}")
            }
            Self::Recovered { .. } => write!(f, "parse recovered with errors"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ParseColumn {
    token: Option<TokenOccurrenceId>,
    base_active: IndexSet<GssNodeId>,
    active: IndexSet<GssNodeId>,
    accepted: Vec<ProductId>,
    pub(crate) products: Vec<ProductId>,
    pub(crate) diagnostics: Vec<ParseErrorInfo>,
    pub(crate) error_derived: bool,
    checkpoint_cache: ColumnCheckpointCache,
}

#[derive(Clone, Default)]
pub struct ParserSessionState {
    pub(crate) columns: Vec<ParseColumn>,
    pub(crate) generation: u32,
    pub(crate) diagnostics: Vec<ParseErrorInfo>,
    token_columns: HashMap<TokenOccurrenceId, usize>,
    token_products: HashMap<TokenOccurrenceId, ProductId>,
    reduced_products: HashMap<ReductionKey, ProductId>,
}

pub(crate) struct SessionContext<'a> {
    pub state: &'a mut ParserSessionState,
    pub trees: &'a mut TreeArena,
    pub products: &'a mut ProductArena,
    pub ast: &'a mut AstArena,
    pub gss: &'a mut GssArena,
    pub(crate) grammar: &'a Grammar,
    pub(crate) actions: &'a [ActionSet],
    pub(crate) gotos: &'a [Option<usize>],
    pub(crate) error_recovery: bool,
    pub(crate) error_recovery_timeout: Duration,
}
