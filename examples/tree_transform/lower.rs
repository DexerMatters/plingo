//! Recursive parser-independent lowering for the tree-transform example.
//!
//! The parser publishes `TransformTree` through the same abstract-tree schema
//! used by the lowered tree.  Each component reads only the fields it needs,
//! calls child components for semantic children, and returns one owned
//! `AstBox` render output.  No identity table or raw topology view is needed.

use plingo::framework::parse::{AstSnapshots, ParseStatus};
use plingo::prelude::*;
use plingo::reactive::Snapshot;
use plingo::reactive::abstract_tree::AstBox;
use plingo::reactive::digest::SemanticDigest;
use std::collections::HashMap;

use super::syntax::{
    TransformDeclaration, TransformDocument, TransformDocumentView, TransformExpr,
    TransformExprView,
};

/// The semantic lowered family.  The target deliberately has its own schema,
/// even though it preserves the source tree's recursive shape.
#[abstract_tree(domain = String, tree = LoweredTree, members(LoweredDocument, LoweredDeclaration, LoweredExpr))]
pub enum LoweredDocument {
    Module {
        declarations: Vec<AstBox<LoweredDeclaration>>,
    },
    Error {
        diagnostic: String,
    },
}

#[abstract_tree(member_of = LoweredTree)]
pub enum LoweredDeclaration {
    Binding { value: AstBox<LoweredExpr> },
    Error { diagnostic: String },
}

#[abstract_tree(member_of = LoweredTree)]
pub enum LoweredExpr {
    Add {
        left: AstBox<LoweredExpr>,
        right: AstBox<LoweredExpr>,
    },
    Subtract {
        left: AstBox<LoweredExpr>,
        right: AstBox<LoweredExpr>,
    },
    Group {
        expression: AstBox<LoweredExpr>,
    },
    Number,
    Name,
    Error {
        diagnostic: String,
    },
}

/// Lowers one parser document root.  The generated root mount owns the target
/// root relation; this body owns only its render slot and child calls.
#[component]
pub fn lower_document(source: AstBox<TransformDocument>) -> Result<AstBox<LoweredDocument>> {
    let value = match source.view()? {
        TransformDocumentView::Program(program) => LoweredDocument::Module {
            declarations: program
                .declarations()?
                .iter()
                .map(lower_declaration)
                .collect::<Result<Vec<_>>>()?,
        },
        TransformDocumentView::Error(error) => LoweredDocument::Error {
            diagnostic: format!("{:?}", error.error()?),
        },
    };
    LoweredDocument::render(value)
}

#[component]
pub fn lower_declaration(
    source: AstBox<TransformDeclaration>,
) -> Result<AstBox<LoweredDeclaration>> {
    let value = match source.view()? {
        super::syntax::TransformDeclarationView::Binding(binding) => LoweredDeclaration::Binding {
            value: lower_expr(binding.value()?)?,
        },
        super::syntax::TransformDeclarationView::Error(error) => LoweredDeclaration::Error {
            diagnostic: format!("{:?}", error.error()?),
        },
    };
    LoweredDeclaration::render(value)
}

#[component]
pub fn lower_expr(source: AstBox<TransformExpr>) -> Result<AstBox<LoweredExpr>> {
    let value = match source.view()? {
        TransformExprView::Add(add) => LoweredExpr::Add {
            left: lower_expr(add.left()?)?,
            right: lower_expr(add.right()?)?,
        },
        TransformExprView::Subtract(subtract) => LoweredExpr::Subtract {
            left: lower_expr(subtract.left()?)?,
            right: lower_expr(subtract.right()?)?,
        },
        TransformExprView::Group(group) => LoweredExpr::Group {
            expression: lower_expr(group.expression()?)?,
        },
        TransformExprView::Number(_) => LoweredExpr::Number,
        TransformExprView::Name(_) => LoweredExpr::Name,
        TransformExprView::Error(error) => LoweredExpr::Error {
            diagnostic: format!("{:?}", error.error()?),
        },
    };
    LoweredExpr::render(value)
}

// ---------------------------------------------------------------------------
// ID-erased semantic digest used by the example's warm/cold and lifecycle
// oracles.  It intentionally traverses generated snapshot accessors rather
// than encoded tree fact keys.
// ---------------------------------------------------------------------------

fn child_path(parent: &str, index: usize) -> String {
    if parent.is_empty() {
        index.to_string()
    } else {
        format!("{parent}.{index}")
    }
}

fn render_status(status: &ParseStatus) -> String {
    match status {
        ParseStatus::Clean => "clean".to_owned(),
        ParseStatus::Recovered { diagnostics } => format!("recovered({diagnostics})"),
        ParseStatus::Unrecoverable { diagnostics } => format!("unrecoverable({diagnostics})"),
    }
}

fn source_leaf_lexeme(
    snapshot: &Snapshot,
    ast: Option<&plingo::framework::parse::AstSnapshot>,
    source: Option<&SourceSnapshot>,
    node: AstBox<TransformExpr>,
) -> String {
    let fallback = || "?".to_owned();
    let token = match snapshot.tree::<super::syntax::TransformTree>().view(node) {
        Ok(TransformExprView::Number(number)) => number.token().ok(),
        Ok(TransformExprView::Name(name)) => name.token().ok(),
        _ => None,
    };
    let Some(token) = token.as_deref().copied() else {
        return fallback();
    };
    let Some(entry) = ast.and_then(|ast| ast.token(token)) else {
        return fallback();
    };
    let range = entry.span.range;
    source
        .and_then(|source| source.byte_slice(range.start()..range.end()).ok())
        .unwrap_or_else(fallback)
}

fn source_expr_at_path(
    snapshot: &Snapshot,
    root: AstBox<TransformDocument>,
    path: &str,
) -> Option<AstBox<TransformExpr>> {
    let mut segments = path.split('.');
    let declaration_index = segments.next()?.parse::<usize>().ok()?;
    let tree = snapshot.tree::<super::syntax::TransformTree>();
    let declaration = match tree.view(root).ok()? {
        TransformDocumentView::Program(program) => {
            program.declarations().ok()?.get(declaration_index)?
        }
        TransformDocumentView::Error(_) => return None,
    };
    let value = match tree.view(declaration).ok()? {
        super::syntax::TransformDeclarationView::Binding(binding) => binding.value().ok()?,
        super::syntax::TransformDeclarationView::Error(_) => return None,
    };
    let rest = segments.collect::<Vec<_>>();
    if rest.first().copied() != Some("0") {
        return None;
    }
    source_expr_descendant(snapshot, value, &rest[1..])
}

fn source_expr_descendant(
    snapshot: &Snapshot,
    mut node: AstBox<TransformExpr>,
    segments: &[&str],
) -> Option<AstBox<TransformExpr>> {
    let tree = snapshot.tree::<super::syntax::TransformTree>();
    for segment in segments {
        let index = segment.parse::<usize>().ok()?;
        node = match tree.view(node).ok()? {
            TransformExprView::Add(add) if index == 0 => add.left().ok()?,
            TransformExprView::Add(add) if index == 1 => add.right().ok()?,
            TransformExprView::Subtract(subtract) if index == 0 => subtract.left().ok()?,
            TransformExprView::Subtract(subtract) if index == 1 => subtract.right().ok()?,
            TransformExprView::Group(group) if index == 0 => group.expression().ok()?,
            _ => return None,
        };
    }
    Some(node)
}

fn render_lowered(
    tree: &SnapshotTree<super::lower::LoweredTree>,
    node: AstBox<LoweredDocument>,
    source_root: Option<AstBox<TransformDocument>>,
    snapshot: &Snapshot,
    ast: Option<&plingo::framework::parse::AstSnapshot>,
    source: Option<&SourceSnapshot>,
    uri: &str,
    path: &str,
    digest: &mut SemanticDigest,
) -> Result<()> {
    let key = format!("{uri}#{path}");
    match tree.materialize(node)? {
        LoweredDocument::Module { declarations } => {
            digest.insert("lowered", &key, "Module");
            digest.insert("origin", &key, &key);
            for (index, declaration) in declarations.into_iter().enumerate() {
                render_declaration(
                    tree,
                    declaration,
                    source_root.clone(),
                    snapshot,
                    ast,
                    source,
                    uri,
                    &child_path(path, index),
                    digest,
                )?;
            }
        }
        LoweredDocument::Error { .. } => {
            digest.insert("lowered", &key, "ParseError");
            digest.insert("origin", &key, &key);
        }
    }
    Ok(())
}

fn render_declaration(
    tree: &SnapshotTree<super::lower::LoweredTree>,
    node: AstBox<LoweredDeclaration>,
    source_root: Option<AstBox<TransformDocument>>,
    snapshot: &Snapshot,
    ast: Option<&plingo::framework::parse::AstSnapshot>,
    source: Option<&SourceSnapshot>,
    uri: &str,
    path: &str,
    digest: &mut SemanticDigest,
) -> Result<()> {
    let key = format!("{uri}#{path}");
    match tree.materialize(node)? {
        LoweredDeclaration::Binding { value } => {
            digest.insert("lowered", &key, "Binding");
            digest.insert("origin", &key, &key);
            render_expr(
                tree,
                value,
                source_root,
                snapshot,
                ast,
                source,
                uri,
                &format!("{path}.0"),
                digest,
            )?;
        }
        LoweredDeclaration::Error { .. } => {
            digest.insert("lowered", &key, "ParseError");
            digest.insert("origin", &key, &key);
        }
    }
    Ok(())
}

fn render_expr(
    tree: &SnapshotTree<super::lower::LoweredTree>,
    node: AstBox<LoweredExpr>,
    source_root: Option<AstBox<TransformDocument>>,
    snapshot: &Snapshot,
    ast: Option<&plingo::framework::parse::AstSnapshot>,
    source: Option<&SourceSnapshot>,
    uri: &str,
    path: &str,
    digest: &mut SemanticDigest,
) -> Result<()> {
    let materialized = tree.materialize(node)?;
    let (kind, children): (&str, Vec<AstBox<LoweredExpr>>) = match materialized {
        LoweredExpr::Add { left, right } => ("Sum", vec![left, right]),
        LoweredExpr::Subtract { left, right } => ("Difference", vec![left, right]),
        LoweredExpr::Group { expression } => ("Group", vec![expression]),
        LoweredExpr::Number => ("Number", Vec::new()),
        LoweredExpr::Name => ("Name", Vec::new()),
        LoweredExpr::Error { .. } => ("ParseError", Vec::new()),
    };
    let rendered = if matches!(kind, "Number" | "Name") {
        // The lowered node's structural path is also the source path because
        // this example is a shape-preserving transform.
        let lexeme = source_root
            .as_ref()
            .and_then(|root| source_expr_at_path(snapshot, root.clone(), path))
            .map(|source_node| source_leaf_lexeme(snapshot, ast, source, source_node))
            .unwrap_or_else(|| "?".to_owned());
        format!("{kind}({lexeme})")
    } else {
        kind.to_owned()
    };
    let key = format!("{uri}#{path}");
    digest.insert("lowered", &key, &rendered);
    digest.insert("origin", &key, &key);
    for (index, child) in children.into_iter().enumerate() {
        render_expr(
            tree,
            child,
            source_root.clone(),
            snapshot,
            ast,
            source,
            uri,
            &child_path(path, index),
            digest,
        )?;
    }
    Ok(())
}

/// Captures parse status and the generated lowered-tree snapshot for every
/// open document, with stable structural paths instead of raw identities.
pub fn semantic_digest(snapshot: &Snapshot) -> SemanticDigest {
    let mut digest = SemanticDigest::new();
    let source_inputs = snapshot.inputs::<AstSnapshots<TransformDocument>>();
    let target_tree = snapshot.tree::<LoweredTree>();
    let source_tree = snapshot.tree::<super::syntax::TransformTree>();
    let mut uris = source_inputs;
    uris.sort();
    uris.dedup();

    for uri in uris {
        if let Some(status) =
            snapshot.observe::<plingo::framework::parse::ParserTreeStatuses>(uri.clone())
        {
            digest.insert("parse", &uri, &render_status(status.as_ref()));
        }
        let ast = snapshot.observe::<AstSnapshots<TransformDocument>>(uri.clone());
        let source = source_snapshot(snapshot, &uri);
        let source_root = source_tree.roots(&uri).next();
        for root in target_tree.roots(&uri) {
            if let Err(error) = render_lowered(
                &target_tree,
                root,
                source_root.clone(),
                snapshot,
                ast.as_ref().map(|document| document.snapshot()),
                source.as_ref(),
                &uri,
                "",
                &mut digest,
            ) {
                digest.insert("error", &uri, &error.to_string());
            }
        }
    }
    digest
}
