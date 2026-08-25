//! `#[abstract_tree]` turns a family of syntax enums into typed tree façades.
//! The root expansion owns one uniform reactive view, typed node identities,
//! recursive emission, and committed snapshot reads.
//!
//! Each member expansion generates a payload node, a typed case, and the
//! hidden parser/effect publication methods. The supported API exposes typed
//! nodes and cases; raw selector ids and role handles are never generated.

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

struct GenMember {
    ident: syn::Ident,
    variants: Vec<GenVariant>,
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
            if matches!(
                class,
                FieldClass::Child {
                    kind: ChildKind::List,
                    ..
                }
            ) {
                if saw_list {
                    return Err(syn::Error::new_spanned(
                        field,
                        "multiple Vec children in one variant are not supported; keep the Vec last",
                    ));
                }
                saw_list = true;
            }
            if saw_list
                && !matches!(class, FieldClass::Leaf)
                && !matches!(
                    class,
                    FieldClass::Child {
                        kind: ChildKind::List,
                        ..
                    }
                )
            {
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
    Ok(GenMember {
        ident: ident.clone(),
        variants,
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
    root: syn::Ident,
    view: syn::Ident,
    node: syn::Ident,
    case: syn::Ident,
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
    while prefix_len < root_text.len() && root_text.as_bytes()[prefix_len].is_ascii_lowercase() {
        prefix_len += 1;
    }
    let family = format_ident!("{}", &root_text[..prefix_len]);
    FamilyNames {
        root: root.clone(),
        view: format_ident!("{}Tree", family),
        node: format_ident!("{}Node", family),
        case: format_ident!("{}Case", family),
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
        if matches!(
            field.class,
            FieldClass::Child {
                kind: ChildKind::Optional,
                ..
            }
        ) {
            let has = format_ident!("has_{}", field.name);
            fields.push(quote! { #has: bool });
        }
    }
    fields
}

/// The ECase variant body: leaves owned, children as typed node values.
fn case_variant_fields(
    variant: &GenVariant,
    view_ident: &syn::Ident,
) -> Vec<proc_macro2::TokenStream> {
    let mut fields: Vec<proc_macro2::TokenStream> = Vec::new();
    for field in &variant.fields {
        let name = &field.name;
        match &field.class {
            FieldClass::Leaf => {
                let ty = &field.leaf_ty;
                fields.push(quote! { #name: #ty });
            }
            FieldClass::Child { kind, .. } => {
                let node_ty = quote! { ::plingo::reactive::view::Node<#view_ident> };
                let ty = match kind {
                    ChildKind::Single => quote! { #node_ty },
                    ChildKind::List => quote! { ::std::vec::Vec<#node_ty> },
                    ChildKind::Optional => {
                        quote! { ::std::option::Option<#node_ty> }
                    }
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
                Some((field.name.clone(), format_ident!("__leaf_{}", field.index)))
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
            if matches!(
                field.class,
                FieldClass::Child {
                    kind: ChildKind::Optional,
                    ..
                }
            ) {
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

/// The `ECase::from_parts` match arm — consumes the typed child slice.
fn gen_from_parts_arm(
    variant: &GenVariant,
    node_ident: &syn::Ident,
    case_ident: &syn::Ident,
    view_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    let vident = &variant.ident;
    let mut pattern_fields: Vec<proc_macro2::TokenStream> = vec![quote! { kind: _ }];
    for (name, binding) in leaf_bindings_of(variant) {
        pattern_fields.push(quote! { #name: #binding });
    }
    for (has, binding) in opt_bindings_of(variant) {
        pattern_fields.push(quote! { #has: #binding });
    }
    let node_ty = quote! { ::plingo::reactive::view::Node<#view_ident> };
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
            FieldClass::Child {
                kind: ChildKind::Single,
                ..
            } => {
                case_fields.push(quote! {
                    #name: {
                        if children_cursor < children.len() {
                            let value: #node_ty = children[children_cursor];
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
            FieldClass::Child {
                kind: ChildKind::Optional,
                ..
            } => {
                let binding = opt_bindings_of(variant)
                    .iter()
                    .find(|(_, b)| b.to_string() == format!("__has_{}", field.index))
                    .map(|(_, b)| b.clone())
                    .expect("optional binding");
                case_fields.push(quote! {
                    #name: {
                        if *#binding {
                            if children_cursor < children.len() {
                                let value: #node_ty = children[children_cursor];
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
            FieldClass::Child {
                kind: ChildKind::List,
                ..
            } => {
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
            FieldClass::Child {
                kind: ChildKind::Optional,
                ..
            } => {
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
    let view_ident = &names.view;
    let view_input_ident = format_ident!("{}Input", view_ident);
    let view_output_ident = format_ident!("{}Output", view_ident);
    let mut child_walks: Vec<proc_macro2::TokenStream> = Vec::new();
    for (index, field) in variant.fields.iter().enumerate() {
        let binding = &bindings[index];
        if let FieldClass::Child { kind, member } = &field.class {
            let child_member = member;
            // Arena-backed children are emitted by the parser's span walk.
            let mut ast_box_rooted = false;
            let mut probe: Option<&syn::Type> = Some(&field.leaf_ty);
            while let Some(ty) = probe {
                match ty {
                    syn::Type::Path(path) => {
                        let Some(seg) = path.path.segments.last() else {
                            break;
                        };
                        if seg.ident == "AstBox" {
                            ast_box_rooted = true;
                            break;
                        }
                        if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                            probe = args.args.iter().find_map(|arg| {
                                if let syn::GenericArgument::Type(inner) = arg {
                                    Some(inner)
                                } else {
                                    None
                                }
                            });
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
                        let child_id =
                            ::plingo::reactive::__macro_private::fresh_node_id::<#view_ident>()?;
                        #child_member::__tree_emit(
                            ::std::option::Option::Some(id),
                            child_id,
                            #binding,
                        )?;
                        children.push(child_id);
                    }
                }),
                ChildKind::List => child_walks.push(quote! {
                    {
                        for child_value in #binding.iter() {
                            let child_id =
                                ::plingo::reactive::__macro_private::fresh_node_id::<#view_ident>()?;
                            #child_member::__tree_emit(
                                ::std::option::Option::Some(id),
                                child_id,
                                child_value,
                            )?;
                            children.push(child_id);
                        }
                    }
                }),
                ChildKind::Optional => child_walks.push(quote! {
                    {
                        if let ::std::option::Option::Some(child_value) = #binding.as_ref() {
                            let child_id =
                                ::plingo::reactive::__macro_private::fresh_node_id::<#view_ident>()?;
                            #child_member::__tree_emit(
                                ::std::option::Option::Some(id),
                                child_id,
                                child_value,
                            )?;
                            children.push(child_id);
                        }
                    }
                }),
            }
        }
    }
    quote! {
        Self::#vident #pattern => {
            let mut children: ::std::vec::Vec<
                ::plingo::reactive::view::Node<#view_ident>,
            > = ::std::vec::Vec::new();
            let payload: #node_ident = ::std::convert::From::from(value);
            #(#child_walks)*
            let emit = ::plingo::reactive::kind::emit_view::<#view_ident>()?;
            emit.put(
                ::plingo::reactive::kind::TreeKey::Payload(id),
                ::std::option::Option::Some(::plingo::reactive::kind::TreeFact::Payload(
                    #union_node_ident::#member_short(payload),
                )),
            )?;
            emit.put(
                ::plingo::reactive::kind::TreeKey::Parent(id),
                ::std::option::Option::Some(::plingo::reactive::kind::TreeFact::Parent(
                    parent,
                )),
            )?;
            let order: ::std::sync::Arc<[u64]> = children
                .iter()
                .map(|child| child.raw_id())
                .collect();
            emit.put(
                ::plingo::reactive::kind::TreeKey::ChildOrder(id),
                ::std::option::Option::Some(::plingo::reactive::kind::TreeFact::Order(
                    order,
                )),
            )?;
            for &child in children.iter() {
                emit.put(
                    ::plingo::reactive::kind::TreeKey::ChildLink(id, child.raw_id()),
                    ::std::option::Option::Some(::plingo::reactive::kind::TreeFact::Link(
                        child,
                    )),
                )?;
            }
            Ok(())
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

/// Generates the non-recursive child collection arm used by sparse patches.
fn gen_plain_children_arm(
    variant: &GenVariant,
    names: &FamilyNames,
) -> proc_macro2::TokenStream {
    let vident = &variant.ident;
    let (pattern, bindings) = bindings(variant);
    let root_ident = &names.root;
    let mut children = Vec::new();
    for (index, field) in variant.fields.iter().enumerate() {
        let binding = &bindings[index];
        let FieldClass::Child { kind, member } = &field.class else {
            continue;
        };
        let is_ast_box = |ty: &syn::Type| -> bool {
            let syn::Type::Path(path) = ty else {
                return false;
            };
            path.path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "AstBox")
        };
        let inner_is_ast_box = |ty: &syn::Type| -> bool {
            let Some((_, inner)) = container_inner(ty) else {
                return false;
            };
            is_ast_box(inner)
        };
        let collect_child = |child_box: proc_macro2::TokenStream,
                             child_member: &syn::Ident|
         -> proc_macro2::TokenStream {
            quote! {
                if arena.get(#child_box).is_some() {
                    let record = (#child_box).identity();
                    let child_id = <#root_ident as ::plingo::framework::parse::AbstractTreeFamily>::__tree_plain_node_for_record(
                                uri,
                                arena,
                                record,
                                false,
                                resolver,
                            )
                            .expect("child record must carry a live lineage");
                    children.push(child_id);
                }
            }
        };
        let collect = match kind {
            ChildKind::Single if is_ast_box(&field.leaf_ty) => {
                Some(collect_child(quote! { *#binding }, member))
            }
            ChildKind::Optional if inner_is_ast_box(&field.leaf_ty) => {
                let body = collect_child(quote! { *child_box }, member);
                Some(quote! {
                    if let ::std::option::Option::Some(child_box) = #binding.as_ref() {
                        #body
                    }
                })
            }
            ChildKind::List if inner_is_ast_box(&field.leaf_ty) => {
                let body = collect_child(quote! { *child_box }, member);
                Some(quote! {
                    for child_box in #binding.iter() {
                        #body
                    }
                })
            }
            _ => None,
        };
        if let Some(collect) = collect {
            children.push(collect);
        }
    }
    quote! {
        Self::#vident #pattern => {
            #(#children)*
        }
    }
}


// ---------------------------------------------------------------------------
// Member surface
// ---------------------------------------------------------------------------

/// Emits one member's payload node + case enums + From + from_parts +
/// tree_kind + __tree_emit.
fn gen_member_surface(
    member: &GenMember,
    names: &FamilyNames,
    member_ordinal: u8,
) -> proc_macro2::TokenStream {
    let ident = &member.ident;
    let node_ident = format_ident!("{}Node", ident);
    let case_ident = format_ident!("{}Case", ident);
    let view_ident = &names.view;
    let root_ident = &names.root;
    let view_input_ident = format_ident!("{}Input", view_ident);
    let view_output_ident = format_ident!("{}Output", view_ident);
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
            let fields = case_variant_fields(variant, view_ident);
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
        .map(|variant| gen_from_value_arm(variant, ident, &member.variants, &node_ident))
        .collect();

    // `ECase::from_parts`.
    let from_parts_arms: Vec<proc_macro2::TokenStream> = member
        .variants
        .iter()
        .map(|variant| gen_from_parts_arm(variant, &node_ident, &case_ident, view_ident))
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
    let plain_children_arms: Vec<proc_macro2::TokenStream> = member
        .variants
        .iter()
        .map(|variant| gen_plain_children_arm(variant, names))
        .collect();
    quote! {
        /// The payload node of one member enum (leaf fields + kind tag).
        #[derive(Clone, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum #node_ident {
            #(#node_variants,)*
        }

        /// The typed case of one member enum (leaf fields + typed children).
        #[derive(Clone, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum #case_ident {
            #(#case_variants,)*
        }

        impl #ident {
            /// Stable per-member discriminant for derived node identity:
            /// unlike the variant ordinal it never changes when a parse
            /// revision flips the payload variant (plan §8.7).
            pub(crate) const __MEMBER_ORDINAL: u8 = #member_ordinal;
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
                children: &[::plingo::reactive::view::Node<#view_ident>],
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
            /// through the legacy hand-built tree API.
            pub(crate) fn __tree_emit(
                parent: ::std::option::Option<
                    ::plingo::reactive::view::Node<#view_ident>,
                >,
                id: ::plingo::reactive::view::Node<#view_ident>,
                value: &Self,
            ) -> ::plingo::reactive::Result<()> {
                match value {
                    #(#emit_arms),*
                }
            }

            /// Publishes exactly one arena-backed node's split facts.
            #[doc(hidden)]
            pub(crate) fn __tree_plain_emit_one(
                parent: ::std::option::Option<
                    ::plingo::reactive::view::Node<#view_ident>,
                >,
                uri: &str,
                arena: &::plingo::framework::parse::data::AstArena,
                id: ::plingo::reactive::view::Node<#view_ident>,
                value: &Self,
                resolver: &dyn Fn(u64) -> ::std::option::Option<u64>,
            ) -> ::plingo::reactive::Result<()> {
                let mut children = ::std::vec::Vec::new();
                match value {
                    #(#plain_children_arms),*
                }
                let payload: #node_ident = ::std::convert::From::from(value);
                let patch = ::plingo::reactive::kind::emit_patch::<#view_ident>()?;
                patch.upsert(
                    ::plingo::reactive::kind::TreeKey::Payload(id),
                    ::plingo::reactive::kind::TreeFact::Payload(
                        #union_node_ident::#member_short(payload),
                    ),
                )?;
                patch.upsert(
                    ::plingo::reactive::kind::TreeKey::Parent(id),
                    ::plingo::reactive::kind::TreeFact::Parent(parent),
                )?;
                Self::__tree_plain_emit_links(&patch, id, &children)
            }

            /// Writes one node's ordered child-link ids and one link fact.
            #[doc(hidden)]
            pub(crate) fn __tree_plain_emit_links(
                patch: &::plingo::reactive::kind::TreePatch<#view_ident>,
                id: ::plingo::reactive::view::Node<#view_ident>,
                children: &[::plingo::reactive::view::Node<#view_ident>],
            ) -> ::plingo::reactive::Result<()> {
                let order: ::std::sync::Arc<[u64]> = children
                    .iter()
                    .map(|child| child.raw_id())
                    .collect();
                patch.upsert(
                    ::plingo::reactive::kind::TreeKey::ChildOrder(id),
                    ::plingo::reactive::kind::TreeFact::Order(order),
                )?;
                for &child in children {
                    patch.upsert(
                        ::plingo::reactive::kind::TreeKey::ChildLink(id, child.raw_id()),
                        ::plingo::reactive::kind::TreeFact::Link(child),
                    )?;
                }
                Ok(())
            }
        }

    }
}

// ---------------------------------------------------------------------------
// Family surface
// ---------------------------------------------------------------------------

/// The shared surface generated by the root's expansion.
fn gen_family_surface(members: &[GenMember], names: &FamilyNames) -> proc_macro2::TokenStream {
    let view_ident = &names.view;
    let node_ident = &names.node;
    let case_ident = &names.case;
    let root_ident = &members[0].ident;

    let node_variants: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let member_node = format_ident!("{}Node", member.ident);
            quote! { #short(#member_node) }
        })
        .collect();
    let case_variants: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let member_case = format_ident!("{}Case", member.ident);
            quote! { #short(#member_case) }
        })
        .collect();
    let observe_dispatch: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let member_case = format_ident!("{}Case", member.ident);
            quote! {
                #node_ident::#short(payload) => {
                    let case = #member_case::from_parts(payload, children.as_ref())?;
                    Ok(::std::option::Option::Some(#case_ident::#short(case)))
                }
            }
        })
        .collect();
    let snapshot_dispatch: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let member_case = format_ident!("{}Case", member.ident);
            quote! {
                #node_ident::#short(payload) => {
                    let case = #member_case::from_parts(payload, children.as_ref()).ok()?;
                    ::std::option::Option::Some(#case_ident::#short(case))
                }
            }
        })
        .collect();
    let upsert_methods: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let short = short_of(names, &member.ident);
            let method = format_ident!("upsert_{}", to_snake_case(&short.to_string()));
            let member_ident = &member.ident;
            quote! {
                pub fn #method(
                    id: ::plingo::reactive::view::Node<#view_ident>,
                    value: &#member_ident,
                ) -> ::plingo::reactive::Result<()> {
                    // Upserting preserves the node's existing parent link.
                    let parent =
                        match ::plingo::reactive::kind::observe_view::<Self>()?
                            .fact(
                                ::plingo::reactive::kind::TreeKey::Parent(id),
                                ::plingo::reactive::__macro_private::Temporal::Current,
                            )? {
                            Some(output) => match &*output {
                                ::plingo::reactive::kind::TreeFact::Parent(parent) =>
                                    *parent,
                                _ => ::std::option::Option::None,
                            },
                            ::std::option::Option::None => ::std::option::Option::None,
                        };
                    #member_ident::__tree_emit(parent, id, value)
                }
            }
        })
        .collect();

    let record_node_arms: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let member_ident = &member.ident;
            quote! {
                if let ::std::option::Option::Some(value) =
                    arena.get_id::<#member_ident>(raw_record)
                {
                    let id = if root {
                        Self::__root_node(uri, #member_ident::__MEMBER_ORDINAL)
                    } else {
                        let lineage = resolver(record)
                            .expect("published record must carry a live lineage");
                        let id = Self::__node_from_parts(
                            uri,
                            lineage,
                            #member_ident::__MEMBER_ORDINAL,
                        );
                        id
                    };
                    return ::std::option::Option::Some(id);
                }
            }
        })
        .collect();
    let kind_members: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let member_ident = &member.ident;
            quote! { #member_ident }
        })
        .collect();
    let payload_arms: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let member_ident = &member.ident;
            let member_node = format_ident!("{}Node", member.ident);
            let short = short_of(names, &member.ident);
            quote! {
                if let ::std::option::Option::Some(value) =
                    arena.get_id::<#member_ident>(raw)
                {
                    let id = Self::__tree_plain_node_for_record(
                        uri,
                        arena,
                        record,
                        root,
                        resolver,
                    )
                    .expect("payload record must resolve to a node");
                    let member_payload: #member_node =
                        ::std::convert::From::from(value);
                    let patch = ::plingo::reactive::kind::emit_patch::<#view_ident>()?;
                    patch.upsert(
                        ::plingo::reactive::kind::TreeKey::Payload(id),
                        ::plingo::reactive::kind::TreeFact::Payload(
                            #node_ident::#short(member_payload),
                        ),
                    )?;
                    return Ok(true);
                }
            }
        })
        .collect();
    let emit_record_arms: Vec<proc_macro2::TokenStream> = members
        .iter()
        .map(|member| {
            let member_ident = &member.ident;
            quote! {
                if let ::std::option::Option::Some(value) =
                    arena.get_id::<#member_ident>(raw_record)
                {
                    let id = Self::__tree_plain_node_for_record(
                        uri,
                        arena,
                        record,
                        root,
                        resolver,
                    )
                        .expect("record type was just established");
                    let parent = if root {
                        ::std::option::Option::None
                    } else {
                        arena.parent_of(raw_record).and_then(|parent| {
                            Self::__tree_plain_node_for_record(
                                uri,
                                arena,
                                parent as u64,
                                false,
                                resolver,
                            )
                        })
                    };
                    #member_ident::__tree_plain_emit_one(
                        parent, uri, arena, id, value, resolver,
                    )?;
                    return Ok(true);
                }
            }
        })
        .collect();

    quote! {
        #[allow(dead_code)]
        pub struct #view_ident;

        /// The payload union of the family.
        #[derive(Clone, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum #node_ident {
            #(#node_variants,)*
        }

        /// The typed case union of the family.
        #[derive(Clone, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum #case_ident {
            #(#case_variants,)*
        }

        impl ::plingo::reactive::kind::TreeView for #view_ident {
            type Key = ::std::string::String;
            type Payload = #node_ident;
        }

        impl ::plingo::reactive::kind::ViewKind for #view_ident {
            type Patch = ::plingo::reactive::kind::TreePatch<Self>;
            type Emit = ::plingo::reactive::kind::TreeEmit<Self>;
            type Observe = ::plingo::reactive::kind::TreeObserve<Self>;
        }

        impl ::plingo::framework::parse::AbstractTreeFamily for #root_ident {
            type Node = #node_ident;
            type Case = #case_ident;
            type View = #view_ident;

            fn __tree_plain_emit_one(
                parent: ::std::option::Option<
                    ::plingo::reactive::view::Node<#view_ident>,
                >,
                uri: &str,
                arena: &::plingo::framework::parse::data::AstArena,
                id: ::plingo::reactive::view::Node<#view_ident>,
                value: &Self,
                resolver: &dyn Fn(u64) -> ::std::option::Option<u64>,
            ) -> ::plingo::reactive::Result<()> {
                #root_ident::__tree_plain_emit_one(
                    parent, uri, arena, id, value, resolver,
                )
            }

            fn __tree_plain_node_for_record(
                uri: &str,
                arena: &::plingo::framework::parse::data::AstArena,
                record: u64,
                root: bool,
                resolver: &dyn Fn(u64) -> ::std::option::Option<u64>,
            ) -> ::std::option::Option<::plingo::reactive::view::Node<#view_ident>> {
                let raw_record = usize::try_from(record).ok()?;
                #(#record_node_arms)*
                ::std::option::Option::None
            }

            /// The payload variant ordinal of one arena record.
            fn __tree_member_kind_of(
                arena: &::plingo::framework::parse::data::AstArena,
                record: u64,
            ) -> ::std::option::Option<u8> {
                let raw = usize::try_from(record).ok()?;
                #(
                    if let ::std::option::Option::Some(value) =
                        arena.get_id::<#kind_members>(raw)
                    {
                        return ::std::option::Option::Some(value.tree_kind());
                    }
                )*
                ::std::option::Option::None
            }

            /// Writes ONLY the payload fact of one record (plan §12 step 1).
            fn __tree_refresh_payload(
                uri: &str,
                arena: &::plingo::framework::parse::data::AstArena,
                record: u64,
                root: bool,
                resolver: &dyn Fn(u64) -> ::std::option::Option<u64>,
            ) -> ::plingo::reactive::Result<bool> {
                let Some(raw) = usize::try_from(record).ok() else {
                    return Ok(false);
                };
                #(#payload_arms)*
                Ok(false)
            }

            fn __tree_plain_emit_record(
                uri: &str,
                arena: &::plingo::framework::parse::data::AstArena,
                record: u64,
                root: bool,
                resolver: &dyn Fn(u64) -> ::std::option::Option<u64>,
            ) -> ::plingo::reactive::Result<bool> {
                let Some(raw_record) = usize::try_from(record).ok() else {
                    return Ok(false);
                };
                #(#emit_record_arms)*
                Ok(false)
            }

            fn __tree_plain_emit_roots(
                uri: &str,
                roots: ::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>,
            ) -> ::plingo::reactive::Result<()> {
                let patch = ::plingo::reactive::kind::emit_patch::<#view_ident>()?;
                let order: ::std::sync::Arc<[u64]> = roots
                    .iter()
                    .map(|root| (*root).raw_id())
                    .collect();
                patch.upsert(
                    ::plingo::reactive::kind::TreeKey::RootOrder(uri.to_string()),
                    ::plingo::reactive::kind::TreeFact::RootOrder(order),
                )?;
                for &root in roots.iter() {
                    patch.upsert(
                        ::plingo::reactive::kind::TreeKey::RootLink(
                            uri.to_string(),
                            root.raw_id(),
                        ),
                        ::plingo::reactive::kind::TreeFact::RootLink(root),
                    )?;
                }
                Ok(())
            }

            /// Retracts one arena-backed record's split facts: payload,
            /// parent, child order, and the surviving parent's link to this
            /// record. Descendant records are retracted by their own calls.
            #[doc(hidden)]
            fn __tree_plain_remove_record(
                uri: &str,
                arena: &::plingo::framework::parse::data::AstArena,
                record: u64,
                resolver: &dyn Fn(u64) -> ::std::option::Option<u64>,
            ) -> ::plingo::reactive::Result<bool> {
                let Some(raw_record) = usize::try_from(record).ok() else {
                    return Ok(false);
                };
                let Some(id) =
                    Self::__tree_plain_node_for_record(uri, arena, record, false, resolver)
                else {
                    return Ok(false);
                };
                let patch = ::plingo::reactive::kind::emit_patch::<#view_ident>()?;
                patch.remove(::plingo::reactive::kind::TreeKey::Payload(id))?;
                patch.remove(::plingo::reactive::kind::TreeKey::Parent(id))?;
                patch.remove(::plingo::reactive::kind::TreeKey::ChildOrder(id))?;
                if let ::std::option::Option::Some(parent_record) = arena.parent_of(raw_record)
                    && let ::std::option::Option::Some(parent) = Self::__tree_plain_node_for_record(
                        uri,
                        arena,
                        parent_record as u64,
                        false,
                        resolver,
                    )
                {
                    patch.remove(::plingo::reactive::kind::TreeKey::ChildLink(
                        parent,
                        id.raw_id(),
                    ))?;
                }
                Ok(true)
            }
            fn __tree_kind_of(value: &Self) -> u8 {
                value.tree_kind()
            }
        }

        impl #view_ident {
            /// The anonymous domain key behind the uri-less legacy API.
            #[doc(hidden)]
            pub fn anonymous_key() -> ::std::string::String {
                ::std::string::String::new()
            }

            /// Emits one root value and returns its opaque typed identity.
            pub fn emit_root(
                value: &#root_ident,
            ) -> ::plingo::reactive::Result<::plingo::reactive::view::Node<#view_ident>> {
                let root =
                    ::plingo::reactive::__macro_private::fresh_node_id::<Self>()?;
                Self::append_root(Self::anonymous_key(), root)?;
                #root_ident::__tree_emit(::std::option::Option::None, root, value)?;
                Ok(root)
            }

            /// Appends one identity to a domain key's root list.
            #[doc(hidden)]
            pub fn append_root(
                key: ::std::string::String,
                root: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<()> {
                let mut roots = Self::observe_roots_of(&key)?;
                roots.push(root);
                Self::replace_roots_of(key, roots)
            }

            /// Replaces the anonymous domain's root list (legacy API).
            pub fn emit_roots(
                roots: ::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>,
            ) -> ::plingo::reactive::Result<()> {
                Self::replace_roots_of(Self::anonymous_key(), roots)
            }

            /// Replaces one domain key's root list.
            pub fn replace_roots_of(
                key: ::std::string::String,
                roots: ::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>,
            ) -> ::plingo::reactive::Result<()> {
                let emit = ::plingo::reactive::kind::emit_view::<Self>()?;
                emit.put(
                    ::plingo::reactive::kind::TreeKey::RootOrder(key.clone()),
                    ::std::option::Option::Some(::plingo::reactive::kind::TreeFact::RootOrder(
                        roots.iter().map(|root| (*root).raw_id()).collect(),
                    )),
                )?;
                for &root in roots.iter() {
                    emit.put(
                        ::plingo::reactive::kind::TreeKey::RootLink(key.clone(), root.raw_id()),
                        ::std::option::Option::Some(::plingo::reactive::kind::TreeFact::RootLink(
                            root,
                        )),
                    )?;
                }
                Ok(())
            }

            pub fn observe_case(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<::std::option::Option<#case_ident>> {
                let observe = ::plingo::reactive::kind::observe_view::<Self>()?;
                let Some(output) = observe.fact(
                        ::plingo::reactive::kind::TreeKey::Payload(id),
                        ::plingo::reactive::__macro_private::Temporal::Current,
                    )? else {
                    return Ok(::std::option::Option::None);
                };
                let children = Self::observe_children(id)?;
                match &*output {
                    ::plingo::reactive::kind::TreeFact::Payload(payload) => {
                        match payload {
                            #(#observe_dispatch)*
                        }
                    }
                    _ => Ok(::std::option::Option::None),
                }
            }

            pub fn observe_node(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<
                ::std::option::Option<::std::sync::Arc<#node_ident>>,
            > {
                let Some(output) = ::plingo::reactive::kind::observe_view::<Self>()?
                    .fact(
                        ::plingo::reactive::kind::TreeKey::Payload(id),
                        ::plingo::reactive::__macro_private::Temporal::Current,
                    )? else {
                    return Ok(::std::option::Option::None);
                };
                Ok(match &*output {
                    ::plingo::reactive::kind::TreeFact::Payload(payload) =>
                        Some(::std::sync::Arc::new(payload.clone())),
                    _ => None,
                })
            }

            pub fn observe_children(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<
                ::std::sync::Arc<::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>>,
            > {
                let observe = ::plingo::reactive::kind::observe_view::<Self>()?;
                let Some(output) = observe.fact(
                        ::plingo::reactive::kind::TreeKey::ChildOrder(id),
                        ::plingo::reactive::__macro_private::Temporal::Current,
                    )? else {
                    return Ok(::std::sync::Arc::new(::std::vec::Vec::new()));
                };
                Ok(match &*output {
                    ::plingo::reactive::kind::TreeFact::Order(order) => {
                        let mut children = ::std::vec::Vec::with_capacity(order.len());
                        for link in order.iter() {
                            if let ::std::option::Option::Some(fact) = observe.fact(
                                ::plingo::reactive::kind::TreeKey::ChildLink(id, *link),
                                ::plingo::reactive::__macro_private::Temporal::Current,
                            )? {
                                if let ::plingo::reactive::kind::TreeFact::Link(child) =
                                    &*fact
                                {
                                    children.push(*child);
                                }
                            }
                        }
                        ::std::sync::Arc::new(children)
                    }
                    _ => ::std::sync::Arc::new(::std::vec::Vec::new()),
                })
            }

            /// Reads one node's parent from its own fact (no domain scan).
            pub fn observe_parent(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<
                ::std::option::Option<::plingo::reactive::view::Node<#view_ident>>,
            > {
                let Some(output) = ::plingo::reactive::kind::observe_view::<Self>()?
                    .fact(
                        ::plingo::reactive::kind::TreeKey::Parent(id),
                        ::plingo::reactive::__macro_private::Temporal::Current,
                    )? else {
                    return Ok(::std::option::Option::None);
                };
                Ok(match &*output {
                    ::plingo::reactive::kind::TreeFact::Parent(parent) => *parent,
                    _ => None,
                })
            }

            pub fn node(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<
                ::std::option::Option<::std::sync::Arc<#node_ident>>,
            > {
                Self::observe_node(id)
            }

            pub fn children(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<
                ::std::sync::Arc<::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>>,
            > {
                Self::observe_children(id)
            }

            pub fn parent(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<
                ::std::option::Option<::plingo::reactive::view::Node<#view_ident>>,
            > {
                Self::observe_parent(id)
            }

            /// Aggregates every domain key's committed root list.
            pub fn roots(
            ) -> ::plingo::reactive::Result<
                ::std::sync::Arc<::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>>,
            > {
                let observe = ::plingo::reactive::kind::observe_view::<Self>()?;
                let mut roots = ::std::vec::Vec::new();
                for input in observe.all_keys(::plingo::reactive::__macro_private::Temporal::Current)? {
                    if let ::plingo::reactive::kind::TreeKey::RootOrder(key) = &input {
                        if let Some(output) = observe.fact(
                            ::plingo::reactive::kind::TreeKey::RootOrder(key.clone()),
                            ::plingo::reactive::__macro_private::Temporal::Current,
                        )?
                        {
                            if let ::plingo::reactive::kind::TreeFact::RootOrder(order) = &*output {
                                for link in order.iter() {
                                    if let ::std::option::Option::Some(link_fact) = observe.fact(
                                        ::plingo::reactive::kind::TreeKey::RootLink(key.clone(), *link),
                                        ::plingo::reactive::__macro_private::Temporal::Current,
                                    )? {
                                        if let ::plingo::reactive::kind::TreeFact::RootLink(
                                            root,
                                        ) = &*link_fact
                                        {
                                            roots.push(*root);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(::std::sync::Arc::new(roots))
            }

            pub fn observe_roots(
            ) -> ::plingo::reactive::Result<
                ::std::sync::Arc<::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>>,
            > {
                Self::roots()
            }

            /// Reads one domain key's committed root list.
            pub fn observe_roots_of(
                key: &::std::string::String,
            ) -> ::plingo::reactive::Result<
                ::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>,
            > {
                let observe = ::plingo::reactive::kind::observe_view::<Self>()?;
                let Some(output) = observe.fact(
                    ::plingo::reactive::kind::TreeKey::RootOrder(key.clone()),
                    ::plingo::reactive::__macro_private::Temporal::Current,
                )? else {
                    return Ok(::std::vec::Vec::new());
                };
                Ok(match &*output {
                    ::plingo::reactive::kind::TreeFact::RootOrder(order) => {
                        let mut roots = ::std::vec::Vec::with_capacity(order.len());
                        for link in order.iter() {
                            if let ::std::option::Option::Some(link_fact) = observe.fact(
                                ::plingo::reactive::kind::TreeKey::RootLink(key.clone(), *link),
                                ::plingo::reactive::__macro_private::Temporal::Current,
                            )? {
                                if let ::plingo::reactive::kind::TreeFact::RootLink(root) =
                                    &*link_fact
                                {
                                    roots.push(*root);
                                }
                            }
                        }
                        roots
                    }
                    _ => ::std::vec::Vec::new(),
                })
            }

            pub fn previous_case(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<::std::option::Option<#case_ident>> {
                let observe = ::plingo::reactive::kind::observe_view::<Self>()?;
                let Some(output) = observe.fact(
                        ::plingo::reactive::kind::TreeKey::Payload(id),
                        ::plingo::reactive::__macro_private::Temporal::Previous,
                    )? else {
                    return Ok(::std::option::Option::None);
                };
                let children = Self::previous_children(id)?;
                match &*output {
                    ::plingo::reactive::kind::TreeFact::Payload(payload) => {
                        match payload {
                            #(#observe_dispatch)*
                        }
                    }
                    _ => Ok(::std::option::Option::None),
                }
            }

            pub fn previous_node(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<
                ::std::option::Option<::std::sync::Arc<#node_ident>>,
            > {
                let Some(output) = ::plingo::reactive::kind::observe_view::<Self>()?
                    .fact(
                        ::plingo::reactive::kind::TreeKey::Payload(id),
                        ::plingo::reactive::__macro_private::Temporal::Previous,
                    )? else {
                    return Ok(::std::option::Option::None);
                };
                Ok(match &*output {
                    ::plingo::reactive::kind::TreeFact::Payload(payload) =>
                        Some(::std::sync::Arc::new(payload.clone())),
                    _ => None,
                })
            }

            pub fn previous_children(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<
                ::std::sync::Arc<::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>>,
            > {
                let observe = ::plingo::reactive::kind::observe_view::<Self>()?;
                let Some(output) = observe.fact(
                        ::plingo::reactive::kind::TreeKey::ChildOrder(id),
                        ::plingo::reactive::__macro_private::Temporal::Previous,
                    )? else {
                    return Ok(::std::sync::Arc::new(::std::vec::Vec::new()));
                };
                Ok(match &*output {
                    ::plingo::reactive::kind::TreeFact::Order(order) => {
                        let mut children = ::std::vec::Vec::with_capacity(order.len());
                        for link in order.iter() {
                            if let ::std::option::Option::Some(fact) = observe.fact(
                                ::plingo::reactive::kind::TreeKey::ChildLink(id, *link),
                                ::plingo::reactive::__macro_private::Temporal::Previous,
                            )? {
                                if let ::plingo::reactive::kind::TreeFact::Link(child) =
                                    &*fact
                                {
                                    children.push(*child);
                                }
                            }
                        }
                        ::std::sync::Arc::new(children)
                    }
                    _ => ::std::sync::Arc::new(::std::vec::Vec::new()),
                })
            }

            pub fn previous_parent(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<
                ::std::option::Option<::plingo::reactive::view::Node<#view_ident>>,
            > {
                let Some(output) = ::plingo::reactive::kind::observe_view::<Self>()?
                    .fact(
                        ::plingo::reactive::kind::TreeKey::Parent(id),
                        ::plingo::reactive::__macro_private::Temporal::Previous,
                    )? else {
                    return Ok(::std::option::Option::None);
                };
                Ok(match &*output {
                    ::plingo::reactive::kind::TreeFact::Parent(parent) => *parent,
                    _ => None,
                })
            }

            /// Aggregates every domain key's previous-epoch root list.
            pub fn previous_roots(
            ) -> ::plingo::reactive::Result<
                ::std::sync::Arc<::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>>,
            > {
                let observe = ::plingo::reactive::kind::observe_view::<Self>()?;
                let mut roots = ::std::vec::Vec::new();
                for input in observe.all_keys(::plingo::reactive::__macro_private::Temporal::Previous)? {
                    if let ::plingo::reactive::kind::TreeKey::RootOrder(key) = &input {
                        if let Some(output) = observe.fact(
                            ::plingo::reactive::kind::TreeKey::RootOrder(key.clone()),
                            ::plingo::reactive::__macro_private::Temporal::Previous,
                        )?
                        {
                            if let ::plingo::reactive::kind::TreeFact::RootOrder(order) = &*output {
                                for link in order.iter() {
                                    if let ::std::option::Option::Some(link_fact) = observe.fact(
                                        ::plingo::reactive::kind::TreeKey::RootLink(key.clone(), *link),
                                        ::plingo::reactive::__macro_private::Temporal::Previous,
                                    )? {
                                        if let ::plingo::reactive::kind::TreeFact::RootLink(
                                            root,
                                        ) = &*link_fact
                                        {
                                            roots.push(*root);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(::std::sync::Arc::new(roots))
            }

            pub fn emit_node(
                id: ::plingo::reactive::view::Node<#view_ident>,
                payload: #node_ident,
                children: ::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>,
            ) -> ::plingo::reactive::Result<()> {
                let emit = ::plingo::reactive::kind::emit_view::<Self>()?;
                emit.put(
                    ::plingo::reactive::kind::TreeKey::Payload(id),
                    ::std::option::Option::Some(::plingo::reactive::kind::TreeFact::Payload(payload)),
                )?;
                emit.put(
                    ::plingo::reactive::kind::TreeKey::Parent(id),
                    ::std::option::Option::Some(::plingo::reactive::kind::TreeFact::Parent(
                        ::std::option::Option::None,
                    )),
                )?;
                let order: ::std::sync::Arc<[u64]> = children
                    .iter()
                    .map(|child| (*child).raw_id())
                    .collect();
                emit.put(
                    ::plingo::reactive::kind::TreeKey::ChildOrder(id),
                    ::std::option::Option::Some(::plingo::reactive::kind::TreeFact::Order(order)),
                )?;
                for &child in children.iter() {
                    emit.put(
                        ::plingo::reactive::kind::TreeKey::ChildLink(id, child.raw_id()),
                        ::std::option::Option::Some(::plingo::reactive::kind::TreeFact::Link(
                            child,
                        )),
                    )?;
                }
                Ok(())
            }

            pub fn remove_node(
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::plingo::reactive::Result<()> {
                let emit = ::plingo::reactive::kind::emit_view::<Self>()?;
                emit.put(
                    ::plingo::reactive::kind::TreeKey::Payload(id),
                    ::std::option::Option::None,
                )?;
                emit.put(
                    ::plingo::reactive::kind::TreeKey::Parent(id),
                    ::std::option::Option::None,
                )?;
                emit.put(
                    ::plingo::reactive::kind::TreeKey::ChildOrder(id),
                    ::std::option::Option::None,
                )
            }

            pub fn snapshot_parent(
                snapshot: &::plingo::reactive::Snapshot,
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::std::option::Option<
                ::plingo::reactive::view::Node<#view_ident>,
            > {
                let fact = snapshot.observe::<Self>(
                    ::plingo::reactive::kind::TreeKey::Parent(id),
                );
                match fact.as_deref() {
                    Some(::plingo::reactive::kind::TreeFact::Parent(parent)) => *parent,
                    _ => ::std::option::Option::None,
                }
            }

            pub fn snapshot_case(
                snapshot: &::plingo::reactive::Snapshot,
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::std::option::Option<#case_ident> {
                let output = snapshot.observe::<Self>(
                    ::plingo::reactive::kind::TreeKey::Payload(id),
                );
                let children = Self::snapshot_children(snapshot, id);
                match output.as_deref() {
                    Some(::plingo::reactive::kind::TreeFact::Payload(payload)) => {
                        match payload {
                            #(#snapshot_dispatch)*
                        }
                    }
                    _ => ::std::option::Option::None,
                }
            }

            pub fn snapshot_node(
                snapshot: &::plingo::reactive::Snapshot,
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::std::option::Option<::std::sync::Arc<#node_ident>> {
                let output = snapshot.observe::<Self>(
                    ::plingo::reactive::kind::TreeKey::Payload(id),
                );
                match output.as_deref() {
                    Some(::plingo::reactive::kind::TreeFact::Payload(payload)) =>
                        Some(::std::sync::Arc::new(payload.clone())),
                    _ => None,
                }
            }

            pub fn snapshot_children(
                snapshot: &::plingo::reactive::Snapshot,
                id: ::plingo::reactive::view::Node<#view_ident>,
            ) -> ::std::sync::Arc<::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>> {
                let output = snapshot.observe::<Self>(
                    ::plingo::reactive::kind::TreeKey::ChildOrder(id),
                );
                match output.as_deref() {
                    Some(::plingo::reactive::kind::TreeFact::Order(order)) => {
                        let mut children = ::std::vec::Vec::with_capacity(order.len());
                        for link in order.iter() {
                            if let ::std::option::Option::Some(fact) = snapshot.observe::<Self>(
                                ::plingo::reactive::kind::TreeKey::ChildLink(id, *link),
                            ) {
                                if let ::plingo::reactive::kind::TreeFact::Link(child) = &*fact {
                                    children.push(*child);
                                }
                            }
                        }
                        ::std::sync::Arc::new(children)
                    }
                    _ => ::std::sync::Arc::new(::std::vec::Vec::new()),
                }
            }

            /// Aggregates every domain key's committed root list.
            pub fn snapshot_roots(
                snapshot: &::plingo::reactive::Snapshot,
            ) -> ::std::sync::Arc<::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>>> {
                let mut roots = ::std::vec::Vec::new();
                for input in snapshot.inputs::<Self>() {
                    if let ::plingo::reactive::kind::TreeKey::RootOrder(key) = &input {
                        if let Some(output) = snapshot.observe::<Self>(input.clone()) {
                            if let ::plingo::reactive::kind::TreeFact::RootOrder(order) = &*output {
                                for link in order.iter() {
                                    if let ::std::option::Option::Some(link_fact) =
                                        snapshot.observe::<Self>(
                                            ::plingo::reactive::kind::TreeKey::RootLink(
                                                key.clone(),
                                                *link,
                                            ),
                                        )
                                    {
                                        if let ::plingo::reactive::kind::TreeFact::RootLink(
                                            root,
                                        ) = &*link_fact
                                        {
                                            roots.push(*root);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                ::std::sync::Arc::new(roots)
            }

            /// Reads one domain key's committed root list.
            pub fn snapshot_roots_of(
                snapshot: &::plingo::reactive::Snapshot,
                key: &::std::string::String,
            ) -> ::std::vec::Vec<::plingo::reactive::view::Node<#view_ident>> {
                let fact = snapshot.observe::<Self>(
                    ::plingo::reactive::kind::TreeKey::RootOrder(key.clone()),
                );
                let Some(::plingo::reactive::kind::TreeFact::RootOrder(order)) = fact.as_deref()
                else {
                    return ::std::vec::Vec::new();
                };
                let mut roots = ::std::vec::Vec::with_capacity(order.len());
                for link in order.iter() {
                    if let ::std::option::Option::Some(link_fact) = snapshot.observe::<Self>(
                        ::plingo::reactive::kind::TreeKey::RootLink(key.clone(), *link),
                    ) {
                        if let ::plingo::reactive::kind::TreeFact::RootLink(root) = &*link_fact {
                            roots.push(*root);
                        }
                    }
                }
                roots
            }

            #(#upsert_methods)*
        }

        impl ::plingo::reactive::view::View for #view_ident {
            type Input = ::plingo::reactive::kind::TreeKey<
                ::std::string::String,
                ::plingo::reactive::view::Node<Self>,
            >;
            type Output = ::plingo::reactive::kind::TreeFact<
                ::plingo::reactive::view::Node<Self>,
                #node_ident,
            >;

            fn name() -> &'static str { stringify!(#view_ident) }

            fn __shared_writes() -> bool {
                true
            }

            #[doc(hidden)]
            fn __register(
                effect: &::plingo::reactive::__macro_private::EffectContext,
            ) -> ::plingo::reactive::Result<()> {
                effect.register::<Self>()
            }

            #[doc(hidden)]
            fn __observe(
                effect: &::plingo::reactive::__macro_private::EffectContext,
                input: Self::Input,
                temporal: ::plingo::reactive::__macro_private::Temporal,
            ) -> ::plingo::reactive::Result<Option<::std::sync::Arc<Self::Output>>> {
                effect.observe::<Self>(input, temporal)
            }

            #[doc(hidden)]
            fn __inputs(
                effect: &::plingo::reactive::__macro_private::EffectContext,
                temporal: ::plingo::reactive::__macro_private::Temporal,
            ) -> ::plingo::reactive::Result<::std::vec::Vec<Self::Input>> {
                effect.inputs::<Self>(temporal)
            }

            #[doc(hidden)]
            fn __emit(
                effect: &::plingo::reactive::__macro_private::EffectContext,
                input: Self::Input,
                output: Option<Self::Output>,
            ) -> ::plingo::reactive::Result<()> {
                effect.emit::<Self>(input, output)
            }

            #[doc(hidden)]
            fn __snapshot(
                snapshot: &::plingo::reactive::Snapshot,
                input: Self::Input,
            ) -> Option<::std::sync::Arc<Self::Output>> {
                snapshot.__plain_observe::<Self>(input)
            }

            #[doc(hidden)]
            fn __snapshot_inputs(
                snapshot: &::plingo::reactive::Snapshot,
            ) -> ::std::vec::Vec<Self::Input> {
                snapshot.__plain_inputs::<Self>()
            }
        }
    }
}

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
    let member_ordinal = members
        .iter()
        .position(|member| member == &item_enum.ident)
        .expect("member presence checked above");
    let member_tokens = gen_member_surface(&classified, &names, member_ordinal as u8);
    if item_enum.ident != root {
        return Ok(quote! {
            #item_tokens
            #member_tokens
        });
    }
    // The root also generates the shared family surface.
    let member_list: Vec<GenMember> = members
        .iter()
        .map(|m| GenMember {
            ident: m.clone(),
            variants: Vec::new(),
        })
        .collect();
    let family_tokens = gen_family_surface(&member_list, &names);
    Ok(quote! {
        #item_tokens
        #member_tokens
        #family_tokens
    })
}
