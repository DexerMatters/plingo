//! Reusable view-level tree-transform harness.
//!
//! `SurfacePrograms` is intentionally a tiny editable model. It publishes a
//! source tree, then `lower_view_pass` maps that source tree to a distinct,
//! heterogeneous core tree. The two stages meet only through public views:
//! `SurfaceRoots`, `SurfaceTree`, `CoreTree`, and `CoreOrigin`.
//!
//! This keeps dependency tests independent of a parser's current lineage
//! policy. A source payload update changes one `TreeKey::Payload`; a child
//! insertion changes one `TreeKey::ChildOrder` plus one link. The transform
//! mirrors those same smallest units in its target tree.

use std::sync::Arc;
use plingo::reactive::component::{EachKey, Read, Write};
use plingo::reactive::kind::{Map, Tree, TreeFact, TreeKey, TreeView, emit_view};
use plingo::reactive::prelude::*;
use plingo::reactive::view::Node;
use reactive_macros::{component, view};

/// Editable source fixture for one document.
///
/// The aggregate remains as a compatibility input for the original harness.
/// New callers should write the four granular source views below in one
/// command; the bridge component keeps both entry points equivalent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceProgram {
    pub left: i64,
    pub right_name: Option<String>,
}

/// Source forest payloads. The source is structurally distinct from the core
/// target: its expression is surface `Add`, not a semantic operation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SurfaceNode {
    Document,
    Binding,
    /// A binding name is a payload dimension separate from the fixed tree
    /// topology. Rendering intentionally keeps the historical `Binding` row.
    BindingName(String),
    Add,
    Number(i64),
    Name(String),
}

/// Target forest payloads. The lowered tree uses different roles and names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CoreNode {
    Module,
    LetBinding,
    Integer(i64),
    ApplyAdd,
    Reference(String),
}

/// Stable source membership. The key is the only lifecycle driver for the
/// document/add/binding/number components.
#[view]
pub struct ProgramMembership(Map<String, ()>);

/// Independently editable source dimensions.
#[view]
pub struct BindingNames(Map<String, String>);

#[view]
pub struct NumberValues(Map<String, i64>);

/// Absence removes the optional name subtree.
#[view]
pub struct ReferenceNames(Map<String, String>);

#[view]
pub struct SurfacePrograms(Map<String, SurfaceProgram>);

/// One stable source root per document.
#[view]
pub struct SurfaceRoots(Map<String, Node<SurfaceTree>>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SurfacePart {
    Root,
    Binding,
    Add,
    Number,
    Name,
}

/// The generated source-node identity join. It is intentionally a derived
/// map, not an editable identity table.
#[view]
pub struct SurfaceNodes(Map<(String, SurfacePart), Node<SurfaceTree>>);

#[view]
pub struct SurfaceTree(Tree<String, SurfaceNode>);

#[view]
pub struct CoreRoots(Map<String, Node<CoreTree>>);

#[view]
pub struct CoreNodes(Map<(String, SurfacePart), Node<CoreTree>>);

#[view]
pub struct CoreTree(Tree<String, CoreNode>);

#[view]
pub struct CoreOrigin(Map<Node<CoreTree>, Node<SurfaceTree>>);

// ---------------------------------------------------------------------------
// Granular source components
// ---------------------------------------------------------------------------

/// Compatibility bridge from the historical aggregate fixture to the
/// independent source dimensions. It owns only source inputs, never tree
/// facts, so direct writes to the granular views remain first-class.
#[component]
pub fn split_surface_program(
    key: EachKey<SurfacePrograms>,
    programs: Read<SurfacePrograms>,
    membership: Write<ProgramMembership>,
    bindings: Write<BindingNames>,
    numbers: Write<NumberValues>,
    references: Write<ReferenceNames>,
) -> Result<()> {
    let Some(program) = programs.get(&key)? else {
        return Ok(());
    };
    membership.insert(key.clone(), ())?;
    bindings.insert(key.clone(), program.right_name.clone().unwrap_or_default())?;
    numbers.insert(key.clone(), program.left)?;
    match &program.right_name {
        Some(name) => references.insert(key, name.clone()),
        None => references.remove(key),
    }
}

/// One source document owns the fixed tree topology and the identity join.
/// Payloads for binding, add, and number nodes are owned by separate
/// components so an editable dimension never republishes another payload.
#[component]
pub fn build_surface_root(
    key: EachKey<ProgramMembership>,
    roots: Write<SurfaceRoots>,
    nodes: Write<SurfaceNodes>,
) -> Result<()> {
    let uri = key.clone();
    let tree = emit_view::<SurfaceTree>()?;
    let root = tree.allocate()?;
    let binding = tree.allocate()?;
    let add = tree.allocate()?;
    let number = tree.allocate()?;

    tree.put(
        TreeKey::Payload(root.clone()),
        Some(TreeFact::Payload(SurfaceNode::Document)),
    )?;
    tree.put(
        TreeKey::Parent(root.clone()),
        Some(TreeFact::Parent(None)),
    )?;
    tree.put(
        TreeKey::ChildOrder(root.clone()),
        Some(TreeFact::Order(Arc::from(vec![binding.clone()]))),
    )?;
    tree.put(
        TreeKey::ChildLink(root.clone(), binding.clone()),
        Some(TreeFact::Link(binding.clone())),
    )?;
    tree.put(
        TreeKey::Parent(binding.clone()),
        Some(TreeFact::Parent(Some(root.clone()))),
    )?;
    tree.put(
        TreeKey::ChildOrder(binding.clone()),
        Some(TreeFact::Order(Arc::from(vec![add.clone()]))),
    )?;
    tree.put(
        TreeKey::ChildLink(binding.clone(), add.clone()),
        Some(TreeFact::Link(add.clone())),
    )?;
    tree.put(
        TreeKey::Parent(add.clone()),
        Some(TreeFact::Parent(Some(binding.clone()))),
    )?;
    tree.put(
        TreeKey::Parent(number.clone()),
        Some(TreeFact::Parent(Some(add.clone()))),
    )?;
    tree.put(
        TreeKey::ChildOrder(number.clone()),
        Some(TreeFact::Order(Arc::from(Vec::new()))),
    )?;
    tree.put(
        TreeKey::RootOrder(uri.clone()),
        Some(TreeFact::RootOrder(Arc::from(vec![root.clone()]))),
    )?;
    tree.put(
        TreeKey::RootLink(uri.clone(), root.clone()),
        Some(TreeFact::RootLink(root.clone())),
    )?;

    roots.insert(uri.clone(), root.clone())?;
    nodes.insert((uri.clone(), SurfacePart::Root), root)?;
    nodes.insert((uri.clone(), SurfacePart::Binding), binding)?;
    nodes.insert((uri.clone(), SurfacePart::Add), add)?;
    nodes.insert((uri, SurfacePart::Number), number)
}

/// The reference identity is a separate output slot. It creates no topology
/// or payload, and re-emits the existing identity on every reevaluation so
/// ownership survives exact input changes.
#[component]
pub fn surface_reference_identity(
    key: EachKey<ReferenceNames>,
    names: Read<ReferenceNames>,
    nodes: Read<SurfaceNodes>,
    node_writes: Write<SurfaceNodes>,
) -> Result<()> {
    let Some(_name) = names.get(&key)? else {
        return Ok(());
    };
    let Some(_add) = nodes.get(&(key.clone(), SurfacePart::Add))? else {
        return Ok(());
    };
    let node = match nodes.get(&(key.clone(), SurfacePart::Name))? {
        Some(node) => node.as_ref().clone(),
        None => emit_view::<SurfaceTree>()?.allocate()?,
    };
    node_writes.insert((key, SurfacePart::Name), node)
}

/// Owns the aggregate child order for the source add node. Payload and
/// optional-reference components therefore never compete for this fact.
#[component]
pub fn surface_child_edges(
    key: EachKey<ProgramMembership>,
    references: Read<ReferenceNames>,
    nodes: Read<SurfaceNodes>,
) -> Result<()> {
    let reference = references.get(&key)?;
    let Some(add) = nodes.get(&(key.clone(), SurfacePart::Add))? else {
        return Ok(());
    };
    let Some(number) = nodes.get(&(key.clone(), SurfacePart::Number))? else {
        return Ok(());
    };

    let tree = emit_view::<SurfaceTree>()?;
    let mut children = vec![number.as_ref().clone()];
    if reference.is_some()
        && let Some(name_node) = nodes.get(&(key.clone(), SurfacePart::Name))?
    {
        tree.put(
            TreeKey::Parent(name_node.as_ref().clone()),
            Some(TreeFact::Parent(Some(add.as_ref().clone()))),
        )?;
        tree.put(
            TreeKey::ChildOrder(name_node.as_ref().clone()),
            Some(TreeFact::Order(Arc::from(Vec::new()))),
        )?;
        children.push(name_node.as_ref().clone());
    }

    tree.put(
        TreeKey::ChildOrder(add.as_ref().clone()),
        Some(TreeFact::Order(Arc::from(
            children.iter().cloned().collect::<Vec<_>>(),
        ))),
    )?;
    for child in children {
        tree.put(
            TreeKey::ChildLink(add.as_ref().clone(), child.clone()),
            Some(TreeFact::Link(child)),
        )?;
    }
    Ok(())
}

/// The fixed source add payload has its own publication owner.
#[component]
pub fn surface_add_payload(
    key: EachKey<ProgramMembership>,
    nodes: Read<SurfaceNodes>,
) -> Result<()> {
    let Some(add) = nodes.get(&(key, SurfacePart::Add))? else {
        return Ok(());
    };
    emit_view::<SurfaceTree>()?.put(
        TreeKey::Payload(add.as_ref().clone()),
        Some(TreeFact::Payload(SurfaceNode::Add)),
    )
}

/// Binding text is a payload-only dimension. The optional map key is read
/// exactly; absence restores the fixed binding payload.
#[component]
pub fn surface_binding_payload(
    key: EachKey<ProgramMembership>,
    names: Read<BindingNames>,
    nodes: Read<SurfaceNodes>,
) -> Result<()> {
    let Some(binding) = nodes.get(&(key.clone(), SurfacePart::Binding))? else {
        return Ok(());
    };
    let payload = match names.get(&key)? {
        Some(name) => SurfaceNode::BindingName((*name).clone()),
        None => SurfaceNode::Binding,
    };
    emit_view::<SurfaceTree>()?.put(
        TreeKey::Payload(binding.as_ref().clone()),
        Some(TreeFact::Payload(payload)),
    )
}


// ---------------------------------------------------------------------------
// Granular target components
// ---------------------------------------------------------------------------

/// The target root component maps only the fixed source topology. Payload
/// components below update target leaves without reading source payloads.
#[component]
pub fn lower_view_root(
    key: EachKey<SurfaceRoots>,
    roots: Write<CoreRoots>,
    nodes: Write<CoreNodes>,
    source_nodes: Read<SurfaceNodes>,
) -> Result<()> {
    let uri = key.clone();
    let Some(source_root) = observe_view::<SurfaceRoots>()?.get(&uri)? else {
        return Ok(());
    };
    let source_root = source_root.as_ref().clone();
    let tree = emit_view::<CoreTree>()?;
    let root = tree.allocate()?;
    let binding = tree.allocate()?;
    let add = tree.allocate()?;
    let number = tree.allocate()?;

    tree.put(
        TreeKey::Payload(root.clone()),
        Some(TreeFact::Payload(CoreNode::Module)),
    )?;
    tree.put(
        TreeKey::Parent(root.clone()),
        Some(TreeFact::Parent(None)),
    )?;
    tree.put(
        TreeKey::ChildOrder(root.clone()),
        Some(TreeFact::Order(Arc::from(vec![binding.clone()]))),
    )?;
    tree.put(
        TreeKey::ChildLink(root.clone(), binding.clone()),
        Some(TreeFact::Link(binding.clone())),
    )?;
    tree.put(
        TreeKey::Parent(binding.clone()),
        Some(TreeFact::Parent(Some(root.clone()))),
    )?;
    tree.put(
        TreeKey::ChildOrder(binding.clone()),
        Some(TreeFact::Order(Arc::from(vec![add.clone()]))),
    )?;
    tree.put(
        TreeKey::ChildLink(binding.clone(), add.clone()),
        Some(TreeFact::Link(add.clone())),
    )?;
    tree.put(
        TreeKey::Parent(add.clone()),
        Some(TreeFact::Parent(Some(binding.clone()))),
    )?;
    tree.put(
        TreeKey::Parent(number.clone()),
        Some(TreeFact::Parent(Some(add.clone()))),
    )?;
    tree.put(
        TreeKey::ChildOrder(number.clone()),
        Some(TreeFact::Order(Arc::from(Vec::new()))),
    )?;
    tree.put(
        TreeKey::RootOrder(uri.clone()),
        Some(TreeFact::RootOrder(Arc::from(vec![root.clone()]))),
    )?;
    tree.put(
        TreeKey::RootLink(uri.clone(), root.clone()),
        Some(TreeFact::RootLink(root.clone())),
    )?;

    roots.insert(uri.clone(), root.clone())?;
    nodes.insert((uri.clone(), SurfacePart::Root), root.clone())?;
    nodes.insert((uri.clone(), SurfacePart::Binding), binding.clone())?;
    nodes.insert((uri.clone(), SurfacePart::Add), add.clone())?;
    nodes.insert((uri.clone(), SurfacePart::Number), number.clone())?;

    let origin = emit_view::<CoreOrigin>()?;
    for (part, target) in [
        (SurfacePart::Root, root),
        (SurfacePart::Binding, binding),
        (SurfacePart::Add, add),
        (SurfacePart::Number, number),
    ] {
        if let Some(source) = source_nodes.get(&(uri.clone(), part))? {
            origin.insert(target, source.as_ref().clone())?;
        }
    }
    // Keep the exact root join alive in the component dependency graph.
    let _ = source_root;
    Ok(())
}

/// The target reference identity has a separate automatic output slot.
#[component]
pub fn lower_reference_identity(
    key: EachKey<ReferenceNames>,
    names: Read<ReferenceNames>,
    source_nodes: Read<SurfaceNodes>,
    core_nodes: Read<CoreNodes>,
    node_writes: Write<CoreNodes>,
) -> Result<()> {
    let Some(_name) = names.get(&key)? else {
        return Ok(());
    };
    let Some(_source_add) = source_nodes.get(&(key.clone(), SurfacePart::Add))? else {
        return Ok(());
    };
    let Some(_core_add) = core_nodes.get(&(key.clone(), SurfacePart::Add))? else {
        return Ok(());
    };
    let node = match core_nodes.get(&(key.clone(), SurfacePart::Name))? {
        Some(node) => node.as_ref().clone(),
        None => emit_view::<CoreTree>()?.allocate()?,
    };
    node_writes.insert((key, SurfacePart::Name), node)
}

/// Owns the target add order and optional-reference topology.
#[component]
pub fn lower_child_edges(
    key: EachKey<ProgramMembership>,
    references: Read<ReferenceNames>,
    source_nodes: Read<SurfaceNodes>,
    core_nodes: Read<CoreNodes>,
) -> Result<()> {
    let reference = references.get(&key)?;
    let Some(source_add) = source_nodes.get(&(key.clone(), SurfacePart::Add))? else {
        return Ok(());
    };
    let Some(core_add) = core_nodes.get(&(key.clone(), SurfacePart::Add))? else {
        return Ok(());
    };
    let Some(core_number) = core_nodes.get(&(key.clone(), SurfacePart::Number))? else {
        return Ok(());
    };
    let tree = emit_view::<CoreTree>()?;
    let mut children = vec![core_number.as_ref().clone()];
    if reference.is_some()
        && let Some(name_node) = core_nodes.get(&(key.clone(), SurfacePart::Name))?
    {
        tree.put(
            TreeKey::Parent(name_node.as_ref().clone()),
            Some(TreeFact::Parent(Some(core_add.as_ref().clone()))),
        )?;
        tree.put(
            TreeKey::ChildOrder(name_node.as_ref().clone()),
            Some(TreeFact::Order(Arc::from(Vec::new()))),
        )?;
        children.push(name_node.as_ref().clone());
    }
    tree.put(
        TreeKey::ChildOrder(core_add.as_ref().clone()),
        Some(TreeFact::Order(Arc::from(
            children.iter().cloned().collect::<Vec<_>>(),
        ))),
    )?;
    for child in children {
        tree.put(
            TreeKey::ChildLink(core_add.as_ref().clone(), child.clone()),
            Some(TreeFact::Link(child)),
        )?;
    }
    let _ = source_add;
    Ok(())
}

/// Fixed target binding payload producer.
#[component]
pub fn lower_binding_payload(
    key: EachKey<ProgramMembership>,
    nodes: Read<CoreNodes>,
) -> Result<()> {
    let Some(binding) = nodes.get(&(key, SurfacePart::Binding))? else {
        return Ok(());
    };
    emit_view::<CoreTree>()?.put(
        TreeKey::Payload(binding.as_ref().clone()),
        Some(TreeFact::Payload(CoreNode::LetBinding)),
    )
}

/// Fixed target add payload producer.
#[component]
pub fn lower_add_payload(
    key: EachKey<ProgramMembership>,
    nodes: Read<CoreNodes>,
) -> Result<()> {
    let Some(add) = nodes.get(&(key, SurfacePart::Add))? else {
        return Ok(());
    };
    emit_view::<CoreTree>()?.put(
        TreeKey::Payload(add.as_ref().clone()),
        Some(TreeFact::Payload(CoreNode::ApplyAdd)),
    )
}

/// Number projection reads one source value and updates exactly two payload
/// facts. It does not read roots, children, or sibling payloads.
#[component]
pub fn lower_number_payload(
    key: EachKey<ProgramMembership>,
    numbers: Read<NumberValues>,
    source_nodes: Read<SurfaceNodes>,
    core_nodes: Read<CoreNodes>,
) -> Result<()> {
    let Some(source) = source_nodes.get(&(key.clone(), SurfacePart::Number))? else {
        return Ok(());
    };
    let Some(target) = core_nodes.get(&(key.clone(), SurfacePart::Number))? else {
        return Ok(());
    };
    let (source_payload, target_payload) = match numbers.get(&key)? {
        Some(value) => (
            Some(SurfaceNode::Number(*value)),
            Some(CoreNode::Integer(*value)),
        ),
        None => (None, None),
    };
    let source_tree = emit_view::<SurfaceTree>()?;
    let target_tree = emit_view::<CoreTree>()?;
    source_tree.put(
        TreeKey::Payload(source.as_ref().clone()),
        source_payload.map(TreeFact::Payload),
    )?;
    target_tree.put(
        TreeKey::Payload(target.as_ref().clone()),
        target_payload.map(TreeFact::Payload),
    )
}

/// Optional reference payload. Its instance exists only while the
/// reference-name membership key exists; topology and identity are owned by
/// the field-edge component.
#[component]
pub fn lower_reference_payload(
    key: EachKey<ReferenceNames>,
    names: Read<ReferenceNames>,
    source_nodes: Read<SurfaceNodes>,
    core_nodes: Read<CoreNodes>,
    origins: Write<CoreOrigin>,
) -> Result<()> {
    let Some(source_name) = source_nodes.get(&(key.clone(), SurfacePart::Name))? else {
        return Ok(());
    };
    let Some(core_name) = core_nodes.get(&(key.clone(), SurfacePart::Name))? else {
        return Ok(());
    };
    let Some(name) = names.get(&key)? else {
        return Ok(());
    };
    let name = (*name).clone();
    emit_view::<SurfaceTree>()?.put(
        TreeKey::Payload(source_name.as_ref().clone()),
        Some(TreeFact::Payload(SurfaceNode::Name(name.clone()))),
    )?;
    emit_view::<CoreTree>()?.put(
        TreeKey::Payload(core_name.as_ref().clone()),
        Some(TreeFact::Payload(CoreNode::Reference(name))),
    )?;
    origins.insert(core_name.as_ref().clone(), source_name.as_ref().clone())
}

/// Public installer retained for the harness while the generated definitions
/// remain individually addressable for component-graph tests.
pub fn build_surface_pass_install(engine: &mut plingo::reactive::Engine) -> plingo::Result<()> {
    split_surface_program_install(engine)?;
    build_surface_root_install(engine)?;
    surface_reference_identity_install(engine)?;
    surface_child_edges_install(engine)?;
    surface_add_payload_install(engine)?;
    surface_binding_payload_install(engine)?;
    lower_number_payload_install(engine)?;
    Ok(())
}

/// Installs the target-root, edge, and payload definitions.
pub fn lower_view_pass_install(engine: &mut plingo::reactive::Engine) -> plingo::Result<()> {
    lower_view_root_install(engine)?;
    lower_reference_identity_install(engine)?;
    lower_child_edges_install(engine)?;
    lower_binding_payload_install(engine)?;
    lower_add_payload_install(engine)?;
    lower_reference_payload_install(engine)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Semantic digest (follow-up plan §4 item 1): complete public-view content,
// ID-erased and canonically ordered. Tree rows are keyed by structural DFS
// paths (`surface:<uri>#0.1`), never by raw node ordinals, so a warm
// workspace and a cold rebuild produce identical digests.
// ---------------------------------------------------------------------------

use plingo::reactive::digest::SemanticDigest;
use std::collections::HashMap;

fn render_program(program: &SurfaceProgram) -> String {
    let right_name = match &program.right_name {
        Some(name) => format!("some({name:?})"),
        None => "none".to_owned(),
    };
    format!("program{{left:{},right_name:{right_name}}}", program.left)
}

fn render_surface(node: &SurfaceNode) -> String {
    match node {
        SurfaceNode::Document => "Document".to_owned(),
        SurfaceNode::Binding | SurfaceNode::BindingName(_) => "Binding".to_owned(),
        SurfaceNode::Add => "Add".to_owned(),
        SurfaceNode::Number(value) => format!("Number({value})"),
        SurfaceNode::Name(name) => format!("Name({name:?})"),
    }
}

fn render_core(node: &CoreNode) -> String {
    match node {
        CoreNode::Module => "Module".to_owned(),
        CoreNode::LetBinding => "LetBinding".to_owned(),
        CoreNode::ApplyAdd => "ApplyAdd".to_owned(),
        CoreNode::Integer(value) => format!("Integer({value})"),
        CoreNode::Reference(name) => format!("Reference({name:?})"),
    }
}

/// Captures one tree family under one domain key as DFS path-keyed payload
/// and parent rows plus the root-list row. Returns every visited node's path
/// so provenance rows can be ID-erased too.
fn capture_tree<V, F>(
    digest: &mut SemanticDigest,
    snapshot: &plingo::reactive::Snapshot,
    uri: &str,
    family: &str,
    render: F,
) -> HashMap<Node<V>, String>
where
    V: plingo::reactive::kind::TreeView<Key = String>,
    F: Fn(&V::Payload) -> String,
{
    fn visit<V, F>(
        digest: &mut SemanticDigest,
        snapshot: &plingo::reactive::Snapshot,
        node: Node<V>,
        key: &str,
        parent_key: Option<&str>,
        family: &str,
        identities: &mut HashMap<Node<V>, String>,
        render: &F,
    ) where
        V: plingo::reactive::kind::TreeView<Key = String>,
        F: Fn(&V::Payload) -> String,
    {
        identities.insert(node.clone(), key.to_owned());
        let payload = snapshot
            .tree_payload::<V>(node.clone())
            .map(|payload| render(&payload))
            .unwrap_or_else(|| "absent".to_owned());
        digest.insert(&format!("{family}_tree"), key, &payload);
        digest.insert(
            &format!("{family}_parent"),
            key,
            parent_key.unwrap_or("none"),
        );
        for (index, child) in snapshot
            .tree_children::<V>(node.clone())
            .iter()
            .enumerate()
        {
            let child_key = format!("{key}.{index}");
            visit(
                digest,
                snapshot,
                child.clone(),
                &child_key,
                Some(key),
                family,
                identities,
                render,
            );
        }
    }

    let mut identities = HashMap::new();
    let roots = snapshot.tree_roots_of::<V>(&uri.to_owned());
    let root_paths: Vec<String> = roots
        .iter()
        .enumerate()
        .map(|(index, _)| format!("{family}:{uri}#{index}"))
        .collect();
    digest.insert(
        &format!("{family}_roots"),
        uri,
        &format!("[{}]", root_paths.join(",")),
    );
    for (index, root) in roots.iter().enumerate() {
        let key = format!("{family}:{uri}#{index}");
        visit(
            digest,
            snapshot,
            root.clone(),
            &key,
            None,
            family,
            &mut identities,
            &render,
        );
    }
    identities
}

/// Captures every present entry of every public view of this family.
pub fn semantic_digest(snapshot: &plingo::reactive::Snapshot) -> SemanticDigest {
    let mut digest = SemanticDigest::new();

    // Complete domain enumeration: every URI known to any public view,
    // including tree keys unreachable from an expected root.
    let mut uris: Vec<String> = Vec::new();
    uris.extend(snapshot.inputs::<SurfacePrograms>());
    uris.extend(snapshot.inputs::<SurfaceRoots>());
    for input in snapshot.inputs::<SurfaceTree>() {
        if let TreeKey::RootOrder(uri) = input {
            uris.push(uri);
        }
    }
    for input in snapshot.inputs::<CoreTree>() {
        if let TreeKey::RootOrder(uri) = input {
            uris.push(uri);
        }
    }
    uris.sort();
    uris.dedup();

    for uri in &uris {
        let program = snapshot
            .observe::<SurfacePrograms>(uri.clone())
            .as_deref()
            .map(render_program)
            .unwrap_or_else(|| "absent".to_owned());
        digest.insert("programs", uri, &program);

        let surface_nodes =
            capture_tree::<SurfaceTree, _>(&mut digest, snapshot, uri, "surface", render_surface);
        let core_nodes =
            capture_tree::<CoreTree, _>(&mut digest, snapshot, uri, "core", render_core);

        let source_root_row = match snapshot.observe::<SurfaceRoots>(uri.clone()).as_deref() {
            None => "absent".to_owned(),
            Some(root) => surface_nodes
                .get(root)
                .cloned()
                .unwrap_or_else(|| "unresolved-surface".to_owned()),
        };
        digest.insert("surface_roots_map", uri, &source_root_row);
        for (core_node, core_path) in &core_nodes {
            let origin_row = match snapshot.observe::<CoreOrigin>(core_node.clone()).as_deref() {
                None => "absent".to_owned(),
                Some(source) => surface_nodes
                    .get(source)
                    .cloned()
                    .unwrap_or_else(|| "unresolved-surface".to_owned()),
            };
            digest.insert("core_origin", core_path, &origin_row);
        }
    }
    digest
}
