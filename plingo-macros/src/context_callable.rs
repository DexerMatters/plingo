use proc_macro::TokenStream;
use quote::quote;
use syn::{
    FnArg, GenericArgument, ImplItemFn, ItemFn, Lifetime, PatType, PathArguments, ReturnType,
    TraitItemFn, Type, TypePath, parse2, spanned::Spanned,
};

pub fn expand_context_callable(item: TokenStream) -> TokenStream {
    let item_ts = proc_macro2::TokenStream::from(item);

    let mut method = match parse2::<ImplItemFn>(item_ts.clone()) {
        Ok(method) => method,
        Err(_) => {
            if parse2::<TraitItemFn>(item_ts.clone()).is_ok() || parse2::<ItemFn>(item_ts).is_ok() {
                return syn::Error::new(
                    proc_macro2::Span::call_site(),
                    "#[context_callable] requires an inherent async method",
                )
                .to_compile_error()
                .into();
            }
            return syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[context_callable] requires an inherent async method",
            )
            .to_compile_error()
            .into();
        }
    };

    if method.sig.asyncness.is_none() {
        return syn::Error::new(
            method.sig.fn_token.span(),
            "#[context_callable] requires an inherent async method",
        )
        .to_compile_error()
        .into();
    }

    let lifetime = match extract_signature_lifetime(&method) {
        Ok(lifetime) => lifetime,
        Err(err) => return err.to_compile_error().into(),
    };

    let output = match extract_call_outcome_output(&method.sig.output) {
        Ok(output) => output,
        Err(err) => return err.to_compile_error().into(),
    };

    method.sig.asyncness = None;
    method.sig.output =
        syn::parse_quote!(-> ::plingo::scheme::call::LayerCallFuture<#lifetime, Self, #output>);

    let block = method.block;
    method.block = syn::parse_quote!({
        ::std::boxed::Box::pin(async move #block)
    });

    quote!(#method).into()
}

fn extract_signature_lifetime(method: &ImplItemFn) -> syn::Result<Lifetime> {
    if method.sig.inputs.len() != 3 {
        return Err(syn::Error::new(
            method.sig.inputs.span(),
            "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
        ));
    }

    let mut inputs = method.sig.inputs.iter();
    let receiver = inputs.next().unwrap();
    let ctx = inputs.next().unwrap();
    let arg = inputs.next().unwrap();

    let receiver_lifetime = match receiver {
        FnArg::Receiver(receiver) => {
            if !receiver.mutability.is_some() {
                return Err(syn::Error::new(
                    receiver.span(),
                    "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
                ));
            }
            let Some((_, Some(lifetime))) = &receiver.reference else {
                return Err(syn::Error::new(
                    receiver.span(),
                    "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
                ));
            };
            lifetime.clone()
        }
        FnArg::Typed(arg) => {
            return Err(syn::Error::new(
                arg.span(),
                "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
            ));
        }
    };

    assert_context_arg(ctx, &receiver_lifetime)?;
    assert_payload_arg(arg, &receiver_lifetime)?;

    Ok(receiver_lifetime)
}

fn assert_context_arg(arg: &FnArg, lifetime: &Lifetime) -> syn::Result<()> {
    let FnArg::Typed(arg) = arg else {
        return Err(syn::Error::new(
            arg.span(),
            "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
        ));
    };
    let reference = match arg.ty.as_ref() {
        Type::Reference(reference) => reference,
        _ => {
            return Err(syn::Error::new(
                arg.ty.span(),
                "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
            ));
        }
    };
    let Some(arg_lifetime) = &reference.lifetime else {
        return Err(syn::Error::new(
            arg.ty.span(),
            "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
        ));
    };
    if arg_lifetime.ident != lifetime.ident || reference.mutability.is_some() {
        return Err(syn::Error::new(
            arg.ty.span(),
            "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
        ));
    }
    if !is_named_type(reference.elem.as_ref(), "Context") {
        return Err(syn::Error::new(
            arg.ty.span(),
            "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
        ));
    }
    Ok(())
}

fn assert_payload_arg(arg: &FnArg, lifetime: &Lifetime) -> syn::Result<()> {
    let FnArg::Typed(PatType { ty, .. }) = arg else {
        return Err(syn::Error::new(
            arg.span(),
            "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
        ));
    };
    let reference = match ty.as_ref() {
        Type::Reference(reference) => reference,
        _ => {
            return Err(syn::Error::new(
                ty.span(),
                "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
            ));
        }
    };
    let Some(arg_lifetime) = &reference.lifetime else {
        return Err(syn::Error::new(
            ty.span(),
            "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
        ));
    };
    if arg_lifetime.ident != lifetime.ident {
        return Err(syn::Error::new(
            ty.span(),
            "#[context_callable] requires signature `(&'a mut self, &'a Context, &'a Args)`",
        ));
    }
    Ok(())
}

fn extract_call_outcome_output(output: &ReturnType) -> syn::Result<Type> {
    let ReturnType::Type(_, ty) = output else {
        return Err(syn::Error::new(
            output.span(),
            "#[context_callable] requires return type `CallOutcome<_, O>`",
        ));
    };
    let Type::Path(TypePath { path, .. }) = ty.as_ref() else {
        return Err(syn::Error::new(
            ty.span(),
            "#[context_callable] requires return type `CallOutcome<_, O>`",
        ));
    };
    let Some(last) = path.segments.last() else {
        return Err(syn::Error::new(
            ty.span(),
            "#[context_callable] requires return type `CallOutcome<_, O>`",
        ));
    };
    if last.ident != "CallOutcome" {
        return Err(syn::Error::new(
            ty.span(),
            "#[context_callable] requires return type `CallOutcome<_, O>`",
        ));
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return Err(syn::Error::new(
            ty.span(),
            "#[context_callable] requires return type `CallOutcome<_, O>`",
        ));
    };
    if args.args.len() != 2 {
        return Err(syn::Error::new(
            ty.span(),
            "#[context_callable] requires return type `CallOutcome<_, O>`",
        ));
    }
    let output = args.args.iter().nth(1).unwrap();
    let GenericArgument::Type(output) = output else {
        return Err(syn::Error::new(
            output.span(),
            "#[context_callable] requires return type `CallOutcome<_, O>`",
        ));
    };
    Ok(output.clone())
}

fn is_named_type(ty: &Type, expected: &str) -> bool {
    match ty {
        Type::Path(TypePath { path, .. }) => path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == expected),
        _ => false,
    }
}
