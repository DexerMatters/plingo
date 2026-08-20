//! `#[component]` and `#[view]` — the reactive authoring macros.
//!
//! `#[component]` generates, from an ordinary function whose signature is
//! expressed in view handles, the `Component` impl: signature edges
//! (observed/previous/emitted views), store registration, and the root run
//! that executes the author's body. The generated code references only
//! `reactive` types; no legacy runtime type appears.
//!
//! Both the explicit handle form and the sugar forms of plan §4 compile to
//! the same impl:
//!
//! ```ignore
//! #[component]
//! fn check(
//!     syntax: SyntaxTree<Stlc>,        // bare ≡ Observed<_>
//!     scopes: ScopeGraph<StlcScope>,   // shared with name_pass
//!     types: Previous<TypeFacts>,
//! ) -> (TypeFacts, StlcDiagnostics, ScopeGraph<StlcScope>)
//! //  ^ bare tuple ≡ Result<(Emitted<...>, ...)>
//! { ... }
//! ```
//!
//! Rules:
//!
//! - Argument: bare `V` ≡ `Observed<V>`; `Observed<V>`/`Previous<V>` stay
//!   accepted; `Emitted` is rejected in argument position.
//! - Return: `(E, F, G)` / `Result<(E, F, G)>`; `()`/`Result<()>` declares a
//!   sink (observe-only) component.
//! - Duplicate views in one signature are errors except one
//!   `Observed<V>`/`Previous<V>` pair for the same `V`.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, GenericArgument, ItemFn, Pat, PathArguments, ReturnType, Type, TypePath};

mod abstract_tree;



/// The view handle kinds recognized in the signature.
#[derive(Clone)]
enum HandleKind {
    Observed,
    Previous,
    Emitted,
}

/// One signature element: the handle kind and the view type.
struct ViewRef {
    kind: HandleKind,
    view: syn::Type,
}

fn classify(path: &syn::Path) -> Option<HandleKind> {
    let segment = path.segments.last()?;
    match segment.ident.to_string().as_str() {
        "Observed" => Some(HandleKind::Observed),
        "Previous" => Some(HandleKind::Previous),
        "Emitted" => Some(HandleKind::Emitted),
        _ => None,
    }
}

/// Extracts the first type argument of `Observed<Syntax>`-style paths.
fn view_type(ty: &Type, name: &str) -> Result<syn::Type, syn::Error> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return Err(syn::Error::new_spanned(
            ty,
            format!("`{name}` arguments must be paths like `Observed<Syntax>`"),
        ));
    };
    let segment = path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(ty, format!("empty path in `{name}` argument")))?;
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            format!("`{name}` argument must name a view type"),
        ));
    };
    for arg in &args.args {
        if let GenericArgument::Type(ty) = arg {
            return Ok(ty.clone());
        }
    }
    Err(syn::Error::new_spanned(
        ty,
        format!("`{name}` argument must name a view type"),
    ))
}

/// The view type written in a signature position. `None` for bare views.
fn bare_view(ty: &Type) -> Option<syn::Type> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    if classify(path).is_some() {
        return None;
    }
    Some(ty.clone())
}

/// Builds the handle type `Observed<V>` / `Previous<V>` for a rewritten
/// body signature.
fn handle_type(kind: &HandleKind, view: &syn::Type) -> proc_macro2::TokenStream {
    match kind {
        HandleKind::Observed => quote! { ::plingo::reactive::Observed<#view> },
        HandleKind::Previous => quote! { ::plingo::reactive::Previous<#view> },
        HandleKind::Emitted => quote! { ::plingo::reactive::Emitted<#view> },
    }
}

/// Parses one function argument into a view reference, applying the bare
/// sugar: `x: V` ≡ `x: Observed<V>`.
fn parse_arg(arg: &FnArg) -> Result<(syn::Ident, ViewRef), syn::Error> {
    let FnArg::Typed(pat_ty) = arg else {
        return Err(syn::Error::new_spanned(
            arg,
            "`self` is not allowed in a component",
        ));
    };
    let Pat::Ident(pat) = &*pat_ty.pat else {
        return Err(syn::Error::new_spanned(
            &pat_ty.pat,
            "component arguments must be simple bindings",
        ));
    };
    let ty = &*pat_ty.ty;
    if let Some(kind) = match ty {
        Type::Path(TypePath { path, .. }) => classify(path),
        _ => None,
    } {
        if matches!(kind, HandleKind::Emitted) {
            return Err(syn::Error::new_spanned(
                ty,
                "components do not accept `Emitted<V>` arguments; emit views in the return tuple",
            ));
        }
        let view = view_type(ty, &pat.ident.to_string())?;
        return Ok((pat.ident.clone(), ViewRef { kind, view }));
    }
    // Bare: the whole type is the view.
    Ok((pat.ident.clone(), ViewRef {
        kind: HandleKind::Observed,
        view: ty.clone(),
    }))
}

/// Parses the return type, returning the (possibly sugar) emitted view
/// types in order. Accepts `Result<(Emitted<A>, ...)>`, `Result<(A, ...)>`,
/// bare `(A, ...)`, or `Result<()>` / `()` (sink).
fn parse_return(ret: &ReturnType) -> Result<Vec<syn::Type>, syn::Error> {
    let ReturnType::Type(_, ty) = ret else {
        return Ok(Vec::new());
    };
    let mut ty = &**ty;
    // Strip one layer of Result<...>.
    if let Type::Path(TypePath { path, .. }) = ty {
        let last = path.segments.last();
        if last.is_some_and(|seg| seg.ident == "Result") {
            if let PathArguments::AngleBracketed(args) = &last.expect("Result").arguments {
                for arg in &args.args {
                    if let GenericArgument::Type(t) = arg {
                        ty = t;
                        break;
                    }
                }
            }
        }
    }
    let Type::Tuple(tuple) = ty else {
        if view_key(ty) == "()" {
            return Ok(Vec::new());
        }
        return Err(syn::Error::new_spanned(
            ty,
            "a component must return a tuple of emitted views, `()`, or their `Result` forms",
        ));
    };
    let mut views = Vec::new();
    for elem in &tuple.elems {
        views.push(match bare_view(elem) {
            Some(view) => view,
            None => view_type(elem, "return")?,
        });
    }
    if views.len() == 1 && view_key(&views[0]) == "()" {
        Ok(Vec::new())
    } else {
        Ok(views)
    }
}

/// A normalized comparison key for one view type (source text).
fn view_key(ty: &syn::Type) -> String {
    use quote::ToTokens;
    let mut tokens = proc_macro2::TokenStream::new();
    ty.to_tokens(&mut tokens);
    tokens.to_string()
}

/// Validates that every view appears at most once, except one
/// `Observed<V>`/`Previous<V>` pair for the same `V`.
fn validate_duplicates(
    args: &[(syn::Ident, ViewRef)],
    returns: &[syn::Type],
) -> Result<(), syn::Error> {
    let mut seen: Vec<(String, HandleKind)> = Vec::new();
    for (this_kind, this_view) in args.iter().map(|(_, v)| (v.kind.clone(), v.view.clone())) {
        let key = view_key(&this_view);
        if let Some((_, existing_kind)) = seen.iter().find(|(have, _)| have == &key) {
            let paired = matches!(
                (existing_kind, &this_kind),
                (HandleKind::Observed, HandleKind::Previous)
                    | (HandleKind::Previous, HandleKind::Observed)
            );
            if !paired {
                return Err(syn::Error::new_spanned(
                    &this_view,
                    format!("duplicate view `{key}` in one component signature"),
                ));
            }
        } else {
            seen.push((key, this_kind));
        }
    }
    for view in returns {
        let key = view_key(view);
        if let Some((_, existing_kind)) = seen.iter().find(|(have, _)| have == &key) {
            // Re-emitting the same view is multi-producer and fine; this
            // is not a duplicate in the signature sense.
            let _ = existing_kind;
        } else {
            seen.push((key, HandleKind::Emitted));
        }
    }
    Ok(())
}

/// `#[component]` on a free function generates the `Component` impl.
#[proc_macro_attribute]
pub fn component(_attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand(item.into()) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand(item: proc_macro2::TokenStream) -> Result<proc_macro2::TokenStream, syn::Error> {
    let mut function: ItemFn = syn::parse2(item)?;
    let name = function.sig.ident.clone();
    // The generated constant keeps the author's name; the function body is
    // renamed (like the legacy macro's `__plingo_component_body_*`).
    let body_name = format_ident!("__reactive_component_body_{name}");
    function.sig.ident = body_name.clone();
    let struct_name = format_ident!("{}", pascal_case(&name.to_string()));
    let visibility = &function.vis;
    let mut args = Vec::new();
    for arg in &function.sig.inputs {
        args.push(parse_arg(arg)?);
    }
    let emitted = parse_return(&function.sig.output)?;
    validate_duplicates(&args, &emitted)?;

    // Rewrite the body's signature: bare args become `Observed<V>`,
    // `Previous<V>` stays, and the return becomes `Result<(Emitted<A>,
    // ...)>` so `Emitted::new()` infers from tuple position.
    {
        // Rewrite the body's signature: bare args become `Observed<V>`,
        // `Previous<V>` stays, and the return becomes `Result<(Emitted<A>,
        // ...)>` so `Emitted::new()` infers from tuple position.
        for (index, arg) in function.sig.inputs.iter_mut().enumerate() {
            let FnArg::Typed(pat_ty) = arg else {
                return Err(syn::Error::new_spanned(
                    arg,
                    "`self` is not allowed in a component",
                ));
            };
            let view = &args[index].1;
            let new_ty = handle_type(&view.kind, &view.view);
            pat_ty.ty = syn::parse2(new_ty).expect("handle type parses");
        }
        if emitted.is_empty() {
            function.sig.output = syn::parse2(quote! {
                -> ::plingo::reactive::Result<()>
            })
            .expect("sink return parses");
        } else {
            let tuple_elems: Vec<proc_macro2::TokenStream> = emitted
                .iter()
                .map(|view| {
                    let handle = handle_type(&HandleKind::Emitted, view);
                    quote! { #handle, }
                })
                .collect();
            function.sig.output = syn::parse2(quote! {
                -> ::plingo::reactive::Result<(#(#tuple_elems)*)>
            })
            .expect("emitted return parses");
        }
    }

    let observed_calls: Vec<proc_macro2::TokenStream> = args
        .iter()
        .filter(|(_, view)| matches!(view.kind, HandleKind::Observed))
        .map(|(ident, view)| {
            let ty = &view.view;
            quote! { let #ident = cx.observed::<#ty>()?; }
        })
        .collect();
    let previous_calls: Vec<proc_macro2::TokenStream> = args
        .iter()
        .filter(|(_, view)| matches!(view.kind, HandleKind::Previous))
        .map(|(ident, view)| {
            let ty = &view.view;
            quote! { let #ident = cx.previous::<#ty>()?; }
        })
        .collect();
    let arg_names: Vec<&syn::Ident> = args.iter().map(|(ident, _)| ident).collect();
    let install_observe: Vec<proc_macro2::TokenStream> = args
        .iter()
        .filter(|(_, view)| matches!(view.kind, HandleKind::Observed))
        .map(|(_, view)| {
            let ty = &view.view;
            quote! { builder.observe::<#ty>()?; }
        })
        .collect();
    let install_previous: Vec<proc_macro2::TokenStream> = args
        .iter()
        .filter(|(_, view)| matches!(view.kind, HandleKind::Previous))
        .map(|(_, view)| {
            let ty = &view.view;
            quote! { builder.previous::<#ty>()?; }
        })
        .collect();
    let install_emit: Vec<proc_macro2::TokenStream> = emitted
        .iter()
        .map(|ty| quote! { builder.emit::<#ty>()?; })
        .collect();

    Ok(quote! {
        #visibility struct #struct_name;

        #[allow(non_upper_case_globals)]
        #visibility const #name: #struct_name = #struct_name;

        impl ::plingo::reactive::Component for #struct_name {
            fn name(&self) -> &'static str {
                stringify!(#name)
            }

            fn install(
                &self,
                builder: &mut ::plingo::reactive::EngineBuilder,
            ) -> ::plingo::reactive::Result<()> {
                #(#install_observe)*
                #(#install_previous)*
                #(#install_emit)*
                Ok(())
            }

            fn run(&self, cx: &::plingo::reactive::RunContext) -> ::plingo::reactive::Result<()> {
                #(#observed_calls)*
                #(#previous_calls)*
                #body_name(
                    #(#arg_names),*
                ).map_err(::plingo::reactive::Error::authored)?;
                Ok(())
            }
        }

        #function
    })
}

fn pascal_case(name: &str) -> String {
    let mut out = String::new();
    let mut upper = true;
    for ch in name.chars() {
        if ch == '_' {
            upper = true;
        } else if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}
// ---------------------------------------------------------------------------
// `#[view]` — declared view types
// ---------------------------------------------------------------------------

/// The parsed `#[view(...)]` attribute content (see the module docs of the
/// `view` macro for the accepted grammar).
pub(crate) struct ViewArgs {
    shape: Option<ViewShape>,
    map_key: Option<syn::Type>,
    value: Option<syn::Type>,
    edge: Option<syn::Type>,
    label: Option<syn::Type>,
}

/// Shape choice parsed from the attribute.
#[derive(Clone, Copy)]
pub(crate) enum ViewShape {
    Box,
    Map,
    Tree,
    Graph,
}

impl ViewArgs {
    fn default() -> ViewArgs {
        ViewArgs {
            shape: None,
            map_key: None,
            value: None,
            edge: None,
            label: None,
        }
    }

    fn parse(&mut self, meta: syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
        if meta.path.is_ident("box")
            || meta.path.is_ident("map")
            || meta.path.is_ident("tree")
            || meta.path.is_ident("graph")
        {
            if self.shape.is_some() {
                return Err(meta.error("duplicate shape"));
            }
            let shape = if meta.path.is_ident("box") {
                ViewShape::Box
            } else if meta.path.is_ident("map") {
                ViewShape::Map
            } else if meta.path.is_ident("tree") {
                ViewShape::Tree
            } else {
                ViewShape::Graph
            };
            self.shape = Some(shape);
            Ok(())
        } else if meta.path.is_ident("key") {
            let ty: syn::Type = meta.value()?.parse()?;
            self.map_key = Some(ty);
            Ok(())
        } else if meta.path.is_ident("value") {
            let ty: syn::Type = meta.value()?.parse()?;
            self.value = Some(ty);
            Ok(())
        } else if meta.path.is_ident("edge") {
            let ty: syn::Type = meta.value()?.parse()?;
            self.edge = Some(ty);
            Ok(())
        } else if meta.path.is_ident("label") {
            let ty: syn::Type = meta.value()?.parse()?;
            self.label = Some(ty);
            Ok(())
        } else {
            Err(meta.error("unsupported view property"))
        }
    }
}

/// The shape guards for the keyed views: `key` only on maps, `edge`/`label`
/// only on graphs, and `value` always required.
fn validate_view_shape(args: &ViewArgs) -> syn::Result<()> {
    if args.value.is_none() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "a #[view] requires `value = <type>`",
        ));
    }
    let shape = args.shape.expect("view shape");
    match shape {
        ViewShape::Map => Ok(()),
        ViewShape::Graph => {
            if args.edge.is_none() {
                return Err(syn::Error::new(
                    proc_macro2::Span::call_site(),
                    "a graph view requires `edge = <type>`",
                ));
            }
            if args.label.is_none() {
                return Err(syn::Error::new(
                    proc_macro2::Span::call_site(),
                    "a graph view requires `label = <type>`",
                ));
            }
            Ok(())
        }
        _ => {
            if args.map_key.is_some() {
                return Err(syn::Error::new(
                    proc_macro2::Span::call_site(),
                    "a non-map view cannot declare `key = ...`",
                ));
            }
            if args.edge.is_some() || args.label.is_some() {
                return Err(syn::Error::new(
                    proc_macro2::Span::call_site(),
                    "only graph views declare `edge` and `label`",
                ));
            }
            Ok(())
        }
    }
}

/// `#[view(...)]` on a unit struct declares a reactive view type.
#[proc_macro_attribute]
pub fn view(args: TokenStream, item: TokenStream) -> TokenStream {
    let mut parsed = ViewArgs::default();
    let parser = syn::meta::parser(|meta| parsed.parse(meta));
    let parsed_args = match syn::parse::Parser::parse2(parser, args.into()) {
        Ok(()) => parsed,
        Err(error) => return error.to_compile_error().into(),
    };
    if let Err(error) = validate_view_shape(&parsed_args) {
        return error.to_compile_error().into();
    }
    match expand_view(item.into(), &parsed_args) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand_view(
    item: proc_macro2::TokenStream,
    args: &ViewArgs,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut struct_item: syn::ItemStruct = syn::parse2(item)?;
    // Strip the #[view(...)] attribute before re-emitting.
    struct_item.attrs = struct_item
        .attrs
        .iter()
        .filter(|attr| !attr.path().is_ident("view"))
        .cloned()
        .collect();
    let view_spec_tokens = build_view_spec(&struct_item, args)?;
    Ok(quote! {
        #struct_item
        #view_spec_tokens
    })
}

fn build_view_spec(
    item: &syn::ItemStruct,
    args: &ViewArgs,
) -> syn::Result<proc_macro2::TokenStream> {
    let shape = args.shape.expect("view shape");
    let (shape_ty, key, edge, label) = match shape {
        ViewShape::Box => {
            let shape_ty = quote! { ::plingo::reactive::view::BoxShape };
            (shape_ty, quote! { () }, quote! { () }, quote! { () })
        }
        ViewShape::Map => {
            let shape_ty = quote! { ::plingo::reactive::view::MapShape };
            let key = args.map_key.as_ref().expect("map key type");
            (shape_ty, quote! { #key }, quote! { () }, quote! { () })
        }
        ViewShape::Tree => {
            let shape_ty = quote! { ::plingo::reactive::view::TreeShape };
            (shape_ty, quote! { () }, quote! { () }, quote! { () })
        }
        ViewShape::Graph => {
            let shape_ty = quote! { ::plingo::reactive::view::GraphShape };
            let key = args.label.as_ref().expect("graph label type");
            let edge = args.edge.as_ref().expect("graph edge type");
            let label = args.label.as_ref().expect("graph label type");
            (shape_ty, quote! { #key }, quote! { #edge }, quote! { #label })
        }
    };
    let value = args.value.as_ref().expect("view value type");
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();
    let name = &item.ident;
    let view_name = name.to_string();
    Ok(quote! {
        impl #impl_generics ::plingo::reactive::view::ViewSpec for #name #ty_generics #where_clause {
            type Shape = #shape_ty;
            type Key = #key;
            type Value = #value;
            type Edge = #edge;
            type Label = #label;

            fn view_name() -> &'static str {
                #view_name
            }
        }
    })
}

// ---------------------------------------------------------------------------
// `#[abstract_tree]` — family syntax trees
// ---------------------------------------------------------------------------

/// `#[abstract_tree(members(A, B, ...))]` on every family member enum; the
/// root's expansion generates the shared view and unions (see
/// [`abstract_tree` module docs]).
///
/// [`abstract_tree` module docs]: abstract_tree
#[proc_macro_attribute]
pub fn abstract_tree(args: TokenStream, item: TokenStream) -> TokenStream {
    let mut parsed = abstract_tree::AbstractTreeArgs { members: None };
    let parser = syn::meta::parser(|meta| parsed.parse(meta));
    let parsed_args = match syn::parse::Parser::parse2(parser, args.into()) {
        Ok(()) => parsed,
        Err(error) => return error.to_compile_error().into(),
    };
    match abstract_tree::expand(&parsed_args, item.into()) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}
