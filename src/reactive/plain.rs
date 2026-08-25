use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::{self, Debug};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::reactive::engine::{EngineWork, InvocationIdentity, InvocationWork};
use crate::reactive::error::{Error, Result};
use crate::reactive::store::{
    FactJournal, PlainState, SnapshotRoot,
};
pub(crate) use crate::reactive::store::PlainState as PlainStatePub;
use crate::reactive::value::{KeyValue, Value};
use crate::reactive::view::View;

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub(crate) const EXTERNAL_WRITER: u64 = u64::MAX;

pub(crate) fn fresh_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn fresh_engine_id() -> usize {
    fresh_token() as usize
}

// PlainFact / PlainState live in the indexed store module (plan §5.1);
// the journal below is their only mutation path inside commands.

#[derive(Clone)]
pub(crate) struct FactChange {
    pub view: TypeId,
    pub key: Arc<dyn KeyValue>,
    /// True when the fact entered or left the view, rather than merely
    /// changing its payload.
    pub presence_changed: bool,
}

// ---------------------------------------------------------------------------
// Command transaction (plan §5.3)
// ---------------------------------------------------------------------------

/// One inverse operation recorded by the running command.
pub(crate) enum Undo {
    /// Restore (or remove) one invocation's full record. `None` restores
    /// "did not exist".
    Invocation { root: u64, before: Option<Invocation> },
    /// Remove a root inserted by this command.
    RootInserted { root: u64 },
    /// Reinsert a removed root with its prior runtime.
    RootRemoved { root: u64, before: RootRuntime },
    /// Restore one invocation's private state slot.
    StateSlot {
        root: u64,
        id: u64,
        before: Option<InvocationSlot>,
    },
}

/// The journaled command transaction: fact rollback rides in the
/// [`FactJournal`]; graph/invocation/root inverses accumulate here.
#[derive(Default)]
pub(crate) struct CommandTxn {
    pub journal: FactJournal,
    undo: Vec<Undo>,
    touched_invocations: HashMap<(u64, u64, TypeId), ()>,
}

impl CommandTxn {
    /// Records the pre-mutation slot value the first time this command
    /// touches one invocation's state.
    pub(crate) fn touch_state_slot(&mut self, root: u64, graph: &PlainGraph, id: u64) {
        let key = (root, id, TypeId::of::<InvocationSlot>());
        if self.touched_invocations.contains_key(&key) {
            return;
        }
        self.touched_invocations.insert(key, ());
        let before = graph.state_slots.get(&id).cloned();
        self.undo.push(Undo::StateSlot { root, id, before });
    }

    /// Moves the whole transaction out (commit or rollback handoff).
    fn take_commands(&mut self) -> CommandTxn {
        std::mem::replace(
            self,
            CommandTxn {
                journal: FactJournal::default(),
                undo: Vec::new(),
                touched_invocations: HashMap::new(),
            },
        )
    }

    /// Records the pre-mutation state of one invocation the first time this
    /// command mutates it; later mutations reuse the same inverse.
    pub(crate) fn touch_invocation(&mut self, root: u64, graph: &PlainGraph, id: u64) {
        let key = (root, id, TypeId::of::<Invocation>());
        if self.touched_invocations.contains_key(&key) {
            return;
        }
        self.touched_invocations.insert(key, ());
        let before = graph.invocations.iter().find(|inv| inv.id == id).cloned();
        self.undo.push(Undo::Invocation { root, before });
    }

    pub(crate) fn push_undo(&mut self, undo: Undo) {
        self.undo.push(undo);
    }
}

thread_local! {
    /// The active command transaction. Commands are synchronous, so a
    /// thread-local frame is sufficient and keeps evaluation paths free of
    /// runtime plumbing.
    static ACTIVE_TXN: RefCell<Option<std::rc::Rc<std::cell::RefCell<CommandTxn>>>> =
        const { RefCell::new(None) };
    /// Pre-eval write/read sets of running evaluations, keyed by id.
    static OLD_WRITES: RefCell<Vec<(u64, Vec<PlainWrite>, Vec<ReadDep>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Pops the transaction frame on drop, including unwind paths.
pub(crate) struct TxnFrame;

impl Drop for TxnFrame {
    fn drop(&mut self) {
        ACTIVE_TXN.with(|txn| {
            txn.borrow_mut().take();
        });
    }
}

pub(crate) fn push_txn() -> TxnFrame {
    ACTIVE_TXN.with(|txn| {
        *txn.borrow_mut() = Some(std::rc::Rc::new(std::cell::RefCell::new(
            CommandTxn::default(),
        )));
    });
    TxnFrame
}

/// Runs `f` with mutable access to the active transaction. Outside an
/// active command (isolated plan capture pushes its own frame) this panics:
/// every caller sits inside a frame by construction.
#[track_caller]
fn with_txn<R>(f: impl FnOnce(&mut CommandTxn) -> R) -> R {
    ACTIVE_TXN.with(|txn| {
        let frame = txn
            .borrow()
            .as_ref()
            .unwrap_or_else(|| {
                panic!(
                    "active command txn required at {}",
                    std::panic::Location::caller()
                )
            })
            .clone();
        let mut guard = frame.borrow_mut();
        f(&mut guard)
    })
}

/// Applies the full inverse of the active transaction to the runtime:
/// journal rollback first (facts), then undo entries in reverse order.
fn rollback_txn(txn: CommandTxn, state: &Arc<Mutex<PlainState>>, roots: &mut BTreeMap<u64, RootRuntime>) {
    txn.journal.rollback(&mut state.lock());
    // Dependency rows rebuild from restored reads; the dirty queue resets.
    for undo in txn.undo.iter().rev() {
        if let Undo::Invocation { root, before } = undo {
            let Some(root_runtime) = roots.get(root) else { continue };
            let mut graph = root_runtime.graph.lock();
            match before {
                Some(before_invocation) => {
                    // Drop whatever rows the aborted evaluation installed,
                    // then reinstall the pre-command rows verbatim.
                    let current_reads: Vec<(
                        TypeId,
                        Option<Arc<dyn KeyValue>>,
                        bool,
                        bool,
                    )> = graph
                        .invocations
                        .iter()
                        .find(|inv| inv.id == before_invocation.id)
                        .map(|inv| {
                            inv.reads
                                .iter()
                                .map(|read| {
                                    (read.view, read.key.clone(), read.temporal, read.keyset)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    graph.deps.remove_all(&current_reads, before_invocation.id);
                    if !before_invocation.retired {
                        let restored: Vec<(
                            TypeId,
                            Option<Arc<dyn KeyValue>>,
                            bool,
                            bool,
                        )> = before_invocation
                            .reads
                            .iter()
                            .map(|read| {
                                (read.view, read.key.clone(), read.temporal, read.keyset)
                            })
                            .collect();
                        graph.deps.replace(&restored, &[], before_invocation.id);
                    }
                }
                None => {}
            }
        }
    }
    for undo in txn.undo.into_iter().rev() {
        match undo {
            Undo::Invocation { root, before } => {
                if let Some(root_runtime) = roots.get(&root) {
                    let mut graph = root_runtime.graph.lock();
                    match before {
                        Some(before) => {
                            if let Some(slot) = graph
                                .invocations
                                .iter_mut()
                                .find(|inv| inv.id == before.id)
                            {
                                *slot = before;
                            } else {
                                graph.invocations.push(before);
                            }
                        }
                        None => {
                            // Creation undone: remove the newest matching id.
                            if let Some(position) = graph
                                .invocations
                                .iter()
                                .rposition(|inv| true)
                            {
                                graph.invocations.remove(position);
                            }
                        }
                    }
                }
            }
            Undo::RootInserted { root } => {
                roots.remove(&root);
            }
            Undo::RootRemoved { root, before } => {
                roots.insert(root, before);
            }
            Undo::StateSlot { root, id, before } => {
                if let Some(root_runtime) = roots.get_mut(&root) {
                    let mut graph = root_runtime.graph.lock();
                    match before {
                        Some(slot) => {
                            graph.state_slots.insert(id, slot);
                        }
                        None => {
                            graph.state_slots.remove(&id);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct ReadDep {
    view: TypeId,
    name: &'static str,
    key: Option<Arc<dyn KeyValue>>,
    temporal: bool,
    /// A domain/keyset read wakes only on fact insertion or removal.
    keyset: bool,
}

impl ReadDep {
    fn matches(&self, change: &FactChange, temporal: bool) -> bool {
        if self.temporal != temporal || self.view != change.view {
            return false;
        }
        if self.keyset && !change.presence_changed {
            return false;
        }
        self.key
            .as_ref()
            .is_none_or(|key| key.eq_value(change.key.as_ref()))
    }
}

#[derive(Clone)]
struct PlainWrite {
    view: TypeId,
    name: &'static str,
    /// Stable view-type name used only for canonical commit ordering.
    view_name: &'static str,
    key: Arc<dyn KeyValue>,
    value: Option<Arc<dyn Value>>,
    shareable: bool,
}

#[derive(Clone)]
struct CallIdentity {
    file: &'static str,
    line: u32,
    column: u32,
    function: TypeId,
    input: Arc<dyn KeyValue>,
    occurrence: u64,
    /// Keyed relationships identify children by their semantic input rather
    /// than by their position among sibling `run` calls.
    stable_input: bool,
}

impl CallIdentity {
    fn same(&self, other: &Self) -> bool {
        self.file == other.file
            && self.line == other.line
            && self.column == other.column
            && self.function == other.function
            && self.input.eq_value(other.input.as_ref())
            && (self.stable_input == other.stable_input
                && (self.stable_input || self.occurrence == other.occurrence))
    }
}

pub(crate) trait ErasedCall: Send + Sync {
    fn invoke(&self) -> Result<Arc<dyn Value>>;
    fn function_type(&self) -> TypeId;
    fn input_key(&self) -> Arc<dyn KeyValue>;
    fn function_name(&self) -> &'static str;
}

struct TypedCall<F, A, B> {
    function: F,
    input: A,
    _marker: std::marker::PhantomData<fn() -> B>,
}

impl<F, A, B> ErasedCall for TypedCall<F, A, B>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    fn invoke(&self) -> Result<Arc<dyn Value>> {
        Ok(Arc::new((self.function)(self.input.clone())?))
    }

    fn function_type(&self) -> TypeId {
        TypeId::of::<F>()
    }

    fn input_key(&self) -> Arc<dyn KeyValue> {
        Arc::new(self.input.clone())
    }

    fn function_name(&self) -> &'static str {
        std::any::type_name::<F>()
    }
}
/// Placeholder call for keyed-child placeholders and family roots.
fn erased_noop() -> Arc<dyn ErasedCall> {
    struct Noop;
    impl ErasedCall for Noop {
        fn invoke(&self) -> Result<Arc<dyn Value>> {
            Ok(Arc::new(()))
        }
        fn function_type(&self) -> TypeId {
            TypeId::of::<Noop>()
        }
        fn input_key(&self) -> Arc<dyn KeyValue> {
            Arc::new(())
        }
        fn function_name(&self) -> &'static str {
            "<keyed-family-root>"
        }
    }
    Arc::new(Noop)
}

pub(crate) fn erased_call<F, A, B>(function: F, input: A) -> Arc<dyn ErasedCall>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    Arc::new(TypedCall {
        function,
        input,
        _marker: std::marker::PhantomData,
    })
}

#[derive(Clone)]
struct Invocation {
    id: u64,
    parent: Option<u64>,
    identity: Option<CallIdentity>,
    call: Arc<dyn ErasedCall>,
    function_name: &'static str,
    result: Option<Arc<dyn Value>>,
    fresh_sites: HashMap<(&'static str, u32, u32, TypeId, u64), u64>,
    reads: Vec<ReadDep>,
    writes: Vec<PlainWrite>,
    children: Vec<u64>,
    seen_children: Vec<u64>,
    /// Frozen emission modes per view for THIS evaluation (plan §5.5).
    pub emission_modes: HashMap<TypeId, EmissionMode>,
    dirty: bool,
    retired: bool,
}

pub(crate) struct PlainGraph {
    pub state: Arc<Mutex<PlainState>>,
    pub invocations: Vec<Invocation>,
    pub root: u64,
    metadata: HashMap<TypeId, &'static str>,
    /// Reverse-dependency rows for this root's invocations (plan §5.2).
    pub deps: crate::reactive::store::DependencyIndex,
    /// Invocation-private state slots (plan §5.6), keyed by invocation id.
    pub state_slots: HashMap<u64, InvocationSlot>,
    dirty: HashSet<u64>,
    change_log: Vec<FactChange>,
}

impl PlainGraph {
    /// Function name of one invocation by id.
    pub(crate) fn function_name_of(&self, id: u64) -> Result<&'static str> {
        self.invocation(id).map(|invocation| invocation.function_name)
    }

    pub(crate) fn invocation_identity(&self, id: u64) -> Result<InvocationIdentity> {
        let invocation = self.invocation(id)?;
        let identity = invocation.identity.as_ref();
        Ok(InvocationIdentity {
            function: invocation.function_name.to_string(),
            file: identity.map(|identity| identity.file.to_string()),
            line: identity.map_or(0, |identity| identity.line),
            column: identity.map_or(0, |identity| identity.column),
            input_hash: invocation.call.input_key().hash_value(),
        })
    }

    /// Ids of live direct children (used by keyed-family removal).
    pub(crate) fn live_child_ids(&self) -> Vec<u64> {
        self.invocations
            .iter()
            .filter(|invocation| {
                invocation.parent == Some(self.root) && !invocation.retired
            })
            .map(|invocation| invocation.id)
            .collect()
    }

    pub(crate) fn new(state: PlainState, call: Arc<dyn ErasedCall>, root: u64) -> Self {
        let function_name = call.function_name();
        PlainGraph {
            state: Arc::new(Mutex::new(state)),
            deps: crate::reactive::store::DependencyIndex::default(),
            state_slots: HashMap::new(),
            invocations: vec![Invocation {
                id: root,
                parent: None,
                identity: None,
                call,
                function_name,
                result: None,
                reads: Vec::new(),
                writes: Vec::new(),
                children: Vec::new(),
                seen_children: Vec::new(),
                emission_modes: HashMap::new(),
                fresh_sites: HashMap::new(),
                dirty: false,
                retired: false,
            }],
            root,
            metadata: HashMap::new(),
            dirty: HashSet::new(),
            change_log: Vec::new(),
        }
    }

    fn invocation(&self, id: u64) -> Result<&Invocation> {
        self.invocations
            .iter()
            .find(|invocation| invocation.id == id && !invocation.retired)
            .ok_or_else(|| {
                Error::Internal(format!("invocation {id} missing/retired in root {}", self.root).into())
            })
    }

    fn invocation_mut(&mut self, id: u64) -> Result<&mut Invocation> {
        self.invocations
            .iter_mut()
            .find(|invocation| invocation.id == id && !invocation.retired)
            .ok_or_else(|| {
                Error::Internal(format!("invocation {id} missing/retired (mut) in root {}", self.root).into())
            })
    }

    fn register<V: View>(&mut self) {
        self.metadata.insert(TypeId::of::<V>(), V::name());
    }

    fn take_changes(&mut self) -> Vec<FactChange> {
        std::mem::take(&mut self.change_log)
    }
}

#[derive(Clone)]
struct CallBase {
    file: &'static str,
    line: u32,
    column: u32,
    function: TypeId,
}

impl CallBase {
    fn same(&self, other: &Self) -> bool {
        self.file == other.file
            && self.line == other.line
            && self.column == other.column
            && self.function == other.function
    }
}

// ---------------------------------------------------------------------------
// Command-local metric extensions (plan §10.1).

/// Command-local typed metric accumulator owned by the running command.
/// Framework components record fixed counter structs through
/// [`record_command_metric`]; commit freezes the accumulator into the raw
/// command report while rollback drops it. The accumulator records no facts
/// and installs no reactive dependencies.
#[derive(Default)]
pub(crate) struct MetricExtensions {
    slots: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl MetricExtensions {
    fn slot_mut<T: Default + Send + Sync + 'static>(&mut self) -> &mut T {
        self.slots
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::<T>::default())
            .downcast_mut::<T>()
            .expect("metric slot type matches its TypeId")
    }

    pub(crate) fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.slots.get(&TypeId::of::<T>()).and_then(|slot| slot.downcast_ref::<T>())
    }
}

impl fmt::Debug for MetricExtensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetricExtensions")
            .field("slots", &self.slots.len())
            .finish()
    }
}

thread_local! {
    /// Per-command metric frames. Commands are synchronous, so one frame
    /// per active command is sufficient; nested pushes would shadow outer
    /// recordings and never occur.
    static METRIC_FRAMES: RefCell<Vec<std::rc::Rc<RefCell<MetricExtensions>>>> =
        const { RefCell::new(Vec::new()) };
}

/// Records into the fixed metric slot `T` of the active command. Outside an
/// active command (isolated plan capture, direct helper calls in tests) the
/// record is dropped: instrumentation never creates state.
pub(crate) fn record_command_metric<T: Default + Send + Sync + 'static>(
    record: impl FnOnce(&mut T),
) {
    METRIC_FRAMES.with(|frames| {
        let top = frames.borrow().last().cloned();
        let Some(frame) = top else {
            return;
        };
        let mut extensions = frame.borrow_mut();
        record(extensions.slot_mut::<T>());
    });
}

pub(crate) fn record_invocation_evaluation(identity: InvocationIdentity) {
    record_command_metric::<InvocationWork>(|work| work.record(identity));
}
/// Pops the metric frame on drop, including unwind paths.
pub(crate) struct MetricFrame;

impl Drop for MetricFrame {
    fn drop(&mut self) {
        METRIC_FRAMES.with(|frames| {
            frames.borrow_mut().pop();
        });
    }
}

/// Extracts the accumulated engine counters from the active frame. The
/// counters live in a dedicated slot of the same accumulator so framework
/// and engine instrumentation share one freeze point.
fn take_engine_work() -> EngineWork {
    METRIC_FRAMES.with(|frames| {
        let frame = frames.borrow().last().cloned();
        let Some(cell) = frame else {
            return EngineWork::default();
        };
        let mut extensions = cell.borrow_mut();
        std::mem::take(extensions.slot_mut::<EngineWork>())
    })
}

/// Extracts one typed metric slot from the active frame, leaving the slot
/// empty. Used by the workspace facade to lift pre-command validation
/// counters into its report.
pub(crate) fn take_frame_metric<T: Default + Send + Sync + 'static>() -> T {
    METRIC_FRAMES.with(|frames| {
        let frame = frames.borrow().last().cloned();
        let Some(cell) = frame else {
            return T::default();
        };
        let mut extensions = cell.borrow_mut();
        std::mem::take(extensions.slot_mut::<T>())
    })
}

pub(crate) fn push_metric_frame() -> MetricFrame {
    METRIC_FRAMES.with(|frames| {
        frames
            .borrow_mut()
            .push(std::rc::Rc::new(RefCell::new(MetricExtensions::default())));
    });
    MetricFrame
}

fn freeze_metric_frame() -> Arc<MetricExtensions> {
    METRIC_FRAMES.with(|frames| {
        let frame = frames.borrow().last().cloned();
        match frame {
            Some(cell) => Arc::new(std::mem::replace(
                &mut *cell.borrow_mut(),
                MetricExtensions::default(),
            )),
            None => Arc::new(MetricExtensions::default()),
        }
    })
}

/// One buffered-but-uncommitted write of the active invocation.
pub(crate) struct PendingEntry {
    pub view: TypeId,
    pub key: Arc<dyn KeyValue>,
    pub value: Option<Arc<dyn Value>>,
}

/// Hash-index address for an erased view key. Hashes only choose a collision
/// bucket; every candidate still receives exact `KeyValue` equality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct IndexedKey {
    view: TypeId,
    hash: u64,
}

impl IndexedKey {
    fn new(view: TypeId, key: &dyn KeyValue) -> Self {
        Self {
            view,
            hash: key.hash_value(),
        }
    }
}

fn record_patch_lookup() {
    record_command_metric::<EngineWork>(|work| {
        work.patch_key_lookups += 1;
    });
}

fn record_patch_comparison() {
    record_command_metric::<EngineWork>(|work| {
        work.patch_key_comparisons += 1;
    });
}

/// Indexed pending-value overlay. The vectors are collision buckets only, so
/// lookup cost is amortized O(1) plus exact checks for true hash collisions.
#[derive(Default)]
struct PendingEntries {
    entries: Vec<PendingEntry>,
    buckets: HashMap<IndexedKey, Vec<usize>>,
}

impl PendingEntries {
    fn locate(&self, view: TypeId, key: &dyn KeyValue) -> Option<usize> {
        record_patch_lookup();
        let indexed = IndexedKey::new(view, key);
        self.buckets.get(&indexed)?.iter().copied().find(|index| {
            record_patch_comparison();
            self.entries[*index].key.eq_value(key)
        })
    }

    fn put(&mut self, view: TypeId, key: Arc<dyn KeyValue>, value: Option<Arc<dyn Value>>) {
        if let Some(index) = self.locate(view, key.as_ref()) {
            self.entries[index].value = value;
            return;
        }
        let index = self.entries.len();
        self.buckets
            .entry(IndexedKey::new(view, key.as_ref()))
            .or_default()
            .push(index);
        self.entries.push(PendingEntry { view, key, value });
    }

    fn get(&self, view: TypeId, key: &dyn KeyValue) -> Option<Option<Arc<dyn Value>>> {
        self.locate(view, key)
            .map(|index| self.entries[index].value.clone())
    }
}

/// Invocation-scoped pending-write overlay shared by every handle created
/// inside one evaluation (plan §5.3).
#[derive(Default)]
pub(crate) struct PendingOverlay {
    entries: RefCell<PendingEntries>,
    /// Buffered patch operations in authored call order. Freeze uses an
    /// indexed collision-safe key set before commit.
    patch_ops: RefCell<Vec<PatchOp>>,
}

/// One buffered patch operation against one view fact.
#[derive(Clone)]
pub(crate) struct PatchOp {
    pub view: TypeId,
    pub view_name: &'static str,
    pub key: Arc<dyn KeyValue>,
    pub kind: PatchOpKind,
}

#[derive(Clone)]
pub(crate) enum PatchOpKind {
    Upsert(Arc<dyn Value>),
    Remove,
}

impl PendingOverlay {
    fn push_patch(&self, op: PatchOp) {
        self.patch_ops.borrow_mut().push(op);
    }

    fn put(&self, view: TypeId, key: &Arc<dyn KeyValue>, value: Option<Arc<dyn Value>>) {
        self.entries
            .borrow_mut()
            .put(view, Arc::clone(key), value);
    }

    fn get(
        &self,
        view: TypeId,
        key: &Arc<dyn KeyValue>,
    ) -> Option<Option<Arc<dyn Value>>> {
        self.entries.borrow().get(view, key.as_ref())
    }
}

struct ActiveEval {
    graph: Arc<Mutex<PlainGraph>>,
    id: u64,
    occurrences: RefCell<Vec<(CallBase, u64)>>,
}

impl Clone for ActiveEval {
    fn clone(&self) -> Self {
        Self {
            graph: Arc::clone(&self.graph),
            id: self.id,
            occurrences: RefCell::new(self.occurrences.borrow().clone()),
        }
    }
}

enum ActiveDispatcher {
    Eval(Arc<Mutex<PlainGraph>>, u64, std::rc::Rc<PendingOverlay>),
    Command(Arc<Mutex<CommandBuffer>>, std::rc::Rc<PendingOverlay>),
}

thread_local! {
    static ACTIVE: RefCell<Vec<ActiveDispatcher>> = const { RefCell::new(Vec::new()) };
    static ACTIVE_EVALS: RefCell<Vec<ActiveEval>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone)]
pub enum Temporal {
    Current,
    Previous,
}

#[derive(Clone)]
pub struct EffectContext {
    dispatcher: ActiveDispatcherHandle,
    passive: bool,
}

#[derive(Clone)]
enum ActiveDispatcherHandle {
    Eval(Arc<Mutex<PlainGraph>>, u64, std::rc::Rc<PendingOverlay>),
    Command(std::rc::Rc<PendingOverlay>, Arc<Mutex<CommandBuffer>>),
}

fn downcast_arc<T: Any + Send + Sync>(value: Arc<dyn Value>) -> Option<Arc<T>> {
    let value: Arc<dyn Any + Send + Sync> = value;
    value.downcast::<T>().ok()
}

impl EffectContext {
    pub fn register<V: View>(&self) -> Result<()> {
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(graph, _, _) => {
                graph.lock().register::<V>();
                Ok(())
            }
            ActiveDispatcherHandle::Command(_, _) => Ok(()),
        }
    }

    pub fn observe<V: View>(
        &self,
        input: V::Input,
        temporal: Temporal,
    ) -> Result<Option<Arc<V::Output>>> {
        let temporal = matches!(temporal, Temporal::Previous);
        let key: Arc<dyn KeyValue> = Arc::new(input);
        let value = match &self.dispatcher {
            ActiveDispatcherHandle::Eval(graph, id, _) => {
                let mut graph = graph.lock();
                graph.register::<V>();
                let value = if temporal {
                    previous_read(graph.state.as_ref(), TypeId::of::<V>(), key.as_ref())
                } else {
                    graph.state.lock().read(TypeId::of::<V>(), key.as_ref())
                };
                if !self.passive {
                    graph.invocation_mut(*id)?.reads.push(ReadDep {
                        view: TypeId::of::<V>(),
                        name: V::name(),
                        key: Some(Arc::clone(&key)),
                        temporal,
                        keyset: false,
                    });
                }
                value
            }
            ActiveDispatcherHandle::Command(..) => {
                return Err(Error::InvalidCommandEffect {
                    effect: "observe".to_string(),
                });
            }
        };
        value
            .map(|value| {
                downcast_arc::<V::Output>(value)
                    .ok_or_else(|| Error::Internal("view output type mismatch".into()))
            })
            .transpose()
    }

    /// Reads one committed fact WITHOUT recording a dependency.
    ///
    /// This is the codec-internal base for read-modify-write emitters
    /// (`push`, `replace`, `link`, ...): their diff reads must not wake
    /// them for their own writes, or a non-idempotent writer would feed
    /// back on itself forever. Foreign overwrites of owned facts do not
    /// re-derive them; ownership (T5) makes such overwrites hostile by
    /// definition.
    pub fn peek<V: View>(&self, input: V::Input) -> Result<Option<Arc<V::Output>>> {
        let key: Arc<dyn KeyValue> = Arc::new(input);
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(graph, _, _) => {
                let mut graph = graph.lock();
                graph.register::<V>();
                let value = graph.state.lock().read(TypeId::of::<V>(), key.as_ref());
                Ok(value
                    .and_then(downcast_arc::<V::Output>))
            }
            ActiveDispatcherHandle::Command(..) => Err(Error::InvalidCommandEffect {
                effect: "peek".to_string(),
            }),
        }
    }

    pub fn inputs<V: View>(&self, temporal: Temporal) -> Result<Vec<V::Input>> {
        let temporal = matches!(temporal, Temporal::Previous);
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(graph, id, _) => {
                let mut graph = graph.lock();
                graph.register::<V>();
                if !self.passive {
                    graph.invocation_mut(*id)?.reads.push(ReadDep {
                        view: TypeId::of::<V>(),
                        name: V::name(),
                        key: None,
                        temporal,
                        keyset: false,
                    });
                }
                if temporal {
                    Ok(previous_inputs::<V>(graph.state.as_ref()))
                } else {
                    Ok(graph.state.lock().inputs::<V>())
                }
            }
            ActiveDispatcherHandle::Command(..) => Err(Error::InvalidCommandEffect {
                effect: "inputs".to_string(),
            }),
        }
    }
    /// Enumerates a view while depending only on its fact keyset.
    ///
    /// Payload replacements do not wake this computation; insertion and
    /// removal do. This is the dependency used by keyed child discovery.
    pub fn inputs_keyset<V: View>(&self, temporal: Temporal) -> Result<Vec<V::Input>> {
        let temporal = matches!(temporal, Temporal::Previous);
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(graph, id, _) => {
                let mut graph = graph.lock();
                graph.register::<V>();
                if !self.passive {
                    graph.invocation_mut(*id)?.reads.push(ReadDep {
                        view: TypeId::of::<V>(),
                        name: V::name(),
                        key: None,
                        temporal,
                        keyset: true,
                    });
                }
                if temporal {
                    Ok(previous_inputs::<V>(graph.state.as_ref()))
                } else {
                    Ok(graph.state.lock().inputs::<V>())
                }
            }
            ActiveDispatcherHandle::Command(..) => Err(Error::InvalidCommandEffect {
                effect: "inputs_keyset".to_string(),
            }),
        }
    }


    /// Records the handle's latest write for one fact so later reads inside
    /// the SAME invocation observe it (the invocation's writes commit only
    /// when it retires).
    pub fn pending_put<V: View>(&self, key: V::Input, value: Option<Arc<V::Output>>) {
        let key: Arc<dyn KeyValue> = Arc::new(key);
        let value: Option<Arc<dyn Value>> =
            value.map(|value| value as Arc<dyn Value>);
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(_, _, pending)
                | ActiveDispatcherHandle::Command(pending, _) => {
                pending.put(TypeId::of::<V>(), &key, value);
            }
        }
    }

    /// Looks up the invocation's pending write for one fact, if any.
    pub fn pending_get<V: View>(
        &self,
        key: &V::Input,
    ) -> Option<Option<Arc<V::Output>>> {
        let key: Arc<dyn KeyValue> = Arc::new(key.clone());
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(_, _, pending)
                | ActiveDispatcherHandle::Command(pending, _) => {
                pending
                    .get(TypeId::of::<V>(), &key)
                    .map(|value| {
                        value.and_then(downcast_arc::<V::Output>)
                    })
            }
        }
    }

    /// Declares one patch operation against a view (plan §5.5).
    ///
    /// Modes freeze per (invocation, view): mixing [`emit_view`] with
    /// [`EffectContext::emit_patch`] for the same view returns
    /// [`Error::MixedEmissionMode`].
    pub fn emit_patch<V: View>(
        &self,
        key: V::Input,
        value: Option<V::Output>,
    ) -> Result<()> {
        let key: Arc<dyn KeyValue> = Arc::new(key);
        let view = TypeId::of::<V>();
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(graph, id, pending) => {
                let mut graph_guard = graph.lock();
                graph_guard.register::<V>();
                // Mode freeze per invocation+view.
                let invocation = graph_guard.invocation_mut(*id)?;
                match invocation.emission_modes.get(&view) {
                    Some(EmissionMode::Replace) => {
                        return Err(Error::MixedEmissionMode {
                            view: V::name().to_string(),
                        });
                    }
                    Some(EmissionMode::Patch) => {}
                    None => {
                        invocation.emission_modes.insert(view, EmissionMode::Patch);
                    }
                }
                drop(graph_guard);
                pending.push_patch(PatchOp {
                    view,
                    view_name: V::name(),
                    key,
                    kind: match value {
                        Some(value) => PatchOpKind::Upsert(Arc::new(value)),
                        None => PatchOpKind::Remove,
                    },
                });
                Ok(())
            }
            ActiveDispatcherHandle::Command(_, buffer) => {
                match buffer.lock().modes.get(&view) {
                    Some(EmissionMode::Replace) => {
                        return Err(Error::MixedEmissionMode {
                            view: V::name().to_string(),
                        });
                    }
                    _ => {}
                }
                buffer.lock().modes.insert(view, EmissionMode::Patch);
                buffer.lock().patch_ops.push(PatchOp {
                    view,
                    view_name: V::name(),
                    key,
                    kind: match value {
                        Some(value) => PatchOpKind::Upsert(Arc::new(value)),
                        None => PatchOpKind::Remove,
                    },
                });
                Ok(())
            }
        }
    }

    pub fn emit<V: View>(&self, input: V::Input, output: Option<V::Output>) -> Result<()> {
        let key: Arc<dyn KeyValue> = Arc::new(input);
        let view = TypeId::of::<V>();
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(graph, id, _) => {
                let mut graph = graph.lock();
                graph.register::<V>();
                // Mode freeze per invocation+view (plan §5.5).
                let invocation = graph.invocation_mut(*id)?;
                match invocation.emission_modes.get(&view) {
                    Some(EmissionMode::Patch) => {
                        return Err(Error::MixedEmissionMode {
                            view: V::name().to_string(),
                        });
                    }
                    Some(EmissionMode::Replace) => {}
                    None => {
                        invocation.emission_modes.insert(view, EmissionMode::Replace);
                    }
                }
                invocation.writes.push(PlainWrite {
                    view,
                    name: V::name(),
                    view_name: V::name(),
                    key,
                    value: output.map(|output| Arc::new(output) as Arc<dyn Value>),
                    shareable: V::__shared_writes(),
                });
                Ok(())
            }
            ActiveDispatcherHandle::Command(_, buffer) => {
                match buffer.lock().modes.get(&view) {
                    Some(EmissionMode::Patch) => {
                        return Err(Error::MixedEmissionMode {
                            view: V::name().to_string(),
                        });
                    }
                    _ => {}
                }
                buffer.lock().modes.insert(view, EmissionMode::Replace);
                buffer.lock().writes.push(PlainWrite {
                    view,
                    name: V::name(),
                    view_name: V::name(),
                    key,
                    value: output.map(|output| Arc::new(output) as Arc<dyn Value>),
                    shareable: false,
                });
                Ok(())
            }
        }
    }
}

pub(crate) fn context_for(effect: &str, view: &str) -> Result<EffectContext> {
    context_for_mode(effect, view, false)
}

pub(crate) fn peek_context_for(effect: &str, view: &str) -> Result<EffectContext> {
    context_for_mode(effect, view, true)
}

fn context_for_mode(effect: &str, view: &str, passive: bool) -> Result<EffectContext> {
    ACTIVE.with(|active| {
        let active = active.borrow();
        let Some(dispatcher) = active.last() else {
            return Err(Error::EffectOutsideRun {
                effect: effect.to_string(),
                view: view.to_string(),
            });
        };
        let dispatcher = match dispatcher {
            ActiveDispatcher::Eval(graph, id, pending) => {
                ActiveDispatcherHandle::Eval(Arc::clone(graph), *id, std::rc::Rc::clone(pending))
            }
            ActiveDispatcher::Command(buffer, pending) => {
                ActiveDispatcherHandle::Command(
                    std::rc::Rc::clone(pending),
                    Arc::clone(buffer),
                )
            }
        };
        Ok(EffectContext {
            dispatcher,
            passive,
        })
    })
}

fn panic_error(payload: Box<dyn Any + Send>) -> Error {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string());
    Error::Panic(Arc::from(message))
}

fn active_ids() -> Vec<u64> {
    ACTIVE_EVALS.with(|active| active.borrow().iter().map(|frame| frame.id).collect())
}

fn evaluate_graph(graph: &Arc<Mutex<PlainGraph>>, id: u64) -> Result<(Arc<dyn Value>, bool)> {
    // Rollback authority is the command transaction (journal + undo), so a
    // failed evaluation leaves state dirty and lets the caller unwind the
    // whole epoch atomically.
    let identity = graph.lock().invocation_identity(id)?;
    record_invocation_evaluation(identity);
    record_command_metric::<EngineWork>(|work| {
        work.invocation_evaluations += 1;
    });
    let root_token = graph.lock().root;
    with_txn(|txn| txn.touch_invocation(root_token, &graph.lock(), id));
    let call = {
        let mut graph_guard = graph.lock();
        let invocation = graph_guard.invocation_mut(id)?;
        let old_writes = std::mem::take(&mut invocation.writes);
        let old_reads = std::mem::take(&mut invocation.reads);
        invocation.fresh_sites.clear();
        invocation.seen_children.clear();
        // Emission modes freeze per evaluation only (plan §5.5).
        invocation.emission_modes.clear();
        invocation.dirty = false;
        OLD_WRITES.with(|slot| {
            slot.borrow_mut()
                .push((id, old_writes, old_reads))
        });
        Arc::clone(&invocation.call)
    };

    let pending = std::rc::Rc::new(PendingOverlay::default());
    ACTIVE_EVALS.with(|active| {
        active.borrow_mut().push(ActiveEval {
            graph: Arc::clone(graph),
            id,
            occurrences: RefCell::new(Vec::new()),
        });
    });
    ACTIVE.with(|active| {
        active
            .borrow_mut()
            .push(ActiveDispatcher::Eval(Arc::clone(graph), id, std::rc::Rc::clone(&pending)));
    });
    let result = catch_unwind(AssertUnwindSafe(|| call.invoke()));
    ACTIVE.with(|active| {
        active.borrow_mut().pop();
    });
    ACTIVE_EVALS.with(|active| {
        active.borrow_mut().pop();
    });

    let result = match result {
        Ok(result) => result,
        Err(payload) => return Err(panic_error(payload)),
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => return Err(error),
    };

    let mut graph_guard = graph.lock();
    let (old_writes, old_read_deps) = OLD_WRITES
        .with(|slot| {
            slot.borrow_mut()
                .iter_mut()
                .rev()
                .find(|(eval_id, _, _)| *eval_id == id)
                .map(|(_, writes, reads)| {
                    (
                        std::mem::take(writes),
                        std::mem::take(reads),
                    )
                })
                .unwrap_or((Vec::new(), Vec::new()))
        });
    let emission_modes = graph_guard.invocation(id)?.emission_modes.clone();
    let mut patch_views: HashSet<TypeId> = old_writes
        .iter()
        .filter(|write| write.name == "patch")
        .filter(|write| !matches!(emission_modes.get(&write.view), Some(EmissionMode::Replace)))
        .map(|write| write.view)
        .collect();
    patch_views.extend(
        emission_modes.iter().filter_map(|(view, mode)| {
            matches!(mode, EmissionMode::Patch).then_some(*view)
        }),
    );

    // Build one exact-key index for this candidate. Old patch-owned keys are
    // retained only when the invocation remains in patch mode; touched keys
    // replace them in O(1) expected time without scanning unrelated writes.
    let direct_writes = std::mem::take(&mut graph_guard.invocation_mut(id)?.writes);
    let mut candidate_index = IndexedWrites::from_writes(direct_writes);
    let patch_ops = std::mem::take(&mut *pending.patch_ops.borrow_mut());
    let patch_index = patch_ops_to_indexed(patch_ops)?;
    for previous in old_writes
        .iter()
        .filter(|write| patch_views.contains(&write.view))
    {
        candidate_index.insert_if_absent(previous.clone());
    }
    for write in patch_index.into_writes() {
        candidate_index.replace(write);
    }
    let candidate = candidate_index.ordered();
    graph_guard.invocation_mut(id)?.writes = candidate.clone();
    let seen_children = graph_guard.invocation(id)?.seen_children.clone();

    // Retire unseen children first; every mutation flows through the
    // transaction so the epoch stays atomic.
    let unseen: Vec<u64> = graph_guard
        .invocation(id)?
        .children
        .iter()
        .copied()
        .filter(|child| !seen_children.contains(child))
        .collect();
    let mut changes = Vec::new();
    if !unseen.is_empty() {
        let function_name = graph_guard.invocation(id)?.function_name;
        drop(graph_guard);
        for child in &unseen {
            changes.extend(retract_invocation(&graph, *child, function_name)?);
        }
        graph_guard = graph.lock();
    }
    let _ = &mut graph_guard;

    // Retract omitted writes and apply candidate writes through the journal.
    {
        let function_name = graph_guard.invocation(id)?.function_name;
        let state = Arc::clone(&graph_guard.state);
        let retracts: Vec<PlainWrite> = old_writes
            .iter()
            .filter(|previous| !candidate_index.contains(previous))
            .cloned()
            .collect();
        drop(graph_guard);
        with_txn(|txn| -> Result<()> {
            let mut state = state.lock();
            for write in &retracts {
                if let Some(change) = txn.journal.retract(
                    &mut state,
                    write.view,
                    write.name,
                    write.key.as_ref(),
                    id,
                    function_name,
                )? {
                    changes.push(change);
                }
            }
            for write in &candidate {
                if let Some(change) = txn.journal.write(
                    &mut state,
                    write.view,
                    write.name,
                    Arc::clone(&write.key),
                    write.value.clone(),
                    id,
                    function_name,
                    write.shareable,
                )? {
                    changes.push(change);
                }
            }
            Ok(())
        })?;
        graph_guard = graph.lock();
    }

    let previous_result = graph_guard.invocation(id)?.result.clone();
    let changed = previous_result
        .as_ref()
        .is_none_or(|old| !old.value_eq(result.as_ref()));
    {
        let new_read_rows: Vec<(
            TypeId,
            Option<Arc<dyn KeyValue>>,
            bool,
            bool,
        )> = graph_guard
            .invocation(id)?
            .reads
            .iter()
            .map(|read| (read.view, read.key.clone(), read.temporal, read.keyset))
            .collect();
        let old_read_rows: Vec<(
            TypeId,
            Option<Arc<dyn KeyValue>>,
            bool,
            bool,
        )> = old_read_deps
            .iter()
            .map(|read| (read.view, read.key.clone(), read.temporal, read.keyset))
            .collect();
        graph_guard.deps.replace(&new_read_rows, &old_read_rows, id);
        let invocation = graph_guard.invocation_mut(id)?;
        invocation.result = Some(Arc::clone(&result));
        invocation.writes = candidate;
        invocation.children = seen_children;
        invocation.dirty = false;
    }
    graph_guard.change_log.extend(changes);
    Ok((result, changed))
}

/// Collision-safe indexed write set. Hash tables are never iterated for
/// publication order; `ordered` freezes entries by stable view name, stable
/// key hash, then the deterministic authored insertion ordinal.
#[derive(Default)]
struct IndexedWrites {
    entries: Vec<IndexedWrite>,
    buckets: HashMap<IndexedKey, Vec<usize>>,
    next_ordinal: u64,
}

#[derive(Clone)]
struct IndexedWrite {
    ordinal: u64,
    write: PlainWrite,
}

impl IndexedWrites {
    fn from_writes(writes: impl IntoIterator<Item = PlainWrite>) -> Self {
        let mut indexed = Self::default();
        for write in writes {
            indexed.replace(write);
        }
        indexed
    }

    fn locate(&self, view: TypeId, key: &dyn KeyValue) -> Option<usize> {
        record_patch_lookup();
        self.buckets
            .get(&IndexedKey::new(view, key))?
            .iter()
            .copied()
            .find(|index| {
                record_patch_comparison();
                self.entries[*index].write.key.eq_value(key)
            })
    }

    fn append(&mut self, write: PlainWrite) {
        let index = self.entries.len();
        self.buckets
            .entry(IndexedKey::new(write.view, write.key.as_ref()))
            .or_default()
            .push(index);
        self.entries.push(IndexedWrite {
            ordinal: self.next_ordinal,
            write,
        });
        self.next_ordinal = self
            .next_ordinal
            .checked_add(1)
            .expect("indexed write ordinal overflow");
    }

    /// Replaces the value for one exact key, preserving its first authored
    /// ordinal so duplicate direct writes retain historical semantics.
    fn replace(&mut self, write: PlainWrite) -> Option<PlainWrite> {
        if let Some(index) = self.locate(write.view, write.key.as_ref()) {
            record_command_metric::<EngineWork>(|work| {
                work.patch_ops_coalesced += 1;
            });
            return Some(std::mem::replace(&mut self.entries[index].write, write));
        }
        self.append(write);
        None
    }

    /// Adds a write only when an exact key is absent.
    fn insert_if_absent(&mut self, write: PlainWrite) -> bool {
        if self.locate(write.view, write.key.as_ref()).is_some() {
            return false;
        }
        self.append(write);
        true
    }

    fn contains(&self, write: &PlainWrite) -> bool {
        self.locate(write.view, write.key.as_ref()).is_some()
    }

    /// Yields the buffered writes in first-authored insertion order. Each
    /// entry keeps its own ordinal, so merging preserves historical
    /// semantics and the single final canonical sort stays deterministic.
    fn into_writes(self) -> Vec<PlainWrite> {
        self.entries
            .into_iter()
            .map(|entry| entry.write)
            .collect()
    }

    fn ordered(&self) -> Vec<PlainWrite> {
        let mut entries = self.entries.clone();
        entries.sort_by(|left, right| {
            left.write
                .view_name
                .cmp(right.write.view_name)
                .then_with(|| {
                    left.write
                        .key
                        .hash_value()
                        .cmp(&right.write.key.hash_value())
                })
                .then(left.ordinal.cmp(&right.ordinal))
        });
        entries.into_iter().map(|entry| entry.write).collect()
    }
}

/// Converts buffered patch operations into an indexed candidate set,
/// rejecting exact duplicate keys with indexed collision buckets rather
/// than vector scans. The returned index is NOT sorted: each commit path
/// merges it into its final [`IndexedWrites`] and performs exactly one
/// canonical sort, so authors pay no intermediate freeze order.
fn patch_ops_to_indexed(ops: Vec<PatchOp>) -> Result<IndexedWrites> {
    let mut writes = IndexedWrites::default();
    for op in ops {
        let write = PlainWrite {
            view: op.view,
            name: "patch",
            view_name: op.view_name,
            key: op.key,
            value: match op.kind {
                PatchOpKind::Upsert(value) => Some(value),
                PatchOpKind::Remove => None,
            },
            shareable: false,
        };
        if !writes.insert_if_absent(write) {
            return Err(Error::DuplicatePatchKey {
                view: op.view_name.to_string(),
            });
        }
    }
    Ok(writes)
}

fn retract_invocation(
    graph: &Arc<Mutex<PlainGraph>>,
    id: u64,
    _root_function: &'static str,
) -> Result<Vec<FactChange>> {
    let (children, writes, name, read_rows) = {
        let graph_guard = graph.lock();
        let Some(invocation) = graph_guard
            .invocations
            .iter()
            .find(|invocation| invocation.id == id)
            .cloned()
        else {
            return Ok(Vec::new());
        };
        if invocation.retired {
            return Ok(Vec::new());
        }
        with_txn(|txn| txn.touch_invocation(graph_guard.root, &graph_guard, id));
        (
            invocation.children.clone(),
            invocation.writes.clone(),
            invocation.function_name,
            invocation
                .reads
                .iter()
                .map(|read| (read.view, read.key.clone(), read.temporal, read.keyset))
                .collect::<Vec<_>>(),
        )
    };
    let mut changes = Vec::new();
    for child in children {
        changes.extend(retract_invocation(graph, child, name)?);
    }
    // Retired invocations hold no dependency rows: they must never be
    // marked again by later changes.
    graph.lock().deps.remove_all(&read_rows, id);
    let state = Arc::clone(&graph.lock().state);
    with_txn(|txn| -> Result<()> {
        let mut state = state.lock();
        for write in writes {
            if let Some(change) =
                txn.journal
                    .retract(&mut state, write.view, write.name, write.key.as_ref(), id, name)?
            {
                changes.push(change);
            }
        }
        Ok(())
    })?;
    let mut graph_guard = graph.lock();
    let invocation = graph_guard.invocation_mut(id)?;
    invocation.retired = true;
    invocation.result = None;
    invocation.reads.clear();
    invocation.writes.clear();
    invocation.children.clear();
    invocation.seen_children.clear();
    invocation.dirty = false;
    // State slots are invocation-private: retirement drops them (plan §5.6).
    graph_guard.state_slots.remove(&id);
    Ok(changes)
}

#[track_caller]
pub(crate) fn run_effect<F, A, B>(function: F, input: A) -> Result<B>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    run_effect_at(function, input, false, std::panic::Location::caller())
}

#[track_caller]
pub(crate) fn run_keyed_effect<F, A, B>(function: F, input: A) -> Result<B>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    run_effect_at(function, input, true, std::panic::Location::caller())
}

fn run_effect_at<F, A, B>(
    function: F,
    input: A,
    stable_input: bool,
    location: &'static std::panic::Location<'static>,
) -> Result<B>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    let evaluation = ACTIVE_EVALS.with(|active| {
        let active = active.borrow();
        let Some(frame) = active.last() else {
            return Ok(None);
        };
        let key: Arc<dyn KeyValue> = Arc::new(input.clone());
        let base = CallBase {
            file: location.file(),
            line: location.line(),
            column: location.column(),
            function: TypeId::of::<F>(),
        };
        let mut occurrences = frame.occurrences.borrow_mut();
        let entry = occurrences.iter_mut().find(|(have, _)| have.same(&base));
        let occurrence = if let Some((_, count)) = entry {
            let value = *count;
            *count += 1;
            value
        } else {
            occurrences.push((base, 1));
            0
        };
        Ok(Some((Arc::clone(&frame.graph), frame.id, occurrence)))
    })?;
    let Some((graph, parent, occurrence)) = evaluation else {
        let in_command = ACTIVE
            .with(|active| matches!(active.borrow().last(), Some(ActiveDispatcher::Command(..))));
        return Err(if in_command {
            Error::InvalidCommandEffect {
                effect: "run".to_string(),
            }
        } else {
            Error::EffectOutsideRun {
                effect: "run".to_string(),
                view: "<computation>".to_string(),
            }
        });
    };

    let key: Arc<dyn KeyValue> = Arc::new(input.clone());
    let identity = CallIdentity {
        file: location.file(),
        line: location.line(),
        column: location.column(),
        function: TypeId::of::<F>(),
        input: Arc::clone(&key),
        occurrence,
        stable_input,
    };
    let call = erased_call(function, input);

    let ancestors = active_ids();
    {
        let graph_guard = graph.lock();
        if let Some(cycle_start) = ancestors.iter().position(|ancestor| {
            graph_guard
                .invocation(*ancestor)
                .ok()
                .is_some_and(|invocation| {
                    invocation.call.function_type() == identity.function
                        && invocation
                            .call
                            .input_key()
                            .eq_value(identity.input.as_ref())
                })
        }) {
            let mut functions = ancestors[cycle_start..]
                .iter()
                .filter_map(|ancestor| graph_guard.invocation(*ancestor).ok())
                .map(|invocation| invocation.function_name.to_string())
                .collect::<Vec<_>>();
            functions.push(call.function_name().to_string());
            return Err(Error::ComputationCycle { functions });
        }
    }

    let child = {
        let (existing, stale) = {
            let graph_guard = graph.lock();
            let children = graph_guard.invocation(parent)?.children.clone();
            let existing = children.iter().find_map(|child| {
                graph_guard
                    .invocations
                    .iter()
                    .find(|invocation| invocation.id == *child && !invocation.retired)
                    .filter(|invocation| {
                        !graph_guard
                            .invocation(parent)
                            .map(|parent| parent.seen_children.contains(&invocation.id))
                            .unwrap_or(false)
                    })
                    .filter(|invocation| {
                        invocation
                            .identity
                            .as_ref()
                            .is_some_and(|have| have.same(&identity))
                    })
                    .map(|invocation| invocation.id)
            });
            let stale = if existing.is_none() && !identity.stable_input {
                children
                    .iter()
                    .filter_map(|child| {
                        graph_guard
                            .invocations
                            .iter()
                            .find(|invocation| invocation.id == *child && !invocation.retired)
                    })
                    .filter(|invocation| {
                        invocation.identity.as_ref().is_some_and(|have| {
                            have.file == identity.file
                                && have.line == identity.line
                                && have.column == identity.column
                                && have.function == identity.function
                                && have.occurrence == identity.occurrence
                        })
                    })
                    .map(|invocation| invocation.id)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            (existing, stale)
        };
        for stale_child in stale {
            let changes = retract_invocation(&graph, stale_child, call.function_name())?;
            graph.lock().change_log.extend(changes);
            let mut graph_guard = graph.lock();
            graph_guard
                .invocation_mut(parent)?
                .children
                .retain(|child| *child != stale_child);
        }
        let mut graph_guard = graph.lock();
        with_txn(|txn| txn.touch_invocation(graph_guard.root, &graph_guard, parent));
        let child = if let Some(child) = existing {
            with_txn(|txn| txn.touch_invocation(graph_guard.root, &graph_guard, child));
            graph_guard.invocation_mut(child)?.call = Arc::clone(&call);
            child
        } else {
            let child = fresh_token();
            graph_guard.invocations.push(Invocation {
                id: child,
                parent: Some(parent),
                identity: Some(identity),
                call: Arc::clone(&call),
                function_name: call.function_name(),
                reads: Vec::new(),
                writes: Vec::new(),
                result: None,
                fresh_sites: HashMap::new(),
                children: Vec::new(),
                seen_children: Vec::new(),
                emission_modes: HashMap::new(),
                dirty: false,
                retired: false,
            });
            graph_guard.invocation_mut(parent)?.children.push(child);
            child
        };
        graph_guard
            .invocation_mut(parent)?
            .seen_children
            .push(child);
        child
    };

    let evaluate = {
        let graph_guard = graph.lock();
        let invocation = graph_guard.invocation(child)?;
        invocation.result.is_none() || invocation.dirty
    };
    if evaluate {
        evaluate_graph(&graph, child)?;
    }
    let graph_guard = graph.lock();
    let result = graph_guard
        .invocation(child)?
        .result
        .as_ref()
        .ok_or_else(|| Error::Internal("child computation produced no result".into()))?;
    result
        .as_any()
        .downcast_ref::<B>()
        .cloned()
        .ok_or_else(|| Error::Internal("child result type mismatch".into()))
}
/// Mints a deterministic typed node identity for the active computation
/// invocation. The identity excludes the parent/root instance so equal
/// constructor calls in independent roots share one node.
#[track_caller]
pub(crate) fn fresh_node_id<V: View>() -> Result<crate::reactive::view::Node<V>> {
    let location = std::panic::Location::caller();
    let (graph, id) = ACTIVE_EVALS.with(|active| {
        active
            .borrow()
            .last()
            .map(|frame| (Arc::clone(&frame.graph), frame.id))
            .ok_or_else(|| Error::EffectOutsideRun {
                effect: "fresh_node_id".to_string(),
                view: V::name().to_string(),
            })
    })?;
    let mut graph_guard = graph.lock();
    let invocation = graph_guard.invocation_mut(id)?;
    let function = invocation.call.function_type();
    let input_hash = invocation.call.input_key().hash_value();
    let occurrence = {
        let key = (
            location.file(),
            location.line(),
            location.column(),
            TypeId::of::<V>(),
            input_hash,
        );
        let next = invocation.fresh_sites.entry(key).or_insert(0);
        let occurrence = *next;
        *next = next.saturating_add(1);
        occurrence
    };
    let mut hasher = DefaultHasher::new();
    TypeId::of::<V>().hash(&mut hasher);
    function.hash(&mut hasher);
    input_hash.hash(&mut hasher);
    location.file().hash(&mut hasher);
    location.line().hash(&mut hasher);
    location.column().hash(&mut hasher);
    occurrence.hash(&mut hasher);
    Ok(crate::reactive::view::Node::from_raw(hasher.finish()))
}

#[derive(Default)]
struct CommandBuffer {
    writes: Vec<PlainWrite>,
    patch_ops: Vec<PatchOp>,
    modes: HashMap<TypeId, EmissionMode>,
}

/// Frozen per-(invocation, view) emission mode (plan §5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EmissionMode {
    Replace,
    Patch,
}

#[derive(Clone)]
pub(crate) struct PlainPlan {
    pub graph: Arc<Mutex<PlainGraph>>,
    pub root: u64,
    pub captured_epoch: u64,
    pub output: Arc<dyn Value>,
}

pub(crate) struct OutputSink {
    pub update: Box<dyn Fn(Arc<dyn Value>) + Send + Sync>,
}

#[derive(Clone)]
pub(crate) struct RootRuntime {
    pub graph: Arc<Mutex<PlainGraph>>,
    pub sink: Arc<OutputSink>,
    /// Monotonic installation ordinal driving deterministic dirty order.
    pub install_ordinal: u64,
}

pub(crate) struct PlainRuntime {
    /// The committed fact store. Shared by handle with every graph during
    /// commands; the journal is the only mutation path and rollback authority.
    pub state: Arc<Mutex<PlainState>>,
    /// The committed persistent read index; snapshots clone it in O(1).
    pub committed: SnapshotRoot,
    pub epoch: u64,
    pub roots: BTreeMap<u64, RootRuntime>,
    pub last_changed: Vec<FactChange>,
    /// Ordered dirty invocations across roots (plan §5.2).
    pub dirty: crate::reactive::store::DirtyQueue,
    /// Keyed component families (plan §5.4), by family id and root token.
    pub families: HashMap<u64, FamilyRuntime>,
    pub(crate) family_by_root: HashMap<u64, u64>,
    pub(crate) next_install_ordinal: u64,
}

/// One installed keyed family: its dedicated graph plus watch metadata and
/// the erased constructor that stamps typed calls onto children.
pub(crate) struct FamilyRuntime {
    pub graph: Arc<Mutex<PlainGraph>>,
    pub view: TypeId,
    pub view_name: &'static str,
    pub install_ordinal: u64,
    pub build_call: Arc<dyn Fn(Arc<dyn KeyValue>) -> Arc<dyn ErasedCall> + Send + Sync>,
}

impl Default for PlainRuntime {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(PlainState::default())),
            committed: SnapshotRoot::default(),
            epoch: 0,
            roots: BTreeMap::new(),
            last_changed: Vec::new(),
            dirty: crate::reactive::store::DirtyQueue::default(),
            families: HashMap::new(),
            family_by_root: HashMap::new(),
            next_install_ordinal: 1,
        }
    }
}


/// A frozen committed view of all facts: one outer-map clone per snapshot.
#[derive(Clone)]
pub(crate) struct PlainSnapshot {
    root: SnapshotRoot,
}

impl PlainSnapshot {
    /// Total committed fact count across every view (plan §20.6 "live
    /// persistent bytes" proxy): entries retained in the immutable read
    /// index after this command.
    pub(crate) fn live_fact_count(&self) -> u64 {
        self.root
            .views()
            .iter()
            .map(|(_, view)| view.len() as u64)
            .sum()
    }

    pub(crate) fn observe<V: View>(&self, input: V::Input) -> Option<Arc<V::Output>> {
        record_command_metric::<EngineWork>(|work| {
            work.fact_reads += 1;
            work.index_probes += 1;
        });
        let view = self.root.view(TypeId::of::<V>())?;
        let key: Arc<dyn KeyValue> = Arc::new(input);
        let entry = view.lookup(key.as_ref())?;
        entry.value.as_any().downcast_ref::<V::Output>().map(|_| {
            let value: Arc<dyn Value> = Arc::clone(&entry.value);
            value
        }).and_then(downcast_arc::<V::Output>)
    }

    pub(crate) fn inputs<V: View>(&self) -> Vec<V::Input> {
        record_command_metric::<EngineWork>(|work| {
            work.view_enumerations += 1;
        });
        let Some(view) = self.root.view(TypeId::of::<V>()) else {
            return Vec::new();
        };
        view.entries()
            .filter_map(|entry| entry.key.as_any().downcast_ref::<V::Input>().cloned())
            .collect()
    }
}

pub(crate) fn snapshot(runtime: &PlainRuntime) -> PlainSnapshot {
    // One outer-map handle clone per snapshot; view roots are shared.
    PlainSnapshot {
        root: runtime.committed.clone(),
    }
}

pub(crate) fn capture_plan<F, A, B>(
    state: PlainState,
    epoch: u64,
    function: F,
    input: A,
) -> Result<(PlainPlan, Arc<B>)>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    let root = fresh_token();
    let call = erased_call(function, input);
    let graph = Arc::new(Mutex::new(PlainGraph::new(state, call, root)));
    // Isolated capture runs under its own throwaway transaction frame so
    // evaluation paths behave identically; nothing here commits.
    let _txn_frame = push_txn();
    evaluate_graph(&graph, root)?;
    drop(_txn_frame);
    let output = graph
        .lock()
        .invocation(root)?
        .result
        .as_ref()
        .cloned()
        .ok_or_else(|| Error::Internal("root computation produced no result".into()))?;
    let typed = output
        .as_any()
        .downcast_ref::<B>()
        .cloned()
        .ok_or_else(|| Error::Internal("root result type mismatch".into()))?;
    Ok((
        PlainPlan {
            graph,
            root,
            captured_epoch: epoch,
            output,
        },
        Arc::new(typed),
    ))
}

pub(crate) fn recapture_plan(
    plan: &mut PlainPlan,
    state: PlainState,
    epoch: u64,
) -> Result<Arc<dyn Value>> {
    let root_call = {
        let graph_guard = plan.graph.lock();
        graph_guard
            .invocations
            .iter()
            .find(|invocation| invocation.id == plan.root)
            .map(|invocation| Arc::clone(&invocation.call))
            .ok_or_else(|| Error::Internal("missing planned root".into()))?
    };
    *plan.graph.lock() = PlainGraph::new(state, root_call, plan.root);
    let _txn_frame = push_txn();
    let outcome = evaluate_graph(&plan.graph, plan.root);
    drop(_txn_frame);
    outcome?;
    let output = plan
        .graph
        .lock()
        .invocation(plan.root)?
        .result
        .as_ref()
        .cloned()
        .ok_or_else(|| Error::Internal("recaptured root produced no result".into()))?;
    plan.captured_epoch = epoch;
    plan.output = Arc::clone(&output);
    Ok(output)
}

pub(crate) fn promote_plan(
    runtime: &mut PlainRuntime,
    plan: PlainPlan,
    sink: Arc<OutputSink>,
) -> Result<Arc<dyn Value>> {
    if runtime.roots.contains_key(&plan.root) {
        return Err(Error::PlanAlreadyRun);
    }
    let graph = Arc::clone(&plan.graph);
    // Merge the captured publication through the active transaction's
    // journal so rollback covers root installation too. Each captured fact
    // keeps its ORIGINAL owning invocation (nested children included);
    // equal-valued collisions union owners instead of conflicting.
    let captured = graph.lock().state.lock().clone();
    with_txn(|txn| -> Result<()> {
        let mut state = runtime.state.lock();
        for slot in captured.slots.iter().flatten() {
            txn.journal.install(&mut state, slot.clone())?;
        }
        Ok(())
    })?;
    with_txn(|txn| {
        txn.push_undo(Undo::RootInserted { root: plan.root });
    });
    let install_ordinal = runtime.next_install_ordinal;
    runtime.next_install_ordinal += 1;
    runtime.roots.insert(plan.root, RootRuntime { graph, sink, install_ordinal });
    Ok(plan.output)
}

fn change_matches(read: &ReadDep, changes: &[FactChange], temporal: bool) -> bool {
    changes.iter().any(|change| read.matches(change, temporal))
}

/// Temporal::Previous fact read: the journal's first-touch value when the
/// command touched the key, else the committed value (identical for
/// untouched keys).
fn previous_read(state: &Mutex<PlainState>, view: TypeId, key: &dyn KeyValue) -> Option<Arc<dyn Value>> {
    ACTIVE_TXN.with(|txn| {
        let frame = txn.borrow().as_ref()?.clone();
        let guard = frame.borrow();
        guard.journal.first_value(view, key)
    })
    .or_else(|| state.lock().read(view, key))
}

/// Temporal::Previous input enumeration with journal adjustment.
fn previous_inputs<V: View>(state: &Mutex<PlainState>) -> Vec<V::Input> {
    let live = state.lock().inputs::<V>();
    ACTIVE_TXN.with(|txn| -> Vec<V::Input> {
        let Some(frame) = txn.borrow().as_ref().cloned() else {
            return live;
        };
        let guard = frame.borrow();
        guard.journal.previous_inputs::<V>(&state.lock())
    })
}

fn mark_changes(runtime: &mut PlainRuntime, changes: &[FactChange]) {
    if changes.is_empty() {
        return;
    }
    // Keyed families schedule their children directly per changed key
    // (plan §5.4); no root re-discovers keys through enumeration.
    schedule_families(runtime, changes);
    let changes: Vec<(TypeId, Option<Arc<dyn KeyValue>>, bool)> = changes
        .iter()
        .map(|change| {
            (
                change.view,
                Some(Arc::clone(&change.key)),
                change.presence_changed,
            )
        })
        .collect();
    let mut queued = 0usize;
    for root in runtime.roots.values() {
        let install_ordinal = root.install_ordinal;
        let graph = root.graph.lock();
        let mut ids: Vec<u64> = Vec::new();
        graph.deps.mark_current(&changes, |id| ids.push(id));
        for id in ids {
            runtime.dirty.insert(crate::reactive::store::DirtyKey {
                root_install_ordinal: install_ordinal,
                invocation_ordinal: id,
                root: graph.root,
                invocation: id,
            });
            queued += 1;
        }
    }
    record_command_metric::<EngineWork>(|work| {
        work.exact_marks += queued as u64;
        work.queue_pushes += queued as u64;
    });
}

pub(crate) fn install_root(
    runtime: &mut PlainRuntime,
    plan: PlainPlan,
    sink: Arc<OutputSink>,
) -> Result<Arc<dyn Value>> {
    // Rollback is the caller's transaction responsibility.
    let output = promote_plan(runtime, plan, sink)?;
    if let Some(views) = dependency_cycle(runtime) {
        return Err(Error::DependencyCycle { views });
    }
    Ok(output)
}

pub(crate) fn dependency_cycle(runtime: &PlainRuntime) -> Option<Vec<String>> {
    struct Fact {
        view: TypeId,
        name: &'static str,
        key: Option<Arc<dyn KeyValue>>,
    }

    fn same_fact(left: &Fact, right: &Fact) -> bool {
        left.view == right.view
            && match (&left.key, &right.key) {
                (Some(left), Some(right)) => left.eq_value(right.as_ref()),
                (None, None) => true,
                _ => false,
            }
    }

    fn index_of(facts: &mut Vec<Fact>, candidate: Fact) -> usize {
        if let Some(index) = facts.iter().position(|fact| same_fact(fact, &candidate)) {
            return index;
        }
        facts.push(candidate);
        facts.len() - 1
    }

    let mut facts = Vec::new();
    let mut edges: Vec<Vec<usize>> = Vec::new();
    for root in runtime.roots.values() {
        let graph = root.graph.lock();
        for invocation in graph
            .invocations
            .iter()
            .filter(|invocation| !invocation.retired)
        {
            let reads: Vec<usize> = invocation
                .reads
                .iter()
                .filter(|read| !read.temporal)
                .map(|read| {
                    index_of(
                        &mut facts,
                        Fact {
                            view: read.view,
                            name: read.name,
                            key: read.key.clone(),
                        },
                    )
                })
                .collect();
            let writes: Vec<usize> = invocation
                .writes
                .iter()
                .map(|write| {
                    index_of(
                        &mut facts,
                        Fact {
                            view: write.view,
                            name: write.name,
                            key: Some(Arc::clone(&write.key)),
                        },
                    )
                })
                .collect();
            if edges.len() < facts.len() {
                edges.resize_with(facts.len(), Vec::new);
            }
            for read in reads {
                for &write in &writes {
                    // Writes to the same view are publication updates. They
                    // may form a semantic fixed point across exact facts
                    // (recursive scope/type references are one example), but
                    // they are not cross-view computation cycles.
                    if read == write || facts[read].view == facts[write].view {
                        continue;
                    }
                    if !edges[read].contains(&write) {
                        edges[read].push(write);
                    }
                }
            }
        }
    }

    fn visit(
        node: usize,
        facts: &[Fact],
        edges: &[Vec<usize>],
        active: &mut Vec<usize>,
        seen: &mut HashSet<usize>,
    ) -> Option<Vec<String>> {
        if let Some(index) = active.iter().position(|have| *have == node) {
            let mut cycle = active[index..]
                .iter()
                .map(|index| facts[*index].name.to_string())
                .collect::<Vec<_>>();
            cycle.push(facts[node].name.to_string());
            return Some(cycle);
        }
        if !seen.insert(node) {
            return None;
        }
        active.push(node);
        for &target in &edges[node] {
            if let Some(cycle) = visit(target, facts, edges, active, seen) {
                return Some(cycle);
            }
        }
        active.pop();
        None
    }

    let mut active = Vec::new();
    let mut seen = HashSet::new();
    let nodes: Vec<usize> = (0..facts.len()).collect();
    for node in nodes {
        if let Some(cycle) = visit(node, &facts, &edges, &mut active, &mut seen) {
            return Some(cycle);
        }
    }
    None
}

pub(crate) fn remove_root(runtime: &mut PlainRuntime, root: u64) -> Result<()> {
    let Some(root_runtime) = runtime.roots.get(&root) else {
        return Ok(());
    };
    let graph = Arc::clone(&root_runtime.graph);
    let root_id = graph.lock().root;
    let mut ids = vec![root_id];
    let mut index = 0;
    {
        let graph_guard = graph.lock();
        while index < ids.len() {
            let id = ids[index];
            index += 1;
            if let Ok(invocation) = graph_guard.invocation(id) {
                ids.extend(invocation.children.iter().copied());
            }
        }
    }
    for id in ids.into_iter().rev() {
        retract_invocation_owned(&graph, &runtime.state, id)?;
    }
    with_txn(|txn| {
        txn.push_undo(Undo::RootRemoved {
            root,
            before: runtime.roots[&root].clone(),
        });
    });
    runtime.roots.remove(&root);
    Ok(())
}

/// Retracts one invocation's publication through the active transaction
/// without mutating the graph (removal never re-evaluates).
fn retract_invocation_owned(
    graph: &Arc<Mutex<PlainGraph>>,
    state: &Arc<Mutex<PlainState>>,
    id: u64,
) -> Result<()> {
    let (writes, name) = {
        let graph_guard = graph.lock();
        let invocation = graph_guard
            .invocations
            .iter()
            .find(|inv| inv.id == id)
            .ok_or_else(|| {
                Error::Internal(format!("owned invocation {id} absent in root {}", graph_guard.root).into())
            })?;
        (invocation.writes.clone(), invocation.function_name)
    };
    with_txn(|txn| -> Result<()> {
        let mut state = state.lock();
        for write in writes {
            txn.journal.retract(&mut state, write.view, write.name, write.key.as_ref(), id, name)?;
        }
        Ok(())
    })
}
pub(crate) fn initialize_dirty(runtime: &mut PlainRuntime, changes: &[FactChange]) {
    runtime.dirty.clear();
    let pairs: Vec<(TypeId, Option<Arc<dyn KeyValue>>, bool)> = changes
        .iter()
        .map(|change| {
            (
                change.view,
                Some(Arc::clone(&change.key)),
                change.presence_changed,
            )
        })
        .collect();
    let previous_pairs: Vec<(TypeId, Option<Arc<dyn KeyValue>>, bool)> = runtime
        .last_changed
        .iter()
        .map(|change| {
            (
                change.view,
                Some(Arc::clone(&change.key)),
                change.presence_changed,
            )
        })
        .collect();
    let mut queued = 0usize;
    for root in runtime.roots.values() {
        let install_ordinal = root.install_ordinal;
        let mut graph = root.graph.lock();
        if !Arc::ptr_eq(&graph.state, &runtime.state) {
            // Graphs created by isolated captures adopt the committed store.
            graph.state = Arc::clone(&runtime.state);
        }
        // Each epoch starts with a clean per-graph change log; residue from
        // the previous epoch would re-mark readers of already-committed facts.
        let _ = graph.take_changes();
        let mut ids: Vec<u64> = Vec::new();
        if !previous_pairs.is_empty() {
            graph.deps.mark_previous(&previous_pairs, |id| ids.push(id));
        }
        if !pairs.is_empty() {
            graph.deps.mark_current(&pairs, |id| ids.push(id));
        }
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            runtime.dirty.insert(crate::reactive::store::DirtyKey {
                root_install_ordinal: install_ordinal,
                invocation_ordinal: id,
                root: graph.root,
                invocation: id,
            });
            queued += 1;
        }
    }
    record_command_metric::<EngineWork>(|work| {
        work.exact_marks += queued as u64;
        work.queue_pushes += queued as u64;
    });
}

/// Queues one keyed child evaluation for `key`, creating the invocation
/// when absent. Returns the queued child id.
pub(crate) fn queue_family_child(
    runtime: &mut PlainRuntime,
    family_id: u64,
    key: Arc<dyn KeyValue>,
) -> Result<Option<u64>> {
    let Some(family) = runtime.families.get(&family_id) else {
        return Ok(None);
    };
    let graph = Arc::clone(&family.graph);
    let install_ordinal = family.install_ordinal;
    let view = family.view;
    let root_token = graph.lock().root;

    let call = (family.build_call)(Arc::clone(&key));
    let function_type = call.function_type();
    let existing = {
        let graph_guard = graph.lock();
        graph_guard.invocations.iter().find_map(|invocation| {
            if invocation.retired || invocation.parent != Some(graph_guard.root) {
                return None;
            }
            let identity = invocation.identity.as_ref()?;
            if identity.function != function_type || !identity.input.eq_value(key.as_ref()) {
                return None;
            }
            Some(invocation.id)
        })
    };

    let child = match existing {
        Some(child) => {
            graph.lock().invocation_mut(child)?.call = call;
            child
        }
        None => {
            let child = fresh_token();
            with_txn(|txn| txn.touch_invocation(root_token, &graph.lock(), child));
            let mut graph_guard = graph.lock();
            graph_guard.invocations.push(Invocation {
                id: child,
                parent: Some(root_token),
                identity: Some(CallIdentity {
                    file: "<keyed>",
                    line: 0,
                    column: 0,
                    function: function_type,
                    input: Arc::clone(&key),
                    occurrence: 0,
                    stable_input: true,
                }),
                call,
                function_name: family.view_name,
                reads: Vec::new(),
                writes: Vec::new(),
                result: None,
                fresh_sites: HashMap::new(),
                children: Vec::new(),
                seen_children: Vec::new(),
                emission_modes: HashMap::new(),
                dirty: false,
                retired: false,
            });
            drop(graph_guard);
            child
        }
    };

    runtime.dirty.insert(crate::reactive::store::DirtyKey {
        root_install_ordinal: install_ordinal,
        invocation_ordinal: child,
        root: root_token,
        invocation: child,
    });
    let _ = view;
    Ok(Some(child))
}

/// Schedules every family watching a changed view.
pub(crate) fn schedule_families(runtime: &mut PlainRuntime, changes: &[FactChange]) {
    for change in changes {
        let watchers: Vec<u64> = runtime
            .families
            .iter()
            .filter(|(_, family)| family.view == change.view)
            .map(|(family_id, _)| *family_id)
            .collect();
        for family_id in watchers {
            let _ = queue_family_child(runtime, family_id, Arc::clone(&change.key));
        }
    }
}

pub(crate) fn quiesce(runtime: &mut PlainRuntime) -> Result<u32> {
    let mut rounds = 0u32;
    while let Some(key) = runtime.dirty.pop() {
        let (root, id) = (key.root, key.invocation);
        if rounds >= 1_000_000 {
            return Err(Error::Internal(
                "plain computation run limit exceeded".into(),
            ));
        }
        // Retired or removed invocations hold no queue claim. Families are
        // resolved through the same ordered path as roots.
        let graph_opt = runtime
            .roots
            .get(&root)
            .map(|root_runtime| {
                let live = root_runtime
                    .graph
                    .lock()
                    .invocations
                    .iter()
                    .any(|invocation| invocation.id == id && !invocation.retired);
                live.then(|| Arc::clone(&root_runtime.graph))
            })
            .unwrap_or_else(|| {
                runtime.family_by_root.get(&root).and_then(|family_id| {
                    runtime.families.get(family_id).and_then(|family| {
                        let live = family
                            .graph
                            .lock()
                            .invocations
                            .iter()
                            .any(|invocation| invocation.id == id && !invocation.retired);
                        live.then(|| Arc::clone(&family.graph))
                    })
                })
            });
        let graph = match graph_opt {
            Some(graph) => graph,
            None => continue,
        };
        record_command_metric::<EngineWork>(|work| {
            work.queue_pops += 1;
        });
        // sweep inside evaluate_graph produces changes that downstream
        // dependents need. Skipping this drain on the absence-retirement
        // path strands those retractions — the root cause of the STLC
        // structural_pipeline test failure.
        let graph_changes = graph.lock().take_changes();
        mark_changes(runtime, &graph_changes);

        // A keyed child whose input vanished retires with its publication.
        if let Some(family_id) = runtime.family_by_root.get(&root).copied() {
            let (view, input_key) = {
                let graph_guard = graph.lock();
                let invocation = graph_guard
                    .invocations
                    .iter()
                    .find(|invocation| invocation.id == id)
                    .cloned();
                match invocation.and_then(|invocation| invocation.identity) {
                    Some(identity) => {
                        let view = runtime.families.get(&family_id).map(|family| family.view);
                        (view, Some(identity.input))
                    }
                    None => (None, None),
                }
            };
            if let (Some(view), Some(input_key)) = (view, input_key) {
                let still_present = runtime
                    .state
                    .lock()
                    .slot_index(view, input_key.as_ref())
                    .is_some();
                if !still_present {
                    let function_name = graph.lock().function_name_of(id)?;
                    let changes = retract_invocation(&graph, id, function_name)?;
                    mark_changes(runtime, &changes);
                    let mut graph_guard = graph.lock();
                    if let Some(position) = graph_guard
                        .invocations
                        .iter()
                        .position(|invocation| invocation.id == id)
                    {
                        graph_guard.invocations.remove(position);
                    }
                    let _ = family_id;
                    rounds = rounds.saturating_add(1);
                    continue;
                }
            }
        }
        let evaluation = evaluate_graph(&graph, id)?;
        // sweep inside evaluate_graph produces changes that downstream
        // dependents need.
        let graph_changes = graph.lock().take_changes();
        mark_changes(runtime, &graph_changes);
        if evaluation.1 {
            let parent = graph
                .lock()
                .invocation(id)
                .ok()
                .and_then(|invocation| invocation.parent);
            if let Some(parent) = parent {
                let install_ordinal = runtime
                    .roots
                    .get(&root)
                    .map(|root_runtime| root_runtime.install_ordinal)
                    .or_else(|| {
                        runtime
                            .family_by_root
                            .get(&root)
                            .and_then(|family_id| runtime.families.get(family_id))
                            .map(|family| family.install_ordinal)
                    });
                let is_family = runtime.family_by_root.contains_key(&root);
                if let Some(install_ordinal) = install_ordinal
                    && !(is_family && parent == graph.lock().root)
                {
                    runtime.dirty.insert(crate::reactive::store::DirtyKey {
                        root_install_ordinal: install_ordinal,
                        invocation_ordinal: parent,
                        root,
                        invocation: parent,
                    });
                }
            }
        }
        rounds = rounds.saturating_add(1);
    }
    Ok(rounds)
}

pub(crate) fn update_sinks(runtime: &PlainRuntime) {
    for root in runtime.roots.values() {
        let graph = root.graph.lock();
        if let Ok(invocation) = graph.invocation(graph.root)
            && let Some(output) = &invocation.result {
                (root.sink.update)(Arc::clone(output));
            }
    }
}
pub(crate) fn run_command<F>(runtime: &mut PlainRuntime, effects: F) -> Result<PlainCommandReport>
where
    F: FnOnce() -> Result<()>,
{
    let buffer = Arc::new(Mutex::new(CommandBuffer::default()));
    let pending = std::rc::Rc::new(PendingOverlay::default());
    let _metrics = push_metric_frame();
    let txn_frame = push_txn();
    ACTIVE.with(|active| {
        active.borrow_mut().push(ActiveDispatcher::Command(
            Arc::clone(&buffer),
            std::rc::Rc::clone(&pending),
        ));
    });
    let result = catch_unwind(AssertUnwindSafe(effects));
    ACTIVE.with(|active| {
        active.borrow_mut().pop();
    });

    let rollback = |txn: CommandTxn, runtime: &mut PlainRuntime| {
        rollback_txn(txn, &runtime.state, &mut runtime.roots);
        #[cfg(debug_assertions)]
        {
            // Touched-key presence must match between slots and the
            // UNTOUCHED committed root after a full rollback.
            let state = runtime.state.lock();
            for delta in &runtime.last_changed {
                let live = state.slot_index(delta.view, delta.key.as_ref()).is_some();
                let rooted = runtime
                    .committed
                    .view(delta.view)
                    .map(|view| view.lookup(delta.key.as_ref()).is_some())
                    .unwrap_or(false);
                debug_assert_eq!(live, rooted, "rollback invariant violated");
            }
        }
    };

    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let txn = take_txn();
            drop(txn_frame);
            rollback(txn, runtime);
            return Err(error);
        }
        Err(payload) => {
            let txn = take_txn();
            drop(txn_frame);
            rollback(txn, runtime);
            return Err(panic_error(payload));
        }
    }

    let round_changes = with_txn(|txn| -> Result<Vec<FactChange>> {
        let (direct_writes, patch_ops) = {
            let mut buffer = buffer.lock();
            (
                buffer.writes.clone(),
                std::mem::take(&mut buffer.patch_ops),
            )
        };
        let mut writes = IndexedWrites::from_writes(direct_writes);
        for write in patch_ops_to_indexed(patch_ops)?.into_writes() {
            writes.replace(write);
        }
        let writes = writes.ordered();
        let mut state = runtime.state.lock();
        let mut round_changes = Vec::new();
        for write in writes {
            if let Some(change) = txn.journal.write(
                &mut state,
                write.view,
                write.name,
                write.key,
                write.value,
                EXTERNAL_WRITER,
                "external",
                false,
            )? {
                round_changes.push(change);
            }
        }
        Ok(round_changes)
    });
    let round_changes = match round_changes {
        Ok(changes) => changes,
        Err(error) => {
            let txn = take_txn();
            drop(txn_frame);
            rollback(txn, runtime);
            return Err(error);
        }
    };
    let touched = with_txn(|txn| !txn.journal.is_empty());
    if !touched {
        drop(txn_frame);
        return Ok(PlainCommandReport {
            epoch: runtime.epoch,
            rounds: 0,
            changes: Vec::new(),
            engine: take_engine_work(),
            invocation_work: take_frame_metric::<InvocationWork>(),
            metrics: freeze_metric_frame(),
        });
    }

    initialize_dirty(runtime, &round_changes);
    // Families consume the command's RoundDelta AFTER the queue reset so
    // their children survive into quiescence.
    schedule_families(runtime, &round_changes);
    let rounds_result = quiesce(runtime);
    if let Err(error) = rounds_result {
        let txn = take_txn();
        drop(txn_frame);
        rollback(txn, runtime);
        return Err(error);
    }
    if let Some(views) = dependency_cycle(runtime) {
        let txn = take_txn();
        drop(txn_frame);
        rollback(txn, runtime);
        return Err(Error::DependencyCycle { views });
    }

    let final_changes = with_txn(|txn| txn.journal.commit_changes());
    let rounds = rounds_result.unwrap_or_default();
    let txn = take_txn();
    {
        let deltas = txn.journal.commit_deltas();
        runtime.committed.apply(&deltas);
        // Debug invariant: mutable slots and the committed root agree for
        // every touched key after commit (plan §5.1).
        let state = runtime.state.lock();
        for delta in &deltas {
            let live_present = state.slot_index(delta.view, delta.key.as_ref()).is_some();
            let root_present = runtime
                .committed
                .view(delta.view)
                .map(|view| view.lookup(delta.key.as_ref()).is_some())
                .unwrap_or(false);
            debug_assert!(
                live_present == root_present,
                "commit invariant violated: live={live_present} root={root_present}"
            );
        }
    }
    drop(txn_frame);
    runtime.epoch = runtime.epoch.saturating_add(1);
    runtime.last_changed = final_changes.clone();
    update_sinks(runtime);
    Ok(PlainCommandReport {
        epoch: runtime.epoch,
        rounds,
        changes: final_changes,
        engine: take_engine_work(),
        invocation_work: take_frame_metric::<InvocationWork>(),
        metrics: freeze_metric_frame(),
    })
}

/// Public-to-crate wrappers for engine.rs transaction control.
/// Builds a frozen plain-snapshot handle over the committed root. The
/// framework read boundary (plan §6) consumes this through the hidden
/// reactive root export.
#[doc(hidden)]
pub fn __snapshot_pub(runtime: &PlainRuntime) -> PlainSnapshot {
    snapshot(runtime)
}

/// Re-exported state-cell constructor for the framework seam.
pub(crate) fn state_cell_pub<T: StateValue>() -> StateCell<T> {
    state_cell::<T>()
}

/// Reads one committed fact without recording a reactive dependency.
/// Used by publication owners performing read-modify-write on their own
/// prior output (plan §5.5, barrier-solutions §2.3).
pub(crate) fn peek_committed_pub<V: View>(input: V::Input) -> Result<Option<Arc<V::Output>>> {
    ACTIVE_EVALS.with(|evals| {
        let frame = evals.borrow().last().cloned();
        let Some(frame) = frame else {
            return Err(Error::EffectOutsideRun {
                effect: "peek_committed".to_string(),
                view: std::any::type_name::<V>().to_string(),
            });
        };
        let graph = Arc::clone(&frame.graph);
        let state = Arc::clone(&graph.lock().state);
        let value = state.lock().read(TypeId::of::<V>(), &input);
        match value {
            Some(v) => {
                let typed = downcast_arc::<V::Output>(v);
                Ok(typed)
            }
            None => Ok(None),
        }
    })
}

pub(crate) fn erased_noop_pub() -> Arc<dyn ErasedCall> {
    erased_noop()
}

/// Retires one keyed child without touching its graph's scheduling state:
/// retracts owned facts through the active transaction.
pub(crate) fn retract_child_owned(
    graph: &Arc<Mutex<PlainGraph>>,
    state: &Arc<Mutex<PlainState>>,
    id: u64,
) -> Result<()> {
    let _ = state;
    let _ = retract_invocation(graph, id, "<keyed>")?;
    Ok(())
}

pub(crate) fn push_txn_pub() -> TxnFrame {
    push_txn()
}

pub(crate) fn take_txn_pub() -> CommandTxn {
    take_txn()
}

pub(crate) fn rollback_txn_pub(
    txn: CommandTxn,
    state: &Arc<Mutex<PlainState>>,
    roots: &mut BTreeMap<u64, RootRuntime>,
) {
    rollback_txn(txn, state, roots)
}

pub(crate) fn with_txn_pub<R>(f: impl FnOnce(&mut CommandTxn) -> R) -> R {
    with_txn(f)
}

/// Takes the active transaction out of its frame.
fn take_txn() -> CommandTxn {
    ACTIVE_TXN.with(|txn| {
        let frame = txn.borrow().as_ref().expect("active command txn").clone();
        frame.borrow_mut().take_commands()
    })
}

#[derive(Clone)]
pub(crate) struct PlainCommandReport {
    pub epoch: u64,
    pub rounds: u32,
    pub changes: Vec<FactChange>,
    pub engine: EngineWork,
    pub invocation_work: InvocationWork,
    pub metrics: Arc<MetricExtensions>,
}


// ---------------------------------------------------------------------------
// Invocation-local state slots (plan §5.6)
// ---------------------------------------------------------------------------

/// Deep-immutability marker for values stored in [`StateCell`]s.
///
/// # Safety
/// Implementors guarantee that no alias can mutate or observe mutation of
/// the value after it is handed to the engine. Ownership-only `Arc`
/// reference-count changes do not violate the contract. Interior mutability
/// (`Mutex`, atomics, cells, raw pointers) breaks rollback and must never
/// reach this trait; the audited impls below and the `derive(StateValue)`
/// macro are the only supported routes.
pub unsafe trait StateValue: Send + Sync + std::fmt::Debug + 'static {}

macro_rules! impl_state_value {
    ($($ty:ty),* $(,)?) => {
        $(unsafe impl StateValue for $ty {})*
    };
}

impl_state_value!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, bool, char, String, ());
unsafe impl<T: StateValue> StateValue for Arc<T> {}
unsafe impl<T: StateValue> StateValue for Vec<T> {}
unsafe impl<T: StateValue> StateValue for Option<T> {}
unsafe impl<K: StateValue, V: StateValue> StateValue for std::collections::BTreeMap<K, V> {}
unsafe impl<K: StateValue, V: StateValue> StateValue for std::collections::HashMap<K, V>
where
    K: std::hash::Hash + Eq,
{}
unsafe impl<A: StateValue, B: StateValue> StateValue for (A, B) {}

/// One stored slot: type-checked value plus a generation for stale-handle
/// detection after retirement.
#[derive(Clone)]
pub(crate) struct InvocationSlot {
    type_id: TypeId,
    value: Arc<dyn Any + Send + Sync>,
    generation: u64,
}

/// Typed handle to the active invocation's private state slot.
///
/// The handle is neither cloneable nor readable outside the invocation:
/// `with`/`set`/`clear` resolve through the running evaluation. Values roll
/// back with the command transaction and drop on retirement.
pub struct StateCell<T: StateValue> {
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T: StateValue> std::fmt::Debug for StateCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StateCell")
    }
}

/// Mints a typed state-cell handle. Construction is infallible; resolution
/// errors surface at use time inside the running invocation.
pub fn state_cell<T: StateValue>() -> StateCell<T> {
    StateCell {
        _marker: std::marker::PhantomData,
    }
}

impl<T: StateValue> StateCell<T> {
    fn resolve() -> Result<(Arc<Mutex<PlainGraph>>, u64)> {
        ACTIVE_EVALS.with(|active| {
            let frame = active.borrow().last().cloned();
            let Some(frame) = frame else {
                return Err(Error::EffectOutsideRun {
                    effect: "state_cell".to_string(),
                    view: std::any::type_name::<T>().to_string(),
                });
            };
            Ok((Arc::clone(&frame.graph), frame.id))
        })
    }

    /// Borrows the slot's current value (committed or staged) inside `f`.
    /// Records no reactive dependency.
    pub fn with<R>(&self, f: impl FnOnce(Option<&T>) -> R) -> Result<R> {
        let (graph, id) = Self::resolve()?;
        let graph_guard = graph.lock();
        let slot = graph_guard.state_slots.get(&id);
        let value = match slot {
            Some(entry) if entry.type_id == TypeId::of::<T>() => {
                let typed = entry
                    .value
                    .downcast_ref::<T>()
                    .ok_or_else(|| Error::Internal("state slot type mismatch".into()))?;
                Some(typed)
            }
            Some(_) => {
                return Err(Error::Internal("state slot type mismatch".into()));
            }
            None => None,
        };
        Ok(f(value))
    }

    /// Replaces the slot's value; the previous value joins the transaction
    /// journal for rollback. Requires no `Clone` or equality on `T`.
    pub fn set(&self, value: T) -> Result<()> {
        let (graph, id) = Self::resolve()?;
        let mut graph_guard = graph.lock();
        with_txn(|txn| {
            txn.touch_state_slot(graph_guard.root, &graph_guard, id);
        });
        let generation = graph_guard
            .state_slots
            .get(&id)
            .map(|entry| entry.generation + 1)
            .unwrap_or(1);
        graph_guard.state_slots.insert(
            id,
            InvocationSlot {
                type_id: TypeId::of::<T>(),
                value: Arc::new(value),
                generation,
            },
        );
        Ok(())
    }

    /// Drops the slot; retirement drops it unconditionally.
    pub fn clear(&self) -> Result<()> {
        let (graph, id) = Self::resolve()?;
        let mut graph_guard = graph.lock();
        with_txn(|txn| {
            txn.touch_state_slot(graph_guard.root, &graph_guard, id);
            graph_guard.state_slots.remove(&id);
        });
        Ok(())
    }
}
