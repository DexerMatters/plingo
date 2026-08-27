//! Deterministic engine errors. Every error path aborts the epoch with a
//! full rollback (T6): no partial state, edge, counter, or subscription
//! escapes.

use std::fmt;
use std::sync::Arc;

/// The cloneable author-facing error type.
#[derive(Clone, Debug)]
pub enum Error {
    EffectOutsideRun {
        effect: String,
        view: String,
    },
    InvalidCommandEffect {
        effect: String,
    },
    PlanForDifferentEngine,
    PlanAlreadyRun,
    ConflictingWrites {
        view: String,
        input: String,
        functions: Vec<String>,
    },
    DependencyCycle {
        views: Vec<String>,
    },
    ComputationCycle {
        functions: Vec<String>,
    },
    TopologyViolation {
        view: String,
        message: String,
    },
    MixedEmissionMode {
        view: String,
    },
    StaleSourceRevision {
        uri: String,
    },
    DuplicateComponent {
        descriptor: String,
    },
    DuplicatePatchKey {
        view: String,
    },
    Authored(Arc<dyn std::error::Error + Send + Sync>),
    Panic(Arc<str>),
    Internal(Arc<str>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::EffectOutsideRun { effect, view } => {
                write!(f, "{effect} used outside a reactive run for view `{view}`")
            }
            Error::InvalidCommandEffect { effect } => {
                write!(f, "{effect} is invalid in an external command")
            }
            Error::PlanForDifferentEngine => write!(f, "plan belongs to a different engine"),
            Error::PlanAlreadyRun => write!(f, "plan has already been run"),
            Error::ConflictingWrites {
                view,
                input,
                functions,
            } => write!(
                f,
                "conflicting writes on `{view}` at {input} from {}",
                functions.join(", ")
            ),
            Error::DependencyCycle { views } => {
                write!(f, "view dependency cycle: {}", views.join(" -> "))
            }
            Error::ComputationCycle { functions } => {
                write!(f, "computation cycle: {}", functions.join(" -> "))
            }
            Error::TopologyViolation { view, message } => {
                write!(f, "topology violation on `{view}`: {message}")
            }
            Error::MixedEmissionMode { view } => {
                write!(f, "mixed replace and patch emission on `{view}`")
            }
            Error::StaleSourceRevision { uri } => {
                write!(f, "stale source revision for `{uri}`")
            }
            Error::DuplicateComponent { descriptor } => {
                write!(f, "component `{descriptor}` is already installed")
            }
            Error::DuplicatePatchKey { view } => {
                write!(f, "duplicate patch key on `{view}`")
            }
            Error::Authored(error) => write!(f, "authored error: {error}"),
            Error::Panic(message) => write!(f, "authored panic: {message}"),
            Error::Internal(message) => write!(f, "internal engine error: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Wraps an authored error without losing cloneability.
    pub fn authored<E: std::error::Error + Send + Sync + 'static>(error: E) -> Self {
        Error::Authored(Arc::new(error))
    }

    /// Builds the T5 ownership-conflict error for one fact identity.
    pub(crate) fn conflicting_write(
        view: &str,
        key: &dyn crate::reactive::value::KeyValue,
        writer: &str,
        existing_owners: &[(u64, String)],
    ) -> Self {
        Error::ConflictingWrites {
            view: view.to_string(),
            input: format!("{key:?}#{}", key.hash_value()),
            functions: std::iter::once(writer.to_string())
                .chain(existing_owners.iter().map(|(_, name)| name.clone()))
                .collect(),
        }
    }
}

/// The engine's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
