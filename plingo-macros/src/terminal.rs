use std::collections::{BTreeMap, BTreeSet};

use quote::{format_ident, quote};
use syn::{
    Fields, ItemEnum, Token, Type, Variant,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    spanned::Spanned,
};

use crate::shared::{
    VariantConfig, ensure_no_field_parse_attrs, field_parse_expr, parse_variant_config,
};

struct ScopeEntryConfig {
    variant: syn::Ident,
}

struct ScopeConfig {
    name: syn::Ident,
    entries: Vec<ScopeEntryConfig>,
}

struct ScopesConfig {
    scopes: Vec<ScopeConfig>,
}

struct ScopeSlotConfig {
    name: syn::Ident,
    ty: Type,
}

struct ScopeSlotsConfig {
    slots: Vec<ScopeSlotConfig>,
}

impl Parse for ScopesConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let scopes = Punctuated::<ScopeConfig, Token![,]>::parse_terminated(input)?
            .into_iter()
            .collect();
        Ok(Self { scopes })
    }
}

impl Parse for ScopeConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let name = input.parse::<syn::Ident>()?;
        let content;
        syn::braced!(content in input);
        let entries = Punctuated::<ScopeEntryConfig, Token![,]>::parse_terminated(&content)?
            .into_iter()
            .collect();
        Ok(Self { name, entries })
    }
}

impl Parse for ScopeEntryConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let variant = input.parse::<syn::Ident>()?;
        if input.peek(Token![=>]) {
            return Err(input.error(
                "scope entries only declare membership; move enter/exit onto the variant attributes",
            ));
        }
        Ok(Self { variant })
    }
}

impl Parse for ScopeSlotsConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let slots = Punctuated::<ScopeSlotConfig, Token![,]>::parse_terminated(input)?
            .into_iter()
            .collect();
        Ok(Self { slots })
    }
}

impl Parse for ScopeSlotConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let name = input.parse::<syn::Ident>()?;
        input.parse::<Token![:]>()?;
        let ty = input.parse::<Type>()?;
        Ok(Self { name, ty })
    }
}

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
        .filter_map(|variant| {
            configs
                .get(&variant.ident)
                .and_then(|(_, config)| config.error.then_some(variant))
        })
        .collect::<Vec<_>>();
    if error_variants.len() != 1 {
        return Err(syn::Error::new(
            enum_ident.span(),
            "terminal enums require exactly one #[error] variant",
        ));
    }
    let error_variant = error_variants[0];

    let scopes = parse_enum_scopes(&item, &variants, &configs)?;
    let (scope_slots, implicit_scope_key) = parse_scope_slots(&item)?;

    let mut builder_tokens = Vec::new();
    let mut builder_idents = BTreeMap::new();
    for (index, variant) in variants.iter().enumerate() {
        let (_, config) = configs.get(&variant.ident).unwrap();
        if config.error {
            continue;
        }
        let builder_ident = format_ident!("__plingo_build_{}_{}", enum_ident, index);
        builder_idents.insert(variant.ident.clone(), builder_ident.clone());
        builder_tokens.push(build_regex_builder(&enum_ident, variant, index)?);
    }

    let (error_builder_ident, error_builder_fn) = build_error_builder(&enum_ident, error_variant)?;
    let scope_specs = scopes
        .iter()
        .map(|scope| build_scope_specs_fn(&enum_ident, scope, &variants, &configs, &builder_idents))
        .collect::<syn::Result<Vec<_>>>()?;
    let scope_regs = scopes
        .iter()
        .map(|scope| build_scope_registration(&enum_ident, scope, &error_builder_ident))
        .collect::<Vec<_>>();
    let slot_support = build_slot_support(&enum_ident, &scope_slots, implicit_scope_key)?;
    let slot_tokens = slot_support.tokens;
    let slot_value = slot_support.slot_value;
    let slot_count = slot_support.count;
    let recover_key = slot_support.recover_key;

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

        impl ::plingo::component::lex::LexerRoot for #enum_ident {
            type SlotValue = #slot_value;

            fn state_registrations() -> ::std::vec::Vec<::plingo::component::lex::__macro_private::ScopeRegistration<Self>> {
                let error_builder = ::std::sync::Arc::new(#error_builder_ident)
                    as ::plingo::component::lex::__macro_private::BuildErrorToken<Self>;
                let mut registrations = ::std::vec::Vec::new();
                #(#scope_regs)*
                registrations
            }

            fn slot_count() -> usize {
                #slot_count
            }

            fn recover_key(
                slots: &::plingo::component::lex::SlotStore<Self>,
            ) -> ::std::option::Option<&str> {
                #recover_key
            }
        }

        #slot_tokens
        #(#builder_tokens)*
        #error_builder_fn
        #(#scope_specs)*
        #generate_impl
        #parser_terminal_impl
    }
    .into())
}

fn parse_scope_slots(item: &ItemEnum) -> syn::Result<(Vec<ScopeSlotConfig>, bool)> {
    let mut parsed = None;
    for attr in &item.attrs {
        let syn::Meta::List(meta) = &attr.meta else {
            continue;
        };
        if !meta.path.is_ident("scope_slots") {
            continue;
        }
        if parsed.is_some() {
            return Err(syn::Error::new(
                attr.span(),
                "duplicate #[scope_slots(...)] attribute",
            ));
        }
        parsed = Some(attr.parse_args::<ScopeSlotsConfig>()?);
    }

    let implicit_scope_key = parsed.is_none();
    let mut slots = parsed.map(|config| config.slots).unwrap_or_default();
    if implicit_scope_key {
        slots.push(ScopeSlotConfig {
            name: format_ident!("scope_key"),
            ty: syn::parse_quote!(String),
        });
    }

    let mut seen = BTreeSet::new();
    for slot in &slots {
        if !seen.insert(slot.name.to_string()) {
            return Err(syn::Error::new(
                slot.name.span(),
                "duplicate scope slot name",
            ));
        }
    }

    Ok((slots, implicit_scope_key))
}

struct SlotSupport {
    tokens: proc_macro2::TokenStream,
    slot_value: proc_macro2::TokenStream,
    count: proc_macro2::TokenStream,
    recover_key: proc_macro2::TokenStream,
}

fn build_slot_support(
    root_ident: &syn::Ident,
    scope_slots: &[ScopeSlotConfig],
    implicit_scope_key: bool,
) -> syn::Result<SlotSupport> {
    let slot_value_ident = format_ident!("__PlingoSlotValue_{}", root_ident);
    let slot_count = scope_slots.len();
    let mut slot_variants = Vec::new();
    let mut slot_fns = Vec::new();
    let mut slot_consts = Vec::new();

    for (index, slot) in scope_slots.iter().enumerate() {
        let variant_ident = format_ident!("Slot{}", index);
        let pack_ident = format_ident!("__plingo_slot_pack_{}_{}", root_ident, slot.name);
        let ref_ident = format_ident!("__plingo_slot_ref_{}_{}", root_ident, slot.name);
        let slot_name = &slot.name;
        let slot_ty = &slot.ty;

        slot_variants.push(quote! { #variant_ident(#slot_ty) });
        slot_fns.push(quote! {
            #[allow(non_snake_case)]
            fn #pack_ident(value: #slot_ty) -> #slot_value_ident {
                #slot_value_ident::#variant_ident(value)
            }

            #[allow(non_snake_case)]
            fn #ref_ident(value: &#slot_value_ident) -> ::std::option::Option<&#slot_ty> {
                match value {
                    #slot_value_ident::#variant_ident(inner) => ::std::option::Option::Some(inner),
                    _ => ::std::option::Option::None,
                }
            }
        });
        slot_consts.push(quote! {
            #[allow(non_upper_case_globals)]
            pub const #slot_name: ::plingo::component::lex::Slot<Self, #slot_ty> =
                ::plingo::component::lex::Slot::new(#index, #pack_ident, #ref_ident);
        });
    }

    let tokens = quote! {
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        #[derive(Clone, PartialEq, Eq, Hash)]
        pub enum #slot_value_ident {
            #(#slot_variants),*
        }

        #(#slot_fns)*

        impl #root_ident {
            #(#slot_consts)*
        }
    };

    let recover_key = if implicit_scope_key {
        quote! { slots.get(#root_ident::scope_key).map(|value| value.as_str()) }
    } else {
        quote! { ::std::option::Option::None }
    };

    Ok(SlotSupport {
        tokens,
        slot_value: quote! { #slot_value_ident },
        count: quote! { #slot_count },
        recover_key,
    })
}

fn parse_enum_scopes(
    item: &ItemEnum,
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
    configs: &BTreeMap<syn::Ident, (usize, VariantConfig)>,
) -> syn::Result<Vec<ScopeConfig>> {
    let mut parsed = None;
    for attr in &item.attrs {
        let syn::Meta::List(meta) = &attr.meta else {
            continue;
        };
        if !meta.path.is_ident("scopes") {
            continue;
        }
        if parsed.is_some() {
            return Err(syn::Error::new(
                attr.span(),
                "duplicate #[scopes(...)] attribute",
            ));
        }
        parsed = Some(attr.parse_args::<ScopesConfig>()?);
    }

    let explicit_scopes = parsed.is_some();
    let mut scopes = match parsed {
        Some(config) => config.scopes,
        None => vec![ScopeConfig {
            name: format_ident!("root"),
            entries: variants
                .iter()
                .filter_map(|variant| {
                    let (_, config) = configs.get(&variant.ident).unwrap();
                    (!config.error).then_some(ScopeEntryConfig {
                        variant: variant.ident.clone(),
                    })
                })
                .collect(),
        }],
    };

    let scope_names = scopes
        .iter()
        .map(|scope| scope.name.clone())
        .collect::<Vec<_>>();
    let mut seen_scope_names = BTreeSet::new();
    for scope in &scope_names {
        if !seen_scope_names.insert(scope.to_string()) {
            return Err(syn::Error::new(scope.span(), "duplicate scope name"));
        }
    }

    if explicit_scopes && !scope_names.iter().any(|scope| scope == "root") {
        return Err(syn::Error::new(
            item.ident.span(),
            "#[scopes(...)] requires a root scope",
        ));
    }

    let mut assigned = BTreeSet::new();
    let known_scopes = scope_names
        .iter()
        .map(|name| name.to_string())
        .collect::<BTreeSet<_>>();

    for scope in &mut scopes {
        let mut seen_variants = BTreeSet::new();
        for entry in &scope.entries {
            let Some((_, config)) = configs.get(&entry.variant) else {
                return Err(syn::Error::new(
                    entry.variant.span(),
                    "unknown variant in #[scopes(...)]",
                ));
            };
            if config.error {
                return Err(syn::Error::new(
                    entry.variant.span(),
                    "#[error] variants cannot be listed in #[scopes(...)]",
                ));
            }
            if !seen_variants.insert(entry.variant.to_string()) {
                return Err(syn::Error::new(
                    entry.variant.span(),
                    "duplicate variant entry in the same scope",
                ));
            }
            if let Some(child) = &config.enter {
                if !known_scopes.contains(&child.to_string()) {
                    return Err(syn::Error::new(
                        child.span(),
                        "unknown child scope in #[enter(...)]",
                    ));
                }
            }
            assigned.insert(entry.variant.to_string());
        }
    }

    if explicit_scopes {
        for variant in variants {
            let (_, config) = configs.get(&variant.ident).unwrap();
            if config.error {
                continue;
            }
            if !assigned.contains(&variant.ident.to_string()) {
                return Err(syn::Error::new(
                    variant.ident.span(),
                    "every non-error variant must be assigned to at least one scope",
                ));
            }
        }
    }

    Ok(scopes)
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

fn build_scope_specs_fn(
    root_ident: &syn::Ident,
    scope: &ScopeConfig,
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
    configs: &BTreeMap<syn::Ident, (usize, VariantConfig)>,
    builder_idents: &BTreeMap<syn::Ident, syn::Ident>,
) -> syn::Result<proc_macro2::TokenStream> {
    let specs_fn_ident = format_ident!("__plingo_token_specs_for_{}_{}", root_ident, scope.name);
    let mut spec_statements = Vec::new();

    for entry in &scope.entries {
        let (index, config) = configs
            .get(&entry.variant)
            .expect("validated scope variants must exist");
        let variant = variants
            .iter()
            .find(|candidate| candidate.ident == entry.variant)
            .expect("validated scope variants must exist");
        let builder_ident = builder_idents
            .get(&entry.variant)
            .expect("every non-error variant has a builder");
        let spec = build_scope_token_spec(root_ident, variant, *index, config, builder_ident)?;
        spec_statements.push(quote! { specs.push(#spec); });
    }

    Ok(quote! {
        #[allow(non_snake_case)]
        fn #specs_fn_ident() -> ::std::vec::Vec<::plingo::component::lex::__macro_private::TokenSpec<#root_ident>> {
            let mut specs = ::std::vec::Vec::new();
            #(#spec_statements)*
            specs
        }
    })
}

fn build_scope_registration(
    root_ident: &syn::Ident,
    scope: &ScopeConfig,
    _error_builder_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    let specs_fn_ident = format_ident!("__plingo_token_specs_for_{}_{}", root_ident, scope.name);
    let scope_name = &scope.name;
    let type_name = if scope.name == "root" {
        quote! { <#root_ident as ::plingo::component::lex::TokenState>::state_key() }
    } else {
        let state_key = scope_state_key_expr(root_ident, &scope.name);
        quote! { #state_key }
    };

    quote! {
        registrations.push(::plingo::component::lex::__macro_private::ScopeRegistration::new(
            stringify!(#scope_name),
            #type_name,
            #specs_fn_ident,
            error_builder.clone(),
            error_builder.clone(),
        ));
    }
}

fn build_scope_token_spec(
    root_ident: &syn::Ident,
    variant: &Variant,
    index: usize,
    config: &VariantConfig,
    builder_ident: &syn::Ident,
) -> syn::Result<proc_macro2::TokenStream> {
    let matcher = if let Some(regex) = &config.regex {
        quote! { ::plingo::component::lex::__macro_private::TokenMatcher::Regex(#regex) }
    } else if config.empty {
        quote! { ::plingo::component::lex::__macro_private::TokenMatcher::Empty }
    } else {
        return Err(syn::Error::new(
            variant.span(),
            "scope variants require #[regex(...)] or #[empty]",
        ));
    };
    let label = format!("{}::{}", root_ident, variant.ident);
    let when = guard_expr(root_ident, &config.when);
    let recover_when = recover_expr(&config.recover_when);
    let with = with_expr(root_ident, &config.with);
    let terminal = terminal_id_expr(root_ident, index);
    let skip = config.skip;
    let action = if let Some(child) = &config.enter {
        let child_state_key = scope_state_key_expr(root_ident, child);
        quote! {
            ::plingo::component::lex::__macro_private::ScopeDirective::Enter {
                target: #child_state_key.to_string(),
            }
        }
    } else if config.exit {
        quote! { ::plingo::component::lex::__macro_private::ScopeDirective::Exit }
    } else {
        quote! { ::plingo::component::lex::__macro_private::ScopeDirective::None }
    };

    Ok(quote! {
        ::plingo::component::lex::__macro_private::TokenSpec {
            matcher: #matcher,
            terminal: #terminal,
            precedence: #index,
            label: #label,
            action: #action,
            skip: #skip,
            build: ::std::sync::Arc::new(#builder_ident),
            when: #when,
            recover_when: #recover_when,
            with: #with,
        }
    })
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
            quote! { ::std::result::Result::Ok(#root_ident::#variant_ident(info)) }
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

    let Some(regex) = config.regex.as_ref() else {
        return Ok(quote! {
            stringify!(#variant_ident) => ::std::result::Result::Err(
                ::plingo::component::lex::GenerateError::UnsupportedEmptyVariant {
                    token: #label,
                },
            )
        });
    };
    if config.when.is_some() {
        return Ok(quote! {
            stringify!(#variant_ident) => ::std::result::Result::Err(
                ::plingo::component::lex::GenerateError::UnsupportedWhenVariant {
                    token: #label,
                },
            )
        });
    }

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

fn scope_state_key_expr(
    root_ident: &syn::Ident,
    scope_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    if scope_ident == "root" {
        quote! { <#root_ident as ::plingo::component::lex::TokenState>::state_key() }
    } else {
        quote! {
            concat!(
                module_path!(),
                "::",
                stringify!(#root_ident),
                "::scope::",
                stringify!(#scope_ident)
            )
        }
    }
}

fn guard_expr(root_ident: &syn::Ident, predicate: &Option<syn::Expr>) -> proc_macro2::TokenStream {
    match predicate {
        Some(predicate) => quote! {
            ::std::option::Option::Some(
                ::std::sync::Arc::new(
                    #predicate as fn(
                        &::plingo::component::lex::WhenCx<#root_ident>
                    ) -> bool
                )
                    as ::plingo::component::lex::__macro_private::WhenGuard<#root_ident>
            )
        },
        None => quote! { ::std::option::Option::None },
    }
}

fn recover_expr(predicate: &Option<syn::Expr>) -> proc_macro2::TokenStream {
    match predicate {
        Some(predicate) => quote! {
            ::std::option::Option::Some(
                ::std::sync::Arc::new(#predicate as fn(&str, ::std::option::Option<&str>) -> usize)
                    as ::plingo::component::lex::__macro_private::RecoverWhen
            )
        },
        None => quote! { ::std::option::Option::None },
    }
}

fn with_expr(root_ident: &syn::Ident, mapper: &Option<syn::Expr>) -> proc_macro2::TokenStream {
    match mapper {
        Some(mapper) => quote! {
            ::std::option::Option::Some(
                ::std::sync::Arc::new(
                    #mapper as fn(&mut ::plingo::component::lex::WithCx<#root_ident>)
                )
                    as ::plingo::component::lex::__macro_private::WithHook<#root_ident>
            )
        },
        None => quote! { ::std::option::Option::None },
    }
}

fn ensure_lex_error_info(ty: &syn::Type) -> syn::Result<()> {
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
