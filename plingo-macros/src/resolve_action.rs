use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemImpl, parse_macro_input, spanned::Spanned};

pub fn expand_resolve_impl(item: TokenStream) -> TokenStream {
    let item_impl = parse_macro_input!(item as ItemImpl);
    let (impl_generics, _ty_generics, where_clause) = item_impl.generics.split_for_impl();

    let Some((_, trait_path, _)) = &item_impl.trait_ else {
        return syn::Error::new(
            item_impl.self_ty.span(),
            "resolve action attributes can only be used on trait impl blocks",
        )
        .to_compile_error()
        .into();
    };

    let Some(last_segment) = trait_path.segments.last() else {
        return syn::Error::new(trait_path.span(), "expected a trait path ending in Resolve")
            .to_compile_error()
            .into();
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
    let receiver_output = quote!(<#self_type as ::plingo::scheme::Resolve<#action_type>>::Output);

    quote! {
        #item_impl
        impl #impl_generics ::plingo::marker::Receiver<#action_type> for #self_type #where_clause {
            type _Output = #receiver_output;
        }
    }
    .into()
}

fn extract_action_type(segment: &syn::PathSegment) -> syn::Result<syn::Type> {
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(syn::Error::new(
            segment.arguments.span(),
            "expected Resolve<YourAction>",
        ));
    };

    let Some(first_arg) = args.args.first() else {
        return Err(syn::Error::new(args.span(), "expected one action type argument"));
    };

    let syn::GenericArgument::Type(action_type) = first_arg else {
        return Err(syn::Error::new(
            first_arg.span(),
            "expected a concrete action type argument",
        ));
    };

    Ok(action_type.clone())
}
