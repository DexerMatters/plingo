//! The scope graph (plan §7): one framework graph view per domain.
//!
//! [`ScopeGraph<D>`] is a reactive Graph view whose nodes carry
//! [`ScopeNode<D>`] payloads (scopes, declarations, references) and whose
//! edges are labelled by `D::Label`. [`ScopeId<D>`] is a newtype over
//! [`NodeId`]; allocation is `fresh_node_id()`.
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
//! move here engine-free from `component::scope::query`; the
//! `scope_path!`/`lregex!` macros re-root to this module.

use std::fmt;
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::reactive::api::{GraphObservedExt, GraphPreviousExt};
use crate::reactive::prelude::*;
use crate::reactive_view as view;

// ---------------------------------------------------------------------------
// Domain
// ---------------------------------------------------------------------------

/// Types owned by one independent scope-graph domain. The engine-free
/// contract: no graph-kernel bounds, just data.
pub trait ScopeDomain: Clone + Eq + Hash + Debug + Send + Sync + 'static {
    type ScopeKey: Clone + Eq + Hash + Debug + Send + Sync + 'static;
    type ScopeData: Clone + Eq + Hash + Debug + Send + Sync + 'static;
    type Label: Clone + Eq + Hash + Debug + Send + Sync + 'static;
    type Request: Clone + Eq + Hash + Debug + Send + Sync + 'static;
}

/// A stable identity for one semantic scope: a newtype over the reactive
/// [`NodeId`].
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeId<D: ScopeDomain>(NodeId, PhantomData<fn() -> D>);

impl<D: ScopeDomain> Copy for ScopeId<D> {}

impl<D: ScopeDomain> Clone for ScopeId<D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D: ScopeDomain> ScopeId<D> {
    /// Wraps a reactive node identity.
    pub const fn new(node: NodeId) -> Self {
        Self(node, PhantomData)
    }

    /// The underlying reactive node identity.
    pub const fn node(self) -> NodeId {
        self.0
    }
}

impl<D: ScopeDomain> fmt::Debug for ScopeId<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ScopeId").field(&self.0).finish()
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

/// The scope graph of one domain: nodes = scopes/declarations/references,
/// edges labelled by `D::Label`. Multi-producer by construction (§7.3):
/// distinct passes write disjoint payloads into one graph.
#[view(graph, value = ScopeNode<D>, edge = (), label = D::Label)]
pub struct ScopeGraph<D: ScopeDomain>(PhantomData<D>);

/// Cross-document source requirements of one domain (plan §7.3): a Map so
/// the workspace can decide what to load without depending on topology.
#[view(map, key = String, value = Vec<D::Request>)]
pub struct ScopeRequirements<D: ScopeDomain>(PhantomData<D>);

// ---------------------------------------------------------------------------
// Path data (ported engine-free from `component::scope::query`)
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
    pub scopes: Arc<[ScopeId<D>]>,
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

    /// The target scope of this witness (the last scope on the path).
    pub fn target_scope(&self) -> ScopeId<D> {
        self.scopes[self.scopes.len() - 1]
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
/// [`PathOrder`] (ported unchanged from `component::scope::query`).
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

// ---------------------------------------------------------------------------
// Emission surface (§7.2)
// ---------------------------------------------------------------------------

/// Emitted-handle surface for a scope graph.
pub trait ScopeGraphEmittedExt<D: ScopeDomain> {
    /// Allocates a fresh scope node (deterministic `fresh_node_id`).
    fn new_scope(&self) -> Result<ScopeId<D>>;
    /// Ensures a scope node exists with `data` (upsert).
    fn ensure_scope(&self, id: ScopeId<D>, data: D::ScopeData) -> Result<()>;
    /// Declares `name` in `scope`: a `Declaration` node with a `name` edge
    /// from the scope. Returns the declaration's id.
    fn declare(
        &self,
        scope: ScopeId<D>,
        name: D::Label,
        decl: D::ScopeData,
    ) -> Result<ScopeId<D>>;
    /// Adds a reference: a `Reference` node under `name`, linked to the
    /// target scope by an `edge`.
    fn reference(
        &self,
        from: ScopeId<D>,
        name: D::Label,
        reference: D::ScopeData,
        target: ScopeId<D>,
    ) -> Result<()>;
    /// Inserts one labelled edge between two scope nodes.
    fn edge(&self, source: ScopeId<D>, label: D::Label, target: ScopeId<D>) -> Result<()>;
    /// Removes a scope node.
    fn remove_scope(&self, id: ScopeId<D>) -> Result<()>;
}

impl<D: ScopeDomain> ScopeGraphEmittedExt<D> for EmittedHandle<ScopeGraph<D>> {
    fn new_scope(&self) -> Result<ScopeId<D>> {
        // The caller ensures the node; `new_scope` only mints the identity
        // (the scope payload arrives via `ensure_scope`).
        Ok(ScopeId::<D>::new(self.fresh_node_id()?))
    }

    fn ensure_scope(&self, id: ScopeId<D>, data: D::ScopeData) -> Result<()> {
        self.upsert_node(id.node(), ScopeNode::Scope(data))
    }

    fn declare(
        &self,
        scope: ScopeId<D>,
        name: D::Label,
        decl: D::ScopeData,
    ) -> Result<ScopeId<D>> {
        let id = ScopeId::<D>::new(self.fresh_node_id()?);
        self.insert_node(id.node(), ScopeNode::Declaration(decl))?;
        self.insert_edge(scope.node(), name, id.node(), ())?;
        Ok(id)
    }

    fn reference(
        &self,
        from: ScopeId<D>,
        name: D::Label,
        reference: D::ScopeData,
        target: ScopeId<D>,
    ) -> Result<()> {
        let id = ScopeId::<D>::new(self.fresh_node_id()?);
        self.insert_node(id.node(), ScopeNode::Reference(reference))?;
        self.insert_edge(from.node(), name.clone(), id.node(), ())?;
        self.insert_edge(id.node(), name, target.node(), ())?;
        Ok(())
    }

    fn edge(&self, source: ScopeId<D>, label: D::Label, target: ScopeId<D>) -> Result<()> {
        self.insert_edge(source.node(), label, target.node(), ())
    }

    fn remove_scope(&self, id: ScopeId<D>) -> Result<()> {
        self.remove_node(id.node())
    }
}

// ---------------------------------------------------------------------------
// Observed surface (§7.2)
// ---------------------------------------------------------------------------

/// Observed-handle surface for a scope graph.
pub trait ScopeGraphObservedExt<D: ScopeDomain> {
    /// Reads one scope node's data.
    fn scope(&self, id: ScopeId<D>) -> Result<Option<Arc<D::ScopeData>>>;
    /// The declaration ids reachable from `scope` under `name`.
    fn declarations(&self, scope: ScopeId<D>, name: &D::Label) -> Result<Vec<ScopeId<D>>>;
    /// Resolves `name` from `scope`: the declarations reachable directly
    /// by one `name` edge.
    fn resolve_name(
        &self,
        scope: ScopeId<D>,
        name: &D::Label,
    ) -> Result<Vec<Arc<D::ScopeData>>>;
    /// The targets of one labelled edge bucket.
    fn outgoing(&self, scope: ScopeId<D>, label: &D::Label) -> Result<Vec<ScopeId<D>>>;
    /// Resolves `path` from `start`, reading exactly the touched
    /// `bucket(s, l)` and `node(i)` facts.
    fn resolve(
        &self,
        start: ScopeId<D>,
        path: ScopePath<D::Label>,
        accepts: impl Fn(&ScopeNode<D>) -> bool,
    ) -> Result<std::collections::HashSet<ResolutionPath<D>>>;
    /// One child visitor per edge in `(scope, label)`.
    fn visit_outgoing_each<F, E>(&self, scope: ScopeId<D>, label: D::Label, f: F) -> Result<()>
    where
        F: FnMut(ScopeId<D>, Option<Arc<D::ScopeData>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static;
}

impl<D: ScopeDomain> ScopeGraphObservedExt<D> for ObservedHandle<ScopeGraph<D>> {
    fn scope(&self, id: ScopeId<D>) -> Result<Option<Arc<D::ScopeData>>> {
        let payload = self.node(id.node())?;
        match payload {
            Some(payload) => match &*payload {
                ScopeNode::Scope(data) => Ok(Some(Arc::new(data.clone()))),
                _ => Ok(None),
            },
            None => Ok(None),
        }
    }

    fn declarations(&self, scope: ScopeId<D>, name: &D::Label) -> Result<Vec<ScopeId<D>>> {
        let mut out = Vec::new();
        for edge in GraphObservedExt::outgoing(self, scope.node(), name)? {
            let payload = self.node(edge.target)?;
            if matches!(
                payload.as_deref(),
                Some(ScopeNode::Declaration(_))
            ) {
                out.push(ScopeId::new(edge.target));
            }
        }
        Ok(out)
    }

    fn resolve_name(
        &self,
        scope: ScopeId<D>,
        name: &D::Label,
    ) -> Result<Vec<Arc<D::ScopeData>>> {
        let mut out = Vec::new();
        for edge in GraphObservedExt::outgoing(self, scope.node(), name)? {
            let payload = self.node(edge.target)?;
            if let Some(ScopeNode::Declaration(data)) = payload.as_deref() {
                out.push(Arc::new(data.clone()));
            }
        }
        Ok(out)
    }

    fn outgoing(&self, scope: ScopeId<D>, label: &D::Label) -> Result<Vec<ScopeId<D>>> {
        Ok(GraphObservedExt::outgoing(self, scope.node(), label)?
            .into_iter()
            .map(|edge| ScopeId::new(edge.target))
            .collect())
    }

    fn resolve(
        &self,
        start: ScopeId<D>,
        path: ScopePath<D::Label>,
        accepts: impl Fn(&ScopeNode<D>) -> bool,
    ) -> Result<std::collections::HashSet<ResolutionPath<D>>> {
        resolve_walk(self, start, path.into_path(), accepts)
    }

    fn visit_outgoing_each<F, E>(&self, scope: ScopeId<D>, label: D::Label, mut f: F) -> Result<()>
    where
        F: FnMut(ScopeId<D>, Option<Arc<D::ScopeData>>) -> Result<(), E> + Send + Sync + 'static,
        E: Into<Error> + 'static,
    {
        let handle = ::std::clone::Clone::clone(self);
        GraphObservedExt::visit_outgoing_each(
            self,
            scope.node(),
            label,
            move |edge, _| -> Result<(), Error> {
                let data = handle.scope(ScopeId::new(edge.target))?;
                f(ScopeId::new(edge.target), data).map_err(::std::convert::Into::into)?;
                Ok(())
            },
        )
    }
}

/// The bucket-at-a-time resolution walk (ported `resolve_indexed`):
/// per reached state, reads exactly one `bucket(s, l)` per label named in
/// the residual expression, and one `node(i)` per reached node.
fn resolve_walk<D: ScopeDomain, F>(
    graph: &ObservedHandle<ScopeGraph<D>>,
    start: ScopeId<D>,
    path: PathExpr<D::Label>,
    accepts: F,
) -> Result<std::collections::HashSet<ResolutionPath<D>>>
where
    F: Fn(&ScopeNode<D>) -> bool,
{
    #[derive(Clone)]
    struct Search<D: ScopeDomain> {
        scope: ScopeId<D>,
        expression: PathExpr<D::Label>,
        scopes: Vec<ScopeId<D>>,
        labels: Vec<D::Label>,
        states: std::collections::HashSet<(ScopeId<D>, PathExpr<D::Label>)>,
    }

    let initial = (start, path.clone());
    let mut initial_states = std::collections::HashSet::new();
    initial_states.insert(initial);
    let mut pending = vec![Search {
        scope: start,
        expression: path,
        scopes: vec![start],
        labels: Vec::new(),
        states: initial_states,
    }];
    let mut answers = std::collections::HashSet::new();

    while let Some(search) = pending.pop() {
        let nullable = search.expression.nullable();
        // `node(i)` read for the reached scope when the path may accept
        // here.
        let data = if nullable {
            match graph.node(search.scope.node())? {
                Some(payload) => match &*payload {
                    ScopeNode::Scope(data) => Some(data.clone()),
                    _ => None,
                },
                None => None,
            }
        } else {
            None
        };
        if nullable
            && let Some(data) = &data
            && accepts(&ScopeNode::Scope(data.clone()))
        {
            answers.insert(ResolutionPath {
                scopes: search.scopes.clone().into(),
                labels: search.labels.clone().into(),
                data: data.clone(),
            });
        }

        // One `bucket(s, l)` read per label the residual can consume.
        for label in search.expression.labels() {
            let targets = GraphObservedExt::outgoing(graph, search.scope.node(), &label)?;
            for edge in targets {
                let residual = search.expression.derivative(&label);
                if residual == PathExpr::Empty {
                    continue;
                }
                let state = (ScopeId::new(edge.target), residual.clone());
                if search.states.contains(&state) {
                    continue;
                }
                let mut next = search.clone();
                next.scope = ScopeId::new(edge.target);
                next.expression = residual;
                next.scopes.push(ScopeId::new(edge.target));
                next.labels.push(label.clone());
                next.states.insert(state);
                pending.push(next);
            }
        }
    }
    Ok(answers)
}

// ---------------------------------------------------------------------------
// Previous surface (minimal committed reads)
// ---------------------------------------------------------------------------

/// Previous-handle surface: committed scope-graph reads.
pub trait ScopeGraphPreviousExt<D: ScopeDomain> {
    fn scope(&self, id: ScopeId<D>) -> Result<Option<Arc<D::ScopeData>>>;
    fn outgoing(&self, scope: ScopeId<D>, label: &D::Label) -> Result<Vec<ScopeId<D>>>;
}

impl<D: ScopeDomain> ScopeGraphPreviousExt<D> for PreviousHandle<ScopeGraph<D>> {
    fn scope(&self, id: ScopeId<D>) -> Result<Option<Arc<D::ScopeData>>> {
        match GraphPreviousExt::node(self, id.node())? {
            Some(payload) => match &*payload {
                ScopeNode::Scope(data) => Ok(Some(Arc::new(data.clone()))),
                _ => Ok(None),
            },
            None => Ok(None),
        }
    }

    fn outgoing(&self, scope: ScopeId<D>, label: &D::Label) -> Result<Vec<ScopeId<D>>> {
        Ok(GraphPreviousExt::outgoing(self, scope.node(), label)?
            .into_iter()
            .map(|edge| ScopeId::new(edge.target))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Snapshot surface
// ---------------------------------------------------------------------------

/// Committed-state reads for a scope graph.
pub struct ScopeGraphSnapshot<'a, D: ScopeDomain> {
    graph: &'a crate::reactive::engine::SnapshotGraph<ScopeGraph<D>>,
    _marker: PhantomData<D>,
}

impl<'a, D: ScopeDomain> ScopeGraphSnapshot<'a, D> {
    pub fn new(graph: &'a crate::reactive::engine::SnapshotGraph<ScopeGraph<D>>) -> Self {
        Self {
            graph,
            _marker: PhantomData,
        }
    }

    pub fn scope(&self, id: ScopeId<D>) -> Option<Arc<D::ScopeData>> {
        match self.graph.node(id.node())? {
            payload => match &*payload {
                ScopeNode::Scope(data) => Some(Arc::new(data.clone())),
                _ => None,
            },
        }
    }

    /// Reads any node's data payload (Scope, Declaration, or Reference).
    pub fn node_data(&self, id: ScopeId<D>) -> Option<Arc<D::ScopeData>> {
        self.graph
            .node(id.node())
            .map(|payload| match &*payload {
                ScopeNode::Scope(data) => Arc::new(data.clone()),
                ScopeNode::Declaration(data) => Arc::new(data.clone()),
                ScopeNode::Reference(data) => Arc::new(data.clone()),
            })
    }

    /// The committed node registry (ordered).
    pub fn node_ids(&self) -> Vec<ScopeId<D>> {
        self.graph
            .nodes()
            .into_iter()
            .map(ScopeId::new)
            .collect()
    }

    pub fn outgoing(&self, scope: ScopeId<D>, label: &D::Label) -> Vec<ScopeId<D>> {
        self.graph
            .outgoing(scope.node(), label)
            .into_iter()
            .map(|edge| ScopeId::new(edge.target))
            .collect()
    }

    pub fn declarations(&self, scope: ScopeId<D>, name: &D::Label) -> Vec<ScopeId<D>> {
        self.graph
            .outgoing(scope.node(), name)
            .into_iter()
            .filter(|edge| {
                matches!(
                    self.graph.node(edge.target).as_deref(),
                    Some(ScopeNode::Declaration(_))
                )
            })
            .map(|edge| ScopeId::new(edge.target))
            .collect()
    }
}
