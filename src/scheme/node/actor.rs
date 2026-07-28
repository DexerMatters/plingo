//! Tokio integration for the graph's single-writer transaction runtime.
//!
//! The actor owns [`Graph`] and invokes each existing synchronous operation to
//! completion before receiving another message. This deliberately does not make
//! node derivation asynchronous: a transaction still either commits atomically
//! or leaves the published snapshot unchanged.

use super::{
    Command, Graph, GraphReader, Node, NodeError, Relation, RelationSubscription, RequestHandle,
    Subscription, View,
};
use std::fmt;
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

type GraphWork = Box<dyn FnOnce(&mut Graph) + Send + 'static>;

/// Errors returned by asynchronous graph operations.
#[derive(Debug, Error)]
pub enum GraphActorError {
    #[error(transparent)]
    Node(#[from] NodeError),
    #[error("the graph actor has stopped")]
    Closed,
    #[error("the graph operation was cancelled before it began")]
    Cancelled,
}

/// Cloneable asynchronous entry point to one serialized [`Graph`] runtime.
#[derive(Clone)]
pub struct GraphHandle {
    sender: mpsc::Sender<GraphWork>,
    reader: GraphReader,
}

impl fmt::Debug for GraphHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GraphHandle")
            .finish_non_exhaustive()
    }
}

impl GraphHandle {
    /// Returns a lock-free reader for successfully committed graph snapshots.
    pub fn reader(&self) -> GraphReader {
        self.reader.clone()
    }

    /// Runs a fallible graph operation on the actor and awaits its result.
    async fn call<T, F>(&self, operation: F) -> Result<T, GraphActorError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Graph) -> Result<T, NodeError> + Send + 'static,
    {
        let (reply_sender, reply_receiver) = oneshot::channel();
        self.sender
            .send(Box::new(move |graph| {
                let _ = reply_sender.send(operation(graph).map_err(GraphActorError::from));
            }))
            .await
            .map_err(|_| GraphActorError::Closed)?;
        reply_receiver.await.map_err(|_| GraphActorError::Closed)?
    }

    /// Installs a node implementation on the graph actor.
    pub async fn install<N: Node>(&self, node: N) -> Result<(), GraphActorError> {
        self.call(move |graph| graph.install(node)).await
    }

    /// Applies one root command atomically.
    pub async fn command<C: Command>(&self, command: C) -> Result<C::Output, GraphActorError>
    where
        C::Output: Send + 'static,
    {
        self.call(move |graph| graph.command(command)).await
    }

    /// Applies a command unless `cancel` fires before the actor starts it.
    /// Once command execution has started it remains non-preemptible, preserving
    /// the graph's all-or-nothing transaction semantics.
    pub async fn command_with_cancel<C>(
        &self,
        command: C,
        cancel: CancellationToken,
    ) -> Result<C::Output, GraphActorError>
    where
        C: Command,
        C::Output: Send + 'static,
    {
        if cancel.is_cancelled() {
            return Err(GraphActorError::Cancelled);
        }
        let (reply_sender, reply_receiver) = oneshot::channel();
        let queued_cancel = cancel.clone();
        let work = Box::new(move |graph: &mut Graph| {
            let result = if queued_cancel.is_cancelled() {
                Err(GraphActorError::Cancelled)
            } else {
                graph.command(command).map_err(GraphActorError::from)
            };
            let _ = reply_sender.send(result);
        });
        tokio::select! {
            _ = cancel.cancelled() => Err(GraphActorError::Cancelled),
            sent = self.sender.send(work) => {
                sent.map_err(|_| GraphActorError::Closed)?;
                reply_receiver.await.map_err(|_| GraphActorError::Closed)?
            }
        }
    }

    /// Requests a derived node and returns its normal RAII demand handle.
    pub async fn request<N: Node>(&self, key: N::Key) -> Result<RequestHandle<N>, GraphActorError> {
        self.call(move |graph| graph.request::<N>(key)).await
    }

    /// Subscribes to a materialized view. The returned subscription keeps the
    /// existing exact, unbounded update contract; callers may bridge it to an
    /// async stream if they need per-transition delivery.
    pub async fn subscribe_view<V: View>(
        &self,
        key: V::Key,
    ) -> Result<Subscription<V>, GraphActorError> {
        self.call(move |graph| graph.subscribe_view::<V>(key)).await
    }

    /// Activates and subscribes to a node's primary output.
    pub async fn subscribe<N: Node>(
        &self,
        key: N::Key,
    ) -> Result<Subscription<N::Output>, GraphActorError> {
        self.call(move |graph| graph.subscribe::<N>(key)).await
    }

    /// Subscribes to one relation presence transition.
    pub async fn subscribe_relation<R: Relation>(
        &self,
        fact: R::Fact,
    ) -> Result<RelationSubscription<R>, GraphActorError> {
        self.call(move |graph| Ok(graph.subscribe_relation::<R>(fact)))
            .await
    }

    /// Processes leases dropped by requests and subscriptions when no later
    /// mutation is otherwise sent to the graph actor.
    pub async fn collect_garbage(&self) -> Result<(), GraphActorError> {
        self.call(Graph::collect_garbage).await
    }
}

/// Owns the asynchronous actor lifecycle. Dropping or calling [`Self::shutdown`]
/// stops the actor after its current operation, while all cloned [`GraphHandle`]s
/// become closed.
pub struct GraphRuntime {
    handle: GraphHandle,
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl GraphRuntime {
    /// Spawns a serialized graph actor on the current Tokio runtime.
    ///
    /// `capacity` bounds pending mutations, not subscriptions. A capacity of
    /// zero is normalized to one so callers always have a queueing boundary.
    pub fn spawn(graph: Graph, capacity: usize) -> Self {
        let (sender, mut receiver) = mpsc::channel::<GraphWork>(capacity.max(1));
        let shutdown = CancellationToken::new();
        let actor_shutdown = shutdown.clone();
        let reader = graph.reader();
        let task = tokio::spawn(async move {
            let mut graph = graph;
            loop {
                tokio::select! {
                    biased;
                    _ = actor_shutdown.cancelled() => break,
                    work = receiver.recv() => match work {
                        Some(work) => work(&mut graph),
                        None => break,
                    },
                }
            }
        });
        Self {
            handle: GraphHandle { sender, reader },
            shutdown,
            task: Some(task),
        }
    }

    pub fn handle(&self) -> GraphHandle {
        self.handle.clone()
    }

    /// Cancels the actor queue and waits for the current operation to finish.
    pub async fn shutdown(mut self) -> Result<(), tokio::task::JoinError> {
        self.shutdown.cancel();
        match self.task.take() {
            Some(task) => task.await,
            None => Ok(()),
        }
    }
}

impl Drop for GraphRuntime {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// The actor loop is exposed for hosts that need to own task spawning.
pub struct GraphActor {
    graph: Graph,
    receiver: mpsc::Receiver<GraphWork>,
    shutdown: CancellationToken,
}

impl GraphActor {
    /// Constructs an actor without spawning it, returning its handle.
    pub fn new(graph: Graph, capacity: usize) -> (GraphHandle, Self) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let shutdown = CancellationToken::new();
        let reader = graph.reader();
        (
            GraphHandle { sender, reader },
            Self {
                graph,
                receiver,
                shutdown,
            },
        )
    }

    /// Returns the actor's cancellation token for external lifecycle control.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Runs until the sender closes or cancellation is requested.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                work = self.receiver.recv() => match work {
                    Some(work) => work(&mut self.graph),
                    None => break,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheme::node::{CommandCx, View};

    struct Value;
    impl View for Value {
        type Key = String;
        type Value = String;
    }

    struct Set(&'static str);
    impl Command for Set {
        type Output = ();

        fn apply(self, cx: &mut CommandCx<'_, '_>) -> Result<(), NodeError> {
            cx.set::<Value>("value".into(), self.0.into())
        }
    }

    #[tokio::test]
    async fn actor_serializes_commands_and_publishes_reader_snapshots() {
        let runtime = GraphRuntime::spawn(Graph::new(), 4);
        let handle = runtime.handle();
        let first = handle.command(Set("one"));
        let second = handle.command(Set("two"));
        let (first, second) = tokio::join!(first, second);
        first.unwrap();
        second.unwrap();

        let snapshot = handle.reader().snapshot();
        assert_eq!(snapshot.read::<Value>("value".into()), Some("two".into()));
        assert_eq!(snapshot.id(), 2);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queued_cancelled_command_does_not_mutate_the_graph() {
        let runtime = GraphRuntime::spawn(Graph::new(), 1);
        let handle = runtime.handle();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            handle.command_with_cancel(Set("cancelled"), cancel).await,
            Err(GraphActorError::Cancelled)
        ));
        assert_eq!(handle.reader().read::<Value>("value".into()), None);
        runtime.shutdown().await.unwrap();
    }
}
