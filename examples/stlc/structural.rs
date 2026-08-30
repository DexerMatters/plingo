//! Reactive structural products for the STLC syntax family.

use super::syntax::{
    StlcDeclaration, StlcDocument, StlcExpr, StlcParam, StlcPath, StlcType, StlcTypeAtom,
};
use plingo::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StlcNodeKind {
    Document,
    Declaration,
    Expression,
    Type,
    Other,
}

#[view]
pub struct StlcNodeIndex(Map<AstBox<()>, StlcNodeKind>);

#[view]
pub struct StlcLowered(Map<AstBox<()>, String>);

#[view]
pub struct StlcLoweredOrigin(Map<AstBox<()>, AstBox<()>>);

#[view]
pub struct StlcLoweringDiagnostics(List<AstBox<()>, String>);

#[view]
pub struct StlcLoweredSummary(Map<AstBox<()>, String>);

#[derive(Clone, Debug, PartialEq, Effects)]
struct StructuralEffects {
    index: Set<StlcNodeIndex>,
    lowered: Set<StlcLowered>,
    origin: Set<StlcLoweredOrigin>,
    diagnostics: Replace<StlcLoweringDiagnostics>,
    summary: Set<StlcLoweredSummary>,
}

fn structural_effect(id: AstBox<()>, kind: StlcNodeKind) -> StructuralEffects {
    let lowered = format!("untyped::{kind:?}");
    let diagnostics = if matches!(kind, StlcNodeKind::Other) {
        vec![format!("unclassified source node {id:?}")]
    } else {
        Vec::new()
    };
    StructuralEffects {
        index: StlcNodeIndex::set(id.clone(), kind),
        lowered: StlcLowered::set(id.clone(), lowered.clone()),
        origin: StlcLoweredOrigin::set(id.clone(), id.clone()),
        diagnostics: StlcLoweringDiagnostics::replace(id.clone(), diagnostics),
        summary: StlcLoweredSummary::set(id, format!("summary:{lowered}")),
    }
}

#[component]
pub fn structural_document(node: AstBox<StlcDocument>) -> Result<StructuralEffects> {
    Ok(structural_effect(node.erased(), StlcNodeKind::Document))
}

#[component]
pub fn structural_declaration(node: AstBox<StlcDeclaration>) -> Result<StructuralEffects> {
    Ok(structural_effect(node.erased(), StlcNodeKind::Declaration))
}

#[component]
pub fn structural_expression(node: AstBox<StlcExpr>) -> Result<StructuralEffects> {
    Ok(structural_effect(node.erased(), StlcNodeKind::Expression))
}

#[component]
pub fn structural_path(node: AstBox<StlcPath>) -> Result<StructuralEffects> {
    Ok(structural_effect(node.erased(), StlcNodeKind::Other))
}

#[component]
pub fn structural_parameter(node: AstBox<StlcParam>) -> Result<StructuralEffects> {
    Ok(structural_effect(node.erased(), StlcNodeKind::Other))
}

#[component]
pub fn structural_type(node: AstBox<StlcType>) -> Result<StructuralEffects> {
    Ok(structural_effect(node.erased(), StlcNodeKind::Type))
}

#[component]
pub fn structural_type_atom(node: AstBox<StlcTypeAtom>) -> Result<StructuralEffects> {
    Ok(structural_effect(node.erased(), StlcNodeKind::Type))
}
