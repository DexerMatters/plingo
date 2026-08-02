use std::{collections::HashSet, hash::Hash, sync::Arc};

use crate::{
    component::{
        api::{Component, Context, Error},
        structural::{StructureEdges, StructureNode},
    },
    scheme::node::ReadGraph,
};

use super::{ScopeDomain, ScopeEdge, ScopeId, ScopeStructure};

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

/// One dependency-tracked resolution witness returned by a scope query.
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
}

impl<D: ScopeDomain> std::fmt::Debug for ResolutionPath<D> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolutionPath")
            .field("scopes", &self.scopes)
            .field("labels", &"..")
            .field("data", &"..")
            .finish()
    }
}

/// Resolves a materialized query while observing only reached edge buckets and data.
pub(crate) fn resolve_indexed<D, Accepts, Lookup>(
    start: ScopeId<D>,
    path: PathExpr<D::Label>,
    accepts: Accepts,
    mut lookup: Lookup,
) -> HashSet<ResolutionPath<D>>
where
    D: ScopeDomain,
    Accepts: Fn(&D::ScopeData) -> bool,
    Lookup: FnMut(ScopeId<D>, bool) -> (Vec<ScopeEdge<D>>, Option<D::ScopeData>),
{
    #[derive(Clone)]
    struct Search<D: ScopeDomain> {
        scope: ScopeId<D>,
        expression: PathExpr<D::Label>,
        scopes: Vec<ScopeId<D>>,
        labels: Vec<D::Label>,
        states: HashSet<(ScopeId<D>, PathExpr<D::Label>)>,
    }

    let initial = (start, path.clone());
    let mut initial_states = HashSet::new();
    initial_states.insert(initial);
    let mut pending = vec![Search {
        scope: start,
        expression: path,
        scopes: vec![start],
        labels: Vec::new(),
        states: initial_states,
    }];
    let mut answers = HashSet::new();

    while let Some(search) = pending.pop() {
        let nullable = search.expression.nullable();
        let (edges, data) = lookup(search.scope, nullable);
        if nullable
            && let Some(data) = data
            && accepts(&data)
        {
            answers.insert(ResolutionPath {
                scopes: search.scopes.clone().into(),
                labels: search.labels.clone().into(),
                data,
            });
        }

        for edge in edges {
            let residual = search.expression.derivative(&edge.label);
            if residual == PathExpr::Empty {
                continue;
            }
            let state = (edge.target, residual.clone());
            if search.states.contains(&state) {
                continue;
            }
            let mut next = search.clone();
            next.scope = edge.target;
            next.expression = residual;
            next.scopes.push(edge.target);
            next.labels.push(edge.label);
            next.states.insert(state);
            pending.push(next);
        }
    }
    answers
}

/// A query assembled from an explicit scope and a component context.
pub struct ScopeQuery<'cx, 'tx, C: Component, D: ScopeDomain> {
    pub(crate) cx: &'cx mut Context<'tx, C>,
    pub(crate) start: ScopeId<D>,
}

impl<'cx, 'tx, C: Component, D: ScopeDomain> ScopeQuery<'cx, 'tx, C, D> {
    pub fn along(self, path: ScopePath<D::Label>) -> ScopePathQuery<'cx, 'tx, C, D> {
        ScopePathQuery {
            cx: self.cx,
            start: self.start,
            path,
        }
    }
}

/// A scope query with its start scope and path selected.
pub struct ScopePathQuery<'cx, 'tx, C: Component, D: ScopeDomain> {
    cx: &'cx mut Context<'tx, C>,
    start: ScopeId<D>,
    path: ScopePath<D::Label>,
}

impl<'cx, 'tx, C: Component, D: ScopeDomain> ScopePathQuery<'cx, 'tx, C, D> {
    pub fn all(self) -> HashSet<ResolutionPath<D>> {
        let Self { cx, start, path } = self;
        resolve_paths(cx, start, path, |_| true)
    }

    pub fn filter<F>(self, filter: F) -> FilteredScopeQuery<'cx, 'tx, C, D, F>
    where
        F: Fn(&D::ScopeData) -> bool,
    {
        FilteredScopeQuery {
            cx: self.cx,
            start: self.start,
            path: self.path,
            filter,
        }
    }
}

/// A path query whose reached scope data is filtered.
pub struct FilteredScopeQuery<'cx, 'tx, C: Component, D: ScopeDomain, F> {
    cx: &'cx mut Context<'tx, C>,
    start: ScopeId<D>,
    path: ScopePath<D::Label>,
    filter: F,
}

impl<'cx, 'tx, C: Component, D: ScopeDomain, F> FilteredScopeQuery<'cx, 'tx, C, D, F>
where
    F: Fn(&D::ScopeData) -> bool,
{
    pub fn all(self) -> HashSet<ResolutionPath<D>> {
        let Self {
            cx,
            start,
            path,
            filter,
        } = self;
        resolve_paths(cx, start, path, filter)
    }

    pub fn visible_under(self, order: PathOrder<D::Label>) -> OrderedScopeQuery<'cx, 'tx, C, D, F> {
        OrderedScopeQuery {
            cx: self.cx,
            start: self.start,
            path: self.path,
            filter: self.filter,
            order,
        }
    }

    pub fn with_context<Ctx>(
        self,
        context: Ctx,
    ) -> ScopeResolution<'cx, 'tx, C, D, F, Ctx, Unset, Unset, Unset, Unset> {
        ScopeResolution {
            cx: self.cx,
            start: self.start,
            path: self.path,
            filter: self.filter,
            order: None,
            context,
            shadowed: None,
            missing: None,
            unique: None,
            ambiguous: None,
        }
    }
}

/// A filtered query with an explicit partial order for visible paths.
pub struct OrderedScopeQuery<'cx, 'tx, C: Component, D: ScopeDomain, F> {
    cx: &'cx mut Context<'tx, C>,
    start: ScopeId<D>,
    path: ScopePath<D::Label>,
    filter: F,
    order: PathOrder<D::Label>,
}

impl<'cx, 'tx, C: Component, D: ScopeDomain, F> OrderedScopeQuery<'cx, 'tx, C, D, F>
where
    F: Fn(&D::ScopeData) -> bool,
{
    pub fn with_context<Ctx>(
        self,
        context: Ctx,
    ) -> ScopeResolution<'cx, 'tx, C, D, F, Ctx, Unset, Unset, Unset, Unset> {
        ScopeResolution {
            cx: self.cx,
            start: self.start,
            path: self.path,
            filter: self.filter,
            order: Some(self.order),
            context,
            shadowed: None,
            missing: None,
            unique: None,
            ambiguous: None,
        }
    }
}

/// Marker for a response that has not been installed yet.
#[doc(hidden)]
pub struct Unset;

/// A filtered query with explicit visibility and cardinality responses.
pub struct ScopeResolution<'cx, 'tx, C: Component, D: ScopeDomain, F, Ctx, S, M, U, A> {
    cx: &'cx mut Context<'tx, C>,
    start: ScopeId<D>,
    path: ScopePath<D::Label>,
    filter: F,
    order: Option<PathOrder<D::Label>>,
    context: Ctx,
    shadowed: Option<S>,
    missing: Option<M>,
    unique: Option<U>,
    ambiguous: Option<A>,
}

/// Runs one optional shadowing response for every dominated witness.
pub trait ShadowResponse<C: Component, D: ScopeDomain, Ctx> {
    fn run(
        &mut self,
        cx: &mut Context<'_, C>,
        context: &mut Ctx,
        shadowed: ResolutionPath<D>,
        visible_by: &[ResolutionPath<D>],
    ) -> Result<(), Error>;
}

impl<C: Component, D: ScopeDomain, Ctx> ShadowResponse<C, D, Ctx> for Unset {
    fn run(
        &mut self,
        _cx: &mut Context<'_, C>,
        _context: &mut Ctx,
        _shadowed: ResolutionPath<D>,
        _visible_by: &[ResolutionPath<D>],
    ) -> Result<(), Error> {
        Ok(())
    }
}

impl<C, D, Ctx, F> ShadowResponse<C, D, Ctx> for F
where
    C: Component,
    D: ScopeDomain,
    F: for<'a, 'b, 'c> FnMut(
        &'a mut Context<'b, C>,
        &'c mut Ctx,
        ResolutionPath<D>,
        &'c [ResolutionPath<D>],
    ) -> Result<(), Error>,
{
    fn run(
        &mut self,
        cx: &mut Context<'_, C>,
        context: &mut Ctx,
        shadowed: ResolutionPath<D>,
        visible_by: &[ResolutionPath<D>],
    ) -> Result<(), Error> {
        self(cx, context, shadowed, visible_by)
    }
}

impl<'cx, 'tx, C: Component, D: ScopeDomain, F, Ctx, S, M, U, A>
    ScopeResolution<'cx, 'tx, C, D, F, Ctx, S, M, U, A>
{
    pub fn on_shadowed<N>(self, handler: N) -> ScopeResolution<'cx, 'tx, C, D, F, Ctx, N, M, U, A>
    where
        N: for<'a, 'b, 'c> FnMut(
                &'a mut Context<'b, C>,
                &'c mut Ctx,
                ResolutionPath<D>,
                &'c [ResolutionPath<D>],
            ) -> Result<(), Error>
            + ShadowResponse<C, D, Ctx>,
    {
        let Self {
            cx,
            start,
            path,
            filter,
            order,
            context,
            missing,
            unique,
            ambiguous,
            shadowed: _,
        } = self;
        ScopeResolution {
            cx,
            start,
            path,
            filter,
            order,
            context,
            shadowed: Some(handler),
            missing,
            unique,
            ambiguous,
        }
    }

    pub fn on_missing<N>(self, handler: N) -> ScopeResolution<'cx, 'tx, C, D, F, Ctx, S, N, U, A> {
        let Self {
            cx,
            start,
            path,
            filter,
            order,
            context,
            shadowed,
            unique,
            ambiguous,
            missing: _,
        } = self;
        ScopeResolution {
            cx,
            start,
            path,
            filter,
            order,
            context,
            shadowed,
            missing: Some(handler),
            unique,
            ambiguous,
        }
    }

    pub fn on_unique<N, R>(self, handler: N) -> ScopeResolution<'cx, 'tx, C, D, F, Ctx, S, M, N, A>
    where
        N: FnOnce(&'cx mut Context<'tx, C>, Ctx, ResolutionPath<D>) -> Result<R, Error>,
    {
        let Self {
            cx,
            start,
            path,
            filter,
            order,
            context,
            shadowed,
            missing,
            ambiguous,
            unique: _,
        } = self;
        ScopeResolution {
            cx,
            start,
            path,
            filter,
            order,
            context,
            shadowed,
            missing,
            unique: Some(handler),
            ambiguous,
        }
    }

    pub fn on_ambiguous<N>(
        self,
        handler: N,
    ) -> ScopeResolution<'cx, 'tx, C, D, F, Ctx, S, M, U, N> {
        let Self {
            cx,
            start,
            path,
            filter,
            order,
            context,
            shadowed,
            missing,
            unique,
            ambiguous: _,
        } = self;
        ScopeResolution {
            cx,
            start,
            path,
            filter,
            order,
            context,
            shadowed,
            missing,
            unique,
            ambiguous: Some(handler),
        }
    }
}

impl<'cx, 'tx, C: Component, D, F, Ctx, S, M, U, A>
    ScopeResolution<'cx, 'tx, C, D, F, Ctx, S, M, U, A>
where
    D: ScopeDomain,
{
    pub fn resolve<R>(self) -> Result<R, Error>
    where
        F: Fn(&D::ScopeData) -> bool,
        S: ShadowResponse<C, D, Ctx>,
        M: FnOnce(&'cx mut Context<'tx, C>, Ctx) -> Result<R, Error>,
        U: FnOnce(&'cx mut Context<'tx, C>, Ctx, ResolutionPath<D>) -> Result<R, Error>,
        A: FnOnce(&'cx mut Context<'tx, C>, Ctx, usize) -> Result<R, Error>,
    {
        let Self {
            cx,
            start,
            path,
            filter,
            order,
            context,
            shadowed,
            missing,
            unique,
            ambiguous,
        } = self;
        let paths = resolve_paths(cx, start, path, filter);
        let (visible, dominated) = match order.as_ref() {
            Some(order) => partition_visible(paths, order),
            None => (paths.into_iter().collect(), Vec::new()),
        };
        let mut shadowed = shadowed.expect("shadow response is required");
        let mut context = context;
        for (shadowed_path, visible_by) in dominated {
            shadowed.run(cx, &mut context, shadowed_path, &visible_by)?;
        }
        match visible.len() {
            0 => (missing.expect("missing response is required"))(cx, context),
            1 => {
                let path = visible.into_iter().next().expect("one resolution path");
                (unique.expect("unique response is required"))(cx, context, path)
            }
            candidates => {
                (ambiguous.expect("ambiguous response is required"))(cx, context, candidates)
            }
        }
    }
}

fn resolve_paths<'tx, C, D, F>(
    cx: &mut Context<'tx, C>,
    start: ScopeId<D>,
    path: ScopePath<D::Label>,
    accepts: F,
) -> HashSet<ResolutionPath<D>>
where
    C: Component,
    D: ScopeDomain,
    F: Fn(&D::ScopeData) -> bool,
{
    resolve_indexed(start, path.into_path(), accepts, |scope, needs| {
        let edges = crate::scheme::node::ReadGraph::scan::<StructureEdges<ScopeStructure<D>>>(
            &cx.derive, scope,
        );
        let data = if needs {
            cx.derive
                .get::<StructureNode<ScopeStructure<D>>>(scope)
                .and_then(|artifact| artifact.deref::<D::ScopeData>())
                .map(|value| (*value).clone())
        } else {
            None
        };
        (edges, data)
    })
}

pub(crate) fn partition_visible<D: ScopeDomain>(
    paths: HashSet<ResolutionPath<D>>,
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

#[cfg(test)]
#[path = "../../../tests/unit/component_scope_query.rs"]
mod tests;
