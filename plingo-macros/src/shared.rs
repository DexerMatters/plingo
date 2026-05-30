use quote::{format_ident, quote};
use syn::{Expr, Field, Meta, Type, spanned::Spanned};

pub struct VariantConfig {
    pub regex: syn::LitStr,
    pub enter: Option<Type>,
    pub leave: bool,
    pub skip: bool,
    pub validate: Option<syn::Expr>,
}

pub fn parse_variant_config(variant: &syn::Variant) -> syn::Result<VariantConfig> {
    let mut regex = None;
    let mut enter = None;
    let mut leave = false;
    let mut skip = false;
    let mut validate = None;

    for attr in &variant.attrs {
        match &attr.meta {
            Meta::List(meta) if meta.path.is_ident("regex") => {
                if regex.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "duplicate #[regex(...)] attribute",
                    ));
                }
                regex = Some(attr.parse_args::<syn::LitStr>()?);
            }
            Meta::List(meta) if meta.path.is_ident("enter") => {
                if enter.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "duplicate #[enter(...)] attribute",
                    ));
                }
                enter = Some(attr.parse_args::<Type>()?);
            }
            Meta::Path(path) if path.is_ident("leave") => {
                if leave {
                    return Err(syn::Error::new(attr.span(), "duplicate #[leave] attribute"));
                }
                leave = true;
            }
            Meta::List(meta) if meta.path.is_ident("validate") => {
                if validate.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "duplicate #[validate(...)] attribute",
                    ));
                }
                validate = Some(attr.parse_args::<syn::Expr>()?);
            }
            Meta::Path(path) if path.is_ident("skip") => {
                if skip {
                    return Err(syn::Error::new(attr.span(), "duplicate #[skip] attribute"));
                }
                skip = true;
            }
            _ => {}
        }
    }

    let Some(regex) = regex else {
        return Err(syn::Error::new(
            variant.span(),
            "each token variant requires #[regex(...)]",
        ));
    };

    if enter.is_some() && leave {
        return Err(syn::Error::new(
            variant.span(),
            "#[enter(...)] and #[leave] cannot be used on the same variant",
        ));
    }

    Ok(VariantConfig {
        regex,
        enter,
        leave,
        skip,
        validate,
    })
}

pub fn ensure_no_field_parse_attrs(variant: &syn::Variant) -> syn::Result<()> {
    for field in variant.fields.iter() {
        if field.attrs.iter().any(|attr| attr.path().is_ident("parse")) {
            return Err(syn::Error::new(
                field.span(),
                "unit variants cannot use #[parse(...)]",
            ));
        }
    }
    Ok(())
}

pub fn field_parse_expr(field: &Field, token_name: &str) -> syn::Result<proc_macro2::TokenStream> {
    let field_ty = &field.ty;
    let mut parse_expr = None;

    for attr in &field.attrs {
        if !attr.path().is_ident("parse") {
            continue;
        }
        if parse_expr.is_some() {
            return Err(syn::Error::new(
                attr.span(),
                "duplicate #[parse(...)] attribute",
            ));
        }

        let expr = attr.parse_args::<Expr>()?;
        parse_expr = Some(quote! {
            ::plingo::component::lex::__macro_private::IntoLexemeResult::into_lexeme_result((#expr)(lexeme))
                .map_err(|source| ::plingo::component::lex::LexInterrupt::token_parse_failed(#token_name, lexeme, source))?
        });
    }

    Ok(match parse_expr {
        Some(expr) => expr,
        None => quote! {
            <#field_ty as ::plingo::component::lex::FromLexeme>::from_lexeme(lexeme)
                .map_err(|source| ::plingo::component::lex::LexInterrupt::token_parse_failed(#token_name, lexeme, source))?
        },
    })
}

pub fn target_last_ident(target: &Type) -> syn::Result<syn::Ident> {
    let Type::Path(type_path) = target else {
        return Err(syn::Error::new(
            target.span(),
            "enter target must be a concrete type path",
        ));
    };
    type_path
        .path
        .segments
        .last()
        .map(|segment| segment.ident.clone())
        .ok_or_else(|| syn::Error::new(target.span(), "enter target path cannot be empty"))
}

pub fn push_missing_derives(
    item: &mut syn::ItemEnum,
    required: &[&str],
) -> syn::Result<()> {
    let mut derive_index = None;
    let mut derives = Vec::<syn::Ident>::new();

    for (index, attr) in item.attrs.iter().enumerate() {
        let Meta::List(meta) = &attr.meta else {
            continue;
        };
        if !meta.path.is_ident("derive") {
            continue;
        }
        derive_index = Some(index);
        attr.parse_nested_meta(|nested| {
            if let Some(ident) = nested.path.get_ident() {
                derives.push(ident.clone());
            }
            Ok(())
        })?;
    }

    for required in required {
        if !derives.iter().any(|ident| ident == required) {
            derives.push(format_ident!("{}", required));
        }
    }

    if let Some(index) = derive_index {
        item.attrs[index] = syn::parse_quote!(#[derive(#(#derives),*)]);
    } else {
        item.attrs.push(syn::parse_quote!(#[derive(#(#derives),*)]));
    }

    Ok(())
}
