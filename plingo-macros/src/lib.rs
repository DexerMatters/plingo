use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Expr, Field, Fields, Ident, ItemEnum, ItemImpl, ItemStruct, Meta, Type, Variant,
    parse::{Parse, ParseStream},
    parse_macro_input, parse_quote,
    spanned::Spanned,
};

// ---------------------------------------------------------------------------
// Layer attribute — two forms:
//   #[layer]             on struct  → snapshot machinery only
//   #[layer(top|middle|bottom)]  on impl  → conduit impls only
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
    if attr.is_empty() {
        expand_layer_struct(item)
    } else {
        expand_layer_impl(attr, item)
    }
}

// ---- #[layer] on struct: snapshot machinery ----

fn expand_layer_struct(item: TokenStream) -> TokenStream {
    let mut item_struct = parse_macro_input!(item as ItemStruct);
    let self_ident = item_struct.ident.clone();
    let generics = item_struct.generics.clone();
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let fields = match &mut item_struct.fields {
        Fields::Named(fields) => &mut fields.named,
        _ => {
            return syn::Error::new(
                item_struct.span(),
                "#[layer] on structs currently requires named fields",
            )
            .to_compile_error()
            .into();
        }
    };

    let mut snapshot_ty = None;
    let mut snapshot_field_ident = None;
    for field in fields.iter_mut() {
        let field_span = field.span();
        let mut keep_attrs = Vec::new();
        for attr in field.attrs.drain(..) {
            if attr.path().is_ident("snapshot") {
                if snapshot_ty.is_some() {
                    return syn::Error::new(attr.span(), "only one #[snapshot] field is supported")
                        .to_compile_error()
                        .into();
                }
                let Some(field_ident) = field.ident.clone() else {
                    return syn::Error::new(field_span, "#[snapshot] requires a named field")
                        .to_compile_error()
                        .into();
                };
                snapshot_ty = Some(field.ty.clone());
                snapshot_field_ident = Some(field_ident);
            } else {
                keep_attrs.push(attr);
            }
        }
        field.attrs = keep_attrs;
    }

    let has_reserved_name = fields.iter().any(|field| {
        field.ident.as_ref().is_some_and(|ident| ident == "_snapshot")
    });
    if has_reserved_name {
        return syn::Error::new(
            item_struct.span(),
            "layer structs cannot define a field named _snapshot",
        )
        .to_compile_error()
        .into();
    }

    if let Some(ref snapshot_ty) = snapshot_ty {
        fields.push(parse_quote! {
            _snapshot: ::std::collections::HashMap<::plingo::scheme::SnapshotId, #snapshot_ty>
        });
    }

    let snapshot_impl = match (snapshot_field_ident, snapshot_ty.as_ref()) {
        (Some(field_ident), Some(snapshot_ty)) => quote! {
            impl #impl_generics ::plingo::scheme::SnapshotLayer for #self_ident #ty_generics #where_clause {
                type State = #snapshot_ty;

                fn push_state(&mut self, snapshot: ::plingo::scheme::SnapshotId) {
                    self._snapshot.insert(snapshot, self.#field_ident.clone());
                }

                fn state(
                    &self,
                    snapshot: ::std::option::Option<::plingo::scheme::SnapshotId>,
                ) -> ::std::option::Option<&Self::State> {
                    match snapshot {
                        Some(snapshot) => self._snapshot.get(&snapshot),
                        None => Some(&self.#field_ident),
                    }
                }

                fn latest_state(&self) -> &Self::State {
                    &self.#field_ident
                }

                fn latest_state_mut(&mut self) -> &mut Self::State {
                    &mut self.#field_ident
                }
            }
        },
        _ => quote! {},
    };

    quote! {
        #item_struct

        #snapshot_impl
    }
    .into()
}

// ---- #[layer(top|middle|bottom)] on impl: conduit impls ----

fn expand_layer_impl(attr: TokenStream, item: TokenStream) -> TokenStream {
    let role = parse_macro_input!(attr as LayerRole);
    let item_impl = parse_macro_input!(item as ItemImpl);

    let self_type = match item_impl.self_ty.as_ref() {
        Type::Path(path) => &path.path,
        _ => {
            return syn::Error::new(
                item_impl.self_ty.span(),
                "#[layer(role)] requires a struct or type name as the impl target",
            )
            .to_compile_error()
            .into();
        }
    };

    let (impl_generics, _ty_generics, where_clause) = item_impl.generics.split_for_impl();

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
            "#[layer(role)] can only be used on trait impl blocks",
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
                type _Value = <Self as ::plingo::scheme::MiddleLayer>::Value;
            }
        },
        LayerRole::Bottom => quote! {
            impl #impl_generics ::plingo::scheme::FallibleLayer for #self_type #where_clause {
                type __Error = <Self as ::plingo::scheme::BottomLayer>::Error;
            }

            impl #impl_generics ::plingo::scheme::NonTopLayer for #self_type #where_clause {
                type _Key = <Self as ::plingo::scheme::BottomLayer>::Key;
                type _Error = <Self as ::plingo::scheme::BottomLayer>::Error;
                type _Value = <Self as ::plingo::scheme::BottomLayer>::Value;
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
// Tokens attribute: #[tokens]
// ---------------------------------------------------------------------------

#[proc_macro_attribute]
pub fn tokens(_attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_tokens_attr(parse_macro_input!(item as ItemEnum)) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_tokens_attr(mut item: ItemEnum) -> syn::Result<TokenStream> {
    ensure_tokens_derives(&mut item)?;
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
    }
    .into())
}

fn ensure_tokens_derives(item: &mut ItemEnum) -> syn::Result<()> {
    let mut derive_index = None;
    let mut seen = std::collections::HashSet::new();

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
                seen.insert(ident.to_string());
            }
            Ok(())
        })?;
    }

    if let Some(index) = derive_index {
        let mut derives = Vec::<Ident>::new();
        item.attrs[index].parse_nested_meta(|nested| {
            if let Some(ident) = nested.path.get_ident() {
                derives.push(ident.clone());
            }
            Ok(())
        })?;

        for required in ["PartialEq", "Eq", "Hash"] {
            if !derives.iter().any(|ident| ident == required) {
                derives.push(format_ident!("{}", required));
            }
        }

        item.attrs[index] = parse_quote!(#[derive(#(#derives),*)]);
    } else {
        item.attrs
            .push(parse_quote!(#[derive(PartialEq, Eq, Hash)]));
    }

    Ok(())
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
        wrappers.push(parse_quote! {
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

fn target_last_ident(target: &Type) -> syn::Result<Ident> {
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

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

struct VariantConfig {
    regex: syn::LitStr,
    enter: Option<Type>,
    leave: bool,
    skip: bool,
    validate: Option<syn::Expr>,
}

fn parse_variant_config(variant: &syn::Variant) -> syn::Result<VariantConfig> {
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
