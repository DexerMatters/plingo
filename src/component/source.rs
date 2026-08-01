//! Authoritative source-document vocabulary for the node graph.
//!
//! Source text is no longer a top layer. [`node::DocumentText`] is a root view
//! and [`node::ApplySourceEdit`] / [`node::LoadSource`] are graph commands.

use std::{ops::Range, sync::Arc};

use crate::utils::Span;

pub mod node;
pub use node::{
    ApplySourceEdit, ApplySourceEdits, DocumentChange, DocumentText, LoadSource, LoadSourceText,
    SourceInput, SourceViews,
};

/// One editor operation against an authoritative source document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceEdit {
    Insert { key: Span, value: String },
    Delete { key: Span },
}

impl SourceEdit {
    pub fn span(&self) -> &Span {
        match self {
            Self::Insert { key, .. } | Self::Delete { key } => key,
        }
    }
}

/// One exact source replacement between the command's original and final
/// document revisions. A batch retains disjoint sparse splices with old/new
/// coordinate maps rather than rediscovering one broad whole-text diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSplice {
    pub old_range: Range<usize>,
    pub new_range: Range<usize>,
    pub removed: Arc<str>,
    pub inserted: Arc<str>,
}

/// The exact sparse source delta that produced the current document revision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceDelta {
    pub splices: Arc<[SourceSplice]>,
}
