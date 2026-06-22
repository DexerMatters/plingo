use std::collections::BTreeMap;

use quote::{format_ident, quote};
use syn::{Fields, ItemEnum, Type, Variant, spanned::Spanned};

use crate::shared::{
    MatcherConfig, VariantConfig, ensure_no_field_parse_attrs, field_parse_expr,
    parse_variant_config,
};

pub fn expand_terminal_derive(item: ItemEnum) -> syn::Result<proc_macro::TokenStream> {
    let enum_ident = item.ident.clone();
    let variants = item.variants.clone();

    let configs = variants
        .iter()
        .enumerate()
        .map(|(index, variant)| {
            parse_variant_config(variant).map(|config| (variant.ident.clone(), (index, config)))
        })
        .collect::<syn::Result<BTreeMap<_, _>>>()?;

    let error_variants = variants
        .iter()
        .filter_map(|variant| configs.get(&variant.ident).and_then(|(_, config)| config.error.then_some(variant)))
        .collect::<Vec<_>>();
    if error_variants.len() != 1 {
        return Err(syn::Error::new(
            enum_ident.span(),
            "terminal enums require exactly one #[error] variant",
        ));
    }
    let error_variant = error_variants[0];

    for variant in &variants {
        let (_, config) = configs.get(&variant.ident).unwrap();
        let matcher = config.matcher.as_ref();

        if config.skip && matches!(matcher, Some(MatcherConfig::From(_))) {
            return Err(syn::Error::new(
                variant.span(),
                "#[skip] is not supported on #[from(...)] variants",
            ));
        }
        if config.then_require.is_some() && matches!(matcher, Some(MatcherConfig::From(_))) {
            return Err(syn::Error::new(
                variant.span(),
                "#[from(...)] variants cannot also use #[then_require(...)]",
            ));
        }
        if config.till.is_some() && matches!(matcher, Some(MatcherConfig::Regex(_))) {
            return Err(syn::Error::new(
                variant.span(),
                "#[till(...)] is only valid on #[from(...)] variants",
            ));
        }
        if matcher.is_none() && (config.then_require.is_some() || config.till.is_some() || config.skip) {
            return Err(syn::Error::new(
                variant.span(),
                "#[error] variants cannot carry matcher flow attributes",
            ));
        }

        if let Some(target) = &config.then_require {
            let Some((_, target_config)) = configs.get(target) else {
                return Err(syn::Error::new(
                    target.span(),
                    "unknown #[then_require(...)] target",
                ));
            };
            if !matches!(target_config.matcher, Some(MatcherConfig::From(_))) {
                return Err(syn::Error::new(
                    target.span(),
                    "#[then_require(...)] target must be a #[from(...)] variant",
                ));
            }
        }

        if let Some(target) = &config.till {
            let Some((_, target_config)) = configs.get(target) else {
                return Err(syn::Error::new(
                    target.span(),
                    "unknown #[till(...)] target",
                ));
            };
            if !matches!(target_config.matcher, Some(MatcherConfig::Regex(_))) {
                return Err(syn::Error::new(
                    target.span(),
                    "#[till(...)] target must be a #[regex(...)] variant",
                ));
            }
        }

        if matches!(matcher, Some(MatcherConfig::From(_))) && config.till.is_none() {
            return Err(syn::Error::new(
                variant.span(),
                "#[from(...)] variants require #[till(...)]",
            ));
        }
    }

    let mut helpers = Vec::new();
    let mut lifted_regs = Vec::new();
    for (index, variant) in variants.iter().enumerate() {
        let (_, config) = configs.get(&variant.ident).unwrap();
        match config.matcher.as_ref() {
            Some(MatcherConfig::Regex(_)) if !config.error => {
                helpers.push(build_regex_builder(&enum_ident, variant, index)?);
            }
            Some(MatcherConfig::From(inner)) if !config.error => {
                let (from_helpers, lifted) =
                    build_from_variant_helpers(&enum_ident, variant, index, inner, &configs)?;
                helpers.extend(from_helpers);
                lifted_regs.push(lifted);
            }
            _ => {}
        }
    }

    let (error_builder_ident, error_builder_fn) = build_error_builder(&enum_ident, error_variant)?;
    let root_specs_fn = format_ident!("__plingo_token_specs_for_{}", enum_ident);
    let root_specs = variants
        .iter()
        .enumerate()
        .filter_map(|(index, variant)| {
            let (_, config) = configs.get(&variant.ident).unwrap();
            matches!(config.matcher, Some(MatcherConfig::Regex(_))).then_some((index, variant))
        })
        .map(|(index, variant)| build_root_token_spec(&enum_ident, variant, index, &configs))
        .collect::<syn::Result<Vec<_>>>()?;

    let generate_impl = build_generate_impl(&enum_ident, &variants, &configs)?;
    let parser_terminal_impl = build_parser_terminal_impl(&enum_ident, &variants, &configs)?;

    Ok(quote! {
        impl ::plingo::component::lex::TokenState for #enum_ident {
            fn display_name() -> &'static str {
                stringify!(#enum_ident)
            }

            fn state_key() -> &'static str {
                concat!(module_path!(), "::", stringify!(#enum_ident))
            }
        }

        impl ::plingo::component::lex::StateTokens for #enum_ident {
            fn token_specs() -> ::std::vec::Vec<::plingo::component::lex::__macro_private::TokenSpec<Self>> {
                #root_specs_fn()
            }

            fn error_builder() -> ::plingo::component::lex::__macro_private::BuildErrorToken<Self> {
                ::std::sync::Arc::new(#error_builder_ident)
            }
        }

        impl ::plingo::component::lex::LexerRoot for #enum_ident {
            fn state_registrations() -> ::std::vec::Vec<::plingo::component::lex::__macro_private::StateRegistration<Self>> {
                let mut registrations = ::std::vec![<#enum_ident as ::plingo::component::lex::StateTokens>::state_registration()];
                #(#lifted_regs)*
                registrations
            }
        }

        #(#helpers)*
        #error_builder_fn

        #[allow(non_snake_case)]
        fn #root_specs_fn() -> ::std::vec::Vec<::plingo::component::lex::__macro_private::TokenSpec<#enum_ident>> {
            ::std::vec![#(#root_specs),*]
        }

        #generate_impl
        #parser_terminal_impl
    }
    .into())
}

fn build_regex_builder(
    root_ident: &syn::Ident,
    variant: &Variant,
    index: usize,
) -> syn::Result<proc_macro2::TokenStream> {
    let builder_ident = format_ident!("__plingo_build_{}_{}", root_ident, index);
    let variant_ident = &variant.ident;
    let token_name = format!("{}::{}", root_ident, variant_ident);

    let body = match &variant.fields {
        Fields::Unit => {
            ensure_no_field_parse_attrs(variant)?;
            quote! { ::std::result::Result::Ok(#root_ident::#variant_ident) }
        }
        Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            let field = fields.unnamed.first().unwrap();
            let field_ty = &field.ty;
            let parse_expr = field_parse_expr(field, "lexeme", &token_name)?;
            quote! {
                let value: #field_ty = #parse_expr;
                ::std::result::Result::Ok(#root_ident::#variant_ident(value))
            }
        }
        Fields::Named(fields) if fields.named.len() == 1 => {
            let field = fields.named.first().unwrap();
            let field_ident = field.ident.as_ref().unwrap();
            let field_ty = &field.ty;
            let parse_expr = field_parse_expr(field, "lexeme", &token_name)?;
            quote! {
                let value: #field_ty = #parse_expr;
                ::std::result::Result::Ok(#root_ident::#variant_ident { #field_ident: value })
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
        ) -> ::std::result::Result<#root_ident, ::plingo::component::lex::LexInterrupt> {
            #body
        }
    })
}

fn build_from_variant_helpers(
    root_ident: &syn::Ident,
    variant: &Variant,
    index: usize,
    inner: &Type,
    configs: &BTreeMap<syn::Ident, (usize, VariantConfig)>,
) -> syn::Result<(Vec<proc_macro2::TokenStream>, proc_macro2::TokenStream)> {
    let success_builder_ident = format_ident!("__plingo_from_success_{}_{}", root_ident, index);
    let wrap_builder_ident = format_ident!("__plingo_from_wrap_{}_{}", root_ident, index);
    let variant_ident = &variant.ident;
    let token_name = format!("{}::{}", root_ident, variant_ident);
    let label = token_name.clone();

    let (success_body, wrap_body) = match &variant.fields {
        Fields::Unit => {
            ensure_no_field_parse_attrs(variant)?;
            (
                quote! {
                    let _ = lexeme;
                    let _ = nested;
                    ::std::result::Result::Ok(#root_ident::#variant_ident)
                },
                None,
            )
        }
        Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            let field = fields.unnamed.first().unwrap();
            let field_ty = &field.ty;
            if field.attrs.iter().any(|attr| attr.path().is_ident("parse")) {
                let parse_expr = field_parse_expr(field, "lexeme", &token_name)?;
                (
                    quote! {
                        let _ = nested;
                        let value: #field_ty = #parse_expr;
                        ::std::result::Result::Ok(#root_ident::#variant_ident(value))
                    },
                    None,
                )
            } else if type_eq(field_ty, inner) {
                (
                    quote! {
                        ::std::result::Result::Ok(#root_ident::#variant_ident(nested))
                    },
                    Some(quote! {
                        ::std::result::Result::Ok(#root_ident::#variant_ident(nested))
                    }),
                )
            } else {
                return Err(syn::Error::new(
                    field.span(),
                    "#[from(...)] payload without #[parse(...)] must be the inner token type",
                ));
            }
        }
        Fields::Named(fields) if fields.named.len() == 1 => {
            let field = fields.named.first().unwrap();
            let field_ident = field.ident.as_ref().unwrap();
            let field_ty = &field.ty;
            if field.attrs.iter().any(|attr| attr.path().is_ident("parse")) {
                let parse_expr = field_parse_expr(field, "lexeme", &token_name)?;
                (
                    quote! {
                        let _ = nested;
                        let value: #field_ty = #parse_expr;
                        ::std::result::Result::Ok(#root_ident::#variant_ident { #field_ident: value })
                    },
                    None,
                )
            } else if type_eq(field_ty, inner) {
                (
                    quote! {
                        ::std::result::Result::Ok(#root_ident::#variant_ident { #field_ident: nested })
                    },
                    Some(quote! {
                        ::std::result::Result::Ok(#root_ident::#variant_ident { #field_ident: nested })
                    }),
                )
            } else {
                return Err(syn::Error::new(
                    field.span(),
                    "#[from(...)] payload without #[parse(...)] must be the inner token type",
                ));
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

    let wrap_builder = wrap_body.map(|body| {
        quote! {
            #[allow(non_snake_case)]
            fn #wrap_builder_ident(
                nested: #inner,
            ) -> ::std::result::Result<#root_ident, ::plingo::component::lex::LexInterrupt> {
                #body
            }
        }
    });

    let success_builder = quote! {
        #[allow(non_snake_case)]
        fn #success_builder_ident(
            lexeme: &str,
            nested: #inner,
        ) -> ::std::result::Result<#root_ident, ::plingo::component::lex::LexInterrupt> {
            #success_body
        }
    };

    let till_target = configs
        .get(
            configs
                .get(&variant.ident)
                .unwrap()
                .1
                .till
                .as_ref()
                .expect("validated #[from] variant must have till target"),
        )
        .unwrap()
        .0;

    let boundary_terminal = terminal_id_expr(root_ident, till_target);
    let variant_terminal = terminal_id_expr(root_ident, index);
    let synthetic_key = synthetic_state_key_expr(root_ident, variant_ident);
    let outer_validate = validate_expr(
        &configs.get(&variant.ident).unwrap().1.validate,
    );
    let wrap_expr = if wrap_builder.is_some() {
        quote! {
            ::std::option::Option::Some(
                ::std::sync::Arc::new(#wrap_builder_ident)
                    as ::plingo::component::lex::__macro_private::WrapLiftedToken<#root_ident, #inner>
            )
        }
    } else {
        quote! { ::std::option::Option::None }
    };

    let lifted = quote! {
        registrations.extend(::plingo::component::lex::lift_state_registrations::<#root_ident, #inner>(
            ::std::sync::Arc::new(#success_builder_ident)
                as ::plingo::component::lex::__macro_private::BuildLiftedToken<#root_ident, #inner>,
            #wrap_expr,
            <#root_ident as ::plingo::component::lex::StateTokens>::error_builder(),
            #synthetic_key,
            ::std::option::Option::Some(::plingo::component::lex::__macro_private::StateBoundary {
                target_terminal: #boundary_terminal,
            }),
            #variant_terminal,
            #label,
            #outer_validate,
        ));
    };

    let mut helpers = vec![success_builder];
    if let Some(wrap_builder) = wrap_builder {
        helpers.push(wrap_builder);
    }
    Ok((helpers, lifted))
}

fn build_error_builder(
    root_ident: &syn::Ident,
    variant: &Variant,
) -> syn::Result<(syn::Ident, proc_macro2::TokenStream)> {
    let builder_ident = format_ident!("__plingo_build_error_{}", root_ident);
    let variant_ident = &variant.ident;

    let body = match &variant.fields {
        Fields::Unit => quote! {
            let _ = info;
            ::std::result::Result::Ok(#root_ident::#variant_ident)
        },
        Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            let field = fields.unnamed.first().unwrap();
            ensure_lex_error_info(&field.ty)?;
            quote! {
                ::std::result::Result::Ok(#root_ident::#variant_ident(info))
            }
        }
        Fields::Named(fields) if fields.named.len() == 1 => {
            let field = fields.named.first().unwrap();
            let field_ident = field.ident.as_ref().unwrap();
            ensure_lex_error_info(&field.ty)?;
            quote! {
                ::std::result::Result::Ok(#root_ident::#variant_ident { #field_ident: info })
            }
        }
        Fields::Unnamed(fields) => {
            return Err(syn::Error::new(
                fields.span(),
                "#[error] variants support either unit or a single LexErrorInfo field",
            ));
        }
        Fields::Named(fields) => {
            return Err(syn::Error::new(
                fields.span(),
                "#[error] variants support either unit or a single LexErrorInfo field",
            ));
        }
    };

    Ok((
        builder_ident.clone(),
        quote! {
            #[allow(non_snake_case)]
            fn #builder_ident(
                info: ::plingo::component::lex::LexErrorInfo,
            ) -> ::std::result::Result<#root_ident, ::plingo::component::lex::LexInterrupt> {
                #body
            }
        },
    ))
}

fn build_root_token_spec(
    root_ident: &syn::Ident,
    variant: &Variant,
    index: usize,
    configs: &BTreeMap<syn::Ident, (usize, VariantConfig)>,
) -> syn::Result<proc_macro2::TokenStream> {
    let (_, config) = configs.get(&variant.ident).unwrap();
    let Some(MatcherConfig::Regex(regex)) = config.matcher.as_ref() else {
        return Err(syn::Error::new(
            variant.span(),
            "only #[regex(...)] variants belong to the root state",
        ));
    };
    let builder_ident = format_ident!("__plingo_build_{}_{}", root_ident, index);
    let label = format!("{}::{}", root_ident, variant.ident);
    let skip = config.skip;
    let captures_context = config.then_require.is_some() && !variant.fields.is_empty();
    let validate = validate_expr(&config.validate);
    let action = if let Some(target) = &config.then_require {
        let synthetic_key = synthetic_state_key_expr(root_ident, target);
        quote! {
            ::plingo::component::lex::__macro_private::StateDirective::Enter(#synthetic_key.to_string())
        }
    } else {
        quote! { ::plingo::component::lex::__macro_private::StateDirective::None }
    };
    let terminal = terminal_id_expr(root_ident, index);

    Ok(quote! {
        ::plingo::component::lex::__macro_private::TokenSpec {
            regex: #regex,
            terminal: #terminal,
            precedence: #index,
            label: #label,
            action: #action,
            skip: #skip,
            build: ::std::sync::Arc::new(#builder_ident),
            captures_context: #captures_context,
            validate: #validate,
        }
    })
}

fn build_generate_impl(
    root_ident: &syn::Ident,
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
    configs: &BTreeMap<syn::Ident, (usize, VariantConfig)>,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut arms = Vec::new();

    for variant in variants {
        let (index, config) = configs.get(&variant.ident).unwrap();
        if config.error {
            continue;
        }
        arms.push(build_generate_arm(root_ident, variant, *index, config)?);
    }

    Ok(quote! {
        impl #root_ident {
            #[doc(hidden)]
            pub fn __plingo_generate_variant<W: ::std::fmt::Write>(
                variant: &'static str,
                seed: u64,
                dest: &mut W,
            ) -> ::std::result::Result<(), ::plingo::component::lex::GenerateError> {
                match variant {
                    #(#arms),*,
                    _ => ::std::result::Result::Err(
                        ::plingo::component::lex::GenerateError::UnknownVariant {
                            state: stringify!(#root_ident),
                            variant,
                        },
                    ),
                }
            }
        }
    })
}

fn build_generate_arm(
    root_ident: &syn::Ident,
    variant: &Variant,
    index: usize,
    config: &VariantConfig,
) -> syn::Result<proc_macro2::TokenStream> {
    let builder_ident = format_ident!("__plingo_build_{}_{}", root_ident, index);
    let generator_ident = format_ident!("__PLINGO_GENERATOR_{}_{}", root_ident, index);
    let variant_ident = &variant.ident;
    let label = format!("{}::{}", root_ident, variant_ident);

    match config.matcher.as_ref() {
        Some(MatcherConfig::From(_)) => Ok(quote! {
            stringify!(#variant_ident) => ::std::result::Result::Err(
                ::plingo::component::lex::GenerateError::UnsupportedFromVariant {
                    token: #label,
                },
            )
        }),
        Some(MatcherConfig::Regex(_)) if config.validate.is_some() => Ok(quote! {
            stringify!(#variant_ident) => ::std::result::Result::Err(
                ::plingo::component::lex::GenerateError::UnsupportedValidatedVariant {
                    token: #label,
                },
            )
        }),
        Some(MatcherConfig::Regex(regex)) => {
            let accept_pattern = generate_accept_pattern(root_ident, variant)?;
            Ok(quote! {
                stringify!(#variant_ident) => {
                    #[allow(non_upper_case_globals)]
                    static #generator_ident: ::plingo::component::lex::__macro_private::GeneratorCache =
                        ::plingo::component::lex::__macro_private::GeneratorCache::new();
                    ::plingo::component::lex::__macro_private::generate_token(
                        &#generator_ident,
                        #label,
                        #regex,
                        seed,
                        dest,
                        |candidate| match #builder_ident(candidate) {
                            ::std::result::Result::Ok(value) => matches!(value, #accept_pattern),
                            ::std::result::Result::Err(_) => false,
                        },
                    )
                }
            })
        }
        None => unreachable!(),
    }
}

fn generate_accept_pattern(
    root_ident: &syn::Ident,
    variant: &Variant,
) -> syn::Result<proc_macro2::TokenStream> {
    let variant_ident = &variant.ident;
    Ok(match &variant.fields {
        Fields::Unit => quote! { #root_ident::#variant_ident },
        Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            quote! { #root_ident::#variant_ident(..) }
        }
        Fields::Named(fields) if fields.named.len() == 1 => {
            quote! { #root_ident::#variant_ident { .. } }
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
    })
}

fn build_parser_terminal_impl(
    root_ident: &syn::Ident,
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
    configs: &BTreeMap<syn::Ident, (usize, VariantConfig)>,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut arms = Vec::new();
    for (index, variant) in variants.iter().enumerate() {
        if configs.get(&variant.ident).unwrap().1.error {
            continue;
        }
        let variant_ident = &variant.ident;
        let label = format!("{}::{}", root_ident, variant_ident);
        let terminal = terminal_id_expr(root_ident, index);
        arms.push(quote! {
            stringify!(#variant_ident) => grammar.terminal_symbol(
                #label,
                #terminal,
                ::std::option::Option::None,
            )
        });
    }

    Ok(quote! {
        impl ::plingo::component::parse::__macro_private::TokenVariantSpec for #root_ident {
            fn register_terminal(
                grammar: &mut ::plingo::component::parse::grammar::GrammarBuilder,
                variant: &'static str,
            ) -> ::plingo::component::parse::grammar::Symbol {
                match variant {
                    #(#arms,)*
                    _ => panic!("unknown token variant {}::{}", stringify!(#root_ident), variant),
                }
            }
        }
    })
}

fn terminal_id_expr(root_ident: &syn::Ident, index: usize) -> proc_macro2::TokenStream {
    quote! {
        ::plingo::component::parse::grammar::TerminalId {
            state_key: <#root_ident as ::plingo::component::lex::TokenState>::state_key(),
            token_id: #index as u32,
        }
    }
}

fn synthetic_state_key_expr(
    root_ident: &syn::Ident,
    variant_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    quote! {
        concat!(
            module_path!(),
            "::",
            stringify!(#root_ident),
            "::",
            stringify!(#variant_ident),
            "::from"
        )
    }
}

fn validate_expr(validate: &Option<syn::Expr>) -> proc_macro2::TokenStream {
    match validate {
        Some(v) => quote! {
            ::std::option::Option::Some(
                ::std::sync::Arc::new(#v as fn(&str, ::std::option::Option<&str>) -> bool)
                    as ::plingo::component::lex::__macro_private::ValidateLexeme
            )
        },
        None => quote! { ::std::option::Option::None },
    }
}

fn type_eq(left: &Type, right: &Type) -> bool {
    quote!(#left).to_string() == quote!(#right).to_string()
}

fn ensure_lex_error_info(ty: &Type) -> syn::Result<()> {
    if quote!(#ty).to_string().replace(' ', "") == "LexErrorInfo"
        || quote!(#ty)
            .to_string()
            .replace(' ', "")
            .ends_with("::LexErrorInfo")
    {
        Ok(())
    } else {
        Err(syn::Error::new(
            ty.span(),
            "#[error] payload must be LexErrorInfo",
        ))
    }
}
