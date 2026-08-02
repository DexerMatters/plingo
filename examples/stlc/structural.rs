//! User-authored components that publish composable STLC structural views.

use std::{marker::PhantomData, sync::Arc};

use fluent_uri::Uri;
use plingo::{
    Component, Context, NodeError, Result, Table,
    component::writes,
    component::{
        parse::{AstKey, AstWalk},
        structural::{
            ChildRef, NoEdge, OrderedChildren, Structure, StructureChildren, StructureEntries,
            StructureEntry, StructureNode,
        },
    },
    scheme::node::Graph,
};

use super::syntax::{StlcDeclaration, StlcDocument, StlcExpr, StlcToken, StlcType};

/// A compact typed classification of parser artifacts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcNodeKind {
    Document,
    Declaration,
    Expression,
    Type,
    Other,
}

/// Structural product of the parser-to-index component.
pub struct StlcNodeIndex(PhantomData<fn() -> ()>);

impl Structure for StlcNodeIndex {
    type NodeKey = AstKey;
    type NodeMetadata = ();
    type Edge = NoEdge<Self>;
    type Topology = OrderedChildren;
}

/// Structural product of the index-to-summary component.
pub struct StlcNodeSummary(PhantomData<fn() -> ()>);

impl Structure for StlcNodeSummary {
    type NodeKey = AstKey;
    type NodeMetadata = ();
    type Edge = NoEdge<Self>;
    type Topology = OrderedChildren;
}

/// The one document coordinator for the parser-to-index component.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IndexCoordinator {
    pub uri: Uri<&'static str>,
}

/// One indexed AST item.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IndexTask {
    pub ast: AstKey,
}

impl Component for IndexCoordinator {
    type Output = ();
    type Writes = writes!(StructureEntries<StlcNodeIndex, Uri<&'static str>, usize>);

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<()> {
        let uri = self.uri;
        let mut parsed = cx.view::<plingo::component::Parsed<StlcToken, StlcDocument>>();
        let entries = parsed.entries(uri);
        if let Some(candidates) = parsed.candidates::<StlcDocument>(uri) {
            assert_eq!(
                candidates.len(),
                entries.len(),
                "parser candidates and structural entries describe one parse",
            );
        }
        drop(parsed);
        for entry in entries {
            cx.keep(IndexTask {
                ast: entry.node.clone(),
            });
            cx.view::<StlcNodeIndex>()
                .support_entry::<Uri<&'static str>, usize>(StructureEntry::new(
                    entry.entry,
                    entry.node,
                    entry.metadata,
                ))?;
        }
        Ok(())
    }
}

impl Component for IndexTask {
    type Output = Option<AstKey>;
    type Writes = writes!(
        StructureNode<StlcNodeIndex>,
        StructureChildren<StlcNodeIndex>,
    );

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<Self::Output> {
        let ast = self.ast.clone();
        let Some(artifact) = cx
            .view::<plingo::component::Parsed<StlcToken, StlcDocument>>()
            .raw_artifact(ast.clone())
        else {
            return Ok(None);
        };
        if artifact.deref::<StlcDocument>().is_some() {
            let mut children = Vec::new();
            let document = artifact.deref::<StlcDocument>().expect("document artifact");
            document.direct_children(&mut |child| children.push(child));
            cx.view::<StlcNodeIndex>()
                .define_artifact(ast.clone(), StlcNodeKind::Document)?;
            cx.view::<StlcNodeIndex>().define_children(
                ast.clone(),
                children
                    .iter()
                    .cloned()
                    .enumerate()
                    .map(|(slot, target)| ChildRef { slot, target })
                    .collect::<Vec<_>>()
                    .into(),
            )?;
            for child in children {
                cx.keep(IndexTask { ast: child });
            }
            return Ok(Some(ast));
        }
        let kind = if artifact.deref::<StlcDeclaration>().is_some() {
            StlcNodeKind::Declaration
        } else if artifact.deref::<StlcExpr>().is_some() {
            StlcNodeKind::Expression
        } else if artifact.deref::<StlcType>().is_some() {
            StlcNodeKind::Type
        } else {
            StlcNodeKind::Other
        };
        cx.view::<StlcNodeIndex>()
            .define_artifact(ast.clone(), kind)?;
        Ok(Some(ast))
    }
}

/// The one document coordinator for the index-to-summary component.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SummaryCoordinator {
    pub uri: Uri<&'static str>,
}

/// One summarized indexed item.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SummaryTask {
    pub ast: AstKey,
}

impl Component for SummaryCoordinator {
    type Output = ();
    type Writes = writes!(StructureEntries<StlcNodeSummary, Uri<&'static str>, usize>);

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<()> {
        cx.call(IndexCoordinator { uri: self.uri })?;
        for entry in cx
            .view::<StlcNodeIndex>()
            .entries::<Uri<&'static str>, usize>(self.uri)
        {
            cx.keep(SummaryTask {
                ast: entry.node.clone(),
            });
            cx.view::<StlcNodeSummary>()
                .support_entry::<Uri<&'static str>, usize>(StructureEntry::new(
                    entry.entry,
                    entry.node,
                    entry.metadata,
                ))?;
        }
        Ok(())
    }
}

impl Component for SummaryTask {
    type Output = Option<AstKey>;
    type Writes = writes!(
        StructureNode<StlcNodeSummary>,
        StructureChildren<StlcNodeSummary>,
    );

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<Self::Output> {
        let ast = self.ast.clone();
        cx.call(IndexTask { ast: ast.clone() })?;
        let Some(artifact) = cx
            .view::<StlcNodeIndex>()
            .artifact::<StlcNodeKind>(ast.clone())
        else {
            return Ok(None);
        };
        let kind = &*artifact;
        cx.view::<StlcNodeSummary>()
            .define_artifact(ast.clone(), format!("{kind:?}"))?;
        if let Some(children) = cx.view::<StlcNodeIndex>().children(ast.clone()) {
            cx.view::<StlcNodeSummary>()
                .define_children(ast.clone(), Arc::clone(&children))?;
            for child in children.iter() {
                cx.keep(SummaryTask {
                    ast: child.target.clone(),
                });
            }
        }
        Ok(Some(ast))
    }
}

/// Untyped lowered structural product derived from the indexed AST view.
pub struct StlcLowered(PhantomData<fn() -> ()>);

impl Structure for StlcLowered {
    type NodeKey = AstKey;
    type NodeMetadata = ();
    type Edge = NoEdge<Self>;
    type Topology = OrderedChildren;
}

/// Origin of one lowered node in the source AST.
pub struct StlcLoweredOrigin;

impl plingo::View for StlcLoweredOrigin {
    type Key = AstKey;
    type Value = AstKey;
}

/// Lowering diagnostics owned by one AST item.
pub struct StlcLoweringDiagnostics;

impl plingo::View for StlcLoweringDiagnostics {
    type Key = AstKey;
    type Value = Arc<[String]>;
}

/// The one document coordinator for the lowering component.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LowerCoordinator {
    pub uri: Uri<&'static str>,
}

/// One lowered indexed item.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LowerTask {
    pub ast: AstKey,
}

impl Component for LowerCoordinator {
    type Output = ();
    type Writes = writes!(StructureEntries<StlcLowered, Uri<&'static str>, usize>);

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<()> {
        cx.call(IndexCoordinator { uri: self.uri })?;
        for entry in cx
            .view::<StlcNodeIndex>()
            .entries::<Uri<&'static str>, usize>(self.uri)
        {
            cx.keep(LowerTask {
                ast: entry.node.clone(),
            });
            cx.view::<StlcLowered>()
                .support_entry::<Uri<&'static str>, usize>(StructureEntry::new(
                    entry.entry,
                    entry.node,
                    entry.metadata,
                ))?;
        }
        Ok(())
    }
}

impl Component for LowerTask {
    type Output = Option<AstKey>;
    type Writes = writes!(
        StructureNode<StlcLowered>,
        StructureChildren<StlcLowered>,
        Table<StlcLoweredOrigin>,
        Table<StlcLoweringDiagnostics>,
    );

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<Self::Output> {
        let ast = self.ast.clone();
        cx.call(IndexTask { ast: ast.clone() })?;
        let Some(index) = cx
            .view::<StlcNodeIndex>()
            .artifact::<StlcNodeKind>(ast.clone())
        else {
            return Ok(None);
        };
        let kind = &*index;
        let lowered = format!("untyped::{kind:?}");
        cx.view::<StlcLowered>()
            .define_artifact(ast.clone(), lowered)?;
        cx.view::<Table<StlcLoweredOrigin>>()
            .set(ast.clone(), ast.clone())?;
        let diagnostics: Arc<[String]> = if matches!(&*kind, StlcNodeKind::Other) {
            vec![format!("unclassified source node {}", ast.id)].into()
        } else {
            Vec::new().into()
        };
        cx.view::<Table<StlcLoweringDiagnostics>>()
            .set(ast.clone(), diagnostics)?;
        if let Some(children) = cx.view::<StlcNodeIndex>().children(ast.clone()) {
            cx.view::<StlcLowered>()
                .define_children(ast.clone(), Arc::clone(&children))?;
            for child in children.iter() {
                cx.keep(LowerTask {
                    ast: child.target.clone(),
                });
            }
        }
        Ok(Some(ast))
    }
}

/// A downstream consumer proving that lowered structural views compose.
pub struct StlcLoweredSummary(PhantomData<fn() -> ()>);

impl Structure for StlcLoweredSummary {
    type NodeKey = AstKey;
    type NodeMetadata = ();
    type Edge = NoEdge<Self>;
    type Topology = OrderedChildren;
}

/// The one document coordinator for the lowered-summary component.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LoweredSummaryCoordinator {
    pub uri: Uri<&'static str>,
}

/// One summarized lowered item.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LoweredSummaryTask {
    pub ast: AstKey,
}

impl Component for LoweredSummaryCoordinator {
    type Output = ();
    type Writes = writes!(StructureEntries<StlcLoweredSummary, Uri<&'static str>, usize>);

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<()> {
        cx.call(LowerCoordinator { uri: self.uri })?;
        for entry in cx
            .view::<StlcLowered>()
            .entries::<Uri<&'static str>, usize>(self.uri)
        {
            cx.keep(LoweredSummaryTask {
                ast: entry.node.clone(),
            });
            cx.view::<StlcLoweredSummary>()
                .support_entry::<Uri<&'static str>, usize>(StructureEntry::new(
                    entry.entry,
                    entry.node,
                    entry.metadata,
                ))?;
        }
        Ok(())
    }
}

impl Component for LoweredSummaryTask {
    type Output = Option<AstKey>;
    type Writes = writes!(StructureNode<StlcLoweredSummary>);

    fn run(&self, cx: &mut Context<'_, Self>) -> Result<Self::Output> {
        let ast = self.ast.clone();
        cx.call(LowerTask { ast: ast.clone() })?;
        let Some(value) = cx.view::<StlcLowered>().artifact::<String>(ast.clone()) else {
            return Ok(None);
        };
        cx.view::<StlcLoweredSummary>()
            .define_artifact(ast.clone(), format!("summary:{value}"))?;
        Ok(Some(ast))
    }
}

/// Installs parser, indexed, summary, lowering, and downstream structural
/// components, connecting each coordinator after its source publishes.
pub fn install_structural_pipeline(graph: &mut Graph) -> std::result::Result<(), NodeError> {
    graph.register::<IndexCoordinator>()?;
    graph.register::<IndexTask>()?;
    graph.register::<SummaryCoordinator>()?;
    graph.register::<SummaryTask>()?;
    graph.register::<LowerCoordinator>()?;
    graph.register::<LowerTask>()?;
    graph.register::<LoweredSummaryCoordinator>()?;
    graph.register::<LoweredSummaryTask>()?;
    graph.connect_component::<plingo::component::parse::ParserNode<StlcToken, StlcDocument>, IndexCoordinator>(
        |uri| IndexCoordinator { uri },
    )?;
    graph.connect_components::<IndexCoordinator, SummaryCoordinator>(|coordinator| {
        SummaryCoordinator {
            uri: coordinator.uri,
        }
    })?;
    graph.connect_components::<IndexCoordinator, LowerCoordinator>(|coordinator| {
        LowerCoordinator {
            uri: coordinator.uri,
        }
    })?;
    graph.connect_components::<LowerCoordinator, LoweredSummaryCoordinator>(|coordinator| {
        LoweredSummaryCoordinator {
            uri: coordinator.uri,
        }
    })?;
    Ok(())
}
