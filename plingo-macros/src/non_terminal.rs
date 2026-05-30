use quote::{format_ident, quote};
use syn::{
    Field, Fields, Ident, ItemEnum, LitInt, Type, TypePath, Variant,
    parse::{Parse, ParseStream},
    spanned::Spanned,
};

use crate::shared::push_missing_derives;

pub fn expand_non_terminal_derive(mut item: ItemEnum) -> syn::Result<proc_macro::TokenStream> {
    push_missing_derives(&mut item, &["Debug"])?;
    let enum_ident = item.ident.clone();
    let variants = item.variants.clone();
    strip_non_terminal_attrs(&mut item);

    let register_fn = format_ident!("__plingo_register_non_terminal_{}", enum_ident);
    let mut builders = Vec::new();
    let mut registrations = Vec::new();
    for (variant_index, variant) in variants.iter().enumerate() {
        let lowered = LowerCtx::new(&enum_ident, variant_index).lower_variant(variant)?;
        builders.extend(lowered.builders);
        registrations.extend(lowered.registrations);
    }

    Ok(quote! {
        impl ::plingo::component::parse::__macro_private::NonTerminalSpec for #enum_ident {
            fn register(grammar: &mut ::plingo::component::parse::grammar::GrammarBuilder) -> ::plingo::component::parse::grammar::Symbol {
                #register_fn(grammar)
            }
        }

        #[allow(non_snake_case)]
        fn #register_fn(
            grammar: &mut ::plingo::component::parse::grammar::GrammarBuilder,
        ) -> ::plingo::component::parse::grammar::Symbol {
            let (lhs, fresh) = grammar.begin_non_terminal(stringify!(#enum_ident));
            if !fresh {
                return lhs;
            }
            #(#builders)*
            #(#registrations)*
            lhs
        }
    }
    .into())
}

fn strip_non_terminal_attrs(item: &mut ItemEnum) {
    for variant in &mut item.variants {
        variant.attrs.retain(|attr| !attr.path().is_ident("rule"));
        for field in &mut variant.fields {
            field.attrs.retain(|attr| !attr.path().is_ident("from"));
        }
    }
}

struct LoweredVariant {
    builders: Vec<proc_macro2::TokenStream>,
    registrations: Vec<proc_macro2::TokenStream>,
}

struct LowerCtx<'a> {
    enum_ident: &'a Ident,
    variant_index: usize,
    synthetic_index: usize,
}

impl<'a> LowerCtx<'a> {
    fn new(enum_ident: &'a Ident, variant_index: usize) -> Self {
        Self {
            enum_ident,
            variant_index,
            synthetic_index: 0,
        }
    }

    fn lower_variant(mut self, variant: &Variant) -> syn::Result<LoweredVariant> {
        let enum_ident = self.enum_ident;
        let builder_ident = format_ident!("__plingo_build_{}_{}", enum_ident, self.variant_index);
        let variant_ident = &variant.ident;
        let label = format!("{}::{}", enum_ident, variant_ident);
        let rule = parse_rule_expr(variant)?;

        let mut builders = Vec::new();
        let mut registrations = Vec::new();
        let lowered = self.lower_expr(&rule, &mut builders, &mut registrations)?;
        let rhs_exprs = lowered.symbol_exprs;
        let field_exprs = build_variant_field_exprs(&variant.fields, &rhs_exprs)?;
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
                cx: &mut ::plingo::component::parse::grammar::BuildCx<'_>,
                production: ::plingo::component::parse::grammar::ProductionId,
                children: &[::plingo::component::parse::data::ProductId],
            ) -> ::std::result::Result<::plingo::component::parse::data::ProductId, ::plingo::component::parse::grammar::BuildError> {
                let value = #ctor;
                ::plingo::component::parse::__macro_private::production_node(cx, production, children, value)
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
            grammar.rule(
                #label,
                lhs,
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
                vec![atom_symbol_expr(atom)],
                atom_value_type(atom)?,
            )),
            RuleExpr::Seq(items) => {
                let mut symbol_exprs = Vec::new();
                let mut value_types = Vec::new();
                for item in items {
                    let lowered = self.lower_expr(item, builders, registrations)?;
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
                Ok(LoweredExpr::new(symbol_exprs, value_type))
            }
            RuleExpr::Optional(inner) => {
                let lowered = self.lower_expr(inner, builders, registrations)?;
                let synthetic = self.synthetic_name("opt");
                let builder_none = self.synthetic_builder("opt_none");
                let builder_some = self.synthetic_builder("opt_some");
                let inner_kind = lowered.value_kind.clone();
                let inner_type = inner_kind.ty_tokens();
                let inner_extract = inner_kind.extract_expr(
                    quote! { ::plingo::component::parse::__macro_private::production_child(children, 0)? },
                );
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
                        cx: &mut ::plingo::component::parse::grammar::BuildCx<'_>,
                        production: ::plingo::component::parse::grammar::ProductionId,
                        children: &[::plingo::component::parse::data::ProductId],
                    ) -> ::std::result::Result<::plingo::component::parse::data::ProductId, ::plingo::component::parse::grammar::BuildError> {
                        ::plingo::component::parse::__macro_private::production_node(
                            cx,
                            production,
                            children,
                            ::std::option::Option::<#inner_type>::None,
                        )
                    }
                });
                builders.push(quote! {
                    fn #builder_some(
                        cx: &mut ::plingo::component::parse::grammar::BuildCx<'_>,
                        production: ::plingo::component::parse::grammar::ProductionId,
                        children: &[::plingo::component::parse::data::ProductId],
                    ) -> ::std::result::Result<::plingo::component::parse::data::ProductId, ::plingo::component::parse::grammar::BuildError> {
                        let value: #inner_type = #inner_extract;
                        ::plingo::component::parse::__macro_private::production_node(
                            cx,
                            production,
                            children,
                            ::std::option::Option::Some(value),
                        )
                    }
                });

                registrations.push(quote! {
                    let lhs = grammar.begin_internal_non_terminal(#synthetic);
                    #(#inner_bindings)*
                    grammar.rule(#synthetic, lhs, ::std::vec![], ::std::option::Option::None, ::std::option::Option::Some(#builder_none));
                    grammar.rule(#synthetic, lhs, ::std::vec![#(#inner_idents),*], ::std::option::Option::None, ::std::option::Option::Some(#builder_some));
                });

                Ok(LoweredExpr::new(
                    vec![quote! { grammar.begin_internal_non_terminal(#synthetic) }],
                    ValueKind::Option(Box::new(inner_kind)),
                ))
            }
            RuleExpr::Repeat(inner, bounds) => {
                let lowered = self.lower_expr(inner, builders, registrations)?;
                let synthetic = self.synthetic_name("rep");
                let item_kind = lowered.value_kind.clone();
                let item_type = item_kind.ty_tokens();
                let inner_symbols = lowered.symbol_exprs.clone();
                let (min, max) = bounds.unwrap_or((0, None));
                let upper = max.unwrap_or(min + 3);

                if min == 0 {
                    let builder_empty = self.synthetic_builder("rep_empty");
                    builders.push(quote! {
                        fn #builder_empty(
                            cx: &mut ::plingo::component::parse::grammar::BuildCx<'_>,
                            production: ::plingo::component::parse::grammar::ProductionId,
                            children: &[::plingo::component::parse::data::ProductId],
                        ) -> ::std::result::Result<::plingo::component::parse::data::ProductId, ::plingo::component::parse::grammar::BuildError> {
                            ::plingo::component::parse::__macro_private::production_node(
                                cx,
                                production,
                                children,
                                ::std::vec::Vec::<#item_type>::new(),
                            )
                        }
                    });
                    registrations.push(quote! {
                        let lhs = grammar.begin_internal_non_terminal(#synthetic);
                        grammar.rule(#synthetic, lhs, ::std::vec![], ::std::option::Option::None, ::std::option::Option::Some(#builder_empty));
                    });
                } else {
                    registrations.push(quote! {
                        let lhs = grammar.begin_internal_non_terminal(#synthetic);
                    });
                }

                for count in min.max(1)..=upper {
                    let builder = self.synthetic_builder(&format!("rep_{}", count));
                    let seq = (0..count)
                        .flat_map(|_| inner_symbols.clone())
                        .collect::<Vec<_>>();
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
                            quote! {
                                let #ident = #expr;
                            }
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
                    let indexes = (0..count).collect::<Vec<_>>();
                    let pushes = indexes
                        .iter()
                        .map(|index| {
                            let extract = item_kind.extract_expr(quote! {
                                ::plingo::component::parse::__macro_private::production_child(children, #index)?
                            });
                            quote! {
                                value.push(#extract);
                            }
                        })
                        .collect::<Vec<_>>();
                    builders.push(quote! {
                        fn #builder(
                            cx: &mut ::plingo::component::parse::grammar::BuildCx<'_>,
                            production: ::plingo::component::parse::grammar::ProductionId,
                            children: &[::plingo::component::parse::data::ProductId],
                        ) -> ::std::result::Result<::plingo::component::parse::data::ProductId, ::plingo::component::parse::grammar::BuildError> {
                            let mut value = ::std::vec::Vec::<#item_type>::new();
                            #(#pushes)*
                            ::plingo::component::parse::__macro_private::production_node(cx, production, children, value)
                        }
                    });
                    registrations.push(quote! {
                        #(#seq_bindings)*
                        grammar.rule(#synthetic, lhs, ::std::vec![#(#seq_idents),*], ::std::option::Option::None, ::std::option::Option::Some(#builder));
                    });
                }

                Ok(LoweredExpr::new(
                    vec![quote! { grammar.begin_internal_non_terminal(#synthetic) }],
                    ValueKind::Vec(Box::new(item_kind)),
                ))
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
                    quote! { ::plingo::component::parse::__macro_private::production_child(children, 0)? },
                );
                let right_extract = right_kind.extract_expr(
                    quote! { ::plingo::component::parse::__macro_private::production_child(children, 0)? },
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
                        cx: &mut ::plingo::component::parse::grammar::BuildCx<'_>,
                        production: ::plingo::component::parse::grammar::ProductionId,
                        children: &[::plingo::component::parse::data::ProductId],
                    ) -> ::std::result::Result<::plingo::component::parse::data::ProductId, ::plingo::component::parse::grammar::BuildError> {
                        let value: #left_type = #left_extract;
                        ::plingo::component::parse::__macro_private::production_node(
                            cx,
                            production,
                            children,
                            ::plingo::utils::Either::<#left_type, #right_type>::Left(value),
                        )
                    }
                });
                builders.push(quote! {
                    fn #right_builder(
                        cx: &mut ::plingo::component::parse::grammar::BuildCx<'_>,
                        production: ::plingo::component::parse::grammar::ProductionId,
                        children: &[::plingo::component::parse::data::ProductId],
                    ) -> ::std::result::Result<::plingo::component::parse::data::ProductId, ::plingo::component::parse::grammar::BuildError> {
                        let value: #right_type = #right_extract;
                        ::plingo::component::parse::__macro_private::production_node(
                            cx,
                            production,
                            children,
                            ::plingo::utils::Either::<#left_type, #right_type>::Right(value),
                        )
                    }
                });

                registrations.push(quote! {
                    let lhs = grammar.begin_internal_non_terminal(#synthetic);
                    #(#left_bindings)*
                    grammar.rule(#synthetic, lhs, ::std::vec![#(#left_idents),*], ::std::option::Option::None, ::std::option::Option::Some(#left_builder));
                    #(#right_bindings)*
                    grammar.rule(#synthetic, lhs, ::std::vec![#(#right_idents),*], ::std::option::Option::None, ::std::option::Option::Some(#right_builder));
                });

                Ok(LoweredExpr::new(
                    vec![quote! { grammar.begin_internal_non_terminal(#synthetic) }],
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
}

#[derive(Clone)]
struct LoweredExpr {
    symbol_exprs: Vec<proc_macro2::TokenStream>,
    value_kind: ValueKind,
}

impl LoweredExpr {
    fn new(symbol_exprs: Vec<proc_macro2::TokenStream>, value_kind: ValueKind) -> Self {
        Self {
            symbol_exprs,
            value_kind,
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
    Either(Box<ValueKind>, Box<ValueKind>),
    Tuple(Vec<ValueKind>),
}

impl ValueKind {
    fn ty_tokens(&self) -> proc_macro2::TokenStream {
        match self {
            Self::Unit => quote! { () },
            Self::Node(ty) => quote! { ::plingo::component::parse::data::AstBox<#ty> },
            Self::Token(ty) => quote! { #ty },
            Self::Option(inner) => {
                let inner = inner.ty_tokens();
                quote! { ::std::option::Option<#inner> }
            }
            Self::Vec(inner) => {
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
        }
    }

    fn extract_expr(&self, child: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
        match self {
            Self::Unit => quote! { () },
            Self::Node(_) | Self::Option(_) | Self::Vec(_) | Self::Either(_, _) => {
                let ty = self.ty_tokens();
                quote! {
                    <#ty as ::plingo::component::parse::__macro_private::BuildField>::from_product(cx, #child)?
                }
            }
            Self::Token(ty) => {
                quote! {
                    <#ty as ::plingo::component::parse::__macro_private::TokenField>::from_token_entry(
                        cx,
                        ::plingo::component::parse::__macro_private::BuildField::from_product(cx, #child)?,
                    )?
                }
            }
            Self::Tuple(_) => unreachable!(),
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
    Repeat(Box<RuleExpr>, Option<(usize, Option<usize>)>),
}

#[derive(Clone)]
enum Atom {
    Token { root: Type, variant: Ident },
    NonTerminal(Type),
}

fn parse_rule_expr(variant: &Variant) -> syn::Result<RuleExpr> {
    let mut expr = None;
    for attr in &variant.attrs {
        if !attr.path().is_ident("rule") {
            continue;
        }
        if expr.is_some() {
            return Err(syn::Error::new(
                attr.span(),
                "duplicate #[rule(...)] attribute",
            ));
        }
        expr = Some(
            if matches!(&attr.meta, syn::Meta::List(meta) if meta.tokens.is_empty()) {
                RuleExpr::Empty
            } else {
                attr.parse_args::<RuleExpr>()?
            },
        );
    }
    expr.ok_or_else(|| {
        syn::Error::new(
            variant.span(),
            "each nonterminal variant requires #[rule(...)]",
        )
    })
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
        return Ok(RuleExpr::Repeat(Box::new(inner), bounds));
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

fn atom_symbol_expr(atom: &Atom) -> proc_macro2::TokenStream {
    match atom {
        Atom::NonTerminal(ty) => {
            quote! {
                <#ty as ::plingo::component::parse::__macro_private::NonTerminalSpec>::register(grammar)
            }
        }
        Atom::Token { root, variant } => {
            quote! {
                <#root as ::plingo::component::parse::__macro_private::TokenVariantSpec>::register_terminal(
                    grammar,
                    stringify!(#variant),
                )
            }
        }
    }
}

fn atom_value_type(atom: &Atom) -> syn::Result<ValueKind> {
    match atom {
        Atom::NonTerminal(ty) => Ok(ValueKind::Node(ty.clone())),
        Atom::Token { root, .. } => Ok(ValueKind::Token(root.clone())),
    }
}

fn build_variant_field_exprs(
    fields: &Fields,
    rhs_exprs: &[proc_macro2::TokenStream],
) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    fields
        .iter()
        .map(|field| build_variant_field_expr(field, rhs_exprs))
        .collect()
}

fn build_variant_field_expr(
    field: &Field,
    rhs_exprs: &[proc_macro2::TokenStream],
) -> syn::Result<proc_macro2::TokenStream> {
    let field_ty = &field.ty;
    let index = parse_from_index(field)?;
    if index >= rhs_exprs.len() {
        return Err(syn::Error::new(
            field.ty.span(),
            format!("field receiver index {index} is out of bounds"),
        ));
    }
    let child =
        quote! { ::plingo::component::parse::__macro_private::production_child(children, #index)? };
    Ok(
        if is_ast_box(field_ty) || is_option(field_ty) || is_vec(field_ty) || is_either(field_ty) {
            quote! {
                <#field_ty as ::plingo::component::parse::__macro_private::BuildField>::from_product(
                    cx,
                    #child,
                )?
            }
        } else {
            quote! {
                <#field_ty as ::plingo::component::parse::__macro_private::TokenField>::from_token_entry(
                    cx,
                    <::plingo::component::parse::data::TokenEntryId as ::plingo::component::parse::__macro_private::BuildField>::from_product(
                        cx,
                        #child,
                    )?,
                )?
            }
        },
    )
}

fn parse_from_index(field: &Field) -> syn::Result<usize> {
    let mut index = None;
    for attr in &field.attrs {
        if !attr.path().is_ident("from") {
            continue;
        }
        if index.is_some() {
            return Err(syn::Error::new(
                attr.span(),
                "duplicate #[from(...)] attribute",
            ));
        }
        let lit = attr.parse_args::<LitInt>()?;
        index = Some(lit.base10_parse::<usize>()?);
    }
    index.ok_or_else(|| {
        syn::Error::new(
            field.span(),
            "each nonterminal field requires #[from(index)]",
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

fn path_head(ty: &Type) -> Option<String> {
    let Type::Path(path) = ty else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}
