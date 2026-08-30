//! The scope graph (plan §7): one framework graph view per domain.
//!
//! [`ScopeGraph<D>`] is a reactive Graph view whose nodes carry
//! [`ScopeNode<D>`] payloads (scopes, declarations, references) and whose
//! edges are labelled by `D::Label`. [`Scope<D>`] is an opaque typed identity
//! allocated by the reactive dispatcher.
//!
//! The API range (§7.2): emitters create scopes/declarations/references
//! and labelled edges; observers read nodes, declaration buckets,
//! reference buckets, outgoing edges, and resolve paths. Resolution reads
//! exactly the `bucket(s, l)` and `node(i)` facts it touches, so a
//! resolution re-runs only when one of those buckets changes.
//!
//! [`ScopeRequirements<D>`] is a separate Map view: cross-document source
//! requirements, read by consumers that must not depend on graph
//! topology.
//!
//! `PathExpr`/`ScopePath`/`PathOrder`/`ResolutionPath`/`partition_visible`
//! remain engine-free in this module; `scope_path!`/`lregex!` macros re-root
//! to the public framework path.

use std::fmt;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;

use crate::reactive::kind::{Graph, Map};
use crate::reactive::view::Node;
use crate::reactive::{Result, Snapshot};
use reactive_macros::view;
// Domain
// ---------------------------------------------------------------------------

/// Types owned by one independent scope-graph domain. The engine-free
/// contract: no graph-kernel bounds, just data.
pub trait ScopeDomain: Clone + Eq + Hash + Debug + Send + Sync + 'static {
    type ScopeData: Clone + Eq + Hash + Debug + Send + Sync + 'static;
    type Label: Clone + Eq + Hash + Debug + Send + Sync + 'static;
    type Request: Clone + Eq + Hash + Debug + Send + Sync + 'static;
}

#[derive(PartialEq, Eq, Hash)]
pub struct Scope<D: ScopeDomain>(Node<ScopeGraph<D>>);

impl<D: ScopeDomain> Clone for Scope<D> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<D: ScopeDomain> fmt::Debug for Scope<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Scope")
    }
}

impl<D: ScopeDomain> Scope<D> {
    #[doc(hidden)]
    pub(crate) fn from_graph_node(node: Node<ScopeGraph<D>>) -> Self {
        Self(node)
    }

    /// Allocates the graph identity owned by the active component instance.
    ///
    /// This is the semantic graph counterpart of `AstBox::render`: the
    /// definition and exact component input determine the identity.
    pub fn automatic() -> Result<Self> {
        let node = crate::reactive::plain::automatic_graph_node_id::<ScopeGraph<D>>()?;
        Ok(Self::from_graph_node(node))
    }

    /// Returns a complete graph-node publication for this scope.
    pub fn render(
        self,
        payload: ScopeNode<D>,
    ) -> crate::reactive::component::GraphRender<ScopeGraph<D>> {
        crate::reactive::component::GraphRender::from_node(self.node(), Some(payload))
    }

    /// Returns a bucket-only publication for this scope.
    pub fn patch(self) -> crate::reactive::component::GraphRender<ScopeGraph<D>> {
        crate::reactive::component::GraphRender::patch_node(self.node())
    }
}

impl<D: ScopeDomain> From<Scope<D>> for Node<ScopeGraph<D>> {
    fn from(scope: Scope<D>) -> Self {
        scope.0
    }
}

impl<D: ScopeDomain> Scope<D> {
    /// Derives a stable anchored scope identity from any hashable seed.
    ///
    /// The identity mixes the graph domain so equal seeds in different
    /// domains never collide; the hash is fixed-seed and therefore stable
    /// across warm/cold builds and worker counts (T3).
    #[doc(hidden)]
    pub fn anchored(seed: &impl std::hash::Hash) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::any::TypeId::of::<ScopeGraph<D>>().hash(&mut hasher);
        seed.hash(&mut hasher);
        let identity = hasher.finish();
        Self(Node::from_syntax(identity, "<scope>", identity, false))
    }
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// One node's payload inside a scope graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeNode<D: ScopeDomain> {
    Scope(D::ScopeData),
    Declaration(D::ScopeData),
    Reference(D::ScopeData),
}

/// Cross-document source requirements of one domain.
#[view]
pub struct ScopeRequirements<D: ScopeDomain>(Map<String, Vec<D::Request>>);

/// The scope graph of one domain: nodes = scopes, declarations, references,
/// and labelled edge buckets (one fact per node payload, one per bucket).
#[view]
pub struct ScopeGraph<D: ScopeDomain>(Graph<ScopeNode<D>, D::Label>);

impl<D: ScopeDomain> Scope<D> {
    /// The underlying graph identity.
    #[doc(hidden)]
    pub fn node(&self) -> Node<ScopeGraph<D>> {
        self.0.clone()
    }
}

// ---------------------------------------------------------------------------

/// Allocates and publishes one scope node.
pub fn scope<D: ScopeDomain>(data: D::ScopeData) -> Result<Scope<D>> {
    let graph = crate::reactive::kind::emit_view::<ScopeGraph<D>>()?;
    let node = graph.mint(ScopeNode::Scope(data))?;
    Ok(Scope::from_graph_node(node))
}

/// Publishes or replaces the payload of an existing scope.
pub fn ensure_scope<D: ScopeDomain>(id: Scope<D>, data: D::ScopeData) -> Result<()> {
    let graph = crate::reactive::kind::emit_view::<ScopeGraph<D>>()?;
    graph.set_node(id.node(), ScopeNode::Scope(data))
}

/// Publishes a declaration node and its labelled edge from `owner`.
pub fn declare<D: ScopeDomain>(
    owner: Scope<D>,
    name: D::Label,
    data: D::ScopeData,
) -> Result<Scope<D>> {
    let graph = crate::reactive::kind::emit_view::<ScopeGraph<D>>()?;
    let declaration = Scope::from_graph_node(graph.mint(ScopeNode::Declaration(data))?);
    graph.link(owner.node(), name, declaration.node())?;
    Ok(declaration)
}

/// Publishes a reference node, its name edge, and its target edge.
pub fn reference<D: ScopeDomain>(
    owner: Scope<D>,
    name: D::Label,
    data: D::ScopeData,
    target: Scope<D>,
) -> Result<()> {
    let graph = crate::reactive::kind::emit_view::<ScopeGraph<D>>()?;
    let reference = Scope::from_graph_node(graph.mint(ScopeNode::Reference(data))?);
    graph.link(owner.node(), name.clone(), reference.node())?;
    graph.link(reference.node(), name, target.node())
}

/// Publishes one labelled graph edge.
pub fn edge<D: ScopeDomain>(source: Scope<D>, label: D::Label, target: Scope<D>) -> Result<()> {
    let graph = crate::reactive::kind::emit_view::<ScopeGraph<D>>()?;
    graph.link(source.node(), label, target.node())
}
/// Retracts one scope node. Buckets referencing it keep their owners.
pub fn remove_scope<D: ScopeDomain>(id: Scope<D>) -> Result<()> {
    let graph = crate::reactive::kind::emit_view::<ScopeGraph<D>>()?;
    graph.remove_node(id.node())
}

/// Reads one scope payload.
pub fn observe_scope<D: ScopeDomain>(id: Scope<D>) -> Result<Option<Arc<D::ScopeData>>> {
    let observe = crate::reactive::kind::observe_view::<ScopeGraph<D>>()?;
    Ok(match observe.payload(id.node())?.as_deref() {
        Some(ScopeNode::Scope(data)) => Some(Arc::new(data.clone())),
        _ => None,
    })
}

/// Reads one node payload, including declarations and references.
pub fn observe_node<D: ScopeDomain>(id: Scope<D>) -> Result<Option<Arc<ScopeNode<D>>>> {
    let observe = crate::reactive::kind::observe_view::<ScopeGraph<D>>()?;
    observe.payload(id.node())
}

/// Reads all targets in one labelled edge bucket — exactly one fact read,
/// replacing the legacy full-domain scan (plan §5.3).
pub fn outgoing<D: ScopeDomain>(source: Scope<D>, label: &D::Label) -> Result<Vec<Scope<D>>> {
    let observe = crate::reactive::kind::observe_view::<ScopeGraph<D>>()?;
    Ok(observe
        .outgoing(source.node(), label)?
        .into_iter()
        .map(Scope::from_graph_node)
        .collect())
}

/// Reads declaration nodes reachable under one name.
pub fn declarations<D: ScopeDomain>(source: Scope<D>, label: &D::Label) -> Result<Vec<Scope<D>>> {
    let mut result: Vec<Scope<D>> = Vec::new();
    for target in outgoing(source, label)? {
        if matches!(
            observe_node(target.clone())?.as_deref(),
            Some(ScopeNode::Declaration(_))
        ) {
            result.push(target);
        }
    }
    Ok(result)
}

/// Reads declaration payloads reachable under one name.
pub fn resolve_name<D: ScopeDomain>(
    source: Scope<D>,
    label: &D::Label,
) -> Result<Vec<Arc<D::ScopeData>>> {
    let mut result: Vec<Arc<D::ScopeData>> = Vec::new();
    for target in declarations(source, label)? {
        if let Some(ScopeNode::Declaration(data)) = observe_node(target)?.as_deref() {
            result.push(Arc::new(data.clone()));
        }
    }
    Ok(result)
}

/// Resolves a regular path over the exact node and edge facts it touches.
pub fn resolve<D: ScopeDomain, F>(
    start: Scope<D>,
    path: ScopePath<D::Label>,
    accepts: F,
) -> Result<std::collections::HashSet<ResolutionPath<D>>>
where
    F: Fn(&ScopeNode<D>) -> bool,
{
    #[derive(Clone)]
    struct Search<D: ScopeDomain> {
        scope: Scope<D>,
        expression: PathExpr<D::Label>,
        scopes: Vec<Scope<D>>,
        labels: Vec<D::Label>,
        states: std::collections::HashSet<(Scope<D>, PathExpr<D::Label>)>,
    }

    let expression: PathExpr<D::Label> = path.into_path();
    let mut states = std::collections::HashSet::new();
    states.insert((start.clone(), expression.clone()));
    let mut pending = vec![Search {
        scope: start.clone(),
        expression,
        scopes: vec![start],
        labels: Vec::new(),
        states,
    }];
    let mut answers = std::collections::HashSet::new();

    while let Some(search) = pending.pop() {
        if search.expression.nullable()
            && let Some(node) = observe_node(search.scope.clone())?
            && accepts(&node)
            && let ScopeNode::Scope(data) = &*node
        {
            answers.insert(ResolutionPath {
                scopes: search.scopes.clone().into(),
                labels: search.labels.clone().into(),
                data: data.clone(),
            });
        }
        for label in search.expression.labels() {
            let residual = search.expression.derivative(&label);
            if residual == PathExpr::Empty {
                continue;
            }
            for target in outgoing(search.scope.clone(), &label)? {
                let state = (target.clone(), residual.clone());
                if search.states.contains(&state) {
                    continue;
                }
                let mut next = search.clone();
                next.scope = target.clone();
                next.expression = residual.clone();
                next.scopes.push(target);
                next.labels.push(label.clone());
                next.states.insert(state);
                pending.push(next);
            }
        }
    }
    Ok(answers)
}
/// Returns the committed scope identities in registration order.
pub fn snapshot_nodes<D: ScopeDomain>(snapshot: &Snapshot) -> Vec<Scope<D>> {
    snapshot
        .inputs::<ScopeGraph<D>>()
        .into_iter()
        .filter_map(|input| match input {
            crate::reactive::kind::GraphKey::Node(id) => Some(Scope::from_graph_node(id)),
            _ => None,
        })
        .collect()
}

/// Reads one committed scope-graph node.
pub fn snapshot_node<D: ScopeDomain>(
    snapshot: &Snapshot,
    node: Scope<D>,
) -> Option<Arc<ScopeNode<D>>> {
    snapshot.graph_node::<ScopeGraph<D>>(node.node())
}

/// Reads one committed scope payload.
pub fn snapshot_scope<D: ScopeDomain>(
    snapshot: &Snapshot,
    node: Scope<D>,
) -> Option<Arc<D::ScopeData>> {
    snapshot_node::<D>(snapshot, node).and_then(|payload| match payload.as_ref() {
        ScopeNode::Scope(data) => Some(Arc::new(data.clone())),
        ScopeNode::Declaration(_) | ScopeNode::Reference(_) => None,
    })
}

/// Reads the committed targets of one labelled edge bucket.
pub fn snapshot_outgoing<D: ScopeDomain>(
    snapshot: &Snapshot,
    source: Scope<D>,
    label: &D::Label,
) -> Vec<Scope<D>> {
    snapshot
        .outgoing::<ScopeGraph<D>>(source.node(), label)
        .into_iter()
        .map(Scope::from_graph_node)
        .collect()
}

/// Reads committed declaration nodes in one labelled edge bucket.
pub fn snapshot_declarations<D: ScopeDomain>(
    snapshot: &Snapshot,
    source: Scope<D>,
    label: &D::Label,
) -> Vec<Scope<D>> {
    snapshot_outgoing::<D>(snapshot, source, label)
        .into_iter()
        .filter(|node| {
            matches!(
                snapshot_node::<D>(snapshot, node.clone()).as_deref(),
                Some(ScopeNode::Declaration(_))
            )
        })
        .collect()
}
// ---------------------------------------------------------------------------

/// A regular path language used by scope-graph resolution.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PathExpr<Label> {
    /// A dead path: no continuation is possible.
    Empty,
    /// Accept at the current scope without traversing an edge.
    Epsilon,
    Label(Label),
    Or(Arc<PathExpr<Label>>, Arc<PathExpr<Label>>),
    Then(Arc<PathExpr<Label>>, Arc<PathExpr<Label>>),
    Star(Arc<PathExpr<Label>>),
}

impl<Label> PathExpr<Label>
where
    Label: Clone + Eq,
{
    pub fn label(label: Label) -> Self {
        Self::Label(label)
    }

    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, right) => right,
            (left, Self::Empty) => left,
            (left, right) if left == right => left,
            (left, right) => Self::Or(Arc::new(left), Arc::new(right)),
        }
    }

    pub fn then(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, _) | (_, Self::Empty) => Self::Empty,
            (Self::Epsilon, right) => right,
            (left, Self::Epsilon) => left,
            (left, right) => Self::Then(Arc::new(left), Arc::new(right)),
        }
    }

    pub fn star(self) -> Self {
        match self {
            Self::Empty | Self::Epsilon => Self::Epsilon,
            Self::Star(inner) => Self::Star(inner),
            expression => Self::Star(Arc::new(expression)),
        }
    }

    pub fn zero_or_more(label: Label) -> Self {
        Self::label(label).star()
    }

    pub fn nullable(&self) -> bool {
        match self {
            Self::Empty | Self::Label(_) => false,
            Self::Epsilon | Self::Star(_) => true,
            Self::Or(left, right) => left.nullable() || right.nullable(),
            Self::Then(left, right) => left.nullable() && right.nullable(),
        }
    }

    pub fn derivative(&self, label: &Label) -> Self {
        match self {
            Self::Empty | Self::Epsilon => Self::Empty,
            Self::Label(expected) => {
                if expected == label {
                    Self::Epsilon
                } else {
                    Self::Empty
                }
            }
            Self::Or(left, right) => left.derivative(label).or(right.derivative(label)),
            Self::Then(left, right) => {
                let first = left.derivative(label).then((**right).clone());
                if left.nullable() {
                    first.or(right.derivative(label))
                } else {
                    first
                }
            }
            Self::Star(inner) => inner.derivative(label).then(Self::Star(Arc::clone(inner))),
        }
    }

    /// The distinct labels named in this expression, in first-appearance
    /// order. The resolution walk reads exactly one `bucket(s, l)` per
    /// label here per reached state.
    pub fn labels(&self) -> Vec<Label> {
        let mut out: Vec<Label> = Vec::new();
        fn walk<L: Clone + Eq>(expr: &PathExpr<L>, out: &mut Vec<L>) {
            match expr {
                PathExpr::Empty | PathExpr::Epsilon => {}
                PathExpr::Label(l) => {
                    if !out.contains(l) {
                        out.push(l.clone());
                    }
                }
                PathExpr::Or(a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                PathExpr::Then(a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                PathExpr::Star(inner) => walk(inner, out),
            }
        }
        walk(self, &mut out);
        out
    }
}

/// A typed path pattern interpreted from an explicit starting scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopePath<Label>(PathExpr<Label>);

impl<Label> ScopePath<Label>
where
    Label: Clone + Eq,
{
    pub fn nullable(&self) -> bool {
        self.0.nullable()
    }

    pub fn derivative(&self, label: &Label) -> PathExpr<Label> {
        self.0.derivative(label)
    }

    pub fn into_path(self) -> PathExpr<Label> {
        self.0
    }
}

impl<Label> From<PathExpr<Label>> for ScopePath<Label> {
    fn from(expression: PathExpr<Label>) -> Self {
        Self(expression)
    }
}

/// A strict partial order over edge labels used to select visible paths.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PathOrder<Label> {
    preferred: Arc<[(Label, Label)]>,
}

impl<Label> Default for PathOrder<Label> {
    fn default() -> Self {
        Self {
            preferred: Arc::from([]),
        }
    }
}

impl<Label> PathOrder<Label>
where
    Label: Clone + Eq,
{
    pub fn new() -> Self {
        Self::default()
    }

    pub fn prefer(mut self, preferred: Label, less_preferred: Label) -> Self {
        let mut relations = self.preferred.to_vec();
        if preferred != less_preferred
            && !relations
                .iter()
                .any(|(left, right)| left == &preferred && right == &less_preferred)
        {
            relations.push((preferred, less_preferred));
        }
        let mut changed = true;
        while changed {
            changed = false;
            let snapshot = relations.clone();
            for (left, middle) in &snapshot {
                for (candidate, right) in &snapshot {
                    if middle == candidate
                        && left != right
                        && !relations.iter().any(|(l, r)| l == left && r == right)
                    {
                        relations.push((left.clone(), right.clone()));
                        changed = true;
                    }
                }
            }
        }
        self.preferred = relations.into();
        self
    }

    pub fn compare<Scope>(
        &self,
        left: &ResolutionPath<Scope>,
        right: &ResolutionPath<Scope>,
    ) -> Option<std::cmp::Ordering>
    where
        Scope: ScopeDomain<Label = Label>,
    {
        for (left_label, right_label) in left.labels.iter().zip(right.labels.iter()) {
            if left_label == right_label {
                continue;
            }
            if self
                .preferred
                .iter()
                .any(|(preferred, less)| preferred == left_label && less == right_label)
            {
                return Some(std::cmp::Ordering::Greater);
            }
            if self
                .preferred
                .iter()
                .any(|(preferred, less)| preferred == right_label && less == left_label)
            {
                return Some(std::cmp::Ordering::Less);
            }
            return None;
        }
        Some(left.labels.len().cmp(&right.labels.len()).reverse())
    }
}

/// One dependency-tracked resolution witness.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ResolutionPath<D: ScopeDomain> {
    pub scopes: Arc<[Scope<D>]>,
    pub labels: Arc<[D::Label]>,
    pub data: D::ScopeData,
}

impl<D: ScopeDomain> ResolutionPath<D> {
    pub fn data(&self) -> &D::ScopeData {
        &self.data
    }

    pub fn into_data(self) -> D::ScopeData {
        self.data
    }

    pub fn target_scope(&self) -> Scope<D> {
        self.scopes[self.scopes.len() - 1].clone()
    }
}

impl<D: ScopeDomain> fmt::Debug for ResolutionPath<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolutionPath")
            .field("scopes", &self.scopes)
            .field("labels", &"..")
            .field("data", &"..")
            .finish()
    }
}
/// Partitions resolved paths into visible vs shadowed witnesses under a
/// [`PathOrder`].
pub fn partition_visible<D: ScopeDomain>(
    paths: std::collections::HashSet<ResolutionPath<D>>,
    order: &PathOrder<D::Label>,
) -> (
    Vec<ResolutionPath<D>>,
    Vec<(ResolutionPath<D>, Vec<ResolutionPath<D>>)>,
) {
    let paths = paths.into_iter().collect::<Vec<_>>();
    let mut visible = Vec::new();
    let mut dominated = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let dominated_by_any = paths.iter().enumerate().any(|(other_index, other)| {
            other_index != index && order.compare(other, path) == Some(std::cmp::Ordering::Greater)
        });
        if dominated_by_any {
            dominated.push(path.clone());
        } else {
            visible.push(path.clone());
        }
    }

    let shadowed = dominated
        .into_iter()
        .map(|path| {
            let visible_by = visible
                .iter()
                .filter(|candidate| {
                    order.compare(candidate, &path) == Some(std::cmp::Ordering::Greater)
                })
                .cloned()
                .collect::<Vec<_>>();
            (path, visible_by)
        })
        .collect();
    (visible, shadowed)
}
