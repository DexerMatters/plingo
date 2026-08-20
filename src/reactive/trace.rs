//! Computation instances, dynamic dependency capture, visitor paths,
//! the child registry, the reverse fact index, and the ownership index
//! (§5.1, §4.1).
//!
//! A *visitor instance* is one computation: the root instance of a
//! component, or one nested visitor closure created by a `visit_*` method.
//! Its reads are captured dynamically during evaluation (the dynamic input
//! set of §4.1); its writes are the outputs. The reverse index maps each
//! fact to the instances that read it, so a fact change schedules exactly
//! the readers whose read set intersects it (T4). Temporal reads
//! (`Previous`) live in a separate index and only schedule at the next
//! epoch's start, against the previous epoch's committed delta.

use std::collections::HashMap;
use std::sync::Arc;


use parking_lot::Mutex;

use crate::reactive::error::{Error, Result};
use crate::reactive::store::WriteKind;
use crate::reactive::value::KeyValue;

pub(crate) type ViewId = u32;
pub(crate) type ComponentId = u32;
pub(crate) type InstanceId = u32;

/// One read: a fact reference plus whether it is temporal (`Previous`).
#[derive(Clone, Debug)]
pub(crate) struct FactRef {
    pub view: ViewId,
    pub key: Arc<dyn KeyValue>,
    pub temporal: bool,
}

/// One path step: the visit kind and the element's debug string.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PathStep {
    pub kind: &'static str,
    pub elem: String,
}

/// The identity of one child visitor within its parent: the visit kind
/// and the element identity (a map key, node id, edge key, or unit).
#[derive(Clone)]
pub(crate) struct ChildKey {
    pub kind: &'static str,
    pub key: Arc<dyn KeyValue>,
}

impl ChildKey {
    pub(crate) fn hash(&self) -> u64 {
        self.key.hash_value()
    }
    pub(crate) fn matches(&self, other: &ChildKey) -> bool {
        self.kind == other.kind && self.key.eq_value(other.key.as_ref())
    }
    pub(crate) fn path_step(&self) -> PathStep {
        PathStep {
            kind: self.kind,
            elem: format!("{:?}", self.key),
        }
    }
}

pub(crate) enum InstanceKind {
    Root,
    Child,
}

/// One visitor instance's metadata. Closures live in [`Registry::closures`]
/// so runs can take them out while the registry lock is released.
pub(crate) struct Instance {
    pub id: InstanceId,
    pub component: ComponentId,
    pub path: Vec<PathStep>,
    pub rank: u32,
    pub kind: InstanceKind,
    pub parent: Option<InstanceId>,
    /// Reads of the last run (committed dynamic graph, §5.5 step 4).
    pub reads: Vec<FactRef>,
    /// Facts written this epoch (accumulated; used for cycle rejection).
    pub epoch_writes: Vec<(ViewId, Arc<dyn KeyValue>)>,
    /// Facts written since the instance's creation (accumulated; retired
    /// children retract exactly these facts).
    pub lifetime_writes: Vec<(ViewId, Arc<dyn KeyValue>)>,
    /// Retired children are dead: their reads leave the reverse index.
    /// The flag is epoch-local; commit compacts them away.
    pub retired: bool,
}

/// One instance's run buffer: what its evaluation captured.
pub(crate) struct RunBuffer {
    pub reads: Vec<FactRef>,
    pub writes: Vec<(ViewId, WriteKind)>,
    /// Child keys registered during the run (for retirement diffs).
    pub children: Vec<ChildKey>,
    /// Fresh-id lane counter (stable allocation order within a run).
    pub fresh_lane: u64,
    pub error: Option<Error>,
}

impl RunBuffer {
    pub(crate) fn new(_instance: InstanceId) -> Self {
        RunBuffer {
            reads: Vec::new(),
            writes: Vec::new(),
            children: Vec::new(),
            fresh_lane: 0,
            error: None,
        }
    }
}

/// The instance registry and fact indexes.
pub(crate) struct Registry {
    /// Instances by id; ids are never reused within an epoch and dead
    /// instances are compacted away at commit.
    pub instances: Vec<Instance>,
    /// Stored closures, keyed by instance id (take/put-back while running).
    pub closures: HashMap<InstanceId, Box<dyn FnMut() -> Result<()> + Send + Sync>>,
    /// (parent, key-hash) → matching child keys. Lookup only; iteration
    /// order never drives committed behavior.
    pub children: HashMap<(InstanceId, u64), Vec<(ChildKey, InstanceId)>>,
    /// Non-temporal reads: fact → readers (insertion order = deterministic).
    pub reverse_now: HashMap<(ViewId, u64), Vec<(Arc<dyn KeyValue>, Vec<InstanceId>)>>,
    /// Temporal (`Previous`) reads: scheduled only at the next epoch start.
    pub reverse_prev: HashMap<(ViewId, u64), Vec<(Arc<dyn KeyValue>, Vec<InstanceId>)>>,
}

impl Registry {
    pub(crate) fn new() -> Self {
        Registry {
            instances: Vec::new(),
            closures: HashMap::new(),
            children: HashMap::new(),
            reverse_now: HashMap::new(),
            reverse_prev: HashMap::new(),
        }
    }

    pub(crate) fn reverse_add(&mut self, id: InstanceId, fact: &FactRef) {
        let map = if fact.temporal {
            &mut self.reverse_prev
        } else {
            &mut self.reverse_now
        };
        let hash = fact.key.hash_value();
        let bucket = map.entry((fact.view, hash)).or_default();
        if let Some((_, readers)) = bucket
            .iter_mut()
            .find(|(key, _)| key.eq_value(fact.key.as_ref()))
        {
            if !readers.contains(&id) {
                readers.push(id);
            }
        } else {
            bucket.push((fact.key.clone(), vec![id]));
        }
    }

    pub(crate) fn reverse_remove(&mut self, id: InstanceId, fact: &FactRef) {
        let map = if fact.temporal {
            &mut self.reverse_prev
        } else {
            &mut self.reverse_now
        };
        let hash = fact.key.hash_value();
        if let Some(bucket) = map.get_mut(&(fact.view, hash)) {
            if let Some((_, readers)) = bucket
                .iter_mut()
                .find(|(key, _)| key.eq_value(fact.key.as_ref()))
            {
                readers.retain(|reader| *reader != id);
            }
        }
    }

    /// The readers of one fact, in insertion order (deterministic).
    pub(crate) fn readers_of(&self, fact: &FactRef, temporal: bool) -> &[InstanceId] {
        let map = if temporal {
            &self.reverse_prev
        } else {
            &self.reverse_now
        };
        let hash = fact.key.hash_value();
        map.get(&(fact.view, hash))
            .and_then(|bucket| {
                bucket
                    .iter()
                    .find(|(key, _)| key.eq_value(fact.key.as_ref()))
                    .map(|(_, readers)| readers.as_slice())
            })
            .unwrap_or(&[])
    }

    /// Rebuilds both reverse indexes from every live instance's reads.
    pub(crate) fn rebuild_reverse(&mut self) {
        let reads: Vec<(InstanceId, Vec<FactRef>)> = self
            .instances
            .iter()
            .filter(|instance| !instance.retired)
            .map(|instance| (instance.id, instance.reads.clone()))
            .collect();
        self.reverse_now.clear();
        self.reverse_prev.clear();
        for (id, facts) in reads {
            for fact in &facts {
                self.reverse_add(id, fact);
            }
        }
    }

    /// Drops dead instances and renumbers ids so the registry stays
    /// bounded across epochs. Runs once at commit.
    pub(crate) fn compact(&mut self) {
        let mut remap: HashMap<InstanceId, InstanceId> = HashMap::new();
        let mut live: Vec<Instance> = Vec::with_capacity(self.instances.len());
        for instance in self.instances.drain(..) {
            if instance.retired {
                continue;
            }
            remap.insert(instance.id, live.len() as InstanceId);
            live.push(instance);
        }
        for instance in &mut live {
            instance.id = remap[&instance.id];
            instance.parent = instance.parent.map(|p| remap[&p]);
        }
        let closures = std::mem::take(&mut self.closures);
        self.closures = closures
            .into_iter()
            .filter_map(|(id, closure)| remap.get(&id).map(|new_id| (*new_id, closure)))
            .collect();
        let children = std::mem::take(&mut self.children);
        self.children = children
            .into_iter()
            .filter_map(|((parent, hash), bucket)| {
                let new_parent = *remap.get(&parent)?;
                let bucket: Vec<(ChildKey, InstanceId)> = bucket
                    .into_iter()
                    .filter_map(|(key, id)| remap.get(&id).map(|new_id| (key, *new_id)))
                    .collect();
                Some(((new_parent, hash), bucket))
            })
            .collect();
        self.instances = live;
        self.rebuild_reverse();
    }
}

/// A shared run-buffer handle, used by the thread-local active frame.
pub(crate) type RunBufferHandle = Arc<Mutex<RunBuffer>>;

/// One entry of the per-thread active-visitor stack.
pub(crate) struct Frame {
    pub instance: InstanceId,
    pub component: ComponentId,
    pub buffer: RunBufferHandle,
    pub shared: Arc<crate::reactive::engine::Shared>,
}

thread_local! {
    /// The active visitor stack (innermost last). Reads and writes attach
    /// to the top frame's instance.
    pub(crate) static ACTIVE: std::cell::RefCell<Vec<Frame>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` against the active visitor frame, or fails with the given
/// outside-visitor error (deterministic).
pub(crate) fn with_frame<T>(view: &str, f: impl FnOnce(&Frame) -> T) -> Result<T> {
    ACTIVE.with(|active| {
        let active = active.borrow();
        let Some(frame) = active.last() else {
            return Err(Error::ReadOutsideVisitor {
                view: view.to_string(),
            });
        };
        Ok(f(frame))
    })
}

/// Records one read against the active visitor (dynamic capture).
pub(crate) fn record_read(view: ViewId, key: Arc<dyn KeyValue>, temporal: bool) -> Result<()> {
    with_frame("read", |frame| {
        frame
            .buffer
            .lock()
            .reads
            .push(FactRef { view, key, temporal });
    })
}

/// Records one write against the active visitor, or fails outside one.
pub(crate) fn record_write(view: ViewId, kind: WriteKind) -> Result<()> {
    ACTIVE.with(|active| {
        let active = active.borrow();
        let Some(frame) = active.last() else {
            return Err(Error::WriteOutsideVisitor {
                view: "<unknown>".to_string(),
            });
        };
        frame.buffer.lock().writes.push((view, kind));
        Ok(())
    })
}