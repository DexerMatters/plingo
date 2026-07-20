use std::{
    any::{TypeId, type_name},
    marker::PhantomData,
    sync::{Arc, atomic::Ordering},
};

use tokio::sync::oneshot;

use crate::scheme::{
    __macro_private,
    call::LayerMethod,
    error::ActionError,
    layer::FallibleLayer,
    runtime::{
        LayerRegistry,
        message::{DEFAULT_DEMAND_RETRY_BUDGET, Demand, WorkerMessage},
    },
};

/// Context shared by all layers which can be used to resolve actions and access
/// the registry.
#[derive(Clone)]
pub struct Context {
    pub(crate) registry: Arc<LayerRegistry>,
    pub(crate) snapshot: Option<SnapshotId>,
    pub(crate) current_layer_type: Option<TypeId>,
    pub(crate) call_stack: Vec<TypeId>,
}

pub type SnapshotId = u64;

impl Default for Context {
    fn default() -> Self {
        Self {
            registry: Arc::new(LayerRegistry::default()),
            snapshot: None,
            current_layer_type: None,
            call_stack: Vec::new(),
        }
    }
}

impl Context {
    pub fn snapshot(&self) -> Option<SnapshotId> {
        self.snapshot
    }

    pub fn with_snapshot(&self, snapshot: Option<SnapshotId>) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
            snapshot,
            current_layer_type: self.current_layer_type,
            call_stack: self.call_stack.clone(),
        }
    }

    pub fn last_snapshot(&self) -> Context {
        self.with_snapshot(self.snapshot.map(|snapshot| {
            self.registry
                .snapshot_parents
                .lock()
                .expect("snapshot parent registry poisoned")
                .get(&snapshot)
                .copied()
                .unwrap_or(snapshot)
        }))
    }

    pub(crate) fn allocate_snapshot(&self, base: SnapshotId) -> SnapshotId {
        let target = self.registry.next_snapshot.fetch_add(1, Ordering::Relaxed);
        let mut parents = self
            .registry
            .snapshot_parents
            .lock()
            .expect("snapshot parent registry poisoned");
        parents.insert(target, base);
        while parents.len() > 64 {
            parents.pop_first();
        }
        target
    }

    pub(crate) fn with_current_layer(&self, current_layer_type: TypeId) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
            snapshot: self.snapshot,
            current_layer_type: Some(current_layer_type),
            call_stack: self.call_stack.clone(),
        }
    }

    pub(crate) fn with_call_stack(&self, call_stack: Vec<TypeId>) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
            snapshot: self.snapshot,
            current_layer_type: self.current_layer_type,
            call_stack,
        }
    }

    pub async fn call<L, Args, O>(
        &self,
        method: LayerMethod<L, Args, O>,
        args: Args,
    ) -> Result<O, ActionError>
    where
        L: FallibleLayer + 'static,
        Args: Send + Sync + 'static,
        O: Send + Sync + 'static,
    {
        let action_name = type_name::<Args>().to_string();
        let layer_type = TypeId::of::<L>();
        let layer_name = self
            .registry
            .layer_names
            .get(&layer_type)
            .copied()
            .unwrap_or(type_name::<L>());
        if let Some(current_layer_type) = self.current_layer_type {
            if current_layer_type == layer_type || self.call_stack.contains(&layer_type) {
                let current_layer_name = self
                    .registry
                    .layer_names
                    .get(&current_layer_type)
                    .copied()
                    .unwrap_or("unknown");
                return Err(ActionError::LayerCallCycle {
                    action: action_name.clone(),
                    layer: current_layer_name.to_string(),
                    target: layer_name.to_string(),
                });
            }
        }

        let sender = self
            .registry
            .senders
            .get(&layer_type)
            .cloned()
            .ok_or_else(|| ActionError::MissingResource {
                action: action_name.clone(),
                layer: layer_name.to_string(),
            })?;

        let (response_tx, response_rx) = oneshot::channel();
        let mut call_stack = self.call_stack.clone();
        if let Some(current_layer_type) = self.current_layer_type {
            call_stack.push(current_layer_type);
        }
        let demand = Demand {
            action: Arc::new(__macro_private::CallPayload::<L, Args, O> {
                method,
                args,
                _marker: PhantomData,
            }),
            action_name: type_name::<Args>(),
            requester_layer_type: layer_type,
            snapshot: self.snapshot,
            remaining_retries: DEFAULT_DEMAND_RETRY_BUDGET,
            dispatch: __macro_private::dispatch_call::<L, Args, O>,
            call_stack,
            response_tx,
        };

        sender
            .send(WorkerMessage::Demand(demand))
            .await
            .map_err(|_| ActionError::ChannelClosed {
                action: action_name.clone(),
                layer: layer_name.to_string(),
            })?;

        let erased = response_rx.await.map_err(|_| ActionError::ChannelClosed {
            action: action_name.clone(),
            layer: layer_name.to_string(),
        })??;

        let typed = erased.value.downcast::<O>().unwrap_or_else(|_| {
            unreachable!(
                "call output downcast must match surface API: action={}, layer={}, expected={}",
                action_name,
                layer_name,
                type_name::<O>(),
            )
        });

        Ok(*typed)
    }
}
