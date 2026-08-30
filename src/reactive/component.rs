//! First-class reactive components (plan §6).
//!
//! A component is ONE named computation whose identity is
//! `(definition marker TypeId, exact semantic input)` — never a callsite,
//! installation ordinal, or worker order. The [`#[component]`](macro@crate::component)
//! macro generates a zero-sized definition marker, the same-named definition
//! module with its `Component` mount type, and typed call wrappers; the
//! runtime stamps every evaluation with the definition id so reaction
//! graphs, retirement, and duplicate-install rejection all key off the
//! authored definition.
//!
//! Component inputs are semantic values: [`Each`] for map membership, an
//! `AstBox<T>` tree node, or any plain value a parent component passes.
//! Reads happen through view effects and returned effects own the outputs.
//! Raw effect handles stay inside the crate; application code reaches
//! effects only through view witnesses, returned effect types, and
//! `T::render` inside a `#[component]` body.

use crate::reactive::abstract_tree::{AbstractTreeFamily, AbstractTreeNode, AstBox};
use crate::reactive::kind::{GraphEmit, GraphView, MapView, TreeEmit, TreeView, ViewKind};
use crate::reactive::{Error, Result};
use std::any::TypeId;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

/// The runtime record of one installed component definition.
#[derive(Clone, Debug)]
pub(crate) struct DefinitionEntry {
    /// Module-qualified authored path (`module::function`).
    pub descriptor: &'static str,
    /// The driving-port kind wire name.
    pub driver: &'static str,
}

/// Per-engine registry of installed definitions. A second installer for the
/// same marker is a deterministic error before anything mutates.
#[derive(Default)]
pub(crate) struct DefinitionRegistry {
    by_marker: HashMap<TypeId, DefinitionEntry>,
}

impl DefinitionRegistry {
    pub(crate) fn register(
        &mut self,
        marker: TypeId,
        descriptor: &'static str,
        driver: &'static str,
    ) -> Result<()> {
        match self.by_marker.get(&marker) {
            Some(existing) => Err(Error::DuplicateComponent {
                descriptor: existing.descriptor.to_string(),
            }),
            None => {
                self.by_marker
                    .insert(marker, DefinitionEntry { descriptor, driver });
                Ok(())
            }
        }
    }

    pub(crate) fn descriptor_of(&self, marker: &TypeId) -> Option<&'static str> {
        self.by_marker.get(marker).map(|entry| entry.descriptor)
    }

    pub(crate) fn len(&self) -> usize {
        self.by_marker.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_marker.is_empty()
    }

    /// Definitions in deterministic order for snapshots/reports.
    pub(crate) fn descriptors(&self) -> Vec<&'static str> {
        let mut rows: Vec<(String, &'static str)> = self
            .by_marker
            .values()
            .map(|entry| (entry.descriptor.to_string(), entry.descriptor))
            .collect();
        rows.sort();
        rows.into_iter().map(|(_, descriptor)| descriptor).collect()
    }
}

/// Implemented by the zero-sized marker the `#[component]` macro generates.
///
/// The descriptor is the module-qualified authored path; the registry uses
/// it for duplicate-install rejection and reaction attribution.
pub trait ComponentDefinition {
    #[doc(hidden)]
    fn __descriptor() -> &'static str;
}

/// Semantic map-entry input. `key` is the lifecycle identity and does not
/// read the entry payload; `value` records one exact payload dependency.
pub struct Each<V: MapView + ViewKind<Observe = crate::reactive::kind::MapObserve<V>>> {
    key: V::Input,
    _marker: PhantomData<fn() -> V>,
}

impl<V> Each<V>
where
    V: MapView + ViewKind<Observe = crate::reactive::kind::MapObserve<V>>,
{
    #[doc(hidden)]
    pub fn __from_key(key: V::Input) -> Self {
        Self {
            key,
            _marker: PhantomData,
        }
    }

    /// Borrows the stable semantic key without reading the map payload.
    pub fn key(&self) -> &V::Input {
        &self.key
    }

    /// Moves the already-owned key into a returned effect.
    pub fn into_key(self) -> V::Input {
        self.key
    }

    /// Reads this entry's optional payload as one exact reactive fact.
    pub fn value(&self) -> Result<Option<Arc<V::Output>>> {
        crate::reactive::kind::observe_view::<V>()?.get(&self.key)
    }

    /// Reads this entry's committed payload from the previous epoch as one
    /// exact reactive fact. Used by publication owners implementing the
    /// close-tombstone pattern (previous value when the current is absent).
    pub fn value_previous(&self) -> Result<Option<Arc<V::Output>>> {
        crate::reactive::kind::observe_view::<V>()?.get_previous(&self.key)
    }
}

/// One opaque publication operation accepted by [`emit`].
pub trait Effect: Sized + Send + Sync + 'static {
    /// Macro ABI: applies this desired output to the active invocation's
    /// pending output buffer.
    fn __apply(&self) -> Result<()>;
}

/// Compatibility marker for code generated by older versions of the macro.
/// New authored functions should use [`Effect`] through [`emit`] instead of
/// returning an effect bundle.
pub trait Effects: Effect {}

impl<T: Effect> Effects for T {}

/// Emits one typed operation from the currently evaluating component.
///
/// The operation is buffered until the invocation succeeds. Replace-mode
/// outputs omitted by a later successful evaluation are retracted by the
/// existing invocation reconciliation path.
pub fn emit<E: Effect>(effect: E) -> Result<()> {
    effect.__apply()
}

pub struct Set<V: MapView> {
    key: V::Input,
    value: V::Output,
}

impl<V: MapView> Clone for Set<V> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            value: self.value.clone(),
        }
    }
}
impl<V: MapView> std::fmt::Debug for Set<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Set")
            .field("key", &self.key)
            .field("value", &self.value)
            .finish()
    }
}
impl<V: MapView> PartialEq for Set<V> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.value == other.value
    }
}

impl<V: MapView> Set<V> {
    #[doc(hidden)]
    pub fn __new(key: V::Input, value: V::Output) -> Self {
        Self { key, value }
    }
}

pub struct Remove<V: MapView> {
    key: V::Input,
}
impl<V: MapView> Clone for Remove<V> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
        }
    }
}
impl<V: MapView> std::fmt::Debug for Remove<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remove").field("key", &self.key).finish()
    }
}
impl<V: MapView> PartialEq for Remove<V> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<V: MapView> Remove<V> {
    #[doc(hidden)]
    pub fn __new(key: V::Input) -> Self {
        Self { key }
    }
}

impl<V> Effect for Set<V>
where
    V: MapView + ViewKind<Emit = crate::reactive::kind::MapEmit<V>>,
{
    fn __apply(&self) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.insert(self.key.clone(), self.value.clone())
    }
}

impl<V> Effect for Remove<V>
where
    V: MapView + ViewKind<Emit = crate::reactive::kind::MapEmit<V>>,
{
    fn __apply(&self) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.remove(self.key.clone())
    }
}

/// Desired ordered contents of one list domain.
pub struct Replace<V: crate::reactive::kind::ListView> {
    key: V::Key,
    items: Vec<V::Item>,
}
impl<V: crate::reactive::kind::ListView> Clone for Replace<V> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            items: self.items.clone(),
        }
    }
}
impl<V: crate::reactive::kind::ListView> std::fmt::Debug for Replace<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replace")
            .field("key", &self.key)
            .field("items", &self.items)
            .finish()
    }
}
impl<V: crate::reactive::kind::ListView> PartialEq for Replace<V> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.items == other.items
    }
}
impl<V: crate::reactive::kind::ListView> Replace<V> {
    #[doc(hidden)]
    pub fn __new(key: V::Key, items: Vec<V::Item>) -> Self {
        Self { key, items }
    }
}
impl<V> Effect for Replace<V>
where
    V: crate::reactive::kind::ListView + ViewKind<Emit = crate::reactive::kind::ListEmit<V>>,
{
    fn __apply(&self) -> Result<()> {
        crate::reactive::kind::emit_view::<V>()?.replace(&self.key, self.items.clone())
    }
}

/// Desired publication for one graph node and its labelled outgoing buckets.
pub struct GraphRender<V: crate::reactive::kind::GraphView> {
    node: crate::reactive::view::Node<V>,
    payload: Option<V::NodePayload>,
    buckets: Vec<(V::Label, Vec<crate::reactive::view::Node<V>>)>,
}
impl<V: crate::reactive::kind::GraphView> Clone for GraphRender<V> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            payload: self.payload.clone(),
            buckets: self.buckets.clone(),
        }
    }
}
impl<V: crate::reactive::kind::GraphView> std::fmt::Debug for GraphRender<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphRender")
            .field("node", &self.node)
            .field("payload", &self.payload)
            .field("buckets", &self.buckets)
            .finish()
    }
}
impl<V: crate::reactive::kind::GraphView> PartialEq for GraphRender<V> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node && self.payload == other.payload && self.buckets == other.buckets
    }
}
impl<V: crate::reactive::kind::GraphView> GraphRender<V> {
    /// Creates one graph publication for the active component's automatic
    /// graph slot.  The slot identity is the component definition, exact
    /// semantic input, and graph view type; it is never a call-site ordinal.
    pub fn automatic(payload: V::NodePayload) -> Result<Self>
    where
        V: crate::reactive::view::View,
    {
        let node = crate::reactive::plain::automatic_graph_node_id::<V>()?;
        Ok(Self::new(node, payload))
    }

    /// Internal constructor used by framework façades for an already-owned
    /// graph node.  Application code should use a semantic `Scope` façade.
    #[doc(hidden)]
    pub(crate) fn from_node(
        node: crate::reactive::view::Node<V>,
        payload: Option<V::NodePayload>,
    ) -> Self {
        Self {
            node,
            payload,
            buckets: Vec::new(),
        }
    }

    #[doc(hidden)]
    pub(crate) fn new(node: crate::reactive::view::Node<V>, payload: V::NodePayload) -> Self {
        Self::from_node(node, Some(payload))
    }

    pub fn bucket<T>(mut self, label: V::Label, targets: impl IntoIterator<Item = T>) -> Self
    where
        T: Into<crate::reactive::view::Node<V>>,
    {
        self.buckets
            .push((label, targets.into_iter().map(Into::into).collect()));
        self
    }

    /// Internal patch constructor for framework façades that update only
    /// buckets on an already-owned graph node.
    #[doc(hidden)]
    pub(crate) fn patch_node(node: crate::reactive::view::Node<V>) -> Self {
        Self::from_node(node, None)
    }
}
impl<V> Effect for GraphRender<V>
where
    V: crate::reactive::kind::GraphView + ViewKind<Emit = crate::reactive::kind::GraphEmit<V>>,
{
    fn __apply(&self) -> Result<()> {
        let emit = crate::reactive::kind::emit_view::<V>()?;
        if let Some(payload) = &self.payload {
            emit.set_node(self.node.clone(), payload.clone())?;
        }
        for (label, targets) in &self.buckets {
            emit.set_bucket(self.node.clone(), label.clone(), targets.clone())?;
        }
        Ok(())
    }
}

impl Effect for () {
    fn __apply(&self) -> Result<()> {
        Ok(())
    }
}

impl<E: Effect> Effect for Option<E> {
    fn __apply(&self) -> Result<()> {
        if let Some(effect) = self {
            effect.__apply()?;
        }
        Ok(())
    }
}

impl<E: Effect> Effect for Vec<E> {
    fn __apply(&self) -> Result<()> {
        for effect in self {
            effect.__apply()?;
        }
        Ok(())
    }
}

macro_rules! tuple_effects {
    ($($name:ident),+) => {
        impl<$($name: Effect),+> Effect for ($($name,)+) {
            fn __apply(&self) -> Result<()> {
                let ($($name,)+) = self;
                $($name.__apply()?;)+
                Ok(())
            }
        }
    };
}
tuple_effects!(A, B);
tuple_effects!(A, B, C);
tuple_effects!(A, B, C, D);
tuple_effects!(A, B, C, D, E);

/// The normalized key accepted by a heterogeneous component.
///
/// The member descriptor is derived from the generated abstract-tree member
/// type. It is retained alongside the complete opaque node identity so case
/// dispatch never has to materialize the node or inspect a broad family view.
pub struct FamilyNode<F: AbstractTreeFamily> {
    node: AstBox<()>,
    member: &'static str,
    marker: PhantomData<fn() -> F>,
}

impl<F: AbstractTreeFamily> Clone for FamilyNode<F> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            member: self.member,
            marker: PhantomData,
        }
    }
}

impl<F: AbstractTreeFamily> PartialEq for FamilyNode<F> {
    fn eq(&self, other: &Self) -> bool {
        self.member == other.member && self.node == other.node
    }
}

impl<F: AbstractTreeFamily> Eq for FamilyNode<F> {}

impl<F: AbstractTreeFamily> std::hash::Hash for FamilyNode<F> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.member.hash(state);
        self.node.hash(state);
    }
}

impl<F: AbstractTreeFamily> std::fmt::Debug for FamilyNode<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FamilyNode")
            .field("node", &self.node)
            .field("member", &self.member)
            .finish()
    }
}

impl<F: AbstractTreeFamily> FamilyNode<F> {
    /// Normalizes one typed member without reading its discriminant.
    #[doc(hidden)]
    pub fn from_typed<T>(node: AstBox<T>) -> Self
    where
        T: AbstractTreeNode<Family = F>,
    {
        Self::from_erased(node.erased(), T::__member())
    }

    /// Macro/runtime constructor for an already erased member key.
    #[doc(hidden)]
    pub fn from_erased(node: AstBox<()>, member: &'static str) -> Self {
        Self {
            node,
            member,
            marker: PhantomData,
        }
    }

    /// Returns the opaque node identity for use with generated accessors.
    #[doc(hidden)]
    pub fn erased(&self) -> AstBox<()> {
        self.node.clone()
    }

    /// Returns the stable generated member descriptor.
    #[doc(hidden)]
    pub fn member(&self) -> &'static str {
        self.member
    }

    /// Reads a typed member view. The generated reader validates that the
    /// requested member matches this normalized key.
    pub fn view<T>(&self) -> Result<T::View>
    where
        T: AbstractTreeNode<Family = F>,
    {
        T::__view(AstBox::<T>::from_erased(self.node.clone()))
    }

    /// Starts the inline heterogeneous case chain.
    pub fn cases<P, O>(self, props: P) -> CaseChain<F, P, O> {
        CaseChain {
            node: self,
            props,
            cases: HashMap::new(),
            duplicate: false,
        }
    }
}

impl<F, T> From<AstBox<T>> for FamilyNode<F>
where
    F: AbstractTreeFamily,
    T: AbstractTreeNode<Family = F>,
{
    fn from(node: AstBox<T>) -> Self {
        Self::from_typed(node)
    }
}

/// Sealed-by-convention adapter for inline case closures. The macro packs the
/// uniform trailing props into one internal tuple, so authored closures can
/// retain their natural zero-through-three argument spelling.
pub trait CaseHandler<T: AbstractTreeNode, P, O>: Send + Sync + 'static {
    #[doc(hidden)]
    fn __invoke(&self, node: AstBox<T>, props: P) -> Result<O>;
}

impl<T, P, O, H> CaseHandler<T, P, O> for H
where
    T: AbstractTreeNode,
    P: Send + 'static,
    H: Fn(AstBox<T>, P) -> Result<O> + Send + Sync + 'static,
{
    fn __invoke(&self, node: AstBox<T>, props: P) -> Result<O> {
        self(node, props)
    }
}

/// Adapter for the total-chain fallback closure.
pub trait OtherwiseHandler<F: AbstractTreeFamily, P, O>: Send + Sync + 'static {
    #[doc(hidden)]
    fn __invoke(&self, node: FamilyNode<F>, props: P) -> Result<O>;
}

impl<F, P, O, H> OtherwiseHandler<F, P, O> for H
where
    F: AbstractTreeFamily,
    P: Send + 'static,
    H: Fn(FamilyNode<F>, P) -> Result<O> + Send + Sync + 'static,
{
    fn __invoke(&self, node: FamilyNode<F>, props: P) -> Result<O> {
        self(node, props)
    }
}

/// Runtime storage for one generated total case chain. Construction is
/// local to an invocation; only the selected closure is ever called.
pub struct CaseChain<F: AbstractTreeFamily, P, O> {
    node: FamilyNode<F>,
    props: P,
    cases: HashMap<&'static str, Box<dyn Fn(AstBox<()>, P) -> Result<O> + Send + Sync>>,
    duplicate: bool,
}

impl<F, P, O> CaseChain<F, P, O>
where
    F: AbstractTreeFamily,
    P: Send + 'static,
    O: Send + 'static,
{
    /// Adds one typed member case. The map provides constant-time dispatch by
    /// generated descriptor; it never reads a family view to select the case.
    pub fn case<T, H>(mut self, handler: H) -> Self
    where
        T: AbstractTreeNode<Family = F>,
        H: CaseHandler<T, P, O>,
    {
        let member = T::__member();
        if self.cases.contains_key(member) {
            self.duplicate = true;
        } else {
            self.cases.insert(
                member,
                Box::new(move |node, props| {
                    handler.__invoke(AstBox::<T>::from_erased(node), props)
                }),
            );
        }
        self
    }

    /// Completes and evaluates a total chain with an explicit fallback.
    pub fn otherwise<H>(self, handler: H) -> Result<O>
    where
        H: OtherwiseHandler<F, P, O>,
    {
        if self.duplicate {
            return Err(Error::Internal("duplicate abstract-tree component case".into()));
        }
        if let Some(case) = self.cases.get(self.node.member()) {
            case(self.node.erased(), self.props)
        } else {
            handler.__invoke(self.node, self.props)
        }
    }
}

/// Executes a generated component call as a keyed child of the active
/// component instance.
#[doc(hidden)]
pub fn __call_component<D, F, A, B>(function: F, input: A) -> Result<B>
where
    D: ComponentDefinition + 'static,
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static,
    B: Effects + Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static,
{
    crate::reactive::plain::run_component_effect::<D, F, A, B>(function, input)
}

/// Executes one ordinary component function with a stable key and replaceable
/// value parameters. The key establishes lifecycle and ownership identity;
/// props are compared independently and dirty the same invocation in place.
#[doc(hidden)]
pub fn __call_component_props<D, F, K, P, B>(
    function: F,
    key: K,
    props: P,
) -> Result<B>
where
    D: ComponentDefinition + 'static,
    F: Fn(K, P) -> Result<B> + Clone + Send + Sync + 'static,
    K: Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static,
    P: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static,
    B: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static,
{
    crate::reactive::plain::run_component_value::<D, F, K, P, B>(function, key, props)
}
/// Executes a tree component call without entering the child body
/// recursively. The returned identity is available immediately and the
/// child's queued evaluation is drained by the engine.
#[doc(hidden)]
pub fn __call_tree_component<D, F, A, T>(
    function: F,
    input: A,
) -> Result<crate::reactive::abstract_tree::AstBox<T>>
where
    D: ComponentDefinition + 'static,
    F: Fn(A) -> Result<crate::reactive::abstract_tree::AstBox<T>> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static,
    T: crate::reactive::abstract_tree::AbstractTreeNode,
{
    crate::reactive::plain::run_tree_component_effect::<D, F, A, T>(function, input)
}
