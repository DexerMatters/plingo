#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]
#![allow(private_interfaces)]

extern crate self as plingo;

pub use fluent_uri::Uri;
pub use reactive_macros::component as reactive_component;

pub use plingo_macros::{
    NonTerminal, PrettyNonTerminal, PrettyTerminal, Terminal, generate, lregex, scope_path,
};
pub use scheme::node::{
    Command, CommandCx, DefinitionEdge, DemandLease, DeriveCx, EdgeKind, Graph, InputNode,
    NodeError, NodeInspection, NodeKey, NodeProvider, NodeSchema, NodeValue, PortDeclaration,
    PortKind, ProviderState, ReadGraph, Relation, RelationSubscription, RelationUpdate, Snapshot,
    SnapshotId, Subscription, View, ViewFamily, ViewUpdate,
};

pub use component::workspace::{Document, Workspace};
pub use component::{
    Component, ComponentDiagnostics, Context, ContextView, DiagnosticSet, Diagnostics, Error,
    Index, IndexView, Output, Parsed, Result, Scope, Set, SetView, Table, TableView, WriteSet,
};
pub mod component;
pub mod reactive;
pub mod scheme;
pub mod utils;
pub mod visual;
