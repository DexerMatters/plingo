use std::sync::Arc;

use crate::{
    Component, Context, ReadGraph, Result, Set, Table, View,
    component::structural::{StructuralArtifact, StructureEntry},
    component::writes,
    scheme::node::Graph,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Support {
    source: String,
    target: String,
}

impl crate::Relation for Support {
    type Fact = Support;
}

struct Lowered;

impl View for Lowered {
    type Key = String;
    type Value = StructuralArtifact<String>;
}

/// One lowering component keyed by its source string.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LoweringKey(String);

impl Component for LoweringKey {
    type Output = ();
    type Writes = writes!(Table<Lowered>, Set<Support>);

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<()> {
        cx.view::<Table<Lowered>>().set(
            self.0.clone(),
            StructuralArtifact::new(self.0.clone(), format!("lowered:{}", self.0)),
        )?;
        cx.view::<Set<Support>>().add(Support {
            source: self.0.clone(),
            target: "target".to_owned(),
        })?;
        Ok(())
    }
}

#[test]
fn structural_artifacts_keep_typed_values_and_entries() {
    let artifact = StructuralArtifact::new("root".to_owned(), 7_u32);
    assert_eq!(artifact.deref::<u32>(), Some(Arc::new(7)));
    assert_eq!(
        StructureEntry::new("document".to_owned(), "root".to_owned(), ()),
        StructureEntry::new("document".to_owned(), "root".to_owned(), ()),
    );
}

#[test]
fn components_publish_structural_views_and_relations() {
    let mut graph = Graph::new();
    graph.register::<LoweringKey>().unwrap();
    let _lease = graph.request(LoweringKey("root".to_owned())).unwrap();

    assert_eq!(
        graph
            .get::<Lowered>("root".to_owned())
            .and_then(|artifact| artifact.deref::<String>()),
        Some(Arc::new("lowered:root".to_owned())),
    );
    assert!(graph.contains::<Support>(Support {
        source: "root".to_owned(),
        target: "target".to_owned(),
    }));
}
