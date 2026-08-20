//! Deterministic engine errors. Every error path aborts the epoch with a
//! full rollback (T6): no partial state, edge, counter, or subscription
//! escapes.

use std::fmt;

/// A producer identity: a component (by registration ordinal) or the
/// external command channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Producer {
    Component(u32),
    External,
    /// Structural facts (keys, roots, children, buckets): the shared
    /// union structure of a view, writable by every producer.
    Structural,
}

impl Producer {
    pub(crate) fn label(&self) -> String {
        match self {
            Producer::Component(id) => format!("component[{id}]"),
            Producer::External => "external".to_string(),
            Producer::Structural => "structural".to_string(),
        }
    }
}

/// The engine's error type.
#[derive(Debug)]
pub enum Error {
    /// A write attempted outside any visitor instance (§5.3, matrix 11).
    WriteOutsideVisitor { view: String },
    /// A read attempted outside any visitor instance.
    ReadOutsideVisitor { view: String },
    /// A view type is not registered (internal misuse).
    ViewNotRegistered { view: String },
    /// A component observes a view with no producer (authority, §5.4).
    NoProducerForView { view: String },
    /// An external patch targeted a view without an external producer.
    ExternalPatchToNonExternal { view: String },
    /// A producer wrote a fact owned by another producer (T5).
    OwnershipViolation {
        view: String,
        fact: String,
        writer: String,
        owner: String,
    },
    /// A topology violation (missing node, invalid reorder, ...).
    TopologyViolation { view: String, message: String },
    /// A fact cycle was detected; the epoch is rejected (T6).
    FactCycle { listing: Vec<String> },
    /// An authored component error (aborts the epoch identically).
    Authored(Box<dyn std::error::Error + Send + Sync>),
    /// A panic inside authored code.
    Panic(String),
    /// Internal invariants (a bug in the engine, not user code).
    Internal(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::WriteOutsideVisitor { view } => {
                write!(f, "write outside a visitor on view `{view}`")
            }
            Error::ReadOutsideVisitor { view } => {
                write!(f, "read outside a visitor on view `{view}`")
            }
            Error::ViewNotRegistered { view } => write!(f, "view `{view}` is not registered"),
            Error::NoProducerForView { view } => {
                write!(f, "view `{view}` is observed but has no producer")
            }
            Error::ExternalPatchToNonExternal { view } => {
                write!(f, "external patch on view `{view}` which has no external producer")
            }
            Error::OwnershipViolation { view, fact, writer, owner } => {
                write!(f, "ownership violation on `{view}`: fact {fact} written by {writer} but owned by {owner}")
            }
            Error::TopologyViolation { view, message } => {
                write!(f, "topology violation on `{view}`: {message}")
            }
            Error::FactCycle { listing } => {
                write!(f, "fact cycle detected:\n  {}", listing.join("\n  "))
            }
            Error::Authored(e) => write!(f, "component error: {e}"),
            Error::Panic(message) => write!(f, "component panic: {message}"),
            Error::Internal(message) => write!(f, "internal engine error: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Wraps an authored error.
    pub fn authored<E: std::error::Error + Send + Sync + 'static>(e: E) -> Self {
        Error::Authored(Box::new(e))
    }
}

/// The engine's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
