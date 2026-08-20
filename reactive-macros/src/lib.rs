//! `#[component]` — the reactive authoring macro (plan §7 Phase 2).
//!
//! Generates, from an ordinary function whose signature is expressed in
//! view handles, the component's `Component` impl: signature edges
//! (observed/previous/emitted views), store registration, and the root
//! run that executes the author's body. The generated code references
//! only `reactive` types; no legacy runtime type appears.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, GenericArgument, ItemFn, Pat, PathArguments, ReturnType, Type, TypePath};

/// The view handle kinds recognized in the signature.
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

/// Parses one function argument into a view reference.
fn parse_arg(arg: &FnArg) -> Result<(syn::Ident, ViewRef), syn::Error> {
    let FnArg::Typed(pat_ty) = arg else {
        return Err(syn::Error::new_spanned(arg, "`self` is not allowed in a component"));
    };
    let Pat::Ident(pat) = &*pat_ty.pat else {
        return Err(syn::Error::new_spanned(
            &pat_ty.pat,
            "component arguments must be simple bindings",
        ));
    };
    let Type::Path(TypePath { path, .. }) = &*pat_ty.ty else {
        return Err(syn::Error::new_spanned(
            &pat_ty.ty,
            "component arguments must be view handles (`Observed<V>`, `Previous<V>`, `Emitted<V>`)",
        ));
    };
    let kind = classify(path).ok_or_else(|| {
        syn::Error::new_spanned(
            path,
            "component arguments must be view handles (`Observed<V>`, `Previous<V>`, `Emitted<V>`)",
        )
    })?;
    let view = view_type(&pat_ty.ty, &pat.ident.to_string())?;
    Ok((pat.ident.clone(), ViewRef { kind, view }))
}

/// Parses the return type `Result<(Emitted<A>, Emitted<B>, ...)>` into
/// the emitted views, in order.
fn parse_return(ret: &ReturnType) -> Result<Vec<syn::Type>, syn::Error> {
    let ReturnType::Type(_, ty) = ret else {
        return Err(syn::Error::new_spanned(
            ret,
            "a component must return `Result<(Emitted<...>, ...)>`",
        ));
    };
    let Type::Path(TypePath { path, .. }) = &**ty else {
        return Err(syn::Error::new_spanned(
            ty,
            "a component must return `Result<(Emitted<...>, ...)>`",
        ));
    };
    let segment = path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(ty, "empty return path"))?;
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            "a component must return `Result<(Emitted<...>, ...)>`",
        ));
    };
    for arg in &args.args {
        if let GenericArgument::Type(inner) = arg {
            // `Result<T>` with a one-arg alias: T is the tuple.
            let Type::Tuple(tuple) = inner else {
                return Err(syn::Error::new_spanned(
                    inner,
                    "a component must return `Result<(Emitted<...>, ...)>`",
                ));
            };
            let mut views = Vec::new();
            for elem in &tuple.elems {
                let view = view_type(elem, "return")?;
                views.push(view);
            }
            return Ok(views);
        }
    }
    Err(syn::Error::new_spanned(
        ty,
        "a component must return `Result<(Emitted<...>, ...)>`",
    ))
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
    let emitted_idents: Vec<syn::Ident> = (0..emitted.len())
        .map(|index| format_ident!("_emitted_{}", index))
        .collect();
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
                let (#(#emitted_idents),*) = #body_name(
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
