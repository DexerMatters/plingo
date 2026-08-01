use super::*;
use std::{
    any::{TypeId, type_name},
    sync::{Arc, Mutex, mpsc},
};

struct Text;
impl View for Text {
    type Key = String;
    type Value = String;
}

struct Tokens;
impl View for Tokens {
    type Key = String;
    type Value = Vec<String>;
}

struct Tokenize;
impl NodeProvider for Tokenize {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(type_name::<Self>(), vec![PortDeclaration::map::<Tokens>()])
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        let tokens = cx
            .get::<Text>(key.clone())
            .ok_or_else(NodeError::missing_view::<Text>)?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        cx.emit::<Tokens>(key, tokens)
    }
}

struct Count;
impl View for Count {
    type Key = String;
    type Value = usize;
}

struct CountTokens;
impl NodeProvider for CountTokens {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(type_name::<Self>(), vec![PortDeclaration::map::<Count>()])
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        cx.materialize::<Tokenize>(key.clone())?;
        let count = cx
            .get::<Tokens>(key.clone())
            .ok_or_else(NodeError::missing_view::<Tokens>)?
            .len();
        cx.emit::<Count>(key, count)
    }
}

struct SetText {
    key: String,
    value: String,
}

impl Command for SetText {
    type Output = ();

    fn apply(self, cx: &mut CommandCx<'_, '_>) -> Result<(), NodeError> {
        cx.set::<Text>(self.key, self.value)
    }
}

#[test]
fn commands_recompute_only_observed_nodes_and_publish_after_commit() {
    let mut graph = Graph::new();
    graph.install(Tokenize).unwrap();
    graph.install(CountTokens).unwrap();
    graph
        .command(SetText {
            key: "document".into(),
            value: "one two".into(),
        })
        .unwrap();

    let _demand = graph.demand::<CountTokens>("document".into()).unwrap();
    let subscription = graph.subscribe::<Count>("document".into()).unwrap();
    assert_eq!(
        subscription.recv().unwrap(),
        ViewUpdate::Initial {
            snapshot: graph.revision(),
            value: 2,
        }
    );

    let reader = graph.reader();
    let before = reader.snapshot();
    graph
        .command(SetText {
            key: "document".into(),
            value: "one two three".into(),
        })
        .unwrap();

    assert_eq!(before.get::<Count>("document".into()), Some(2));
    assert_eq!(graph.get::<Count>("document".into()), Some(3));
    assert_eq!(reader.get::<Count>("document".into()), Some(3));
    assert_eq!(
        subscription.recv().unwrap(),
        ViewUpdate::Changed {
            snapshot: graph.revision(),
            value: 3,
        }
    );
}

struct Failing;
impl View for Failing {
    type Key = String;
    type Value = String;
}

struct FailingNode;
impl NodeProvider for FailingNode {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(type_name::<Self>(), vec![PortDeclaration::map::<Failing>()])
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        let text = cx
            .get::<Text>(key.clone())
            .ok_or_else(NodeError::missing_view::<Text>)?;
        if text == "fail" {
            return Err(NodeError::message("expected failure"));
        }
        cx.emit::<Failing>(key, text)
    }
}

#[test]
fn failed_derivation_rolls_back_root_and_suppresses_subscription() {
    let mut graph = Graph::new();
    graph.install(FailingNode).unwrap();
    graph
        .command(SetText {
            key: "document".into(),
            value: "ok".into(),
        })
        .unwrap();
    let _demand = graph.demand::<FailingNode>("document".into()).unwrap();
    let subscription = graph.subscribe::<Failing>("document".into()).unwrap();
    assert!(matches!(
        subscription.recv().unwrap(),
        ViewUpdate::Initial { .. }
    ));
    let reader = graph.reader();
    let snapshot = reader.snapshot();

    let result = graph.command(SetText {
        key: "document".into(),
        value: "fail".into(),
    });
    assert!(result.is_err());
    assert_eq!(graph.get::<Text>("document".into()), Some("ok".into()));
    assert_eq!(reader.get::<Text>("document".into()), Some("ok".into()));
    assert_eq!(reader.snapshot().id(), snapshot.id());
    assert_eq!(snapshot.get::<Text>("document".into()), Some("ok".into()));
    assert!(matches!(
        subscription.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
}

struct Enabled;
impl View for Enabled {
    type Key = String;
    type Value = bool;
}

struct SharedName;
impl Relation for SharedName {
    type Fact = String;
}

struct FirstOutput;
impl View for FirstOutput {
    type Key = String;
    type Value = bool;
}

struct SecondOutput;
impl View for SecondOutput {
    type Key = String;
    type Value = bool;
}

struct FirstSupport;
impl NodeProvider for FirstSupport {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![
                PortDeclaration::map::<FirstOutput>(),
                PortDeclaration::set::<SharedName>(),
            ],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        let enabled = cx
            .get::<Enabled>(format!("first:{key}"))
            .ok_or_else(NodeError::missing_view::<Enabled>)?;
        if enabled {
            cx.emit_relation::<SharedName>(key.clone())?;
        }
        cx.emit::<FirstOutput>(key, enabled)
    }
}

struct SecondSupport;
impl NodeProvider for SecondSupport {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![
                PortDeclaration::map::<SecondOutput>(),
                PortDeclaration::set::<SharedName>(),
            ],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        let enabled = cx
            .get::<Enabled>(format!("second:{key}"))
            .ok_or_else(NodeError::missing_view::<Enabled>)?;
        if enabled {
            cx.emit_relation::<SharedName>(key.clone())?;
        }
        cx.emit::<SecondOutput>(key, enabled)
    }
}

struct SetEnabled {
    key: String,
    value: bool,
}

impl Command for SetEnabled {
    type Output = ();

    fn apply(self, cx: &mut CommandCx<'_, '_>) -> Result<(), NodeError> {
        cx.set::<Enabled>(self.key, self.value)
    }
}

#[test]
fn relation_facts_survive_until_the_final_node_support_is_retracted() {
    let added = Arc::new(Mutex::new(Vec::new()));
    let removed = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.install(FirstSupport).unwrap();
    graph.install(SecondSupport).unwrap();
    graph
        .command(SetEnabled {
            key: "first:name".into(),
            value: true,
        })
        .unwrap();
    graph
        .command(SetEnabled {
            key: "second:name".into(),
            value: true,
        })
        .unwrap();
    graph.on_relation_added::<SharedName>({
        let added = Arc::clone(&added);
        move |snapshot, fact| {
            added.lock().unwrap().push((snapshot, fact));
            Ok(())
        }
    });
    graph.on_relation_removed::<SharedName>({
        let removed = Arc::clone(&removed);
        move |snapshot, fact| {
            removed.lock().unwrap().push((snapshot, fact));
            Ok(())
        }
    });

    let _first = graph.demand::<FirstSupport>("name".into()).unwrap();
    let _second = graph.demand::<SecondSupport>("name".into()).unwrap();
    assert!(graph.contains::<SharedName>("name".into()));
    assert_eq!(added.lock().unwrap().as_slice().len(), 1);
    assert!(removed.lock().unwrap().is_empty());
    let subscription = graph.subscribe_relation::<SharedName>("name".into());
    assert!(matches!(
        subscription.recv().unwrap(),
        RelationUpdate::Initial { present: true, .. }
    ));

    graph
        .command(SetEnabled {
            key: "first:name".into(),
            value: false,
        })
        .unwrap();
    assert!(graph.contains::<SharedName>("name".into()));
    assert_eq!(added.lock().unwrap().as_slice().len(), 1);
    assert!(removed.lock().unwrap().is_empty());
    assert!(matches!(
        subscription.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    graph
        .command(SetEnabled {
            key: "second:name".into(),
            value: false,
        })
        .unwrap();
    assert!(!graph.contains::<SharedName>("name".into()));
    assert_eq!(added.lock().unwrap().as_slice().len(), 1);
    assert_eq!(removed.lock().unwrap().as_slice().len(), 1);
    assert!(matches!(
        subscription.recv().unwrap(),
        RelationUpdate::Removed { .. }
    ));
}

#[test]
fn relation_effects_run_after_committed_state_and_subscription_publication() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.install(FirstSupport).unwrap();
    graph
        .command(SetEnabled {
            key: "first:name".into(),
            value: true,
        })
        .unwrap();
    let subscription = Arc::new(Mutex::new(
        graph.subscribe_relation::<SharedName>("name".into()),
    ));
    assert!(matches!(
        subscription.lock().unwrap().recv().unwrap(),
        RelationUpdate::Initial { present: false, .. }
    ));
    graph.on_relation_added::<SharedName>({
        let seen = Arc::clone(&seen);
        let subscription = Arc::clone(&subscription);
        move |snapshot, fact| {
            assert_eq!(
                subscription.lock().unwrap().try_recv().unwrap(),
                RelationUpdate::Added {
                    snapshot,
                    fact: fact.clone(),
                }
            );
            seen.lock().unwrap().push((snapshot, fact));
            Ok(())
        }
    });

    graph.demand::<FirstSupport>("name".into()).unwrap();

    assert!(graph.contains::<SharedName>("name".into()));
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[(graph.revision(), "name".into())]
    );
}

struct RejectEffectsOutput;
impl View for RejectEffectsOutput {
    type Key = String;
    type Value = bool;
}

struct RejectEffects;
impl NodeProvider for RejectEffects {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![PortDeclaration::map::<RejectEffectsOutput>()],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        cx.materialize::<FirstSupport>(key.clone())?;
        let enabled = cx
            .get::<FirstOutput>(key.clone())
            .ok_or_else(NodeError::missing_view::<FirstOutput>)?;
        if enabled {
            return Err(NodeError::message("reject relation effect transaction"));
        }
        cx.emit::<RejectEffectsOutput>(key, enabled)
    }
}

#[test]
fn aborted_transactions_do_not_run_relation_effects() {
    let calls = Arc::new(Mutex::new(0));
    let mut graph = Graph::new();
    graph.install(FirstSupport).unwrap();
    graph.install(RejectEffects).unwrap();
    graph
        .command(SetEnabled {
            key: "first:name".into(),
            value: false,
        })
        .unwrap();
    let _reject = graph.demand::<RejectEffects>("name".into()).unwrap();
    graph.on_relation_added::<SharedName>({
        let calls = Arc::clone(&calls);
        move |_, _| {
            *calls.lock().unwrap() += 1;
            Ok(())
        }
    });

    let result = graph.command(SetEnabled {
        key: "first:name".into(),
        value: true,
    });

    assert!(
        matches!(result, Err(NodeError::Message(message)) if message == "reject relation effect transaction")
    );
    assert!(!graph.contains::<SharedName>("name".into()));
    assert_eq!(*calls.lock().unwrap(), 0);
}

#[test]
fn relation_effect_failures_do_not_rollback_commits_and_can_be_drained() {
    let mut graph = Graph::new();
    graph.install(FirstSupport).unwrap();
    graph
        .command(SetEnabled {
            key: "first:name".into(),
            value: true,
        })
        .unwrap();
    graph.on_relation_added::<SharedName>(|_, _| Err("effect failed".into()));

    graph.demand::<FirstSupport>("name".into()).unwrap();

    assert!(graph.contains::<SharedName>("name".into()));
    assert_eq!(graph.effect_failures().len(), 1);
    let failures = graph.drain_effect_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].snapshot, graph.revision());
    assert_eq!(failures[0].relation, TypeId::of::<SharedName>());
    assert_eq!(failures[0].relation_name, type_name::<SharedName>());
    assert_eq!(failures[0].fact, "\"name\"");
    assert_eq!(failures[0].message, "effect failed");
    assert!(graph.effect_failures().is_empty());
}

struct StatefulOutput;
impl View for StatefulOutput {
    type Key = String;
    type Value = usize;
}

struct StatefulNode {
    state: ComponentState<usize>,
}

impl NodeProvider for StatefulNode {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![PortDeclaration::map::<StatefulOutput>()],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        let text = cx
            .get::<Text>(key.clone())
            .ok_or_else(NodeError::missing_view::<Text>)?;
        if text == "fail" {
            *cx.state_mut(&self.state)? += 1;
        }
        cx.emit::<StatefulOutput>(key, text.len())
    }
}

struct RejectStatefulOutput;
impl View for RejectStatefulOutput {
    type Key = String;
    type Value = usize;
}

struct RejectAfterStateful;
impl NodeProvider for RejectAfterStateful {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![PortDeclaration::map::<RejectStatefulOutput>()],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        cx.materialize::<StatefulNode>(key.clone())?;
        let value = cx
            .get::<StatefulOutput>(key.clone())
            .ok_or_else(NodeError::missing_view::<StatefulOutput>)?;
        if value == "fail".len() {
            return Err(NodeError::message("dependent failure"));
        }
        cx.emit::<RejectStatefulOutput>(key, value)
    }
}

#[test]
fn component_state_is_unchanged_when_a_later_derivation_fails() {
    let state = ComponentState::new(0usize);
    let mut graph = Graph::new();
    graph
        .install(StatefulNode {
            state: state.clone(),
        })
        .unwrap();
    graph.install(RejectAfterStateful).unwrap();
    graph
        .command(SetText {
            key: "document".into(),
            value: "ok".into(),
        })
        .unwrap();
    let _reject = graph
        .demand::<RejectAfterStateful>("document".into())
        .unwrap();

    let result = graph.command(SetText {
        key: "document".into(),
        value: "fail".into(),
    });

    assert!(matches!(result, Err(NodeError::Message(message)) if message == "dependent failure"));
    assert_eq!(state.get().unwrap(), 0);
    assert_eq!(graph.get::<Text>("document".into()), Some("ok".into()));
}

struct OwnedChildOutput;
impl View for OwnedChildOutput {
    type Key = String;
    type Value = bool;
}

struct OwnedChildExtra;
impl View for OwnedChildExtra {
    type Key = String;
    type Value = String;
}

struct OwnedChildRelation;
impl Relation for OwnedChildRelation {
    type Fact = String;
}

struct OwnedChild;
impl NodeProvider for OwnedChild {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![
                PortDeclaration::map::<OwnedChildOutput>(),
                PortDeclaration::map::<OwnedChildExtra>(),
                PortDeclaration::set::<OwnedChildRelation>(),
            ],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        cx.emit::<OwnedChildExtra>(key.clone(), format!("extra:{key}"))?;
        cx.emit_relation::<OwnedChildRelation>(key.clone())?;
        cx.emit::<OwnedChildOutput>(key, true)
    }
}

struct OwnedParentOutput;
impl View for OwnedParentOutput {
    type Key = String;
    type Value = bool;
}

struct OwnedParent;
impl NodeProvider for OwnedParent {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![PortDeclaration::map::<OwnedParentOutput>()],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        let enabled = cx
            .get::<Enabled>(format!("parent:{key}"))
            .ok_or_else(NodeError::missing_view::<Enabled>)?;
        if enabled {
            cx.materialize::<OwnedChild>("child".into())?;
        }
        cx.emit::<OwnedParentOutput>(key, enabled)
    }
}

fn owned_parent_graph() -> Graph {
    let mut graph = Graph::new();
    graph.install(OwnedChild).unwrap();
    graph.install(OwnedParent).unwrap();
    graph
}

#[test]
fn child_outputs_and_relations_are_retracted_when_parent_stops_requiring_them() {
    let mut graph = owned_parent_graph();
    graph
        .command(SetEnabled {
            key: "parent:one".into(),
            value: true,
        })
        .unwrap();
    let _parent = graph.demand::<OwnedParent>("one".into()).unwrap();
    assert_eq!(graph.get::<OwnedChildOutput>("child".into()), Some(true));
    assert_eq!(
        graph.get::<OwnedChildExtra>("child".into()),
        Some("extra:child".into())
    );
    assert!(graph.contains::<OwnedChildRelation>("child".into()));

    graph
        .command(SetEnabled {
            key: "parent:one".into(),
            value: false,
        })
        .unwrap();

    assert_eq!(graph.get::<OwnedChildOutput>("child".into()), None);
    assert_eq!(graph.get::<OwnedChildExtra>("child".into()), None);
    assert!(!graph.contains::<OwnedChildRelation>("child".into()));
}

#[test]
fn shared_child_is_reclaimed_only_after_its_final_parent_releases_it() {
    let mut graph = owned_parent_graph();
    let mut parents = Vec::new();
    for key in ["one", "two"] {
        graph
            .command(SetEnabled {
                key: format!("parent:{key}"),
                value: true,
            })
            .unwrap();
        parents.push(graph.demand::<OwnedParent>(key.into()).unwrap());
    }
    assert_eq!(graph.get::<OwnedChildOutput>("child".into()), Some(true));

    graph
        .command(SetEnabled {
            key: "parent:one".into(),
            value: false,
        })
        .unwrap();
    assert_eq!(graph.get::<OwnedChildOutput>("child".into()), Some(true));

    graph
        .command(SetEnabled {
            key: "parent:two".into(),
            value: false,
        })
        .unwrap();
    assert_eq!(graph.get::<OwnedChildOutput>("child".into()), None);
}

#[test]
fn root_pin_keeps_child_alive_after_parent_releases_it() {
    let mut graph = owned_parent_graph();
    graph
        .command(SetEnabled {
            key: "parent:one".into(),
            value: true,
        })
        .unwrap();
    let parent = graph.demand::<OwnedParent>("one".into()).unwrap();
    let child = graph.demand::<OwnedChild>("child".into()).unwrap();

    drop(parent);
    graph.collect_garbage().unwrap();
    assert_eq!(graph.get::<OwnedParentOutput>("one".into()), None);
    assert_eq!(graph.get::<OwnedChildOutput>("child".into()), Some(true));

    drop(child);
    graph.collect_garbage().unwrap();
    assert_eq!(graph.get::<OwnedChildOutput>("child".into()), None);
}

struct IndexedNames;
impl Relation for IndexedNames {
    type Fact = String;
}

impl IndexedRelation for IndexedNames {
    type Index = String;

    fn index(fact: &Self::Fact) -> Self::Index {
        fact.chars().next().unwrap().to_string()
    }
}

struct IndexedSupportOutput;
impl View for IndexedSupportOutput {
    type Key = String;
    type Value = bool;
}

struct IndexedSupport;
impl NodeProvider for IndexedSupport {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![
                PortDeclaration::map::<IndexedSupportOutput>(),
                PortDeclaration::indexed_set::<IndexedNames>(),
            ],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        let enabled = cx
            .get::<Enabled>(key.clone())
            .ok_or_else(NodeError::missing_view::<Enabled>)?;
        if enabled {
            cx.emit_relation::<IndexedNames>(key.clone())?;
        }
        cx.emit::<IndexedSupportOutput>(key, enabled)
    }
}

struct BucketCount;
impl View for BucketCount {
    type Key = String;
    type Value = usize;
}

struct ObserveBucket {
    runs: ComponentState<usize>,
}

impl NodeProvider for ObserveBucket {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![PortDeclaration::map::<BucketCount>()],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        *cx.state_mut(&self.runs)? += 1;
        let count = cx.scan::<IndexedNames>(key.clone()).len();
        cx.emit::<BucketCount>(key, count)
    }
}

#[test]
fn indexed_relation_invalidates_only_observed_buckets_including_empty_ones() {
    let runs = ComponentState::new(0usize);
    let mut graph = Graph::new();
    graph.install(IndexedSupport).unwrap();
    graph.install(ObserveBucket { runs: runs.clone() }).unwrap();
    for (key, value) in [("a", true), ("b", false), ("z", false)] {
        graph
            .command(SetEnabled {
                key: key.into(),
                value,
            })
            .unwrap();
    }
    let mut supports = Vec::new();
    for key in ["a", "b", "z"] {
        supports.push(graph.demand::<IndexedSupport>(key.into()).unwrap());
    }

    let _observed_a = graph.demand::<ObserveBucket>("a".into()).unwrap();
    let _observed_z = graph.demand::<ObserveBucket>("z".into()).unwrap();
    assert_eq!(graph.get::<BucketCount>("a".into()), Some(1));
    assert_eq!(graph.get::<BucketCount>("z".into()), Some(0));
    assert_eq!(runs.get().unwrap(), 2);

    graph
        .command(SetEnabled {
            key: "b".into(),
            value: true,
        })
        .unwrap();
    assert_eq!(runs.get().unwrap(), 2);

    graph
        .command(SetEnabled {
            key: "z".into(),
            value: true,
        })
        .unwrap();
    assert_eq!(runs.get().unwrap(), 3);
    assert_eq!(graph.get::<BucketCount>("z".into()), Some(1));
}

#[test]
fn indexed_relation_removal_invalidates_a_nonempty_observed_bucket() {
    let runs = ComponentState::new(0usize);
    let mut graph = Graph::new();
    graph.install(IndexedSupport).unwrap();
    graph.install(ObserveBucket { runs: runs.clone() }).unwrap();
    let mut supports = Vec::new();
    for key in ["apple", "apricot"] {
        graph
            .command(SetEnabled {
                key: key.into(),
                value: true,
            })
            .unwrap();
        supports.push(graph.demand::<IndexedSupport>(key.into()).unwrap());
    }

    let _observed = graph.demand::<ObserveBucket>("a".into()).unwrap();
    assert_eq!(graph.get::<BucketCount>("a".into()), Some(2));
    assert_eq!(runs.get().unwrap(), 1);

    graph
        .command(SetEnabled {
            key: "apple".into(),
            value: false,
        })
        .unwrap();
    assert_eq!(runs.get().unwrap(), 2);
    assert_eq!(graph.get::<BucketCount>("a".into()), Some(1));
}

struct RootShadow;
impl NodeProvider for RootShadow {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(type_name::<Self>(), vec![PortDeclaration::map::<Text>()])
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        cx.emit::<Text>(key, "derived".into())
    }
}

#[test]
fn derived_outputs_cannot_take_over_root_facts() {
    let mut graph = Graph::new();
    graph.install(RootShadow).unwrap();
    graph
        .command(SetText {
            key: "document".into(),
            value: "root".into(),
        })
        .unwrap();

    assert!(matches!(
        graph.demand::<RootShadow>("document".into()),
        Err(NodeError::OutputRootConflict(_))
    ));
    assert_eq!(graph.get::<Text>("document".into()), Some("root".into()));
}

struct OptionalText;
impl View for OptionalText {
    type Key = String;
    type Value = bool;
}

struct ObserveOptionalText;
impl NodeProvider for ObserveOptionalText {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![PortDeclaration::map::<OptionalText>()],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        let present = cx.get::<Text>(key.clone()).is_some();
        cx.emit::<OptionalText>(key, present)
    }
}

#[test]
fn missing_view_observations_invalidate_when_the_view_appears() {
    let mut graph = Graph::new();
    graph.install(ObserveOptionalText).unwrap();
    let _observed = graph
        .demand::<ObserveOptionalText>("document".into())
        .unwrap();
    assert_eq!(graph.get::<OptionalText>("document".into()), Some(false));

    graph
        .command(SetText {
            key: "document".into(),
            value: "now present".into(),
        })
        .unwrap();
    assert_eq!(graph.get::<OptionalText>("document".into()), Some(true));
}

#[test]
fn relation_command_effect_runs_in_a_follow_up_transaction() {
    let mut graph = Graph::new();
    graph.install(FirstSupport).unwrap();
    graph
        .command(SetEnabled {
            key: "first:name".into(),
            value: true,
        })
        .unwrap();
    graph.on_relation_added_command::<SharedName, SetText>(|_, fact| {
        Ok(SetText {
            key: "effect".into(),
            value: fact,
        })
    });

    let _support = graph.demand::<FirstSupport>("name".into()).unwrap();
    assert_eq!(graph.get::<Text>("effect".into()), Some("name".into()));
}

struct DeclaredPort;
impl View for DeclaredPort {
    type Key = String;
    type Value = bool;
}

struct UndeclaredPort;
impl View for UndeclaredPort {
    type Key = String;
    type Value = bool;
}

struct UndeclaredProvider;
impl NodeProvider for UndeclaredProvider {
    type Key = String;

    fn schema() -> NodeSchema {
        NodeSchema::new(
            type_name::<Self>(),
            vec![PortDeclaration::map::<DeclaredPort>()],
        )
    }

    fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<(), NodeError> {
        cx.emit::<UndeclaredPort>(key, true)
    }
}

#[test]
fn instance_inspection_reports_liveness_and_publications() {
    let mut graph = Graph::new();
    graph.install(Tokenize).unwrap();
    graph
        .command(SetText {
            key: "document".into(),
            value: "one two".into(),
        })
        .unwrap();
    let demand = graph.demand::<Tokenize>("document".into()).unwrap();
    let inspection = graph.inspect::<Tokenize>("document".into());
    assert!(inspection.materialized);
    assert_eq!(inspection.root_pins, 1);
    assert_eq!(inspection.publications, 1);
    assert_eq!(inspection.dependencies, 1);
    drop(demand);
}

#[test]
fn schema_edges_expose_provider_publications() {
    let mut graph = Graph::new();
    graph.install(Tokenize).unwrap();
    assert!(graph.definition_edges().iter().any(|edge| {
        edge.from == type_name::<Tokenize>()
            && edge.to == type_name::<Tokens>()
            && edge.kind == EdgeKind::Publishes
    }));
}

#[test]
fn schemas_reject_undeclared_publications() {
    let mut graph = Graph::new();
    graph.install(UndeclaredProvider).unwrap();
    assert!(matches!(
        graph.demand::<UndeclaredProvider>("document".into()),
        Err(NodeError::UndeclaredPort { kind: "map", .. })
    ));
}

#[test]
fn dropped_subscriptions_are_removed_without_waiting_for_a_view_change() {
    let mut graph = Graph::new();
    graph
        .command(SetText {
            key: "document".into(),
            value: "text".into(),
        })
        .unwrap();
    let subscription = graph.subscribe::<Text>("document".into()).unwrap();
    assert_eq!(graph.subscriber_count(), 1);
    drop(subscription);
    graph.collect_garbage().unwrap();
    assert_eq!(graph.subscriber_count(), 0);
}
