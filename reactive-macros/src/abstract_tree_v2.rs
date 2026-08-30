use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{
    Fields, GenericArgument, Generics, ItemEnum, Path, PathArguments, Type, TypeParam,
    WherePredicate,
};
#[derive(Default)]
pub(crate) struct Args {
    pub tree: Option<Path>,
    pub domain: Option<Type>,
    pub members: Option<Vec<Path>>,
    pub member_of: Option<Path>,
    pub syntax: bool,
}

impl Args {
    pub(crate) fn parse(&mut self, meta: syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
        if meta.path.is_ident("tree") {
            if self.tree.is_some() {
                return Err(meta.error("duplicate `tree`"));
            }
            self.tree = Some(meta.value()?.parse()?);
        } else if meta.path.is_ident("domain") {
            if self.domain.is_some() {
                return Err(meta.error("duplicate `domain`"));
            }
            self.domain = Some(meta.value()?.parse()?);
        } else if meta.path.is_ident("member_of") {
            if self.member_of.is_some() {
                return Err(meta.error("duplicate `member_of`"));
            }
            self.member_of = Some(meta.value()?.parse()?);
        } else if meta.path.is_ident("members") {
            if self.members.is_some() {
                return Err(meta.error("duplicate `members`"));
            }
            let mut members = Vec::new();
            meta.parse_nested_meta(|nested| {
                members.push(nested.path.clone());
                Ok(())
            })?;
            self.members = Some(members);
        } else if meta.path.is_ident("syntax") {
            if self.syntax {
                return Err(meta.error("duplicate `syntax`"));
            }
            self.syntax = true;
        } else {
            return Err(meta.error("unsupported `abstract_tree` property; expected tree, domain, members, member_of, or syntax"));
        }
        Ok(())
    }

    pub(crate) fn is_v2(&self) -> bool {
        self.tree.is_some()
            || self.domain.is_some()
            || self.member_of.is_some()
            || self.members.is_none()
    }
}

#[derive(Clone)]
enum Shape {
    Leaf,
    Child(Type),
    Optional(Type),
    List(Type),
}

struct Field {
    name: syn::Ident,
    ty: Type,
    shape: Shape,
}
struct Variant {
    ident: syn::Ident,
    fields: Vec<Field>,
    named: bool,
}

fn last_ident(path: &Path) -> Option<&syn::Ident> {
    path.segments.last().map(|segment| &segment.ident)
}
fn type_path_name(ty: &Type) -> Option<&syn::Ident> {
    match ty {
        Type::Path(path) => last_ident(&path.path),
        _ => None,
    }
}

fn angle_type(path: &Path) -> Option<Type> {
    let segment = path.segments.last()?;
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    let mut types = arguments.args.iter().filter_map(|argument| match argument {
        GenericArgument::Type(ty) => Some(ty.clone()),
        _ => None,
    });
    let ty = types.next()?;
    types.next().is_none().then_some(ty)
}
fn ast_box_inner(ty: &Type) -> Option<Type> {
    let Type::Path(path) = ty else { return None };
    (type_path_name(ty).is_some_and(|name| name == "AstBox"))
        .then(|| angle_type(&path.path))
        .flatten()
}
fn contains_ast_box(ty: &Type) -> bool {
    match ty {
        Type::Path(path) => {
            if type_path_name(ty).is_some_and(|name| name == "AstBox") {
                return true;
            }
            let Some(segment) = path.path.segments.last() else {
                return false;
            };
            let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                return false;
            };
            arguments.args.iter().any(|argument| match argument {
                GenericArgument::Type(inner) => contains_ast_box(inner),
                _ => false,
            })
        }
        Type::Reference(reference) => contains_ast_box(reference.elem.as_ref()),
        Type::Ptr(pointer) => contains_ast_box(pointer.elem.as_ref()),
        Type::Slice(slice) => contains_ast_box(slice.elem.as_ref()),
        Type::Array(array) => contains_ast_box(array.elem.as_ref()),
        Type::Tuple(tuple) => tuple.elems.iter().any(contains_ast_box),
        Type::Paren(paren) => contains_ast_box(paren.elem.as_ref()),
        Type::Group(group) => contains_ast_box(group.elem.as_ref()),
        Type::BareFn(function) => {
            function
                .inputs
                .iter()
                .any(|argument| contains_ast_box(&argument.ty))
                || matches!(
                    &function.output,
                    syn::ReturnType::Type(_, output) if contains_ast_box(output.as_ref())
                )
        }
        Type::ImplTrait(bounds) => bounds.bounds.iter().any(|bound| match bound {
            syn::TypeParamBound::Trait(bound) => bound.path.segments.iter().any(|segment| {
                matches!(
                    &segment.arguments,
                    PathArguments::AngleBracketed(arguments)
                        if arguments.args.iter().any(|argument| matches!(
                            argument,
                            GenericArgument::Type(inner) if contains_ast_box(inner)
                        ))
                )
            }),
            _ => false,
        }),
        _ => false,
    }
}
fn classify(ty: &Type) -> syn::Result<Shape> {
    if let Some(inner) = ast_box_inner(ty) {
        return Ok(Shape::Child(inner));
    }
    let Type::Path(path) = ty else {
        return if contains_ast_box(ty) {
            Err(syn::Error::new_spanned(
                ty,
                "unsupported wrapper around `AstBox`; use AstBox<T>, Option<AstBox<T>>, or Vec<AstBox<T>>",
            ))
        } else {
            Ok(Shape::Leaf)
        };
    };
    let name = type_path_name(ty)
        .map(ToString::to_string)
        .unwrap_or_default();
    if matches!(name.as_str(), "Option" | "Vec") {
        let Some(inner) = angle_type(&path.path) else {
            return Err(syn::Error::new_spanned(
                ty,
                "tree wrapper must contain exactly one type",
            ));
        };
        let Some(child) = ast_box_inner(&inner) else {
            if contains_ast_box(&inner) {
                return Err(syn::Error::new_spanned(
                    ty,
                    "unsupported nested `AstBox` wrapper; only Option<AstBox<T>> and Vec<AstBox<T>> are supported",
                ));
            }
            return Ok(Shape::Leaf);
        };
        return Ok(if name == "Option" {
            Shape::Optional(child)
        } else {
            Shape::List(child)
        });
    }
    if contains_ast_box(ty) {
        return Err(syn::Error::new_spanned(
            ty,
            "unsupported wrapper around `AstBox`; only AstBox<T>, Option<AstBox<T>>, and Vec<AstBox<T>> are supported",
        ));
    }
    Ok(Shape::Leaf)
}

fn classify_enum(item: &ItemEnum) -> syn::Result<Vec<Variant>> {
    item.variants.iter().map(|variant| {
        let fields = variant.fields.iter().enumerate().map(|(index, field)| {
            let name = field.ident.clone().unwrap_or_else(|| format_ident!("field_{index}"));
            let shape = classify(&field.ty)?;
            for attr in &field.attrs {
                if attr.path().is_ident("tree") {
                    return Err(syn::Error::new_spanned(attr, "`#[tree(...)]` is no longer part of the public abstract-tree grammar; use AstBox<T> field types"));
                }
            }
            Ok(Field { name, ty: field.ty.clone(), shape })
        }).collect::<syn::Result<Vec<_>>>()?;

        Ok(Variant { ident: variant.ident.clone(), fields, named: matches!(variant.fields, Fields::Named(_)) })
    }).collect()
}
fn family_path(path: &Path, generics: &Generics) -> Path {
    if generics.params.is_empty() {
        return path.clone();
    }
    let mut path = path.clone();
    let Some(segment) = path.segments.last_mut() else {
        return path;
    };
    if !matches!(segment.arguments, PathArguments::None) {
        return path;
    }
    let args = generics
        .params
        .iter()
        .filter_map(|param| match param {
            syn::GenericParam::Type(TypeParam { ident, .. }) => {
                Some(GenericArgument::Type(syn::parse_quote!(#ident)))
            }
            syn::GenericParam::Lifetime(param) => {
                Some(GenericArgument::Lifetime(param.lifetime.clone()))
            }
            syn::GenericParam::Const(param) => {
                Some(GenericArgument::Const(syn::parse_quote!(#param.ident)))
            }
        })
        .collect();
    segment.arguments = PathArguments::AngleBracketed(syn::AngleBracketedGenericArguments {
        colon2_token: None,
        lt_token: syn::token::Lt::default(),
        args,
        gt_token: syn::token::Gt::default(),
    });
    path
}
fn turbofish(generics: &Generics) -> TokenStream {
    if generics.params.is_empty() {
        return quote! {};
    }
    let args = generics.params.iter().map(|param| match param {
        syn::GenericParam::Type(param) => {
            let ident = &param.ident;
            quote! { #ident }
        }
        syn::GenericParam::Lifetime(param) => {
            let lifetime = &param.lifetime;
            quote! { #lifetime }
        }
        syn::GenericParam::Const(param) => {
            let ident = &param.ident;
            quote! { #ident }
        }
    });
    quote! { ::<#(#args),*> }
}

fn constrained_generics(generics: &Generics, variants: &[Variant]) -> Generics {
    let mut generics = generics.clone();
    let where_clause = generics
        .where_clause
        .get_or_insert_with(|| syn::WhereClause {
            where_token: syn::token::Where::default(),
            predicates: syn::punctuated::Punctuated::new(),
        });
    for param in &generics.params {
        let syn::GenericParam::Type(param) = param else {
            continue;
        };
        let ident = &param.ident;
        let predicate: WherePredicate =
            syn::parse_quote!(#ident: ::core::marker::Send + ::core::marker::Sync + 'static);
        let predicate_text = quote!(#predicate).to_string();
        if !where_clause
            .predicates
            .iter()
            .any(|existing| quote!(#existing).to_string() == predicate_text)
        {
            where_clause.predicates.push(predicate);
        }
    }
    for field in variants.iter().flat_map(|variant| &variant.fields) {
        if !matches!(field.shape, Shape::Leaf) {
            continue;
        }
        let ty = &field.ty;
        let predicate: WherePredicate = syn::parse_quote!(
            #ty: ::core::clone::Clone
                + ::core::cmp::PartialEq
                + ::core::fmt::Debug
                + ::core::marker::Send
                + ::core::marker::Sync
                + 'static
        );
        let predicate_text = quote!(#predicate).to_string();
        if !where_clause
            .predicates
            .iter()
            .any(|existing| quote!(#existing).to_string() == predicate_text)
        {
            where_clause.predicates.push(predicate);
        }
    }
    generics
}
fn qualified_member_lit(item: &ItemEnum) -> TokenStream {
    let member = item.ident.to_string();
    quote! { ::core::concat!(::core::module_path!(), "::", #member) }
}

fn static_name(item: &ItemEnum, variant: &syn::Ident, field: Option<&syn::Ident>) -> TokenStream {
    let member = item.ident.to_string();
    let variant = variant.to_string();
    let field = field.map(ToString::to_string).unwrap_or_default();
    let member_lit = syn::LitStr::new(&member, Span::call_site());
    let variant_lit = syn::LitStr::new(&variant, Span::call_site());
    let field_lit = syn::LitStr::new(&field, Span::call_site());
    quote! { (stringify!(#item), #member_lit, #variant_lit, #field_lit) }
}

fn member_lit(item: &ItemEnum) -> syn::LitStr {
    syn::LitStr::new(&item.ident.to_string(), item.ident.span())
}
fn field_lit(field: &Field) -> syn::LitStr {
    syn::LitStr::new(&field.name.to_string(), field.name.span())
}

fn pattern(variant: &Variant, bindings: &[syn::Ident]) -> TokenStream {
    let name = &variant.ident;
    if variant.named {
        let fields = variant.fields.iter().zip(bindings).map(|(field, binding)| {
            let field_name = &field.name;
            quote! { #field_name: #binding }
        });
        quote! { Self::#name { #(#fields),* } }
    } else if bindings.is_empty() {
        quote! { Self::#name }
    } else {
        quote! { Self::#name(#(#bindings),*) }
    }
}
fn construct(variant: &Variant, values: &[TokenStream]) -> TokenStream {
    let name = &variant.ident;
    if variant.named {
        let fields = variant.fields.iter().zip(values).map(|(field, value)| {
            let field_name = &field.name;
            quote! { #field_name: #value }
        });
        quote! { Self::#name { #(#fields),* } }
    } else if values.is_empty() {
        quote! { Self::#name }
    } else {
        quote! { Self::#name(#(#values),*) }
    }
}

fn leaf_read(
    shape: &Shape,
    node: &TokenStream,
    tree: &Path,
    item: &ItemEnum,
    variant: &Variant,
    field: &Field,
    snapshot: Option<&syn::Ident>,
) -> TokenStream {
    let member = qualified_member_lit(item);
    let variant_lit = syn::LitStr::new(&variant.ident.to_string(), variant.ident.span());
    let field_lit = field_lit(field);
    match (shape, snapshot) {
        (Shape::Leaf, None) => {
            let ty = &field.ty;
            quote! { ::plingo::reactive::abstract_tree::__read_leaf::<#tree, #ty>(#node.erased(), #member, #variant_lit, #field_lit)? }
        }
        (Shape::Leaf, Some(snapshot)) => {
            let ty = &field.ty;
            quote! { ::plingo::reactive::abstract_tree::__snapshot_leaf::<#tree, #ty>(#snapshot, #node.erased(), #member, #variant_lit, #field_lit)? }
        }
        (Shape::Child(child), None) => {
            quote! { ::plingo::reactive::abstract_tree::__read_child::<#tree, #child>(#node.erased(), #member, #variant_lit, #field_lit)?.ok_or_else(|| ::plingo::reactive::Error::Internal("required abstract-tree child is absent".into()))? }
        }
        (Shape::Child(child), Some(snapshot)) => {
            quote! { ::plingo::reactive::abstract_tree::__snapshot_child::<#tree, #child>(#snapshot, #node.erased(), #member, #variant_lit, #field_lit)?.ok_or_else(|| ::plingo::reactive::Error::Internal("required abstract-tree child is absent".into()))? }
        }
        (Shape::Optional(child), None) => {
            quote! { ::plingo::reactive::abstract_tree::__read_child::<#tree, #child>(#node.erased(), #member, #variant_lit, #field_lit)? }
        }
        (Shape::Optional(child), Some(snapshot)) => {
            quote! { ::plingo::reactive::abstract_tree::__snapshot_child::<#tree, #child>(#snapshot, #node.erased(), #member, #variant_lit, #field_lit)? }
        }
        (Shape::List(child), None) => {
            quote! { ::plingo::reactive::abstract_tree::__read_children::<#tree, #child>(#node.erased(), #member, #variant_lit, #field_lit)? }
        }
        (Shape::List(child), Some(snapshot)) => {
            quote! { ::plingo::reactive::abstract_tree::__snapshot_children::<#tree, #child>(#snapshot, #node.erased(), #member, #variant_lit, #field_lit)? }
        }
    }
}

fn gen_accessor(
    item: &ItemEnum,
    variant: &Variant,
    tree: &Path,
    generics: &Generics,
    view_generics: &Generics,
) -> TokenStream {
    let item_ident = &item.ident;
    let accessor = format_ident!("{}{}Access", item.ident, variant.ident);
    let (impl_generics, ty_generics, where_clause) = view_generics.split_for_impl();
    let (debug_impl_generics, debug_ty_generics, debug_where_clause) = generics.split_for_impl();
    let (_, item_ty_generics, _) = generics.split_for_impl();
    let snapshot_reader = format_ident!("__snapshot");
    let fields = variant.fields.iter().map(|field| {
        let name = &field.name;
        let ty = match &field.shape {
            Shape::Leaf => {
                let leaf = &field.ty;
                quote! { ::std::sync::Arc<#leaf> }
            }
            Shape::Child(child) => quote! { ::plingo::reactive::abstract_tree::AstBox<#child> },
            Shape::Optional(child) => {
                quote! { ::std::option::Option<::plingo::reactive::abstract_tree::AstBox<#child>> }
            }
            Shape::List(child) => quote! { ::plingo::reactive::abstract_tree::ChildList<#child> },
        };
        let live = leaf_read(
            &field.shape,
            &quote!(self.node),
            tree,
            item,
            variant,
            field,
            None,
        );
        let snapshot = leaf_read(
            &field.shape,
            &quote!(self.node),
            tree,
            item,
            variant,
            field,
            Some(&snapshot_reader),
        );
        let value = quote! {
            Ok(match &self.snapshot {
                ::std::option::Option::Some(#snapshot_reader) => #snapshot,
                ::std::option::Option::None => #live,
            })
        };
        quote! {
            pub fn #name(&self) -> ::plingo::reactive::Result<#ty> { #value }
        }
    });
    quote! {
        #[doc(hidden)]
        pub struct #accessor #ty_generics {
            node: ::plingo::reactive::abstract_tree::AstBox<#item_ident #item_ty_generics>,
            snapshot: ::std::option::Option<::plingo::reactive::Snapshot>,
        }
        impl #debug_impl_generics ::core::fmt::Debug for #accessor #debug_ty_generics #debug_where_clause {
            fn fmt(&self, formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                formatter.debug_struct(::core::stringify!(#accessor))
                    .field("node", &self.node)
                    .finish()
            }
        }
        impl #impl_generics #accessor #ty_generics #where_clause {
            #(#fields)*
        }
    }
}
fn gen_view_enum(
    item: &ItemEnum,
    variants: &[Variant],
    tree: &Path,
    generics: &Generics,
    constrained: &Generics,
) -> (syn::Ident, TokenStream) {
    let view_ident = format_ident!("{}View", item.ident);
    let (_, ty_generics, _) = generics.split_for_impl();
    let fields = variants.iter().map(|variant| {
        let name = &variant.ident;
        let accessor = format_ident!("{}{}Access", item.ident, variant.ident);
        quote! { #name(#accessor #ty_generics) }
    });
    let accessors = variants
        .iter()
        .map(|variant| gen_accessor(item, variant, tree, generics, constrained));
    (
        view_ident.clone(),
        quote! {
            #(#accessors)*
            #[derive(Debug)]
            pub enum #view_ident #ty_generics { #(#fields),* }
        },
    )
}

fn gen_render_arm(item: &ItemEnum, variant: &Variant, tree: &Path) -> TokenStream {
    let item_ident = &item.ident;
    let (_, item_ty_generics, _) = item.generics.split_for_impl();
    let bindings: Vec<_> = (0..variant.fields.len())
        .map(|i| format_ident!("__field_{i}"))
        .collect();
    let pat = pattern(variant, &bindings);
    let member = qualified_member_lit(item);
    let variant_lit = syn::LitStr::new(&variant.ident.to_string(), variant.ident.span());
    let mut statements = Vec::new();
    statements.push(quote! {
        __facts.push((
            ::plingo::reactive::abstract_tree::TreeKey::Member(__node.erased(), #member),
            ::plingo::reactive::abstract_tree::TreeFact::Member(#member)
        ));
    });
    statements.push(quote! {
        __facts.push((
            ::plingo::reactive::abstract_tree::TreeKey::Kind(
                __node.erased(), #member
            ),
            ::plingo::reactive::abstract_tree::TreeFact::Kind(#variant_lit)
        ));
    });
    for (field, binding) in variant.fields.iter().zip(&bindings) {
        let field_lit = field_lit(field);
        match &field.shape {
            Shape::Leaf => statements.push(quote! {
                __facts.push((
                    ::plingo::reactive::abstract_tree::TreeKey::Leaf(
                        __node.erased(), #member, #variant_lit, #field_lit
                    ),
                    ::plingo::reactive::abstract_tree::TreeFact::Leaf(
                        ::std::sync::Arc::new(#binding)
                    )
                ));
            }),
            Shape::Child(_) => statements.push(quote! {
                let __child = #binding;
                __facts.push((
                    ::plingo::reactive::abstract_tree::TreeKey::Child(
                        __node.erased(), #member, #variant_lit, #field_lit
                    ),
                    ::plingo::reactive::abstract_tree::TreeFact::Child(
                        ::std::option::Option::Some(__child.erased())
                    )
                ));
                __facts.push((
                    ::plingo::reactive::abstract_tree::TreeKey::Parent(__child.erased()),
                    ::plingo::reactive::abstract_tree::TreeFact::Parent(
                        ::std::option::Option::Some(__node.erased())
                    )
                ));
            }),
            Shape::Optional(_) => statements.push(quote! {
                let __child = #binding;
                let __child = __child.map(|__child| __child.erased());
                __facts.push((
                    ::plingo::reactive::abstract_tree::TreeKey::Child(
                        __node.erased(), #member, #variant_lit, #field_lit
                    ),
                    ::plingo::reactive::abstract_tree::TreeFact::Child(__child.clone())
                ));
                if let Some(__child) = __child {
                    __facts.push((
                        ::plingo::reactive::abstract_tree::TreeKey::Parent(__child),
                        ::plingo::reactive::abstract_tree::TreeFact::Parent(
                            ::std::option::Option::Some(__node.erased())
                        )
                    ));
                }
            }),
            Shape::List(_) => statements.push(quote! {
                let __children = #binding;
                let mut __order = ::std::vec::Vec::with_capacity(__children.len());
                for __child in __children {
                    let __child = __child.erased();
                    __facts.push((
                        ::plingo::reactive::abstract_tree::TreeKey::ChildLink(
                            __node.erased(), #member, #variant_lit, #field_lit, __child.clone()
                        ),
                        ::plingo::reactive::abstract_tree::TreeFact::Link(__child.clone())
                    ));
                    __facts.push((
                        ::plingo::reactive::abstract_tree::TreeKey::Parent(__child.clone()),
                        ::plingo::reactive::abstract_tree::TreeFact::Parent(
                            ::std::option::Option::Some(__node.erased())
                        )
                    ));
                    __order.push(__child);
                }
                __facts.push((
                    ::plingo::reactive::abstract_tree::TreeKey::ChildOrder(
                        __node.erased(), #member, #variant_lit, #field_lit
                    ),
                    ::plingo::reactive::abstract_tree::TreeFact::Order(__order.into())
                ));
            }),
        }
    }
    quote! {
        #pat => {
            let __node = ::plingo::reactive::abstract_tree::__automatic_box::<#tree>()?;
            let mut __facts: ::std::vec::Vec<(
                ::plingo::reactive::abstract_tree::TreeKey<
                    <#tree as ::plingo::reactive::abstract_tree::AbstractTreeFamily>::Domain
                >,
                ::plingo::reactive::abstract_tree::TreeFact,
            )> = ::std::vec::Vec::new();
            #(#statements)*
            let __rendered = ::plingo::reactive::abstract_tree::__render_at::<#tree>(
                __node.erased(), __facts
            )?;
            Ok(::plingo::reactive::abstract_tree::AstBox::<#item_ident #item_ty_generics>::from_erased(__rendered))
        }
    }
}
fn gen_materialize_arm(
    item: &ItemEnum,
    variant: &Variant,
    tree: &Path,
    snapshot: Option<&syn::Ident>,
) -> TokenStream {
    let bindings: Vec<_> = (0..variant.fields.len())
        .map(|i| format_ident!("__value_{i}"))
        .collect();
    let values = variant
        .fields
        .iter()
        .zip(&bindings)
        .map(|(field, binding)| {
            let read = leaf_read(
                &field.shape,
                &quote!(node),
                tree,
                item,
                variant,
                field,
                snapshot,
            );
            match &field.shape {
                Shape::Leaf => quote! { (*#read).clone() },
                Shape::Child(_) | Shape::Optional(_) => quote! { #read },
                Shape::List(_) => quote! { #read.to_vec() },
            }
        });
    let constructor = construct(variant, &values.collect::<Vec<_>>());
    let kind = syn::LitStr::new(&variant.ident.to_string(), variant.ident.span());
    quote! { #kind => Ok(#constructor) }
}

fn gen_family(_tree: &Path, _domain: &Type, _root: &ItemEnum) -> TokenStream {
    quote! {}
}
/// Generates the sealed parser publication adapter for one syntax member:
/// `SyntaxPublication::__syntax_facts` decomposes one arena enum value into
/// exact member/kind/leaf/child facts over the general tree codec.
fn gen_syntax_publication(
    item: &ItemEnum,
    ident: &syn::Ident,
    variants: &[Variant],
    tree: &Path,
) -> TokenStream {
    let member = qualified_member_lit(item);
    let child_record_arms = variants.iter().map(|variant| {
        let bindings: Vec<_> = (0..variant.fields.len())
            .map(|i| format_ident!("__value_{i}"))
            .collect();
        let pat = pattern(variant, &bindings);
        let mut collects = Vec::new();
        for (field, binding) in variant.fields.iter().zip(&bindings) {
            match &field.shape {
                Shape::Child(_) => collects.push(quote! {
                    __records.push(
                        ::plingo::reactive::abstract_tree::__syntax_child_id(#binding)
                    );
                }),
                Shape::Optional(_) => collects.push(quote! {
                    if let ::std::option::Option::Some(__child_value) = #binding.as_ref() {
                        __records.push(
                            ::plingo::reactive::abstract_tree::__syntax_child_id(__child_value)
                        );
                    }
                }),
                Shape::List(_) => collects.push(quote! {
                    for __child_value in #binding.iter() {
                        __records.push(
                            ::plingo::reactive::abstract_tree::__syntax_child_id(__child_value)
                        );
                    }
                }),
                Shape::Leaf => {}
            }
        }
        quote! { #pat => { #(#collects)* } }
    });
    let arms = variants.iter().map(|variant| {
        let bindings: Vec<_> = (0..variant.fields.len())
            .map(|i| format_ident!("__value_{i}"))
            .collect();
        let pat = pattern(variant, &bindings);
        let variant_lit = syn::LitStr::new(&variant.ident.to_string(), variant.ident.span());
        let mut statements = Vec::new();
        statements.push(quote! {
            __out.push((
                ::plingo::reactive::abstract_tree::TreeKey::Member(__node.clone(), #member),
                ::plingo::reactive::abstract_tree::TreeFact::Member(#member),
            ));
            __out.push((
                ::plingo::reactive::abstract_tree::TreeKey::Kind(
                    __node.clone(), #member,
                ),
                ::plingo::reactive::abstract_tree::TreeFact::Kind(#variant_lit),
            ));
        });
        for (field, binding) in variant.fields.iter().zip(&bindings) {
            let field_lit = field_lit(field);
            match &field.shape {
                Shape::Leaf => statements.push(quote! {
                    __out.push((
                        ::plingo::reactive::abstract_tree::TreeKey::Leaf(
                            __node.clone(), #member, #variant_lit, #field_lit,
                        ),
                        ::plingo::reactive::abstract_tree::TreeFact::Leaf(
                            ::std::sync::Arc::new(#binding.clone()),
                        ),
                    ));
                }),
                Shape::Child(_) => statements.push(quote! {
                    let __child_id =
                        ::plingo::reactive::abstract_tree::__syntax_child_id(#binding);
                    let __child = __project(__child_id).ok_or_else(|| {
                        ::plingo::reactive::Error::Internal(
                            "abstract-tree syntax child lineage is unpublished".into(),
                        )
                    })?;
                    __out.push((
                        ::plingo::reactive::abstract_tree::TreeKey::Child(
                            __node.clone(), #member, #variant_lit, #field_lit,
                        ),
                        ::plingo::reactive::abstract_tree::TreeFact::Child(
                            ::std::option::Option::Some(__child.clone()),
                        ),
                    ));
                    __out.push((
                        ::plingo::reactive::abstract_tree::TreeKey::Parent(__child),
                        ::plingo::reactive::abstract_tree::TreeFact::Parent(
                            ::std::option::Option::Some(__node.clone()),
                        ),
                    ));
                }),
                Shape::Optional(_) => statements.push(quote! {
                    let __child = #binding.as_ref().and_then(|__child| {
                        __project(
                            ::plingo::reactive::abstract_tree::__syntax_child_id(__child)
                        )
                    });
                    __out.push((
                        ::plingo::reactive::abstract_tree::TreeKey::Child(
                            __node.clone(), #member, #variant_lit, #field_lit,
                        ),
                        ::plingo::reactive::abstract_tree::TreeFact::Child(__child.clone()),
                    ));
                    if let ::std::option::Option::Some(__child) = __child {
                        __out.push((
                            ::plingo::reactive::abstract_tree::TreeKey::Parent(__child),
                            ::plingo::reactive::abstract_tree::TreeFact::Parent(
                                ::std::option::Option::Some(__node.clone()),
                            ),
                        ));
                    }
                }),
                Shape::List(_) => statements.push(quote! {
                    let mut __order = ::std::vec::Vec::with_capacity(#binding.len());
                    for __child_value in #binding.iter() {
                        let __child = __project(
                            ::plingo::reactive::abstract_tree::__syntax_child_id(__child_value)
                        ).ok_or_else(|| {
                            ::plingo::reactive::Error::Internal(
                                "abstract-tree syntax child lineage is unpublished".into(),
                            )
                        })?;
                        __out.push((
                            ::plingo::reactive::abstract_tree::TreeKey::ChildLink(
                                __node.clone(), #member, #variant_lit, #field_lit,
                                __child.clone(),
                            ),
                            ::plingo::reactive::abstract_tree::TreeFact::Link(
                                __child.clone(),
                            ),
                        ));
                        __out.push((
                            ::plingo::reactive::abstract_tree::TreeKey::Parent(
                                __child.clone(),
                            ),
                            ::plingo::reactive::abstract_tree::TreeFact::Parent(
                                ::std::option::Option::Some(__node.clone()),
                            ),
                        ));
                        __order.push(__child);
                    }
                    __out.push((
                        ::plingo::reactive::abstract_tree::TreeKey::ChildOrder(
                            __node.clone(), #member, #variant_lit, #field_lit,
                        ),
                        ::plingo::reactive::abstract_tree::TreeFact::Order(__order.into()),
                    ));
                }),
            }
        }
        quote! { #pat => { #(#statements)* } }
    });
    quote! {
        #[doc(hidden)]
        impl ::plingo::reactive::abstract_tree::SyntaxPublication for #ident {
            fn __syntax_member() -> &'static str { #member }

            fn __syntax_child_records(__value: &Self) -> ::std::vec::Vec<u64> {
                let mut __records = ::std::vec::Vec::new();
                match __value {
                    #(#child_record_arms),*
                }
                __records
            }

            fn __syntax_facts(
                __node: ::plingo::reactive::abstract_tree::AstBox<()>,
                __value: &Self,
                __root: bool,
                __project: &dyn Fn(u64) -> ::std::option::Option<
                    ::plingo::reactive::abstract_tree::AstBox<()>
                >,
                __out: &mut ::std::vec::Vec<(
                        ::plingo::reactive::abstract_tree::TreeKey<
                            <<Self as ::plingo::reactive::abstract_tree::AbstractTreeNode>
                                ::Family as
                                ::plingo::reactive::abstract_tree::AbstractTreeFamily>::Domain
                        >,
                    ::plingo::reactive::abstract_tree::TreeFact,
                )>,
            ) -> ::plingo::reactive::Result<()> {
                let _ = __root;
                match __value {
                    #(#arms),*
                }
                ::std::result::Result::Ok(())
            }
        }
    }
}

pub(crate) fn expand(args: &Args, mut item: ItemEnum) -> syn::Result<TokenStream> {
    let ident = item.ident.clone();
    let (tree, domain, root_ident, is_root) = if let Some(member_of) = &args.member_of {
        if args.tree.is_some() || args.domain.is_some() || args.members.is_some() {
            return Err(syn::Error::new_spanned(
                member_of,
                "`member_of` is mutually exclusive with `tree`, `domain`, and `members`",
            ));
        }
        (
            member_of.clone(),
            syn::parse_quote!(()),
            ident.clone(),
            false,
        )
    } else {
        let members = args
            .members
            .clone()
            .unwrap_or_else(|| vec![syn::parse_quote!(#ident)]);
        if members.is_empty() {
            return Err(syn::Error::new_spanned(
                &item,
                "`members(...)` must list the family root",
            ));
        }
        let Some(first) = last_ident(&members[0]) else {
            return Err(syn::Error::new_spanned(
                &item,
                "invalid abstract-tree family root",
            ));
        };
        if first != &ident {
            return Err(syn::Error::new_spanned(
                &item,
                "the family root must be the first item in `members(...)`",
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for member in &members {
            let Some(name) = last_ident(member) else {
                return Err(syn::Error::new_spanned(
                    member,
                    "invalid abstract-tree family member",
                ));
            };
            if !seen.insert(name.to_string()) {
                return Err(syn::Error::new_spanned(
                    member,
                    "duplicate abstract-tree family member",
                ));
            }
        }
        let tree_ident = args
            .tree
            .clone()
            .and_then(|path| last_ident(&path).cloned())
            .unwrap_or_else(|| format_ident!("{}Tree", ident));
        (
            args.tree
                .clone()
                .unwrap_or_else(|| syn::parse_quote!(#tree_ident)),
            args.domain.clone().unwrap_or_else(|| syn::parse_quote!(())),
            ident.clone(),
            true,
        )
    };
    let variants = classify_enum(&item)?;
    let constrained = constrained_generics(&item.generics, &variants);
    let item_ty_generics = item.generics.split_for_impl().1;
    let tree = family_path(&tree, &item.generics);
    let item_turbofish = turbofish(&item.generics);
    item.attrs
        .retain(|attr| !attr.path().is_ident("abstract_tree"));
    let (view_ident, view_tokens) =
        gen_view_enum(&item, &variants, &tree, &item.generics, &constrained);
    let kind_ident = format_ident!("{}Kind", ident);
    let kind_variants = variants.iter().map(|variant| &variant.ident);
    let member = qualified_member_lit(&item);
    let view_arms: Vec<TokenStream> = variants.iter().map(|variant| {
        let kind = syn::LitStr::new(&variant.ident.to_string(), variant.ident.span());
        let accessor = format_ident!("{}{}Access", ident, variant.ident);
        let view_variant = &variant.ident;
        quote! { #kind => Ok(#view_ident::#view_variant(#accessor #item_turbofish { node, snapshot: ::std::option::Option::None })) }
    }).collect();
    let snapshot_view_arms: Vec<TokenStream> = variants.iter().map(|variant| {
        let kind = syn::LitStr::new(&variant.ident.to_string(), variant.ident.span());
        let accessor = format_ident!("{}{}Access", ident, variant.ident);
        let view_variant = &variant.ident;
        quote! { #kind => Ok(#view_ident::#view_variant(#accessor #item_turbofish { node, snapshot: ::std::option::Option::Some(snapshot.clone()) })) }
    }).collect();
    let snapshot_view_arms = &snapshot_view_arms;
    let kind_arms = variants.iter().map(|variant| {
        let kind = syn::LitStr::new(&variant.ident.to_string(), variant.ident.span());
        let variant_ident = &variant.ident;
        quote! { #kind => Ok(#kind_ident::#variant_ident) }
    });
    let render_arms = variants
        .iter()
        .map(|variant| gen_render_arm(&item, variant, &tree));
    let snapshot_ident = format_ident!("snapshot");
    let materialize_arms = variants
        .iter()
        .map(|variant| gen_materialize_arm(&item, variant, &tree, None));
    let snapshot_materialize_arms = variants
        .iter()
        .map(|variant| gen_materialize_arm(&item, variant, &tree, Some(&snapshot_ident)));
    let member_paths: Vec<Path> = args
        .members
        .clone()
        .unwrap_or_else(|| vec![syn::parse_quote!(#ident)]);
    let snapshot_ident = format_ident!("snapshot");
    let syntax_family_publication = if args.syntax && is_root {
        let (family_constrained_impl, _, family_constrained_where) = constrained.split_for_impl();
        let member_arms = member_paths.iter().filter_map(|path| {
            let ident = last_ident(path)?;
            Some(quote! {
                if let ::std::option::Option::Some(__value) =
                    __value.downcast_ref::<#ident>()
                {
                    return ::std::option::Option::Some(
                        <#ident as ::plingo::reactive::abstract_tree::SyntaxPublication>
                            ::__syntax_member()
                    );
                }
            })
        });
        let ordinal_arms = member_paths
            .iter()
            .enumerate()
            .filter_map(|(ordinal, path)| {
                let ident = last_ident(path)?;
                let ordinal = u8::try_from(ordinal).ok()?;
                Some(quote! {
                    if __value.downcast_ref::<#ident>().is_some() {
                        return ::std::option::Option::Some(#ordinal);
                    }
                })
            });
        let child_arms = member_paths.iter().filter_map(|path| {
            let ident = last_ident(path)?;
            Some(quote! {
                if let ::std::option::Option::Some(__value) =
                    __value.downcast_ref::<#ident>()
                {
                    return <#ident as
                        ::plingo::reactive::abstract_tree::SyntaxPublication>
                        ::__syntax_child_records(__value);
                }
            })
        });
        let publish_arms = member_paths.iter().filter_map(|path| {
            let ident = last_ident(path)?;
            Some(quote! {
                if let ::std::option::Option::Some(__value) =
                    __value.downcast_ref::<#ident>()
                {
                    <#ident as ::plingo::reactive::abstract_tree::SyntaxPublication>
                        ::__syntax_facts(
                            __node,
                            __value,
                            __root,
                            __project,
                            __out,
                        )?;
                    return ::std::result::Result::Ok(true);
                }
            })
        });
        quote! {
            #[doc(hidden)]
            impl #family_constrained_impl
                ::plingo::reactive::abstract_tree::SyntaxFamilyPublication
                for #root_ident #item_ty_generics #family_constrained_where
            {
                fn __domain_from_uri(
                    __uri: &str,
                ) -> <<Self as
                    ::plingo::reactive::abstract_tree::AbstractTreeNode>::Family as
                    ::plingo::reactive::abstract_tree::AbstractTreeFamily>::Domain {
                    __uri.to_string()
                }

                fn __syntax_member_of(
                    __value: &(dyn ::std::any::Any + Send + Sync),
                ) -> ::std::option::Option<&'static str> {
                    #(#member_arms)*
                    ::std::option::Option::None
                }

                fn __syntax_member_kind(
                    __value: &(dyn ::std::any::Any + Send + Sync),
                ) -> ::std::option::Option<u8> {
                    #(#ordinal_arms)*
                    ::std::option::Option::None
                }

                fn __syntax_child_records(
                    __value: &(dyn ::std::any::Any + Send + Sync),
                ) -> ::std::vec::Vec<u64> {
                    #(#child_arms)*
                    ::std::vec::Vec::new()
                }

                fn __syntax_publish(
                    __node: ::plingo::reactive::abstract_tree::AstBox<()>,
                    __value: &(dyn ::std::any::Any + Send + Sync),
                    __root: bool,
                    __project: &dyn Fn(
                        u64,
                    ) -> ::std::option::Option<
                        ::plingo::reactive::abstract_tree::AstBox<()>
                    >,
                    __out: &mut ::std::vec::Vec<(
                        ::plingo::reactive::abstract_tree::TreeKey<
                            <<Self as
                                ::plingo::reactive::abstract_tree::AbstractTreeNode>
                                ::Family as
                                ::plingo::reactive::abstract_tree::AbstractTreeFamily>
                                ::Domain,
                        >,
                        ::plingo::reactive::abstract_tree::TreeFact,
                    )>,
                ) -> ::plingo::reactive::Result<bool> {
                    #(#publish_arms)*
                    ::std::result::Result::Ok(false)
                }
            }
        }
    } else {
        quote! {}
    };
    let family = if is_root {
        let tree_ident = last_ident(&tree).expect("tree path has a final segment");
        let (family_impl_generics, family_ty_generics, family_where) =
            item.generics.split_for_impl();
        let (family_constrained_impl, _, family_constrained_where) = constrained.split_for_impl();
        quote! {
            #[doc(hidden)]
            #[derive(Clone, Copy, Debug)]
            pub struct #tree_ident #family_ty_generics(
                ::std::marker::PhantomData<fn() -> #root_ident #item_ty_generics>
            );
            impl #family_impl_generics ::core::default::Default
                for #tree #family_where
            {
                fn default() -> Self {
                    Self(::std::marker::PhantomData)
                }
            }
            #[doc(hidden)]
            impl #family_constrained_impl ::plingo::reactive::view::View
                for #tree #family_constrained_where
            {
                type Input = ::plingo::reactive::abstract_tree::TreeKey<#domain>;
                type Output = ::plingo::reactive::abstract_tree::TreeFact;
                fn name() -> &'static str { stringify!(#tree_ident) }
                fn __register(effect: &::plingo::reactive::__macro_private::EffectContext) -> ::plingo::reactive::Result<()> { effect.register::<Self>() }
                fn __observe(effect: &::plingo::reactive::__macro_private::EffectContext, input: Self::Input, temporal: ::plingo::reactive::__macro_private::Temporal) -> ::plingo::reactive::Result<::std::option::Option<::std::sync::Arc<Self::Output>>> { effect.observe::<Self>(input, temporal) }
                fn __inputs(effect: &::plingo::reactive::__macro_private::EffectContext, temporal: ::plingo::reactive::__macro_private::Temporal) -> ::plingo::reactive::Result<::std::vec::Vec<Self::Input>> { effect.inputs::<Self>(temporal) }
                fn __emit(effect: &::plingo::reactive::__macro_private::EffectContext, input: Self::Input, output: ::std::option::Option<Self::Output>) -> ::plingo::reactive::Result<()> { effect.emit::<Self>(input, output) }
                fn __snapshot(snapshot: &::plingo::reactive::Snapshot, input: Self::Input) -> ::std::option::Option<::std::sync::Arc<Self::Output>> { snapshot.__plain_observe::<Self>(input) }
                fn __snapshot_inputs(snapshot: &::plingo::reactive::Snapshot) -> ::std::vec::Vec<Self::Input> { snapshot.__plain_inputs::<Self>() }
            }
            impl #family_constrained_impl ::plingo::reactive::abstract_tree::AbstractTreeFamily
                for #tree #family_constrained_where
            {
                type Domain = #domain;
                type Root = #root_ident #item_ty_generics;
            }
        }
    } else {
        quote! {}
    };
    let (impl_generics, _, where_clause) = constrained.split_for_impl();
    let family_selector = if args.syntax {
        if is_root {
            quote! {
                impl #impl_generics #ident #item_ty_generics #where_clause {
                    pub fn roots() -> ::plingo::reactive::abstract_tree::RootSelector<
                        #tree, #ident #item_ty_generics
                    > {
                        ::plingo::reactive::abstract_tree::RootSelector::new()
                    }
                    pub fn nodes() -> ::plingo::reactive::abstract_tree::NodeSelector<
                        #tree, #ident #item_ty_generics
                    > {
                        ::plingo::reactive::abstract_tree::NodeSelector::new()
                    }
                }
            }
        } else {
            quote! {
                impl #impl_generics #ident #item_ty_generics #where_clause {
                    pub fn nodes() -> ::plingo::reactive::abstract_tree::NodeSelector<
                        #tree, #ident #item_ty_generics
                    > {
                        ::plingo::reactive::abstract_tree::NodeSelector::new()
                    }
                }
            }
        }
    } else if is_root {
        quote! {
            impl #impl_generics #ident #item_ty_generics #where_clause {
                pub fn roots() -> ::plingo::reactive::abstract_tree::RootSelector<
                    #tree, #ident #item_ty_generics
                > {
                    ::plingo::reactive::abstract_tree::RootSelector::new()
                }
                pub fn nodes() -> ::plingo::reactive::abstract_tree::NodeSelector<
                    #tree, #ident #item_ty_generics
                > {
                    ::plingo::reactive::abstract_tree::NodeSelector::new()
                }
                pub fn render(value: Self) -> ::plingo::reactive::Result<
                    ::plingo::reactive::abstract_tree::AstBox<Self>
                > {
                    <Self as ::plingo::reactive::abstract_tree::TreeRender>::__render(value)
                }
            }
        }
    } else {
        quote! {
            impl #impl_generics #ident #item_ty_generics #where_clause {
                pub fn nodes() -> ::plingo::reactive::abstract_tree::NodeSelector<
                    #tree, #ident #item_ty_generics
                > {
                    ::plingo::reactive::abstract_tree::NodeSelector::new()
                }
                pub fn render(value: Self) -> ::plingo::reactive::Result<
                    ::plingo::reactive::abstract_tree::AstBox<Self>
                > {
                    <Self as ::plingo::reactive::abstract_tree::TreeRender>::__render(value)
                }
            }
        }
    };
    let node_impl = quote! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum #kind_ident { #(#kind_variants),* }
        impl #impl_generics ::plingo::reactive::abstract_tree::AbstractTreeNode
            for #ident #item_ty_generics #where_clause
        {
            type Family = #tree;
            type View = #view_ident #item_ty_generics;
            type Kind = #kind_ident;
            fn __member() -> &'static str { #member }
            fn __view(node: ::plingo::reactive::abstract_tree::AstBox<Self>) -> ::plingo::reactive::Result<Self::View> {
                ::plingo::reactive::abstract_tree::__read_member::<#tree>(node.erased(), #member)?;
                let kind = ::plingo::reactive::abstract_tree::__read_kind::<#tree>(node.erased(), #member)?;
                match kind {
                    #(#view_arms),*,
                    _ => Err(::plingo::reactive::Error::Internal("unknown abstract-tree variant".into())),
                }
            }
            fn __kind(node: ::plingo::reactive::abstract_tree::AstBox<Self>) -> ::plingo::reactive::Result<Self::Kind> {
                let kind = ::plingo::reactive::abstract_tree::__read_kind::<#tree>(node.erased(), #member)?;
                match kind {
                    #(#kind_arms),*,
                    _ => Err(::plingo::reactive::Error::Internal("unknown abstract-tree variant".into())),
                }
            }
            fn __snapshot_view(
                snapshot: &::plingo::reactive::Snapshot,
                node: ::plingo::reactive::abstract_tree::AstBox<Self>,
            ) -> ::plingo::reactive::Result<Self::View> {
                let kind = match snapshot.__plain_observe::<#tree>(::plingo::reactive::abstract_tree::TreeKey::Kind(node.erased(), #member)).as_deref() {
                    Some(::plingo::reactive::abstract_tree::TreeFact::Kind(kind)) => *kind,
                    _ => return Err(::plingo::reactive::Error::Internal("abstract-tree snapshot kind missing".into())),
                };
                match kind {
                    #(#snapshot_view_arms),*,
                    _ => Err(::plingo::reactive::Error::Internal("unknown abstract-tree variant".into())),
                }
            }
        }
    };
    let render_impl = if args.syntax {
        quote! {}
    } else {
        quote! {
            impl #impl_generics ::plingo::reactive::abstract_tree::TreeRender
                for #ident #item_ty_generics #where_clause
            {
                fn __materialize(node: ::plingo::reactive::abstract_tree::AstBox<Self>) -> ::plingo::reactive::Result<Self> {
                    let kind = ::plingo::reactive::abstract_tree::__read_kind::<#tree>(node.erased(), #member)?;
                    match kind {
                        #(#materialize_arms),*,
                        _ => Err(::plingo::reactive::Error::Internal("unknown abstract-tree variant".into())),
                    }
                }
                fn __snapshot_materialize(snapshot: &::plingo::reactive::Snapshot, node: ::plingo::reactive::abstract_tree::AstBox<Self>) -> ::plingo::reactive::Result<Self> {
                    let kind = match snapshot.__plain_observe::<#tree>(::plingo::reactive::abstract_tree::TreeKey::Kind(node.erased(), #member)).as_deref() {
                        Some(::plingo::reactive::abstract_tree::TreeFact::Kind(kind)) => *kind,
                        _ => return Err(::plingo::reactive::Error::Internal("abstract-tree snapshot kind missing".into())),
                    };
                    match kind {
                        #(#snapshot_materialize_arms),*,
                        _ => Err(::plingo::reactive::Error::Internal("unknown abstract-tree variant".into())),
                    }
                }
                fn __render(value: Self) -> ::plingo::reactive::Result<::plingo::reactive::abstract_tree::AstBox<Self>> {
                    match value {
                        #(#render_arms),*
                    }
                }
            }
        }
    };
    let publication_impl = if args.syntax {
        gen_syntax_publication(&item, &ident, &variants, &tree)
    } else {
        quote! {}
    };
    let tokens = quote! { #item #family #view_tokens #node_impl #render_impl #publication_impl #family_selector #syntax_family_publication };
    if std::env::var_os("PLINGO_DUMP_V2").is_some() {
        std::fs::write(
            format!("/tmp/v2_dump_{}.rs", item.ident),
            tokens.to_string(),
        )
        .ok();
    }
    Ok(tokens)
}
