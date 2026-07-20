use quote::{format_ident, quote};
use syn::{Expr, Field, Meta, spanned::Spanned};

pub struct VariantConfig {
    pub regex: Option<syn::LitStr>,
    pub empty: bool,
    pub skip: bool,
    pub when: Option<syn::Expr>,
    pub recover_when: Option<syn::Expr>,
    pub enter: Option<syn::Ident>,
    pub exit: bool,
    pub with: Option<syn::Expr>,
    pub error: bool,
}

pub fn parse_variant_config(variant: &syn::Variant) -> syn::Result<VariantConfig> {
    let mut regex = None;
    let mut empty = false;
    let mut skip = false;
    let mut when = None;
    let mut recover_when = None;
    let mut enter = None;
    let mut exit = false;
    let mut with = None;
    let mut error = false;

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
            Meta::Path(path) if path.is_ident("empty") => {
                if empty {
                    return Err(syn::Error::new(attr.span(), "duplicate #[empty] attribute"));
                }
                empty = true;
            }
            Meta::List(meta) if meta.path.is_ident("empty") => {
                return Err(syn::Error::new(attr.span(), "#[empty] takes no arguments"));
            }
            Meta::List(meta) if meta.path.is_ident("when") => {
                if when.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "duplicate #[when(...)] attribute",
                    ));
                }
                when = Some(attr.parse_args::<syn::Expr>()?);
            }
            Meta::List(meta) if meta.path.is_ident("recover_when") => {
                if recover_when.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "duplicate #[recover_when(...)] attribute",
                    ));
                }
                recover_when = Some(attr.parse_args::<syn::Expr>()?);
            }
            Meta::List(meta) if meta.path.is_ident("enter") => {
                if enter.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "duplicate #[enter(...)] attribute",
                    ));
                }
                enter = Some(attr.parse_args::<syn::Ident>()?);
            }
            Meta::Path(path) if path.is_ident("exit") => {
                if exit {
                    return Err(syn::Error::new(attr.span(), "duplicate #[exit] attribute"));
                }
                exit = true;
            }
            Meta::List(meta) if meta.path.is_ident("exit") => {
                return Err(syn::Error::new(
                    attr.span(),
                    "#[exit] takes no arguments; use #[exit] and optionally #[when(...)]",
                ));
            }
            Meta::List(meta) if meta.path.is_ident("with") => {
                if with.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "duplicate #[with(...)] attribute",
                    ));
                }
                with = Some(attr.parse_args::<syn::Expr>()?);
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
            Meta::List(meta) if meta.path.is_ident("one_of") => {
                return Err(syn::Error::new(
                    attr.span(),
                    "#[one_of(...)] was removed; use enum-level #[scopes(...)] entries instead",
                ));
            }
            Meta::Path(path) if path.is_ident("leave") => {
                return Err(syn::Error::new(
                    attr.span(),
                    "#[leave] was removed; use #[exit]",
                ));
            }
            Meta::List(meta) if meta.path.is_ident("leave_when") => {
                return Err(syn::Error::new(
                    attr.span(),
                    "#[leave_when(...)] was removed; use #[exit] with #[when(...)]",
                ));
            }
            _ => {}
        }
    }

    if enter.is_some() && exit {
        return Err(syn::Error::new(
            variant.span(),
            "token variants cannot use both #[enter(...)] and #[exit]",
        ));
    }

    let matcher_count = usize::from(regex.is_some()) + usize::from(empty);
    if empty {
        if when.is_none() {
            return Err(syn::Error::new(
                variant.span(),
                "#[empty] variants require #[when(...)]",
            ));
        }
        if !(enter.is_some() || exit || with.is_some()) {
            return Err(syn::Error::new(
                variant.span(),
                "#[empty] variants must change lexer state with #[enter(...)], #[exit], or #[with(...)]",
            ));
        }
        if skip {
            return Err(syn::Error::new(
                variant.span(),
                "#[empty] variants cannot use #[skip]",
            ));
        }
        if recover_when.is_some() {
            return Err(syn::Error::new(
                variant.span(),
                "#[empty] variants cannot use #[recover_when(...)]",
            ));
        }
        if !matches!(variant.fields, syn::Fields::Unit) {
            return Err(syn::Error::new(
                variant.span(),
                "#[empty] variants cannot have payload fields",
            ));
        }
    }

    match (matcher_count, error) {
        (_, true) if matcher_count != 0 => Err(syn::Error::new(
            variant.span(),
            "#[error] variants cannot also define matcher attributes",
        )),
        (0, false) => Err(syn::Error::new(
            variant.span(),
            "token variants require exactly one matcher: #[regex(...)] or #[empty]",
        )),
        (2.., false) => Err(syn::Error::new(
            variant.span(),
            "token variants cannot use both #[regex(...)] and #[empty]",
        )),
        _ if error
            && (when.is_some()
                || recover_when.is_some()
                || enter.is_some()
                || exit
                || with.is_some()
                || skip) =>
        {
            Err(syn::Error::new(
                variant.span(),
                "#[error] variants cannot also define matcher, scope, skip, or recovery attributes",
            ))
        }
        _ => Ok(VariantConfig {
            regex,
            empty,
            skip,
            when,
            recover_when,
            enter,
            exit,
            with,
            error,
        }),
    }
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
