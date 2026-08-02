use super::*;
use crate::scheme::node::{CommandCx, ReadGraph, View};

struct Value;
impl View for Value {
    type Key = String;
    type Value = String;
}

struct Set(&'static str);
impl Command for Set {
    type Output = ();

    fn apply(self, cx: &mut CommandCx<'_>) -> Result<(), NodeError> {
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
    assert_eq!(snapshot.get::<Value>("value".into()), Some("two".into()));
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
    assert_eq!(handle.reader().get::<Value>("value".into()), None);
    runtime.shutdown().await.unwrap();
}
