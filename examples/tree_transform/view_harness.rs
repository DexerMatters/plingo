//! Parser-independent recursive tree transformation harness.
//!
//! The editable input is a map of semantic programs.  Components turn each
//! map entry into a generated source tree, and a second recursive component
//! family lowers that tree into a distinct target tree.  Stable component
//! inputs, rather than identity tables or topology views, own node lifetime.

use plingo::prelude::*;
use plingo::reactive::Snapshot;
use plingo::reactive::digest::SemanticDigest;
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Semantic input and source/target trees
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceProgram {
    pub left: i64,
    pub right_name: Option<String>,
}

#[view]
pub struct SurfacePrograms(Map<String, SurfaceProgram>);

#[abstract_tree(
    domain = String,
    tree = SurfaceTree,
    members(SurfaceDocument, SurfaceDeclaration, SurfaceExpr)
)]
pub enum SurfaceDocument {
    Program {
        declarations: Vec<AstBox<SurfaceDeclaration>>,
    },
    Error {
        diagnostic: String,
    },
}

#[abstract_tree(member_of = SurfaceTree)]
pub enum SurfaceDeclaration {
    Binding { value: AstBox<SurfaceExpr> },
    Error { diagnostic: String },
}

#[abstract_tree(member_of = SurfaceTree)]
pub enum SurfaceExpr {
    Add { operands: Vec<AstBox<SurfaceExpr>> },
    Number { value: i64 },
    Name { value: String },
    Error { diagnostic: String },
}

#[abstract_tree(
    domain = String,
    tree = CoreTree,
    members(CoreDocument, CoreDeclaration, CoreExpr)
)]
pub enum CoreDocument {
    Module {
        declarations: Vec<AstBox<CoreDeclaration>>,
    },
    Error {
        diagnostic: String,
    },
}

#[abstract_tree(member_of = CoreTree)]
pub enum CoreDeclaration {
    Binding { value: AstBox<CoreExpr> },
    Error { diagnostic: String },
}

#[abstract_tree(member_of = CoreTree)]
pub enum CoreExpr {
    ApplyAdd { operands: Vec<AstBox<CoreExpr>> },
    Integer { value: i64 },
    Reference { name: String },
    Error { diagnostic: String },
}

// ---------------------------------------------------------------------------
// Recursive source construction
// ---------------------------------------------------------------------------

/// Map membership owns one source-document component per URI.  The payload
/// is deliberately not read here, so a payload edit does not recreate the
/// document's root identity.
#[component]
pub fn build_surface(entry: Each<SurfacePrograms>) -> Result<AstBox<SurfaceDocument>> {
    surface_document(entry.key().clone())
}

#[component]
fn surface_document(uri: String) -> Result<AstBox<SurfaceDocument>> {
    let declaration = surface_declaration(uri)?;
    SurfaceDocument::render(SurfaceDocument::Program {
        declarations: vec![declaration],
    })
}

#[component]
fn surface_declaration(uri: String) -> Result<AstBox<SurfaceDeclaration>> {
    let value = surface_expr(uri)?;
    SurfaceDeclaration::render(SurfaceDeclaration::Binding { value })
}

/// The only source component that reads the editable program.  It calls one
/// stable child component per semantic leaf, so changing the optional name
/// changes only the authored child list and the affected leaf.
#[component]
fn surface_expr(uri: String) -> Result<AstBox<SurfaceExpr>> {
    let Some(program) = SurfacePrograms::get(&uri)? else {
        return SurfaceExpr::render(SurfaceExpr::Error {
            diagnostic: "missing program".to_owned(),
        });
    };
    let number = surface_number(uri.clone())?;
    let mut operands = vec![number];
    if program.right_name.is_some() {
        operands.push(surface_name(uri)?);
    }
    SurfaceExpr::render(SurfaceExpr::Add { operands })
}

#[component]
fn surface_number(uri: String) -> Result<AstBox<SurfaceExpr>> {
    let value = SurfacePrograms::get(&uri)?.map(|program| program.left);
    match value {
        Some(value) => SurfaceExpr::render(SurfaceExpr::Number { value }),
        None => SurfaceExpr::render(SurfaceExpr::Error {
            diagnostic: "missing number".to_owned(),
        }),
    }
}

#[component]
fn surface_name(uri: String) -> Result<AstBox<SurfaceExpr>> {
    let value = SurfacePrograms::get(&uri)?.and_then(|program| program.right_name.clone());
    match value {
        Some(value) => SurfaceExpr::render(SurfaceExpr::Name { value }),
        None => SurfaceExpr::render(SurfaceExpr::Error {
            diagnostic: "missing name".to_owned(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Recursive lowering
// ---------------------------------------------------------------------------

#[component]
pub fn lower_document(source: AstBox<SurfaceDocument>) -> Result<AstBox<CoreDocument>> {
    let value = match source.view()? {
        SurfaceDocumentView::Program(program) => CoreDocument::Module {
            declarations: program
                .declarations()?
                .iter()
                .map(lower_declaration)
                .collect::<Result<Vec<_>>>()?,
        },
        SurfaceDocumentView::Error(error) => CoreDocument::Error {
            diagnostic: error.diagnostic()?.to_string(),
        },
    };
    CoreDocument::render(value)
}

#[component]
fn lower_declaration(source: AstBox<SurfaceDeclaration>) -> Result<AstBox<CoreDeclaration>> {
    let value = match source.view()? {
        SurfaceDeclarationView::Binding(binding) => CoreDeclaration::Binding {
            value: lower_expr(binding.value()?)?,
        },
        SurfaceDeclarationView::Error(error) => CoreDeclaration::Error {
            diagnostic: error.diagnostic()?.to_string(),
        },
    };
    CoreDeclaration::render(value)
}

#[component]
fn lower_expr(source: AstBox<SurfaceExpr>) -> Result<AstBox<CoreExpr>> {
    let value = match source.view()? {
        SurfaceExprView::Add(add) => CoreExpr::ApplyAdd {
            operands: add
                .operands()?
                .iter()
                .map(|child| lower_expr(child))
                .collect::<Result<Vec<_>>>()?,
        },
        SurfaceExprView::Number(number) => CoreExpr::Integer {
            value: *number.value()?,
        },
        SurfaceExprView::Name(name) => CoreExpr::Reference {
            name: name.value()?.to_string(),
        },
        SurfaceExprView::Error(error) => CoreExpr::Error {
            diagnostic: error.diagnostic()?.to_string(),
        },
    };
    CoreExpr::render(value)
}

// ---------------------------------------------------------------------------
// Snapshot digest
// ---------------------------------------------------------------------------

fn child_path(parent: &str, index: usize) -> String {
    if parent.is_empty() {
        index.to_string()
    } else {
        format!("{parent}.{index}")
    }
}

fn render_surface_document(
    tree: &SnapshotTree<SurfaceTree>,
    node: AstBox<SurfaceDocument>,
    uri: &str,
    path: &str,
    parent: Option<&str>,
    digest: &mut SemanticDigest,
) -> Result<()> {
    let key = format!("surface:{uri}#{path}");
    let payload = match tree.view(node)? {
        SurfaceDocumentView::Program(program) => {
            let declarations = program.declarations()?;
            for (index, child) in declarations.iter().enumerate() {
                render_surface_declaration(
                    tree,
                    child,
                    uri,
                    &child_path(path, index),
                    Some(&key),
                    digest,
                )?;
            }
            "Document".to_owned()
        }
        SurfaceDocumentView::Error(error) => format!("Error({:?})", error.diagnostic()?),
    };
    digest.insert("surface_tree", &key, &payload);
    digest.insert("surface_parent", &key, parent.unwrap_or("none"));
    Ok(())
}

fn render_surface_declaration(
    tree: &SnapshotTree<SurfaceTree>,
    node: AstBox<SurfaceDeclaration>,
    uri: &str,
    path: &str,
    parent: Option<&str>,
    digest: &mut SemanticDigest,
) -> Result<()> {
    let key = format!("surface:{uri}#{path}");
    let payload = match tree.view(node)? {
        SurfaceDeclarationView::Binding(binding) => {
            render_surface_expr(
                tree,
                binding.value()?,
                uri,
                &child_path(path, 0),
                Some(&key),
                digest,
            )?;
            "Binding".to_owned()
        }
        SurfaceDeclarationView::Error(error) => format!("Error({:?})", error.diagnostic()?),
    };
    digest.insert("surface_tree", &key, &payload);
    digest.insert("surface_parent", &key, parent.unwrap_or("none"));
    Ok(())
}

fn render_surface_expr(
    tree: &SnapshotTree<SurfaceTree>,
    node: AstBox<SurfaceExpr>,
    uri: &str,
    path: &str,
    parent: Option<&str>,
    digest: &mut SemanticDigest,
) -> Result<()> {
    let key = format!("surface:{uri}#{path}");
    let payload = match tree.view(node)? {
        SurfaceExprView::Add(add) => {
            for (index, child) in add.operands()?.iter().enumerate() {
                render_surface_expr(
                    tree,
                    child,
                    uri,
                    &child_path(path, index),
                    Some(&key),
                    digest,
                )?;
            }
            "Add".to_owned()
        }
        SurfaceExprView::Number(number) => format!("Number({})", number.value()?),
        SurfaceExprView::Name(name) => format!("Name({:?})", name.value()?),
        SurfaceExprView::Error(error) => format!("Error({:?})", error.diagnostic()?),
    };
    digest.insert("surface_tree", &key, &payload);
    digest.insert("surface_parent", &key, parent.unwrap_or("none"));
    Ok(())
}

fn render_core_document(
    tree: &SnapshotTree<CoreTree>,
    node: AstBox<CoreDocument>,
    uri: &str,
    path: &str,
    parent: Option<&str>,
    digest: &mut SemanticDigest,
) -> Result<()> {
    let key = format!("core:{uri}#{path}");
    let payload = match tree.view(node)? {
        CoreDocumentView::Module(module) => {
            for (index, child) in module.declarations()?.iter().enumerate() {
                render_core_declaration(
                    tree,
                    child,
                    uri,
                    &child_path(path, index),
                    Some(&key),
                    digest,
                )?;
            }
            "Module".to_owned()
        }
        CoreDocumentView::Error(error) => format!("Error({:?})", error.diagnostic()?),
    };
    digest.insert("core_tree", &key, &payload);
    digest.insert("core_parent", &key, parent.unwrap_or("none"));
    digest.insert("core_origin", &key, &format!("surface:{uri}#{path}"));
    Ok(())
}

fn render_core_declaration(
    tree: &SnapshotTree<CoreTree>,
    node: AstBox<CoreDeclaration>,
    uri: &str,
    path: &str,
    parent: Option<&str>,
    digest: &mut SemanticDigest,
) -> Result<()> {
    let key = format!("core:{uri}#{path}");
    let payload = match tree.view(node)? {
        CoreDeclarationView::Binding(binding) => {
            render_core_expr(
                tree,
                binding.value()?,
                uri,
                &child_path(path, 0),
                Some(&key),
                digest,
            )?;
            "LetBinding".to_owned()
        }
        CoreDeclarationView::Error(error) => format!("Error({:?})", error.diagnostic()?),
    };
    digest.insert("core_tree", &key, &payload);
    digest.insert("core_parent", &key, parent.unwrap_or("none"));
    digest.insert("core_origin", &key, &format!("surface:{uri}#{path}"));
    Ok(())
}

fn render_core_expr(
    tree: &SnapshotTree<CoreTree>,
    node: AstBox<CoreExpr>,
    uri: &str,
    path: &str,
    parent: Option<&str>,
    digest: &mut SemanticDigest,
) -> Result<()> {
    let key = format!("core:{uri}#{path}");
    let payload = match tree.view(node)? {
        CoreExprView::ApplyAdd(add) => {
            for (index, child) in add.operands()?.iter().enumerate() {
                render_core_expr(
                    tree,
                    child,
                    uri,
                    &child_path(path, index),
                    Some(&key),
                    digest,
                )?;
            }
            "ApplyAdd".to_owned()
        }
        CoreExprView::Integer(integer) => format!("Integer({})", integer.value()?),
        CoreExprView::Reference(reference) => format!("Reference({:?})", reference.name()?),
        CoreExprView::Error(error) => format!("Error({:?})", error.diagnostic()?),
    };
    digest.insert("core_tree", &key, &payload);
    digest.insert("core_parent", &key, parent.unwrap_or("none"));
    digest.insert("core_origin", &key, &format!("surface:{uri}#{path}"));
    Ok(())
}

/// Captures all semantic input and generated source/target snapshot rows.
/// Traversal uses only generated family views and typed child accessors; no
/// encoded fact keys or identity/projection maps are exposed to the example.
pub fn semantic_digest(snapshot: &Snapshot) -> SemanticDigest {
    let mut digest = SemanticDigest::new();
    let mut uris: BTreeSet<String> = snapshot.inputs::<SurfacePrograms>().into_iter().collect();
    let surface = snapshot.tree::<SurfaceTree>();
    let core = snapshot.tree::<CoreTree>();

    for uri in uris.iter() {
        let program = SurfacePrograms::get_snapshot(snapshot, uri)
            .map(|value| {
                format!(
                    "program{{left:{},right_name:{:?}}}",
                    value.left, value.right_name
                )
            })
            .unwrap_or_else(|| "absent".to_owned());
        digest.insert("programs", uri, &program);

        let surface_roots: Vec<_> = surface.roots(uri).collect();
        let surface_paths: Vec<String> = surface_roots
            .iter()
            .enumerate()
            .map(|(index, _)| format!("surface:{uri}#{index}"))
            .collect();
        digest.insert(
            "surface_roots_map",
            uri,
            surface_paths
                .first()
                .map(String::as_str)
                .unwrap_or("absent"),
        );
        digest.insert(
            "surface_roots",
            uri,
            &format!("[{}]", surface_paths.join(",")),
        );
        for (index, root) in surface_roots.into_iter().enumerate() {
            let path = index.to_string();
            if let Err(error) =
                render_surface_document(&surface, root, uri, &path, None, &mut digest)
            {
                digest.insert(
                    "errors",
                    &format!("surface:{uri}#{path}"),
                    &error.to_string(),
                );
            }
        }

        let core_roots: Vec<_> = core.roots(uri).collect();
        let core_paths: Vec<String> = core_roots
            .iter()
            .enumerate()
            .map(|(index, _)| format!("core:{uri}#{index}"))
            .collect();
        digest.insert("core_roots", uri, &format!("[{}]", core_paths.join(",")));
        for (index, root) in core_roots.into_iter().enumerate() {
            let path = index.to_string();
            if let Err(error) = render_core_document(&core, root, uri, &path, None, &mut digest) {
                digest.insert("errors", &format!("core:{uri}#{path}"), &error.to_string());
            }
        }
    }
    digest
}

trait SnapshotSurfacePrograms {
    fn get_snapshot(snapshot: &Snapshot, key: &String) -> Option<SurfaceProgram>;
}

impl SnapshotSurfacePrograms for SurfacePrograms {
    fn get_snapshot(snapshot: &Snapshot, key: &String) -> Option<SurfaceProgram> {
        snapshot
            .observe::<SurfacePrograms>(key.clone())
            .map(|value| (*value).clone())
    }
}
