use proc_macro2::TokenStream;
use quote::quote;
use syn::{DeriveInput, Ident, Result, Type, parse_quote};

pub fn expand_elaborator_role(input: DeriveInput) -> Result<TokenStream> {
    let role = input.ident;
    let mut domain = None;
    let mut input_ty = None;
    let mut output = None;
    let mut diagnostic = None;
    let mut access = None;

    for attribute in input.attrs {
        if !attribute.path().is_ident("elaborator") {
            continue;
        }
        attribute.parse_nested_meta(|meta| {
            let name = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("expected an elaborator field name"))?;
            let name = name.to_string();
            match name.as_str() {
                "domain" => {
                    if domain.is_some() {
                        return Err(meta.error("duplicate `domain`"));
                    }
                    domain = Some(meta.value()?.parse::<Type>()?);
                }
                "input" => {
                    if input_ty.is_some() {
                        return Err(meta.error("duplicate `input`"));
                    }
                    input_ty = Some(meta.value()?.parse::<Type>()?);
                }
                "output" => {
                    if output.is_some() {
                        return Err(meta.error("duplicate `output`"));
                    }
                    output = Some(meta.value()?.parse::<Type>()?);
                }
                "diagnostic" => {
                    if diagnostic.is_some() {
                        return Err(meta.error("duplicate `diagnostic`"));
                    }
                    diagnostic = Some(meta.value()?.parse::<Type>()?);
                }
                "access" => {
                    if access.is_some() {
                        return Err(meta.error("duplicate `access`"));
                    }
                    access = Some(meta.value()?.parse::<Ident>()?);
                }
                _ => return Err(meta.error("unknown elaborator field")),
            }
            Ok(())
        })?;
    }

    let domain = domain.ok_or_else(|| syn::Error::new_spanned(&role, "missing `domain`"))?;
    let input_ty: Type = input_ty.unwrap_or_else(|| parse_quote!(()));
    let output: Type = output.unwrap_or_else(|| parse_quote!(()));
    let diagnostic: Type =
        diagnostic.unwrap_or_else(|| parse_quote!(::plingo::component::semantic::NoDiagnostic));
    let access = access.unwrap_or_else(|| Ident::new("Build", role.span()));
    if !matches!(access.to_string().as_str(), "Build" | "Extend" | "Query") {
        return Err(syn::Error::new_spanned(
            access,
            "expected `Build`, `Extend`, or `Query`",
        ));
    }

    Ok(quote! {
        impl ::plingo::component::semantic::ElaboratorRole for #role {
            type Domain = #domain;
            type Input = #input_ty;
            type Output = #output;
            type Diagnostic = #diagnostic;
            const SCOPE_ACCESS: ::plingo::component::semantic::ScopeAccess =
                ::plingo::component::semantic::ScopeAccess::#access;
        }
    })
}
