use proc_macro2::TokenStream;
use quote::quote;
use syn::{DeriveInput, Result, Type};

pub fn expand_scope_domain(input: DeriveInput) -> Result<TokenStream> {
    let domain = input.ident;
    let mut scope_key = None;
    let mut scope_data = None;
    let mut label = None;
    let mut request = None;

    for attribute in input.attrs {
        if !attribute.path().is_ident("scope_domain") {
            continue;
        }
        attribute.parse_nested_meta(|meta| {
            let name = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("expected a scope-domain field name"))?;
            let value: Type = meta.value()?.parse()?;
            match name.to_string().as_str() {
                "scope_key" => scope_key = Some(value),
                "scope_data" => scope_data = Some(value),
                "label" => label = Some(value),
                "request" => request = Some(value),
                _ => return Err(meta.error("unknown scope-domain field")),
            }
            Ok(())
        })?;
    }

    let required = |value: Option<Type>, name: &str| {
        value.ok_or_else(|| syn::Error::new_spanned(&domain, format!("missing `{name}`")))
    };
    let scope_key = required(scope_key, "scope_key")?;
    let scope_data = required(scope_data, "scope_data")?;
    let label = required(label, "label")?;
    let request = required(request, "request")?;

    Ok(quote! {
        impl ::plingo::component::scope::ScopeDomain for #domain {
            type ScopeKey = #scope_key;
            type ScopeData = #scope_data;
            type Label = #label;
            type Request = #request;
        }
    })
}
