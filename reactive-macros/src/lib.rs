//! Procedural macros for the transparent reactive authoring surface.
//!
//! `#[view]` declares one typed reactive view from a single kind-witness
//! tuple field (`Map<K, V>`, `List<K, I>`, `Tree<K, N>`, `Graph<P, L>`, or
//! `Box<V>`); the witness selects the fact codec and the emit/observe
//! handle pair (plan §5.2). Computations are ordinary Rust functions;
//! there is deliberately no component attribute or generated runtime
//! descriptor.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{GenericArgument, ItemStruct, PathArguments, ReturnType, Type, TypePath};

mod abstract_tree_v2;

/// Derives the sealed effect-operation implementation.
#[proc_macro_derive(Effects)]
pub fn derive_effects(item: TokenStream) -> TokenStream {
    let input = match syn::parse::<syn::DeriveInput>(item) {
        Ok(input) => input,
        Err(error) => return error.to_compile_error().into(),
    };
    let ident = input.ident;
    let generics = input.generics;
    let fields = match input.data {
        syn::Data::Struct(data) => data.fields,
        _ => {
            return syn::Error::new_spanned(ident, "`Effects` can only be derived for structs")
                .to_compile_error()
                .into();
        }
    };
    let accesses: Vec<proc_macro2::TokenStream> = fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let member = field
                .ident
                .as_ref()
                .map(|ident| quote! { #ident })
                .unwrap_or_else(|| {
                    let index = syn::Index::from(index);
                    quote! { #index }
                });
            quote! { ::plingo::reactive::component::Effects::__apply(&self.#member)?; }
        })
        .collect();
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let effect_impl = quote! {
        impl #impl_generics ::plingo::reactive::component::Effect for #ident #ty_generics #where_clause {
            fn __apply(&self) -> ::plingo::reactive::Result<()> {
                #(#accesses)*
                Ok(())
            }
        }
    };
    effect_impl.into()
}
/// The kind witness carried by the struct's single tuple field.
enum Witness {
    /// `Map<K, V>` — one fact per present entry.
    Map { key: Type, value: Type },
    /// `List<K, I>` — one fact per slot plus one length fact.
    List { key: Type, item: Type },
    /// `Tree<K, N>` — one fact per node plus one root list per key.
    Tree { key: Type, payload: Type },
    /// `Graph<P, L>` — one fact per node payload plus labelled buckets.
    Graph { payload: Type, label: Type },
    /// `Box<V>` — one cell.
    Cell { value: Type },
}

impl Witness {
    fn classify(ty: &Type) -> syn::Result<Self> {
        const HELP: &str = "a view declares one kind witness as its single tuple field: \
`Map<K, V>`, `List<K, I>`, `Tree<K, N>`, `Graph<P, L>`, or `Box<V>`";
        let syn::Type::Path(path) = ty else {
            return Err(syn::Error::new_spanned(ty, HELP));
        };
        let Some(segment) = path.path.segments.last() else {
            return Err(syn::Error::new_spanned(ty, HELP));
        };
        let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
            return Err(syn::Error::new_spanned(ty, HELP));
        };
        let types: Vec<Type> = arguments
            .args
            .iter()
            .filter_map(|argument| match argument {
                GenericArgument::Type(ty) => Some(ty.clone()),
                _ => None,
            })
            .collect();
        let mut items = types.into_iter();
        match (segment.ident.to_string().as_str(), items.len()) {
            ("Map", 2) => Ok(Self::Map {
                key: items.next().expect("length checked"),
                value: items.next().expect("length checked"),
            }),
            ("List", 2) => Ok(Self::List {
                key: items.next().expect("length checked"),
                item: items.next().expect("length checked"),
            }),
            ("Tree", 2) => Ok(Self::Tree {
                key: items.next().expect("length checked"),
                payload: items.next().expect("length checked"),
            }),
            ("Graph", 2) => Ok(Self::Graph {
                payload: items.next().expect("length checked"),
                label: items.next().expect("length checked"),
            }),
            ("Box", 1) => Ok(Self::Cell {
                value: items.next().expect("length checked"),
            }),
            _ => Err(syn::Error::new_spanned(ty, HELP)),
        }
    }
}

/// Declares a typed reactive view.
///
/// The struct's single tuple field must be a kind witness; the macro
/// rewrites the field to a zero-sized marker and generates the kind's fact
/// codec plus `ViewKind` with its handle pair (plan §5.2).
#[proc_macro_attribute]
pub fn view(args: TokenStream, item: TokenStream) -> TokenStream {
    if !args.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "`#[view]` takes no arguments: declare a kind witness as the struct's              single tuple field (`Map<K, V>`, `List<K, I>`, `Tree<K, N>`,              `Graph<P, L>`, or `Box<V>`)",
        )
        .to_compile_error()
        .into();
    }
    match expand_witness(item.into()) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn split_generics(
    item: &ItemStruct,
) -> (
    syn::ImplGenerics<'_>,
    syn::TypeGenerics<'_>,
    Option<&syn::WhereClause>,
) {
    item.generics.split_for_impl()
}

/// The shared hidden `View` implementation body, parameterized by the
/// kind's fact key and value types.
#[allow(clippy::too_many_arguments)]
fn view_impl(
    name: &syn::Ident,
    view_name: &str,
    impl_generics: &syn::ImplGenerics<'_>,
    ty_generics: &syn::TypeGenerics<'_>,
    where_clause: Option<&syn::WhereClause>,
    input: &Type,
    output: &Type,
    shared_writes: bool,
) -> proc_macro2::TokenStream {
    let shared = if shared_writes {
        quote! { true }
    } else {
        quote! { false }
    };
    quote! {
        impl #impl_generics ::plingo::reactive::view::View for #name #ty_generics #where_clause {
            type Input = #input;
            type Output = #output;

            fn name() -> &'static str { #view_name }

            fn __shared_writes() -> bool { #shared }

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

/// Expands the witness-field grammar: rewrites the tuple field to a
/// zero-sized marker and generates the kind implementation.
fn expand_witness(item: proc_macro2::TokenStream) -> syn::Result<proc_macro2::TokenStream> {
    let mut item: ItemStruct = syn::parse2(item)?;
    item.attrs.retain(|attr| !attr.path().is_ident("view"));
    let witness_type: Type = match &item.fields {
        syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            fields.unnamed.first().expect("length checked").ty.clone()
        }
        _ => {
            return Err(syn::Error::new_spanned(
                &item,
                "a view declares exactly one kind-witness tuple field",
            ));
        }
    };
    let witness = Witness::classify(&witness_type)?;
    let visibility = match &item.fields {
        syn::Fields::Unnamed(fields) => fields
            .unnamed
            .first()
            .map(|field| field.vis.clone())
            .unwrap_or_else(|| syn::Visibility::Inherited),
        _ => syn::Visibility::Inherited,
    };
    item.fields = syn::Fields::Unnamed(syn::parse_quote! {
        (#visibility ::std::marker::PhantomData<fn() -> #witness_type>)
    });

    let name = item.ident.clone();
    let view_name = name.to_string();
    let (impl_generics, ty_generics, where_clause) = split_generics(&item);
    let core = |input: &Type, output: &Type, shared: bool| {
        view_impl(
            &name,
            &view_name,
            &impl_generics,
            &ty_generics,
            where_clause,
            input,
            output,
            shared,
        )
    };
    let kind_impls = match &witness {
        Witness::Map { key, value } => {
            let core = core(key, value, false);
            quote! {
                #core
                impl #impl_generics ::plingo::reactive::kind::MapView
                    for #name #ty_generics #where_clause
                {
                }
                impl #impl_generics ::plingo::reactive::kind::ViewKind
                    for #name #ty_generics #where_clause
                {
                    type Emit = ::plingo::reactive::kind::MapEmit<Self>;
                    type Observe = ::plingo::reactive::kind::MapObserve<Self>;
                    type Patch = ::plingo::reactive::kind::MapPatch<Self>;
                }
            }
        }
        Witness::List {
            key,
            item: list_item,
        } => {
            let core = core(
                &syn::parse_quote!(::plingo::reactive::kind::ListKey<#key>),
                &syn::parse_quote!(::plingo::reactive::kind::ListFact<#list_item>),
                false,
            );
            quote! {
                #core
                impl #impl_generics ::plingo::reactive::kind::ListView
                    for #name #ty_generics #where_clause
                {
                    type Key = #key;
                    type Item = #list_item;
                }
                impl #impl_generics ::plingo::reactive::kind::ViewKind
                    for #name #ty_generics #where_clause
                {
                    type Emit = ::plingo::reactive::kind::ListEmit<Self>;
                    type Observe = ::plingo::reactive::kind::ListObserve<Self>;
                    type Patch = ::plingo::reactive::kind::NoPatch;
                }
            }
        }
        Witness::Tree { key, payload } => {
            let core = core(
                &syn::parse_quote!(
                    ::plingo::reactive::kind::TreeKey<
                        #key,
                        ::plingo::reactive::view::Node<Self>,
                    >
                ),
                &syn::parse_quote!(
                    ::plingo::reactive::kind::TreeFact<
                        ::plingo::reactive::view::Node<Self>,
                        #payload,
                    >
                ),
                true,
            );
            quote! {
                #core
                impl #impl_generics ::plingo::reactive::kind::TreeView
                    for #name #ty_generics #where_clause
                {
                    type Key = #key;
                    type Payload = #payload;
                }
                impl #impl_generics ::plingo::reactive::kind::ViewKind
                    for #name #ty_generics #where_clause
                {
                    type Emit = ::plingo::reactive::kind::TreeEmit<Self>;
                    type Observe = ::plingo::reactive::kind::TreeObserve<Self>;
                    type Patch = ::plingo::reactive::kind::TreePatch<Self>;
                }
            }
        }
        Witness::Graph { payload, label } => {
            let core = core(
                &syn::parse_quote!(
                    ::plingo::reactive::kind::GraphKey<
                        ::plingo::reactive::view::Node<Self>,
                        #label,
                    >
                ),
                &syn::parse_quote!(
                    ::plingo::reactive::kind::GraphFact<
                        #payload,
                        ::plingo::reactive::view::Node<Self>,
                    >
                ),
                true,
            );
            quote! {
                #core
                impl #impl_generics ::plingo::reactive::kind::GraphView
                    for #name #ty_generics #where_clause
                {
                    type NodePayload = #payload;
                    type Label = #label;
                }
                impl #impl_generics ::plingo::reactive::kind::ViewKind
                    for #name #ty_generics #where_clause
                {
                    type Emit = ::plingo::reactive::kind::GraphEmit<Self>;
                    type Observe = ::plingo::reactive::kind::GraphObserve<Self>;
                    type Patch = ::plingo::reactive::kind::NoPatch;
                }
            }
        }
        Witness::Cell { value } => {
            let core = core(&syn::parse_quote!(()), value, false);
            quote! {
                #core
                impl #impl_generics ::plingo::reactive::kind::BoxView
                    for #name #ty_generics #where_clause
                {
                }
                impl #impl_generics ::plingo::reactive::kind::ViewKind
                    for #name #ty_generics #where_clause
                {
                    type Emit = ::plingo::reactive::kind::BoxEmit<Self>;
                    type Observe = ::plingo::reactive::kind::BoxObserve<Self>;
                    type Patch = ::plingo::reactive::kind::NoPatch;
                }
            }
        }
    };
    let constructors = match &witness {
        Witness::Map { key, value } => quote! {
            impl #impl_generics #name #ty_generics #where_clause {
                /// Selects one component instance per present map entry.
                pub fn entries() -> ::plingo::reactive::framework_mount::MapEntries<Self> {
                    ::plingo::reactive::framework_mount::MapEntries::new()
                }
                /// Reads one map entry while recording its exact dependency.
                pub fn get(
                    key: &#key,
                ) -> ::plingo::reactive::Result<::std::option::Option<::std::sync::Arc<#value>>> {
                    ::plingo::reactive::kind::observe_view::<Self>()?.get(key)
                }
                /// Returns a desired map insertion/update.
                pub fn set(
                    key: #key,
                    value: #value,
                ) -> ::plingo::reactive::component::Set<Self> {
                    ::plingo::reactive::component::Set::__new(key, value)
                }
                /// Returns a desired map removal.
                pub fn remove(key: #key) -> ::plingo::reactive::component::Remove<Self> {
                    ::plingo::reactive::component::Remove::__new(key)
                }
            }
        },
        Witness::List { key, item } => quote! {
            impl #impl_generics #name #ty_generics #where_clause {
                /// Reads one list slot while recording its exact dependency.
                pub fn get(
                    key: &#key,
                    index: usize,
                ) -> ::plingo::reactive::Result<::std::option::Option<::std::sync::Arc<#item>>> {
                    ::plingo::reactive::kind::observe_view::<Self>()?.get(key, index)
                }
                /// Reads the current list length under one semantic key.
                pub fn len(
                    key: &#key,
                ) -> ::plingo::reactive::Result<usize> {
                    ::plingo::reactive::kind::observe_view::<Self>()?.len(key)
                }
                /// Reads all present list slots and the length fact.
                pub fn iter(
                    key: &#key,
                ) -> ::plingo::reactive::Result<::std::vec::Vec<::std::sync::Arc<#item>>> {
                    ::plingo::reactive::kind::observe_view::<Self>()?.iter(key)
                }
                /// Returns a desired replacement for one list domain.
                pub fn replace(
                    key: #key,
                    items: impl ::std::iter::IntoIterator<Item = #item>,
                ) -> ::plingo::reactive::component::Replace<Self> {
                    ::plingo::reactive::component::Replace::__new(key, items.into_iter().collect())
                }
            }
        },
        _ => quote! {},
    };
    Ok(quote! {
        #item
        #kind_impls
        #constructors
    })
}

/// Derives the deep-immutability `StateValue` marker (plan §5.6).
///
/// Every field must itself implement `StateValue`: the generated impl
/// carries `where`-clauses naming each field type, so interior mutability
/// (`Mutex`, atomics, cells, raw pointers) and unproven opaque fields fail
/// to compile.
#[proc_macro_derive(StateValue)]
pub fn derive_state_value(item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::DeriveInput);
    let name = &input.ident;
    let generics = &input.generics;
    let (impl_generics, type_generics, _) = generics.split_for_impl();

    let mut field_types: Vec<&syn::Type> = Vec::new();
    match &input.data {
        syn::Data::Struct(data) => {
            for field in data.fields.iter() {
                field_types.push(&field.ty);
            }
        }
        syn::Data::Enum(data) => {
            for variant in data.variants.iter() {
                for field in variant.fields.iter() {
                    field_types.push(&field.ty);
                }
            }
        }
        syn::Data::Union(_) => {
            return syn::Error::new_spanned(
                &input.ident,
                "StateValue cannot be derived for unions",
            )
            .to_compile_error()
            .into();
        }
    }

    // One bound per distinct field type; duplicates dedupe naturally.
    let mut predicates: Vec<syn::WherePredicate> = Vec::new();
    for ty in &field_types {
        let duplicate = predicates.iter().any(|existing| {
            quote::quote!(#existing).to_string()
                == quote::quote!(#ty: ::plingo::reactive::StateValue).to_string()
        });
        if duplicate {
            continue;
        }
        let parsed: syn::WherePredicate = syn::parse_quote!(#ty: ::plingo::reactive::StateValue);
        predicates.push(parsed);
    }

    let mut where_clause = generics
        .where_clause
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(where));
    for predicate in predicates {
        where_clause.predicates.push(predicate);
    }

    let expanded = quote::quote! {
        #[automatically_derived]
        unsafe impl #impl_generics ::plingo::reactive::StateValue for #name #type_generics #where_clause {}
    };
    expanded.into()
}

/// Expands a family member of a typed abstract tree.
#[proc_macro_attribute]

pub fn abstract_tree(args: TokenStream, item: TokenStream) -> TokenStream {
    let mut modern = abstract_tree_v2::Args::default();
    let modern_parser = syn::meta::parser(|meta| modern.parse(meta));
    if let Err(error) = syn::parse::Parser::parse2(modern_parser, args.into()) {
        return error.to_compile_error().into();
    }
    let item_enum = match syn::parse::<syn::ItemEnum>(item) {
        Ok(item_enum) => item_enum,
        Err(error) => return error.to_compile_error().into(),
    };
    match abstract_tree_v2::expand(&modern, item_enum) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

// Keep these imports type-checked in the proc-macro crate. They also make the
// accepted grammar explicit in rustdoc-generated diagnostics.
#[allow(dead_code)]
fn _result_type_shape(_: ReturnType, _: GenericArgument, _: PathArguments, _: TypePath) {}
// ---------------------------------------------------------------------------
// #[component] (plan §6)
// ---------------------------------------------------------------------------

/// Declares a first-class reactive component (plan §6).
///
/// The function takes exactly ONE semantic input — `Each<V>` for map
/// membership, an `AstBox<T>` tree node, or any plain value a parent
/// component passes — and returns either `Result<Effects>` (a desired
/// output) or `Result<AstBox<T>>` (a generated tree render slot). The macro
/// generates a zero-sized definition marker, a same-named definition module
/// containing the `Component` mount type, and the typed call surface that
/// reuses the authored function name. A second mount of the same
/// definition is a deterministic error.
///
/// ```ignore
/// #[component]
/// fn score(name: Each<Names>) -> Result<Set<Scores>> {
///     let quantity = Quantities::get(name.key())?.map(|value| *value);
///     Ok(Scores::set(name.into_key(), quantity.unwrap_or_default()))
/// }
/// ```
#[proc_macro_attribute]
pub fn component(args: TokenStream, item: TokenStream) -> TokenStream {
    if !args.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "`#[component]` takes no arguments",
        )
        .to_compile_error()
        .into();
    }
    let parsed = match syn::parse::<syn::ItemFn>(item.clone()) {
        Ok(function) => function,
        Err(error) => return error.to_compile_error().into(),
    };
    let modern = parsed.sig.inputs.len() == 1
        && parsed
            .sig
            .inputs
            .first()
            .and_then(|argument| match argument {
                syn::FnArg::Typed(argument) => {
                    let syn::Type::Path(path) = argument.ty.as_ref() else {
                        return None;
                    };
                    path.path.segments.last().map(|segment| {
                        segment.ident == "Each" || segment.ident == "AstBox"
                    })
                }
                syn::FnArg::Receiver(_) => None,
            })
            .unwrap_or(false);
    let result = if modern {
        expand_component_modern(parsed)
    } else {
        expand_component_v2(parsed)
    };
    match result {
        Ok(tokens) => {
            if std::env::var_os("PLINGO_DUMP_COMPONENT").is_some() {
                std::fs::write("/tmp/component_expansion.rs", tokens.to_string()).ok();
            }
            tokens.into()
        }
        Err(error) => error.to_compile_error().into(),
    }
}

fn component_result_type(item: &syn::ItemFn) -> syn::Result<syn::Type> {
    let syn::ReturnType::Type(_, ty) = &item.sig.output else {
        return Err(syn::Error::new_spanned(
            &item.sig.output,
            "a component must return Result<Effects> or Result<AstBox<T>>",
        ));
    };
    let syn::Type::Path(path) = ty.as_ref() else {
        return Err(syn::Error::new_spanned(
            ty,
            "a component must return Result<Effects> or Result<AstBox<T>>",
        ));
    };
    let Some(segment) = path.path.segments.last() else {
        return Err(syn::Error::new_spanned(
            ty,
            "component result must be Result<E>",
        ));
    };
    if segment.ident != "Result" {
        return Err(syn::Error::new_spanned(
            ty,
            "component result must be Result<E>",
        ));
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            "component result must be Result<E>",
        ));
    };
    let Some(syn::GenericArgument::Type(output)) = arguments.args.first() else {
        return Err(syn::Error::new_spanned(
            ty,
            "component result must be Result<E>",
        ));
    };
    Ok(output.clone())
}

fn component_ast_box_type(ty: &syn::Type) -> Option<syn::Type> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "AstBox" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    match arguments.args.first()? {
        syn::GenericArgument::Type(inner) => Some(inner.clone()),
        _ => None,
    }
}

fn component_input_type(ty: &syn::Type, name: &str) -> Option<syn::Type> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != name {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    match arguments.args.first()? {
        syn::GenericArgument::Type(inner) => Some(inner.clone()),
        _ => None,
    }
}

fn expand_component_modern(item: syn::ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    use syn::{FnArg, Pat};
    if item.sig.generics.params.len() != 0 || item.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &item.sig.generics,
            "`#[component]` functions may not be generic",
        ));
    }
    if item.sig.asyncness.is_some() || item.sig.variadic.is_some() {
        return Err(syn::Error::new_spanned(
            &item.sig,
            "`#[component]` functions are synchronous and non-variadic",
        ));
    }
    if item.sig.inputs.len() != 1 {
        return Err(syn::Error::new_spanned(
            &item.sig.inputs,
            "a component has exactly one semantic input",
        ));
    }
    let FnArg::Typed(argument) = item.sig.inputs.first().expect("one argument") else {
        return Err(syn::Error::new_spanned(
            &item.sig,
            "a component has no receiver",
        ));
    };
    let Pat::Ident(pattern) = argument.pat.as_ref() else {
        return Err(syn::Error::new_spanned(
            &argument.pat,
            "the component input must be a plain identifier",
        ));
    };
    let input_ident = pattern.ident.clone();
    let input_ty = argument.ty.as_ref().clone();
    let (mode, semantic_ty, runtime_input) =
        if let Some(view) = component_input_type(&input_ty, "Each") {
            (
                "map",
                view.clone(),
                quote! { <#view as ::plingo::reactive::View>::Input },
            )
        } else if let Some(node) = component_input_type(&input_ty, "AstBox") {
            (
                "tree",
                node.clone(),
                quote! { ::plingo::reactive::abstract_tree::AstBox<#node> },
            )
        } else {
            ("semantic", input_ty.clone(), quote! { #input_ty })
        };
    let output_ty = component_result_type(&item)?;
    let tree_output = component_ast_box_type(&output_ty);
    if mode == "tree" && tree_output.is_none() {
        // Tree readers may be ordinary analysis components.  Only a
        // component that returns an AstBox owns a generated render slot.
    }
    let _ = mode;
    let fn_ident = &item.sig.ident;
    let body_ident = format_ident!("__plingo_{}_body", fn_ident);
    let marker_ident = format_ident!("Component{}", to_pascal_case(&fn_ident.to_string()));
    let vis = &item.vis;
    let attrs = &item.attrs;
    let body = &item.block;
    let ret = &item.sig.output;
    let body_param_ty = if mode == "map" {
        quote! { #input_ty }
    } else {
        quote! { #input_ty }
    };
    let invoke = if mode == "map" && tree_output.is_some() {
        let node = tree_output.clone().expect("checked above");
        quote! {
            ::plingo::reactive::component::__call_tree_component::<
                #marker_ident,
                _,
                _,
                #node,
            >(
                |__key| #body_ident(
                    ::plingo::reactive::component::Each::<#semantic_ty>::__from_key(__key)
                ),
                #input_ident.into_key(),
            )
        }
    } else if mode == "map" {
        quote! {
            ::plingo::reactive::component::__call_component::<
                #marker_ident, _, _, _
            >(
                |__key| #body_ident(
                    ::plingo::reactive::component::Each::<#semantic_ty>::__from_key(__key)
                ),
                #input_ident.into_key(),
            )
        }
    } else if mode == "semantic" && tree_output.is_some() {
        let node = tree_output.clone().expect("checked above");
        quote! {
            ::plingo::reactive::component::__call_tree_component::<
                #marker_ident, _, _, #node,
            >(#body_ident, #input_ident)
        }
    } else if mode == "semantic" {
        quote! {
            ::plingo::reactive::component::__call_component::<
                #marker_ident, _, _, _
            >(#body_ident, #input_ident)
        }
    } else if mode == "tree" && tree_output.is_some() {
        let node = tree_output.clone().expect("checked above");
        quote! {
            ::plingo::reactive::component::__call_tree_component::<
                #marker_ident,
                _,
                #input_ty,
                #node,
            >(#body_ident, #input_ident)
        }
    } else if mode == "tree" {
        quote! {
            ::plingo::reactive::component::__call_component::<
                #marker_ident, _, _, _
            >(#body_ident, #input_ident)
        }
    } else {
        quote! {}
    };
    let mount = if mode == "map" && tree_output.is_some() {
        let node = tree_output.clone().expect("checked above");
        quote! {
            impl ::plingo::reactive::framework_mount::MountComponent<
                ::plingo::reactive::framework_mount::MapEntries<#semantic_ty>
            > for Component
            where
                <#node as ::plingo::reactive::abstract_tree::AbstractTreeNode>::Family:
                    ::plingo::reactive::abstract_tree::AbstractTreeFamily,
                <<#node as ::plingo::reactive::abstract_tree::AbstractTreeNode>::Family
                    as ::plingo::reactive::abstract_tree::AbstractTreeFamily>::Domain:
                    ::core::convert::From<<#semantic_ty as ::plingo::reactive::View>::Input>,
            {
                type Output = ();
                fn mount(
                    __engine: &mut ::plingo::reactive::Engine,
                    _selector: ::plingo::reactive::framework_mount::MapEntries<#semantic_ty>,
                ) -> ::plingo::reactive::Result<()> {
                    __engine.install_component_each_key::<#marker_ident, #semantic_ty, _>(
                        |__key| {
                            let __rendered = #body_ident(
                                ::plingo::reactive::component::Each::<#semantic_ty>::__from_key(__key.clone()),
                            )?;
                            ::plingo::reactive::abstract_tree::__set_root::<
                                <#node as ::plingo::reactive::abstract_tree::AbstractTreeNode>::Family,
                            >(
                                __key.into(),
                                __rendered.erased(),
                            )
                        },
                    )?;
                    Ok(())
                }
            }
        }
    } else if mode == "map" {
        quote! {
            impl ::plingo::reactive::framework_mount::MountComponent<
                ::plingo::reactive::framework_mount::MapEntries<#semantic_ty>
            > for Component {
                type Output =
                    ::plingo::reactive::KeyedFamily<#semantic_ty>;
                fn mount(
                    __engine: &mut ::plingo::reactive::Engine,
                    _selector: ::plingo::reactive::framework_mount::MapEntries<#semantic_ty>,
                ) -> ::plingo::reactive::Result<
                    ::plingo::reactive::KeyedFamily<#semantic_ty>,
                > {
                    __engine.install_component_each_key_effect::<#marker_ident, #semantic_ty, _, _>(
                        move |__key| #body_ident(
                            ::plingo::reactive::component::Each::<#semantic_ty>::__from_key(__key),
                        ),
                    )
                }
            }
        }
    } else if mode == "tree" && tree_output.is_none() {
        let input_node = component_input_type(&input_ty, "AstBox").expect("tree input");
        let source_family = quote! {
            <#input_node as ::plingo::reactive::abstract_tree::AbstractTreeNode>::Family
        };
        quote! {
            impl ::plingo::reactive::framework_mount::MountComponent<
                ::plingo::reactive::abstract_tree::NodeSelector<#source_family, #input_node>,
            > for Component {
                type Output = ();
                fn mount(
                    __engine: &mut ::plingo::reactive::Engine,
                    _selector: ::plingo::reactive::abstract_tree::NodeSelector<#source_family, #input_node>,
                ) -> ::plingo::reactive::Result<()> {
                    __engine.install_component_tree_nodes::<
                        #marker_ident,
                        #source_family,
                        #input_node,
                        (),
                        _,
                    >(_selector, move |__node| {
                        let __output = #body_ident(__node)?;
                        <#output_ty as ::plingo::reactive::component::Effects>::__apply(&__output)?;
                        Ok(())
                    })?;
                    Ok(())
                }
            }
        }
    } else if mode == "tree" {
        let input_node = component_input_type(&input_ty, "AstBox").expect("tree input");
        let output_node = tree_output.clone().expect("tree output");
        let source_family = quote! {
            <#input_node as ::plingo::reactive::abstract_tree::AbstractTreeNode>::Family
        };
        let target_family = quote! {
            <#output_node as ::plingo::reactive::abstract_tree::AbstractTreeNode>::Family
        };
        quote! {
            impl ::plingo::reactive::framework_mount::MountComponent<
                ::plingo::reactive::abstract_tree::RootSelector<#source_family, #input_node>,
            > for Component
            where
                <#target_family as ::plingo::reactive::abstract_tree::AbstractTreeFamily>::Domain:
                    ::core::convert::From<
                        <#source_family as ::plingo::reactive::abstract_tree::AbstractTreeFamily>::Domain
                    >,
            {
                type Output = ();
                fn mount(
                    __engine: &mut ::plingo::reactive::Engine,
                    _selector: ::plingo::reactive::abstract_tree::RootSelector<#source_family, #input_node>,
                ) -> ::plingo::reactive::Result<()> {
                    __engine.install_component_tree_roots::<
                        #marker_ident,
                        #source_family,
                        _,
                        (),
                        _,
                    >(_selector, move |__domain, __node| {
                        let __rendered = #body_ident(__node)?;
                        ::plingo::reactive::abstract_tree::__set_root::<#target_family>(
                            __domain.into(),
                            __rendered.erased(),
                        )?;
                        Ok(())
                    })?;
                    Ok(())
                }
            }

            impl<__DomainProjection>
                ::plingo::reactive::framework_mount::MountComponentWithDomain<
                    ::plingo::reactive::abstract_tree::RootSelector<#source_family, #input_node>,
                    __DomainProjection,
                > for Component
            where
                __DomainProjection:
                    Fn(
                        <#source_family as ::plingo::reactive::abstract_tree::AbstractTreeFamily>::Domain,
                    ) -> <#target_family as ::plingo::reactive::abstract_tree::AbstractTreeFamily>::Domain
                    + Clone
                    + Send
                    + Sync
                    + 'static,
            {
                fn mount_with_domain(
                    __engine: &mut ::plingo::reactive::Engine,
                    _selector: ::plingo::reactive::abstract_tree::RootSelector<#source_family, #input_node>,
                    __projection: __DomainProjection,
                ) -> ::plingo::reactive::Result<()> {
                    __engine.install_component_tree_roots::<
                        #marker_ident,
                        #source_family,
                        _,
                        (),
                        _,
                    >(_selector, move |__domain, __node| {
                        let __rendered = #body_ident(__node)?;
                        ::plingo::reactive::abstract_tree::__set_root::<#target_family>(
                            __projection(__domain),
                            __rendered.erased(),
                        )?;
                        Ok(())
                    })?;
                    Ok(())
                }
            }
        }
    } else {
        quote! {}
    };
    let tokens = quote! {
        #[derive(Clone, Copy, Debug)]
        #vis struct #marker_ident;
        impl ::plingo::reactive::component::ComponentDefinition for #marker_ident {
            fn __descriptor() -> &'static str {
                ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#fn_ident))
            }
        }
        #[doc(hidden)]
        #vis fn #body_ident(#input_ident: #body_param_ty) #ret #body
        #(#attrs)*
        #vis fn #fn_ident(#input_ident: #input_ty) #ret {
            #invoke
        }
        #vis mod #fn_ident {
            /// Generated component definition used by `WorkspaceBuilder::mount`.
            #[derive(Clone, Copy, Debug, Default)]
            pub struct Component;
            /// Builds a typed root mount request.
            pub fn on<S>(
                selector: S,
            ) -> ::plingo::reactive::framework_mount::MountToken<Component, S> {
                ::plingo::reactive::framework_mount::MountToken::new(selector)
            }
            #mount
            use super::*;
        }
    };
    if std::env::var_os("PLINGO_DUMP_COMPONENT").is_some() {
        std::fs::write(format!("/tmp/component_{fn_ident}.rs"), tokens.to_string()).ok();
    }
    Ok(tokens)
}

fn to_pascal_case(name: &str) -> String {
    name.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn type_constructor(ty: &syn::Type) -> Option<(&syn::Ident, Vec<syn::Type>)> {
    let syn::Type::Path(path) = ty else { return None };
    let segment = path.path.segments.last()?;
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    let types = arguments
        .args
        .iter()
        .filter_map(|argument| match argument {
            syn::GenericArgument::Type(ty) => Some(ty.clone()),
            _ => None,
        })
        .collect();
    Some((&segment.ident, types))
}

fn tuple_type(types: &[syn::Type]) -> proc_macro2::TokenStream {
    match types {
        [] => quote! { () },
        [one] => quote! { (#one,) },
        many => quote! { (#(#many),*) },
    }
}

fn tuple_value(idents: &[syn::Ident]) -> proc_macro2::TokenStream {
    match idents {
        [] => quote! { () },
        [one] => quote! { (#one,) },
        many => quote! { (#(#many),*) },
    }
}

fn tuple_pattern(idents: &[syn::Ident]) -> proc_macro2::TokenStream {
    match idents {
        [] => quote! { () },
        [one] => quote! { (#one,) },
        many => quote! { (#(#many),*) },
    }
}

fn validate_case_chain(
    expression: &syn::Expr,
    cases: &mut std::collections::HashSet<String>,
) -> syn::Result<()> {
    match expression {
        syn::Expr::MethodCall(call) => {
            if call.method == "case" {
                let Some(turbofish) = &call.turbofish else {
                    return Err(syn::Error::new_spanned(
                        &call.method,
                        "`.case` requires a member type: `.case::<Member>(closure)`",
                    ));
                };
                let Some(argument) = turbofish.args.first() else {
                    return Err(syn::Error::new_spanned(
                        turbofish,
                        "`.case` requires one member type",
                    ));
                };
                let syn::GenericArgument::Type(ty) = argument else {
                    return Err(syn::Error::new_spanned(
                        argument,
                        "`.case` member must be a type",
                    ));
                };
                let name = quote!(#ty).to_string();
                if !cases.insert(name) {
                    return Err(syn::Error::new_spanned(
                        ty,
                        "duplicate heterogeneous component case",
                    ));
                }
            }
            validate_case_chain(&call.receiver, cases)
        }
        syn::Expr::Paren(paren) => validate_case_chain(&paren.expr, cases),
        syn::Expr::Group(group) => validate_case_chain(&group.expr, cases),
        _ => Ok(()),
    }
}

/// Expands the v2 ordinary function surface. A family function is authored
/// with `FamilyNode<F>` but called with any typed `AstBox<Member>` belonging
/// to `F`; direct functions keep their ordinary stable-key first argument.
fn expand_component_v2(item: syn::ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    use syn::{FnArg, Pat};

    if item.sig.generics.params.len() != 0 || item.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &item.sig.generics,
            "`#[component]` functions may not be generic; family dispatch generates its call generic",
        ));
    }
    if item.sig.asyncness.is_some() || item.sig.variadic.is_some() {
        return Err(syn::Error::new_spanned(
            &item.sig,
            "`#[component]` functions are synchronous and non-variadic",
        ));
    }
    if item.sig.inputs.is_empty() {
        return Err(syn::Error::new_spanned(
            &item.sig.inputs,
            "a component requires one stable key input",
        ));
    }
    let mut idents = Vec::new();
    let mut types = Vec::new();
    for argument in &item.sig.inputs {
        let FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new_spanned(&item.sig, "components do not have a receiver"));
        };
        let Pat::Ident(pattern) = argument.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &argument.pat,
                "component parameters must be plain identifiers",
            ));
        };
        idents.push(pattern.ident.clone());
        types.push(argument.ty.as_ref().clone());
    }
    let output_ty = component_result_type(&item)?;
    let Some((first_name, first_args)) = type_constructor(&types[0]) else {
        return Err(syn::Error::new_spanned(
            &types[0],
            "the first component parameter must be a stable key or FamilyNode<F>",
        ));
    };
    let family = if first_name == "FamilyNode" {
        if first_args.len() != 1 {
            return Err(syn::Error::new_spanned(
                &types[0],
                "FamilyNode requires exactly one family type",
            ));
        }
        Some(first_args[0].clone())
    } else {
        None
    };
    if let Some(_) = &family {
        let mut cases = std::collections::HashSet::new();
        let body_expression = match item.block.stmts.as_slice() {
            [syn::Stmt::Expr(expression, None)] => expression,
            _ => {
                return Err(syn::Error::new_spanned(
                    &item.block,
                    "a heterogeneous component body must be exactly one `.cases(...).case(...)...otherwise(...)` expression",
                ));
            }
        };
        validate_case_chain(body_expression, &mut cases)?;
    }

    let fn_ident = &item.sig.ident;
    let marker_ident = format_ident!("Component{}", to_pascal_case(&fn_ident.to_string()));
    let body_ident = format_ident!("__plingo_{}_body", fn_ident);
    let vis = &item.vis;
    let attrs = &item.attrs;
    let body = &item.block;
    let ret = &item.sig.output;
    let prop_idents = &idents[1..];
    let prop_types = &types[1..];
    let props_ty = tuple_type(prop_types);
    let props_value = tuple_value(prop_idents);
    let props_pattern = tuple_pattern(
        &prop_idents
            .iter()
            .map(|ident| format_ident!("__plingo_prop_{}", ident))
            .collect::<Vec<_>>(),
    );
    let prop_bindings: Vec<_> = prop_idents
        .iter()
        .map(|ident| format_ident!("__plingo_prop_{}", ident))
        .collect();
    let body_call = quote! { #body_ident(__plingo_key, #(#prop_bindings),*) };
    let input_ty = &types[0];
    let (public_signature, public_where, call_input) = if let Some(family) = &family {
        let node_ident = format_ident!("__PlingoNode");
        (
            quote! {
                <#node_ident>(__plingo_input: #node_ident, #(#prop_idents: #prop_types),*)
            },
            quote! {
                where
                    #node_ident: ::core::convert::Into<
                        ::plingo::reactive::component::FamilyNode<#family>
                    >,
            },
            quote! { __plingo_input.into() },
        )
    } else {
        (
            quote! { (__plingo_input: #input_ty, #(#prop_idents: #prop_types),*) },
            quote! {},
            quote! { __plingo_input },
        )
    };
    let call = quote! {
        ::plingo::reactive::component::__call_component_props::<
            #marker_ident, _, _, #props_ty, _
        >(
            |__plingo_key, __plingo_props: #props_ty| {
                let #props_pattern = __plingo_props;
                #body_call
            },
            #call_input,
            #props_value,
        )
    };
    let public_call = quote! { #call };
    let mount = if let Some(family) = &family {
        quote! {
            impl<__PlingoN, __PlingoP>
                ::plingo::reactive::framework_mount::MountComponentWithProps<
                    ::plingo::reactive::abstract_tree::RootSelector<#family, __PlingoN>,
                    __PlingoP,
                > for Component
            where
                __PlingoN: ::plingo::reactive::abstract_tree::AbstractTreeNode<Family = #family>,
                __PlingoP: ::core::clone::Clone
                    + ::core::cmp::PartialEq
                    + ::core::fmt::Debug
                    + ::core::marker::Send
                    + ::core::marker::Sync
                    + 'static,
                #props_ty: ::core::convert::From<__PlingoP>,
            {
                fn mount_with_props(
                    __engine: &mut ::plingo::reactive::Engine,
                    __selector: ::plingo::reactive::abstract_tree::RootSelector<#family, __PlingoN>,
                    __props: __PlingoP,
                ) -> ::plingo::reactive::Result<()> {
                    __engine.install_component_tree_family_roots::<
                        #marker_ident, #family, __PlingoN, __PlingoP, #output_ty, _
                    >(__selector, __props, move |__node, __props| {
                        let __plingo_key = __node;
                        let __props: #props_ty = __props.into();
                        let #props_pattern = __props;
                        #body_call
                    })
                }
            }
        }
    } else {
        quote! {}
    };
    let on = if family.is_some() {
        quote! {
            pub fn on<__PlingoS>(
                __selector: __PlingoS,
                #(#prop_idents: #prop_types),*
            ) -> ::plingo::reactive::framework_mount::MountTokenWithProps<
                Component, __PlingoS, #props_ty
            > {
                ::plingo::reactive::framework_mount::MountTokenWithProps::new(
                    __selector,
                    #props_value,
                )
            }
        }
    } else {
        quote! {}
    };
    let public_fn = if family.is_some() {
        quote! {
            #vis fn #fn_ident
                #public_signature #ret #public_where
                #public_call
        }
    } else {
        quote! {
            #vis fn #fn_ident #public_signature #ret
                #public_call
        }
    };
    Ok(quote! {
        #[derive(Clone, Copy, Debug)]
        #vis struct #marker_ident;
        impl ::plingo::reactive::component::ComponentDefinition for #marker_ident {
            fn __descriptor() -> &'static str {
                ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#fn_ident))
            }
        }
        #[doc(hidden)]
        #vis fn #body_ident(#(#idents: #types),*) #ret #body
        #(#attrs)*
        #public_fn
        #vis mod #fn_ident {
            #[derive(Clone, Copy, Debug, Default)]
            pub struct Component;
            #on
            #mount
            use super::*;
        }
    })
}
