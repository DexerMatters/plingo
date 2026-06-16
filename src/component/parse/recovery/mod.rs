use std::time::Duration;

use crate::component::parse::{
    grammar::TerminalId,
    parsing::{ParseToken, SessionContext},
};

mod search;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Repair {
    Insert(TerminalId),
    Delete,
    Shift,
    ShiftAsError,
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveryResult {
    pub(crate) repairs: Vec<Repair>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryError {
    Timeout { elapsed: Duration },
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout { elapsed } => write!(
                f,
                "recovery search timed out after {:?} (no complete repair found)",
                elapsed
            ),
        }
    }
}

pub(crate) fn find_recovery(
    ctx: &SessionContext<'_>,
    column: usize,
    tokens: &[ParseToken],
    timeout: Duration,
) -> Result<Option<RecoveryResult>, RecoveryError> {
    search::find_recovery(ctx, column, tokens, timeout)
}
