//! Procedural macros for the transparent reactive authoring surface.
//!
//! `#[view]` declares one typed reactive view from a single kind-witness
//! tuple field (`Map<K, V>`, `List<K, I>`, `Tree<K, N>`, `Graph<P, L>`, or
//! `Box<V>`); the witness selects the fact codec and the emit/observe
//! handle pair (plan §5.2). Computations are ordinary Rust functions;
//! there is deliberately no component attribute or generated runtime
//! descriptor.

use proc_macro::TokenStream;
use quote::quote;
use syn::{GenericArgument, ItemStruct, PathArguments, ReturnType, Type, TypePath};

mod abstract_tree;

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
            ))
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
        Witness::List { key, item: list_item } => {
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
    Ok(quote! {
        #item
        #kind_impls
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
            quote::quote!(#existing).to_string() == quote::quote!(#ty: ::plingo::reactive::StateValue).to_string()
        });
        if duplicate {
            continue;
        }
        let parsed: syn::WherePredicate = syn::parse_quote!(#ty: ::plingo::reactive::StateValue);
        predicates.push(parsed);
    }

    let mut where_clause = generics.where_clause.clone().unwrap_or_else(|| syn::parse_quote!(where));
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
    let mut parsed = abstract_tree::AbstractTreeArgs { members: None };
    let parser = syn::meta::parser(|meta| parsed.parse(meta));
    let parsed = match syn::parse::Parser::parse2(parser, args.into()) {
        Ok(()) => parsed,
        Err(error) => return error.to_compile_error().into(),
    };
    match abstract_tree::expand(&parsed, item.into()) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

// Keep these imports type-checked in the proc-macro crate. They also make the
// accepted grammar explicit in rustdoc-generated diagnostics.
#[allow(dead_code)]
fn _result_type_shape(_: ReturnType, _: GenericArgument, _: PathArguments, _: TypePath) {}
