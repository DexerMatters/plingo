use std::collections::{BTreeMap, BTreeSet};

use quote::{format_ident, quote};
use syn::{
    Field, Fields, Ident, ItemEnum, LitInt, Type, TypePath, Variant,
    parse::{Parse, ParseStream, discouraged::Speculative},
    spanned::Spanned,
};

use crate::shared::push_missing_derives;

pub fn expand_non_terminal_derive(mut item: ItemEnum) -> syn::Result<proc_macro::TokenStream> {
    push_missing_derives(&mut item, &["Debug"])?;
    let enum_ident = item.ident.clone();
    let variants = item.variants.clone();
    strip_non_terminal_attrs(&mut item);

    let register_fn = format_ident!("__plingo_register_non_terminal_{}", enum_ident);
    let mut rules = variants
        .iter()
        .map(parse_rule_spec)
        .collect::<syn::Result<Vec<_>>>()?;
    let tier_plan = TierPlan::build(&enum_ident, &variants, &mut rules)?;

    let mut builders = Vec::new();
    let mut registrations = Vec::new();
    for ((variant_index, variant), rule) in variants.iter().enumerate().zip(&rules) {
        let lhs = tier_plan
            .as_ref()
            .and_then(|plan| rule.output_level.map(|level| plan.symbol(level)))
            .map(|symbol| quote! { #symbol })
            .unwrap_or_else(|| quote! { lhs });
        let lowered = LowerCtx::new(
            &enum_ident,
            variant_index,
            tier_plan.as_ref().map(|plan| &plan.symbols),
        )
        .lower_variant(variant, &rule.expr, lhs)?;
        builders.extend(lowered.builders);
        registrations.extend(lowered.registrations);
    }

    let tier_lowering = tier_plan
        .as_ref()
        .map(|plan| plan.lowering(&enum_ident))
        .transpose()?;
    let tier_builders = tier_lowering
        .as_ref()
        .map(|lowering| lowering.builders.as_slice())
        .unwrap_or_default();
    let tier_bindings = tier_lowering
        .as_ref()
        .map(|lowering| lowering.bindings.as_slice())
        .unwrap_or_default();
    let tier_promotions = tier_lowering
        .as_ref()
        .map(|lowering| lowering.promotions.as_slice())
        .unwrap_or_default();

    Ok(quote! {
        impl ::plingo::framework::parse::__macro_private::NonTerminalSpec for #enum_ident {
            fn register(grammar: &mut ::plingo::framework::parse::grammar::GrammarBuilder) -> ::plingo::framework::parse::grammar::Symbol {
                #register_fn(grammar)
            }
        }


        #[allow(non_snake_case)]
        fn #register_fn(
            grammar: &mut ::plingo::framework::parse::grammar::GrammarBuilder,
        ) -> ::plingo::framework::parse::grammar::Symbol {
            let (lhs, fresh) = ::plingo::framework::parse::__macro_private::begin_non_terminal(grammar, stringify!(#enum_ident));
            if !fresh {
                return lhs;
            }
            #(#tier_builders)*
            #(#tier_bindings)*
            #(#builders)*
            #(#registrations)*
            #(#tier_promotions)*
            lhs
        }
    }
    .into())
}

fn strip_non_terminal_attrs(item: &mut ItemEnum) {
    for variant in &mut item.variants {
        variant
            .attrs
            .retain(|attr| !attr.path().is_ident("rule") && !attr.path().is_ident("parse_err"));
        for field in &mut variant.fields {
            field.attrs.retain(|attr| !attr.path().is_ident("from"));
        }
    }
}

struct LoweredVariant {
    builders: Vec<proc_macro2::TokenStream>,
    registrations: Vec<proc_macro2::TokenStream>,
}

struct TierLowering {
    builders: Vec<proc_macro2::TokenStream>,
    bindings: Vec<proc_macro2::TokenStream>,
    promotions: Vec<proc_macro2::TokenStream>,
}

/// One generated nonterminal for each declared binding-power level.  Lower
/// numbers are less binding; promotions only move from a looser level to the
/// next tighter level.  Consequently a rule can only recurse through the
/// levels it explicitly names.
struct TierPlan {
    levels: Vec<u32>,
    symbols: BTreeMap<u32, Ident>,
}

impl TierPlan {
    fn build(
        enum_ident: &Ident,
        variants: &syn::punctuated::Punctuated<Variant, syn::Token![,]>,
        rules: &mut [RuleSpec],
    ) -> syn::Result<Option<Self>> {
        let tiered = rules
            .iter()
            .any(|rule| rule.output.is_some() || rule.expr.contains_tiered_non_terminal());
        if !tiered {
            return Ok(None);
        }

        let mut levels = BTreeSet::new();
        for (variant, rule) in variants.iter().zip(rules.iter_mut()) {
            if let Some(output) = &rule.output {
                if !is_current_non_terminal(&output.ty, enum_ident) {
                    return Err(syn::Error::new(
                        output.ty.span(),
                        format!(
                            "tiered rule output must name the current nonterminal `{enum_ident}`"
                        ),
                    ));
                }
                levels.insert(output.level);
            }
            rule.expr
                .collect_and_validate_tiers(enum_ident, &mut levels)?;

            if rule.output.is_none() && rule.expr.contains_tiered_non_terminal() {
                let Some(level) = rule.expr.leading_tier() else {
                    return Err(syn::Error::new(
                        variant.span(),
                        format!(
                            "a tiered rule must begin with `{enum_ident}:n`, or use the explicit `{enum_ident}:n <- ...` form"
                        ),
                    ));
                };
                rule.output_level = Some(level);
            } else if let Some(output) = &rule.output {
                rule.output_level = Some(output.level);
            }
        }

        let Some(tightest) = levels.last().copied() else {
            return Err(syn::Error::new(
                enum_ident.span(),
                "a tiered nonterminal requires at least one `Nonterminal:n` binding-power annotation",
            ));
        };
        for rule in rules {
            rule.output_level.get_or_insert(tightest);
        }

        let levels = levels.into_iter().collect::<Vec<_>>();
        let symbols = levels
            .iter()
            .map(|level| {
                (
                    *level,
                    format_ident!("__plingo_tier_{}_{}", enum_ident, level),
                )
            })
            .collect();
        Ok(Some(Self { levels, symbols }))
    }

    fn symbol(&self, level: u32) -> &Ident {
        &self.symbols[&level]
    }

    fn lowering(&self, enum_ident: &Ident) -> syn::Result<TierLowering> {
        let passthrough = format_ident!("__plingo_build_{}_tier_passthrough", enum_ident);
        let bindings = self
            .levels
            .iter()
            .map(|level| {
                let symbol = self.symbol(*level);
                let label = syn::LitStr::new(
                    &format!("{enum_ident}::__tier_{level}"),
                    enum_ident.span(),
                );
                quote! {
                    let #symbol = ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #label);
                }
            })
            .collect();

        let root_label = syn::LitStr::new(&format!("{enum_ident}::__tier_root"), enum_ident.span());
        let first = self.symbol(self.levels[0]);
        let mut promotions = vec![quote! {
            ::plingo::framework::parse::__macro_private::rule(
                grammar,
                #root_label,
                lhs,
                ::std::vec![#first],
                ::std::option::Option::None,
                ::std::option::Option::Some(#passthrough),
            );
        }];
        for levels in self.levels.windows(2) {
            let lower = self.symbol(levels[0]);
            let tighter = self.symbol(levels[1]);
            let label = syn::LitStr::new(
                &format!(
                    "{enum_ident}::__tier_promotion_{}_to_{}",
                    levels[0], levels[1]
                ),
                enum_ident.span(),
            );
            promotions.push(quote! {
                ::plingo::framework::parse::__macro_private::rule(
                    grammar,
                    #label,
                    #lower,
                    ::std::vec![#tighter],
                    ::std::option::Option::None,
                    ::std::option::Option::Some(#passthrough),
                );
            });
        }

        Ok(TierLowering {
            builders: vec![quote! {
                fn #passthrough(
                    _: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                    _: ::plingo::framework::parse::grammar::ProductionId,
                    children: &[::plingo::framework::parse::data::ProductId],
                ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                    ::plingo::framework::parse::__macro_private::production_child(children, 0)
                }
            }],
            bindings,
            promotions,
        })
    }
}

struct LowerCtx<'a> {
    enum_ident: &'a Ident,
    variant_index: usize,
    synthetic_index: usize,
    tier_symbols: Option<&'a BTreeMap<u32, Ident>>,
}

impl<'a> LowerCtx<'a> {
    fn new(
        enum_ident: &'a Ident,
        variant_index: usize,
        tier_symbols: Option<&'a BTreeMap<u32, Ident>>,
    ) -> Self {
        Self {
            enum_ident,
            variant_index,
            synthetic_index: 0,
            tier_symbols,
        }
    }

    fn lower_variant(
        mut self,
        variant: &Variant,
        rule: &RuleExpr,
        lhs: proc_macro2::TokenStream,
    ) -> syn::Result<LoweredVariant> {
        let enum_ident = self.enum_ident;
        let builder_ident = format_ident!("__plingo_build_{}_{}", enum_ident, self.variant_index);
        let variant_ident = &variant.ident;
        let label = format!("{}::{}", enum_ident, variant_ident);
        let mut builders = Vec::new();
        let mut registrations = Vec::new();
        let lowered = self.lower_expr(rule, &mut builders, &mut registrations)?;
        let rhs_exprs = lowered.symbol_exprs;
        let mut named_captures = Vec::new();
        if let Some(name) = &lowered.name {
            named_captures.push((name.clone(), 0usize));
        }
        for (name, index, _kind) in &lowered.captures {
            named_captures.push((name.clone(), *index));
        }
        let field_exprs = build_variant_field_exprs(&variant.fields, &rhs_exprs, &named_captures)?;
        let ctor = match &variant.fields {
            Fields::Unit => quote! { #enum_ident::#variant_ident },
            Fields::Unnamed(_) => quote! { #enum_ident::#variant_ident(#(#field_exprs),*) },
            Fields::Named(fields) => {
                let field_idents = fields
                    .named
                    .iter()
                    .map(|field| field.ident.as_ref().unwrap());
                quote! { #enum_ident::#variant_ident { #(#field_idents: #field_exprs),* } }
            }
        };

        builders.push(quote! {
            #[allow(non_snake_case)]
            fn #builder_ident(
                cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                production: ::plingo::framework::parse::grammar::ProductionId,
                children: &[::plingo::framework::parse::data::ProductId],
            ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                let value = #ctor;
                ::plingo::framework::parse::__macro_private::production_node(cx, production, children, value)
            }
        });

        let rhs_bindings = rhs_exprs
            .iter()
            .enumerate()
            .map(|(rule_index, expr)| {
                let ident =
                    format_ident!("__plingo_rule_symbol_{}_{}", self.variant_index, rule_index);
                quote! {
                    let #ident = #expr;
                }
            })
            .collect::<Vec<_>>();
        let rhs_idents = (0..rhs_exprs.len())
            .map(|rule_index| {
                format_ident!("__plingo_rule_symbol_{}_{}", self.variant_index, rule_index)
            })
            .collect::<Vec<_>>();

        registrations.push(quote! {
            #(#rhs_bindings)*
            ::plingo::framework::parse::__macro_private::rule(grammar,
                #label,
                #lhs,
                ::std::vec![#(#rhs_idents),*],
                ::std::option::Option::None,
                ::std::option::Option::Some(#builder_ident),
            );
        });

        Ok(LoweredVariant {
            builders,
            registrations,
        })
    }

    fn lower_expr(
        &mut self,
        expr: &RuleExpr,
        builders: &mut Vec<proc_macro2::TokenStream>,
        registrations: &mut Vec<proc_macro2::TokenStream>,
    ) -> syn::Result<LoweredExpr> {
        match expr {
            RuleExpr::Empty => Ok(LoweredExpr::new(Vec::new(), ValueKind::Unit)),
            RuleExpr::Atom(atom) => Ok(LoweredExpr::new(
                vec![self.atom_symbol_expr(atom)?],
                atom_value_type(atom)?,
            )),
            RuleExpr::Seq(items) => {
                let mut symbol_exprs = Vec::new();
                let mut value_types = Vec::new();
                let mut captures = Vec::new();
                for item in items {
                    let lowered = self.lower_expr(item, builders, registrations)?;
                    let symbol_offset = symbol_exprs.len();
                    if let Some(name) = lowered.name {
                        captures.push((name, symbol_offset, lowered.value_kind.clone()));
                    }
                    captures.extend(lowered.captures);
                    symbol_exprs.extend(lowered.symbol_exprs);
                    value_types.push(lowered.value_kind);
                }
                let value_type = if value_types.is_empty() {
                    ValueKind::Unit
                } else if value_types.len() == 1 {
                    value_types.remove(0)
                } else {
                    ValueKind::Tuple(value_types)
                };
                Ok(LoweredExpr {
                    symbol_exprs,
                    value_kind: value_type,
                    name: None,
                    captures,
                })
            }
            RuleExpr::Named(name, inner) => {
                let mut lowered = self.lower_expr(inner, builders, registrations)?;
                lowered.name = Some(name.clone());
                Ok(lowered)
            }
            RuleExpr::Optional(inner) => {
                let lowered = self.lower_expr(inner, builders, registrations)?;
                // An optional sequence with exactly one named child projects
                // that child rather than its punctuation-bearing tuple. Thus
                // `[Token::Colon, $annotation(Type)]` has the concrete value
                // `Option<AstBox<Type>>`, not `Option<(AstToken<_>, AstBox<Type>)>`.
                let projected = lowered
                    .name
                    .as_ref()
                    .map(|name| (name.clone(), 0usize, lowered.value_kind.clone()))
                    .or_else(|| (lowered.captures.len() == 1).then(|| lowered.captures[0].clone()));
                let capture_name = projected.as_ref().map(|(name, _, _)| name.clone());
                // The wrapper owns one RHS symbol; nested capture coordinates
                // refer to its internal production and cannot escape directly.
                let captures = Vec::new();
                let synthetic = self.synthetic_name("opt");
                let builder_none = self.synthetic_builder("opt_none");
                let builder_some = self.synthetic_builder("opt_some");
                let inner_kind = projected
                    .as_ref()
                    .map(|(_, _, kind)| kind.clone())
                    .unwrap_or_else(|| lowered.value_kind.clone());
                let inner_type = inner_kind.ty_tokens();
                let inner_extract = if let Some((_, index, kind)) = &projected {
                    kind.extract_expr(quote! {
                        ::plingo::framework::parse::__macro_private::production_child(children, #index)?
                    })
                } else {
                    inner_kind.extract_expr(
                        quote! { ::plingo::framework::parse::__macro_private::production_child(children, 0)? },
                    )
                };
                let inner_symbols = lowered.symbol_exprs;
                let inner_bindings = inner_symbols
                    .iter()
                    .enumerate()
                    .map(|(rule_index, expr)| {
                        let ident = format_ident!(
                            "__plingo_opt_symbol_{}_{}",
                            self.variant_index,
                            rule_index
                        );
                        quote! {
                            let #ident = #expr;
                        }
                    })
                    .collect::<Vec<_>>();
                let inner_idents = (0..inner_symbols.len())
                    .map(|rule_index| {
                        format_ident!("__plingo_opt_symbol_{}_{}", self.variant_index, rule_index)
                    })
                    .collect::<Vec<_>>();

                builders.push(quote! {
                    fn #builder_none(
                        cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                        production: ::plingo::framework::parse::grammar::ProductionId,
                        children: &[::plingo::framework::parse::data::ProductId],
                    ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                        ::plingo::framework::parse::__macro_private::production_node(
                            cx,
                            production,
                            children,
                            ::std::option::Option::<#inner_type>::None,
                        )
                    }
                });
                builders.push(quote! {
                    fn #builder_some(
                        cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                        production: ::plingo::framework::parse::grammar::ProductionId,
                        children: &[::plingo::framework::parse::data::ProductId],
                    ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                        let value: #inner_type = #inner_extract;
                        ::plingo::framework::parse::__macro_private::production_node(
                            cx,
                            production,
                            children,
                            ::std::option::Option::Some(value),
                        )
                    }
                });

                registrations.push(quote! {
                    let opt_lhs = ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #synthetic);
                    #(#inner_bindings)*
                    ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, opt_lhs, ::std::vec![], ::std::option::Option::None, ::std::option::Option::Some(#builder_none));
                    ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, opt_lhs, ::std::vec![#(#inner_idents),*], ::std::option::Option::None, ::std::option::Option::Some(#builder_some));
                });

                Ok(LoweredExpr {
                    symbol_exprs: vec![
                        quote! { ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #synthetic) },
                    ],
                    value_kind: ValueKind::Option(Box::new(inner_kind)),
                    name: capture_name,
                    captures,
                })
            }
            RuleExpr::Repeat(inner, separator, bounds) => {
                let lowered = self.lower_expr(inner, builders, registrations)?;
                let capture_name = lowered.name.clone();
                let captures = lowered.captures;
                let synthetic = self.synthetic_name("rep");
                let item_kind = lowered.value_kind.clone();
                let item_type = item_kind.ty_tokens();
                let inner_symbols = lowered.symbol_exprs.clone();
                let item_stride = inner_symbols.len();
                let (min, max) = bounds.unwrap_or((0, None));
                let sep_symbols: Vec<_> = if let Some(sep) = separator.as_ref() {
                    let sep_lowered = self.lower_expr(sep, builders, registrations)?;
                    sep_lowered.symbol_exprs.clone()
                } else {
                    Vec::new()
                };
                let has_sep = !sep_symbols.is_empty();

                if max.is_none() && min == 0 {
                    let builder_empty = self.synthetic_builder("rep_empty");
                    builders.push(quote! {
                        fn #builder_empty(
                            cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                            production: ::plingo::framework::parse::grammar::ProductionId,
                            children: &[::plingo::framework::parse::data::ProductId],
                        ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                            ::plingo::framework::parse::__macro_private::production_node(
                                cx,
                                production,
                                children,
                                ::plingo::framework::parse::__macro_private::repeat_empty::<#item_type>(),
                            )
                        }
                    });

                    if has_sep {
                        let tail_synthetic = self.synthetic_name("rep_tail");
                        let builder_item = self.synthetic_builder("rep_item");
                        let builder_tail = self.synthetic_builder("rep_tail_more");
                        let item_bindings = inner_symbols
                            .iter()
                            .enumerate()
                            .map(|(rule_index, expr)| {
                                let ident = format_ident!(
                                    "__plingo_rep_item_symbol_{}_{}_{}",
                                    self.variant_index,
                                    0usize,
                                    rule_index
                                );
                                quote! { let #ident = #expr; }
                            })
                            .collect::<Vec<_>>();
                        let item_idents = (0..inner_symbols.len())
                            .map(|rule_index| {
                                format_ident!(
                                    "__plingo_rep_item_symbol_{}_{}_{}",
                                    self.variant_index,
                                    0usize,
                                    rule_index
                                )
                            })
                            .collect::<Vec<_>>();
                        let tail_seq: Vec<_> = ::std::iter::once(quote! { tail_lhs })
                            .chain(sep_symbols.iter().cloned())
                            .chain(inner_symbols.iter().cloned())
                            .collect();
                        let tail_bindings = tail_seq
                            .iter()
                            .enumerate()
                            .map(|(rule_index, expr)| {
                                let ident = format_ident!(
                                    "__plingo_rep_tail_symbol_{}_{}_{}",
                                    self.variant_index,
                                    0usize,
                                    rule_index
                                );
                                quote! { let #ident = #expr; }
                            })
                            .collect::<Vec<_>>();
                        let tail_idents = (0..tail_seq.len())
                            .map(|rule_index| {
                                format_ident!(
                                    "__plingo_rep_tail_symbol_{}_{}_{}",
                                    self.variant_index,
                                    0usize,
                                    rule_index
                                )
                            })
                            .collect::<Vec<_>>();
                        let item_extract = self.repeat_item_extract_expr(&item_kind, 0);
                        let tail_extract =
                            self.repeat_item_extract_expr(&item_kind, 1 + sep_symbols.len());
                        builders.push(quote! {
                            fn #builder_item(
                                cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                                production: ::plingo::framework::parse::grammar::ProductionId,
                                children: &[::plingo::framework::parse::data::ProductId],
                            ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                                let item: #item_type = #item_extract;
                                let tail: ::plingo::framework::parse::__macro_private::Repeat<#item_type> = cx.expect_value(
                                    ::plingo::framework::parse::__macro_private::production_child(children, #item_stride)?,
                                )?;
                                let value = ::plingo::framework::parse::__macro_private::repeat_prepend(item, tail);
                                ::plingo::framework::parse::__macro_private::production_node(cx, production, children, value)
                            }
                        });
                        builders.push(quote! {
                            fn #builder_tail(
                                cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                                production: ::plingo::framework::parse::grammar::ProductionId,
                                children: &[::plingo::framework::parse::data::ProductId],
                            ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                                let value: ::plingo::framework::parse::__macro_private::Repeat<#item_type> = cx.expect_value(
                                    ::plingo::framework::parse::__macro_private::production_child(children, 0)?,
                                )?;
                                let value = ::plingo::framework::parse::__macro_private::repeat_push(value, #tail_extract);
                                ::plingo::framework::parse::__macro_private::production_node(cx, production, children, value)
                            }
                        });
                        registrations.push(quote! {
                            let rep_lhs = ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #synthetic);
                            let tail_lhs = ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #tail_synthetic);
                            #(#item_bindings)*
                            ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, rep_lhs, ::std::vec![#(#item_idents),*, tail_lhs], ::std::option::Option::None, ::std::option::Option::Some(#builder_item));
                            #(#tail_bindings)*
                            ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, tail_lhs, ::std::vec![#(#tail_idents),*], ::std::option::Option::None, ::std::option::Option::Some(#builder_tail));
                            ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, tail_lhs, ::std::vec![], ::std::option::Option::None, ::std::option::Option::Some(#builder_empty));
                        });
                    } else {
                        let item_bindings = inner_symbols
                            .iter()
                            .enumerate()
                            .map(|(rule_index, expr)| {
                                let ident = format_ident!(
                                    "__plingo_rep_item_symbol_{}_{}_{}",
                                    self.variant_index,
                                    0usize,
                                    rule_index
                                );
                                quote! { let #ident = #expr; }
                            })
                            .collect::<Vec<_>>();
                        let item_idents = (0..inner_symbols.len())
                            .map(|rule_index| {
                                format_ident!(
                                    "__plingo_rep_item_symbol_{}_{}_{}",
                                    self.variant_index,
                                    0usize,
                                    rule_index
                                )
                            })
                            .collect::<Vec<_>>();
                        let item_extract = self.repeat_item_extract_expr(&item_kind, 1);
                        let builder_item = self.synthetic_builder("rep_item");
                        builders.push(quote! {
                            fn #builder_item(
                                cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                                production: ::plingo::framework::parse::grammar::ProductionId,
                                children: &[::plingo::framework::parse::data::ProductId],
                            ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                                let value: ::plingo::framework::parse::__macro_private::Repeat<#item_type> = cx.expect_value(
                                    ::plingo::framework::parse::__macro_private::production_child(children, 0)?,
                                )?;
                                let value = ::plingo::framework::parse::__macro_private::repeat_push(value, #item_extract);
                                ::plingo::framework::parse::__macro_private::production_node(cx, production, children, value)
                            }
                        });
                        registrations.push(quote! {
                            let rep_lhs = ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #synthetic);
                            #(#item_bindings)*
                            ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, rep_lhs, ::std::vec![rep_lhs, #(#item_idents),*], ::std::option::Option::None, ::std::option::Option::Some(#builder_item));
                            ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, rep_lhs, ::std::vec![], ::std::option::Option::None, ::std::option::Option::Some(#builder_empty));
                        });
                    }

                    return Ok(LoweredExpr {
                        symbol_exprs: vec![
                            quote! { ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #synthetic) },
                        ],
                        value_kind: ValueKind::Repeat(Box::new(item_kind)),
                        name: capture_name,
                        captures,
                    });
                }

                let sep_stride = sep_symbols.len();

                if min == 0 {
                    let builder_empty = self.synthetic_builder("rep_empty");
                    builders.push(quote! {
                        fn #builder_empty(
                            cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                            production: ::plingo::framework::parse::grammar::ProductionId,
                            children: &[::plingo::framework::parse::data::ProductId],
                        ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                            ::plingo::framework::parse::__macro_private::production_node(
                                cx,
                                production,
                                children,
                                ::std::vec::Vec::<#item_type>::new(),
                            )
                        }
                    });
                    registrations.push(quote! {
                        let rep_lhs = ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #synthetic);
                        ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, rep_lhs, ::std::vec![], ::std::option::Option::None, ::std::option::Option::Some(#builder_empty));
                    });
                } else {
                    registrations.push(quote! {
                        let rep_lhs = ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #synthetic);
                    });
                }

                let upper = max.unwrap_or(min + 3);
                for count in min.max(1)..=upper {
                    let builder = self.synthetic_builder(&format!("rep_{}", count));
                    let seq: Vec<_> = if has_sep {
                        let mut s = Vec::new();
                        s.extend(inner_symbols.clone());
                        for _ in 1..count {
                            s.extend(sep_symbols.clone());
                            s.extend(inner_symbols.clone());
                        }
                        s
                    } else {
                        (0..count).flat_map(|_| inner_symbols.clone()).collect()
                    };
                    let seq_bindings = seq
                        .iter()
                        .enumerate()
                        .map(|(rule_index, expr)| {
                            let ident = format_ident!(
                                "__plingo_rep_symbol_{}_{}_{}",
                                self.variant_index,
                                count,
                                rule_index
                            );
                            quote! { let #ident = #expr; }
                        })
                        .collect::<Vec<_>>();
                    let seq_idents = (0..seq.len())
                        .map(|rule_index| {
                            format_ident!(
                                "__plingo_rep_symbol_{}_{}_{}",
                                self.variant_index,
                                count,
                                rule_index
                            )
                        })
                        .collect::<Vec<_>>();
                    let stride = if has_sep {
                        item_stride + sep_stride
                    } else {
                        item_stride
                    };
                    let indexes = (0..count).collect::<Vec<_>>();
                    let pushes = indexes
                        .iter()
                        .map(|index| {
                            let base = index * stride;
                            match &item_kind {
                                ValueKind::Tuple(kinds) => {
                                    let parts: Vec<_> = kinds.iter().enumerate().map(|(i, k)| {
                                        let offset = base + i;
                                        k.extract_expr(quote! {
                                            ::plingo::framework::parse::__macro_private::production_child(children, #offset)?
                                        })
                                    }).collect();
                                    quote! { value.push((#(#parts),*)); }
                                }
                                _ => {
                                    let extract = item_kind.extract_expr(quote! {
                                        ::plingo::framework::parse::__macro_private::production_child(children, #base)?
                                    });
                                    quote! { value.push(#extract); }
                                }
                            }
                        })
                        .collect::<Vec<_>>();
                    builders.push(quote! {
                        fn #builder(
                            cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                            production: ::plingo::framework::parse::grammar::ProductionId,
                            children: &[::plingo::framework::parse::data::ProductId],
                        ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                            let mut value = ::std::vec::Vec::<#item_type>::new();
                            #(#pushes)*
                            ::plingo::framework::parse::__macro_private::production_node(cx, production, children, value)
                        }
                    });
                    registrations.push(quote! {
                        #(#seq_bindings)*
                        ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, rep_lhs, ::std::vec![#(#seq_idents),*], ::std::option::Option::None, ::std::option::Option::Some(#builder));
                    });
                }

                Ok(LoweredExpr {
                    symbol_exprs: vec![
                        quote! { ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #synthetic) },
                    ],
                    value_kind: ValueKind::Vec(Box::new(item_kind)),
                    name: capture_name,
                    captures,
                })
            }
            RuleExpr::Alt(items) => {
                if items.len() != 2 {
                    return Err(syn::Error::new(
                        self.enum_ident.span(),
                        "alternation currently supports exactly two branches",
                    ));
                }
                let left = self.lower_expr(&items[0], builders, registrations)?;
                let right = self.lower_expr(&items[1], builders, registrations)?;
                let synthetic = self.synthetic_name("alt");
                let left_builder = self.synthetic_builder("alt_left");
                let right_builder = self.synthetic_builder("alt_right");
                let left_kind = left.value_kind.clone();
                let right_kind = right.value_kind.clone();
                let left_type = left_kind.ty_tokens();
                let right_type = right_kind.ty_tokens();
                let left_extract = left_kind.extract_expr(
                    quote! { ::plingo::framework::parse::__macro_private::production_child(children, 0)? },
                );
                let right_extract = right_kind.extract_expr(
                    quote! { ::plingo::framework::parse::__macro_private::production_child(children, 0)? },
                );
                let left_symbols = left.symbol_exprs;
                let right_symbols = right.symbol_exprs;
                let left_bindings = left_symbols
                    .iter()
                    .enumerate()
                    .map(|(rule_index, expr)| {
                        let ident = format_ident!(
                            "__plingo_alt_left_symbol_{}_{}",
                            self.variant_index,
                            rule_index
                        );
                        quote! {
                            let #ident = #expr;
                        }
                    })
                    .collect::<Vec<_>>();
                let left_idents = (0..left_symbols.len())
                    .map(|rule_index| {
                        format_ident!(
                            "__plingo_alt_left_symbol_{}_{}",
                            self.variant_index,
                            rule_index
                        )
                    })
                    .collect::<Vec<_>>();
                let right_bindings = right_symbols
                    .iter()
                    .enumerate()
                    .map(|(rule_index, expr)| {
                        let ident = format_ident!(
                            "__plingo_alt_right_symbol_{}_{}",
                            self.variant_index,
                            rule_index
                        );
                        quote! {
                            let #ident = #expr;
                        }
                    })
                    .collect::<Vec<_>>();
                let right_idents = (0..right_symbols.len())
                    .map(|rule_index| {
                        format_ident!(
                            "__plingo_alt_right_symbol_{}_{}",
                            self.variant_index,
                            rule_index
                        )
                    })
                    .collect::<Vec<_>>();

                builders.push(quote! {
                    fn #left_builder(
                        cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                        production: ::plingo::framework::parse::grammar::ProductionId,
                        children: &[::plingo::framework::parse::data::ProductId],
                    ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                        let value: #left_type = #left_extract;
                        ::plingo::framework::parse::__macro_private::production_node(
                            cx,
                            production,
                            children,
                            ::plingo::utils::Either::<#left_type, #right_type>::Left(value),
                        )
                    }
                });
                builders.push(quote! {
                    fn #right_builder(
                        cx: &mut ::plingo::framework::parse::grammar::BuildCx<'_>,
                        production: ::plingo::framework::parse::grammar::ProductionId,
                        children: &[::plingo::framework::parse::data::ProductId],
                    ) -> ::std::result::Result<::plingo::framework::parse::data::ProductId, ::plingo::framework::parse::grammar::BuildError> {
                        let value: #right_type = #right_extract;
                        ::plingo::framework::parse::__macro_private::production_node(
                            cx,
                            production,
                            children,
                            ::plingo::utils::Either::<#left_type, #right_type>::Right(value),
                        )
                    }
                });

                registrations.push(quote! {
                    let alt_lhs = ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #synthetic);
                    #(#left_bindings)*
                    ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, alt_lhs, ::std::vec![#(#left_idents),*], ::std::option::Option::None, ::std::option::Option::Some(#left_builder));
                    #(#right_bindings)*
                    ::plingo::framework::parse::__macro_private::rule(grammar, #synthetic, alt_lhs, ::std::vec![#(#right_idents),*], ::std::option::Option::None, ::std::option::Option::Some(#right_builder));
                });

                Ok(LoweredExpr::new(
                    vec![
                        quote! { ::plingo::framework::parse::__macro_private::begin_internal_non_terminal(grammar, #synthetic) },
                    ],
                    ValueKind::Either(Box::new(left_kind), Box::new(right_kind)),
                ))
            }
        }
    }

    fn synthetic_name(&mut self, kind: &str) -> syn::LitStr {
        let index = self.synthetic_index;
        self.synthetic_index += 1;
        syn::LitStr::new(
            &format!(
                "{}::__{}_{}_{}",
                self.enum_ident, self.variant_index, kind, index
            ),
            self.enum_ident.span(),
        )
    }

    fn synthetic_builder(&mut self, kind: &str) -> Ident {
        let index = self.synthetic_index;
        self.synthetic_index += 1;
        format_ident!(
            "__plingo_build_{}_{}_{}_{}",
            self.enum_ident,
            self.variant_index,
            kind,
            index
        )
    }

    fn repeat_item_extract_expr(
        &self,
        item_kind: &ValueKind,
        base: usize,
    ) -> proc_macro2::TokenStream {
        match item_kind {
            ValueKind::Tuple(kinds) => {
                let parts: Vec<_> = kinds
                    .iter()
                    .enumerate()
                    .map(|(index, kind)| {
                        let offset = base + index;
                        kind.extract_expr(quote! {
                            ::plingo::framework::parse::__macro_private::production_child(children, #offset)?
                        })
                    })
                    .collect();
                quote! { (#(#parts),*) }
            }
            _ => item_kind.extract_expr(quote! {
                ::plingo::framework::parse::__macro_private::production_child(children, #base)?
            }),
        }
    }

    fn atom_symbol_expr(&self, atom: &Atom) -> syn::Result<proc_macro2::TokenStream> {
        match atom {
            Atom::NonTerminal(ty) => Ok(quote! {
                <#ty as ::plingo::framework::parse::__macro_private::NonTerminalSpec>::register(grammar)
            }),
            Atom::TieredNonTerminal { level, .. } => {
                let Some(symbols) = self.tier_symbols else {
                    return Err(syn::Error::new(
                        self.enum_ident.span(),
                        "internal error: tiered rule was lowered without tier symbols",
                    ));
                };
                let Some(symbol) = symbols.get(level) else {
                    return Err(syn::Error::new(
                        self.enum_ident.span(),
                        "internal error: tiered rule referenced an unknown binding-power level",
                    ));
                };
                Ok(quote! { #symbol })
            }
            Atom::Token { root, variant } => Ok(quote! {
                <#root as ::plingo::framework::parse::__macro_private::TokenVariantSpec>::register_terminal(
                    grammar,
                    stringify!(#variant),
                )
            }),
            Atom::Error => Ok(quote! {
                ::plingo::framework::parse::grammar::Symbol::T(
                    ::plingo::framework::parse::grammar::ERROR_TERMINAL,
                )
            }),
        }
    }
}

#[derive(Clone)]
struct LoweredExpr {
    symbol_exprs: Vec<proc_macro2::TokenStream>,
    value_kind: ValueKind,
    name: Option<String>,
    captures: Vec<(String, usize, ValueKind)>,
}

impl LoweredExpr {
    fn new(symbol_exprs: Vec<proc_macro2::TokenStream>, value_kind: ValueKind) -> Self {
        Self {
            symbol_exprs,
            value_kind,
            name: None,
            captures: Vec::new(),
        }
    }
}

#[derive(Clone)]
enum ValueKind {
    Unit,
    Node(Type),
    Token(Type),
    Option(Box<ValueKind>),
    Vec(Box<ValueKind>),
    Repeat(Box<ValueKind>),
    Either(Box<ValueKind>, Box<ValueKind>),
    Tuple(Vec<ValueKind>),
    Error,
}

impl ValueKind {
    fn ty_tokens(&self) -> proc_macro2::TokenStream {
        match self {
            Self::Unit => quote! { () },
            Self::Node(ty) => quote! { ::plingo::framework::parse::data::AstBox<#ty> },
            Self::Token(ty) => quote! { ::plingo::framework::parse::data::AstToken<#ty> },
            Self::Option(inner) => {
                let inner = inner.ty_tokens();
                quote! { ::std::option::Option<#inner> }
            }
            Self::Vec(inner) => {
                let inner = inner.ty_tokens();
                quote! { ::std::vec::Vec<#inner> }
            }
            Self::Repeat(inner) => {
                let inner = inner.ty_tokens();
                quote! { ::std::vec::Vec<#inner> }
            }
            Self::Either(left, right) => {
                let left = left.ty_tokens();
                let right = right.ty_tokens();
                quote! { ::plingo::utils::Either<#left, #right> }
            }
            Self::Tuple(items) => {
                let items = items.iter().map(Self::ty_tokens);
                quote! { (#(#items),*) }
            }
            Self::Error => quote! { () },
        }
    }

    fn extract_expr(&self, child: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
        match self {
            Self::Unit => quote! { () },
            Self::Repeat(inner) => {
                let inner = inner.ty_tokens();
                quote! {
                    ::plingo::framework::parse::__macro_private::repeat_from_product::<#inner>(cx, #child)?
                }
            }
            Self::Node(_) | Self::Option(_) | Self::Vec(_) | Self::Either(_, _) | Self::Error => {
                let ty = self.ty_tokens();
                quote! {
                    <#ty as ::plingo::framework::parse::__macro_private::BuildField>::from_product(cx, #child)?
                }
            }
            Self::Token(ty) => {
                quote! {
                    <::plingo::framework::parse::data::AstToken<#ty> as ::plingo::framework::parse::__macro_private::TokenField>::from_token_entry(
                        cx,
                        ::plingo::framework::parse::__macro_private::BuildField::from_product(cx, #child)?,
                    )?
                }
            }
            Self::Tuple(kinds) => {
                let fields = kinds.iter().enumerate().map(|(i, k)| {
                    k.extract_expr(quote! {
                        ::plingo::framework::parse::__macro_private::production_child(children, #i)?
                    })
                });
                quote! { (#(#fields),*) }
            }
        }
    }
}

#[derive(Clone)]
enum RuleExpr {
    Empty,
    Atom(Atom),
    Seq(Vec<RuleExpr>),
    Alt(Vec<RuleExpr>),
    Optional(Box<RuleExpr>),
    Repeat(
        Box<RuleExpr>,
        Option<Box<RuleExpr>>,
        Option<(usize, Option<usize>)>,
    ),
    Named(String, Box<RuleExpr>),
}

#[derive(Clone)]
enum Atom {
    Token { root: Type, variant: Ident },
    NonTerminal(Type),
    TieredNonTerminal { ty: Type, level: u32 },
    Error,
}

struct RuleSpec {
    expr: RuleExpr,
    output: Option<TierTarget>,
    output_level: Option<u32>,
}

struct TierTarget {
    ty: Type,
    level: u32,
}

fn parse_rule_spec(variant: &Variant) -> syn::Result<RuleSpec> {
    let mut rule = None;
    let mut is_error = false;
    for attr in &variant.attrs {
        if attr.path().is_ident("parse_err") {
            if is_error {
                return Err(syn::Error::new(
                    attr.span(),
                    "duplicate #[parse_err] attribute",
                ));
            }
            if rule.is_some() {
                return Err(syn::Error::new(
                    attr.span(),
                    "#[parse_err] and #[rule] cannot be used on the same variant",
                ));
            }
            is_error = true;
            rule = Some(RuleSpec {
                expr: RuleExpr::Atom(Atom::Error),
                output: None,
                output_level: None,
            });
            continue;
        }
        if attr.path().is_ident("rule") {
            if rule.is_some() {
                return Err(syn::Error::new(
                    attr.span(),
                    "duplicate #[rule(...)] attribute",
                ));
            }
            if is_error {
                return Err(syn::Error::new(
                    attr.span(),
                    "#[parse_err] and #[rule] cannot be used on the same variant",
                ));
            }
            rule = Some(
                if matches!(&attr.meta, syn::Meta::List(meta) if meta.tokens.is_empty()) {
                    RuleSpec {
                        expr: RuleExpr::Empty,
                        output: None,
                        output_level: None,
                    }
                } else {
                    attr.parse_args::<RuleSpec>()?
                },
            );
        }
    }
    rule.ok_or_else(|| {
        syn::Error::new(
            variant.span(),
            "each nonterminal variant requires #[rule(...)] or #[parse_err]",
        )
    })
}

impl Parse for RuleSpec {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let fork = input.fork();
        let explicit_output = (|| {
            let ty: Type = fork.parse()?;
            let _: syn::Token![:] = fork.parse()?;
            let level = fork.parse::<LitInt>()?.base10_parse::<u32>()?;
            let _: syn::Token![<] = fork.parse()?;
            let _: syn::Token![-] = fork.parse()?;
            Ok::<_, syn::Error>(TierTarget { ty, level })
        })();

        let output = if let Ok(output) = explicit_output {
            input.advance_to(&fork);
            Some(output)
        } else {
            None
        };
        let expr = parse_alt(input)?;
        Ok(Self {
            expr,
            output,
            output_level: None,
        })
    }
}

impl RuleExpr {
    fn contains_tiered_non_terminal(&self) -> bool {
        match self {
            Self::Atom(Atom::TieredNonTerminal { .. }) => true,
            Self::Empty | Self::Atom(_) => false,
            Self::Seq(items) | Self::Alt(items) => {
                items.iter().any(Self::contains_tiered_non_terminal)
            }
            Self::Optional(inner) | Self::Named(_, inner) => inner.contains_tiered_non_terminal(),
            Self::Repeat(inner, separator, _) => {
                inner.contains_tiered_non_terminal()
                    || separator
                        .as_deref()
                        .is_some_and(Self::contains_tiered_non_terminal)
            }
        }
    }

    /// Collect the levels used by this production and reject an unqualified
    /// self-reference.  The latter would bypass the tier grammar and bring
    /// its ambiguity back.
    fn collect_and_validate_tiers(
        &self,
        enum_ident: &Ident,
        levels: &mut BTreeSet<u32>,
    ) -> syn::Result<()> {
        match self {
            Self::Empty | Self::Atom(Atom::Token { .. }) | Self::Atom(Atom::Error) => Ok(()),
            Self::Atom(Atom::NonTerminal(ty)) => {
                if is_current_non_terminal(ty, enum_ident) {
                    Err(syn::Error::new(
                        ty.span(),
                        format!(
                            "a tiered `{enum_ident}` grammar must write self-references as `{enum_ident}:n`"
                        ),
                    ))
                } else {
                    Ok(())
                }
            }
            Self::Atom(Atom::TieredNonTerminal { ty, level }) => {
                if !is_current_non_terminal(ty, enum_ident) {
                    return Err(syn::Error::new(
                        ty.span(),
                        format!(
                            "tier annotations currently address only the enclosing nonterminal `{enum_ident}`"
                        ),
                    ));
                }
                levels.insert(*level);
                Ok(())
            }
            Self::Seq(items) | Self::Alt(items) => {
                for item in items {
                    item.collect_and_validate_tiers(enum_ident, levels)?;
                }
                Ok(())
            }
            Self::Optional(inner) | Self::Named(_, inner) => {
                inner.collect_and_validate_tiers(enum_ident, levels)
            }
            Self::Repeat(inner, separator, _) => {
                inner.collect_and_validate_tiers(enum_ident, levels)?;
                if let Some(separator) = separator {
                    separator.collect_and_validate_tiers(enum_ident, levels)?;
                }
                Ok(())
            }
        }
    }

    /// The compact form `#[rule(Expr:n, ...)]` means `Expr:n <- Expr:n,
    /// ...`.  Restrict inference to the first direct symbol so prefix,
    /// postfix, and non-associative rules remain explicit and easy to read.
    fn leading_tier(&self) -> Option<u32> {
        match self {
            Self::Atom(Atom::TieredNonTerminal { level, .. }) => Some(*level),
            Self::Named(_, inner) => inner.leading_tier(),
            Self::Seq(items) => items.first().and_then(Self::leading_tier),
            _ => None,
        }
    }
}

fn is_current_non_terminal(ty: &Type, enum_ident: &Ident) -> bool {
    let Type::Path(type_path) = ty else {
        return false;
    };
    type_path.qself.is_none()
        && type_path.path.leading_colon.is_none()
        && type_path.path.segments.len() == 1
        && type_path.path.segments[0].ident == *enum_ident
        && type_path.path.segments[0].arguments.is_empty()
}

impl Parse for RuleExpr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        parse_alt(input)
    }
}

fn parse_alt(input: ParseStream) -> syn::Result<RuleExpr> {
    let mut items = vec![parse_seq(input)?];
    while input.peek(syn::Token![|]) {
        let _: syn::Token![|] = input.parse()?;
        items.push(parse_seq(input)?);
    }
    Ok(if items.len() == 1 {
        items.remove(0)
    } else {
        RuleExpr::Alt(items)
    })
}

fn parse_seq(input: ParseStream) -> syn::Result<RuleExpr> {
    let mut items = vec![parse_postfix(input)?];
    while input.peek(syn::Token![,]) {
        let _: syn::Token![,] = input.parse()?;
        items.push(parse_postfix(input)?);
    }
    Ok(if items.len() == 1 {
        items.remove(0)
    } else {
        RuleExpr::Seq(items)
    })
}

fn parse_postfix(input: ParseStream) -> syn::Result<RuleExpr> {
    if input.peek(syn::Token![$]) {
        let _: syn::Token![$] = input.parse()?;
        let name: Ident = input.parse()?;
        let content;
        syn::parenthesized!(content in input);
        let inner = parse_alt(&content)?;
        return Ok(RuleExpr::Named(name.to_string(), Box::new(inner)));
    }
    if input.peek(syn::token::Bracket) {
        let content;
        syn::bracketed!(content in input);
        let inner = parse_alt(&content)?;
        return Ok(RuleExpr::Optional(Box::new(inner)));
    }
    if input.peek(syn::token::Brace) {
        let content;
        syn::braced!(content in input);
        let inner = parse_alt(&content)?;
        let separator = if input.peek(syn::token::Brace) {
            let sep_content;
            syn::braced!(sep_content in input);
            Some(Box::new(parse_alt(&sep_content)?))
        } else {
            None
        };
        let bounds = if input.peek(syn::token::Bracket) {
            let bound_content;
            syn::bracketed!(bound_content in input);
            let min = bound_content.parse::<LitInt>()?.base10_parse::<usize>()?;
            let _: syn::Token![..] = bound_content.parse()?;
            let max = if bound_content.is_empty() {
                None
            } else {
                Some(bound_content.parse::<LitInt>()?.base10_parse::<usize>()?)
            };
            Some((min, max))
        } else {
            None
        };
        return Ok(RuleExpr::Repeat(Box::new(inner), separator, bounds));
    }
    parse_atom(input)
}

fn parse_atom(input: ParseStream) -> syn::Result<RuleExpr> {
    if input.peek(syn::token::Paren) {
        let content;
        syn::parenthesized!(content in input);
        if content.is_empty() {
            return Ok(RuleExpr::Empty);
        }
        return parse_alt(&content);
    }

    let ty: Type = input.parse()?;
    let Type::Path(type_path) = &ty else {
        return Err(syn::Error::new(
            ty.span(),
            "rule symbol must be a type path or token variant path",
        ));
    };

    if input.peek(syn::Token![:]) {
        let _: syn::Token![:] = input.parse()?;
        let level = input.parse::<LitInt>()?.base10_parse::<u32>()?;
        return Ok(RuleExpr::Atom(Atom::TieredNonTerminal { ty, level }));
    }

    if type_path.qself.is_none()
        && type_path.path.segments.len() == 2
        && type_path.path.segments[0].arguments.is_empty()
        && type_path.path.segments[1].arguments.is_empty()
    {
        let root = Type::Path(TypePath {
            qself: None,
            path: syn::Path::from(type_path.path.segments[0].ident.clone()),
        });
        let variant = type_path.path.segments[1].ident.clone();
        return Ok(RuleExpr::Atom(Atom::Token { root, variant }));
    }

    Ok(RuleExpr::Atom(Atom::NonTerminal(ty)))
}

fn atom_value_type(atom: &Atom) -> syn::Result<ValueKind> {
    match atom {
        Atom::NonTerminal(ty) => Ok(ValueKind::Node(ty.clone())),
        Atom::TieredNonTerminal { ty, .. } => Ok(ValueKind::Node(ty.clone())),
        Atom::Token { root, .. } => Ok(ValueKind::Token(root.clone())),
        Atom::Error => Ok(ValueKind::Error),
    }
}

enum FromSpec {
    Positional(usize),
    Named(String),
}

fn build_variant_field_exprs(
    fields: &Fields,
    rhs_exprs: &[proc_macro2::TokenStream],
    named_captures: &[(String, usize)],
) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    fields
        .iter()
        .map(|field| build_variant_field_expr(field, rhs_exprs, named_captures))
        .collect()
}

fn build_variant_field_expr(
    field: &Field,
    rhs_exprs: &[proc_macro2::TokenStream],
    named_captures: &[(String, usize)],
) -> syn::Result<proc_macro2::TokenStream> {
    let field_ty = &field.ty;
    let from_spec = parse_from_spec(field)?;
    let index = match from_spec {
        FromSpec::Positional(i) => i,
        FromSpec::Named(ref name) => named_captures
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, i)| *i)
            .ok_or_else(|| {
                syn::Error::new(
                    field.ty.span(),
                    format!(
                        "named capture '{name}' not found in rule; available: {:?}",
                        named_captures
                    ),
                )
            })?,
    };
    if index >= rhs_exprs.len() {
        return Err(syn::Error::new(
            field.ty.span(),
            format!("field receiver index {index} is out of bounds"),
        ));
    }
    let child =
        quote! { ::plingo::framework::parse::__macro_private::production_child(children, #index)? };
    Ok(
        if is_ast_box(field_ty)
            || is_option(field_ty)
            || is_vec(field_ty)
            || is_either(field_ty)
            || is_parse_error_info(field_ty)
        {
            quote! {
                <#field_ty as ::plingo::framework::parse::__macro_private::BuildField>::from_product(
                    cx,
                    #child,
                )?
            }
        } else {
            quote! {
                <#field_ty as ::plingo::framework::parse::__macro_private::TokenField>::from_token_entry(
                    cx,
                    <::plingo::framework::parse::__macro_private::TokenEntryId as ::plingo::framework::parse::__macro_private::BuildField>::from_product(
                        cx,
                        #child,
                    )?,
                )?
            }
        },
    )
}

fn parse_from_spec(field: &Field) -> syn::Result<FromSpec> {
    let mut spec = None;
    for attr in &field.attrs {
        if !attr.path().is_ident("from") {
            continue;
        }
        if spec.is_some() {
            return Err(syn::Error::new(
                attr.span(),
                "duplicate #[from(...)] attribute",
            ));
        }
        let tokens = &attr.meta.require_list()?.tokens;
        let arg = syn::parse2::<syn::LitInt>(tokens.clone()).ok();
        if let Some(lit) = arg {
            spec = Some(FromSpec::Positional(lit.base10_parse::<usize>()?));
        } else {
            let ident = syn::parse2::<Ident>(tokens.clone())?;
            spec = Some(FromSpec::Named(ident.to_string()));
        }
    }
    spec.ok_or_else(|| {
        syn::Error::new(
            field.span(),
            "each nonterminal field requires #[from(index)] or #[from(name)]",
        )
    })
}

fn is_ast_box(ty: &Type) -> bool {
    path_head(ty).as_deref() == Some("AstBox")
}

fn is_option(ty: &Type) -> bool {
    path_head(ty).as_deref() == Some("Option")
}

fn is_vec(ty: &Type) -> bool {
    path_head(ty).as_deref() == Some("Vec")
}

fn is_either(ty: &Type) -> bool {
    path_head(ty).as_deref() == Some("Either")
}

fn is_parse_error_info(ty: &Type) -> bool {
    path_head(ty).as_deref() == Some("ParseErrorInfo")
}

fn path_head(ty: &Type) -> Option<String> {
    let Type::Path(path) = ty else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}
