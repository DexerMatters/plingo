//! The built-in reactive parser (plan §3.2): pure grammar/replay and
//! snapshot machinery live here; parser publication is layered on top of it.
//! The former node-graph glue has been removed.
//!
//! This module is the versioned home of the parser: `grammar.rs`,
//! `build.rs`, `types.rs`, `analyze.rs`, `diagnostics.rs`, `recovery.rs`,
//! `parser.rs`, `parsing/`, and `data/` all live under it.

pub(crate) mod analyze;
pub(crate) mod build;
mod component;
#[doc(hidden)]
pub mod data;
pub mod delta;
pub(crate) mod diagnostics;
#[doc(hidden)]
pub mod grammar;
pub mod recovery_policy;
pub(crate) mod identity;
pub(crate) mod parser;
pub(crate) mod parsing;
pub(crate) mod recovery;
pub(crate) mod types;

#[doc(hidden)]
pub mod __macro_private;

pub use component::{
    AstSnapshots, ParseDiagnostics, ParseUnit, ParseUnits, TreeParseUnit, TreeParseUnits,
    install_parser, install_parser_tree,
};
#[doc(hidden)]
pub use data::ast::AstToken;
#[doc(hidden)]
pub use data::green::{ErrorKind, ParseErrorInfo};
pub use parser::Parser;
pub use recovery_policy::{ErrorRegion, MissingToken, ParserRecoveryPolicy, RecoveryProduct, RegionalFallbackPolicy, SkippedToken};
pub use delta::{KeyDelta, OrderedDelta, ParseDelta, ParseDiagnosticKey, ParsedStatus, RecoverySegmentId, TokenAnchor};
pub use parsing::ParseError;
pub use types::{
    AstLookupError, AstSnapshot, AstTokenSnapshotEntry, IncrementalParseStats, ParseStatus,
    ParserConfig, ParserWork, ResolvedAst, DocumentSnapshot,
};
pub(crate) use types::{ParserSnapshotState, ParserTokenDocument, TokenData};

/// The family marker implemented by the root enum of every
/// `#[abstract_tree(members(...))]` family.
pub trait AbstractTreeFamily: 'static {
    /// The generated payload union (kept private to the parser ABI).
    type Node: Clone + Send + Sync + 'static;
    /// The generated typed case union.
    type Case: Clone + Send + Sync + 'static;
    /// The generated uniform tree view.
    type View: crate::reactive::view::View;

    #[doc(hidden)]
    fn __tree_plain_emit_one(
        parent: Option<crate::reactive::view::Node<Self::View>>,
        uri: &str,
        arena: &crate::framework::parse::data::AstArena,
        id: crate::reactive::view::Node<Self::View>,
        value: &Self,
        resolver: &dyn Fn(u64) -> Option<u64>,
    ) -> crate::reactive::Result<()>;

    /// Returns the deterministic syntax-view node identity for one retained
    /// AST record. `root` selects the document-stable root identity rather
    /// than the ordinary record identity.
    #[doc(hidden)]
    fn __tree_plain_node_for_record(
        uri: &str,
        arena: &crate::framework::parse::data::AstArena,
        record: u64,
        root: bool,
        resolver: &dyn Fn(u64) -> Option<u64>,
    ) -> Option<crate::reactive::view::Node<Self::View>>;

    /// The payload variant ordinal of one arena record.
    #[doc(hidden)]
    fn __tree_member_kind_of(
        arena: &crate::framework::parse::data::AstArena,
        record: u64,
    ) -> Option<u8>;

    /// Writes ONLY the payload fact of one record (plan §12 step 1).
    #[doc(hidden)]
    fn __tree_refresh_payload(
        uri: &str,
        arena: &crate::framework::parse::data::AstArena,
        record: u64,
        root: bool,
        resolver: &dyn Fn(u64) -> Option<u64>,
    ) -> crate::reactive::Result<bool>;

    /// Publishes one exact parser-record mutation. Implementations derive its
    /// parent and child links from arena facts; no tree-wide walk is allowed.
    #[doc(hidden)]
    fn __tree_plain_emit_record(
        uri: &str,
        arena: &crate::framework::parse::data::AstArena,
        record: u64,
        root: bool,
        resolver: &dyn Fn(u64) -> Option<u64>,
    ) -> crate::reactive::Result<bool>;

    /// Retracts one arena-backed record's split facts (payload, parent,
    /// child order, and the surviving parent's link). Descendant records
    /// are retracted by their own calls.
    #[doc(hidden)]
    fn __tree_plain_remove_record(
        uri: &str,
        arena: &crate::framework::parse::data::AstArena,
        record: u64,
        resolver: &dyn Fn(u64) -> Option<u64>,
    ) -> crate::reactive::Result<bool>;

    #[doc(hidden)]
    fn __tree_plain_emit_roots(
        uri: &str,
        roots: Vec<crate::reactive::view::Node<Self::View>>,
    ) -> crate::reactive::Result<()>;

    #[doc(hidden)]
    fn __tree_kind_of(value: &Self) -> u8;

    /// Legacy record-derived identity, retained for compatibility with
    /// hand-built callers that still need an arena-record key.
    #[doc(hidden)]
    fn __node_from_record<M: 'static>(
        uri: &str,
        record: u64,
        kind: u8,
    ) -> crate::reactive::view::Node<Self::View> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        uri.hash(&mut hasher);
        record.hash(&mut hasher);
        kind.hash(&mut hasher);
        std::any::TypeId::of::<Self::View>().hash(&mut hasher);
        std::any::TypeId::of::<M>().hash(&mut hasher);
        crate::reactive::view::Node::from_raw(hasher.finish())
    }

    /// Derives the stable syntax-view identity from a document-stable
    /// lineage serial and the member ordinal.
    ///
    /// The member ordinal identifies the grammar field (for example,
    /// `Expr`), not its current payload variant (`True` versus `Number`).
    /// A retained syntax lineage therefore keeps one node identity while
    /// its payload changes shape.
    #[doc(hidden)]
    fn __node_from_parts(
        uri: &str,
        lineage: u64,
        member: u8,
    ) -> crate::reactive::view::Node<Self::View> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        uri.hash(&mut hasher);
        lineage.hash(&mut hasher);
        member.hash(&mut hasher);
        std::any::TypeId::of::<Self::View>().hash(&mut hasher);
        crate::reactive::view::Node::from_raw(hasher.finish())
    }

    /// Returns the stable syntax identity for a document's accepted root.
    ///
    /// The root identity is document-stable. The member argument remains in
    /// the private ABI so generated code can share the same helper for root
    /// and nested records; it is deliberately not part of the root key.
    #[doc(hidden)]
    fn __root_node(
        uri: &str,
        _member: u8,
    ) -> crate::reactive::view::Node<Self::View> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        uri.hash(&mut hasher);
        0x726f6f74_u64.hash(&mut hasher);
        std::any::TypeId::of::<Self::View>().hash(&mut hasher);
        std::any::TypeId::of::<Self>().hash(&mut hasher);
        crate::reactive::view::Node::from_raw(hasher.finish())
    }
}
