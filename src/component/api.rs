//! Ergonomic incremental authoring API.
//!
//! A language feature is a [`Component`]: a hashable value containing exactly
//! the inputs that distinguish one computation. Its `run` method reads typed
//! views through [`Context`], calls other components, publishes its complete
//! next contribution, and returns a semantic value. Incrementality,
//! suspension, concurrency, replacement, and reclamation are runtime behavior;
//! authors never touch the node-graph kernel directly.

use std::{
    any::TypeId, cell::RefCell, collections::HashSet, fmt, hash::Hash, marker::PhantomData,
    sync::Arc,
};

use crate::{
    component::{lex::LexerRoot, scope::ScopeDomain},
    scheme::node::{
        DeriveCx, ErasedProvider, IndexedRelation, NodeError, NodeKey, NodeProvider, NodeSchema,
        NodeValue, PortDeclaration, ReadGraph, ReclaimCx, Relation, TaskId, View,
    },
};

/// The result returned by a component run: a semantic value, an ordinary
/// error, or the framework's opaque suspension state.
pub type Result<T> = std::result::Result<T, Error>;

/// A user-facing failure. Ordinary view and language failures convert into
/// this error; the suspension state is private and can only be produced by
/// framework operations.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
}

#[derive(Debug)]
enum ErrorKind {
    Node(NodeError),
    Suspended,
}

impl Error {
    pub(crate) fn suspended() -> Self {
        Self {
            kind: ErrorKind::Suspended,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ErrorKind::Node(error) => write!(formatter, "{error}"),
            ErrorKind::Suspended => formatter.write_str("awaiting a dependency"),
        }
    }
}

impl std::error::Error for Error {}

impl From<NodeError> for Error {
    fn from(error: NodeError) -> Self {
        Self {
            kind: ErrorKind::Node(error),
        }
    }
}

/// A stable executable identity. The component value is both the runtime key
/// and the input: there is no separate marker type or dispatch enum.
pub trait Component: Clone + Eq + Hash + Send + Sync + 'static {
    type Output: Clone + PartialEq + Send + Sync + 'static;
    type Writes: WriteSet;

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<Self::Output>;
}

/// The committed primary result of one component instance. It is published
/// only on successful completion; a suspended instance has no committed
/// output, and readers depend on the fact's appearance.
pub struct Output<C: Component>(PhantomData<fn() -> C>);

impl<C: Component> View for Output<C> {
    type Key = C;
    type Value = C::Output;
}

/// One component's complete immutable diagnostic set, keyed by its identity.
pub struct ComponentDiagnostics<C: Component, D: NodeValue>(PhantomData<fn() -> (C, D)>);

impl<C: Component, D: NodeValue> View for ComponentDiagnostics<C, D> {
    type Key = C;
    type Value = Arc<[D]>;
}

/// The declared side-write family of one component. `declarations` is generic
/// over the implementing component so identity-keyed ports (such as
/// diagnostics) can be declared per component kind.
pub trait WriteSet: Send + Sync + 'static {
    fn declarations<Owner: Component>() -> Vec<PortDeclaration>;
}

/// Expands to a tuple write set. Use in `type Writes = writes!(...)`.
#[macro_export]
macro_rules! writes {
    () => {
        ()
    };
    ($($write:ty),+ $(,)?) => {
        ($($write,)+)
    };
}

impl WriteSet for () {
    fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
        Vec::new()
    }
}

macro_rules! tuple_write_set {
    ($($name:ident),+) => {
        impl<$($name: WriteSet),+> WriteSet for ($($name,)+) {
            fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
                let mut declarations = Vec::new();
                $(declarations.extend($name::declarations::<Owner>());)+
                declarations
            }
        }
    };
}

tuple_write_set!(A);
tuple_write_set!(A, B);
tuple_write_set!(A, B, C);
tuple_write_set!(A, B, C, D);
tuple_write_set!(A, B, C, D, E);
tuple_write_set!(A, B, C, D, E, F);
tuple_write_set!(A, B, C, D, E, F, G);
tuple_write_set!(A, B, C, D, E, F, G, H);
tuple_write_set!(A, B, C, D, E, F, G, H, I);
tuple_write_set!(A, B, C, D, E, F, G, H, I, J);
tuple_write_set!(A, B, C, D, E, F, G, H, I, J, K);
tuple_write_set!(A, B, C, D, E, F, G, H, I, J, K, L);

/// A borrowed, dependency-tracked view of committed facts and the current
/// staged contribution.
pub struct Context<'tx, C: Component> {
    pub(crate) derive: DeriveCx<'tx>,
    key: C,
    task: TaskId,
    pub(crate) awaiting: bool,
    awaited: HashSet<TaskId>,
    staged: Vec<Box<dyn FnOnce(&mut DeriveCx<'tx>) -> std::result::Result<(), NodeError> + 'tx>>,
}

impl<'tx, C: Component> Context<'tx, C> {
    pub(crate) fn new(derive: DeriveCx<'tx>, key: C, task: TaskId) -> Self {
        Self {
            derive,
            key,
            task,
            awaiting: false,
            awaited: HashSet::new(),
            staged: Vec::new(),
        }
    }

    /// The identity of the currently running component instance.
    pub fn key(&self) -> &C {
        &self.key
    }

    /// Opens one borrowed view through the unified into-style entry point.
    pub fn view<V: ContextView<C>>(&mut self) -> V::Access<'_, 'tx> {
        V::open(self)
    }

    /// Reads one committed map fact and records the dependency.
    pub fn get<V: View>(&mut self, key: V::Key) -> Option<V::Value> {
        ReadGraph::get::<V>(&self.derive, key)
    }

    /// Stages one exclusive map definition for this run's contribution.
    pub fn define<V: View>(&mut self, key: V::Key, value: V::Value) -> Result<()> {
        self.derive.emit::<V>(key, value).map_err(Error::from)
    }

    /// Stages one support for a relation fact.
    pub fn support<R: Relation>(&mut self, fact: R::Fact) -> Result<()> {
        self.derive.emit_relation::<R>(fact).map_err(Error::from)
    }

    /// Reads one committed map fact or transparently suspends until it is
    /// published. The staged contribution is discarded and this component
    /// reruns after the fact appears.
    pub fn require<V: View>(&mut self, key: V::Key) -> Result<V::Value> {
        match self.get::<V>(key) {
            Some(value) => Ok(value),
            None => {
                self.awaiting = true;
                Err(Error::suspended())
            }
        }
    }

    /// Calls one required child component. The child's output participates in
    /// this component; if it is unavailable, the caller transparently
    /// suspends and reruns after the child publishes.
    pub fn call<Q: Component>(&mut self, value: Q) -> Result<Q::Output> {
        let task = component_task(value.clone());
        self.retain_task(task.clone());
        match self.derive.get::<Output<Q>>(value) {
            Some(output) => Ok(output),
            None => {
                self.derive.check_cycle(&self.task, &task)?;
                self.awaiting = true;
                self.awaited.insert(task);
                Err(Error::suspended())
            }
        }
    }

    /// Calls every child in the batch. All members are retained before any
    /// suspension, so independent work becomes ready for the same scheduler
    /// wave; the caller suspends once if any result is unavailable.
    pub fn call_all<Q: Component>(
        &mut self,
        values: impl IntoIterator<Item = Q>,
    ) -> Result<Vec<Q::Output>> {
        let values: Vec<Q> = values.into_iter().collect();
        let tasks: Vec<TaskId> = values
            .iter()
            .map(|value| {
                let task = component_task(value.clone());
                self.retain_task(task.clone());
                task
            })
            .collect();
        let mut outputs = Vec::with_capacity(values.len());
        let mut missing = Vec::new();
        for (value, task) in values.into_iter().zip(tasks) {
            match self.derive.get::<Output<Q>>(value) {
                Some(output) => outputs.push(output),
                None => {
                    self.derive.check_cycle(&self.task, &task)?;
                    missing.push(task);
                }
            }
        }
        if missing.is_empty() {
            Ok(outputs)
        } else {
            self.awaiting = true;
            self.awaited.extend(missing);
            Err(Error::suspended())
        }
    }

    /// Retains one child component whose result is intentionally ignored.
    pub fn keep<Q: Component>(&mut self, value: Q) {
        self.retain_task(component_task(value));
    }

    /// Retains every child in the batch. The current run's retained set is a
    /// complete replacement, so omitted children are reclaimed automatically.
    pub fn keep_all<Q: Component>(&mut self, values: impl IntoIterator<Item = Q>) {
        for value in values {
            self.retain_task(component_task(value));
        }
    }

    /// Retains one kernel provider task (for example a parser or scope
    /// catalog) as a child of this component. Use together with [`Self::get`]
    /// or [`Self::require`] to consume the provider's committed facts.
    pub fn retain_provider<P: NodeProvider>(&mut self, key: P::Key) {
        self.derive.defer::<P>(key);
    }

    fn retain_task(&mut self, task: TaskId) {
        self.derive.retain_task(task);
    }

    fn finish(
        mut self,
        outcome: Result<C::Output>,
    ) -> std::result::Result<DeriveCx<'tx>, NodeError> {
        match outcome {
            Ok(output) => {
                self.derive.emit::<Output<C>>(self.key.clone(), output)?;
                for staged in self.staged.drain(..) {
                    staged(&mut self.derive)?;
                }
            }
            Err(error) => match error.kind {
                ErrorKind::Suspended => {
                    self.awaiting = true;
                }
                ErrorKind::Node(error) => return Err(error),
            },
        }
        if self.awaiting {
            self.derive.mark_awaiting();
        }
        self.derive.awaited = self.awaited;
        self.derive.defer_connected(&self.task);
        Ok(self.derive)
    }
}

/// A borrowed, dependency-tracked API bound to one component context. The
/// marker type selects the destination API; built-in and application crates
/// can implement this trait.
pub trait ContextView<C: Component> {
    type Access<'cx, 'tx>
    where
        Self: 'cx,
        'tx: 'cx;

    fn open<'cx, 'tx>(cx: &'cx mut Context<'tx, C>) -> Self::Access<'cx, 'tx>;
}

/// One exclusive-value table view. Read operations record dependencies; `set`
/// stages the current run's complete definition.
pub struct Table<V: View>(PhantomData<fn() -> V>);

impl<C: Component, V: View> ContextView<C> for Table<V> {
    type Access<'cx, 'tx>
        = TableView<'cx, 'tx, C, V>
    where
        Self: 'cx,
        'tx: 'cx;

    fn open<'cx, 'tx>(cx: &'cx mut Context<'tx, C>) -> Self::Access<'cx, 'tx> {
        TableView {
            cx,
            _view: PhantomData,
        }
    }
}

impl<V: View> WriteSet for Table<V> {
    fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::map::<V>()]
    }
}

pub struct TableView<'cx, 'tx, C: Component, V: View> {
    cx: &'cx mut Context<'tx, C>,
    _view: PhantomData<fn() -> V>,
}

impl<'cx, 'tx, C: Component, V: View> TableView<'cx, 'tx, C, V> {
    pub fn get(&mut self, key: V::Key) -> Option<V::Value> {
        self.cx.get::<V>(key)
    }

    /// Reads one value or transparently suspends until it is published.
    pub fn require(&mut self, key: V::Key) -> Result<V::Value> {
        self.cx.require::<V>(key)
    }

    pub fn set(&mut self, key: V::Key, value: V::Value) -> Result<()> {
        self.cx.define::<V>(key, value)
    }
}

/// One support-counted set view. `add` stages the current run's support.
pub struct Set<R: Relation>(PhantomData<fn() -> R>);

impl<C: Component, R: Relation> ContextView<C> for Set<R> {
    type Access<'cx, 'tx>
        = SetView<'cx, 'tx, C, R>
    where
        Self: 'cx,
        'tx: 'cx;

    fn open<'cx, 'tx>(cx: &'cx mut Context<'tx, C>) -> Self::Access<'cx, 'tx> {
        SetView {
            cx,
            _view: PhantomData,
        }
    }
}

impl<R: Relation> WriteSet for Set<R> {
    fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::set::<R>()]
    }
}

pub struct SetView<'cx, 'tx, C: Component, R: Relation> {
    cx: &'cx mut Context<'tx, C>,
    _view: PhantomData<fn() -> R>,
}

impl<'cx, 'tx, C: Component, R: Relation> SetView<'cx, 'tx, C, R> {
    pub fn has(&mut self, fact: R::Fact) -> bool {
        ReadGraph::contains::<R>(&self.cx.derive, fact)
    }

    pub fn add(&mut self, fact: R::Fact) -> Result<()> {
        self.cx.support::<R>(fact)
    }
}

/// One indexed set view. `items` scans one observable bucket; `add` stages a
/// support for the bucket.
pub struct Index<R: IndexedRelation>(PhantomData<fn() -> R>);

impl<C: Component, R: IndexedRelation> ContextView<C> for Index<R> {
    type Access<'cx, 'tx>
        = IndexView<'cx, 'tx, C, R>
    where
        Self: 'cx,
        'tx: 'cx;

    fn open<'cx, 'tx>(cx: &'cx mut Context<'tx, C>) -> Self::Access<'cx, 'tx> {
        IndexView {
            cx,
            _view: PhantomData,
        }
    }
}

impl<R: IndexedRelation> WriteSet for Index<R> {
    fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::indexed_set::<R>()]
    }
}

pub struct IndexView<'cx, 'tx, C: Component, R: IndexedRelation> {
    cx: &'cx mut Context<'tx, C>,
    _view: PhantomData<fn() -> R>,
}

impl<'cx, 'tx, C: Component, R: IndexedRelation> IndexView<'cx, 'tx, C, R> {
    pub fn items(&mut self, index: R::Index) -> Vec<R::Fact> {
        ReadGraph::scan::<R>(&self.cx.derive, index)
    }

    pub fn add(&mut self, fact: R::Fact) -> Result<()> {
        self.cx.support::<R>(fact)
    }
}

/// One document's typed parse view. Reads retain the parser as a child, so
/// the parser stays materialized while any component observes its facts.
pub struct Parsed<Root: LexerRoot + Clone, Ast: Send + Sync + 'static>(
    PhantomData<fn() -> (Root, Ast)>,
);

impl<Root: LexerRoot + Clone, Ast: Send + Sync + 'static> WriteSet for Parsed<Root, Ast> {
    fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
        Vec::new()
    }
}

/// One domain's scope-graph view.
pub struct Scope<D: ScopeDomain>(PhantomData<fn() -> D>);

/// One component's canonical diagnostic collection.
pub struct Diagnostics<D: NodeValue>(PhantomData<fn() -> D>);

/// Write-set alias matching the plan's concrete examples.
pub type DiagnosticSet<D> = Diagnostics<D>;

impl<D: NodeValue> WriteSet for Diagnostics<D> {
    fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::map::<ComponentDiagnostics<Owner, D>>()]
    }
}

impl<C: Component, D: NodeValue> ContextView<C> for Diagnostics<D> {
    type Access<'cx, 'tx>
        = DiagnosticsView<'cx, 'tx, C, D>
    where
        Self: 'cx,
        'tx: 'cx;

    fn open<'cx, 'tx>(cx: &'cx mut Context<'tx, C>) -> Self::Access<'cx, 'tx> {
        let key = cx.key.clone();
        let buffer = Arc::new(RefCell::new(Vec::new()));
        let staged: Box<
            dyn FnOnce(&mut DeriveCx<'tx>) -> std::result::Result<(), NodeError> + 'tx,
        > = {
            let buffer = Arc::clone(&buffer);
            Box::new(move |derive| {
                let diagnostics = std::mem::take(&mut *buffer.borrow_mut());
                derive.emit::<ComponentDiagnostics<C, D>>(key, Arc::from(diagnostics))
            })
        };
        cx.staged.push(staged);
        DiagnosticsView {
            _borrow: PhantomData,
            buffer,
            _view: PhantomData,
        }
    }
}

pub struct DiagnosticsView<'cx, 'tx, C: Component, D: NodeValue> {
    _borrow: PhantomData<&'cx mut Context<'tx, C>>,
    buffer: Arc<RefCell<Vec<D>>>,
    _view: PhantomData<fn() -> D>,
}

impl<'cx, 'tx, C: Component, D: NodeValue> DiagnosticsView<'cx, 'tx, C, D> {
    /// Stages one diagnostic. The complete ordered set is published as one
    /// immutable `Arc<[D]>` only when this run completes successfully.
    pub fn add(&mut self, diagnostic: D) -> Result<()> {
        self.buffer.borrow_mut().push(diagnostic);
        Ok(())
    }
}

/// A structure type selects its own structural view directly.
// (Implemented in `component::structural::context`.)

// Write-set markers for canonical structural ports.
impl<S: crate::component::structural::Structure> WriteSet
    for crate::component::structural::StructureNode<S>
{
    fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::map::<
            crate::component::structural::StructureNode<S>,
        >()]
    }
}

impl<S: crate::component::structural::Structure> WriteSet
    for crate::component::structural::StructureChildren<S>
{
    fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::map::<
            crate::component::structural::StructureChildren<S>,
        >()]
    }
}

impl<S: crate::component::structural::Structure> WriteSet
    for crate::component::structural::StructureEdges<S>
{
    fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::indexed_set::<
            crate::component::structural::StructureEdges<S>,
        >()]
    }
}

impl<S, E, M> WriteSet for crate::component::structural::StructureEntries<S, E, M>
where
    S: crate::component::structural::Structure,
    E: NodeKey,
    M: NodeKey,
{
    fn declarations<Owner: Component>() -> Vec<PortDeclaration> {
        vec![PortDeclaration::indexed_set::<
            crate::component::structural::StructureEntries<S, E, M>,
        >()]
    }
}

/// Builds the graph task identity for one component value.
pub(crate) fn component_task<C: Component>(value: C) -> TaskId {
    TaskId {
        provider: TypeId::of::<ComponentProvider<C>>(),
        key: crate::scheme::node::KeyId::new(value),
    }
}

/// The private provider bridge translating component runs into graph
/// derivations. This is the only code that constructs suspension state or
/// touches derivation completion.
pub(crate) struct ComponentProvider<C: Component>(PhantomData<fn() -> C>);

impl<C: Component> ComponentProvider<C> {
    pub(crate) fn new() -> Self {
        Self(PhantomData)
    }

    pub(crate) fn schema() -> NodeSchema {
        let mut ports = C::Writes::declarations::<C>();
        ports.push(PortDeclaration::map::<Output<C>>());
        NodeSchema::new(std::any::type_name::<C>(), ports)
    }
}

impl<C: Component> ErasedProvider for ComponentProvider<C> {
    fn name(&self) -> &'static str {
        std::any::type_name::<C>()
    }

    fn schema(&self) -> NodeSchema {
        Self::schema()
    }

    fn run<'tx>(
        &self,
        cx: DeriveCx<'tx>,
        task: TaskId,
    ) -> std::result::Result<DeriveCx<'tx>, NodeError> {
        let key = task
            .key
            .get::<C>()
            .ok_or(NodeError::MissingProvider(std::any::type_name::<C>()))?;
        let mut context = Context::new(cx, key.clone(), task);
        let outcome = C::run(&key, &mut context);
        context.finish(outcome)
    }

    fn reclaim<'tx>(
        &self,
        _cx: &mut ReclaimCx<'tx>,
        _task: TaskId,
    ) -> std::result::Result<(), NodeError> {
        Ok(())
    }
}
