use quote::{format_ident, quote};
use syn::{Expr, Field, Meta, Type, spanned::Spanned};

pub enum MatcherConfig {
    Regex(syn::LitStr),
    From(Type),
}

pub struct VariantConfig {
    pub matcher: Option<MatcherConfig>,
    pub then_require: Option<syn::Ident>,
    pub till: Option<syn::Ident>,
    pub skip: bool,
    pub validate: Option<syn::Expr>,
    pub error: bool,
}

pub fn parse_variant_config(variant: &syn::Variant) -> syn::Result<VariantConfig> {
    let mut regex = None;
    let mut from = None;
    let mut then_require = None;
    let mut till = None;
    let mut skip = false;
    let mut validate = None;
    let mut error = false;

    for attr in &variant.attrs {
        match &attr.meta {
            Meta::List(meta) if meta.path.is_ident("regex") => {
                if regex.is_some() || from.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "token variants require exactly one matcher attribute",
                    ));
                }
                regex = Some(attr.parse_args::<syn::LitStr>()?);
            }
            Meta::List(meta) if meta.path.is_ident("from") => {
                if regex.is_some() || from.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "token variants require exactly one matcher attribute",
                    ));
                }
                from = Some(attr.parse_args::<Type>()?);
            }
            Meta::List(meta) if meta.path.is_ident("then_require") => {
                if then_require.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "duplicate #[then_require(...)] attribute",
                    ));
                }
                then_require = Some(attr.parse_args::<syn::Ident>()?);
            }
            Meta::List(meta) if meta.path.is_ident("till") => {
                if till.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "duplicate #[till(...)] attribute",
                    ));
                }
                till = Some(attr.parse_args::<syn::Ident>()?);
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
            Meta::Path(path) if path.is_ident("error") => {
                if error {
                    return Err(syn::Error::new(attr.span(), "duplicate #[error] attribute"));
                }
                error = true;
            }
            Meta::List(meta) if meta.path.is_ident("each") => {
                return Err(syn::Error::new(
                    attr.span(),
                    "legacy #[each(...)] is unsupported; use #[from(...)]",
                ));
            }
            Meta::Path(path) if path.is_ident("enter") || path.is_ident("leave") => {
                return Err(syn::Error::new(
                    attr.span(),
                    "legacy #[enter]/#[leave] is unsupported; use #[then_require(...)] and #[from(...)]",
                ));
            }
            _ => {}
        }
    }

    let matcher = match (regex, from, error) {
        (Some(regex), None, false) => Some(MatcherConfig::Regex(regex)),
        (None, Some(from), false) => Some(MatcherConfig::From(from)),
        (None, None, true) => None,
        (Some(_), _, true) | (_, Some(_), true) => {
            return Err(syn::Error::new(
                variant.span(),
                "#[error] variants cannot also define #[regex(...)] or #[from(...)]",
            ));
        }
        (None, None, false) => {
            return Err(syn::Error::new(
                variant.span(),
                "token variants require exactly one of #[regex(...)] or #[from(...)] unless they are marked #[error]",
            ));
        }
        _ => unreachable!(),
    };

    Ok(VariantConfig {
        matcher,
        then_require,
        till,
        skip,
        validate,
        error,
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

pub fn field_parse_expr(
    field: &Field,
    input_ident: &str,
    token_name: &str,
) -> syn::Result<proc_macro2::TokenStream> {
    let field_ty = &field.ty;
    let input_ident = format_ident!("{}", input_ident);
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
            ::plingo::component::lex::__macro_private::IntoLexemeResult::into_lexeme_result((#expr)(#input_ident))
                .map_err(|source| ::plingo::component::lex::LexInterrupt::token_parse_failed(#token_name, #input_ident, source))?
        });
    }

    Ok(match parse_expr {
        Some(expr) => expr,
        None => quote! {
            <#field_ty as ::plingo::component::lex::FromLexeme>::from_lexeme(#input_ident)
                .map_err(|source| ::plingo::component::lex::LexInterrupt::token_parse_failed(#token_name, #input_ident, source))?
        },
    })
}

pub fn push_missing_derives(item: &mut syn::ItemEnum, required: &[&str]) -> syn::Result<()> {
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
