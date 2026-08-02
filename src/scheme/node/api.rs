use super::engine::{CommandCx, DeriveCx, ReclaimCx};
use std::{
    any::{TypeId, type_name},
    hash::Hash,
    sync::{Arc, Mutex},
};
use thiserror::Error;

pub type SnapshotId = u64;

/// A stable, typed key used to address a view fact or provider instance.
pub trait NodeKey: Clone + Eq + Hash + Send + Sync + 'static {}

impl<T> NodeKey for T where T: Clone + Eq + Hash + Send + Sync + 'static {}

/// A value stored in a materialized view.
pub trait NodeValue: Clone + PartialEq + Send + Sync + 'static {}

impl<T> NodeValue for T where T: Clone + PartialEq + Send + Sync + 'static {}

/// A typed, keyed value exposed by the graph.
///
/// A view is a snapshot-readable materialization with at most one value per
/// key. Derived values remain materialized only while their producer is live;
/// root values are retained until a command replaces them.
pub trait View: Send + Sync + 'static {
    type Key: NodeKey;
    type Value: NodeValue;
}

/// A multi-owner set of immutable facts.
///
/// Unlike a [`View`], a relation fact can be emitted by several node instances.
/// It remains visible until its final supporting node retracts it.  This is the
/// primitive used by scope edges, datums, references, and requirements.
pub trait Relation: Send + Sync + 'static {
    type Fact: NodeKey;
}

/// A relation whose facts can be partitioned into independently observable
/// buckets.
pub trait IndexedRelation: Relation {
    type Index: NodeKey;

    fn index(fact: &Self::Fact) -> Self::Index;
}

/// The ownership policy of a declared node port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PortKind {
    /// One owner publishes a keyed value.
    Map,
    /// One or more node instances support an immutable fact.
    Set,
    /// A support-counted set with independently observable index buckets.
    IndexedSet,
}

/// Explicit schema-level edge categories. Runtime traversal never collapses
/// these into an untyped neighbor relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    Publishes,
    Supports,
    DependsOn,
    KeepsAlive,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DefinitionEdge {
    pub from: &'static str,
    pub to: &'static str,
    pub kind: EdgeKind,
}

/// Runtime metadata for one typed port. Type identities are intentionally kept
/// distinct: a port type, rather than a universal integer, is its schema ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PortDeclaration {
    pub name: &'static str,
    pub type_id: TypeId,
    pub kind: PortKind,
}

impl PortDeclaration {
    pub fn map<V: View>() -> Self {
        Self {
            name: type_name::<V>(),
            type_id: TypeId::of::<V>(),
            kind: PortKind::Map,
        }
    }

    pub fn set<R: Relation>() -> Self {
        Self {
            name: type_name::<R>(),
            type_id: TypeId::of::<R>(),
            kind: PortKind::Set,
        }
    }

    pub fn indexed_set<R: IndexedRelation>() -> Self {
        Self {
            name: type_name::<R>(),
            type_id: TypeId::of::<R>(),
            kind: PortKind::IndexedSet,
        }
    }
}

/// Declared, inspectable schema of a provider's observable port family.
#[derive(Clone, Debug)]
pub struct NodeSchema {
    pub provider: &'static str,
    pub ports: Vec<PortDeclaration>,
}

impl NodeSchema {
    pub fn new(provider: &'static str, ports: Vec<PortDeclaration>) -> Self {
        Self { provider, ports }
    }

    pub fn declares_map<V: View>(&self) -> bool {
        self.ports
            .iter()
            .any(|port| port.type_id == TypeId::of::<V>() && port.kind == PortKind::Map)
    }

    pub fn declares_relation<R: Relation>(&self) -> bool {
        self.ports.iter().any(|port| {
            port.type_id == TypeId::of::<R>()
                && matches!(port.kind, PortKind::Set | PortKind::IndexedSet)
        })
    }
}

/// Inspection of one live provider instance and its typed edges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeInspection {
    pub materialized: bool,
    pub root_pins: usize,
    pub keeping_parents: usize,
    pub publications: usize,
    pub relation_supports: usize,
    pub dependencies: usize,
    pub children: usize,
}

/// A family of typed ports exposed by a provider. This is intentionally a
/// small runtime declaration rather than a giant aggregate output value.
pub trait ViewFamily: Send + Sync + 'static {
    fn declaration() -> Vec<PortDeclaration>;
}

/// A first-class externally authored node category. Input nodes never derive;
/// commands are the sole authority allowed to write their declared map ports.
pub trait InputNode: Send + Sync + 'static {
    type Key: NodeKey;
    type Views: ViewFamily;

    fn schema() -> NodeSchema;
}

/// Common read protocol for immutable snapshots and reactive derivations.
/// A derivation records dependencies for these operations; a snapshot simply
/// reads its committed revision. Scan ordering is deliberately unspecified;
/// callers that require order must sort by a domain-specific stable key.
pub trait ReadGraph {
    fn get<V: View>(&self, key: V::Key) -> Option<V::Value>;
    fn contains<R: Relation>(&self, fact: R::Fact) -> bool;
    fn scan<R: IndexedRelation>(&self, index: R::Index) -> Vec<R::Fact>;
    fn scan_all<R: Relation>(&self) -> Vec<R::Fact>;
}

/// A stable keyed provider of declared typed ports.
pub trait NodeProvider: Send + Sync + 'static {
    type Key: NodeKey;

    fn derive(&self, cx: &mut DeriveCx<'_>, key: Self::Key) -> Result<(), NodeError>;

    fn schema() -> NodeSchema
    where
        Self: Sized;

    fn reclaim(&self, _cx: &mut ReclaimCx<'_>, _key: Self::Key) -> Result<(), NodeError> {
        Ok(())
    }

    /// Whether derivations stage private state through [`DeriveCx::state_mut`].
    /// State-touching providers always run on the serial lane; the flag is
    /// statically declared per provider kind.
    fn uses_state() -> bool
    where
        Self: Sized,
    {
        false
    }
}

/// A root-state mutation.
///
/// Commands are the only API that can update a root port. Derived provider
/// publications are written exclusively through [`DeriveCx`].
pub trait Command: Send + 'static {
    type Output;

    fn apply(self, cx: &mut CommandCx<'_>) -> Result<Self::Output, NodeError>;
}

/// Errors raised by the node graph.
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("provider `{0}` is not installed")]
    MissingProvider(&'static str),
    #[error("view `{0}` has no value for the requested key")]
    MissingView(&'static str),
    #[error("provider `{0}` has already been installed")]
    DuplicateProvider(&'static str),
    #[error("provider dependency cycle detected while deriving `{0}`")]
    DependencyCycle(&'static str),
    #[error("provider `{provider}` attempted to overwrite output owned by `{owner}`")]
    OutputConflict {
        provider: &'static str,
        owner: &'static str,
    },
    #[error("provider `{0}` attempted to overwrite an authoritative root view")]
    OutputRootConflict(&'static str),
    #[error("provider `{0}` emitted the same view key more than once")]
    DuplicateOutput(&'static str),
    #[error("provider `{provider}` attempted to publish undeclared {kind} port `{port}`")]
    UndeclaredPort {
        provider: &'static str,
        port: &'static str,
        kind: &'static str,
    },
    #[error("input node `{0}` has already been installed")]
    DuplicateInput(&'static str),
    #[error("root command cannot overwrite output owned by `{0}`")]
    RootOutputConflict(&'static str),
    #[error("graph revision overflow")]
    RevisionOverflow,
    #[error("{0}")]
    Message(String),
}

impl NodeError {
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub fn missing_view<V: View>() -> Self {
        Self::MissingView(type_name::<V>())
    }
}

/// Mutable provider-local data staged with the graph transaction.
///
/// Cloning the handle shares the same state. A provider obtains a mutable
/// staged copy through [`DeriveCx::state_mut`]; that copy replaces the stored
/// value only after the graph transaction commits successfully.
pub struct ProviderState<T: Clone + Send + Sync + 'static> {
    pub(crate) value: Arc<Mutex<T>>,
}

impl<T: Clone + Send + Sync + 'static> ProviderState<T> {
    pub fn new(value: T) -> Self {
        Self {
            value: Arc::new(Mutex::new(value)),
        }
    }

    /// Returns a snapshot of the last successfully committed value.
    pub fn get(&self) -> Result<T, NodeError> {
        self.value
            .lock()
            .map(|value| value.clone())
            .map_err(|_| NodeError::message("component state lock poisoned"))
    }
}

impl<T: Clone + Send + Sync + 'static> Clone for ProviderState<T> {
    fn clone(&self) -> Self {
        Self {
            value: Arc::clone(&self.value),
        }
    }
}
