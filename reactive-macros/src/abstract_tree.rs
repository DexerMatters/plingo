//! `#[abstract_tree]` — turns a family of syntax enums into a tree view
//! (plan §6). The attribute is placed on *every* member enum with the same
//! `members(...)` list (root first); the root's expansion additionally
//! generates the shared view, unions, and extension traits. Proc macros
//! cannot see sibling items, so each member must carry the attribute.
//!
//! Per member enum `E`, each expansion generates:
//! - `ENode` — the payload node (leaf fields + a `kind` tag).
//! - `ECase` — the typed case (leaf fields owned, children as `NodeId`s).
//! - `From<&E> for ENode`, `ECase::from_parts`, `E::tree_kind`, and
//!   `E::__tree_emit` (recursive emission with deterministic fresh ids).
//!
//! The root's expansion additionally generates the view struct
//! (`{Root}Tree`), the `{Root}Node`/`{Root}Case` unions, `id_from_span`,
//! and the `{Root}ObservedExt` / `{Root}EmittedExt` / `{Root}SnapshotExt`
//! extension traits.

use quote::{format_ident, quote};
use syn::{Fields, ItemEnum, PathArguments, Type};

/// Parsed attribute content: the explicit member list.
pub(crate) struct AbstractTreeArgs {
    pub members: Option<Vec<syn::Ident>>,
}

impl AbstractTreeArgs {
    pub fn parse(&mut self, meta: syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
        if meta.path.is_ident("members") {
            if self.members.is_some() {
                return Err(meta.error("duplicate `members`"));
            }
            let mut members = Vec::new();
            meta.parse_nested_meta(|member| {
                if !member.path.segments.is_empty() {
                    members.push(member.path.segments[0].ident.clone());
                }
                Ok(())
            })?;
            self.members = Some(members);
            Ok(())
        } else {
            Err(meta.error("unsupported abstract_tree property"))
        }
    }
}

/// A field's classification inside one member enum.
enum FieldClass {
    /// A syntax child: lives in a separate node.
    Child { kind: ChildKind, member: syn::Ident },
    /// Leaf payload: stored (owned) in the payload node.
    Leaf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChildKind {
    Single,
    List,
    Optional,
}

/// One field entry used by codegen.
struct GenField {
    /// The field's codegen name (real ident or `f{index}`).
    name: syn::Ident,
    index: usize,
    class: FieldClass,
    /// The runtime leaf type as written by the user.
    leaf_ty: Type,
}

/// One variant, split into children and leaves.
struct GenVariant {
    ident: syn::Ident,
    fields: Vec<GenField>,
    /// True when the variant originally used named fields.
    named: bool,
}

/// One member enum with its classification.
struct GenMember {
    ident: syn::Ident,
    variants: Vec<GenVariant>,
    /// The index of the span field (named `span` or `#[tree(span)]`), used
    /// by derived-id emission.
    span_field: Option<usize>,
}

// ---------------------------------------------------------------------------
// Field classification
// ---------------------------------------------------------------------------

fn container_inner(ty: &Type) -> Option<(&str, &Type)> {
    let Type::Path(path) = ty else {
        return None;
    };
    let Some(last) = path.path.segments.last() else {
        return None;
    };
    let name = last.ident.to_string();
    if !matches!(
        name.as_str(),
        "Box" | "Vec" | "Option" | "Arc" | "Rc" | "AstBox"
    ) {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    for arg in &args.args {
        if let syn::GenericArgument::Type(inner) = arg {
            // Return a stable literal: the container name as a constant.
            let real: &'static str = match name.as_str() {
                "Box" => "Box",
                "Vec" => "Vec",
                "Option" => "Option",
                "Arc" => "Arc",
                "Rc" => "Rc",
                "AstBox" => "AstBox",
                _ => continue,
            };
            return Some((real, inner));
        }
    }
    None
}

fn strip_containers(ty: &Type) -> &Type {
    let mut ty = ty;
    while let Some((_, inner)) = container_inner(ty) {
        ty = inner;
    }
    ty
}

/// The outermost child kind of a member-typed field.
fn child_kind(ty: &Type) -> ChildKind {
    let Type::Path(path) = ty else {
        return ChildKind::Single;
    };
    let Some(last) = path.path.segments.last() else {
        return ChildKind::Single;
    };
    match last.ident.to_string().as_str() {
        "Vec" => ChildKind::List,
        "Option" => ChildKind::Optional,
        _ => ChildKind::Single,
    }
}

fn member_of(ty: &Type, members: &[syn::Ident]) -> Option<syn::Ident> {
    let inner = strip_containers(ty);
    let Type::Path(path) = inner else {
        return None;
    };
    path.path
        .get_ident()
        .and_then(|ident| members.iter().find(|m| *m == ident).cloned())
}

/// Classifies one member enum's variants given the family member set.
fn classify_member(
    ident: &syn::Ident,
    item: &ItemEnum,
    members: &[syn::Ident],
) -> syn::Result<GenMember> {
    let mut variants = Vec::new();
    for variant in &item.variants {
        let mut saw_list = false;
        let mut fields = Vec::new();
        for (index, field) in variant.fields.iter().enumerate() {
            // Field overrides: #[tree(child)] / #[tree(leaf)].
            let mut forced_child = false;
            let mut forced_leaf = false;
            for attr in &field.attrs {
                if attr.path().is_ident("tree") {
                    attr.parse_nested_meta(|meta| {
                        if meta.path.is_ident("child") {
                            forced_child = true;
                        } else if meta.path.is_ident("leaf") {
                            forced_leaf = true;
                        } else {
                            return Err(meta.error("unsupported tree field property"));
                        }
                        Ok(())
                    })?;
                }
            }
            let class = if forced_leaf {
                FieldClass::Leaf
            } else if forced_child {
                let member = member_of(&field.ty, members).ok_or_else(|| {
                    syn::Error::new_spanned(
                        field,
                        "#[tree(child)] field must be a family member type",
                    )
                })?;
                FieldClass::Child {
                    kind: child_kind(&field.ty),
                    member,
                }
            } else if let Some(member) = member_of(&field.ty, members) {
                FieldClass::Child {
                    kind: child_kind(&field.ty),
                    member,
                }
            } else {
                FieldClass::Leaf
            };
            if matches!(class, FieldClass::Child { kind: ChildKind::List, .. }) {
                if saw_list {
                    return Err(syn::Error::new_spanned(
                        field,
                        "multiple Vec children in one variant are not supported; keep the Vec last",
                    ));
                }
                saw_list = true;
            }
            if saw_list && !matches!(class, FieldClass::Leaf) && !matches!(
                class,
                FieldClass::Child { kind: ChildKind::List, .. }
            ) {
                return Err(syn::Error::new_spanned(
                    field,
                    "a child after a Vec child is ambiguous; keep Vec children last",
                ));
            }
            let name = field
                .ident
                .clone()
                .unwrap_or_else(|| format_ident!("f{index}"));
            fields.push(GenField {
                name,
                index,
                class,
                leaf_ty: field.ty.clone(),
            });
        }
        let named = matches!(variant.fields, Fields::Named(_));
        variants.push(GenVariant {
            ident: variant.ident.clone(),
            fields,
            named,
        });
    }
    // Detect the span field (`span` by convention, or `#[tree(span)]`),
    // used by derived-id emission. Every variant must carry it (a leaf
    // field) for the parser path.
    let mut span_field: Option<usize> = None;
    for variant in &item.variants {
        for (index, field) in variant.fields.iter().enumerate() {
            let explicit = field.attrs.iter().any(|attr| {
                attr.path().is_ident("tree")
                    && matches!(attr.meta, syn::Meta::List(_))
            });
            let is_span_named = field
                .ident
                .as_ref()
                .is_some_and(|ident| ident == "span");
            if is_span_named || explicit {
                if span_field.is_none() {
                    span_field = Some(index);
                }
            }
        }
    }
    Ok(GenMember {
        ident: ident.clone(),
        variants,
        span_field,
    })
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

fn variant_ordinal(variants: &[GenVariant], ident: &syn::Ident) -> u8 {
    variants
        .iter()
        .position(|v| &v.ident == ident)
        .expect("variant in its own enum") as u8
}

fn to_snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The names shared by the family.
struct FamilyNames {
    view: syn::Ident,
    node: syn::Ident,
    case: syn::Ident,
    observed_trait: syn::Ident,
    emitted_trait: syn::Ident,
    snapshot_trait: syn::Ident,
    /// Short (prefix-stripped) variant name per member ident.
    shorts: Vec<(syn::Ident, syn::Ident)>,
}

/// Strips the longest common prefix of all member idents, yielding the
/// short names used in unions and method suffixes.
fn member_shorts(members: &[syn::Ident]) -> Vec<(syn::Ident, syn::Ident)> {
    let first = members[0].to_string();
    let mut prefix_len = first.len();
    for member in &members[1..] {
        let text = member.to_string();
        let common = text
            .as_bytes()
            .iter()
            .zip(first.as_bytes())
            .take_while(|(a, b)| a == b)
            .count();
        prefix_len = prefix_len.min(common);
    }
    // Move the boundary to a word start: extend past lowercase letters so
    // the stripped suffix begins at an uppercase letter.
    while prefix_len < first.len() && !first.as_bytes()[prefix_len].is_ascii_uppercase() {
        prefix_len += 1;
    }
    let mut shorts: Vec<(syn::Ident, syn::Ident)> = members
        .iter()
        .map(|member| {
            let text = member.to_string();
            let short = if prefix_len > 0 && prefix_len < text.len() {
                format_ident!("{}", &text[prefix_len..])
            } else {
                member.clone()
            };
            (member.clone(), short)
        })
        .collect();
    for i in 0..shorts.len() {
        for j in 0..shorts.len() {
            if i != j && shorts[i].1 == shorts[j].1 {
                shorts[i].1 = shorts[i].0.clone();
                shorts[j].1 = shorts[j].0.clone();
            }
        }
    }
    shorts
}

fn family_names(root: &syn::Ident, members: &[syn::Ident]) -> FamilyNames {
    // The family prefix (e.g. `Stlc`) names the view and unions; the root
    // defines the syntax members on top of it.
    let root_text = root.to_string();
    let mut prefix_len = root_text.len();
    for member in &members[1..] {
        let text = member.to_string();
        let common = text
            .as_bytes()
            .iter()
            .zip(root_text.as_bytes())
            .take_while(|(a, b)| a == b)
            .count();
        prefix_len = prefix_len.min(common);
    }
    while prefix_len > 0
        && prefix_len <= root_text.len()
        && prefix_len > 0
        && root_text.as_bytes()[prefix_len - 1].is_ascii_lowercase()
    {
        prefix_len -= 1;
    }
    // Extend forward past lowercase letters so the family prefix ends at a
    // word boundary: `Stlc` for `StlcExpr/StlcParam/StlcLit`.
    while prefix_len < root_text.len()
        && root_text.as_bytes()[prefix_len].is_ascii_lowercase()
    {
        prefix_len += 1;
    }
    let family = format_ident!("{}", &root_text[..prefix_len]);
    FamilyNames {
        view: format_ident!("{}Tree", family),
        node: format_ident!("{}Node", family),
        case: format_ident!("{}Case", family),
        observed_trait: format_ident!("{}ObservedExt", family),
        emitted_trait: format_ident!("{}EmittedExt", family),
        snapshot_trait: format_ident!("{}SnapshotExt", family),
        shorts: member_shorts(members),
    }
}

fn short_of(names: &FamilyNames, member: &syn::Ident) -> syn::Ident {
    names
        .shorts
        .iter()
        .find(|(long, _)| long == member)
        .map(|(_, short)| short.clone())
        .expect("member in family")
}

// ---------------------------------------------------------------------------
// Codegen helpers
// ---------------------------------------------------------------------------

/// Binding pattern (named, tuple, or unit) plus binding idents.
fn bindings(variant: &GenVariant) -> (proc_macro2::TokenStream, Vec<syn::Ident>) {
    let bindings: Vec<syn::Ident> = (0..variant.fields.len())
        .map(|i| format_ident!("__f{i}"))
        .collect();
    if variant.fields.is_empty() {
        (quote! {}, Vec::new())
    } else if variant.named {
        let named: Vec<proc_macro2::TokenStream> = variant
            .fields
            .iter()
            .zip(&bindings)
            .map(|(field, binding)| {
                let fname = field.name.clone();
                quote! { #fname: #binding }
            })
            .collect();
        (quote! { { #(#named,)* } }, bindings)
    } else {
        (quote! { (#(#bindings),*) }, bindings)
    }
}

/// The ENode variant body: `kind` + leaves + has_<option-child>.
fn node_variant_body(variant: &GenVariant) -> Vec<proc_macro2::TokenStream> {
    let mut fields: Vec<proc_macro2::TokenStream> = vec![quote! { kind: u8 }];
    for field in &variant.fields {
        if matches!(field.class, FieldClass::Leaf) {
            let name = &field.name;
            let ty = &field.leaf_ty;
            fields.push(quote! { #name: #ty });
        }
    }
    for field in &variant.fields {
        if matches!(field.class, FieldClass::Child { kind: ChildKind::Optional, .. }) {
            let has = format_ident!("has_{}", field.name);
            fields.push(quote! { #has: bool });
        }
    }
    fields
}

/// The ECase variant body: leaves owned, children as node ids.
fn case_variant_fields(variant: &GenVariant) -> Vec<proc_macro2::TokenStream> {
    let mut fields: Vec<proc_macro2::TokenStream> = Vec::new();
    for field in &variant.fields {
        let name = &field.name;
        match &field.class {
            FieldClass::Leaf => {
                let ty = &field.leaf_ty;
                fields.push(quote! { #name: #ty });
            }
            FieldClass::Child { kind, .. } => {
                let ty = match kind {
                    ChildKind::Single => quote! { ::plingo::reactive::NodeId },
                    ChildKind::List => quote! {
                        ::std::vec::Vec<::plingo::reactive::NodeId>
                    },
                    ChildKind::Optional => quote! {
                        ::std::option::Option<::plingo::reactive::NodeId>
                    },
                };
                fields.push(quote! { #name: #ty });
            }
        }
    }
    fields
}

fn leaf_bindings_of(variant: &GenVariant) -> Vec<(syn::Ident, syn::Ident)> {
    variant
        .fields
        .iter()
        .filter_map(|field| {
            if matches!(field.class, FieldClass::Leaf) {
                Some((
                    field.name.clone(),
                    format_ident!("__leaf_{}", field.index),
                ))
            } else {
                None
            }
        })
        .collect()
}

fn opt_bindings_of(variant: &GenVariant) -> Vec<(syn::Ident, syn::Ident)> {
    variant
        .fields
        .iter()
        .filter_map(|field| {
            if matches!(field.class, FieldClass::Child { kind: ChildKind::Optional, .. }) {
                Some((
                    format_ident!("has_{}", field.name),
                    format_ident!("__has_{}", field.index),
                ))
            } else {
                None
            }
        })
        .collect()
}

/// The `ECase::from_parts` match arm — consumes the children slice.
fn gen_from_parts_arm(
    variant: &GenVariant,
    node_ident: &syn::Ident,
    case_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    let vident = &variant.ident;
    let mut pattern_fields: Vec<proc_macro2::TokenStream> = vec![quote! { kind: _ }];
    for (name, binding) in leaf_bindings_of(variant) {
        pattern_fields.push(quote! { #name: #binding });
    }
    for (has, binding) in opt_bindings_of(variant) {
        pattern_fields.push(quote! { #has: #binding });
    }
    let mut case_fields: Vec<proc_macro2::TokenStream> = Vec::new();
    for field in variant.fields.iter() {
        let name = &field.name;
        match &field.class {
            FieldClass::Leaf => {
                let binding = leaf_bindings_of(variant)
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, b)| b.clone())
                    .expect("leaf binding");
                case_fields.push(quote! { #name: #binding.clone() });
            }
            FieldClass::Child { kind: ChildKind::Single, .. } => {
                case_fields.push(quote! {
                    #name: {
                        if children_cursor < children.len() {
                            let value = children[children_cursor];
                            children_cursor += 1;
                            value
                        } else {
                            return Err(::plingo::reactive::Error::Internal(
                                format!(
                                    "abstract-tree case arity mismatch (children len {})",
                                    children.len(),
                                )
                                .into(),
                            ));
                        }
                    }
                });
            }
            FieldClass::Child { kind: ChildKind::Optional, .. } => {
                let binding = opt_bindings_of(variant)
                    .iter()
                    .find(|(_, b)| b.to_string() == format!("__has_{}", field.index))
                    .map(|(_, b)| b.clone())
                    .expect("optional binding");
                case_fields.push(quote! {
                    #name: {
                        if *#binding {
                            if children_cursor < children.len() {
                                let value = children[children_cursor];
                                children_cursor += 1;
                                ::std::option::Option::Some(value)
                            } else {
                                return Err(::plingo::reactive::Error::Internal(
                                    format!(
                                        "abstract-tree case arity mismatch (children len {})",
                                        children.len(),
                                    )
                                    .into(),
                                ));
                            }
                        } else {
                            ::std::option::Option::None
                        }
                    }
                });
            }
            FieldClass::Child { kind: ChildKind::List, .. } => {
                case_fields.push(quote! {
                    #name: children[children_cursor..].to_vec()
                });
            }
        }
    }
    quote! {
        #node_ident::#vident
        {
            #(#pattern_fields),*
        } => {
            let mut children_cursor: usize = 0usize;
            ::std::result::Result::Ok(#case_ident::#vident
            {
                #(#case_fields),*
            })
        }
    }
}

/// The `From<&E> for ENode` match arm.
fn gen_from_value_arm(
    variant: &GenVariant,
    source_ident: &syn::Ident,
    members_variants: &[GenVariant],
    node_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    let vident = &variant.ident;
    let ordinal = variant_ordinal(members_variants, vident);
    let (pattern, bindings) = bindings(variant);
    let node_fields: Vec<proc_macro2::TokenStream> = variants_payload(variant, bindings);
    quote! {
        #source_ident::#vident #pattern => {
            #node_ident::#vident
            {
                kind: #ordinal,
                #(#node_fields),*
            }
        }
    }
}

/// Payload fields of ENode (from the From arm): `name: binding.clone()`.
fn variants_payload(
    variant: &GenVariant,
    bindings: Vec<syn::Ident>,
) -> Vec<proc_macro2::TokenStream> {
    let mut fields: Vec<proc_macro2::TokenStream> = Vec::new();
    for (index, field) in variant.fields.iter().enumerate() {
        let binding = &bindings[index];
        match &field.class {
            FieldClass::Leaf => {
                let name = &field.name;
                fields.push(quote! { #name: #binding.clone() });
            }
            FieldClass::Child { kind: ChildKind::Optional, .. } => {
                let has = format_ident!("has_{}", field.name);
                fields.push(quote! { #has: #binding.is_some() });
            }
            _ => {}
        }
    }
    fields
}

/// `E::__tree_emit` match arm — payload write + recursive children.
fn gen_tree_emit_arm(
    variant: &GenVariant,
    member_ident: &syn::Ident,
    names: &FamilyNames,
) -> proc_macro2::TokenStream {
    let vident = &variant.ident;
    let (pattern, bindings) = bindings(variant);
    let node_ident = format_ident!("{}Node", member_ident);
    let union_node_ident = &names.node;
    let member_short = short_of(names, member_ident);
    let mut child_walks: Vec<proc_macro2::TokenStream> = Vec::new();
    for (index, field) in variant.fields.iter().enumerate() {
        let binding = &bindings[index];
        if let FieldClass::Child { kind, member } = &field.class {
            let child_member = member;
            // `AstBox<M>`-rooted children (direct, `Option`, `Vec`) are
            // arena-backed: the fresh-id `__tree_emit` path never sees
            // them (the parser uses `__tree_walk_emit`). Skip so the
            // hand-written emitter type-checks.
            let mut ast_box_rooted = false;
            let mut probe: Option<&syn::Type> = Some(&field.leaf_ty);
            while let Some(ty) = probe {
                match ty {
                    syn::Type::Path(path) => {
                        let Some(seg) = path.path.segments.last() else { break };
                        if seg.ident == "AstBox" {
                            ast_box_rooted = true;
                            break;
                        }
                        if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                            for arg in &args.args {
                                if let syn::GenericArgument::Type(inner) = arg {
                                    probe = Some(inner);
                                    break;
                                }
                            }
                        } else {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            if ast_box_rooted {
                continue;
            }
            match kind {
                ChildKind::Single => child_walks.push(quote! {
                    {
                        let child_id = handle.fresh_node_id()?;
                        #child_member::__tree_emit(handle, child_id, #binding)?;
                        // Attach in field order: move_node appends and
                        // removes the child from the roots (children are
                        // never roots).
                        ::plingo::reactive::api::TreeEmittedExt::move_node(
                            handle, child_id, id,
                        )?;
                    }
                }),
                ChildKind::List => child_walks.push(quote! {
                    {
                        for child_value in #binding.iter() {
                            let child_id = handle.fresh_node_id()?;
                            #child_member::__tree_emit(handle, child_id, child_value)?;
                            ::plingo::reactive::api::TreeEmittedExt::move_node(
                                handle, child_id, id,
                            )?;
                        }
                    }
                }),
                ChildKind::Optional => child_walks.push(quote! {
                    {
                        if let ::std::option::Option::Some(child_value) = #binding.as_ref() {
                            let child_id = handle.fresh_node_id()?;
                            #child_member::__tree_emit(handle, child_id, child_value)?;
                            ::plingo::reactive::api::TreeEmittedExt::move_node(
                                handle, child_id, id,
                            )?;
                        }
                    }
                }),
            }
        }
    }
    quote! {
        Self::#vident #pattern => {
            let payload: #node_ident = ::std::convert::From::from(value);
            #(#child_walks)*
            ::plingo::reactive::api::TreeEmittedExt::upsert_node(
                handle,
                id,
                #union_node_ident::#member_short(payload),
            )?;
            ::std::result::Result::Ok(())
        }
    }
}

/// The `E::tree_kind` match arm.
fn gen_tree_kind_arm(
    variant: &GenVariant,
    members_variants: &[GenVariant],
) -> proc_macro2::TokenStream {
    let vident = &variant.ident;
    let ordinal = variant_ordinal(members_variants, vident);
    if variant.fields.is_empty() {
        quote! { Self::#vident => #ordinal }
    } else {
        quote! { Self::#vident { .. } => #ordinal }
    }
}

/// The inner emission for one arena-backed child: derive its id from the
/// snapshot span, upsert its payload, recurse, and attach under the parent.
fn walk_child_body(
    child_box: proc_macro2::TokenStream,
    member: &syn::Ident,
    names: &FamilyNames,
) -> proc_macro2::TokenStream {
    let member_node = format_ident!("{}Node", member);
    let member_short = short_of(names, member);
    let view_ident = &names.view;
    let union_node_ident = &names.node;
    quote! {
        if let ::std::option::Option::Some(child_value) = arena.get(#child_box) {
            // Identity uses token-coordinate extents (the arena's
            // AnchoredSpan), which are stable under text insertions
            // before the node: an unchanged subtree keeps its derived ids
            // (matrix 2/3). Byte spans would shift on any preceding edit.
            let (cstart, cend) = arena
                .extent_of((#child_box).id)
                .map(|extent| (extent.start as u32, extent.end as u32))
                .unwrap_or((0, 0));
            let child_id = #view_ident::id_from_span_typed::<#member>(
                uri, cstart, cend, child_value.tree_kind());
            let child_payload: #member_node = ::std::convert::From::from(child_value);
            ::plingo::reactive::api::TreeEmittedExt::upsert_node(
                handle,
                child_id,
                #union_node_ident::#member_short(child_payload),
            )?;
            #member::__tree_walk_emit(handle, uri, snapshot, arena, child_id, child_value)?;
            ::plingo::reactive::api::TreeEmittedExt::move_node(handle, child_id, id)?;
        }
    }
}

/// `E::__tree_walk_emit` match arm: unbox `AstBox` child fields (direct,
/// `Option<...>`, or `Vec<...>`) and recurse through the arena.
fn gen_tree_walk_arm(
    variant: &GenVariant,
    names: &FamilyNames,
) -> proc_macro2::TokenStream {
    let vident = &variant.ident;
    let (pattern, bindings) = bindings(variant);
    let mut walks: Vec<proc_macro2::TokenStream> = Vec::new();
    for (index, field) in variant.fields.iter().enumerate() {
        let binding = &bindings[index];
        let FieldClass::Child { member, .. } = &field.class else {
            continue;
        };
        let ty = &field.leaf_ty;
        let last = match ty {
            syn::Type::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        };
        // Only `AstBox<M>`-rooted children are arena-backed (the parse
        // path). `Option<Box<M>>`/`Vec<Box<M>>` in hand-written families
        // must not generate walk code that type-checks against `AstBox`.
        // Unwrap exactly one container level; the inner must itself be
        // `AstBox<M>`.
        let inner_is_ast_box = |t: &syn::Type| -> bool {
            let inner: &syn::Type = match container_inner(t) {
                Some((_, inner)) => inner,
                None => t,
            };
            let syn::Type::Path(path) = inner else {
                return false;
            };
            path.path
                .segments
                .last()
                .is_some_and(|s| s.ident == "AstBox")
        };
        let child_box_ident = format_ident!("child_box");
        let body_single = walk_child_body(quote! { *#binding }, member, names);
        let body_opt = walk_child_body(quote! { *#child_box_ident }, member, names);
        let body_list = walk_child_body(quote! { *#child_box_ident }, member, names);
        let walk = match last.as_deref() {
            // Arena-backed children are exactly `AstBox<M>` fields (the
            // parser path). Hand-written families (`Box<M>`, bare `M`)
            // use `__tree_emit_derived` instead and never call the
            // arena walker.
            Some("AstBox") => Some(quote! { { #body_single } }),
            Some("Option") if inner_is_ast_box(ty) => Some(quote! {
                {
                    if let ::std::option::Option::Some(child_box) = #binding.as_ref() {
                        #body_opt
                    }
                }
            }),
            Some("Vec") if inner_is_ast_box(ty) => Some(quote! {
                {
                    for child_box in #binding.iter() {
                        #body_list
                    }
                }
            }),
            _ => None, // hand-written bare/Box children: not arena-backed
        };
        if let Some(walk) = walk {
            walks.push(walk);
        }
    }
    quote! { Self::#vident #pattern => { #(#walks)* } }
}

// ---------------------------------------------------------------------------
// Member surface
// ---------------------------------------------------------------------------

/// Emits one member's payload node + case enums + From + from_parts +
/// tree_kind + __tree_emit.
fn gen_member_surface(member: &GenMember, names: &FamilyNames) -> proc_macro2::TokenStream {
    let ident = &member.ident;
    let node_ident = format_ident!("{}Node", ident);
    let case_ident = format_ident!("{}Case", ident);
    let view_ident = &names.view;
    let union_node_ident = &names.node;
    let member_short = short_of(names, ident);

    let node_variants: Vec<proc_macro2::TokenStream> = member
        .variants
        .iter()
        .map(|variant| {
            let vident = &variant.ident;
            let fields = node_variant_body(variant);
            quote! {
                #vident
                {
                    #(#fields),*
                }
            }
        })
        .collect();

    let case_variants: Vec<proc_macro2::TokenStream> = member
        .variants
        .iter()
        .map(|variant| {
            let vident = &variant.ident;
            let fields = case_variant_fields(variant);
            quote! {
                #vident
                {
                    #(#fields),*
                }
            }
        })
        .collect();

    // `From<&E> for ENode`.
    let from_arms: Vec<proc_macro2::TokenStream> = member
        .variants
        .iter()
        .map(|variant| {
            gen_from_value_arm(variant, ident, &member.variants, &node_ident)
        })
        .collect();

    // `ECase::from_parts`.
    let from_parts_arms: Vec<proc_macro2::TokenStream> = member
        .variants
        .iter()
        .map(|variant| gen_from_parts_arm(variant, &node_ident, &case_ident))
        .collect();

    // `tree_kind` arms.
    let kind_arms: Vec<proc_macro2::TokenStream> = member
        .variants
        .iter()
        .map(|variant| gen_tree_kind_arm(variant, &member.variants))
        .collect();

    // `__tree_emit`.
    let emit_arms: Vec<proc_macro2::TokenStream> = member
        .variants
        .iter()
        .map(|variant| gen_tree_emit_arm(variant, ident, names))
        .collect();

    // `__tree_walk_emit` (arena-backed parse path).
    let walk_arms: Vec<proc_macro2::TokenStream> = member
        .variants
        .iter()
        .map(|variant| gen_tree_walk_arm(variant, names))
        .collect();

    // `__tree_span` and `__tree_emit_derived` (derived syntax ids, plan
    // §6.4). Emitted only when the family declares a `span` field.
    let span_emit: Option<proc_macro2::TokenStream> = member.span_field.map(|_span_index| {
        let variant_span_index = |variant: &GenVariant| -> Option<usize> {
            variant
                .fields
                .iter()
                .position(|field| field.name == "span")
        };
        let span_read_arm = |variant: &GenVariant| -> proc_macro2::TokenStream {
            let vident = &variant.ident;
            let Some(field_index) = variant_span_index(variant) else {
                return quote! {
                    #ident::#vident { .. } => ::std::option::Option::None,
                };
            };
            let (pattern, bindings) = bindings(variant);
            let binding = &bindings[field_index];
            // The span field is a leaf `u64`-typed span (start:u32, end:u32
            // encoded as (start << 32) | end when the field is a single u64).
            quote! {
                #ident::#vident #pattern => {
                    let value = #binding;
                    Some((
                        (value >> 32) as u32,
                        (value & 0xFFFF_FFFF) as u32,
                    ))
                }
            }
        };
        let span_arms: Vec<proc_macro2::TokenStream> = member
            .variants
            .iter()
            .map(|variant| span_read_arm(variant))
            .collect();
        let derived_walk = |binding: &syn::Ident,
                            member_ident: &syn::Ident| {
            quote! {
                let child_id = #member_ident::__tree_emit_derived(handle, uri, #binding)?;
                ::plingo::reactive::api::TreeEmittedExt::move_node(handle, child_id, id)?;
            }
        };
        let emit_arms: Vec<proc_macro2::TokenStream> = member
            .variants
            .iter()
            .filter(|variant| variant_span_index(variant).is_some())
            .map(|variant| {
                let vident = &variant.ident;
                let (pattern, bindings) = bindings(variant);
                let node_ident = format_ident!("{}Node", ident);
                let union_node_ident = &names.node;
                let member_short = short_of(names, ident);
                let mut child_walks: Vec<proc_macro2::TokenStream> = Vec::new();
                for (index, field) in variant.fields.iter().enumerate() {
                    let binding = &bindings[index];
                    if let FieldClass::Child { kind, member } = &field.class {
                        match kind {
                            ChildKind::Single => {
                                child_walks.push(derived_walk(binding, member));
                            }
                            ChildKind::List => {
                                child_walks.push(quote! {
                                    {
                                        for child_value in #binding.iter() {
                                            let child_id = #member::__tree_emit_derived(
                                                handle, uri, child_value,
                                            )?;
                                            ::plingo::reactive::api::TreeEmittedExt::move_node(
                                                handle, child_id, id,
                                            )?;
                                        }
                                    }
                                });
                            }
                            ChildKind::Optional => {
                                child_walks.push(quote! {
                                    {
                                        if let ::std::option::Option::Some(child_value) =
                                            #binding.as_ref()
                                        {
                                            let child_id = #member::__tree_emit_derived(
                                                handle, uri, child_value,
                                            )?;
                                            ::plingo::reactive::api::TreeEmittedExt::move_node(
                                                handle, child_id, id,
                                            )?;
                                        }
                                    }
                                });
                            }
                        }
                    }
                }
                quote! {
                    #ident::#vident #pattern => {
                        #(#child_walks)*
                        let payload: #node_ident = ::std::convert::From::from(value);
                        ::plingo::reactive::api::TreeEmittedExt::upsert_node(
                            handle,
                            id,
                            #union_node_ident::#member_short(payload),
                        )?;
                        ::std::result::Result::Ok(id)
                    }
                }
            })
            .collect();
        quote! {
            impl #ident {
                /// The node's source extent `(start, end)`, decoded from the
                /// `span` leaf field (u64 packing two u32 offsets).
                pub fn __tree_span(&self) -> ::std::option::Option<(u32, u32)> {
                    match self {
                        #(#span_arms)*
                    }
                }

                /// Emits the payload at the derived id
                /// `H(uri ∥ start ∥ end ∥ kind)` and recursively the child
                /// nodes with their own derived ids (plan §6.4). Returns
                /// the node's id.
                pub(crate) fn __tree_emit_derived(
                    handle: &::plingo::reactive::EmittedHandle<#view_ident>,
                    uri: &str,
                    value: &Self,
                ) -> ::plingo::reactive::Result<::plingo::reactive::NodeId> {
                    let (start, end) = value
                        .__tree_span()
                        .ok_or_else(|| ::plingo::reactive::Error::Internal(
                            "abstract-tree node without a span".into(),
                        ))?;
                    let kind = value.tree_kind();
                    let id = #view_ident::id_from_span(uri, start, end, kind);
                    match value {
                        #(#emit_arms)*
                    }
                }
            }
        }
    });

    quote! {
        /// The payload node of one member enum (leaf fields + kind tag).
        #[derive(Clone, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum #node_ident {
            #(#node_variants,)*
        }

        /// The typed case of one member enum (leaf fields + node-id children).
        #[derive(Clone, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum #case_ident {
            #(#case_variants,)*
        }

        impl ::std::convert::From<&#ident> for #node_ident {
            fn from(value: &#ident) -> Self {
                match value {
                    #(#from_arms)*
                }
            }
        }

        impl #case_ident {
            /// Reconstructs a typed case from payload + children (the
            /// two facts `case` reads).
            pub(crate) fn from_parts(
                node: &#node_ident,
                children: &[::plingo::reactive::NodeId],
            ) -> ::plingo::reactive::Result<Self> {
                match node {
                    #(#from_parts_arms),*
                }
            }
        }

        impl #ident {
            /// The variant ordinal (the `kind` dimension of derived ids).
            pub fn tree_kind(&self) -> u8 {
                match self {
                    #(#kind_arms),*
                }
            }

            /// Emits the payload at `id` and recursively the child nodes
            /// with deterministic fresh ids (plan §6.2).
            pub(crate) fn __tree_emit(
                handle: &::plingo::reactive::EmittedHandle<#view_ident>,
                id: ::plingo::reactive::NodeId,
                value: &Self,
            ) -> ::plingo::reactive::Result<()> {
                match value {
                    #(#emit_arms),*
                }
            }

            /// Arena-backed recursive emission (parse path, plan §6.4).
            /// Child values come from the parser's `AstArena`; spans come
            /// from the committed `AstSnapshot`; node ids are derived from
            /// `H(uri ∥ start ∥ end ∥ kind)` so unchanged subtrees keep
            /// their ids across unrelated edits (matrix 2/3). Hand-written
            /// families (no `AstBox` children) never call this; use
            /// `__tree_emit_derived` there.
            #[allow(dead_code)]
            pub fn __tree_walk_emit(
                handle: &::plingo::reactive::EmittedHandle<#view_ident>,
                uri: &str,
                snapshot: &::plingo::framework::parse::AstSnapshot,
                arena: &::plingo::framework::parse::data::AstArena,
                id: ::plingo::reactive::NodeId,
                value: &Self,
            ) -> ::plingo::reactive::Result<()> {
                let payload: #node_ident = ::std::convert::From::from(value);
                ::plingo::reactive::api::TreeEmittedExt::upsert_node(
                    handle,
                    id,
                    #union_node_ident::#member_short(payload),
                )?;
                match value {
                    #(#walk_arms),*
                }
                ::std::result::Result::Ok(())
            }
        }

        #span_emit
    }
}

// ---------------------------------------------------------------------------
// Family surface
// ---------------------------------------------------------------------------

/// The shared surface generated by the root's expansion.
fn gen_family_surface(
    members: &[GenMember],
    names: &FamilyNames,
) -> proc_macro2::TokenStream {
    let view_ident = &names.view;
    let node_ident = &names.node;
    let case_ident = &names.case;
    let observed_trait = &names.observed_trait;
    let emitted_trait = &names.emitted_trait;
    let snapshot_trait = &names.snapshot_trait;
    // The family root: `AbstractTreeFamily` is implemented on it.
    let root_ident = &members[0].ident;

    // Unions.
    let node_variants: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let node_ident = format_ident!("{}Node", member.ident);
            quote! { #short(#node_ident) }
        })
        .collect();
    let case_variants: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let case_ident = format_ident!("{}Case", member.ident);
            quote! { #short(#case_ident) }
        })
        .collect();

    // The `case` dispatch over the payload union (observed).
    let case_dispatch: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let member_case = format_ident!("{}Case", member.ident);
            quote! {
                #node_ident::#short(payload) => {
                    let case = #member_case::from_parts(payload, &children)?;
                    ::std::result::Result::Ok(::std::option::Option::Some(
                        #case_ident::#short(case),
                    ))
                }
            }
        })
        .collect();

    // The snapshot dispatch (Option-returning).
    let snapshot_dispatch: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let member_case = format_ident!("{}Case", member.ident);
            quote! {
                #node_ident::#short(payload) => {
                    let case = #member_case::from_parts(payload, &children).ok()?;
                    ::std::option::Option::Some(#case_ident::#short(case))
                }
            }
        })
        .collect();

    let visit_decls: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let short_name = to_snake_case(&short.to_string());
            let method = format_ident!("visit_{short_name}_each");
            quote! {
                /// Discovers `parent`'s children and invokes `f` for every
                /// child whose payload is a `#short` node (plan §6.3).
                fn #method<F, E>(
                    &self,
                    parent: ::plingo::reactive::NodeId,
                    f: F,
                ) -> ::plingo::reactive::Result<()>
                where
                    F: FnMut(
                            ::plingo::reactive::NodeId,
                            #case_ident,
                        ) -> ::std::result::Result<(), E>
                        + Send
                        + Sync
                        + 'static,
                    E: Into<::plingo::reactive::Error> + 'static;
            }
        })
        .collect();
    let visit_impls: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let short_name = to_snake_case(&short.to_string());
            let method = format_ident!("visit_{short_name}_each");
            quote! {
                fn #method<F, E>(
                    &self,
                    parent: ::plingo::reactive::NodeId,
                    mut f: F,
                ) -> ::plingo::reactive::Result<()>
                where
                    F: FnMut(
                            ::plingo::reactive::NodeId,
                            #case_ident,
                        ) -> ::std::result::Result<(), E>
                        + Send
                        + Sync
                        + 'static,
                    E: Into<::plingo::reactive::Error> + 'static,
                {
                    let handle = ::std::clone::Clone::clone(self);
                    ::plingo::reactive::api::TreeObservedExt::visit_children_each(
                        self,
                        parent,
                        move |child| -> ::std::result::Result<(), ::plingo::reactive::Error> {
                            match <Self as #observed_trait>::case(&handle, child)? {
                                ::std::option::Option::Some(
                                    #case_ident::#short(case_),
                                ) => {
                                    f(child, #case_ident::#short(case_))
                                        .map_err(::std::convert::Into::into)?;
                                    ::std::result::Result::Ok(())
                                }
                                _ => {
                                    // Not this member's payload; skip.
                                    ::std::result::Result::Ok(())
                                }
                            }
                        },
                    )
                }
            }
        })
        .collect();

    let upsert_decls: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let short_name = to_snake_case(&short.to_string());
            let method = format_ident!("upsert_{short_name}");
            let member_ident = &member.ident;
            quote! {
                /// Emits the member value and its subtree (children with
                /// deterministic fresh ids).
                fn #method(
                    &self,
                    id: ::plingo::reactive::NodeId,
                    value: &#member_ident,
                ) -> ::plingo::reactive::Result<()>;
            }
        })
        .collect();
    let upsert_impls: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let short_name = to_snake_case(&short.to_string());
            let method = format_ident!("upsert_{short_name}");
            let member_ident = &member.ident;
            quote! {
                fn #method(
                    &self,
                    id: ::plingo::reactive::NodeId,
                    value: &#member_ident,
                ) -> ::plingo::reactive::Result<()> {
                    #member_ident::__tree_emit(self, id, value)
                }
            }
        })
        .collect();

    quote! {
        /// The tree view of the syntax family.
        #[allow(dead_code)]
        pub struct #view_ident;

        impl ::plingo::reactive::view::ViewSpec for #view_ident {
            type Shape = ::plingo::reactive::view::AbstractTreeShape;
            type Key = ();
            type Value = #node_ident;
            type Edge = ();
            type Label = ();
        }

        /// The family marker impl (`::plingo::framework::parse::AbstractTreeFamily`):
        /// the parser component uses `Self::View` and the arena-backed
        /// walker to publish the syntax tree per document.
        impl ::plingo::framework::parse::AbstractTreeFamily for #root_ident {
            type Node = #node_ident;
            type Case = #case_ident;
            type View = #view_ident;

            fn __tree_walk_emit(
                handle: &::plingo::reactive::EmittedHandle<Self::View>,
                uri: &str,
                snapshot: &::plingo::framework::parse::AstSnapshot,
                arena: &::plingo::framework::parse::data::AstArena,
                id: ::plingo::reactive::NodeId,
                value: &Self,
            ) -> ::plingo::reactive::Result<()> {
                #root_ident::__tree_walk_emit(handle, uri, snapshot, arena, id, value)
            }

            fn __tree_kind_of(value: &Self) -> u8 {
                value.tree_kind()
            }

            fn __tree_view_id(
                uri: &str,
                start: u32,
                end: u32,
                kind: u8,
            ) -> ::plingo::reactive::NodeId {
                #view_ident::id_from_span(uri, start, end, kind)
            }
        }

        impl #view_ident {
            /// Derives a node id from a source region:
            /// `H(uri ∥ start ∥ end ∥ kind ∥ family-salt)` (plan §6.4).
            /// Unchanged regions re-parse to the same id.
            pub fn id_from_span(
                uri: &str,
                start: u32,
                end: u32,
                kind: u8,
            ) -> ::plingo::reactive::NodeId {
                use ::std::hash::{Hash, Hasher};
                let mut hasher = ::std::collections::hash_map::DefaultHasher::new();
                uri.hash(&mut hasher);
                start.hash(&mut hasher);
                end.hash(&mut hasher);
                kind.hash(&mut hasher);
                stringify!(#view_ident).hash(&mut hasher);
                ::plingo::reactive::NodeId(hasher.finish())
            }

            /// Derives a node id for a *member-typed* node. Mixing the
            /// member's `TypeId` into the salt keeps a parent and its
            /// single child distinct even when their extents and variant
            /// ordinals coincide (e.g. a `Lines` document whose one
            /// declaration spans the whole file).
            pub fn id_from_span_typed<M: ?Sized + 'static>(
                uri: &str,
                start: u32,
                end: u32,
                kind: u8,
            ) -> ::plingo::reactive::NodeId {
                use ::std::hash::{Hash, Hasher};
                let mut hasher = ::std::collections::hash_map::DefaultHasher::new();
                uri.hash(&mut hasher);
                start.hash(&mut hasher);
                end.hash(&mut hasher);
                kind.hash(&mut hasher);
                stringify!(#view_ident).hash(&mut hasher);
                ::std::any::TypeId::of::<M>().hash(&mut hasher);
                ::plingo::reactive::NodeId(hasher.finish())
            }
        }

        /// The payload union: one variant per member enum.
        #[derive(Clone, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum #node_ident {
            #(#node_variants,)*
        }

        /// The case union: one variant per member enum.
        #[derive(Clone, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum #case_ident {
            #(#case_variants,)*
        }

        /// Observed-handle surface (plan §6.2–6.3).
        #[allow(dead_code)]
        pub trait #observed_trait {
            /// Reads `node(id)` and `children(id)` — the two exact facts —
            /// and reconstructs the typed case.
            fn case(
                &self,
                id: ::plingo::reactive::NodeId,
            ) -> ::plingo::reactive::Result<::std::option::Option<#case_ident>>;

            #(#visit_decls)*
        }

        impl #observed_trait for ::plingo::reactive::ObservedHandle<#view_ident> {
            fn case(
                &self,
                id: ::plingo::reactive::NodeId,
            ) -> ::plingo::reactive::Result<::std::option::Option<#case_ident>> {
                let payload = ::plingo::reactive::api::TreeObservedExt::node(self, id)?;
                let children = ::plingo::reactive::api::TreeObservedExt::children(self, id)?;
                match payload {
                    ::std::option::Option::None => {
                        ::std::result::Result::Ok(::std::option::Option::None)
                    }
                    ::std::option::Option::Some(payload) => {
                        match &*payload {
                            #(#case_dispatch)*
                        }
                    }
                }
            }

            #(#visit_impls)*
        }

        /// Emitted-handle surface (plan §5.2).
        #[allow(dead_code)]
        pub trait #emitted_trait {
            #(#upsert_decls)*
        }

        impl #emitted_trait for ::plingo::reactive::EmittedHandle<#view_ident> {
            #(#upsert_impls)*
        }

        /// Snapshot surface (plan §6.2 "Snapshot parity").
        #[allow(dead_code)]
        pub trait #snapshot_trait {
            fn case(
                &self,
                id: ::plingo::reactive::NodeId,
            ) -> ::std::option::Option<#case_ident>;
        }

        impl #snapshot_trait for ::plingo::reactive::SnapshotTree<#view_ident> {
            fn case(
                &self,
                id: ::plingo::reactive::NodeId,
            ) -> ::std::option::Option<#case_ident> {
                let payload = ::plingo::reactive::SnapshotTree::node(self, id);
                match payload {
                    ::std::option::Option::Some(payload) => {
                        let children = ::plingo::reactive::SnapshotTree::children(self, id);
                        match &*payload {
                            #(#snapshot_dispatch)*
                        }
                    }
                    ::std::option::Option::None => ::std::option::Option::None,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

/// Expands one `#[abstract_tree(...)]` attribute.
pub(crate) fn expand(
    attr: &AbstractTreeArgs,
    item: proc_macro2::TokenStream,
) -> Result<proc_macro2::TokenStream, syn::Error> {
    let item_enum: ItemEnum = syn::parse2(item)?;
    let members = match &attr.members {
        Some(members) => members.clone(),
        None => vec![item_enum.ident.clone()],
    };
    if members.is_empty() {
        return Err(syn::Error::new_spanned(
            &item_enum,
            "`members(...)` must name at least the family root",
        ));
    }
    if !members.iter().any(|m| m == &item_enum.ident) {
        return Err(syn::Error::new_spanned(
            &item_enum,
            format!(
                "`{}` is not listed in this family's `members(...)`",
                item_enum.ident
            ),
        ));
    }
    let root = members[0].clone();
    let names = family_names(&root, &members);
    let classified = classify_member(&item_enum.ident, &item_enum, &members)?;
    let item_tokens = quote! { #item_enum };
    let member_tokens = gen_member_surface(&classified, &names);
    if item_enum.ident != root {
        return Ok(quote! {
            #item_tokens
            #member_tokens
        });
    }
    // The root also generates the shared family surface.
    let member_list: Vec<GenMember> = members
        .iter()
        .map(|m| GenMember { ident: m.clone(), variants: Vec::new(), span_field: None })
        .collect();
    let family_tokens = gen_family_surface(&member_list, &names);
    Ok(quote! {
        #item_tokens
        #member_tokens
        #family_tokens
    })
}