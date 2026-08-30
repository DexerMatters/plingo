//! Backend-neutral abstract-tree identities, facts, readers, and render support.
//!
//! This module is the sealed runtime seam used by `#[abstract_tree]`.  Parser
//! publication and component rendering both use the same fact dimensions; the
//! generated code supplies only the semantic schema and field descriptors.

use std::any::{Any, TypeId};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::sync::Arc;

use super::kind::KeyBounds;
use super::value::{KeyValue, Value};
use super::view::View;
use super::{Error, Result, Snapshot};
/// The opaque identity of one abstract-tree node.
///
/// The complete identity is retained behind an `Arc`; `raw` is only an index
/// hint.  It is intentionally impossible to construct one from a raw value.
pub struct AstBox<T> {
    raw: u64,
    identity: Arc<dyn KeyValue>,
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for AstBox<T> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw,
            identity: Arc::clone(&self.identity),
            marker: PhantomData,
        }
    }
}

impl<T> PartialEq for AstBox<T> {
    fn eq(&self, other: &Self) -> bool {
        self.identity.eq_value(other.identity.as_ref())
    }
}
impl<T> Eq for AstBox<T> {}
impl<T> Hash for AstBox<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}
impl<T> fmt::Debug for AstBox<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("AstBox").field(&self.raw).finish()
    }
}

impl<T> AstBox<T> {
    #[doc(hidden)]
    pub fn from_parts(raw: u64, identity: Arc<dyn KeyValue>) -> Self {
        Self {
            raw,
            identity,
            marker: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn erased(&self) -> AstBox<()> {
        AstBox::from_parts(self.raw, Arc::clone(&self.identity))
    }

    #[doc(hidden)]
    pub fn from_erased<U>(node: AstBox<()>) -> AstBox<U> {
        AstBox::from_parts(node.raw, node.identity)
    }

    /// Reads this node's discriminant and returns its generated lazy view.
    pub fn view(&self) -> Result<<T as AbstractTreeNode>::View>
    where
        T: AbstractTreeNode,
    {
        T::__view(self.clone())
    }

    /// Reads every active field and reconstructs the selected enum value.
    pub fn materialize(&self) -> Result<T>
    where
        T: TreeRender,
    {
        T::__materialize(self.clone())
    }

    /// Reads the parent relation for this node.
    pub fn parent(&self) -> Result<Option<AstBox<T>>>
    where
        T: AbstractTreeNode,
    {
        let parent = __read_parent::<T::Family>(self.erased())?;
        Ok(parent.map(|node| AstBox::<T>::from_erased(node)))
    }

    /// Stable identity equality is exposed without exposing the encoded key.
    pub fn same_identity<U>(&self, other: &AstBox<U>) -> bool {
        self.identity.eq_value(other.identity.as_ref())
    }
}

/// Supplies the stable child handle used by parser publication.  The
/// implementation is deliberately generic so generated syntax code need not
/// know whether a child is parser-backed or an automatic reactive node.
#[doc(hidden)]
pub trait SyntaxChild {
    fn __syntax_child_id(&self) -> u64;
}

#[doc(hidden)]
pub fn __syntax_child_id<T: SyntaxChild + ?Sized>(child: &T) -> u64 {
    child.__syntax_child_id()
}

impl<T> SyntaxChild for AstBox<T> {
    fn __syntax_child_id(&self) -> u64 {
        self.raw
    }
}

/// One exact abstract-tree fact key.  The generated family marker selects the
/// view type and its domain; descriptors preserve semantic field identity.
///
/// Macro ABI: generated code addresses facts through this enum; application
/// code reads trees through `AstBox` accessors and never names keys.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TreeKey<D: KeyBounds> {
    Member(AstBox<()>, &'static str),
    Kind(AstBox<()>, &'static str),
    Leaf(AstBox<()>, &'static str, &'static str, &'static str),
    Child(AstBox<()>, &'static str, &'static str, &'static str),
    ChildOrder(AstBox<()>, &'static str, &'static str, &'static str),
    ChildLink(
        AstBox<()>,
        &'static str,
        &'static str,
        &'static str,
        AstBox<()>,
    ),
    Parent(AstBox<()>),
    RootOrder(D),
    RootLink(D, AstBox<()>),
}

/// Closed fact values for the generated abstract-tree codec.
///
/// Macro ABI: paired with [`TreeKey`]; application code never constructs or
/// matches these values.
#[doc(hidden)]
#[derive(Clone)]
pub enum TreeFact {
    Member(&'static str),
    Kind(&'static str),
    Leaf(Arc<dyn Value>),
    Child(Option<AstBox<()>>),
    Order(Arc<[AstBox<()>]>),
    Link(AstBox<()>),
    Parent(Option<AstBox<()>>),
    RootOrder(Arc<[AstBox<()>]>),
    RootLink(AstBox<()>),
}

impl PartialEq for TreeFact {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Member(a), Self::Member(b)) | (Self::Kind(a), Self::Kind(b)) => a == b,
            (Self::Leaf(a), Self::Leaf(b)) => a.value_eq(b.as_ref()),
            (Self::Child(a), Self::Child(b)) | (Self::Parent(a), Self::Parent(b)) => a == b,
            (Self::Order(a), Self::Order(b)) | (Self::RootOrder(a), Self::RootOrder(b)) => a == b,
            (Self::Link(a), Self::Link(b)) | (Self::RootLink(a), Self::RootLink(b)) => a == b,
            _ => false,
        }
    }
}
impl Eq for TreeFact {}
impl fmt::Debug for TreeFact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Member(value) => formatter.debug_tuple("Member").field(value).finish(),
            Self::Kind(value) => formatter.debug_tuple("Kind").field(value).finish(),
            Self::Leaf(value) => formatter.debug_tuple("Leaf").field(value).finish(),
            Self::Child(value) => formatter.debug_tuple("Child").field(value).finish(),
            Self::Order(value) => formatter.debug_tuple("ChildOrder").field(value).finish(),
            Self::Link(value) => formatter.debug_tuple("ChildLink").field(value).finish(),
            Self::Parent(value) => formatter.debug_tuple("Parent").field(value).finish(),
            Self::RootOrder(value) => formatter.debug_tuple("RootOrder").field(value).finish(),
            Self::RootLink(value) => formatter.debug_tuple("RootLink").field(value).finish(),
        }
    }
}

/// A zero-copy cursor over one generated ordered child field.
pub struct ChildList<T> {
    nodes: Arc<[AstBox<()>]>,
    marker: PhantomData<fn() -> T>,
}
impl<T> Clone for ChildList<T> {
    fn clone(&self) -> Self {
        Self {
            nodes: Arc::clone(&self.nodes),
            marker: PhantomData,
        }
    }
}
impl<T> fmt::Debug for ChildList<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(self.iter()).finish()
    }
}
impl<T> ChildList<T> {
    pub(crate) fn new(nodes: Arc<[AstBox<()>]>) -> Self {
        Self {
            nodes,
            marker: PhantomData,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn get(&self, index: usize) -> Option<AstBox<T>> {
        self.nodes
            .get(index)
            .cloned()
            .map(|node| AstBox::<T>::from_erased(node))
    }
    pub fn iter(&self) -> impl Iterator<Item = AstBox<T>> + '_ {
        self.nodes
            .iter()
            .cloned()
            .map(|node| AstBox::<T>::from_erased(node))
    }
    pub fn to_vec(&self) -> Vec<AstBox<T>> {
        self.iter().collect()
    }
}
impl<'a, T> IntoIterator for &'a ChildList<T> {
    type Item = AstBox<T>;
    type IntoIter = Box<dyn Iterator<Item = AstBox<T>> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// Generated family marker contract.
pub trait AbstractTreeFamily:
    View<Input = TreeKey<Self::Domain>, Output = TreeFact> + 'static
{
    type Domain: KeyBounds;
    type Root: AbstractTreeNode;
}

/// Generated semantic enum contract (the read side).
///
/// Every generated family member implements this. Value round-trips
/// (render/materialize) are a separate capability because parser-published
/// syntax families hold arena-backed child handles in their enum values and
/// therefore support fact reads but not value renders (plan §5.5).
pub trait AbstractTreeNode: Sized + Send + Sync + 'static {
    type Family: AbstractTreeFamily;
    type View;
    type Kind: Clone + Copy + Eq + fmt::Debug + Send + Sync + 'static;

    #[doc(hidden)]
    fn __member() -> &'static str;
    #[doc(hidden)]
    fn __view(node: AstBox<Self>) -> Result<Self::View>;
    #[doc(hidden)]
    fn __kind(node: AstBox<Self>) -> Result<Self::Kind>;
    #[doc(hidden)]
    fn __snapshot_view(snapshot: &Snapshot, node: AstBox<Self>) -> Result<Self::View>;
}

/// The value round-trip capability of one generated semantic enum.
pub trait TreeRender: AbstractTreeNode + Sized {
    #[doc(hidden)]
    fn __materialize(node: AstBox<Self>) -> Result<Self>;
    #[doc(hidden)]
    fn __snapshot_materialize(snapshot: &Snapshot, node: AstBox<Self>) -> Result<Self>;
    #[doc(hidden)]
    fn __render(value: Self) -> Result<AstBox<Self>>;
}

/// Reads one exact generated discriminant fact.
#[doc(hidden)]
pub fn __read_kind<F: AbstractTreeFamily>(
    node: AstBox<()>,
    member: &'static str,
) -> Result<&'static str> {
    let fact = __observe::<F>(TreeKey::Kind(node, member))?;
    match fact.as_deref() {
        Some(TreeFact::Kind(kind)) => Ok(kind),
        _ => Err(Error::Internal(
            "abstract-tree kind fact missing or malformed".into(),
        )),
    }
}

/// Reads one exact generated member fact for the requested member.
#[doc(hidden)]
pub fn __read_member<F: AbstractTreeFamily>(node: AstBox<()>, member: &'static str) -> Result<()> {
    let fact = __observe::<F>(TreeKey::Member(node, member))?;
    match fact.as_deref() {
        Some(TreeFact::Member(actual)) if *actual == member => Ok(()),
        _ => Err(Error::Internal(
            "abstract-tree member fact missing or malformed".into(),
        )),
    }
}

/// Reads one exact leaf fact without cloning its payload.
#[doc(hidden)]
pub fn __read_leaf<F, T>(
    node: AstBox<()>,
    member: &'static str,
    variant: &'static str,
    field: &'static str,
) -> Result<Arc<T>>
where
    F: AbstractTreeFamily,
    T: Clone + PartialEq + fmt::Debug + Send + Sync + 'static,
{
    let fact = __observe::<F>(TreeKey::Leaf(node, member, variant, field))?;
    let Some(TreeFact::Leaf(value)) = fact.as_deref() else {
        return Err(Error::Internal(
            "abstract-tree leaf fact missing or malformed".into(),
        ));
    };
    let erased: Arc<dyn Any + Send + Sync> = Arc::clone(value) as Arc<dyn Any + Send + Sync>;
    erased
        .downcast::<T>()
        .map_err(|_| Error::Internal("abstract-tree leaf type mismatch".into()))
}

/// Reads one required or optional child fact.
pub fn __read_child<F, T>(
    node: AstBox<()>,
    member: &'static str,
    variant: &'static str,
    field: &'static str,
) -> Result<Option<AstBox<T>>>
where
    F: AbstractTreeFamily,
    T: AbstractTreeNode<Family = F>,
{
    let fact = __observe::<F>(TreeKey::Child(node, member, variant, field))?;
    match fact.as_deref() {
        Some(TreeFact::Child(child)) => {
            Ok(child.clone().map(|node| AstBox::<T>::from_erased(node)))
        }
        _ => Err(Error::Internal(
            "abstract-tree child fact missing or malformed".into(),
        )),
    }
}

/// Reads one ordered child field.  The order fact and traversed links are
/// separate dependencies, so retaining a link does not wake on reorder.
#[doc(hidden)]
pub fn __read_children<F, T>(
    node: AstBox<()>,
    member: &'static str,
    variant: &'static str,
    field: &'static str,
) -> Result<ChildList<T>>
where
    F: AbstractTreeFamily,
    T: AbstractTreeNode<Family = F>,
{
    let order = __observe::<F>(TreeKey::ChildOrder(node.clone(), member, variant, field))?;
    let Some(TreeFact::Order(order)) = order.as_deref() else {
        return Err(Error::Internal(
            "abstract-tree child order fact missing or malformed".into(),
        ));
    };
    let mut links = Vec::with_capacity(order.len());
    for child in order.iter() {
        let fact = __observe::<F>(TreeKey::ChildLink(
            node.clone(),
            member,
            variant,
            field,
            child.clone(),
        ))?;
        if let Some(TreeFact::Link(link)) = fact.as_deref() {
            links.push(link.clone());
        }
    }
    Ok(ChildList::new(links.into()))
}

#[doc(hidden)]
pub fn __read_parent<F: AbstractTreeFamily>(node: AstBox<()>) -> Result<Option<AstBox<()>>> {
    let fact = __observe::<F>(TreeKey::Parent(node))?;
    match fact.as_deref() {
        Some(TreeFact::Parent(parent)) => Ok(parent.clone()),
        _ => Err(Error::Internal(
            "abstract-tree parent fact missing or malformed".into(),
        )),
    }
}

#[doc(hidden)]
pub fn __snapshot_leaf<F, T>(
    snapshot: &Snapshot,
    node: AstBox<()>,
    member: &'static str,
    variant: &'static str,
    field: &'static str,
) -> Result<Arc<T>>
where
    F: AbstractTreeFamily,
    T: Clone + PartialEq + fmt::Debug + Send + Sync + 'static,
{
    let fact = snapshot.__plain_observe::<F>(TreeKey::Leaf(node, member, variant, field));
    let Some(fact) = fact else {
        return Err(Error::Internal(
            "abstract-tree snapshot leaf missing".into(),
        ));
    };
    let TreeFact::Leaf(value) = fact.as_ref() else {
        return Err(Error::Internal(
            "abstract-tree snapshot leaf malformed".into(),
        ));
    };
    let erased: Arc<dyn Any + Send + Sync> = Arc::clone(value) as Arc<dyn Any + Send + Sync>;
    erased
        .downcast::<T>()
        .map_err(|_| Error::Internal("abstract-tree snapshot leaf type mismatch".into()))
}

#[doc(hidden)]
pub fn __snapshot_child<F, T>(
    snapshot: &Snapshot,
    node: AstBox<()>,
    member: &'static str,
    variant: &'static str,
    field: &'static str,
) -> Result<Option<AstBox<T>>>
where
    F: AbstractTreeFamily,
    T: AbstractTreeNode<Family = F>,
{
    match snapshot
        .__plain_observe::<F>(TreeKey::Child(node, member, variant, field))
        .as_deref()
    {
        Some(TreeFact::Child(child)) => {
            Ok(child.clone().map(|node| AstBox::<T>::from_erased(node)))
        }
        _ => Err(Error::Internal(
            "abstract-tree snapshot child missing or malformed".into(),
        )),
    }
}

#[doc(hidden)]
pub fn __snapshot_children<F, T>(
    snapshot: &Snapshot,
    node: AstBox<()>,
    member: &'static str,
    variant: &'static str,
    field: &'static str,
) -> Result<ChildList<T>>
where
    F: AbstractTreeFamily,
    T: AbstractTreeNode<Family = F>,
{
    let order =
        snapshot.__plain_observe::<F>(TreeKey::ChildOrder(node.clone(), member, variant, field));
    let Some(TreeFact::Order(order)) = order.as_deref() else {
        return Err(Error::Internal(
            "abstract-tree snapshot order missing".into(),
        ));
    };
    let mut links = Vec::with_capacity(order.len());
    for child in order.iter() {
        if let Some(TreeFact::Link(link)) = snapshot
            .__plain_observe::<F>(TreeKey::ChildLink(
                node.clone(),
                member,
                variant,
                field,
                child.clone(),
            ))
            .as_deref()
        {
            links.push(link.clone());
        }
    }
    Ok(ChildList::new(links.into()))
}

fn __observe<F: AbstractTreeFamily>(key: TreeKey<F::Domain>) -> Result<Option<Arc<TreeFact>>> {
    let context = super::plain::context_for("abstract_tree_read", F::name())?;
    context.observe::<F>(key, super::plain::Temporal::Current)
}

/// Publishes a generated render description under an already allocated
/// component output identity.
#[doc(hidden)]
pub fn __render_at<F: AbstractTreeFamily>(
    output: AstBox<()>,
    facts: Vec<(TreeKey<F::Domain>, TreeFact)>,
) -> Result<AstBox<()>> {
    let context = super::plain::context_for("abstract_tree_render", F::name())?;
    context.register::<F>()?;
    for (key, fact) in facts {
        context.emit::<F>(key, Some(fact))?;
    }
    Ok(output)
}

/// Publishes a generated render description and allocates its output identity.
#[doc(hidden)]
pub fn __render<F: AbstractTreeFamily>(
    facts: Vec<(TreeKey<F::Domain>, TreeFact)>,
) -> Result<AstBox<()>> {
    let output = super::plain::automatic_ast_box::<F>()?;
    __render_at::<F>(output, facts)
}

/// A read-only generated tree snapshot facade.
#[derive(Clone)]
pub struct SnapshotTree<F: AbstractTreeFamily> {
    snapshot: Snapshot,
    marker: PhantomData<fn() -> F>,
}
impl<F: AbstractTreeFamily> SnapshotTree<F> {
    pub(crate) fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            marker: PhantomData,
        }
    }

    pub fn roots(&self, domain: &F::Domain) -> impl Iterator<Item = AstBox<F::Root>> {
        let fact = self
            .snapshot
            .__plain_observe::<F>(TreeKey::RootOrder(domain.clone()));
        let roots = match fact.as_deref() {
            Some(TreeFact::RootOrder(order)) => order
                .iter()
                .filter_map(|link| {
                    match self
                        .snapshot
                        .__plain_observe::<F>(TreeKey::RootLink(domain.clone(), link.clone()))
                        .as_deref()
                    {
                        Some(TreeFact::RootLink(root)) => {
                            Some(AstBox::<F::Root>::from_erased(root.clone()))
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        roots.into_iter()
    }
    /// Enumerates domains with a committed root order.
    ///
    /// The encoded root keys stay behind the snapshot facade; callers receive
    /// only the semantic domain values needed to traverse each forest.
    pub fn domains(&self) -> impl Iterator<Item = F::Domain> {
        let mut domains = Vec::new();
        for input in self.snapshot.__plain_inputs::<F>() {
            if let TreeKey::RootOrder(domain) = input
                && !domains.iter().any(|existing| existing == &domain)
            {
                domains.push(domain);
            }
        }
        domains.into_iter()
    }

    pub fn materialize<T: TreeRender<Family = F>>(&self, node: AstBox<T>) -> Result<T> {
        T::__snapshot_materialize(&self.snapshot, node)
    }

    /// Reads one node's discriminant and returns its generated lazy view
    /// bound to this snapshot (no effect context, plan §4.3).
    pub fn view<T: AbstractTreeNode<Family = F>>(&self, node: AstBox<T>) -> Result<T::View> {
        T::__snapshot_view(&self.snapshot, node)
    }
}

impl Snapshot {
    /// Opens an effect-free reader for one generated abstract-tree family.
    pub fn tree<F: AbstractTreeFamily>(&self) -> SnapshotTree<F> {
        SnapshotTree::new(self.clone())
    }
}

/// A selector for all committed roots of one generated family.
///
/// `N` identifies the semantic root member selected by the caller. It
/// defaults to the family's declared root so standalone trees keep the short
/// `RootSelector<F>` spelling.
#[derive(Clone, Copy, Debug, Default)]
pub struct RootSelector<
    F: AbstractTreeFamily,
    N: AbstractTreeNode<Family = F> = <F as AbstractTreeFamily>::Root,
>(PhantomData<fn() -> (F, N)>);
impl<F, N> RootSelector<F, N>
where
    F: AbstractTreeFamily,
    N: AbstractTreeNode<Family = F>,
{
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

/// A selector for all committed nodes of one generated family.
///
/// The selected member type is part of the selector, which lets a
/// heterogeneous family mount the correct typed component without a raw
/// member/ordinal discriminator.
#[derive(Clone, Copy, Debug, Default)]
pub struct NodeSelector<
    F: AbstractTreeFamily,
    N: AbstractTreeNode<Family = F> = <F as AbstractTreeFamily>::Root,
>(PhantomData<fn() -> (F, N)>);
impl<F, N> NodeSelector<F, N>
where
    F: AbstractTreeFamily,
    N: AbstractTreeNode<Family = F>,
{
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

/// Mount-time root publication helper.  The selector itself carries no
/// runtime identity; roots are owned by the mounted component's output.
#[doc(hidden)]
pub fn __set_root<F: AbstractTreeFamily>(domain: F::Domain, root: AstBox<()>) -> Result<()> {
    let context = super::plain::context_for("abstract_tree_root", F::name())?;
    context.emit::<F>(TreeKey::Parent(root.clone()), Some(TreeFact::Parent(None)))?;
    context.emit::<F>(
        TreeKey::RootOrder(domain.clone()),
        Some(TreeFact::RootOrder(Arc::from([root.clone()]))),
    )?;
    context.emit::<F>(
        TreeKey::RootLink(domain, root.clone()),
        Some(TreeFact::RootLink(root)),
    )
}

/// Macro ABI: obtains the active component's stable output identity.
#[doc(hidden)]
pub fn __automatic_box<F: View>() -> Result<AstBox<()>> {
    super::plain::automatic_ast_box::<F>()
}

/// Reads back the published-syntax identity of a node for publication
/// bookkeeping. Returns `None` for non-parser identities.
impl AstBox<()> {
    pub(crate) fn identity_syntax(&self) -> Option<super::view::SyntaxNodeIdentity> {
        match self
            .identity
            .as_ref()
            .as_any()
            .downcast_ref::<super::view::SyntaxNodeIdentity>()
        {
            Some(identity) => Some(identity.clone()),
            None => None,
        }
    }
}

/// The parser maps one stable document lineage to exactly one published
/// node identity per generated family view. The member descriptor is the
/// collision-safe module-qualified grammar field name, never a list
/// ordinal (plan §5.5 / Cut B item 6).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PublishedSyntaxKey {
    pub(crate) view: std::any::TypeId,
    pub(crate) uri: Arc<str>,
    pub(crate) lineage: u64,
    pub(crate) member: &'static str,
    pub(crate) root: bool,
}

/// Mints the published identity of one parser lineage.
#[doc(hidden)]
pub fn __published_syntax_box<F: View>(
    uri: &str,
    lineage: u64,
    member: &'static str,
    root: bool,
) -> AstBox<()> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let identity = PublishedSyntaxKey {
        view: std::any::TypeId::of::<F>(),
        uri: Arc::from(uri),
        lineage,
        member,
        root,
    };
    let mut hasher = DefaultHasher::new();
    identity.hash(&mut hasher);
    AstBox::from_parts(hasher.finish(), Arc::new(identity))
}

/// Emits a batch of exact family facts through one replace-mode effect
/// context. Used by parser publication, which drives the generated family
/// view directly instead of through a kind-specific handle.
#[doc(hidden)]
pub fn __emit_facts<F: AbstractTreeFamily>(
    facts: impl IntoIterator<Item = (TreeKey<F::Domain>, TreeFact)>,
) -> Result<()> {
    let context = super::plain::context_for("abstract_tree_publish", F::name())?;
    context.register::<F>()?;
    let mut unique = std::collections::HashMap::new();
    for (key, fact) in facts {
        unique.insert(key, fact);
    }
    for (key, fact) in unique {
        context.emit::<F>(key, Some(fact))?;
    }
    Ok(())
}

/// Retracts one exact family fact (used by parser publication).
#[doc(hidden)]
pub fn __retract_fact<F: AbstractTreeFamily>(key: TreeKey<F::Domain>) -> Result<()> {
    let context = super::plain::context_for("abstract_tree_publish", F::name())?;
    context.register::<F>()?;
    context.emit::<F>(key, None)
}

/// The parser-independent decomposition adapter generated for one syntax
/// member.  The adapter only sees the authored enum value and generic child
/// identities; parser arena lookup stays in the framework.
#[doc(hidden)]
pub trait SyntaxPublication: AbstractTreeNode + Sized {
    /// The collision-safe member descriptor of this grammar member.
    fn __syntax_member() -> &'static str;

    /// Child record identities in field/list order.
    fn __syntax_child_records(value: &Self) -> Vec<u64>;

    /// Decomposes one authored value into exact member/kind/leaf/child facts.
    fn __syntax_facts(
        node: AstBox<()>,
        value: &Self,
        root: bool,
        project: &dyn Fn(u64) -> Option<AstBox<()>>,
        out: &mut Vec<(
            TreeKey<<<Self as AbstractTreeNode>::Family as AbstractTreeFamily>::Domain>,
            TreeFact,
        )>,
    ) -> Result<()>;
}

/// Family-level syntax publication adapter generated on the family root.
///
/// The parser passes an erased arena value into this interface.  Generated
/// code performs only `Any` downcasts; it never names or probes parser arena
/// APIs.  This keeps the general tree schema independent of the parser.
#[doc(hidden)]
pub trait SyntaxFamilyPublication: AbstractTreeNode + Sized {
    fn __domain_from_uri(
        uri: &str,
    ) -> <<Self as AbstractTreeNode>::Family as AbstractTreeFamily>::Domain;

    fn __syntax_member_of(value: &(dyn Any + Send + Sync)) -> Option<&'static str>;

    fn __syntax_member_kind(value: &(dyn Any + Send + Sync)) -> Option<u8>;

    fn __syntax_child_records(value: &(dyn Any + Send + Sync)) -> Vec<u64>;

    fn __syntax_publish(
        node: AstBox<()>,
        value: &(dyn Any + Send + Sync),
        root: bool,
        project: &dyn Fn(u64) -> Option<AstBox<()>>,
        out: &mut Vec<(
            TreeKey<<<Self as AbstractTreeNode>::Family as AbstractTreeFamily>::Domain>,
            TreeFact,
        )>,
    ) -> Result<bool>;
}
