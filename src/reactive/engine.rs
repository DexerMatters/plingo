//! Plain-function reactive engine.
//!
//! The engine owns committed view state and the set of installed opaque root
//! computations. Authored code crosses the runtime only through the effects in
//! [`crate::reactive::api`]; all invocation graphs, write ownership, and
//! rollback state remain private.

use std::any::TypeId;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, RwLock};

use crate::reactive::api::{Planned, Running};
use crate::reactive::error::{Error, Result};
use crate::reactive::kind::{
    BoxView, GraphFact, GraphKey, GraphView, ListFact, ListKey, ListView, MapView, TreeFact,
    TreeKey, TreeView,
};
use crate::reactive::plain::{self, OutputSink, PlainRuntime};
use crate::reactive::value::{KeyValue, Value};
use crate::reactive::view::{Node, View};

/// Stable identity of one evaluated computation relationship.
///
/// The identity is derived from the authored function type, its `run`
/// callsite (when it is nested), and the semantic input key. It deliberately
/// excludes runtime invocation ids, which are allocation details and differ
/// between cold and warm executions.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InvocationIdentity {
    pub function: String,
    pub file: Option<String>,
    pub line: u32,
    pub column: u32,
    pub input_hash: u64,
}

/// Automatic per-command computation-evaluation counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InvocationWork {
    counts: BTreeMap<InvocationIdentity, u64>,
}

impl InvocationWork {
    /// Returns every evaluated computation identity in deterministic order.
    pub fn counts(&self) -> &BTreeMap<InvocationIdentity, u64> {
        &self.counts
    }

    /// Returns the number of evaluations for one identity.
    pub fn count(&self, identity: &InvocationIdentity) -> u64 {
        self.counts.get(identity).copied().unwrap_or(0)
    }

    /// Returns the total number of evaluated computation invocations.
    pub fn total(&self) -> u64 {
        self.counts.values().copied().sum()
    }

    pub(crate) fn record(&mut self, identity: InvocationIdentity) {
        *self.counts.entry(identity).or_default() += 1;
    }
}

/// The report of one committed external command or root installation.
#[derive(Clone, Debug)]
pub struct CommandReport {
    /// The committed epoch counter. It is unchanged for a zero-work command.
    pub epoch: u64,
    /// Number of computation rounds evaluated by the command.
    pub rounds: u32,
    plain_changed: HashMap<TypeId, usize>,
    /// Deterministic engine work performed by this command.
    pub engine: EngineWork,
    /// Automatic evaluation counts keyed by stable computation identity.
    pub invocations: InvocationWork,
    metrics: Arc<plain::MetricExtensions>,
}

impl CommandReport {
    /// Returns the number of changed facts for one typed view.
    pub fn changed<V: View>(&self) -> usize {
        self.plain_changed
            .get(&TypeId::of::<V>())
            .copied()
            .unwrap_or(0)
    }

    /// Deterministic engine work counters for this command.
    pub fn engine_work(&self) -> &EngineWork {
        &self.engine
    }

    /// Automatic computation-evaluation counts for this command.
    pub fn invocation_work(&self) -> &InvocationWork {
        &self.invocations
    }

    /// One typed command-local metric extension recorded by framework
    /// components. Unknown types return `None`.
    pub fn metric<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.metrics.get::<T>()
    }
}
/// Deterministic engine work counters for one command. Counters are
/// monotonic within the command and roll back with it. Fields introduced by
/// later store generations stay at zero on earlier generations, so the
/// schema is stable across the incremental-store migration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineWork {
    /// Typed fact reads resolved through the store.
    pub fact_reads: u64,
    /// Exact-key lookups performed.
    pub fact_lookups: u64,
    /// Fact entries scanned by linear lookups and enumerations.
    pub fact_scan_steps: u64,
    /// Candidate fact writes applied.
    pub fact_writes: u64,
    /// Candidate fact retractions applied.
    pub fact_retractions: u64,
    /// Distinct facts touched by the committed journal.
    pub facts_touched: u64,
    /// Facts whose committed value changed.
    pub facts_changed: u64,
    /// Whole-view input enumerations.
    pub view_enumerations: u64,
    /// Whole-state diffs computed.
    pub state_diffs: u64,
    /// Fact entries visited by state diffs.
    pub diff_scan_steps: u64,
    /// Dirty-invocation selections performed.
    pub dirty_selections: u64,
    /// Invocation entries scanned while selecting or marking.
    pub invocation_scans: u64,
    /// Dirty-queue insertions.
    pub queue_pushes: u64,
    /// Dirty-queue pops.
    pub queue_pops: u64,
    /// Invocation evaluations executed.
    pub invocation_evaluations: u64,
    /// Dependency index entries replaced after successful evaluation.
    pub dependency_entries_changed: u64,
    /// Exact dependency marks (indexed store generation).
    pub exact_marks: u64,
    /// Wildcard dependency marks (indexed store generation).
    pub wildcard_marks: u64,
    /// Persistent index path nodes created (indexed store generation).
    pub index_path_nodes: u64,
    /// Persistent index probes (indexed store generation).
    pub index_probes: u64,
    /// Persistent owner-set nodes created (indexed store generation).
    pub owner_set_nodes: u64,
    /// Coalescing journal entries (indexed store generation).
    pub journal_entries: u64,
    /// Indexed patch-key bucket probes.
    pub patch_key_lookups: u64,
    /// Exact patch-key equality comparisons inside collision buckets.
    pub patch_key_comparisons: u64,
    /// Patch operations coalesced before commit.
    pub patch_ops_coalesced: u64,
    /// Ordered splice handles applied.
    pub ordered_splices_applied: u64,
    /// Explicit forbidden vector scans in patch processing.
    pub full_patch_vector_scans: u64,
    /// Per-structure physical work (follow-up plan §4 item 7): operations,
    /// comparisons, visited/copied/created nodes, rebalances, and max depth
    /// grouped by primitive persistent-index structure.
    pub path_work: crate::reactive::pathwork::PathWorkReport,
}

impl EngineWork {
    /// Serializes the per-structure work counters for benchmark/reporting
    /// consumers without exposing the internal path-work module.
    pub fn path_work_json(&self) -> String {
        crate::reactive::pathwork::pathwork_path_work_json(&self.path_work)
    }
}

type Subscription = Arc<dyn Fn(Snapshot, usize) + Send + Sync>;

/// A validated handle to one installed keyed component family (plan §5.4).
/// Removal is borrowed and idempotent; ids are checked and never reused.
pub struct KeyedFamily<V: MapView> {
    pub(crate) engine_id: usize,
    pub(crate) id: u64,
    marker: std::marker::PhantomData<fn() -> V>,
}

impl<V: MapView> Clone for KeyedFamily<V> {
    fn clone(&self) -> Self {
        Self {
            engine_id: self.engine_id,
            id: self.id,
            marker: std::marker::PhantomData,
        }
    }
}

impl<V: MapView> std::fmt::Debug for KeyedFamily<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyedFamily").field("id", &self.id).finish()
    }
}

/// A reactive engine with synchronous transactional commands.
pub struct Engine {
    pub(crate) plain: Arc<Mutex<PlainRuntime>>,
    pub(crate) plain_subscriptions: Arc<Mutex<Vec<(TypeId, Subscription)>>>,
    engine_id: usize,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// Creates a synchronous deterministic engine. There is no worker
    /// parameter: committed behavior is defined by repeated identical traces,
    /// never by a scheduling knob (plan §3.2).
    pub fn new() -> Self {
        Self {
            plain: Arc::new(Mutex::new(PlainRuntime::default())),
            plain_subscriptions: Arc::new(Mutex::new(Vec::new())),
            engine_id: plain::fresh_engine_id(),
        }
    }

    fn plain_engine_id(&self) -> usize {
        self.engine_id
    }

    /// Captures one ordinary function in an isolated, scheduler-invisible
    /// computation graph.
    pub(crate) fn plan<F, A, B>(&mut self, function: F, input: A) -> Result<Planned<B>>
    where
        F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
        A: Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static,
        B: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static,
    {
        let runtime = self.plain.lock();
        let (plan, output) =
            plain::capture_plan(runtime.state.lock().clone(), runtime.epoch, function, input)?;
        Ok(Planned {
            engine_id: self.plain_engine_id(),
            token: plan.root,
            output: Arc::new(RwLock::new(output)),
            plan: Mutex::new(Some(plan)),
        })
    }

    /// Promotes an isolated plan into this engine and commits its initial
    /// quiescent state. A failed promotion leaves the plan retryable.
    pub fn run<B>(&mut self, planned: &Planned<B>) -> Result<Running<B>>
    where
        B: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static,
    {
        use crate::reactive::plain::{push_txn, rollback_txn_pub, take_txn_pub, with_txn_pub};

        if planned.engine_id != self.plain_engine_id() {
            return Err(Error::PlanForDifferentEngine);
        }

        let mut plan_guard = planned.plan.lock();
        let Some(plan) = plan_guard.as_mut() else {
            return Err(Error::PlanAlreadyRun);
        };
        let plan_output_backup = Arc::clone(&plan.output);
        let captured_backup = plan.captured_epoch;
        let planned_output_backup = Arc::clone(&planned.output.read());

        let mut runtime = self.plain.lock();
        if plan.captured_epoch != runtime.epoch {
            match plain::recapture_plan(plan, runtime.state.lock().clone(), runtime.epoch) {
                Ok(output) => {
                    *planned.output.write() =
                        Arc::new(output.as_any().downcast_ref::<B>().cloned().ok_or_else(
                            || Error::Internal("recaptured root result type mismatch".into()),
                        )?);
                }
                Err(error) => {
                    plan.output = plan_output_backup;
                    plan.captured_epoch = captured_backup;
                    *planned.output.write() = planned_output_backup;
                    return Err(error);
                }
            }
        }

        let output_cell = Arc::clone(&planned.output);
        let sink_cell = Arc::clone(&output_cell);
        let sink = Arc::new(OutputSink {
            update: Box::new(move |value| {
                if let Some(value) = value.as_any().downcast_ref::<B>() {
                    *sink_cell.write() = Arc::new(value.clone());
                }
            }),
        });

        let _txn_frame = push_txn();
        macro_rules! abort {
            ($error:expr) => {{
                let txn = take_txn_pub();
                drop(_txn_frame);
                rollback_txn_pub(txn, &runtime.state.clone(), &mut runtime.roots);
                plan.output = plan_output_backup;
                plan.captured_epoch = captured_backup;
                *planned.output.write() = Arc::clone(&planned_output_backup);
                return Err($error);
            }};
        }

        let installed = match plain::install_root(&mut runtime, plan.clone(), sink) {
            Ok(output) => output,
            Err(error) => abort!(error),
        };
        if installed.as_any().downcast_ref::<B>().is_none() {
            abort!(Error::Internal("planned root result type mismatch".into()));
        }

        let initial_changes = with_txn_pub(|txn| txn.journal.commit_changes());
        plain::initialize_dirty(&mut runtime, &initial_changes);
        if let Err(error) = plain::quiesce(&mut runtime) {
            abort!(error);
        }
        if let Some(views) = plain::dependency_cycle(&runtime) {
            abort!(Error::DependencyCycle { views });
        }

        let final_changes = with_txn_pub(|txn| txn.journal.commit_changes());
        {
            let deltas = with_txn_pub(|txn| txn.journal.commit_deltas());
            runtime.committed.apply(&deltas);
        }
        let token = planned.token;
        plan_guard.take();
        drop(_txn_frame);
        runtime.epoch = runtime.epoch.saturating_add(1);
        runtime.last_changed = final_changes.clone();
        plain::update_sinks(&runtime);
        drop(runtime);

        let snapshot = self.snapshot();
        let subscriptions = self.plain_subscriptions.lock().clone();
        let mut counts = HashMap::new();
        for change in &final_changes {
            *counts.entry(change.view).or_insert(0usize) += 1;
        }
        for (view, subscriber) in subscriptions {
            if let Some(count) = counts.get(&view).copied()
                && count != 0
            {
                subscriber(snapshot.clone(), count);
            }
        }

        Ok(Running {
            engine_id: self.plain_engine_id(),
            token,
            output: output_cell,
            removed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Removes one committed root. Removal is borrowed and idempotent.
    pub fn remove<B>(&mut self, running: &Running<B>) -> Result<()> {
        use crate::reactive::plain::{push_txn, rollback_txn_pub, take_txn_pub, with_txn_pub};

        if running.engine_id != self.plain_engine_id() {
            return Err(Error::PlanForDifferentEngine);
        }
        if running.removed.load(Ordering::Acquire) {
            return Ok(());
        }

        let mut runtime = self.plain.lock();
        let _txn_frame = push_txn();

        plain::remove_root(&mut runtime, running.token)?;

        let final_changes = with_txn_pub(|txn| txn.journal.commit_changes());
        if !final_changes.is_empty() {
            plain::initialize_dirty(&mut runtime, &final_changes);
            let quiesce_error = plain::quiesce(&mut runtime).err();
            if let Some(error) = quiesce_error {
                let txn = take_txn_pub();
                drop(_txn_frame);
                rollback_txn_pub(txn, &runtime.state.clone(), &mut runtime.roots);
                return Err(error);
            }
            let downstream = with_txn_pub(|txn| txn.journal.commit_changes());
            {
                let deltas = with_txn_pub(|txn| txn.journal.commit_deltas());
                runtime.committed.apply(&deltas);
            }
            let _txn = take_txn_pub();
            drop(_txn_frame);
            runtime.epoch = runtime.epoch.saturating_add(1);
            runtime.last_changed = downstream.clone();
            plain::update_sinks(&runtime);
            drop(runtime);

            let snapshot = self.snapshot();
            let subscriptions = self.plain_subscriptions.lock().clone();
            let mut counts = HashMap::new();
            for change in &downstream {
                *counts.entry(change.view).or_insert(0usize) += 1;
            }
            for (view, subscriber) in subscriptions {
                if let Some(count) = counts.get(&view).copied()
                    && count != 0
                {
                    subscriber(snapshot.clone(), count);
                }
            }
        } else {
            drop(_txn_frame);
        }
        running.removed.store(true, Ordering::Release);
        Ok(())
    }

    /// Runs one external write-only effect closure as one atomic epoch.
    pub fn command<F>(&mut self, effects: F) -> Result<CommandReport>
    where
        F: FnOnce() -> Result<()>,
    {
        let report = plain::run_command(&mut self.plain.lock(), effects)?;
        let mut plain_changed = HashMap::new();
        for change in &report.changes {
            *plain_changed.entry(change.view).or_insert(0usize) += 1;
        }
        let mut engine = report.engine;
        engine.facts_changed = report.changes.len() as u64;
        engine.facts_touched = engine.fact_writes + engine.fact_retractions;
        let report = CommandReport {
            epoch: report.epoch,
            rounds: report.rounds,
            plain_changed,
            engine,
            invocations: report.invocation_work,
            metrics: report.metrics,
        };
        let snapshot = self.snapshot();
        let subscriptions = self.plain_subscriptions.lock().clone();
        for (view, subscriber) in subscriptions {
            if let Some(count) = report.plain_changed.get(&view).copied()
                && count != 0
            {
                subscriber(snapshot.clone(), count);
            }
        }
        Ok(report)
    }

    /// Installs a keyed component family over one map view (plan §5.4).
    ///
    /// The function runs once per key, directly scheduled by that key's
    /// changes; no discovery root enumerates the view afterwards. Existing
    /// keys evaluate once during installation.
    ///
    /// Crate-private since Cut C: authored components install through the
    /// generated marker-registered installer
    /// ([`Self::install_component_each_key`]); this ordinal path remains
    /// only for framework-internal and transaction-test use.
    pub(crate) fn install_keyed<V, F>(&mut self, function: F) -> Result<KeyedFamily<V>>
    where
        V: MapView,
        F: Fn(V::Input) -> Result<()> + Clone + Send + Sync + 'static,
    {
        use crate::reactive::plain::ErasedCall;
        use crate::reactive::plain::{
            PlainStatePub as PlainState, erased_call, push_txn_pub, rollback_txn_pub, take_txn_pub,
            with_txn_pub,
        };

        let mut runtime = self.plain.lock();
        let _txn_frame = push_txn_pub();
        let result = (|| -> Result<KeyedFamily<V>> {
            let root = plain::fresh_token();
            let graph = Arc::new(Mutex::new(plain::PlainGraph::new(
                PlainState::default(),
                plain::erased_noop_pub(),
                root,
            )));
            // Keyed children read/write the shared committed store.
            graph.lock().state = Arc::clone(&runtime.state);
            let view = TypeId::of::<V>();
            let build_call: Arc<dyn Fn(Arc<dyn KeyValue>) -> Arc<dyn ErasedCall> + Send + Sync> = {
                let function = function.clone();
                Arc::new(move |key| {
                    let input = key
                        .as_any()
                        .downcast_ref::<V::Input>()
                        .cloned()
                        .unwrap_or_else(|| {
                            panic!("keyed family received an untyped key");
                        });
                    erased_call(function.clone(), input)
                })
            };
            let install_ordinal = runtime.next_install_ordinal;
            runtime.next_install_ordinal += 1;
            let family_id = root;
            runtime.families.insert(
                family_id,
                plain::FamilyRuntime {
                    graph: Arc::clone(&graph),
                    view,
                    view_name: V::name(),
                    install_ordinal,
                    selector: plain::FamilySelector::MapEntry,
                    accept_key: Arc::new(|_| true),
                    build_call,
                    definition: None,
                },
            );
            runtime.family_by_root.insert(root, family_id);
            with_txn_pub(|txn| {
                txn.push_undo(plain::Undo::RootInserted { root });
            });

            // Initial enumeration in fact-ordinal order.
            let initial_keys: Vec<Arc<dyn KeyValue>> = runtime
                .committed
                .view(view)
                .map(|snapshot| {
                    snapshot
                        .entries()
                        .map(|entry| Arc::clone(&entry.key))
                        .collect()
                })
                .unwrap_or_default();
            for key in initial_keys {
                plain::queue_family_child(&mut runtime, family_id, key)?;
            }
            if let Err(error) = plain::quiesce(&mut runtime) {
                return Err(error);
            }

            Ok(KeyedFamily {
                engine_id: self.plain_engine_id(),
                id: family_id,
                marker: std::marker::PhantomData,
            })
        })();

        match result {
            Ok(family) => {
                use crate::reactive::plain::with_txn_pub;
                let deltas = with_txn_pub(|txn| txn.journal.commit_deltas());
                let changes = with_txn_pub(|txn| txn.journal.commit_changes());
                drop(_txn_frame);
                if !deltas.is_empty() {
                    runtime.committed.apply(&deltas);
                    runtime.epoch += 1;
                    runtime.last_changed = changes;
                    plain::update_sinks(&runtime);
                }
                Ok(family)
            }
            Err(error) => {
                let txn = take_txn_pub();
                drop(_txn_frame);
                rollback_txn_pub(txn, &runtime.state.clone(), &mut runtime.roots);
                Err(error)
            }
        }
    }

    /// Installs one keyed component family over one map view. Framework
    /// seam: application components mount through the generated
    /// `Component::mount` (plan §7 — raw installers are not the authoring
    /// path).
    ///
    /// The definition marker rejects a second installer for the same
    /// component before anything mutates; children take the marker as their
    /// identity type so scheduling, retirement, and reaction attribution
    /// derive from the authored definition and the exact driving element.
    #[doc(hidden)]
    pub fn install_component_each_key<D, V, F>(&mut self, body: F) -> Result<KeyedFamily<V>>
    where
        D: crate::reactive::component::ComponentDefinition + 'static,
        V: MapView,
        F: Fn(V::Input) -> Result<()> + Clone + Send + Sync + 'static,
    {
        use crate::reactive::plain::ErasedCall;
        use crate::reactive::plain::{
            PlainStatePub as PlainState, erased_call_with_definition, push_txn_pub,
            rollback_txn_pub, take_txn_pub, with_txn_pub,
        };

        let definition = TypeId::of::<D>();
        let descriptor = D::__descriptor();
        {
            let mut runtime = self.plain.lock();
            runtime
                .components
                .register(definition, descriptor, "each_key")?;
        }

        let mut runtime = self.plain.lock();
        let _txn_frame = push_txn_pub();
        let result = (|| -> Result<KeyedFamily<V>> {
            let root = plain::fresh_token();
            let graph = Arc::new(Mutex::new(plain::PlainGraph::new(
                PlainState::default(),
                plain::erased_noop_pub(),
                root,
            )));
            graph.lock().state = Arc::clone(&runtime.state);
            let view = TypeId::of::<V>();
            let build_call: Arc<dyn Fn(Arc<dyn KeyValue>) -> Arc<dyn ErasedCall> + Send + Sync> = {
                let body = body.clone();
                Arc::new(move |key| {
                    let input = key
                        .as_any()
                        .downcast_ref::<V::Input>()
                        .cloned()
                        .expect("keyed component received an untyped key");
                    erased_call_with_definition(definition, body.clone(), input)
                })
            };
            let install_ordinal = runtime.next_install_ordinal;
            runtime.next_install_ordinal += 1;
            let family_id = root;
            runtime.families.insert(
                family_id,
                plain::FamilyRuntime {
                    graph: Arc::clone(&graph),
                    view,
                    view_name: descriptor,
                    install_ordinal,
                    selector: plain::FamilySelector::MapEntry,
                    accept_key: Arc::new(|_| true),
                    build_call,
                    definition: Some((descriptor, definition)),
                },
            );
            runtime.family_by_root.insert(root, family_id);
            with_txn_pub(|txn| {
                txn.push_undo(plain::Undo::RootInserted { root });
            });

            // Initial enumeration in fact-ordinal order.
            let initial_keys: Vec<Arc<dyn KeyValue>> = runtime
                .committed
                .view(view)
                .map(|snapshot| {
                    snapshot
                        .entries()
                        .map(|entry| Arc::clone(&entry.key))
                        .collect()
                })
                .unwrap_or_default();
            for key in initial_keys {
                plain::queue_family_child(&mut runtime, family_id, key)?;
            }
            if let Err(error) = plain::quiesce(&mut runtime) {
                return Err(error);
            }

            Ok(KeyedFamily {
                engine_id: self.plain_engine_id(),
                id: family_id,
                marker: std::marker::PhantomData,
            })
        })();

        match result {
            Ok(family) => {
                let deltas = with_txn_pub(|txn| txn.journal.commit_deltas());
                let changes = with_txn_pub(|txn| txn.journal.commit_changes());
                drop(_txn_frame);
                if !deltas.is_empty() {
                    runtime.committed.apply(&deltas);
                    runtime.epoch += 1;
                    runtime.last_changed = changes;
                    plain::update_sinks(&runtime);
                }
                Ok(family)
            }
            Err(error) => {
                let txn = take_txn_pub();
                drop(_txn_frame);
                rollback_txn_pub(txn, &runtime.state.clone(), &mut runtime.roots);
                Err(error)
            }
        }
    }
    /// Installs a keyed component whose returned value is a desired ordinary
    /// effect. The effect is applied inside the component invocation, so the
    /// invocation owns exactly the outputs returned by its latest evaluation.
    #[doc(hidden)]
    pub fn install_component_each_key_effect<D, V, B, F>(
        &mut self,
        body: F,
    ) -> Result<KeyedFamily<V>>
    where
        D: crate::reactive::component::ComponentDefinition + 'static,
        V: MapView,
        B: crate::reactive::component::Effects
            + Clone
            + PartialEq
            + std::fmt::Debug
            + Send
            + Sync
            + 'static,
        F: Fn(V::Input) -> Result<B> + Clone + Send + Sync + 'static,
    {
        let effective = move |input: V::Input| {
            let output = body(input)?;
            output.__apply()
        };
        self.install_component_each_key::<D, V, _>(effective)
    }

    /// Installs one first-class component for every committed root link in a
    /// generated abstract-tree family.
    pub fn install_component_tree_roots<D, F, N, B, Body>(
        &mut self,
        _selector: crate::reactive::abstract_tree::RootSelector<F, N>,
        body: Body,
    ) -> Result<()>
    where
        D: crate::reactive::component::ComponentDefinition + 'static,
        F: crate::reactive::abstract_tree::AbstractTreeFamily,
        N: crate::reactive::abstract_tree::AbstractTreeNode<Family = F>,
        B: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static,
        Body: Fn(F::Domain, crate::reactive::abstract_tree::AstBox<N>) -> Result<B>
            + Clone
            + Send
            + Sync
            + 'static,
    {
        use crate::reactive::plain::ErasedCall;
        use crate::reactive::plain::{
            PlainStatePub as PlainState, erased_call_with_definition, push_txn_pub,
            rollback_txn_pub, take_txn_pub, with_txn_pub,
        };
        let definition = TypeId::of::<D>();
        let descriptor = D::__descriptor();
        {
            let mut runtime = self.plain.lock();
            runtime
                .components
                .register(definition, descriptor, "tree_root")?;
        }
        let mut runtime = self.plain.lock();
        let _txn_frame = push_txn_pub();
        let result = (|| -> Result<()> {
            let root = plain::fresh_token();
            let graph = Arc::new(Mutex::new(plain::PlainGraph::new(
                PlainState::default(),
                plain::erased_noop_pub(),
                root,
            )));
            graph.lock().state = Arc::clone(&runtime.state);
            let view = TypeId::of::<F>();
            let build_call: Arc<dyn Fn(Arc<dyn KeyValue>) -> Arc<dyn ErasedCall> + Send + Sync> = {
                let body = body.clone();
                Arc::new(move |key| {
                    let input = key
                        .as_any()
                        .downcast_ref::<crate::reactive::abstract_tree::TreeKey<F::Domain>>()
                        .and_then(|key| match key {
                            crate::reactive::abstract_tree::TreeKey::RootLink(domain, node) => {
                                Some((
                                    domain.clone(),
                                    crate::reactive::abstract_tree::AstBox::<N>::from_erased(
                                        node.clone(),
                                    ),
                                ))
                            }
                            _ => None,
                        })
                        .expect("tree component received a non-root key");
                    let invoke = {
                        let body = body.clone();
                        move |(domain, node): (
                            F::Domain,
                            crate::reactive::abstract_tree::AstBox<N>,
                        )| { body(domain, node) }
                    };
                    erased_call_with_definition(definition, invoke, input)
                })
            };
            let install_ordinal = runtime.next_install_ordinal;
            runtime.next_install_ordinal += 1;
            let family_id = root;
            runtime.families.insert(
                family_id,
                plain::FamilyRuntime {
                    graph: Arc::clone(&graph),
                    view,
                    view_name: descriptor,
                    install_ordinal,
                    selector: plain::FamilySelector::TreeRoot,
                    accept_key: Arc::new(|key| {
                        key.as_any()
                            .downcast_ref::<crate::reactive::abstract_tree::TreeKey<F::Domain>>()
                            .is_some_and(|key| {
                                matches!(
                                    key,
                                    crate::reactive::abstract_tree::TreeKey::RootLink(_, _)
                                )
                            })
                    }),
                    build_call,
                    definition: Some((descriptor, definition)),
                },
            );
            runtime.family_by_root.insert(family_id, family_id);
            with_txn_pub(|txn| txn.push_undo(plain::Undo::RootInserted { root }));
            let initial_keys: Vec<Arc<dyn KeyValue>> = runtime
                .committed
                .view(view)
                .map(|snapshot| {
                    snapshot
                        .entries()
                        .filter_map(|entry| {
                            entry
                                .key
                                .as_any()
                                .downcast_ref::<crate::reactive::abstract_tree::TreeKey<F::Domain>>(
                                )
                                .and_then(|key| {
                                    matches!(
                                        key,
                                        crate::reactive::abstract_tree::TreeKey::RootLink(_, _)
                                    )
                                    .then(|| Arc::clone(&entry.key))
                                })
                        })
                        .collect()
                })
                .unwrap_or_default();
            for key in initial_keys {
                plain::queue_family_child(&mut runtime, family_id, key)?;
            }
            plain::quiesce(&mut runtime)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                let deltas = with_txn_pub(|txn| txn.journal.commit_deltas());
                let changes = with_txn_pub(|txn| txn.journal.commit_changes());
                drop(_txn_frame);
                if !deltas.is_empty() {
                    runtime.committed.apply(&deltas);
                    runtime.epoch += 1;
                    runtime.last_changed = changes;
                    plain::update_sinks(&runtime);
                }
                Ok(())
            }
            Err(error) => {
                let txn = take_txn_pub();
                drop(_txn_frame);
                rollback_txn_pub(txn, &runtime.state.clone(), &mut runtime.roots);
                Err(error)
            }
        }
    }

    /// Installs one heterogeneous component at each selected root. The
    /// component's normalized family key is stable; `props` are copied into
    /// each root invocation and remain replaceable call values rather than
    /// lifecycle identity.
    #[doc(hidden)]
    pub fn install_component_tree_family_roots<D, F, N, P, B, Body>(
        &mut self,
        _selector: crate::reactive::abstract_tree::RootSelector<F, N>,
        props: P,
        body: Body,
    ) -> Result<()>
    where
        D: crate::reactive::component::ComponentDefinition + 'static,
        F: crate::reactive::abstract_tree::AbstractTreeFamily,
        N: crate::reactive::abstract_tree::AbstractTreeNode<Family = F>,
        P: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static,
        B: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static,
        Body: Fn(crate::reactive::component::FamilyNode<F>, P) -> Result<B>
            + Clone
            + Send
            + Sync
            + 'static,
    {
        use crate::reactive::plain::ErasedCall;
        use crate::reactive::plain::{
            PlainStatePub as PlainState, erased_component_call_with_definition, push_txn_pub,
            rollback_txn_pub, take_txn_pub, with_txn_pub,
        };
        let definition = TypeId::of::<D>();
        let descriptor = D::__descriptor();
        {
            let mut runtime = self.plain.lock();
            runtime
                .components
                .register(definition, descriptor, "tree_family_root")?;
        }
        let mut runtime = self.plain.lock();
        let _txn_frame = push_txn_pub();
        let result = (|| -> Result<()> {
            let root = plain::fresh_token();
            let graph = Arc::new(Mutex::new(plain::PlainGraph::new(
                PlainState::default(),
                plain::erased_noop_pub(),
                root,
            )));
            graph.lock().state = Arc::clone(&runtime.state);
            let view = TypeId::of::<F>();
            let build_call: Arc<dyn Fn(Arc<dyn KeyValue>) -> Arc<dyn ErasedCall> + Send + Sync> = {
                let body = body.clone();
                let props = props.clone();
                Arc::new(move |key| {
                    let node = key
                        .as_any()
                        .downcast_ref::<crate::reactive::abstract_tree::TreeKey<F::Domain>>()
                        .and_then(|key| match key {
                            crate::reactive::abstract_tree::TreeKey::RootLink(_, node) => {
                                Some(crate::reactive::abstract_tree::AstBox::<N>::from_erased(
                                    node.clone(),
                                ))
                            }
                            _ => None,
                        })
                        .expect("heterogeneous component received a non-root key");
                    let family_node = crate::reactive::component::FamilyNode::<F>::from_typed(node);
                    erased_component_call_with_definition(
                        definition,
                        descriptor,
                        Some(N::__member()),
                        body.clone(),
                        family_node,
                        props.clone(),
                    )
                })
            };
            let install_ordinal = runtime.next_install_ordinal;
            runtime.next_install_ordinal += 1;
            let family_id = root;
            runtime.families.insert(
                family_id,
                plain::FamilyRuntime {
                    graph: Arc::clone(&graph),
                    view,
                    view_name: descriptor,
                    install_ordinal,
                    selector: plain::FamilySelector::TreeRoot,
                    accept_key: Arc::new(|key| {
                        key.as_any()
                            .downcast_ref::<crate::reactive::abstract_tree::TreeKey<F::Domain>>()
                            .is_some_and(|key| {
                                matches!(
                                    key,
                                    crate::reactive::abstract_tree::TreeKey::RootLink(_, _)
                                )
                            })
                    }),
                    build_call,
                    definition: Some((descriptor, definition)),
                },
            );
            runtime.family_by_root.insert(family_id, family_id);
            with_txn_pub(|txn| txn.push_undo(plain::Undo::RootInserted { root }));
            let initial_keys: Vec<Arc<dyn KeyValue>> = runtime
                .committed
                .view(view)
                .map(|snapshot| {
                    snapshot
                        .entries()
                        .filter_map(|entry| {
                            entry
                                .key
                                .as_any()
                                .downcast_ref::<crate::reactive::abstract_tree::TreeKey<F::Domain>>()
                                .is_some_and(|key| {
                                    matches!(
                                        key,
                                        crate::reactive::abstract_tree::TreeKey::RootLink(_, _)
                                    )
                                })
                                .then(|| Arc::clone(&entry.key))
                        })
                        .collect()
                })
                .unwrap_or_default();
            for key in initial_keys {
                plain::queue_family_child(&mut runtime, family_id, key)?;
            }
            plain::quiesce(&mut runtime)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                let deltas = with_txn_pub(|txn| txn.journal.commit_deltas());
                let changes = with_txn_pub(|txn| txn.journal.commit_changes());
                drop(_txn_frame);
                if !deltas.is_empty() {
                    runtime.committed.apply(&deltas);
                    runtime.epoch += 1;
                    runtime.last_changed = changes;
                    plain::update_sinks(&runtime);
                }
                Ok(())
            }
            Err(error) => {
                let txn = take_txn_pub();
                drop(_txn_frame);
                rollback_txn_pub(txn, &runtime.state.clone(), &mut runtime.roots);
                Err(error)
            }
        }
    }

    /// Installs one first-class component for every committed node of the
    /// selected member in a generated abstract-tree family.
    pub fn install_component_tree_nodes<D, F, N, B, Body>(
        &mut self,
        _selector: crate::reactive::abstract_tree::NodeSelector<F, N>,
        body: Body,
    ) -> Result<()>
    where
        D: crate::reactive::component::ComponentDefinition + 'static,
        F: crate::reactive::abstract_tree::AbstractTreeFamily,
        N: crate::reactive::abstract_tree::AbstractTreeNode<Family = F>,
        B: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static,
        Body: Fn(crate::reactive::abstract_tree::AstBox<N>) -> Result<B>
            + Clone
            + Send
            + Sync
            + 'static,
    {
        use crate::reactive::plain::ErasedCall;
        use crate::reactive::plain::{
            PlainStatePub as PlainState, erased_call_with_definition, push_txn_pub,
            rollback_txn_pub, take_txn_pub, with_txn_pub,
        };
        let definition = TypeId::of::<D>();
        let descriptor = D::__descriptor();
        {
            let mut runtime = self.plain.lock();
            runtime
                .components
                .register(definition, descriptor, "tree_node")?;
        }
        let mut runtime = self.plain.lock();
        let _txn_frame = push_txn_pub();
        let result = (|| -> Result<()> {
            let root = plain::fresh_token();
            let graph = Arc::new(Mutex::new(plain::PlainGraph::new(
                PlainState::default(),
                plain::erased_noop_pub(),
                root,
            )));
            graph.lock().state = Arc::clone(&runtime.state);
            let view = TypeId::of::<F>();
            let member = N::__member();
            let build_call: Arc<dyn Fn(Arc<dyn KeyValue>) -> Arc<dyn ErasedCall> + Send + Sync> = {
                let body = body.clone();
                Arc::new(move |key| {
                    let input = key
                        .as_any()
                        .downcast_ref::<crate::reactive::abstract_tree::TreeKey<F::Domain>>()
                        .and_then(|key| match key {
                            crate::reactive::abstract_tree::TreeKey::Member(node, actual)
                                if *actual == member =>
                            {
                                Some(crate::reactive::abstract_tree::AstBox::<N>::from_erased(
                                    node.clone(),
                                ))
                            }
                            _ => None,
                        })
                        .expect("tree component received a non-selected member key");
                    erased_call_with_definition(definition, body.clone(), input)
                })
            };
            let install_ordinal = runtime.next_install_ordinal;
            runtime.next_install_ordinal += 1;
            let family_id = root;
            runtime.families.insert(
                family_id,
                plain::FamilyRuntime {
                    graph: Arc::clone(&graph),
                    view,
                    view_name: descriptor,
                    install_ordinal,
                    selector: plain::FamilySelector::TreeNode(member),
                    accept_key: Arc::new(move |key| {
                        key.as_any()
                            .downcast_ref::<crate::reactive::abstract_tree::TreeKey<F::Domain>>()
                            .is_some_and(|key| {
                                matches!(
                                    key,
                                    crate::reactive::abstract_tree::TreeKey::Member(_, actual)
                                        if *actual == member
                                )
                            })
                    }),
                    build_call,
                    definition: Some((descriptor, definition)),
                },
            );
            runtime.family_by_root.insert(family_id, family_id);
            with_txn_pub(|txn| txn.push_undo(plain::Undo::RootInserted { root }));
            let initial_keys: Vec<Arc<dyn KeyValue>> = runtime
                .committed
                .view(view)
                .map(|snapshot| {
                    snapshot
                        .entries()
                        .filter_map(|entry| {
                            entry
                                .key
                                .as_any()
                                .downcast_ref::<crate::reactive::abstract_tree::TreeKey<F::Domain>>(
                                )
                                .and_then(|key| {
                                    matches!(
                                        key,
                                        crate::reactive::abstract_tree::TreeKey::Member(
                                            _,
                                            actual
                                        ) if *actual == member
                                    )
                                    .then(|| Arc::clone(&entry.key))
                                })
                        })
                        .collect()
                })
                .unwrap_or_default();
            for key in initial_keys {
                plain::queue_family_child(&mut runtime, family_id, key)?;
            }
            plain::quiesce(&mut runtime)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                let deltas = with_txn_pub(|txn| txn.journal.commit_deltas());
                let changes = with_txn_pub(|txn| txn.journal.commit_changes());
                drop(_txn_frame);
                if !deltas.is_empty() {
                    runtime.committed.apply(&deltas);
                    runtime.epoch += 1;
                    runtime.last_changed = changes;
                    plain::update_sinks(&runtime);
                }
                Ok(())
            }
            Err(error) => {
                let txn = take_txn_pub();
                drop(_txn_frame);
                rollback_txn_pub(txn, &runtime.state.clone(), &mut runtime.roots);
                Err(error)
            }
        }
    }

    /// Removes a keyed family: every child retires in ordinal order and all
    /// replace/patch-owned publications retract through the journal.
    pub fn remove_keyed<V: MapView>(&mut self, family: &KeyedFamily<V>) -> Result<()> {
        use crate::reactive::plain::{push_txn_pub, rollback_txn_pub, take_txn_pub};
        if family.engine_id != self.plain_engine_id() {
            return Err(Error::PlanForDifferentEngine);
        }
        let mut runtime = self.plain.lock();
        if !runtime.families.contains_key(&family.id) {
            return Ok(()); // idempotent removal
        }
        let _txn_frame = push_txn_pub();
        let outcome = (|| -> Result<()> {
            let family_runtime = runtime.families.get(&family.id).expect("family present");
            let graph = Arc::clone(&family_runtime.graph);
            let ids: Vec<u64> = {
                let graph_guard = graph.lock();
                let mut ids = graph_guard.live_child_ids();
                ids.sort_unstable();
                ids
            };
            for id in ids {
                plain::retract_child_owned(&graph, &runtime.state, id)?;
            }
            runtime.families.remove(&family.id);
            runtime.family_by_root.remove(&graph.lock().root);
            Ok(())
        })();
        match outcome {
            Ok(()) => {
                use crate::reactive::plain::with_txn_pub;
                let deltas = with_txn_pub(|txn| txn.journal.commit_deltas());
                let changes = with_txn_pub(|txn| txn.journal.commit_changes());
                drop(_txn_frame);
                if !deltas.is_empty() {
                    runtime.committed.apply(&deltas);
                    runtime.epoch += 1;
                    runtime.last_changed = changes;
                    plain::update_sinks(&runtime);
                }
                Ok(())
            }
            Err(error) => {
                let txn = take_txn_pub();
                drop(_txn_frame);
                rollback_txn_pub(txn, &runtime.state.clone(), &mut runtime.roots);
                Err(error)
            }
        }
    }

    /// Subscribes to committed changes of one typed view.
    pub fn subscribe<V: View>(
        &mut self,
        subscriber: impl Fn(Snapshot, usize) + Send + Sync + 'static,
    ) -> Result<()> {
        self.plain_subscriptions
            .lock()
            .push((TypeId::of::<V>(), Arc::new(subscriber)));
        Ok(())
    }

    /// Returns a read-only committed snapshot.
    pub fn snapshot(&self) -> Snapshot {
        let runtime = self.plain.lock();
        Snapshot {
            plain: Arc::new(plain::snapshot(&runtime)),
        }
    }

    /// Debug/test liveness audit over the production indexes (follow-up
    /// plan §4 item 12): tree/ownership/dependency/bijection consistency.
    /// Read-only; returns one row per violated invariant.
    #[doc(hidden)]
    pub fn __liveness_audit(&self) -> Vec<String> {
        plain::liveness_audit(&self.plain.lock())
    }
}

/// A read-only view of committed typed facts.
#[derive(Clone)]
pub struct Snapshot {
    pub(crate) plain: Arc<plain::PlainSnapshot>,
}

impl Snapshot {
    /// Reads one committed fact.
    pub fn observe<V: View>(&self, input: V::Input) -> Option<Arc<V::Output>> {
        V::__snapshot(self, input)
    }

    /// Reads every committed input key of one typed view.
    pub fn inputs<V: View>(&self) -> Vec<V::Input> {
        V::__snapshot_inputs(self)
    }

    #[doc(hidden)]
    pub fn __plain_observe<V: View>(&self, input: V::Input) -> Option<Arc<V::Output>> {
        self.plain.observe::<V>(input)
    }

    #[doc(hidden)]
    pub fn __plain_inputs<V: View>(&self) -> Vec<V::Input> {
        self.plain.inputs::<V>()
    }

    /// Total committed fact entries across every view: the live
    /// persistent-bytes proxy (plan §20.6). Diffing two snapshots taken
    /// around one command shows retention growth, not just churn.
    #[doc(hidden)]
    pub fn live_fact_count(&self) -> u64 {
        self.plain.live_fact_count()
    }

    #[doc(hidden)]
    pub fn __debug_view_counts(&self) -> Vec<(String, u64)> {
        self.plain.view_counts()
    }

    /// Reads a list-kind view's committed length under one domain key.
    pub fn list_len<V: ListView>(&self, key: &V::Key) -> usize {
        match self
            .__plain_observe::<V>(ListKey::Len(key.clone()))
            .as_deref()
        {
            Some(ListFact::Len(len)) => *len as usize,
            _ => 0,
        }
    }

    /// Reads all present list slots under one semantic domain key.
    pub fn list<V: ListView>(&self, key: &V::Key) -> Vec<Arc<V::Item>> {
        let len = self.list_len::<V>(key);
        let mut items = Vec::with_capacity(len);
        for index in 0..len {
            if let Some(ListFact::Item(item)) = self
                .__plain_observe::<V>(ListKey::Slot(key.clone(), index as u32))
                .as_deref()
            {
                items.push(Arc::new(item.clone()));
            }
        }
        items
    }

    /// Enumerates list domains without exposing the encoded list-key ABI.
    pub fn list_domains<V: ListView>(&self) -> Vec<V::Key> {
        let mut domains = Vec::new();
        for input in self.__plain_inputs::<V>() {
            let key = match input {
                ListKey::Slot(key, _) | ListKey::Len(key) => key,
            };
            if !domains.iter().any(|existing| existing == &key) {
                domains.push(key);
            }
        }
        domains
    }

    /// Reads a tree-kind view's committed roots across all domain keys.
    pub fn tree_roots<V: TreeView>(&self) -> Vec<Node<V>> {
        let mut roots = Vec::new();
        for input in self.__plain_inputs::<V>() {
            if let TreeKey::RootOrder(key) = &input
                && let Some(observed) = self.__plain_observe::<V>(input.clone())
            {
                if let TreeFact::RootOrder(order) = observed.as_ref() {
                    for link in order.iter() {
                        if let Some(observed) =
                            self.__plain_observe::<V>(TreeKey::RootLink(key.clone(), link.clone()))
                            && let TreeFact::RootLink(root) = observed.as_ref()
                        {
                            roots.push(root.clone());
                        }
                    }
                }
            }
        }
        roots
    }

    /// Reads a tree-kind view's committed root list under one domain key.
    pub fn tree_roots_of<V: TreeView>(&self, key: &V::Key) -> Vec<Node<V>> {
        let Some(observed) = self.__plain_observe::<V>(TreeKey::RootOrder(key.clone())) else {
            return Vec::new();
        };
        let TreeFact::RootOrder(order) = observed.as_ref() else {
            return Vec::new();
        };
        let mut roots = Vec::with_capacity(order.len());
        for link in order.iter() {
            if let Some(observed) =
                self.__plain_observe::<V>(TreeKey::RootLink(key.clone(), link.clone()))
                && let TreeFact::RootLink(root) = observed.as_ref()
            {
                roots.push(root.clone());
            }
        }
        roots
    }

    /// Reads one committed tree node's payload.
    pub fn tree_payload<V: TreeView>(&self, id: Node<V>) -> Option<Arc<V::Payload>> {
        let observed = self.__plain_observe::<V>(TreeKey::Payload(id))?;
        match observed.as_ref() {
            TreeFact::Payload(payload) => Some(Arc::new(payload.clone())),
            _ => None,
        }
    }

    /// Reads one committed tree node's parent.
    pub fn tree_parent<V: TreeView>(&self, id: Node<V>) -> Option<Node<V>> {
        let observed = self.__plain_observe::<V>(TreeKey::Parent(id))?;
        match observed.as_ref() {
            TreeFact::Parent(parent) => parent.clone(),
            _ => None,
        }
    }

    /// Reads one committed tree node's ordered children.
    pub fn tree_children<V: TreeView>(&self, id: Node<V>) -> Vec<Node<V>> {
        let Some(observed) = self.__plain_observe::<V>(TreeKey::ChildOrder(id.clone())) else {
            return Vec::new();
        };
        let TreeFact::Order(order) = observed.as_ref() else {
            return Vec::new();
        };
        let mut children = Vec::with_capacity(order.len());
        for link in order.iter() {
            if let Some(observed) =
                self.__plain_observe::<V>(TreeKey::ChildLink(id.clone(), link.clone()))
                && let TreeFact::Link(child) = observed.as_ref()
            {
                children.push(child.clone());
            }
        }
        children
    }

    /// Reads one committed graph node's payload.
    pub fn graph_node<V: GraphView>(&self, id: Node<V>) -> Option<Arc<V::NodePayload>> {
        match self.__plain_observe::<V>(GraphKey::Node(id)).as_deref() {
            Some(GraphFact::Node(payload)) => Some(Arc::new(payload.clone())),
            _ => None,
        }
    }

    /// Reads one committed labelled edge bucket.
    pub fn outgoing<V: GraphView>(&self, from: Node<V>, label: &V::Label) -> Vec<Node<V>> {
        match self
            .__plain_observe::<V>(GraphKey::Bucket(from, label.clone()))
            .as_deref()
        {
            Some(GraphFact::Targets(targets)) => targets.clone(),
            _ => Vec::new(),
        }
    }
    /// Enumerates committed graph nodes without exposing encoded graph keys.
    pub fn graph_nodes<V: GraphView>(&self) -> Vec<Node<V>> {
        self.__plain_inputs::<V>()
            .into_iter()
            .filter_map(|input| match input {
                GraphKey::Node(node) => Some(node),
                GraphKey::Bucket(_, _) => None,
            })
            .collect()
    }

    /// Enumerates committed labelled graph buckets as semantic entries.
    ///
    /// Each entry owns the bucket's target vector because snapshot inspection
    /// is intentionally detached from the reactive read context.
    pub fn graph_buckets<V: GraphView>(&self) -> Vec<(Node<V>, V::Label, Vec<Node<V>>)> {
        self.__plain_inputs::<V>()
            .into_iter()
            .filter_map(|input| {
                let GraphKey::Bucket(node, label) = input else {
                    return None;
                };
                let targets = match self
                    .__plain_observe::<V>(GraphKey::Bucket(node.clone(), label.clone()))
                    .as_deref()
                {
                    Some(GraphFact::Targets(targets)) => targets.clone(),
                    _ => Vec::new(),
                };
                Some((node, label, targets))
            })
            .collect()
    }

    /// Reads a box-kind view's committed cell.
    pub fn box_value<V: BoxView>(&self) -> Option<Arc<V::Output>> {
        self.__plain_observe::<V>(())
    }
}
