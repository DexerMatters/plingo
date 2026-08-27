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
use syn::spanned::Spanned;
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
    let mut parsed = abstract_tree::AbstractTreeArgs { members: None };
    let parser = syn::meta::parser(|meta| parsed.parse(meta));
    let parsed = match syn::parse::Parser::parse2(parser, args.into()) {
        Ok(()) => parsed,
        Err(error) => return error.to_compile_error().into(),
    };
    match abstract_tree::expand(&parsed, item.into()) {
        Ok(tokens) => {
            let text = tokens.to_string();
            std::fs::write("/tmp/tree_expansion.txt", &text).ok();
            if let Some(pos) = text.find("emit_update") {
                let begin = pos.saturating_sub(2000);
                let end = (pos + 4000).min(text.len());
                std::fs::write("/tmp/tree_update_context.rs", &text[begin..end]).ok();
            }
            tokens.into()
        }
        Err(error) => error.to_compile_error().into(),
    }
}

// Keep these imports type-checked in the proc-macro crate. They also make the
// accepted grammar explicit in rustdoc-generated diagnostics.
#[allow(dead_code)]
fn _result_type_shape(_: ReturnType, _: GenericArgument, _: PathArguments, _: TypePath) {}
// ---------------------------------------------------------------------------
// #[component] (follow-up plan §6.1 / Cut C)
// ---------------------------------------------------------------------------

/// One parsed port of a `#[component]` signature.
enum ComponentPort {
    /// Membership driver: one instance per present map key.
    EachKey {
        ident: syn::Ident,
        view: Box<syn::Type>,
    },
    /// Exact recorded reads over one view.
    Read {
        ident: syn::Ident,
        view: Box<syn::Type>,
    },
    /// Owned writes to one view.
    Write {
        ident: syn::Ident,
        view: Box<syn::Type>,
    },
    /// One generated automatic node output port.
    Output {
        ident: syn::Ident,
        view: Box<syn::Type>,
    },
}

impl ComponentPort {
    fn ident(&self) -> &syn::Ident {
        match self {
            ComponentPort::EachKey { ident, .. }
            | ComponentPort::Read { ident, .. }
            | ComponentPort::Write { ident, .. }
            | ComponentPort::Output { ident, .. } => ident,
        }
    }

    fn view(&self) -> &syn::Type {
        match self {
            ComponentPort::EachKey { view, .. }
            | ComponentPort::Read { view, .. }
            | ComponentPort::Write { view, .. }
            | ComponentPort::Output { view, .. } => view,
        }
    }

    fn is_driver(&self) -> bool {
        matches!(self, ComponentPort::EachKey { .. })
    }

    fn is_output(&self) -> bool {
        matches!(self, ComponentPort::Output { .. })
    }
}


/// Extracts `<T>` from a port type whose last path segment is `segment`.
fn port_view_argument<'t>(ty: &'t syn::Type) -> Option<&'t syn::Type> {
    let syn::Type::Path(type_path) = ty else {
        return None;
    };
    let last = type_path.path.segments.last()?;
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    match args.args.iter().next()? {
        GenericArgument::Type(view) => Some(view),
        _ => None,
    }
}

/// Declares a first-class reactive component (follow-up plan §6.1).
///
/// The function's parameters are TYPED PORTS and nothing else; exactly one
/// `EachKey<V>` driver is required in Cut C. The macro generates a
/// zero-sized definition marker (identity = marker + driving element), the
/// [`ComponentDefinition`] impl with the module-qualified descriptor, and
/// an installer. A second install of the same definition is a
/// deterministic error.
///
/// ```ignore
/// #[component]
/// fn record(
///     key: EachKey<Names>,
///     names: Read<Names>,
///     records: Write<Records>,
/// ) -> Result<()> { /* pure control flow plus port operations */ }
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
    let function = syn::parse::<syn::ItemFn>(item);
    match function.and_then(expand_component) {
        Ok(tokens) => {
            if std::env::var_os("PLINGO_DUMP_COMPONENT").is_some() {
                std::fs::write("/tmp/component_expansion.rs", tokens.to_string()).ok();
            }
            tokens.into()
        }
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand_component(mut item: syn::ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    use syn::{FnArg, Pat};

    if !item.sig.generics.params.is_empty() || item.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new(
            item.sig.ident.span(),
            "`#[component]` functions may not be generic",
        ));
    }
    if item.sig.asyncness.is_some() || item.sig.variadic.is_some() {
        return Err(syn::Error::new(
            item.sig.ident.span(),
            "`#[component]` functions are synchronous and non-variadic",
        ));
    }

    let mut ports: Vec<ComponentPort> = Vec::new();
    for arg in &item.sig.inputs {
        let FnArg::Typed(pat_type) = arg else {
            return Err(syn::Error::new(
                item.sig.ident.span(),
                "`#[component]` takes no receiver",
            ));
        };
        let ident = match pat_type.pat.as_ref() {
            Pat::Ident(pat_ident) => pat_ident.ident.clone(),
            _ => {
                return Err(syn::Error::new(
                    syn::spanned::Spanned::span(pat_type.pat.as_ref()),
                    "component ports must be plain identifiers",
                ));
            }
        };
        let ty = pat_type.ty.as_ref();
        let kind = match ty {
            syn::Type::Path(type_path) => type_path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let view = Box::new(port_view_argument(ty).cloned().ok_or_else(|| {
            syn::Error::new(
                syn::spanned::Spanned::span(ty),
                "port type must carry its view parameter",
            )
        })?);
        let port = match kind.as_str() {
            "EachKey" => ComponentPort::EachKey { ident, view },
            "Read" => ComponentPort::Read { ident, view },
            "Write" => ComponentPort::Write { ident, view },
            "Output" | "NodeOutput" => ComponentPort::Output { ident, view },
            other => {
                return Err(syn::Error::new(
                    syn::spanned::Spanned::span(ty),
                    format!(
                        "`{other}` is not a component port; expected EachKey<V>, Read<V>, Write<V>, or Output<V>"
                    ),
                ));
            }
        };
        ports.push(port);
    }

    let drivers: Vec<&ComponentPort> = ports.iter().filter(|port| port.is_driver()).collect();
    if drivers.len() != 1 {
        return Err(syn::Error::new(
            item.sig.ident.span(),
            "`#[component]` requires exactly one EachKey<V> driver port (Cut C)",
        ));
    }
    let driver_view = drivers[0].view().clone();

    // The authored body becomes the trampoline's inner call: every port is
    // rewritten to its runtime form.
    let inner_params = ports.iter().map(|port| {
        let ident = port.ident();
        let ty = match port {
            ComponentPort::EachKey { view, .. } => quote! {
                <#view as ::plingo::reactive::View>::Input
            },
            ComponentPort::Read { view, .. } => quote! {
                ::plingo::reactive::component::Read<#view>
            },
            ComponentPort::Write { view, .. } => quote! {
                ::plingo::reactive::component::Write<#view>
            },
            ComponentPort::Output { view, .. } => quote! {
                ::plingo::reactive::component::Output<#view>
            },
        };
        quote! { #ident: #ty }
    });

    let fn_ident = &item.sig.ident;
    let marker_ident =
        quote::format_ident!("Component{}", to_pascal_case(&item.sig.ident.to_string()));
    let installer_ident = quote::format_ident!("{}_install", item.sig.ident);
    let key_ident = drivers[0].ident().clone();
    let mut output_ordinal = 0u16;

    // Inside the trampoline, non-driver ports attach through the crate seam;
    // the driver arrives as the plain input value.
    let attach_lets = ports.iter().filter_map(|port| {
        if port.is_driver() {
            return None;
        }
        let ident = port.ident();
        let view = port.view();
        match port {
            ComponentPort::Read { .. } => Some(quote! {
                let #ident =
                    ::plingo::reactive::component::Read::<#view>::__attach();
            }),
            ComponentPort::Write { .. } => Some(quote! {
                let #ident =
                    ::plingo::reactive::component::Write::<#view>::__attach();
            }),
            ComponentPort::Output { .. } => {
                let ordinal = output_ordinal;
                output_ordinal = output_ordinal.saturating_add(1);
                Some(quote! {
                    let #ident =
                        ::plingo::reactive::component::Output::<#view>::__attach::<
                            #marker_ident,
                            _,
                        >(#key_ident.clone(), #ordinal)?;
                })
            }
            ComponentPort::EachKey { .. } => unreachable!(),
        }
    });

    let body_args = ports.iter().map(|port| port.ident());

    let vis = &item.vis;
    let attrs = &item.attrs;
    let ret = &item.sig.output;
    let body = &item.block;

    Ok(quote! {
        /// Definition marker (Cut C): identity derives from this type plus
        /// the exact driving element.
        #[derive(Clone, Copy, Debug)]
        #[allow(missing_docs)]
        #vis struct #marker_ident;

        impl ::plingo::reactive::component::ComponentDefinition for #marker_ident {
            fn __descriptor() -> &'static str {
                ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#fn_ident))
            }
        }

        #[doc = "Authored component body; invoked only by the generated trampoline."]
        #(#attrs)*
        #[doc(hidden)]
        #[allow(clippy::too_many_arguments)]
        #vis fn #fn_ident (
            #(#inner_params),*
        ) #ret #body

        /// Installs this component into an engine (Cut C). A second install
        /// of the same definition fails before mutating anything.
        #vis fn #installer_ident (
            engine: &mut ::plingo::reactive::Engine,
        ) -> ::plingo::Result<::plingo::reactive::KeyedFamily<#driver_view>> {
            engine.install_component_each_key::<#marker_ident, #driver_view, _>(
                move |#key_ident| {
                    #(#attach_lets)*
                    #fn_ident(#(#body_args),*)
                },
            )
        }
    })
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
