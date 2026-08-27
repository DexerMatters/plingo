//! Opaque plans and plain-function reactive effects.
//!
//! Authored computations are ordinary Rust functions. The engine captures
//! their dependencies and writes while the free effects below provide the
//! only runtime boundary visible to authored code.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use parking_lot::{Mutex, RwLock};

use crate::reactive::error::Result;
use crate::reactive::kind::MapView;

/// An isolated capture of one plain function and its effects.
///
/// The captured graph is intentionally private: callers can inspect only the
/// function's typed result and must promote it through [`Engine::run`].
pub(crate) struct Planned<B> {
    pub(crate) engine_id: usize,
    pub(crate) token: u64,
    pub(crate) output: Arc<RwLock<Arc<B>>>,
    pub(crate) plan: Mutex<Option<crate::reactive::plain::PlainPlan>>,
}

impl<B> Planned<B> {
    /// Returns the value captured during planning.
    pub fn output(&self) -> Arc<B> {
        Arc::clone(&self.output.read())
    }
}

/// A committed root computation.
///
/// The engine updates the private output cell after successful reactive
/// reruns. Dropping this handle intentionally does not retire the root;
/// [`Engine::remove`] is the sole retirement operation.
pub(crate) struct Running<B> {
    pub(crate) engine_id: usize,
    pub(crate) token: u64,
    pub(crate) output: Arc<RwLock<Arc<B>>>,
    pub(crate) removed: Arc<AtomicBool>,
}

impl<B> Running<B> {
    /// Returns the latest successfully committed result.
    pub fn output(&self) -> Arc<B> {
        Arc::clone(&self.output.read())
    }
}

/// Explicit nested reactive computation boundary.
#[track_caller]
pub fn run<F, A, B>(function: F, input: A) -> Result<B>
where
    F: Fn(A) -> Result<B> + Clone + Send + Sync + 'static,
    A: Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static,
    B: Clone + PartialEq + std::fmt::Debug + Send + Sync + 'static,
{
    crate::reactive::plain::run_effect(function, input)
}
/// Runs one stable child computation for every current entry in a map view.
///
/// The parent owns the keyset relationship; each child must observe its own
/// map entry. Key insertion, change, and removal are then ordinary reactive
/// lifecycle events rather than caller-managed bookkeeping.
#[track_caller]
pub fn run_each_key<V, F>(function: F) -> Result<()>
where
    V: MapView + crate::reactive::kind::ViewKind<Observe = crate::reactive::kind::MapObserve<V>>,
    F: Fn(V::Input) -> Result<()> + Clone + Send + Sync + 'static,
    V::Input: Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static,
{
    let keys = crate::reactive::kind::observe_view::<V>()?.keys()?;
    for key in keys {
        crate::reactive::plain::run_keyed_effect(function.clone(), key)?;
    }
    Ok(())
}

/// Runs one stable child computation per child link of a tree-kind view
/// (plan §11 child relationship lifecycle).
///
/// The enumerator reads exactly the child-order facts of the view and
/// spawns a keyed effect per `(parent, child)` link. Each child computation
/// observes its own link fact, so an unchanged link never reruns; inserted
/// and removed links are ordinary lifecycle events and retract the child's
/// owned facts. This replaces parent-side `observe_children` loops that
/// re-enumerated unchanged declarations on every document revision.
#[track_caller]
pub fn run_each_child<V, F>(function: F) -> Result<()>
where
    V: crate::reactive::kind::TreeView
        + crate::reactive::kind::ViewKind<Observe = crate::reactive::kind::TreeObserve<V>>,
    F: Fn(crate::reactive::view::Node<V>, crate::reactive::view::Node<V>) -> Result<()>
        + Clone
        + Send
        + Sync
        + 'static,
{
    use crate::reactive::kind::{TreeKey, observe_view};
    let group = std::panic::Location::caller();
    let observe = observe_view::<V>()?;
    let mut parents: Vec<crate::reactive::view::Node<V>> = Vec::new();
    for input in observe.all_keys(crate::reactive::plain::Temporal::Current)? {
        if let TreeKey::ChildOrder(parent) = input {
            parents.push(parent);
        }
    }
    let mut pairs = Vec::new();
    for parent in parents {
        for child in observe.children(parent.clone())? {
            pairs.push((parent.clone(), child));
        }
    }
    let keep = pairs
        .iter()
        .map(|(parent, child)| {
            Arc::new((parent.clone(), child.clone()))
                as Arc<dyn crate::reactive::value::KeyValue>
        })
        .collect::<Vec<_>>();
    crate::reactive::plain::reconcile_keyed_children(group, &keep)?;
    for (parent, child) in pairs {
        spawn_child_adapt(&function, parent, child, group)?;
    }
    Ok(())
}

/// Runs one stable child computation per child link of ONE parent
/// (plan §15.2 scoped keyset dependency).
///
/// The enumerator's dependencies are exactly `ChildOrder(parent)` plus the
/// child links it spawns, so a child insertion under another parent does
/// not wake it, and an insertion under `parent` wakes only this enumerator.
/// Inserted links spawn a keyed effect; removed links retire the existing
/// effect and retract its owned facts.
#[track_caller]
pub fn run_each_child_of<V, F>(parent: crate::reactive::view::Node<V>, function: F) -> Result<()>
where
    V: crate::reactive::kind::TreeView
        + crate::reactive::kind::ViewKind<Observe = crate::reactive::kind::TreeObserve<V>>,
    F: Fn(crate::reactive::view::Node<V>, crate::reactive::view::Node<V>) -> Result<()>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let group = std::panic::Location::caller();
    let observe = crate::reactive::kind::observe_view::<V>()?;
    let children = observe.children(parent.clone())?;
    if std::env::var_os("PLINGO_TRACE_CHILD_EFFECTS").is_some() {
        eprintln!("child-effects parent={parent:?} children={children:?}");
    }
    let keep = children
        .iter()
        .map(|child| {
            Arc::new((parent.clone(), child.clone()))
                as Arc<dyn crate::reactive::value::KeyValue>
        })
        .collect::<Vec<_>>();
    crate::reactive::plain::reconcile_keyed_children(group, &keep)?;
    for child in children {
        spawn_child_adapt(&function, parent.clone(), child, group)?;
    }
    Ok(())
}

fn spawn_child_adapt<V, F>(
    function: &F,
    parent: crate::reactive::view::Node<V>,
    child: crate::reactive::view::Node<V>,
    group: &'static std::panic::Location<'static>,
) -> Result<()>
where
    V: crate::reactive::kind::TreeView,
    F: Fn(crate::reactive::view::Node<V>, crate::reactive::view::Node<V>) -> Result<()>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let function = function.clone();
    crate::reactive::plain::run_keyed_effect_at(
        move |(parent, child): (
            crate::reactive::view::Node<V>,
            crate::reactive::view::Node<V>,
        )| function(parent, child),
        (parent, child),
        group,
    )
}
