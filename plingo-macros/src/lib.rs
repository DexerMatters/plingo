use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Data, DataEnum, DeriveInput, Expr, Fields, Meta,
    parse::{Parse, ParseStream},
    parse_macro_input,
    spanned::Spanned,
    Field, ItemImpl, Type,
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

    quote! {
        #item_impl

        #conduit_impls
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

// ---------------------------------------------------------------------------
// Tokens derive: #[derive(Tokens)]
// ---------------------------------------------------------------------------

#[proc_macro_derive(Tokens, attributes(regex, enter, leave, skip, parse))]
pub fn derive_tokens(item: TokenStream) -> TokenStream {
    match expand_tokens_derive(parse_macro_input!(item as DeriveInput)) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_tokens_derive(input: DeriveInput) -> syn::Result<TokenStream> {
    let enum_ident = input.ident;
    let rules_fn_ident = format_ident!("__plingo_rules_for_{}", enum_ident);

    let data_enum = match input.data {
        Data::Enum(data_enum) => data_enum,
        _ => {
            return Err(syn::Error::new(
                enum_ident.span(),
                "#[derive(Tokens)] can only be used on enums",
            ));
        }
    };

    let builders = build_token_builders(&enum_ident, &data_enum)?;
    let specs = build_token_specs(&enum_ident, &data_enum)?;

    Ok(quote! {
        impl ::plingo::component::lex::TokenState for #enum_ident {
            fn display_name() -> &'static str {
                stringify!(#enum_ident)
            }

            fn state_key() -> &'static str {
                concat!(module_path!(), "::", stringify!(#enum_ident))
            }
        }

        #[allow(non_snake_case)]
        fn #rules_fn_ident() -> ::std::vec::Vec<::plingo::component::lex::__macro_private::TokenSpec> {
            #(#builders)*

            ::std::vec![#(#specs),*]
        }

        ::plingo::inventory::submit! {
            ::plingo::component::lex::__macro_private::StateRegistration::new(
                stringify!(#enum_ident),
                concat!(module_path!(), "::", stringify!(#enum_ident)),
                #rules_fn_ident,
            )
        }
    }
    .into())
}

fn build_token_builders(
    enum_ident: &syn::Ident,
    data_enum: &DataEnum,
) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    data_enum
        .variants
        .iter()
        .enumerate()
        .map(|(index, variant)| build_token_builder(enum_ident, variant, index))
        .collect()
}

fn build_token_builder(
    enum_ident: &syn::Ident,
    variant: &syn::Variant,
    index: usize,
) -> syn::Result<proc_macro2::TokenStream> {
    let builder_ident = format_ident!("__plingo_build_{}_{}", enum_ident, index);
    let variant_ident = &variant.ident;
    let token_name = format!("{}::{}", enum_ident, variant_ident);

    let body = match &variant.fields {
        Fields::Unit => {
            ensure_no_field_parse_attrs(variant)?;
            quote! {
                ::std::result::Result::Ok(::plingo::component::lex::Token::new(
                    stringify!(#variant_ident),
                    #enum_ident::#variant_ident,
                ))
            }
        }
        Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            let field = fields.unnamed.first().unwrap();
            let field_ty = &field.ty;
            let parse_expr = field_parse_expr(field, &token_name)?;
            quote! {
                let value: #field_ty = #parse_expr;
                ::std::result::Result::Ok(::plingo::component::lex::Token::new(
                    stringify!(#variant_ident),
                    #enum_ident::#variant_ident(value),
                ))
            }
        }
        Fields::Named(fields) if fields.named.len() == 1 => {
            let field = fields.named.first().unwrap();
            let field_ident = field.ident.as_ref().unwrap();
            let field_ty = &field.ty;
            let parse_expr = field_parse_expr(field, &token_name)?;
            quote! {
                let value: #field_ty = #parse_expr;
                ::std::result::Result::Ok(::plingo::component::lex::Token::new(
                    stringify!(#variant_ident),
                    #enum_ident::#variant_ident { #field_ident: value },
                ))
            }
        }
        Fields::Unnamed(fields) => {
            return Err(syn::Error::new(
                fields.span(),
                "token payload variants currently support exactly one field",
            ));
        }
        Fields::Named(fields) => {
            return Err(syn::Error::new(
                fields.span(),
                "token payload variants currently support exactly one field",
            ));
        }
    };

    Ok(quote! {
        #[allow(non_snake_case)]
        fn #builder_ident(
            lexeme: &str,
        ) -> ::std::result::Result<::plingo::component::lex::Token, ::plingo::component::lex::LexError> {
            #body
        }
    })
}

fn build_token_specs(
    enum_ident: &syn::Ident,
    data_enum: &DataEnum,
) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    data_enum
        .variants
        .iter()
        .enumerate()
        .map(|(index, variant)| build_token_spec(enum_ident, variant, index))
        .collect()
}

fn build_token_spec(
    enum_ident: &syn::Ident,
    variant: &syn::Variant,
    index: usize,
) -> syn::Result<proc_macro2::TokenStream> {
    let builder_ident = format_ident!("__plingo_build_{}_{}", enum_ident, index);
    let config = parse_variant_config(variant)?;
    let variant_ident = &variant.ident;
    let display = format!("{}::{}", enum_ident, variant_ident);
    let regex = config.regex;
    let skip = config.skip;

    let action = if let Some(target) = config.enter {
        quote! { ::plingo::component::lex::__macro_private::StateDirective::Enter(<#target as ::plingo::component::lex::TokenState>::state_key()) }
    } else if config.leave {
        quote! { ::plingo::component::lex::__macro_private::StateDirective::Leave }
    } else {
        quote! { ::plingo::component::lex::__macro_private::StateDirective::None }
    };

    Ok(quote! {
        ::plingo::component::lex::__macro_private::TokenSpec {
            regex: #regex,
            precedence: #index,
            label: #display,
            action: #action,
            skip: #skip,
            build: #builder_ident,
        }
    })
}

struct VariantConfig {
    regex: syn::LitStr,
    enter: Option<Type>,
    leave: bool,
    skip: bool,
}

fn parse_variant_config(variant: &syn::Variant) -> syn::Result<VariantConfig> {
    let mut regex = None;
    let mut enter = None;
    let mut leave = false;
    let mut skip = false;

    for attr in &variant.attrs {
        match &attr.meta {
            Meta::List(meta) if meta.path.is_ident("regex") => {
                if regex.is_some() {
                    return Err(syn::Error::new(attr.span(), "duplicate #[regex(...)] attribute"));
                }
                regex = Some(attr.parse_args::<syn::LitStr>()?);
            }
            Meta::List(meta) if meta.path.is_ident("enter") => {
                if enter.is_some() {
                    return Err(syn::Error::new(attr.span(), "duplicate #[enter(...)] attribute"));
                }
                enter = Some(attr.parse_args::<Type>()?);
            }
            Meta::Path(path) if path.is_ident("leave") => {
                if leave {
                    return Err(syn::Error::new(attr.span(), "duplicate #[leave] attribute"));
                }
                leave = true;
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
    })
}

fn ensure_no_field_parse_attrs(variant: &syn::Variant) -> syn::Result<()> {
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

fn field_parse_expr(field: &Field, token_name: &str) -> syn::Result<proc_macro2::TokenStream> {
    let field_ty = &field.ty;
    let mut parse_expr = None;

    for attr in &field.attrs {
        if !attr.path().is_ident("parse") {
            continue;
        }
        if parse_expr.is_some() {
            return Err(syn::Error::new(attr.span(), "duplicate #[parse(...)] attribute"));
        }

        let expr = attr.parse_args::<Expr>()?;
        parse_expr = Some(quote! {
            ::plingo::component::lex::__macro_private::IntoLexemeResult::into_lexeme_result((#expr)(lexeme))
                .map_err(|source| ::plingo::component::lex::LexError::token_parse_failed(#token_name, lexeme, source))?
        });
    }

    Ok(match parse_expr {
        Some(expr) => expr,
        None => quote! {
            <#field_ty as ::plingo::component::lex::FromLexeme>::from_lexeme(lexeme)
                .map_err(|source| ::plingo::component::lex::LexError::token_parse_failed(#token_name, lexeme, source))?
        },
    })
}
