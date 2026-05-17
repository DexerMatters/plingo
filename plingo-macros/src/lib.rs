use proc_macro::TokenStream;
use quote::quote;
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input,
    spanned::Spanned,
    ItemImpl, Type,
};

// ---------------------------------------------------------------------------
// Layer attribute: #[layer(top)] | #[layer(middle)] | #[layer(bottom)]
// ---------------------------------------------------------------------------

enum LayerRole {
    Top,
    Middle,
    Bottom,
}

impl Parse for LayerRole {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let ident: syn::Ident = input.parse()?;
        match ident.to_string().as_str() {
            "top" => Ok(LayerRole::Top),
            "middle" => Ok(LayerRole::Middle),
            "bottom" => Ok(LayerRole::Bottom),
            other => Err(syn::Error::new(
                ident.span(),
                format!("expected `top`, `middle`, or `bottom`, found `{other}`"),
            )),
        }
    }
}

#[proc_macro_attribute]
pub fn layer(attr: TokenStream, item: TokenStream) -> TokenStream {
    let role = parse_macro_input!(attr as LayerRole);
    let item_impl = parse_macro_input!(item as ItemImpl);

    let self_type = match item_impl.self_ty.as_ref() {
        Type::Path(path) => &path.path,
        _ => {
            return syn::Error::new(
                item_impl.self_ty.span(),
                "#[layer] requires a struct or type name as the impl target",
            )
            .to_compile_error()
            .into();
        }
    };

    let (impl_generics, _ty_generics, where_clause) = item_impl.generics.split_for_impl();

    // Validate that the trait matches the role.
    if let Some((_, trait_path, _)) = &item_impl.trait_ {
        let expected_trait = match role {
            LayerRole::Top => "TopLayer",
            LayerRole::Middle => "MiddleLayer",
            LayerRole::Bottom => "BottomLayer",
        };
        if let Some(seg) = trait_path.segments.last() {
            if seg.ident != expected_trait {
                return syn::Error::new(
                    seg.ident.span(),
                    format!("#[layer({})] requires impl of {expected_trait}", {
                        match role {
                            LayerRole::Top => "top",
                            LayerRole::Middle => "middle",
                            LayerRole::Bottom => "bottom",
                        }
                    }),
                )
                .to_compile_error()
                .into();
            }
        }
    } else {
        return syn::Error::new(
            item_impl.self_ty.span(),
            "#[layer] can only be used on trait impl blocks",
        )
        .to_compile_error()
        .into();
    }

    let conduit_impls = match role {
        LayerRole::Top => quote! {
            impl #impl_generics ::plingo::scheme::FallibleLayer for #self_type #where_clause {
                type __Error = <Self as ::plingo::scheme::TopLayer>::Error;
            }
        },
        LayerRole::Middle => quote! {
            impl #impl_generics ::plingo::scheme::FallibleLayer for #self_type #where_clause {
                type __Error = <Self as ::plingo::scheme::MiddleLayer>::Error;
            }

            impl #impl_generics ::plingo::scheme::NonTopLayer for #self_type #where_clause {
                type _Key = <Self as ::plingo::scheme::MiddleLayer>::Key;
                type _Error = <Self as ::plingo::scheme::MiddleLayer>::Error;
            }
        },
        LayerRole::Bottom => quote! {
            impl #impl_generics ::plingo::scheme::FallibleLayer for #self_type #where_clause {
                type __Error = <Self as ::plingo::scheme::BottomLayer>::Error;
            }

            impl #impl_generics ::plingo::scheme::NonTopLayer for #self_type #where_clause {
                type _Key = <Self as ::plingo::scheme::BottomLayer>::Key;
                type _Error = <Self as ::plingo::scheme::BottomLayer>::Error;
            }
        },
    };

    // Default (no-op) dynamic-dispatch — generic `#[resolve_action]`
    // will provide a real implementation if one is needed.
    let dyn_dispatch_impl = quote! {
        impl #impl_generics ::plingo::scheme::__PlingoDynamicDispatch for #self_type #where_clause {}
    };

    quote! {
        #item_impl

        #conduit_impls

        #dyn_dispatch_impl
    }
    .into()
}

// ---------------------------------------------------------------------------
// Resolve action attr: #[resolve_action]
// ---------------------------------------------------------------------------

#[proc_macro_attribute]
pub fn resolve_action(_attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_resolve_impl(item)
}

fn expand_resolve_impl(item: TokenStream) -> TokenStream {
    let item_impl = parse_macro_input!(item as ItemImpl);

    let is_generic = !item_impl.generics.params.is_empty();

    let (impl_generics, _ty_generics, where_clause) = item_impl.generics.split_for_impl();

    let Some((_, trait_path, _)) = &item_impl.trait_ else {
        return syn::Error::new(
            item_impl.self_ty.span(),
            "resolve action attributes can only be used on trait impl blocks",
        )
        .to_compile_error()
        .into();
    };

    let last_segment = match trait_path.segments.last() {
        Some(segment) => segment,
        None => {
            return syn::Error::new(trait_path.span(), "expected a trait path ending in Resolve")
                .to_compile_error()
                .into();
        }
    };

    if last_segment.ident != "Resolve" {
        return syn::Error::new(
            last_segment.ident.span(),
            "expected trait Resolve<...> for #[resolve_action]",
        )
        .to_compile_error()
        .into();
    }

    let action_type = match extract_action_type(last_segment) {
        Ok(action_type) => action_type,
        Err(err) => return err.to_compile_error().into(),
    };

    let self_type = item_impl.self_ty.clone();

    let call_resolve = quote! {
        <#self_type as ::plingo::scheme::Resolve<#action_type>>::resolve(layer, ctx, action).await
    };

    let receiver_output = quote!(<#self_type as ::plingo::scheme::Resolve<#action_type>>::Output);

    let map_outcome = quote! {
        ::plingo::scheme::__macro_private::into_registered_dispatch_outcome(typed)
    };

    let dispatch_fn_name = quote::format_ident!("__plingo_dispatch");

    let dispatch_body = quote! {
        let Some(action) = action.downcast_ref::<#action_type>() else {
            unreachable!(
                "resolve action registration type mismatch: layer={}, action={}",
                ::std::any::type_name::<#self_type>(),
                ::std::any::type_name::<#action_type>(),
            );
        };
        ::std::boxed::Box::pin(async move {
            let typed = #call_resolve;
            #map_outcome
        })
    };

    if is_generic {
        // Generic path: emit a specific `__PlingoDynamicDispatch` impl that
        // overrides the default no-op provided by `#[layer]`.
        quote! {
            #item_impl

            impl #impl_generics ::plingo::marker::Receiver<#action_type> for #self_type #where_clause {
                type _Output = #receiver_output;
            }

            impl #impl_generics ::plingo::scheme::__PlingoDynamicDispatch for #self_type #where_clause {
                fn __plingo_try_dispatch<'a>(
                    &'a self,
                    ctx: &'a ::plingo::scheme::Context,
                    action: &'a (dyn ::std::any::Any + Send + Sync),
                ) -> ::std::option::Option<
                    ::plingo::scheme::__macro_private::DispatchFuture<'a>,
                > {
                    let Some(action) = action.downcast_ref::<#action_type>() else {
                        return ::std::option::Option::None;
                    };
                    let fut = <#self_type as ::plingo::scheme::Resolve<#action_type>>::resolve(
                        self, ctx, action,
                    );
                    ::std::option::Option::Some(::std::boxed::Box::pin(async move {
                        ::plingo::scheme::__macro_private::into_registered_dispatch_outcome(fut.await)
                    }))
                }
            }
        }
        .into()
    } else {
        let entry_type = quote!(::plingo::scheme::__macro_private::ResolveActionEntry);

        quote! {
            #item_impl

            impl ::plingo::marker::Receiver<#action_type> for #self_type {
                type _Output = #receiver_output;
            }

            const _: () = {
                fn #dispatch_fn_name<'a>(
                    layer: &'a (dyn ::std::any::Any + Send + Sync),
                    ctx: &'a ::plingo::scheme::Context,
                    action: &'a (dyn ::std::any::Any + Send + Sync),
                ) -> ::std::pin::Pin<
                    ::std::boxed::Box<
                        dyn ::std::future::Future<
                            Output = ::plingo::scheme::__macro_private::RegisteredDispatchOutcome,
                        > + Send + 'a,
                    >,
                > {
                    let Some(layer) = layer.downcast_ref::<#self_type>() else {
                        unreachable!(
                            "resolve action layer type mismatch: layer={}, action={}",
                            ::std::any::type_name::<#self_type>(),
                            ::std::any::type_name::<#action_type>(),
                        );
                    };

                    #dispatch_body
                }

                ::inventory::submit! {
                    #entry_type::new(
                        ::std::any::TypeId::of::<#self_type>(),
                        ::std::any::TypeId::of::<#action_type>(),
                        #dispatch_fn_name,
                    )
                }
            };
        }
        .into()
    }
}

fn extract_action_type(segment: &syn::PathSegment) -> syn::Result<syn::Type> {
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(syn::Error::new(
            segment.arguments.span(),
            "expected Resolve<YourAction>",
        ));
    };

    let Some(first_arg) = args.args.first() else {
        return Err(syn::Error::new(
            args.span(),
            "expected one action type argument",
        ));
    };

    let syn::GenericArgument::Type(action_type) = first_arg else {
        return Err(syn::Error::new(
            first_arg.span(),
            "expected a concrete action type argument",
        ));
    };

    Ok(action_type.clone())
}
