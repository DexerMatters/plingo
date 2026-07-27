use super::*;
use std::{
    any::{TypeId, type_name},
    sync::{Arc, Mutex, mpsc},
};

#[cfg(test)]
mod tests {
    use super::*;

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
    impl Node for Tokenize {
        type Key = String;
        type Output = Tokens;

        fn derive(
            &self,
            cx: &mut DeriveCx<'_, '_>,
            key: Self::Key,
        ) -> Result<Vec<String>, NodeError> {
            Ok(cx
                .observe::<Text>(key)?
                .split_whitespace()
                .map(str::to_owned)
                .collect())
        }
    }

    struct Count;
    impl View for Count {
        type Key = String;
        type Value = usize;
    }

    struct CountTokens;
    impl Node for CountTokens {
        type Key = String;
        type Output = Count;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<usize, NodeError> {
            Ok(cx.require::<Tokenize>(key)?.len())
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

        let subscription = graph.subscribe::<CountTokens>("document".into()).unwrap();
        assert_eq!(
            subscription.recv().unwrap(),
            ViewUpdate::Initial {
                snapshot: graph.revision(),
                value: 2,
            }
        );

        let before = graph.snapshot();
        graph
            .command(SetText {
                key: "document".into(),
                value: "one two three".into(),
            })
            .unwrap();

        assert_eq!(graph.read_at::<Count>(&before, "document".into()), Some(2));
        assert_eq!(graph.read::<Count>("document".into()), Some(3));
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
    impl Node for FailingNode {
        type Key = String;
        type Output = Failing;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<String, NodeError> {
            let text = cx.observe::<Text>(key)?;
            if text == "fail" {
                return Err(NodeError::message("expected failure"));
            }
            Ok(text)
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
        let subscription = graph.subscribe::<FailingNode>("document".into()).unwrap();
        assert!(matches!(
            subscription.recv().unwrap(),
            ViewUpdate::Initial { .. }
        ));
        let snapshot = graph.snapshot();

        let result = graph.command(SetText {
            key: "document".into(),
            value: "fail".into(),
        });
        assert!(result.is_err());
        assert_eq!(graph.read::<Text>("document".into()), Some("ok".into()));
        assert_eq!(
            graph.read_at::<Text>(&snapshot, "document".into()),
            Some("ok".into())
        );
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
    impl Node for FirstSupport {
        type Key = String;
        type Output = FirstOutput;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<bool, NodeError> {
            let enabled = cx.observe::<Enabled>(format!("first:{key}"))?;
            if enabled {
                cx.emit_relation::<SharedName>(key.clone())?;
            }
            Ok(enabled)
        }
    }

    struct SecondSupport;
    impl Node for SecondSupport {
        type Key = String;
        type Output = SecondOutput;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<bool, NodeError> {
            let enabled = cx.observe::<Enabled>(format!("second:{key}"))?;
            if enabled {
                cx.emit_relation::<SharedName>(key.clone())?;
            }
            Ok(enabled)
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

        let _first = graph.request::<FirstSupport>("name".into()).unwrap();
        let _second = graph.request::<SecondSupport>("name".into()).unwrap();
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

        graph.request::<FirstSupport>("name".into()).unwrap();

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
    impl Node for RejectEffects {
        type Key = String;
        type Output = RejectEffectsOutput;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<bool, NodeError> {
            let enabled = cx.require::<FirstSupport>(key)?;
            if enabled {
                return Err(NodeError::message("reject relation effect transaction"));
            }
            Ok(enabled)
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
        let _reject = graph.request::<RejectEffects>("name".into()).unwrap();
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

        graph.request::<FirstSupport>("name".into()).unwrap();

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

    impl Node for StatefulNode {
        type Key = String;
        type Output = StatefulOutput;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<usize, NodeError> {
            let text = cx.observe::<Text>(key)?;
            if text == "fail" {
                *cx.state_mut(&self.state)? += 1;
            }
            Ok(text.len())
        }
    }

    struct RejectStatefulOutput;
    impl View for RejectStatefulOutput {
        type Key = String;
        type Value = usize;
    }

    struct RejectAfterStateful;
    impl Node for RejectAfterStateful {
        type Key = String;
        type Output = RejectStatefulOutput;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<usize, NodeError> {
            let value = cx.require::<StatefulNode>(key)?;
            if value == "fail".len() {
                return Err(NodeError::message("dependent failure"));
            }
            Ok(value)
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
            .request::<RejectAfterStateful>("document".into())
            .unwrap();

        let result = graph.command(SetText {
            key: "document".into(),
            value: "fail".into(),
        });

        assert!(
            matches!(result, Err(NodeError::Message(message)) if message == "dependent failure")
        );
        assert_eq!(state.get().unwrap(), 0);
        assert_eq!(graph.read::<Text>("document".into()), Some("ok".into()));
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
    impl Node for OwnedChild {
        type Key = String;
        type Output = OwnedChildOutput;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<bool, NodeError> {
            cx.emit::<OwnedChildExtra>(key.clone(), format!("extra:{key}"))?;
            cx.emit_relation::<OwnedChildRelation>(key)?;
            Ok(true)
        }
    }

    struct OwnedParentOutput;
    impl View for OwnedParentOutput {
        type Key = String;
        type Value = bool;
    }

    struct OwnedParent;
    impl Node for OwnedParent {
        type Key = String;
        type Output = OwnedParentOutput;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<bool, NodeError> {
            let enabled = cx.observe::<Enabled>(format!("parent:{key}"))?;
            if enabled {
                cx.require::<OwnedChild>("child".into())?;
            }
            Ok(enabled)
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
        let _parent = graph.request::<OwnedParent>("one".into()).unwrap();
        assert_eq!(graph.read::<OwnedChildOutput>("child".into()), Some(true));
        assert_eq!(
            graph.read::<OwnedChildExtra>("child".into()),
            Some("extra:child".into())
        );
        assert!(graph.contains::<OwnedChildRelation>("child".into()));

        graph
            .command(SetEnabled {
                key: "parent:one".into(),
                value: false,
            })
            .unwrap();

        assert_eq!(graph.read::<OwnedChildOutput>("child".into()), None);
        assert_eq!(graph.read::<OwnedChildExtra>("child".into()), None);
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
            parents.push(graph.request::<OwnedParent>(key.into()).unwrap());
        }
        assert_eq!(graph.read::<OwnedChildOutput>("child".into()), Some(true));

        graph
            .command(SetEnabled {
                key: "parent:one".into(),
                value: false,
            })
            .unwrap();
        assert_eq!(graph.read::<OwnedChildOutput>("child".into()), Some(true));

        graph
            .command(SetEnabled {
                key: "parent:two".into(),
                value: false,
            })
            .unwrap();
        assert_eq!(graph.read::<OwnedChildOutput>("child".into()), None);
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
        let parent = graph.request::<OwnedParent>("one".into()).unwrap();
        let child = graph.request::<OwnedChild>("child".into()).unwrap();

        drop(parent);
        graph.collect_garbage().unwrap();
        assert_eq!(graph.read::<OwnedParentOutput>("one".into()), None);
        assert_eq!(graph.read::<OwnedChildOutput>("child".into()), Some(true));

        drop(child);
        graph.collect_garbage().unwrap();
        assert_eq!(graph.read::<OwnedChildOutput>("child".into()), None);
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
    impl Node for IndexedSupport {
        type Key = String;
        type Output = IndexedSupportOutput;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<bool, NodeError> {
            let enabled = cx.observe::<Enabled>(key.clone())?;
            if enabled {
                cx.emit_relation::<IndexedNames>(key)?;
            }
            Ok(enabled)
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

    impl Node for ObserveBucket {
        type Key = String;
        type Output = BucketCount;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<usize, NodeError> {
            *cx.state_mut(&self.runs)? += 1;
            Ok(cx.relation_facts_at::<IndexedNames>(key).len())
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
            supports.push(graph.request::<IndexedSupport>(key.into()).unwrap());
        }

        let observed_a = graph.request::<ObserveBucket>("a".into()).unwrap();
        let observed_z = graph.request::<ObserveBucket>("z".into()).unwrap();
        assert_eq!(*observed_a.value(), 1);
        assert_eq!(*observed_z.value(), 0);
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
        assert_eq!(graph.read::<BucketCount>("z".into()), Some(1));
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
            supports.push(graph.request::<IndexedSupport>(key.into()).unwrap());
        }

        let observed = graph.request::<ObserveBucket>("a".into()).unwrap();
        assert_eq!(*observed.value(), 2);
        assert_eq!(runs.get().unwrap(), 1);

        graph
            .command(SetEnabled {
                key: "apple".into(),
                value: false,
            })
            .unwrap();
        assert_eq!(runs.get().unwrap(), 2);
        assert_eq!(graph.read::<BucketCount>("a".into()), Some(1));
    }

    struct RootShadow;
    impl Node for RootShadow {
        type Key = String;
        type Output = Text;

        fn derive(&self, _cx: &mut DeriveCx<'_, '_>, _key: Self::Key) -> Result<String, NodeError> {
            Ok("derived".into())
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
            graph.request::<RootShadow>("document".into()),
            Err(NodeError::OutputRootConflict(_))
        ));
        assert_eq!(graph.read::<Text>("document".into()), Some("root".into()));
    }

    struct OptionalText;
    impl View for OptionalText {
        type Key = String;
        type Value = bool;
    }

    struct ObserveOptionalText;
    impl Node for ObserveOptionalText {
        type Key = String;
        type Output = OptionalText;

        fn derive(&self, cx: &mut DeriveCx<'_, '_>, key: Self::Key) -> Result<bool, NodeError> {
            match cx.observe::<Text>(key) {
                Ok(_) => Ok(true),
                Err(NodeError::MissingView(_)) => Ok(false),
                Err(error) => Err(error),
            }
        }
    }

    #[test]
    fn missing_view_observations_invalidate_when_the_view_appears() {
        let mut graph = Graph::new();
        graph.install(ObserveOptionalText).unwrap();
        let observed = graph
            .request::<ObserveOptionalText>("document".into())
            .unwrap();
        assert!(!*observed.value());

        graph
            .command(SetText {
                key: "document".into(),
                value: "now present".into(),
            })
            .unwrap();
        assert_eq!(graph.read::<OptionalText>("document".into()), Some(true));
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

        let _support = graph.request::<FirstSupport>("name".into()).unwrap();
        assert_eq!(graph.read::<Text>("effect".into()), Some("name".into()));
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
        let subscription = graph.subscribe_view::<Text>("document".into()).unwrap();
        assert_eq!(graph.subscriber_count(), 1);
        drop(subscription);
        graph.collect_garbage().unwrap();
        assert_eq!(graph.subscriber_count(), 0);
    }
}
