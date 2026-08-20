//! The workspace facade over the reactive engine (plan §8.4).
//!
//! One `Workspace` owns one engine, the external `SourceEdits` channel,
//! the built-in `source` fold, and the user's components. `open`/`edit`/
//! `close` translate to one external command (one epoch); `snapshot` reads
//! the committed state; `subscribe` hooks per-view changed facts. There is
//! no demand-lease API: installed components materialize unconditionally,
//! and closing a document retracts its whole downstream subtree.

use fluent_uri::Uri;

use crate::reactive::prelude::*;
use crate::reactive::view::ViewSpec;

use super::source::{SourceEdits, SourceEdit, edits_delta, install_source, load_delta};

/// The workspace facade.
pub struct Workspace {
    engine: Engine,
}

impl Workspace {
    /// Builds a workspace: installs the source pipeline plus any user
    /// components (`lexer`, `parser`, passes) through `install`.
    pub fn build(
        install: impl FnOnce(&mut Engine) -> Result<()>,
    ) -> Result<Workspace> {
        Self::build_with(1, install)
    }

    /// Builds a workspace with an explicit worker count (determinism
    /// harness: 1 and N workers must commit identical state).
    pub fn build_with(
        workers: usize,
        install: impl FnOnce(&mut Engine) -> Result<()>,
    ) -> Result<Workspace> {
        let mut engine = Engine::with_workers(workers);
        install_source(&mut engine)?;
        install(&mut engine)?;
        Ok(Workspace { engine })
    }

    /// The underlying engine (for tests and advanced use).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The underlying engine, mutably (drive raw commands in tests).
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Opens a document with the given full text. Re-opening an existing
    /// uri replaces its text (one load command).
    pub fn open(&mut self, uri: Uri<&'static str>, text: &str) -> Result<()> {
        let uri = uri.to_string();
        self.engine.command(vec![ExternalOp::map_set::<SourceEdits>(
            uri.clone(),
            load_delta(text),
        )])?;
        Ok(())
    }

    /// Applies one batch of edits to one or more documents (one command;
    /// per-document deltas are merged in position order).
    pub fn edit(&mut self, edits: Vec<SourceEdit>) -> Result<()> {
        // Group edits by uri.
        let mut by_uri: std::collections::BTreeMap<String, Vec<SourceEdit>> =
            std::collections::BTreeMap::new();
        for edit in edits {
            let uri = edit.span().uri.to_string();
            by_uri.entry(uri).or_default().push(edit);
        }
        let mut ops = Vec::new();
        for (uri, uri_edits) in by_uri {
            let delta = edits_delta(&uri_edits)?;
            ops.push(ExternalOp::map_set::<SourceEdits>(uri, delta));
        }
        self.engine.command(ops)?;
        Ok(())
    }

    /// Closes a document: removes its source entries; visitor retirement
    /// retracts the text and every downstream contribution.
    pub fn close(&mut self, uri: Uri<&'static str>) -> Result<()> {
        let uri = uri.to_string();
        self.engine
            .command(vec![ExternalOp::map_remove::<SourceEdits>(uri)])?;
        Ok(())
    }

    /// The committed state (read-only).
    pub fn snapshot(&self) -> Snapshot {
        self.engine.snapshot()
    }

    /// Subscribes to one view's changed facts per committed epoch.
    pub fn subscribe<V: ViewSpec>(&mut self, subscriber: Subscriber) -> Result<()> {
        self.engine.subscribe::<V>(subscriber)
    }
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Workspace { engine }")
    }
}