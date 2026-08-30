use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::{self, Debug};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use smallvec::SmallVec;

use crate::reactive::engine::{EngineWork, InvocationIdentity, InvocationWork};
use crate::reactive::error::{Error, Result};
pub(crate) use crate::reactive::store::PlainState as PlainStatePub;
use crate::reactive::store::{ErasedFactKey, FactJournal, Hamt, PlainState, SnapshotRoot};
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
    Invocation {
        root: u64,
        before: Option<Invocation>,
    },
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
    /// Command-private machine participants (Cut B bridge): framework
    /// components push infallible restore closures that run in reverse
    /// after fact rollback on any failure. A committed command drops them.
    pub private_undos: Vec<Box<dyn FnOnce() + Send>>,
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
                private_undos: Vec::new(),
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
    /// Pre-eval ownership roots and read sets of running evaluations.
    static OLD_WRITES: RefCell<Vec<(u64, ComponentWrites, Vec<ReadDep>)>> =
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
fn rollback_txn(
    mut txn: CommandTxn,
    state: &Arc<Mutex<PlainState>>,
    roots: &mut BTreeMap<u64, RootRuntime>,
) {
    txn.journal.rollback(&mut state.lock());
    // Private participants restore in reverse registration order (Cut B):
    // staged machine roots return to their pre-command values.
    for undo in txn.private_undos.drain(..).rev() {
        undo();
    }
    // Dependency rows rebuild from restored reads; the dirty queue resets.
    for undo in txn.undo.iter().rev() {
        if let Undo::Invocation { root, before } = undo {
            let Some(root_runtime) = roots.get(root) else {
                continue;
            };
            let mut graph = root_runtime.graph.lock();
            match before {
                Some(before_invocation) => {
                    // Drop whatever rows the aborted evaluation installed,
                    // then reinstall the pre-command rows verbatim.
                    let current_reads: Vec<(TypeId, Option<Arc<dyn KeyValue>>, bool, bool)> = graph
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
                        let restored: Vec<(TypeId, Option<Arc<dyn KeyValue>>, bool, bool)> =
                            before_invocation
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
                            if let Some(slot) =
                                graph.invocations.iter_mut().find(|inv| inv.id == before.id)
                            {
                                *slot = before;
                            } else {
                                graph.invocations.push(before);
                            }
                        }
                        None => {
                            // Creation undone: remove the newest matching id.
                            if let Some(position) = graph.invocations.iter().rposition(|_inv| true)
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

/// Persistent ownership of one component instance's outputs.
///
/// Each view keeps an independent HAMT root. Evaluating a patch-mode
/// component clones these roots and path-copies only the touched keys;
/// complete enumeration is reserved for replace-mode omission diffs,
/// retirement, and explicit audits.
#[derive(Clone, Default)]
pub(crate) struct ComponentWrites {
    views: SmallVec<[OwnedView; 2]>,
}

#[derive(Clone)]
struct OwnedView {
    view: TypeId,
    name: &'static str,
    mode: EmissionMode,
    facts: Hamt<ErasedFactKey, PlainWrite>,
}

impl ComponentWrites {
    fn view(&self, view: TypeId) -> Option<&OwnedView> {
        self.views.iter().find(|owned| owned.view == view)
    }

    fn view_mut(&mut self, view: TypeId) -> Option<&mut OwnedView> {
        self.views.iter_mut().find(|owned| owned.view == view)
    }

    fn ensure_view(
        &mut self,
        view: TypeId,
        name: &'static str,
        mode: EmissionMode,
    ) -> &mut OwnedView {
        if let Some(index) = self.views.iter().position(|owned| owned.view == view) {
            let owned = &mut self.views[index];
            owned.name = name;
            owned.mode = mode;
            return owned;
        }
        self.views.push(OwnedView {
            view,
            name,
            mode,
            facts: Hamt::default(),
        });
        self.views.last_mut().expect("just inserted")
    }

    fn mode(&self, view: TypeId) -> Option<EmissionMode> {
        self.view(view).map(|owned| owned.mode)
    }

    fn lookup(&self, view: TypeId, key: &dyn KeyValue) -> Option<&PlainWrite> {
        let owned = self.view(view)?;
        let key = ErasedFactKey::new(view, key.clone_key());
        owned.facts.get(&key)
    }

    fn insert(&mut self, write: PlainWrite, mode: EmissionMode) -> Option<PlainWrite> {
        let view = write.view;
        let key = ErasedFactKey::new(view, Arc::clone(&write.key));
        let previous = self
            .view(view)
            .and_then(|owned| owned.facts.get(&key))
            .cloned();
        self.ensure_view(view, write.view_name, mode)
            .facts
            .insert(key, write);
        previous
    }

    fn remove(&mut self, view: TypeId, key: &dyn KeyValue) -> Option<PlainWrite> {
        let erased = ErasedFactKey::new(view, key.clone_key());
        let previous = self
            .view(view)
            .and_then(|owned| owned.facts.get(&erased))
            .cloned();
        if let Some(owned) = self.view_mut(view) {
            owned.facts.remove(&erased);
        }
        previous
    }

    /// Enumerates one view's owned facts. Callers use this only for
    /// replace-mode diffs, retirement, audits, and report materialization.
    fn view_entries(&self, view: TypeId) -> Vec<PlainWrite> {
        self.view(view)
            .map(|owned| owned.facts.iter().map(|(_, write)| write.clone()).collect())
            .unwrap_or_default()
    }

    /// Enumerates all owned facts for lifecycle teardown and audits.
    fn all_entries(&self) -> Vec<PlainWrite> {
        self.views
            .iter()
            .flat_map(|owned| owned.facts.iter().map(|(_, write)| write.clone()))
            .collect()
    }

    fn contains(&self, view: TypeId, key: &dyn KeyValue) -> bool {
        self.lookup(view, key).is_some()
    }

    fn is_empty(&self) -> bool {
        self.views.iter().all(|owned| owned.facts.is_empty())
    }

    fn len(&self) -> usize {
        self.views.iter().map(|owned| owned.facts.len()).sum()
    }

    fn iter_views(&self) -> impl Iterator<Item = &OwnedView> {
        self.views.iter()
    }
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
        self.function == other.function
            && self.input.eq_value(other.input.as_ref())
            && if self.stable_input && other.stable_input {
                true
            } else {
                self.file == other.file
                    && self.line == other.line
                    && self.column == other.column
                    && self.occurrence == other.occurrence
            }
    }
}

pub(crate) trait ErasedCall: Send + Sync {
    fn invoke(&self) -> Result<Arc<dyn Value>>;
    fn function_type(&self) -> TypeId;
    fn definition_type(&self) -> Option<TypeId> {
        None
    }
    fn input_key(&self) -> Arc<dyn KeyValue>;
    fn function_name(&self) -> &'static str;
    /// Compares replaceable value parameters for the key-matched component
    /// call. Legacy calls have no separate props and always compare equal.
    fn props_equal(&self, _other: &dyn ErasedCall) -> bool {
        true
    }
    fn props_any(&self) -> Option<&(dyn Any + Send + Sync)> {
        None
    }
    /// Stable member descriptor used only for diagnostics/reaction reports.
    fn case_name(&self) -> Option<&'static str> {
        None
    }
}

struct TypedCall<F, A, B> {
    /// Component-definition marker when this call belongs to a first-class
    /// component (Cut C): identity and cycle checks compare against it.
    definition: Option<TypeId>,
    function: F,
    input: A,
    apply_output: Option<Arc<dyn Fn(&B) -> Result<()> + Send + Sync>>,
    _marker: std::marker::PhantomData<fn() -> B>,
}
impl<F, A, B> ErasedCall for TypedCall<F, A, B>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    fn invoke(&self) -> Result<Arc<dyn Value>> {
        let output = (self.function)(self.input.clone())?;
        if let Some(apply_output) = &self.apply_output {
            apply_output(&output)?;
        }
        Ok(Arc::new(output))
    }
    fn function_type(&self) -> TypeId {
        self.definition.unwrap_or_else(|| TypeId::of::<F>())
    }

    fn definition_type(&self) -> Option<TypeId> {
        self.definition
    }

    fn input_key(&self) -> Arc<dyn KeyValue> {
        Arc::new(self.input.clone())
    }

    fn function_name(&self) -> &'static str {
        std::any::type_name::<F>()
    }
}

/// An ordinary component call with identity separated from replaceable props.
/// The erased call owns both values so a dirty invocation can be reevaluated
/// without changing its lifecycle or output ownership roots.
struct ComponentCall<F, K, P, B> {
    definition: TypeId,
    function: F,
    key: K,
    props: P,
    descriptor: &'static str,
    case: Option<&'static str>,
    _marker: std::marker::PhantomData<fn() -> B>,
}

impl<F, K, P, B> ErasedCall for ComponentCall<F, K, P, B>
where
    F: Fn(K, P) -> Result<B> + Clone + Send + Sync + 'static,
    K: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    P: Clone + PartialEq + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    fn invoke(&self) -> Result<Arc<dyn Value>> {
        Ok(Arc::new((self.function)(self.key.clone(), self.props.clone())?))
    }

    fn function_type(&self) -> TypeId {
        self.definition
    }

    fn definition_type(&self) -> Option<TypeId> {
        Some(self.definition)
    }

    fn input_key(&self) -> Arc<dyn KeyValue> {
        Arc::new(self.key.clone())
    }

    fn function_name(&self) -> &'static str {
        self.descriptor
    }

    fn props_equal(&self, other: &dyn ErasedCall) -> bool {
        let Some(other) = other.props_any().and_then(|value| value.downcast_ref::<P>()) else {
            return false;
        };
        self.props == *other
    }

    fn props_any(&self) -> Option<&(dyn Any + Send + Sync)> {
        Some(&self.props)
    }

    fn case_name(&self) -> Option<&'static str> {
        self.case
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
        definition: None,
        function,
        input,
        apply_output: None,
        _marker: std::marker::PhantomData,
    })
}
/// Like [`erased_call`], but stamps a component-definition marker as the
/// effective identity type (Cut C).
pub(crate) fn erased_call_with_definition<F, A, B>(
    definition: TypeId,
    function: F,
    input: A,
) -> Arc<dyn ErasedCall>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    erased_call_with_definition_and_apply(definition, function, input, None)
}

pub(crate) fn erased_call_with_definition_and_apply<F, A, B>(
    definition: TypeId,
    function: F,
    input: A,
    apply_output: Option<Arc<dyn Fn(&B) -> Result<()> + Send + Sync>>,
) -> Arc<dyn ErasedCall>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    Arc::new(TypedCall {
        definition: Some(definition),
        function,
        input,
        apply_output,
        _marker: std::marker::PhantomData,
    })
}

pub(crate) fn erased_component_call_with_definition<F, K, P, B>(
    definition: TypeId,
    descriptor: &'static str,
    case: Option<&'static str>,
    function: F,
    key: K,
    props: P,
) -> Arc<dyn ErasedCall>
where
    F: Fn(K, P) -> Result<B> + Clone + Send + Sync + 'static,
    K: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    P: Clone + PartialEq + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    Arc::new(ComponentCall {
        definition,
        function,
        key,
        props,
        descriptor,
        case,
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
    /// Committed ownership roots. This is never rebuilt by ordinary
    /// patch-mode evaluations.
    writes: ComponentWrites,
    /// Writes authored by the body currently being evaluated. These are
    /// transient and are folded into `writes` only after the body succeeds.
    pending_writes: Vec<PlainWrite>,
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
        self.invocation(id)
            .map(|invocation| invocation.function_name)
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
            .filter(|invocation| invocation.parent == Some(self.root) && !invocation.retired)
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
                fresh_sites: HashMap::new(),
                reads: Vec::new(),
                writes: ComponentWrites::default(),
                pending_writes: Vec::new(),
                children: Vec::new(),
                seen_children: Vec::new(),
                emission_modes: HashMap::new(),
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
                Error::Internal(
                    format!("invocation {id} missing/retired in root {}", self.root).into(),
                )
            })
    }

    fn invocation_mut(&mut self, id: u64) -> Result<&mut Invocation> {
        self.invocations
            .iter_mut()
            .find(|invocation| invocation.id == id && !invocation.retired)
            .ok_or_else(|| {
                Error::Internal(
                    format!(
                        "invocation {id} missing/retired (mut) in root {}",
                        self.root
                    )
                    .into(),
                )
            })
    }

    fn register<V: View>(&mut self) {
        if std::env::var_os("PLINGO_TRACE_FACTS").is_some() {
            eprintln!("registered {:?} {}", TypeId::of::<V>(), V::name());
        }
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
        self.slots
            .get(&TypeId::of::<T>())
            .and_then(|slot| slot.downcast_ref::<T>())
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
        self.entries.borrow_mut().put(view, Arc::clone(key), value);
    }

    fn get(&self, view: TypeId, key: &Arc<dyn KeyValue>) -> Option<Option<Arc<dyn Value>>> {
        self.entries.borrow().get(view, key.as_ref())
    }
}

struct ActiveEval {
    graph: Arc<Mutex<PlainGraph>>,
    id: u64,
    /// The invocation's owned writes as they stood when this evaluation
    /// started (Cut E ownership diff baseline).
    pre_eval_writes: PreEvalOwned,
    occurrences: RefCell<Vec<(CallBase, u64)>>,
    rendered_slots: RefCell<HashSet<TypeId>>,
}

impl Clone for ActiveEval {
    fn clone(&self) -> Self {
        Self {
            graph: Arc::clone(&self.graph),
            id: self.id,
            pre_eval_writes: self.pre_eval_writes.clone(),
            occurrences: RefCell::new(self.occurrences.borrow().clone()),
            rendered_slots: RefCell::new(self.rendered_slots.borrow().clone()),
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

/// Cut E ownership-diff baseline: the full pre-evaluation owned write set
/// across every view the invocation touched.
#[derive(Clone, Default)]
pub(crate) struct PreEvalOwned {
    writes: Vec<PlainWrite>,
}

impl PreEvalOwned {
    fn snapshot(writes: &ComponentWrites) -> Self {
        Self {
            writes: writes.all_entries(),
        }
    }
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
                Ok(value.and_then(downcast_arc::<V::Output>))
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
        let value: Option<Arc<dyn Value>> = value.map(|value| value as Arc<dyn Value>);
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(_, _, pending)
            | ActiveDispatcherHandle::Command(pending, _) => {
                pending.put(TypeId::of::<V>(), &key, value);
            }
        }
    }

    /// Looks up the invocation's pending write for one fact, if any.
    pub fn pending_get<V: View>(&self, key: &V::Input) -> Option<Option<Arc<V::Output>>> {
        let key: Arc<dyn KeyValue> = Arc::new(key.clone());
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(_, _, pending)
            | ActiveDispatcherHandle::Command(pending, _) => pending
                .get(TypeId::of::<V>(), &key)
                .map(|value| value.and_then(downcast_arc::<V::Output>)),
        }
    }

    /// Declares one patch operation against a view (plan §5.5).
    ///
    /// Modes freeze per (invocation, view): mixing [`emit_view`] with
    /// [`EffectContext::emit_patch`] for the same view returns
    /// [`Error::MixedEmissionMode`].
    /// Cut E ownership diff: stage a Remove op from an already-erased key.
    pub(crate) fn emit_patch_erased<V: View>(&self, key: Arc<dyn KeyValue>) -> Result<()> {
        let view = TypeId::of::<V>();
        match &self.dispatcher {
            ActiveDispatcherHandle::Eval(graph, id, pending) => {
                let mut graph_guard = graph.lock();
                graph_guard.register::<V>();
                let invocation = graph_guard.invocation_mut(*id)?;
                match invocation.emission_modes.get(&view) {
                    Some(EmissionMode::Replace) => {
                        return Err(Error::MixedEmissionMode {
                            view: V::name().to_string(),
                        });
                    }
                    _ => {}
                }
                drop(graph_guard);
                pending.push_patch(PatchOp {
                    view,
                    view_name: V::name(),
                    key,
                    kind: PatchOpKind::Remove,
                });
                Ok(())
            }
            ActiveDispatcherHandle::Command(_, buffer) => {
                buffer.lock().modes.insert(view, EmissionMode::Patch);
                buffer.lock().patch_ops.push(PatchOp {
                    view,
                    view_name: V::name(),
                    key,
                    kind: PatchOpKind::Remove,
                });
                Ok(())
            }
        }
    }

    pub fn emit_patch<V: View>(&self, key: V::Input, value: Option<V::Output>) -> Result<()> {
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
                        invocation
                            .emission_modes
                            .insert(view, EmissionMode::Replace);
                    }
                }
                invocation.pending_writes.push(PlainWrite {
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
                ActiveDispatcherHandle::Command(std::rc::Rc::clone(pending), Arc::clone(buffer))
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
        if std::env::var_os("PLINGO_TRACE_EVAL").is_some()
            && invocation.function_name.contains("name_resolve")
        {
            eprintln!(
                "eval-start id={} function={} children={:?}",
                id, invocation.function_name, invocation.children
            );
        }
        let old_writes = std::mem::take(&mut invocation.writes);
        let old_reads = std::mem::take(&mut invocation.reads);
        invocation.pending_writes.clear();
        invocation.fresh_sites.clear();
        invocation.seen_children.clear();
        // Emission modes freeze per evaluation only (plan §5.5).
        invocation.emission_modes.clear();
        invocation.dirty = false;
        OLD_WRITES.with(|slot| slot.borrow_mut().push((id, old_writes, old_reads)));
        Arc::clone(&invocation.call)
    };
    let pending = std::rc::Rc::new(PendingOverlay::default());
    let pre_eval_owned = OLD_WRITES.with(|slot| {
        slot.borrow()
            .iter()
            .rev()
            .find(|(eval_id, _, _)| *eval_id == id)
            .map(|(_, writes, _)| PreEvalOwned::snapshot(writes))
            .unwrap_or_default()
    });
    ACTIVE_EVALS.with(|active| {
        active.borrow_mut().push(ActiveEval {
            graph: Arc::clone(graph),
            id,
            pre_eval_writes: pre_eval_owned,
            occurrences: RefCell::new(Vec::new()),
            rendered_slots: RefCell::new(HashSet::new()),
        });
    });
    ACTIVE.with(|active| {
        active.borrow_mut().push(ActiveDispatcher::Eval(
            Arc::clone(graph),
            id,
            std::rc::Rc::clone(&pending),
        ));
    });
    let result = catch_unwind(AssertUnwindSafe(|| call.invoke()));
    ACTIVE.with(|active| {
        active.borrow_mut().pop();
    });
    ACTIVE_EVALS.with(|active| {
        active.borrow_mut().pop();
    });

    // Pop this evaluation's baseline slot unconditionally: failed
    // evaluations must not leak pre-eval ownership entries.
    let (old_writes, old_read_deps) = OLD_WRITES.with(|slot| {
        let index = slot
            .borrow_mut()
            .iter()
            .rposition(|(eval_id, _, _)| *eval_id == id);
        match index {
            Some(index) => {
                let (_, writes, reads) = slot.borrow_mut().remove(index);
                (writes, reads)
            }
            None => (ComponentWrites::default(), Vec::new()),
        }
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
    let emission_modes = graph_guard.invocation(id)?.emission_modes.clone();
    // Patch-mode views carried over from the previous evaluation keep
    // their ownership unless this body switched the view to Replace.
    let mut patch_views: HashSet<TypeId> = old_writes
        .iter_views()
        .filter(|owned| owned.mode == EmissionMode::Patch)
        .filter(|owned| !matches!(emission_modes.get(&owned.view), Some(EmissionMode::Replace)))
        .map(|owned| owned.view)
        .collect();
    patch_views.extend(
        emission_modes
            .iter()
            .filter_map(|(view, mode)| matches!(mode, EmissionMode::Patch).then_some(*view)),
    );
    // One enumeration of the pre-eval ownership roots feeds both the
    // reaction capture and the replace-mode omission diff below.
    let old_entries = old_writes.all_entries();

    // Build one exact-key index for this candidate (Cut D touched-key
    // patching, follow-up plan sections 16.2-16.3): only keys the body
    // authored this evaluation enter the journal. Untouched patch-owned
    // keys keep ownership pointer-identically via `retained` and are never
    // re-journalled, so one-key work is independent of the instance's
    // owned domain.
    let direct_writes = std::mem::take(&mut graph_guard.invocation_mut(id)?.pending_writes);
    let mut candidate_index = IndexedWrites::from_writes(direct_writes);
    let patch_ops = std::mem::take(&mut *pending.patch_ops.borrow_mut());
    let patch_index = patch_ops_to_indexed(patch_ops)?;
    for write in patch_index.into_writes() {
        candidate_index.replace(write);
    }
    let candidate = candidate_index.ordered();
    let retained: Vec<PlainWrite> = if patch_views.is_empty() {
        Vec::new()
    } else {
        patch_views
            .iter()
            .flat_map(|view| old_writes.view_entries(*view))
            .filter(|write| !candidate_index.contains(write))
            .collect()
    };
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
    let retracts: Vec<PlainWrite> = old_entries
        .iter()
        .filter(|previous| {
            !candidate_index.contains(previous)
                && !retained.iter().any(|keep| {
                    keep.view == previous.view && keep.key.eq_value(previous.key.as_ref())
                })
        })
        .cloned()
        .collect();
    let function_name = graph_guard.invocation(id)?.function_name;
    let state = Arc::clone(&graph_guard.state);
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

    // Ownership fold: retracted keys and patch tombstones leave the
    // invocation's persistent roots; authored upserts enter. Patch-view
    // keys the body never touched keep their HAMT position (structural
    // sharing), so ordinary patch evaluations never enumerate their whole
    // owned domain.
    let mut owned = old_writes;
    for write in &retracts {
        owned.remove(write.view, write.key.as_ref());
    }
    for write in candidate.iter().filter(|write| write.value.is_none()) {
        owned.remove(write.view, write.key.as_ref());
    }
    for write in candidate.iter().filter(|write| write.value.is_some()) {
        let mode = emission_modes.get(&write.view).copied().unwrap_or_else(|| {
            if write.name == "patch" {
                EmissionMode::Patch
            } else {
                EmissionMode::Replace
            }
        });
        owned.insert(write.clone(), mode);
    }

    let mut graph_guard = graph.lock();
    let previous_result = graph_guard.invocation(id)?.result.clone();
    let changed = previous_result
        .as_ref()
        .is_none_or(|old| !old.value_eq(result.as_ref()));
    // Reaction capture: record the exact driving element, read edges, and
    // output edges of this evaluation (observation-only; the metric frame
    // is discarded with a failed command). Gated: consumers opt in.
    if crate::reactive::reaction::capture_enabled() {
        let invocation = graph_guard.invocation(id)?;
        let definition = invocation.function_name;
        let (callsite, driving_element) = match invocation.identity.as_ref() {
            Some(identity) => (
                format!("{}:{}:{}", identity.file, identity.line, identity.column),
                format!("{:?}", identity.input),
            ),
            None => (String::new(), String::new()),
        };
        let mut reads: Vec<crate::reactive::reaction::ElementEdge> = Vec::new();
        for read in &invocation.reads {
            match read.key.as_ref() {
                Some(key) => {
                    let mut element = format!("{key:?}");
                    if read.temporal {
                        element.push_str("@previous");
                    }
                    reads.push(crate::reactive::reaction::ElementEdge {
                        view: read.name,
                        element,
                    });
                }
                None => {
                    record_command_metric::<crate::reactive::reaction::ReactionDigest>(|digest| {
                        digest.push_broad_enumeration(crate::reactive::reaction::ElementEdge {
                            view: read.name,
                            element: "<domain>".to_owned(),
                        });
                    })
                }
            }
        }
        let mut outputs: Vec<crate::reactive::reaction::OutputEdge> = Vec::new();
        for write in candidate.iter() {
            let changed_here = changes
                .iter()
                .any(|change| change.view == write.view && change.key.eq_value(write.key.as_ref()));
            let committed = if !changed_here {
                None
            } else if old_entries
                .iter()
                .any(|old| old.view == write.view && old.key.eq_value(write.key.as_ref()))
            {
                Some("update")
            } else {
                Some("insert")
            };
            outputs.push(crate::reactive::reaction::OutputEdge {
                view: write.view_name,
                element: format!("{:?}", write.key),
                committed,
            });
        }
        for write in old_entries.iter() {
            let retained = candidate
                .iter()
                .any(|next| next.view == write.view && next.key.eq_value(write.key.as_ref()));
            if !retained {
                outputs.push(crate::reactive::reaction::OutputEdge {
                    view: write.view_name,
                    element: format!("{:?}", write.key),
                    committed: Some("retract"),
                });
            }
        }
        record_command_metric::<crate::reactive::reaction::ReactionDigest>(|digest| {
            digest.push_evaluation(crate::reactive::reaction::EvaluatedComponent {
                definition,
                callsite,
                driving_element,
                reads,
                outputs,
            });
        });
    }

    {
        let new_read_rows: Vec<(TypeId, Option<Arc<dyn KeyValue>>, bool, bool)> = graph_guard
            .invocation(id)?
            .reads
            .iter()
            .map(|read| (read.view, read.key.clone(), read.temporal, read.keyset))
            .collect();
        let old_read_rows: Vec<(TypeId, Option<Arc<dyn KeyValue>>, bool, bool)> = old_read_deps
            .iter()
            .map(|read| (read.view, read.key.clone(), read.temporal, read.keyset))
            .collect();
        graph_guard.deps.replace(&new_read_rows, &old_read_rows, id);
        let invocation = graph_guard.invocation_mut(id)?;
        if std::env::var_os("PLINGO_TRACE_QUEUE").is_some()
            && invocation.function_name.contains("tree_semantic")
        {
            eprintln!(
                "reads function={} id={} reads={:?}",
                invocation.function_name,
                id,
                invocation
                    .reads
                    .iter()
                    .map(|read| (
                        read.name,
                        read.view,
                        read.key.is_some(),
                        read.temporal,
                        read.keyset
                    ))
                    .collect::<Vec<_>>()
            );
        }
        invocation.result = Some(Arc::clone(&result));
        invocation.writes = owned;
        invocation.children = seen_children.clone();
        if std::env::var_os("PLINGO_TRACE_EVAL").is_some()
            && invocation.function_name.contains("name_resolve")
        {
            eprintln!(
                "eval-end id={} function={} children={:?} seen={:?}",
                id, invocation.function_name, invocation.children, seen_children
            );
        }
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
        self.entries.into_iter().map(|entry| entry.write).collect()
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
        let duplicate_view = write.view_name;
        if !writes.insert_if_absent(write) {
            return Err(Error::DuplicatePatchKey {
                view: duplicate_view.to_string(),
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
            invocation.writes.all_entries(),
            invocation.function_name,
            invocation
                .reads
                .iter()
                .map(|read| (read.view, read.key.clone(), read.temporal, read.keyset))
                .collect::<Vec<_>>(),
        )
    };
    if std::env::var_os("PLINGO_TRACE_RETRACT").is_some() {
        eprintln!(
            "retract id={} function={} children={children:?} writes={}",
            id,
            name,
            writes.len()
        );
    }
    let mut changes = Vec::new();
    for child in children {
        changes.extend(retract_invocation(graph, child, name)?);
    }
    // Reaction capture: attribute the exact retraction domain to this
    // invocation (observation-only). Gated: consumers opt in.
    if crate::reactive::reaction::capture_enabled() {
        let retired_edges: Vec<crate::reactive::reaction::ElementEdge> = {
            let graph_guard = graph.lock();
            graph_guard
                .invocations
                .iter()
                .find(|invocation| invocation.id == id)
                .map(|invocation| {
                    invocation
                        .writes
                        .all_entries()
                        .iter()
                        .map(|write| crate::reactive::reaction::ElementEdge {
                            view: write.view_name,
                            element: format!("{:?}", write.key),
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let identity_info = {
            let graph_guard = graph.lock();
            graph_guard
                .invocations
                .iter()
                .find(|invocation| invocation.id == id)
                .and_then(|invocation| invocation.identity.clone())
                .map(|identity| {
                    (
                        format!("{}:{}:{}", identity.file, identity.line, identity.column),
                        format!("{:?}", identity.input),
                    )
                })
        };
        let (retired_callsite, retired_driving) = match &identity_info {
            Some((callsite, driving)) => (callsite.clone(), driving.clone()),
            None => (String::new(), String::new()),
        };
        record_command_metric::<crate::reactive::reaction::ReactionDigest>(|digest| {
            digest.push_retirement(crate::reactive::reaction::RetiredComponent {
                definition: name,
                callsite: retired_callsite,
                driving_element: retired_driving,
                retracted_outputs: retired_edges,
            });
        });
    }
    // Retired invocations hold no dependency rows: they must never be
    // marked again by later changes.
    graph.lock().deps.remove_all(&read_rows, id);
    let state = Arc::clone(&graph.lock().state);
    with_txn(|txn| -> Result<()> {
        let mut state = state.lock();
        for write in writes {
            if let Some(change) = txn.journal.retract(
                &mut state,
                write.view,
                write.name,
                write.key.as_ref(),
                id,
                name,
            )? {
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
    invocation.writes = ComponentWrites::default();
    invocation.children.clear();
    invocation.seen_children.clear();
    invocation.dirty = false;
    // State slots are invocation-private: retirement drops them (plan §5.6).
    graph_guard.state_slots.remove(&id);
    Ok(changes)
}
/// Retires stable keyed children from one enumeration group before the
/// enumerator evaluates any replacement child.  Cleanup after the parent
/// body is too late: a replacement child can publish the same fact while a
/// removed sibling still owns it.
pub(crate) fn reconcile_keyed_children(
    group: &'static std::panic::Location<'static>,
    keep: &[Arc<dyn KeyValue>],
) -> Result<()> {
    let (graph, parent) = ACTIVE_EVALS.with(|active| {
        active
            .borrow()
            .last()
            .map(|frame| (Arc::clone(&frame.graph), frame.id))
            .ok_or_else(|| Error::EffectOutsideRun {
                effect: "run_each_child".to_string(),
                view: "<computation>".to_string(),
            })
    })?;
    let stale = {
        let graph_guard = graph.lock();
        let parent_invocation = graph_guard.invocation(parent)?;
        parent_invocation
            .children
            .iter()
            .filter_map(|child| {
                graph_guard
                    .invocations
                    .iter()
                    .find(|invocation| invocation.id == *child && !invocation.retired)
            })
            .filter(|invocation| {
                invocation.identity.as_ref().is_some_and(|identity| {
                    identity.stable_input
                        && identity.file == group.file()
                        && identity.line == group.line()
                        && identity.column == group.column()
                        && !keep.iter().any(|key| identity.input.eq_value(key.as_ref()))
                })
            })
            .map(|invocation| invocation.id)
            .collect::<Vec<_>>()
    };
    for stale_child in stale {
        let changes = retract_invocation(&graph, stale_child, "<keyed-enumeration>")?;
        graph.lock().change_log.extend(changes);
        let mut graph_guard = graph.lock();
        with_txn(|txn| txn.touch_invocation(graph_guard.root, &graph_guard, parent));
        let parent_invocation = graph_guard.invocation_mut(parent)?;
        parent_invocation
            .children
            .retain(|child| *child != stale_child);
        parent_invocation
            .seen_children
            .retain(|child| *child != stale_child);
    }
    Ok(())
}

#[track_caller]
pub(crate) fn run_effect<F, A, B>(function: F, input: A) -> Result<B>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    run_effect_at_with_definition(
        function,
        input,
        false,
        std::panic::Location::caller(),
        None,
        None,
    )
}

#[track_caller]
pub(crate) fn run_keyed_effect<F, A, B>(function: F, input: A) -> Result<B>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    run_effect_at_with_definition(
        function,
        input,
        true,
        std::panic::Location::caller(),
        None,
        None,
    )
}

#[track_caller]
pub(crate) fn run_keyed_effect_at<F, A, B>(
    function: F,
    input: A,
    location: &'static std::panic::Location<'static>,
) -> Result<B>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    run_effect_at_with_definition(function, input, true, location, None, None)
}
fn run_effect_at_with_definition<F, A, B>(
    function: F,
    input: A,
    stable_input: bool,
    location: &'static std::panic::Location<'static>,
    definition: Option<TypeId>,
    apply_output: Option<Arc<dyn Fn(&B) -> Result<()> + Send + Sync>>,
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
        let _key: Arc<dyn KeyValue> = Arc::new(input.clone());
        let base = CallBase {
            file: location.file(),
            line: location.line(),
            column: location.column(),
            function: definition.unwrap_or(TypeId::of::<F>()),
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
        function: definition.unwrap_or(TypeId::of::<F>()),
        input: Arc::clone(&key),
        occurrence,
        stable_input,
    };
    let call = match definition {
        Some(definition) => {
            erased_call_with_definition_and_apply(definition, function, input, apply_output)
        }
        None => erased_call(function, input),
    };
    if std::env::var_os("PLINGO_TRACE_EFFECTS").is_some() {
        eprintln!(
            "effect function={} input={:?} hash={} parent={} occurrence={} stable={}",
            call.function_name(),
            key,
            key.hash_value(),
            parent,
            occurrence,
            stable_input
        );
    }

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
                                && have.occurrence >= identity.occurrence
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
                writes: ComponentWrites::default(),
                pending_writes: Vec::new(),
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

/// Runs one generated component instance as a stable keyed child.
#[track_caller]
pub(crate) fn run_component_effect<D, F, A, B>(function: F, input: A) -> Result<B>
where
    D: crate::reactive::component::ComponentDefinition + 'static,
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    B: crate::reactive::component::Effects + Clone + PartialEq + Debug + Send + Sync + 'static,
{
    let apply_output: Arc<dyn Fn(&B) -> Result<()> + Send + Sync> =
        Arc::new(|output| output.__apply());
    run_effect_at_with_definition(
        function,
        input,
        true,
        std::panic::Location::caller(),
        Some(TypeId::of::<D>()),
        Some(apply_output),
    )
}

/// Runs a v2 component call. Matching is function-definition plus stable key;
/// props are compared separately and update the same invocation in place.
#[track_caller]
pub(crate) fn run_component_value<D, F, K, P, B>(function: F, key: K, props: P) -> Result<B>
where
    D: crate::reactive::component::ComponentDefinition + 'static,
    F: Fn(K, P) -> Result<B> + Clone + Send + Sync + 'static,
    K: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    P: Clone + PartialEq + Debug + Send + Sync + 'static,
    B: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    let (graph, parent) = ACTIVE_EVALS.with(|active| {
        let active = active.borrow();
        let frame = active.last().ok_or_else(|| Error::EffectOutsideRun {
            effect: "component_call".to_string(),
            view: "<component>".to_string(),
        })?;
        Ok((Arc::clone(&frame.graph), frame.id))
    })?;
    let location = std::panic::Location::caller();
    let key_for_identity: Arc<dyn KeyValue> = Arc::new(key.clone());
    let identity = CallIdentity {
        file: location.file(),
        line: location.line(),
        column: location.column(),
        function: TypeId::of::<D>(),
        input: Arc::clone(&key_for_identity),
        occurrence: 0,
        stable_input: true,
    };
    let call = erased_component_call_with_definition(
        TypeId::of::<D>(),
        D::__descriptor(),
        None,
        function,
        key,
        props,
    );

    {
        let graph_guard = graph.lock();
        if let Some(cycle_start) = active_ids().iter().position(|ancestor| {
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
            let mut functions = active_ids()[cycle_start..]
                .iter()
                .filter_map(|ancestor| graph_guard.invocation(*ancestor).ok())
                .map(|invocation| invocation.function_name.to_string())
                .collect::<Vec<_>>();
            functions.push(call.function_name().to_string());
            return Err(Error::ComputationCycle { functions });
        }
    }

    let (child, props_changed) = {
        let mut graph_guard = graph.lock();
        let children = graph_guard.invocation(parent)?.children.clone();
        let mut seen_match = None;
        let mut unseen_match = None;
        for child in children {
            let Some(invocation) = graph_guard
                .invocations
                .iter()
                .find(|invocation| invocation.id == child && !invocation.retired)
            else {
                continue;
            };
            let matches_key = invocation
                .identity
                .as_ref()
                .is_some_and(|have| have.function == identity.function && have.input.eq_value(identity.input.as_ref()));
            if !matches_key {
                continue;
            }
            if graph_guard
                .invocation(parent)?
                .seen_children
                .contains(&invocation.id)
            {
                seen_match = Some(invocation.id);
            } else {
                unseen_match = Some(invocation.id);
            }
        }
        let selected = seen_match.or(unseen_match);
        if let Some(child) = selected {
            let old_props_differ = {
                let existing = &graph_guard.invocation(child)?.call;
                !existing.props_equal(call.as_ref())
            };
            if seen_match == Some(child) && old_props_differ {
                let function = graph_guard.invocation(child)?.function_name.to_string();
                return Err(Error::ConflictingComponentInputs {
                    function,
                    key: format!("{key_for_identity:?}"),
                    previous: "<previous props>".to_string(),
                    current: "<current props>".to_string(),
                });
            }
            with_txn(|txn| txn.touch_invocation(graph_guard.root, &graph_guard, child));
            graph_guard.invocation_mut(child)?.call = Arc::clone(&call);
            graph_guard.invocation_mut(child)?.dirty |= old_props_differ;
            (child, old_props_differ)
        } else {
            let child = fresh_token();
            with_txn(|txn| txn.touch_invocation(graph_guard.root, &graph_guard, child));
            graph_guard.invocations.push(Invocation {
                id: child,
                parent: Some(parent),
                identity: Some(identity),
                call: Arc::clone(&call),
                function_name: call.function_name(),
                reads: Vec::new(),
                writes: ComponentWrites::default(),
                pending_writes: Vec::new(),
                result: None,
                fresh_sites: HashMap::new(),
                children: Vec::new(),
                seen_children: Vec::new(),
                emission_modes: HashMap::new(),
                dirty: false,
                retired: false,
            });
            graph_guard.invocation_mut(parent)?.children.push(child);
            (child, true)
        }
    };
    {
        let mut graph_guard = graph.lock();
        with_txn(|txn| txn.touch_invocation(graph_guard.root, &graph_guard, parent));
        let parent_invocation = graph_guard.invocation_mut(parent)?;
        if !parent_invocation.seen_children.contains(&child) {
            parent_invocation.seen_children.push(child);
        }
        if props_changed {
            graph_guard.dirty.insert(child);
        }
    }

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

/// Declares a tree child by identity and queues its body for later
/// evaluation.  The parent receives the stable output box immediately, so
/// recursive authored calls do not recurse through Rust evaluation frames.
#[track_caller]
pub(crate) fn run_tree_component_effect<D, F, A, T>(
    function: F,
    input: A,
) -> Result<crate::reactive::abstract_tree::AstBox<T>>
where
    D: crate::reactive::component::ComponentDefinition + 'static,
    F: Fn(A) -> Result<crate::reactive::abstract_tree::AstBox<T>> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    T: crate::reactive::abstract_tree::AbstractTreeNode,
{
    let (graph, parent, occurrence) = ACTIVE_EVALS.with(|active| {
        let active = active.borrow();
        let frame = active.last().ok_or_else(|| Error::EffectOutsideRun {
            effect: "component_call".to_string(),
            view: "<component>".to_string(),
        })?;
        let base = CallBase {
            file: std::panic::Location::caller().file(),
            line: std::panic::Location::caller().line(),
            column: std::panic::Location::caller().column(),
            function: TypeId::of::<D>(),
        };
        let mut occurrences = frame.occurrences.borrow_mut();
        let occurrence =
            if let Some((_, count)) = occurrences.iter_mut().find(|(have, _)| have.same(&base)) {
                let value = *count;
                *count += 1;
                value
            } else {
                occurrences.push((base, 1));
                0
            };
        Ok((Arc::clone(&frame.graph), frame.id, occurrence))
    })?;
    let key: Arc<dyn KeyValue> = Arc::new(input.clone());
    let output_input = input.clone();
    let call = erased_call_with_definition(TypeId::of::<D>(), function, input);
    let identity = CallIdentity {
        file: std::panic::Location::caller().file(),
        line: std::panic::Location::caller().line(),
        column: std::panic::Location::caller().column(),
        function: TypeId::of::<D>(),
        input: Arc::clone(&key),
        occurrence,
        stable_input: true,
    };
    let cycle = {
        let graph_guard = graph.lock();
        let mut cursor = Some(parent);
        let mut path = Vec::new();
        let mut found = false;
        while let Some(id) = cursor {
            let invocation = graph_guard.invocation(id)?;
            path.push(invocation.function_name.to_string());
            if invocation.identity.as_ref().is_some_and(|have| {
                have.function == identity.function && have.input.eq_value(identity.input.as_ref())
            }) {
                found = true;
                break;
            }
            cursor = invocation.parent;
        }
        found.then(|| {
            path.reverse();
            path.push(call.function_name().to_string());
            path
        })
    };
    if let Some(functions) = cycle {
        return Err(Error::ComputationCycle { functions });
    }
    let child = {
        let mut graph_guard = graph.lock();
        let existing = graph_guard
            .invocations
            .iter()
            .find(|invocation| {
                !invocation.retired
                    && invocation.parent == Some(parent)
                    && invocation.identity.as_ref().is_some_and(|have| {
                        have.function == identity.function
                            && have.input.eq_value(identity.input.as_ref())
                    })
            })
            .map(|invocation| invocation.id);
        if let Some(child) = existing {
            with_txn(|txn| txn.touch_invocation(graph_guard.root, &graph_guard, child));
            graph_guard.invocation_mut(child)?.call = Arc::clone(&call);
            child
        } else {
            let child = fresh_token();
            with_txn(|txn| txn.touch_invocation(graph_guard.root, &graph_guard, child));
            graph_guard.invocations.push(Invocation {
                id: child,
                parent: Some(parent),
                identity: Some(identity),
                call: Arc::clone(&call),
                function_name: call.function_name(),
                result: None,
                fresh_sites: HashMap::new(),
                reads: Vec::new(),
                writes: ComponentWrites::default(),
                pending_writes: Vec::new(),
                children: Vec::new(),
                seen_children: Vec::new(),
                emission_modes: HashMap::new(),
                dirty: true,
                retired: false,
            });
            graph_guard.invocation_mut(parent)?.children.push(child);
            child
        }
    };
    {
        let mut graph_guard = graph.lock();
        graph_guard
            .invocation_mut(parent)?
            .seen_children
            .push(child);
        graph_guard.dirty.insert(child);
    }
    automatic_component_ast_box::<
        D,
        <T as crate::reactive::abstract_tree::AbstractTreeNode>::Family,
        A,
        T,
    >(&output_input)
}

#[derive(Clone, Debug)]
struct AutomaticTreeKey {
    family: TypeId,
    component: TypeId,
    input: Arc<dyn KeyValue>,
}

impl PartialEq for AutomaticTreeKey {
    fn eq(&self, other: &Self) -> bool {
        self.family == other.family
            && self.component == other.component
            && self.input.eq_value(other.input.as_ref())
    }
}

impl Eq for AutomaticTreeKey {}

impl Hash for AutomaticTreeKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.family.hash(state);
        self.component.hash(state);
        self.input.hash_value().hash(state);
    }
}

/// Mints the deterministic identity for one generated component output port.
///
/// This hidden helper is used by generated/advanced effect constructors. The
/// complete component marker and driving key remain inside the opaque node.
#[doc(hidden)]
pub fn automatic_node_id<V, M, K>(key: K, port: u16) -> Result<crate::reactive::view::Node<V>>
where
    V: View,
    M: crate::reactive::component::ComponentDefinition + 'static,
    K: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
{
    let active = ACTIVE_EVALS.with(|stack| !stack.borrow().is_empty());
    if !active {
        return Err(Error::EffectOutsideRun {
            effect: "component_output".to_string(),
            view: V::name().to_string(),
        });
    }
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct AutomaticKey<K> {
        view: TypeId,
        component: TypeId,
        port: u16,
        driving_key: K,
    }
    let identity = AutomaticKey {
        view: TypeId::of::<V>(),
        component: TypeId::of::<M>(),
        port,
        driving_key: key,
    };
    let mut hasher = DefaultHasher::new();
    identity.hash(&mut hasher);
    let raw = hasher.finish();
    Ok(crate::reactive::view::Node::from_automatic(
        raw,
        Arc::new(identity),
    ))
}

/// Allocates the one stable abstract-tree output identity for the active
/// component invocation.
pub(crate) fn automatic_ast_box<F: View>() -> Result<crate::reactive::abstract_tree::AstBox<()>> {
    let (identity, raw) = ACTIVE_EVALS.with(|active| {
        let active = active.borrow();
        let frame = active.last().ok_or_else(|| Error::EffectOutsideRun {
            effect: "tree_render".to_string(),
            view: F::name().to_string(),
        })?;
        if !frame.rendered_slots.borrow_mut().insert(TypeId::of::<F>()) {
            return Err(Error::Internal(
                "abstract-tree output slot rendered more than once".into(),
            ));
        }
        let graph = frame.graph.lock();
        let invocation = graph.invocation(frame.id)?;
        let component = invocation
            .call
            .definition_type()
            .unwrap_or_else(|| invocation.call.function_type());
        let identity = AutomaticTreeKey {
            family: TypeId::of::<F>(),
            component,
            input: invocation.call.input_key(),
        };
        let mut hasher = DefaultHasher::new();
        identity.hash(&mut hasher);
        Ok((Arc::new(identity) as Arc<dyn KeyValue>, hasher.finish()))
    })?;
    Ok(crate::reactive::abstract_tree::AstBox::from_parts(
        raw, identity,
    ))
}
/// Allocates the one automatic graph-node identity for the active component
/// invocation.  Graph outputs use the same complete identity model as tree
/// outputs, but keep their publication as a returned `GraphRender` effect.
pub(crate) fn automatic_graph_node_id<V: View>() -> Result<crate::reactive::view::Node<V>> {
    let (identity, raw) = ACTIVE_EVALS.with(|active| {
        let active = active.borrow();
        let frame = active.last().ok_or_else(|| Error::EffectOutsideRun {
            effect: "graph_render".to_string(),
            view: V::name().to_string(),
        })?;
        if !frame.rendered_slots.borrow_mut().insert(TypeId::of::<V>()) {
            return Err(Error::Internal(
                "graph output slot rendered more than once".into(),
            ));
        }
        let graph = frame.graph.lock();
        let invocation = graph.invocation(frame.id)?;
        let component = invocation
            .call
            .definition_type()
            .unwrap_or_else(|| invocation.call.function_type());
        let identity = AutomaticTreeKey {
            family: TypeId::of::<V>(),
            component,
            input: invocation.call.input_key(),
        };
        let mut hasher = DefaultHasher::new();
        identity.hash(&mut hasher);
        Ok((Arc::new(identity) as Arc<dyn KeyValue>, hasher.finish()))
    })?;
    Ok(crate::reactive::view::Node::from_automatic(raw, identity))
}

/// Derives a tree output identity for a child before its body is evaluated.
pub(crate) fn automatic_component_ast_box<D, F, A, T>(
    input: &A,
) -> Result<crate::reactive::abstract_tree::AstBox<T>>
where
    D: crate::reactive::component::ComponentDefinition + 'static,
    F: View,
    A: Clone + Eq + std::hash::Hash + Debug + Send + Sync + 'static,
    T: 'static,
{
    let active = ACTIVE_EVALS.with(|stack| !stack.borrow().is_empty());
    if !active {
        return Err(Error::EffectOutsideRun {
            effect: "tree_render".to_string(),
            view: F::name().to_string(),
        });
    }
    let identity = AutomaticTreeKey {
        family: TypeId::of::<F>(),
        component: TypeId::of::<D>(),
        input: Arc::new(input.clone()),
    };
    let mut hasher = DefaultHasher::new();
    identity.hash(&mut hasher);
    Ok(crate::reactive::abstract_tree::AstBox::from_parts(
        hasher.finish(),
        Arc::new(identity),
    ))
}

#[derive(Clone, Debug)]
struct AutomaticEffectKey {
    view: TypeId,
    function: TypeId,
    file: &'static str,
    line: u32,
    column: u32,
    occurrence: u64,
    input: Arc<dyn KeyValue>,
}
impl PartialEq for AutomaticEffectKey {
    fn eq(&self, other: &Self) -> bool {
        self.view == other.view
            && self.function == other.function
            && self.file == other.file
            && self.line == other.line
            && self.column == other.column
            && self.occurrence == other.occurrence
            && self.input.eq_value(other.input.as_ref())
    }
}

impl Eq for AutomaticEffectKey {}

impl Hash for AutomaticEffectKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.view.hash(state);
        self.function.hash(state);
        self.file.hash(state);
        self.line.hash(state);
        self.column.hash(state);
        self.occurrence.hash(state);
        self.input.hash_value().hash(state);
    }
}

#[track_caller]
pub(crate) fn automatic_effect_node_id<V: View>() -> Result<crate::reactive::view::Node<V>> {
    let location = std::panic::Location::caller();
    let (graph, id) = ACTIVE_EVALS.with(|active| {
        active
            .borrow()
            .last()
            .map(|frame| (Arc::clone(&frame.graph), frame.id))
            .ok_or_else(|| Error::EffectOutsideRun {
                effect: "automatic_effect_node_id".to_string(),
                view: V::name().to_string(),
            })
    })?;
    let (function, input) = {
        let graph_guard = graph.lock();
        let invocation = graph_guard.invocation(id)?;
        (invocation.call.function_type(), invocation.call.input_key())
    };
    let input_hash = input.hash_value();
    let occurrence = {
        let mut graph_guard = graph.lock();
        let invocation = graph_guard.invocation_mut(id)?;
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
    let identity = AutomaticEffectKey {
        view: TypeId::of::<V>(),
        function,
        file: location.file(),
        line: location.line(),
        column: location.column(),
        occurrence,
        input,
    };
    let mut hasher = DefaultHasher::new();
    identity.hash(&mut hasher);
    Ok(crate::reactive::view::Node::from_automatic(
        hasher.finish(),
        Arc::new(identity),
    ))
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
    /// Installed first-class component definitions (Cut C).
    pub components: crate::reactive::component::DefinitionRegistry,
    pub(crate) family_by_root: HashMap<u64, u64>,
    pub(crate) next_install_ordinal: u64,
}

/// The external selector that owns a keyed family. Tree families must filter
/// the shared fact view to the exact semantic dimension they mount; treating
/// every tree fact as a child key would feed `Kind`, `Leaf`, and link facts to
/// typed component bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FamilySelector {
    MapEntry,
    TreeRoot,
    TreeNode(&'static str),
}

impl FamilySelector {
    fn accepts<D: crate::reactive::kind::KeyBounds>(self, key: &dyn KeyValue) -> bool {
        match self {
            Self::MapEntry => true,
            Self::TreeRoot => key
                .as_any()
                .downcast_ref::<crate::reactive::abstract_tree::TreeKey<D>>()
                .is_some_and(|key| matches!(key, crate::reactive::abstract_tree::TreeKey::RootLink(_, _))),
            Self::TreeNode(member) => key
                .as_any()
                .downcast_ref::<crate::reactive::abstract_tree::TreeKey<D>>()
                .is_some_and(|key| matches!(key, crate::reactive::abstract_tree::TreeKey::Member(_, actual) if *actual == member)),
        }
    }
}

/// One installed keyed family: its dedicated graph plus watch metadata and
/// the erased constructor that stamps typed calls onto children.
pub(crate) struct FamilyRuntime {
    pub graph: Arc<Mutex<PlainGraph>>,
    pub view: TypeId,
    pub view_name: &'static str,
    pub install_ordinal: u64,
    pub selector: FamilySelector,
    pub accept_key: Arc<dyn Fn(&dyn KeyValue) -> bool + Send + Sync>,
    pub build_call: Arc<dyn Fn(Arc<dyn KeyValue>) -> Arc<dyn ErasedCall> + Send + Sync>,
    /// Component definition when this family backs a `#[component]`
    /// EachKey driver (Cut C): children take the definition marker as their
    /// identity type and its descriptor as their name.
    pub definition: Option<(&'static str, TypeId)>,
}

impl Default for PlainRuntime {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(PlainState::default())),
            components: crate::reactive::component::DefinitionRegistry::default(),
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

    pub(crate) fn view_counts(&self) -> Vec<(String, u64)> {
        let mut counts = self
            .root
            .views()
            .iter()
            .map(|(view_id, view)| (format!("{view_id:?}:{}", view.name()), view.len() as u64))
            .collect::<Vec<_>>();
        counts.sort_by(|left, right| left.0.cmp(&right.0));
        counts
    }

    pub(crate) fn observe<V: View>(&self, input: V::Input) -> Option<Arc<V::Output>> {
        record_command_metric::<EngineWork>(|work| {
            work.fact_reads += 1;
            work.index_probes += 1;
        });
        let view = self.root.view(TypeId::of::<V>())?;
        let key: Arc<dyn KeyValue> = Arc::new(input);
        let entry = view.lookup(key.as_ref())?;
        entry
            .value
            .as_any()
            .downcast_ref::<V::Output>()
            .map(|_| {
                let value: Arc<dyn Value> = Arc::clone(&entry.value);
                value
            })
            .and_then(downcast_arc::<V::Output>)
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

/// Debug/test liveness audit over the production view and invocation
/// indexes (follow-up plan §4 item 12). Read-only: it adds no facts, no
/// dependencies, and no repair paths. Returns one human-readable row per
/// violated invariant; an empty vector means the indexes are consistent.
pub(crate) fn liveness_audit(runtime: &PlainRuntime) -> Vec<String> {
    use std::collections::HashSet;
    let mut violations = Vec::new();
    let state = runtime.state.lock();

    // Every live graph: installed roots plus keyed-family graphs.
    let graphs: Vec<(String, Arc<Mutex<PlainGraph>>)> = runtime
        .roots
        .iter()
        .map(|(token, root)| (format!("root:{token}"), Arc::clone(&root.graph)))
        .chain(runtime.families.iter().map(|(id, family)| {
            (
                format!("family:{id}:{}", family.view_name),
                Arc::clone(&family.graph),
            )
        }))
        .collect();

    for (label, graph) in &graphs {
        let graph_guard = graph.lock();
        for invocation in &graph_guard.invocations {
            let where_ = format!("{label}/{}#{}", invocation.function_name, invocation.id);
            if invocation.retired {
                if !invocation.writes.is_empty() {
                    violations.push(format!(
                        "{where_}: retired instance retains {} output facts",
                        invocation.writes.len()
                    ));
                }
                if !invocation.reads.is_empty() || !invocation.children.is_empty() {
                    violations.push(format!("{where_}: retired instance retains reads/children"));
                }
                continue;
            }
            // Forward ownership: every owned output exists in the store
            // with this instance among its writers.
            for write in invocation.writes.all_entries() {
                match state.slot_index(write.view, write.key.as_ref()) {
                    Some(index) => {
                        let slot = state.slots[index].as_ref().expect("occupied slot");
                        if !slot.fact.writers.contains(invocation.id) {
                            violations.push(format!(
                                "{where_}: owns {:?} on {} without writer membership",
                                write.key, write.view_name
                            ));
                        }
                    }
                    None => violations.push(format!(
                        "{where_}: owns absent fact {:?} on {}",
                        write.key, write.view_name
                    )),
                }
            }
            // Bijection: forward read rows ↔ reverse dependency index.
            for read in &invocation.reads {
                if !graph_guard.deps.contains_row(
                    read.view,
                    read.key.as_ref(),
                    read.temporal,
                    read.keyset,
                    invocation.id,
                ) {
                    violations.push(format!(
                        "{where_}: read row missing from reverse index ({})",
                        read.name
                    ));
                }
            }
        }
        // Converse bijection: every reverse row names a live invocation
        // whose forward rows justify it (no dangling endpoints).
        let mut seen_rows: HashSet<(bool, u8, TypeId, u64)> = HashSet::new();
        for (temporal, kind, view, invocation_id) in graph_guard.deps.iter_rows() {
            if !seen_rows.insert((temporal, kind, view, invocation_id)) {
                continue;
            }
            let Some(target) = graph_guard
                .invocations
                .iter()
                .find(|candidate| candidate.id == invocation_id)
            else {
                violations.push(format!(
                    "{label}: dangling dependency endpoint {invocation_id}"
                ));
                continue;
            };
            if target.retired {
                violations.push(format!(
                    "{label}: dependency row targets retired {invocation_id}"
                ));
                continue;
            }
            let justified = target.reads.iter().any(|read| {
                graph_guard.deps.contains_row(
                    view,
                    read.key.as_ref(),
                    temporal,
                    matches!(kind, 1),
                    invocation_id,
                ) && (kind != 0 || read.key.is_some())
            });
            if !justified {
                violations.push(format!(
                    "{label}/{target_function}#{invocation_id}: unjustified reverse row",
                    target_function = target.function_name
                ));
            }
        }
    }

    // Owner multiplicity: a non-shareable fact never carries two writers;
    // shareable facts expose their full owner set (nothing to reject).
    for slots in state.by_view.values() {
        for (_, index) in slots {
            let Some(slot) = state.slots[*index].as_ref() else {
                continue;
            };
            if !slot.fact.shared && slot.fact.writers.len() > 1 {
                violations.push(format!(
                    "fact {:?} on {} has {} writers but is non-shareable",
                    slot.fact.key,
                    slot.fact.name,
                    slot.fact.writers.len()
                ));
            }
        }
    }

    // Family DIRECT children only: driving elements exist while the child
    // lives. Nested descendants belong to their parent chain, not to the
    // family driver (Cut C lifecycle model).
    for family in runtime.families.values() {
        let graph_guard = family.graph.lock();
        let family_root = graph_guard.root;
        for invocation in &graph_guard.invocations {
            if invocation.retired || invocation.parent != Some(family_root) {
                continue;
            }
            let Some(identity) = invocation.identity.as_ref() else {
                continue;
            };
            let present = runtime
                .committed
                .view(family.view)
                .map(|view| {
                    view.entries()
                        .any(|entry| entry.key.eq_value(identity.input.as_ref()))
                })
                .unwrap_or(false);
            if !present {
                violations.push(format!(
                    "family {}:{}#{} drives on a missing key of {}",
                    family.view_name, invocation.function_name, invocation.id, family.view_name
                ));
            }
        }
    }

    violations.sort();
    violations
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
    runtime.roots.insert(
        plan.root,
        RootRuntime {
            graph,
            sink,
            install_ordinal,
        },
    );
    Ok(plan.output)
}

fn change_matches(read: &ReadDep, changes: &[FactChange], temporal: bool) -> bool {
    changes.iter().any(|change| read.matches(change, temporal))
}

/// Temporal::Previous fact read: the journal's first-touch value when the
/// command touched the key, else the committed value (identical for
/// untouched keys).
fn previous_read(
    state: &Mutex<PlainState>,
    view: TypeId,
    key: &dyn KeyValue,
) -> Option<Arc<dyn Value>> {
    ACTIVE_TXN
        .with(|txn| {
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
    if std::env::var_os("PLINGO_TRACE_QUEUE").is_some() {
        for change in changes {
            eprintln!(
                "mark change view={:?} key_hash={} presence={}",
                change.view,
                change.key.hash_value(),
                change.presence_changed
            );
        }
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
    // Roots and keyed families share one wake contract: every invocation
    // wakes through its OWN recorded dependency rows (follow-up plan §6.1
    // membership-only drivers rely on this for family members).
    let graphs = runtime
        .roots
        .values()
        .map(|root| (root.install_ordinal, &root.graph))
        .chain(
            runtime
                .families
                .values()
                .map(|family| (family.install_ordinal, &family.graph)),
        );
    for (install_ordinal, graph) in graphs {
        let graph = graph.lock();
        let mut ids: Vec<u64> = Vec::new();
        graph.deps.mark_current(&changes, |id| ids.push(id));
        for id in ids {
            if std::env::var_os("PLINGO_TRACE_QUEUE").is_some() {
                eprintln!(
                    "queue install={} root={} invocation={} function={}",
                    install_ordinal,
                    graph.root,
                    id,
                    graph.function_name_of(id).unwrap_or("<missing>")
                );
            }
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
                .all_entries()
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
                Error::Internal(
                    format!("owned invocation {id} absent in root {}", graph_guard.root).into(),
                )
            })?;
        (invocation.writes.all_entries(), invocation.function_name)
    };
    with_txn(|txn| -> Result<()> {
        let mut state = state.lock();
        for write in writes {
            txn.journal.retract(
                &mut state,
                write.view,
                write.name,
                write.key.as_ref(),
                id,
                name,
            )?;
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
    // Roots and keyed-family/component graphs share one epoch wake pass
    // (follow-up plan §6.1: component members wake through their own
    // recorded dependencies).
    let graphs = runtime
        .roots
        .values()
        .map(|root| (root.install_ordinal, &root.graph))
        .chain(
            runtime
                .families
                .values()
                .map(|family| (family.install_ordinal, &family.graph)),
        );
    for (install_ordinal, graph_handle) in graphs {
        let mut graph = graph_handle.lock();
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
    if !(family.accept_key)(key.as_ref()) {
        return Ok(None);
    }
    let graph = Arc::clone(&family.graph);
    let install_ordinal = family.install_ordinal;
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
                writes: ComponentWrites::default(),
                pending_writes: Vec::new(),
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
    Ok(Some(child))
}
fn drain_deferred_children(runtime: &mut PlainRuntime) {
    let graphs: Vec<(u64, u64, Arc<Mutex<PlainGraph>>)> = runtime
        .roots
        .values()
        .map(|root| {
            (
                root.install_ordinal,
                root.graph.lock().root,
                Arc::clone(&root.graph),
            )
        })
        .chain(runtime.families.values().map(|family| {
            (
                family.install_ordinal,
                family.graph.lock().root,
                Arc::clone(&family.graph),
            )
        }))
        .collect();
    for (install_ordinal, root, graph) in graphs {
        let pending = graph.lock().dirty.drain().collect::<Vec<_>>();
        for invocation in pending {
            runtime.dirty.insert(crate::reactive::store::DirtyKey {
                root_install_ordinal: install_ordinal,
                invocation_ordinal: invocation,
                root,
                invocation,
            });
        }
    }
}

/// Schedules every family watching a changed view.
pub(crate) fn schedule_families(runtime: &mut PlainRuntime, changes: &[FactChange]) {
    // Removals retire BEFORE additions evaluate: a child whose input
    // vanished must retract its owned keys before a concurrently-queued
    // new instance writes overlapping keys in the same command (plan
    // §23.5 "edge removal/move retires the old expectation before the new
    // edge settles"). Otherwise the store sees the retired writer's staged
    // value and raises a spurious writer conflict.
    for change in changes {
        if !change.presence_changed {
            continue;
        }
        let still_present = runtime
            .state
            .lock()
            .slot_index(change.view, change.key.as_ref())
            .is_some();
        if std::env::var_os("PLINGO_TRACE_FAMILY").is_some() {
            eprintln!(
                "family-removal? view={:?} hash={} present={}",
                change.view,
                change.key.hash_value(),
                still_present
            );
        }
        if still_present {
            continue;
        }
        let watchers: Vec<u64> = runtime
            .families
            .iter()
            .filter(|(_, family)| {
                family.view == change.view && (family.accept_key)(change.key.as_ref())
            })
            .map(|(family_id, _)| *family_id)
            .collect();
        for family_id in watchers {
            let family = runtime.families.get(&family_id);
            let Some(family) = family else { continue };
            let graph = Arc::clone(&family.graph);
            let root = graph.lock().root;
            drop(family);
            let function_name = graph.lock().function_name_of(root);
            let Ok(function_name) = function_name else {
                continue;
            };
            let removed: Vec<u64> = {
                let graph_guard = graph.lock();
                graph_guard
                    .invocations
                    .iter()
                    .filter(|invocation| {
                        !invocation.retired
                            && invocation.parent == Some(graph_guard.root)
                            && invocation.identity.as_ref().is_some_and(|identity| {
                                identity.input.eq_value(change.key.as_ref())
                            })
                    })
                    .map(|invocation| invocation.id)
                    .collect()
            };
            for id in removed {
                if std::env::var_os("PLINGO_TRACE_FAMILY").is_some() {
                    eprintln!("family-retire id={id}");
                }
                let changes = match retract_invocation(&graph, id, function_name) {
                    Ok(changes) => changes,
                    Err(_) => continue,
                };
                {
                    let mut graph_guard = graph.lock();
                    if let Some(position) = graph_guard
                        .invocations
                        .iter()
                        .position(|invocation| invocation.id == id)
                    {
                        graph_guard.invocations.remove(position);
                    }
                }
                mark_changes(runtime, &changes);
            }
        }
    }
    for change in changes {
        // Families own MEMBERSHIP lifecycle only (follow-up plan §6.1): an
        // instance exists iff its key exists. A payload change reruns a
        // member only through the member's OWN recorded read dependency
        // (mark_current above); membership drivers stay cold otherwise.
        if !change.presence_changed {
            continue;
        }
        let still_present = runtime
            .state
            .lock()
            .slot_index(change.view, change.key.as_ref())
            .is_some();
        if !still_present {
            continue;
        }
        let watchers: Vec<u64> = runtime
            .families
            .iter()
            .filter(|(_, family)| {
                family.view == change.view && (family.accept_key)(change.key.as_ref())
            })
            .map(|(family_id, _)| *family_id)
            .collect();
        for family_id in watchers {
            let _ = queue_family_child(runtime, family_id, Arc::clone(&change.key));
        }
    }
}

pub(crate) fn quiesce(runtime: &mut PlainRuntime) -> Result<u32> {
    let mut rounds = 0u32;
    loop {
        drain_deferred_children(runtime);
        let Some(key) = runtime.dirty.pop() else {
            break;
        };
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
        // DIRECT family children only: nested descendants inherit lifecycle
        // from their parent chain (Cut C), so a grandchild requeued by its
        // own dependency rows must never be driver-checked.
        let popped_is_family_child = runtime.family_by_root.contains_key(&root)
            && runtime
                .family_by_root
                .get(&root)
                .and_then(|family_id| runtime.families.get(family_id))
                .map(|family| {
                    let graph_guard = family.graph.lock();
                    graph_guard
                        .invocation(id)
                        .is_ok_and(|inv| inv.parent == Some(graph_guard.root))
                })
                .unwrap_or(false);
        if popped_is_family_child
            && let Some(family_id) = runtime.family_by_root.get(&root).copied()
        {
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
            && let Some(output) = &invocation.result
        {
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
    crate::reactive::pathwork::reset();
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
            (buffer.writes.clone(), std::mem::take(&mut buffer.patch_ops))
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
            engine: {
                let mut engine = take_engine_work();
                engine.path_work = crate::reactive::pathwork::take();
                engine
            },
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
        engine: {
            let mut engine = take_engine_work();
            engine.path_work = crate::reactive::pathwork::take();
            engine
        },
        changes: final_changes,
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

/// Cut E ownership-diff seam: keys the innermost evaluating invocation
/// owned BEFORE its current body ran (its pre-evaluation write set),
/// filtered to one view.
pub(crate) fn pre_eval_owned_keys<V: View>() -> Vec<Arc<dyn KeyValue>> {
    let view = TypeId::of::<V>();
    ACTIVE_EVALS.with(|active| {
        let active = active.borrow();
        let Some(frame) = active.last() else {
            return Vec::new();
        };
        frame
            .pre_eval_writes
            .writes
            .iter()
            .filter(|write| write.view == view)
            .map(|write| Arc::clone(&write.key))
            .collect()
    })
}

/// Cut E ownership-diff seam: keys buffered for one view by the innermost
/// evaluation's authoring so far (the desired post-publication set).
pub(crate) fn pending_view_keys<V: View>() -> Vec<Arc<dyn KeyValue>> {
    let view = TypeId::of::<V>();
    let mut keys = Vec::new();
    ACTIVE.with(|active| {
        let active = active.borrow();
        if let Some(dispatcher) = active.last() {
            match dispatcher {
                ActiveDispatcher::Eval(_, _, pending) => {
                    for op in pending.patch_ops.borrow().iter() {
                        if op.view == view {
                            keys.push(Arc::clone(&op.key));
                        }
                    }
                }
                ActiveDispatcher::Command(buffer, _) => {
                    for op in buffer.lock().patch_ops.iter() {
                        if op.view == view {
                            keys.push(Arc::clone(&op.key));
                        }
                    }
                }
            }
        }
    });
    keys
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

impl_state_value!(
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    bool,
    char,
    String,
    ()
);
unsafe impl<T: StateValue> StateValue for Arc<T> {}
unsafe impl<T: StateValue> StateValue for Vec<T> {}
unsafe impl<T: StateValue> StateValue for Option<T> {}
unsafe impl<K: StateValue, V: StateValue> StateValue for std::collections::BTreeMap<K, V> {}
unsafe impl<K: StateValue, V: StateValue> StateValue for std::collections::HashMap<K, V> where
    K: std::hash::Hash + Eq
{
}
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

#[cfg(test)]
mod liveness_audit_tests {
    use super::*;

    /// Negative test: an owned write with no committed fact store slot is
    /// flagged as an absent-fact violation.
    #[test]
    fn audit_flags_owned_fact_missing_from_store() {
        let runtime = PlainRuntime::default();
        let call = erased_call(|_: u64| Ok(()), 7u64);
        let graph = Arc::new(Mutex::new(PlainGraph::new(
            (*runtime.state.lock()).clone(),
            call,
            fresh_token(),
        )));
        let root = graph.lock().root;
        graph.lock().invocation_mut(root).unwrap().writes.insert(
            PlainWrite {
                view: TypeId::of::<u64>(),
                name: "emit",
                view_name: "ghost",
                key: Arc::new(1u64),
                value: None,
                shareable: false,
            },
            EmissionMode::Replace,
        );
        let mut probe = PlainRuntime::default();
        probe.roots.insert(
            0,
            RootRuntime {
                graph,
                sink: Arc::new(OutputSink {
                    update: Box::new(|_| {}),
                }),
                install_ordinal: 0,
            },
        );
        let violations = liveness_audit(&probe);
        assert!(
            violations.iter().any(|row| row.contains("absent fact")),
            "violations: {violations:?}"
        );
    }

    /// Negative test: a retired invocation that still lists outputs is
    /// flagged ("no component output remains after its driver retires").
    #[test]
    fn audit_flags_retired_instance_retaining_outputs() {
        let runtime = PlainRuntime::default();
        let call = erased_call(|_: u64| Ok(()), 7u64);
        let key: Arc<dyn KeyValue> = Arc::new(1u64);
        let graph = Arc::new(Mutex::new(PlainGraph::new(
            (*runtime.state.lock()).clone(),
            call,
            fresh_token(),
        )));
        let root = graph.lock().root;
        {
            let mut graph_guard = graph.lock();
            let invocation = graph_guard
                .invocations
                .iter_mut()
                .find(|invocation| invocation.id == root)
                .expect("root invocation exists");
            invocation.retired = true;
            invocation.writes.insert(
                PlainWrite {
                    view: TypeId::of::<u64>(),
                    name: "emit",
                    view_name: "ghost",
                    key,
                    value: None,
                    shareable: false,
                },
                EmissionMode::Replace,
            );
        }
        let mut probe = PlainRuntime::default();
        probe.roots.insert(
            0,
            RootRuntime {
                graph,
                sink: Arc::new(OutputSink {
                    update: Box::new(|_| {}),
                }),
                install_ordinal: 0,
            },
        );
        let violations = liveness_audit(&probe);
        assert!(
            violations
                .iter()
                .any(|row| row.contains("retired instance retains")),
            "violations: {violations:?}"
        );
    }

    /// A freshly constructed runtime audits clean.
    #[test]
    fn audit_is_clean_on_a_fresh_runtime() {
        let runtime = PlainRuntime::default();
        assert!(liveness_audit(&runtime).is_empty());
    }

    /// Participant restore closures run in reverse registration order
    /// during rollback (Cut B section 20.1).
    #[test]
    fn private_participants_restore_in_reverse_order() {
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut txn = CommandTxn::default();
        for name in ["first", "second", "third"] {
            let order = std::sync::Arc::clone(&order);
            let name = name.to_owned();
            txn.private_undos.push(Box::new(move || {
                order.lock().unwrap().push(name);
            }));
        }
        // Simulate the rollback handoff.
        let state: Arc<Mutex<PlainState>> = Arc::new(Mutex::new(PlainState::default()));
        let mut roots: BTreeMap<u64, RootRuntime> = BTreeMap::new();
        rollback_txn(txn, &state, &mut roots);
        assert_eq!(
            *order.lock().unwrap(),
            vec!["third".to_owned(), "second".to_owned(), "first".to_owned()]
        );
    }
}
