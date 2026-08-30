//! The workspace facade over the reactive engine (plan §8.4).
//!
//! One `Workspace` owns one engine, the external `SourceEdits` channel,
//! the built-in source fold, and the user's pipeline stages. `open`/`edit`/
//! `close` translate to one external command (one epoch); `snapshot` reads
//! the committed state; `subscribe` hooks per-view changed facts. There is
//! no demand-lease API: installed stages materialize unconditionally, and
//! closing a document retracts its whole downstream subtree.

use std::collections::{BTreeMap, HashSet};
use std::marker::PhantomData;
use std::sync::Arc;

use fluent_uri::Uri;

use crate::framework::lex::{LexerRoot, LexerWork, install_lexer};
use crate::framework::parse::{ParserWork, install_parser_tree};
use crate::framework::source::{
    DocumentId, SourceCommand, SourceCommandId, SourceDelta, SourceEdit, SourceEdits,
    SourceRevisions, SourceWork, apply_splices, install_source, normalize_edits,
};
use crate::reactive::framework_mount::{MountComponent, MountComponentWithDomain};
use crate::reactive::kind::emit_view;
use crate::reactive::plain;
use crate::reactive::{CommandReport, Engine, EngineWork, Result, Snapshot, View};
// ---------------------------------------------------------------------------
// Work reporting (plan §10.1).
// ---------------------------------------------------------------------------

/// Command-local per-document work counters. One fixed struct holds every
/// document's counters for one command; each document child merges its own
/// entry exactly once.
#[derive(Debug, Default)]
pub(crate) struct DocMetrics {
    pub(crate) source: BTreeMap<String, SourceWork>,
    pub(crate) lexer: BTreeMap<String, LexerWork>,
    pub(crate) parser: BTreeMap<String, ParserWork>,
}

/// Records source-pipeline counters for one document into the active
/// command frame. Outside a command the record is dropped.
pub(crate) fn record_source_work(uri: &str, f: impl FnOnce(&mut SourceWork)) {
    plain::record_command_metric::<DocMetrics>(|metrics| {
        f(metrics.source.entry(uri.to_string()).or_default());
    });
}

/// Records lexer counters for one document into the active command frame.
pub(crate) fn record_lexer_work(uri: &str, f: impl FnOnce(&mut LexerWork)) {
    plain::record_command_metric::<DocMetrics>(|metrics| {
        f(metrics.lexer.entry(uri.to_string()).or_default());
    });
}

/// Records parser counters for one document into the active command frame.
pub(crate) fn record_parser_work(uri: &str, f: impl FnOnce(&mut ParserWork)) {
    plain::record_command_metric::<DocMetrics>(|metrics| {
        f(metrics.parser.entry(uri.to_string()).or_default());
    });
}

/// Exact de-duplicated current-root byte counts (plan §9).
///
/// Shared `Arc` allocations count once. No list specialization or inline
/// storage is excluded from this accounting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveBytes {
    /// Persistent tape nodes.
    pub tape_nodes: u64,
    /// Persistent radix/HAMT trie nodes.
    pub trie_nodes: u64,
    /// Owner-set nodes.
    pub owner_set_nodes: u64,
    /// Immutable record segments.
    pub segments: u64,
    /// Intern indexes (live-record maps).
    pub intern_indexes: u64,
    /// Repair-DAG nodes retained by recovery segments.
    pub repair_nodes: u64,
    /// State slots + publication roots.
    pub state_roots: u64,
}

impl LiveBytes {
    /// Total bytes across all categories.
    pub fn total(&self) -> u64 {
        self.tape_nodes
            + self.trie_nodes
            + self.owner_set_nodes
            + self.segments
            + self.intern_indexes
            + self.repair_nodes
            + self.state_roots
    }
}

/// Deterministic pipeline work counters keyed by document URI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkReport {
    source: BTreeMap<String, SourceWork>,
    lexer: BTreeMap<String, LexerWork>,
    parser: BTreeMap<String, ParserWork>,
}

impl WorkReport {
    fn from_metrics(metrics: &DocMetrics) -> Self {
        Self {
            source: metrics.source.clone(),
            lexer: metrics.lexer.clone(),
            parser: metrics.parser.clone(),
        }
    }

    /// Source-pipeline counters for one document.
    pub fn source(&self, uri: &str) -> Option<&SourceWork> {
        self.source.get(uri)
    }

    /// Lexer counters for one document.
    pub fn lexer(&self, uri: &str) -> Option<&LexerWork> {
        self.lexer.get(uri)
    }

    /// Parser counters for one document.
    pub fn parser(&self, uri: &str) -> Option<&ParserWork> {
        self.parser.get(uri)
    }

    pub(crate) fn merge_source(&mut self, uri: String, work: SourceWork) {
        self.source.entry(uri).or_default().merge(&work);
    }
}

/// The committed result of one workspace command: engine report plus
/// per-document deterministic work counters.
#[derive(Clone, Debug)]
pub struct WorkspaceReport {
    command: CommandReport,
    work: WorkReport,
}

impl WorkspaceReport {
    /// The underlying reactive command report.
    pub fn command(&self) -> &CommandReport {
        &self.command
    }

    /// The committed epoch counter.
    pub fn epoch(&self) -> u64 {
        self.command.epoch
    }

    /// Computation rounds evaluated by the command.
    pub fn rounds(&self) -> u32 {
        self.command.rounds
    }

    /// Changed-fact count for one typed view.
    pub fn changed<V: View>(&self) -> usize {
        self.command.changed::<V>()
    }

    /// Engine-level work counters.
    pub fn engine_work(&self) -> &EngineWork {
        self.command.engine_work()
    }

    /// Per-document pipeline work counters.
    pub fn work(&self) -> &WorkReport {
        &self.work
    }
}

/// The workspace facade.
pub struct Workspace {
    engine: Engine,
    /// URIs that have been opened in this workspace.  A close removes the
    /// live source revision but deliberately keeps this tombstone so a later
    /// open starts a fresh document identity rather than aliasing old facts.
    opened_documents: HashSet<String>,
}
/// Staged workspace construction.
///
/// The builder installs the source pipeline first and then applies the
/// requested lexer, parser, and externally mounted component roots.  The
/// lexer type is carried at the type level so `.parser::<A>()` cannot be
/// detached from the lexer it consumes.
pub struct WorkspaceBuilder<R = ()> {
    stages: Vec<Box<dyn FnOnce(&mut Engine) -> Result<()>>>,
    marker: PhantomData<fn() -> R>,
}

impl Workspace {
    /// Starts the public, typed workspace builder.
    pub fn builder() -> WorkspaceBuilder {
        WorkspaceBuilder {
            stages: Vec::new(),
            marker: PhantomData,
        }
    }
}

impl<R> WorkspaceBuilder<R> {
    fn stage(mut self, stage: impl FnOnce(&mut Engine) -> Result<()> + 'static) -> Self {
        self.stages.push(Box::new(stage));
        self
    }

    /// Adds an externally rooted component mount. The mount's removable
    /// handle (a keyed family for map-entry mounts) is discarded here;
    /// lifecycle is owned by the workspace for its whole life.
    pub fn mount<C, S>(self, selector: S) -> Self
    where
        C: MountComponent<S>,
        S: 'static,
    {
        self.stage(move |engine| {
            C::mount(engine, selector)?;
            Ok(())
        })
    }

    /// Adds an externally rooted component mount with an explicit output
    /// domain.  Use this when the selector domain and rendered tree domain
    /// differ.
    pub fn mount_with_domain<C, S, D>(self, selector: S, domain: D) -> Self
    where
        C: MountComponentWithDomain<S, D>,
        S: 'static,
        D: 'static,
    {
        self.stage(|engine| C::mount_with_domain(engine, selector, domain))
    }

    /// Completes construction and returns a committed workspace.
    pub fn build(self) -> Result<Workspace> {
        let mut engine = Engine::new();
        install_source(&mut engine)?;
        for stage in self.stages {
            stage(&mut engine)?;
        }
        Ok(Workspace {
            engine,
            opened_documents: HashSet::new(),
        })
    }
}

impl WorkspaceBuilder<()> {
    /// Adds the one lexer consumed by the parser and source pipeline.
    pub fn lexer<R>(self) -> WorkspaceBuilder<R>
    where
        R: LexerRoot + Clone + std::fmt::Debug,
    {
        let stages = self.stage(|engine| install_lexer::<R>(engine)).stages;
        WorkspaceBuilder {
            stages,
            marker: PhantomData,
        }
    }
}

impl<R> WorkspaceBuilder<R>
where
    R: LexerRoot + Clone + std::fmt::Debug,
{
    /// Adds the parser and its generated abstract-tree publication.
    pub fn parser<A>(self) -> Self
    where
        A: crate::framework::parse::__macro_private::NonTerminalSpec
            + crate::reactive::abstract_tree::AbstractTreeNode
            + crate::reactive::abstract_tree::SyntaxFamilyPublication
            + 'static,
    {
        self.stage(|engine| install_parser_tree::<R, A>(engine))
    }
}

impl Workspace {
    /// Builds a workspace: installs the source pipeline plus any user
    /// stages (lexer, parser, and passes) through `install`.
    pub fn build(install: impl FnOnce(&mut Engine) -> Result<()>) -> Result<Workspace> {
        let mut engine = Engine::new();
        install_source(&mut engine)?;
        install(&mut engine)?;
        Ok(Workspace {
            engine,
            opened_documents: HashSet::new(),
        })
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
    /// URI is represented as one full-range source replacement; same-content
    /// replacements are filtered by the exact normalized delta path when
    /// callers use `edit`.
    pub fn open(&mut self, uri: Uri<String>, text: &str) -> Result<WorkspaceReport> {
        let uri_string = uri.to_string();
        let existing = self.current_revision(&uri_string);
        let next_text = Arc::new(ropey::Rope::from_str(text));

        // Reopening with identical content is an external no-op.  This
        // comparison is confined to the explicit full-document `open`
        // operation; incremental edits never compare complete Ropes.
        if existing
            .as_ref()
            .is_some_and(|revision| revision.text.as_ref() == next_text.as_ref())
        {
            let report = self.engine.command::<fn() -> Result<()>>(|| Ok(()))?;
            return Ok(Self::assemble(report));
        }

        let base = existing
            .as_ref()
            .map(|revision| (revision.document.id, revision.id));
        let delta = match &existing {
            Some(revision) => {
                // Reopening becomes one full-range replacement. Avoid a
                // complete old/new Rope equality scan on the command path.
                let len = revision.text.len_bytes();
                SourceDelta::Edit {
                    splices: vec![crate::framework::source::SourceSplice {
                        old_range: 0..len,
                        new_range: 0..text.len(),
                    }]
                    .into(),
                }
            }
            None => SourceDelta::Load {
                new_len: text.len(),
            },
        };
        let command_id = crate::framework::source::next_command_id_pub();
        let fresh_document_id = (existing.is_none() && self.opened_documents.contains(&uri_string))
            .then_some(DocumentId(command_id));
        let command = self.engine.command({
            let uri_string = uri_string.clone();
            move || {
                emit_view::<SourceEdits>()?.insert(
                    uri_string,
                    SourceCommand {
                        id: SourceCommandId(command_id),
                        fresh_document_id,
                        base,
                        delta,
                        next_text,
                    },
                )?;
                Ok(())
            }
        })?;
        self.opened_documents.insert(uri_string);
        Ok(Self::assemble(command))
    }

    /// The committed revision for one document, if any.
    fn current_revision(&self, uri: &str) -> Option<Arc<crate::framework::source::SourceRevision>> {
        self.engine
            .snapshot()
            .observe::<SourceRevisions>(uri.to_string())
            .map(|value| (*value).clone())
    }

    /// Applies one batch of edits to one or more documents (one command;
    /// per-document deltas are merged in position order).
    pub fn edit(&mut self, edits: Vec<SourceEdit>) -> Result<WorkspaceReport> {
        // Pre-command normalization runs outside the engine transaction so a
        // rejected batch never opens an epoch (plan §6).
        let mut by_uri: BTreeMap<String, Vec<SourceEdit>> = BTreeMap::new();
        for edit in edits {
            let uri = edit.span().uri.to_string();
            by_uri.entry(uri).or_default().push(edit);
        }
        let mut validations: Vec<(String, SourceWork)> = Vec::with_capacity(by_uri.len());
        let mut commands = Vec::with_capacity(by_uri.len());
        for (uri, uri_edits) in &by_uri {
            let _frame = plain::push_metric_frame();
            let revision = self.current_revision(uri);
            let normalized =
                normalize_edits(revision.as_ref().map(|r| r.text.as_ref()), uri_edits)?;
            let mut validation = plain::take_frame_metric::<SourceWork>();
            validation.validated_operations = normalized.validated_operations;
            validation.effective_splices += normalized.effective_splices;
            validation.bytes_removed += normalized.bytes_removed;
            validation.bytes_inserted += normalized.bytes_inserted;
            validation.rope_chunks_traversed += normalized.rope_chunks_traversed;
            if normalized.splices.is_empty() {
                // Idle batch: no epoch, no write (plan §6 step 5).
                validations.push((uri.clone(), validation));
                continue;
            }
            // Apply descending splices to an O(1) Rope clone.
            let previous = revision
                .as_ref()
                .expect("normalize requires an opened document");
            let next_rope =
                apply_splices(&previous.text, &normalized.splices, &normalized.inserted)?;
            let new_len = next_rope.len_bytes();
            let old_len = previous.text.len_bytes();
            validations.push((uri.clone(), validation));
            // Splices were computed against old coordinates; convert to the
            // exact old/new ranges the delta format stores.
            let shift_total = new_len as isize - old_len as isize;
            let _ = shift_total;
            let delta = SourceDelta::Edit {
                splices: normalized.splices.into(),
            };
            let base = (previous.document.id, previous.id);
            commands.push((
                uri.clone(),
                SourceCommand {
                    id: SourceCommandId(crate::framework::source::next_command_id_pub()),
                    fresh_document_id: None,
                    base: Some(base),
                    delta,
                    next_text: Arc::new(next_rope),
                },
            ));
        }
        if commands.is_empty() {
            let report = self.engine.command::<fn() -> Result<()>>(|| Ok(()))?;
            return Ok(Self::assemble_with_validations(report, validations));
        }
        let command = self.engine.command(move || {
            for (uri, cmd) in commands {
                emit_view::<SourceEdits>()?.insert(uri, cmd)?;
            }
            Ok(())
        })?;
        Ok(Self::assemble_with_validations(command, validations))
    }

    /// Closes a document; the omitted source publication retracts all
    /// downstream facts through the plain-function pipeline.
    pub fn close(&mut self, uri: Uri<String>) -> Result<WorkspaceReport> {
        let uri_string = uri.to_string();
        let command = self
            .engine
            .command(move || emit_view::<SourceEdits>()?.remove(uri_string))?;
        Ok(Self::assemble(command))
    }

    /// Freezes command metrics into the workspace report.
    fn assemble(command: CommandReport) -> WorkspaceReport {
        Self::assemble_with_validations(command, Vec::new())
    }

    /// Freezes command metrics plus pre-command validation counters.
    fn assemble_with_validations(
        command: CommandReport,
        validations: Vec<(String, SourceWork)>,
    ) -> WorkspaceReport {
        let mut work = command
            .metric::<DocMetrics>()
            .map(WorkReport::from_metrics)
            .unwrap_or_default();
        for (uri, validation) in validations {
            work.merge_source(uri, validation);
        }
        WorkspaceReport { command, work }
    }

    /// The committed state (read-only).
    pub fn snapshot(&self) -> Snapshot {
        self.engine.snapshot()
    }

    /// Subscribes to one typed view's changed-fact count per committed epoch.
    pub fn subscribe<V: View>(
        &mut self,
        subscriber: impl Fn(Snapshot, usize) + Send + Sync + 'static,
    ) -> Result<()> {
        self.engine.subscribe::<V>(subscriber)
    }

    /// Debug/test liveness audit over the production indexes (follow-up
    /// plan §4 item 12). Read-only; empty output means consistent.
    #[doc(hidden)]
    pub fn __liveness_audit(&self) -> Vec<String> {
        self.engine.__liveness_audit()
    }
}
impl std::fmt::Debug for Workspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Workspace { engine }")
    }
}
