use std::collections::{BTreeMap, BTreeSet};

use quote::{format_ident, quote};
use syn::{
    Fields, ItemEnum, Token, Variant,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    spanned::Spanned,
};

use crate::shared::{
    VariantConfig, ensure_no_field_parse_attrs, field_parse_expr, parse_variant_config,
};

struct ScopeEntryConfig {
    variant: syn::Ident,
    role: ScopeRoleConfig,
}

enum ScopeRoleConfig {
    Member,
    Enter { child: syn::Ident, key_fn: syn::Expr },
    Exit { guard: syn::Expr },
}

struct ScopeConfig {
    name: syn::Ident,
    entries: Vec<ScopeEntryConfig>,
}

struct ScopesConfig {
    scopes: Vec<ScopeConfig>,
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
        if !input.peek(Token![=>]) {
            return Ok(Self {
                variant,
                role: ScopeRoleConfig::Member,
            });
        }

        input.parse::<Token![=>]>()?;
        let role_name = input.parse::<syn::Ident>()?;
        let content;
        syn::parenthesized!(content in input);

        let role = if role_name == "enter" {
            let child = content.parse::<syn::Ident>()?;
            content.parse::<Token![,]>()?;
            let key_fn = content.parse::<syn::Expr>()?;
            ScopeRoleConfig::Enter { child, key_fn }
        } else if role_name == "exit" {
            let guard = content.parse::<syn::Expr>()?;
            ScopeRoleConfig::Exit { guard }
        } else {
            return Err(syn::Error::new(
                role_name.span(),
                "scope entry roles must be enter(...) or exit(...)",
            ));
        };

        if !content.is_empty() {
            return Err(content.error("unexpected tokens in scope entry"));
        }

        Ok(Self { variant, role })
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
            fn state_registrations() -> ::std::vec::Vec<::plingo::component::lex::__macro_private::ScopeRegistration<Self>> {
                let error_builder = ::std::sync::Arc::new(#error_builder_ident)
                    as ::plingo::component::lex::__macro_private::BuildErrorToken<Self>;
                let mut registrations = ::std::vec::Vec::new();
                #(#scope_regs)*
                registrations
            }
        }

        #(#builder_tokens)*
        #error_builder_fn
        #(#scope_specs)*
        #generate_impl
        #parser_terminal_impl
    }
    .into())
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
                        role: ScopeRoleConfig::Member,
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
            if config.skip && !matches!(entry.role, ScopeRoleConfig::Member) {
                return Err(syn::Error::new(
                    entry.variant.span(),
                    "#[skip] variants can only appear as plain scope members",
                ));
            }
            if let ScopeRoleConfig::Enter { child, .. } = &entry.role {
                if !known_scopes.contains(&child.to_string()) {
                    return Err(syn::Error::new(
                        child.span(),
                        "unknown child scope in enter(...)",
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
    let specs_fn_ident = format_ident!(
        "__plingo_token_specs_for_{}_{}",
        root_ident,
        scope.name
    );
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
        let spec = build_scope_token_spec(
            root_ident,
            variant,
            *index,
            config,
            builder_ident,
            &entry.role,
        )?;
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
    let specs_fn_ident = format_ident!(
        "__plingo_token_specs_for_{}_{}",
        root_ident,
        scope.name
    );
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
    role: &ScopeRoleConfig,
) -> syn::Result<proc_macro2::TokenStream> {
    let regex = config.regex.as_ref().ok_or_else(|| {
        syn::Error::new(
            variant.span(),
            "only #[regex(...)] variants can appear in scopes",
        )
    })?;
    let label = format!("{}::{}", root_ident, variant.ident);
    let when = guard_expr(&config.when);
    let recover_when = recover_expr(&config.recover_when);
    let terminal = terminal_id_expr(root_ident, index);
    let skip = config.skip;
    let action = match role {
        ScopeRoleConfig::Member => {
            quote! { ::plingo::component::lex::__macro_private::ScopeDirective::None }
        }
        ScopeRoleConfig::Enter { child, key_fn } => {
            let child_state_key = scope_state_key_expr(root_ident, child);
            quote! {
                ::plingo::component::lex::__macro_private::ScopeDirective::Enter {
                    target: #child_state_key.to_string(),
                    key: ::std::sync::Arc::new(#key_fn as fn(&#root_ident) -> ::std::option::Option<::std::string::String>)
                        as ::plingo::component::lex::__macro_private::EnterScopeKey<#root_ident>,
                }
            }
        }
        ScopeRoleConfig::Exit { guard } => {
            quote! {
                ::plingo::component::lex::__macro_private::ScopeDirective::Leave {
                    matches: ::std::sync::Arc::new(#guard as fn(&#root_ident, &str) -> bool)
                        as ::plingo::component::lex::__macro_private::ExitScopeGuard<#root_ident>,
                }
            }
        }
    };

    Ok(quote! {
        ::plingo::component::lex::__macro_private::TokenSpec {
            regex: #regex,
            terminal: #terminal,
            precedence: #index,
            label: #label,
            action: #action,
            skip: #skip,
            build: ::std::sync::Arc::new(#builder_ident),
            when: #when,
            recover_when: #recover_when,
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
    let regex = config.regex.as_ref().expect("non-error variants have #[regex(...)]");

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

fn guard_expr(predicate: &Option<syn::Expr>) -> proc_macro2::TokenStream {
    match predicate {
        Some(predicate) => quote! {
            ::std::option::Option::Some(
                ::std::sync::Arc::new(#predicate as fn(&str, ::std::option::Option<&str>) -> bool)
                    as ::plingo::component::lex::__macro_private::WhenGuard
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
