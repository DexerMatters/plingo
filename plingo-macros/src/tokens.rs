use quote::{format_ident, quote};
use syn::{Fields, Ident, ItemEnum, Variant, spanned::Spanned};

use crate::shared::{
    ensure_no_field_parse_attrs, field_parse_expr, parse_variant_config, push_missing_derives,
    target_last_ident,
};

pub fn expand_tokens_attr(mut item: ItemEnum) -> syn::Result<proc_macro::TokenStream> {
    push_missing_derives(&mut item, &["PartialEq", "Eq", "Hash"])?;
    let enum_ident = item.ident.clone();
    let wrapper_variants = collect_wrapper_variants(&item)?;
    let original_variants = item.variants.clone();
    strip_token_attrs(&mut item);
    item.variants.extend(wrapper_variants.clone());

    let state_specs_fn = format_ident!("__plingo_token_specs_for_{}", enum_ident);
    let root_regs_fn = format_ident!("__plingo_state_regs_for_{}", enum_ident);
    let builders = build_root_token_builders(&enum_ident, &original_variants)?;
    let specs = build_root_token_specs(&enum_ident, &original_variants)?;
    let lifted_regs = build_lifted_registrations(&enum_ident, &original_variants)?;
    let parser_terminal_impl = build_parser_terminal_impl(&enum_ident, &original_variants)?;

    Ok(quote! {
        #item

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
                #state_specs_fn()
            }
        }

        impl ::plingo::component::lex::LexerRoot for #enum_ident {
            fn state_registrations() -> ::std::vec::Vec<::plingo::component::lex::__macro_private::StateRegistration<Self>> {
                #root_regs_fn()
            }
        }

        #(#builders)*

        #[allow(non_snake_case)]
        fn #state_specs_fn() -> ::std::vec::Vec<::plingo::component::lex::__macro_private::TokenSpec<#enum_ident>> {
            ::std::vec![#(#specs),*]
        }

        #[allow(non_snake_case)]
        fn #root_regs_fn() -> ::std::vec::Vec<::plingo::component::lex::__macro_private::StateRegistration<#enum_ident>> {
            let mut registrations = ::std::vec![<#enum_ident as ::plingo::component::lex::StateTokens>::state_registration()];
            #(#lifted_regs)*
            registrations
        }

        #parser_terminal_impl
    }
    .into())
}

fn strip_token_attrs(item: &mut ItemEnum) {
    for variant in &mut item.variants {
        variant.attrs.retain(|attr| {
            let path = attr.path();
            !(path.is_ident("regex")
                || path.is_ident("enter")
                || path.is_ident("leave")
                || path.is_ident("skip")
                || path.is_ident("validate"))
        });

        for field in &mut variant.fields {
            field.attrs.retain(|attr| !attr.path().is_ident("parse"));
        }
    }
}

fn collect_wrapper_variants(item: &ItemEnum) -> syn::Result<Vec<Variant>> {
    let mut wrappers = Vec::new();
    for variant in &item.variants {
        let config = parse_variant_config(variant)?;
        let Some(target) = config.enter else {
            continue;
        };
        let wrapper_ident = target_last_ident(&target)?;
        wrappers.push(syn::parse_quote! {
            #wrapper_ident(#target)
        });
    }
    Ok(wrappers)
}

fn build_root_token_builders(
    root_ident: &Ident,
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    variants
        .iter()
        .enumerate()
        .map(|(index, variant)| build_root_token_builder(root_ident, variant, index))
        .collect()
}

fn build_root_token_builder(
    root_ident: &Ident,
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
            let parse_expr = field_parse_expr(field, &token_name)?;
            quote! {
                let value: #field_ty = #parse_expr;
                ::std::result::Result::Ok(#root_ident::#variant_ident(value))
            }
        }
        Fields::Named(fields) if fields.named.len() == 1 => {
            let field = fields.named.first().unwrap();
            let field_ident = field.ident.as_ref().unwrap();
            let field_ty = &field.ty;
            let parse_expr = field_parse_expr(field, &token_name)?;
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

fn build_root_token_specs(
    root_ident: &Ident,
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    variants
        .iter()
        .enumerate()
        .map(|(index, variant)| build_root_token_spec(root_ident, variant, index))
        .collect()
}

fn build_root_token_spec(
    root_ident: &Ident,
    variant: &Variant,
    index: usize,
) -> syn::Result<proc_macro2::TokenStream> {
    let builder_ident = format_ident!("__plingo_build_{}_{}", root_ident, index);
    let config = parse_variant_config(variant)?;
    let variant_ident = &variant.ident;
    let display = format!("{}::{}", root_ident, variant_ident);
    let regex = config.regex;
    let skip = config.skip;
    let has_fields = !variant.fields.is_empty();
    let captures_context = config.enter.is_some() && has_fields;

    let validate = match config.validate {
        Some(v) => {
            quote! { ::std::option::Option::Some(#v as fn(&str, ::std::option::Option<&str>) -> bool) }
        }
        None => quote! { ::std::option::Option::None },
    };

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
            terminal: ::plingo::component::parse::grammar::TerminalId {
                state_key: <#root_ident as ::plingo::component::lex::TokenState>::state_key(),
                token_id: #index as u32,
            },
            precedence: #index,
            label: #display,
            action: #action,
            skip: #skip,
            build: ::std::sync::Arc::new(#builder_ident),
            captures_context: #captures_context,
            validate: #validate,
        }
    })
}

fn build_lifted_registrations(
    root_ident: &Ident,
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    let mut lifted = Vec::new();
    for variant in variants {
        let config = parse_variant_config(variant)?;
        let Some(target) = config.enter else {
            continue;
        };
        let wrapper_ident = target_last_ident(&target)?;
        lifted.push(quote! {
            registrations.extend(::plingo::component::lex::lift_state_registrations::<#root_ident, #target>(#root_ident::#wrapper_ident));
        });
    }
    Ok(lifted)
}

fn build_parser_terminal_impl(
    root_ident: &Ident,
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut arms = Vec::new();
    for (index, variant) in variants.iter().enumerate() {
        match &variant.fields {
            Fields::Unit | Fields::Unnamed(_) | Fields::Named(_) => {
                let variant_ident = &variant.ident;
                let label = format!("{}::{}", root_ident, variant_ident);
                arms.push(quote! {
                    stringify!(#variant_ident) => grammar.terminal_symbol(
                        #label,
                        ::plingo::component::parse::grammar::TerminalId {
                            state_key: <#root_ident as ::plingo::component::lex::TokenState>::state_key(),
                            token_id: #index as u32,
                        },
                        ::std::option::Option::None,
                    )
                });
            }
        }
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
