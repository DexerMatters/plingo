use quote::quote;
use syn::{Fields, ItemEnum, Type};

pub fn expand_pretty_non_terminal_derive(item: ItemEnum) -> syn::Result<proc_macro::TokenStream> {
    let enum_ident = item.ident;
    let arms = item
        .variants
        .iter()
        .map(|variant| {
            let variant_ident = &variant.ident;
            let label = format!("{}::{}", enum_ident, variant_ident);
            let (pattern, fields) = match &variant.fields {
                Fields::Unit => (quote! {}, Vec::new()),
                Fields::Unnamed(fields) => {
                    let bindings = (0..fields.unnamed.len())
                        .map(|index| {
                            syn::Ident::new(&format!("field_{index}"), variant_ident.span())
                        })
                        .collect::<Vec<_>>();
                    let renders = fields
                        .unnamed
                        .iter()
                        .zip(bindings.iter())
                        .enumerate()
                        .map(|(index, (field, binding))| {
                            field_render(index.to_string(), &field.ty, binding)
                        })
                        .collect();
                    (quote! { ( #(#bindings),* ) }, renders)
                }
                Fields::Named(fields) => {
                    let bindings = fields
                        .named
                        .iter()
                        .map(|field| field.ident.as_ref().expect("named field"))
                        .collect::<Vec<_>>();
                    let renders = fields
                        .named
                        .iter()
                        .zip(bindings.iter())
                        .map(|(field, binding)| {
                            field_render(binding.to_string(), &field.ty, binding)
                        })
                        .collect();
                    (quote! { { #(#bindings),* } }, renders)
                }
            };
            quote! {
                Self::#variant_ident #pattern => renderer.variant(#label, |renderer| {
                    #(#fields)*
                    ::std::result::Result::Ok(())
                }),
            }
        })
        .collect::<Vec<_>>();

    Ok(quote! {
        impl ::plingo::visual::ast::PrettyNonTerminal for #enum_ident {
            fn pretty_non_terminal(
                &self,
                renderer: &mut ::plingo::visual::ast::AstRenderer<'_, '_>,
            ) -> ::std::fmt::Result {
                match self {
                    #(#arms)*
                }
            }
        }
    }
    .into())
}

pub fn expand_pretty_terminal_derive(item: ItemEnum) -> syn::Result<proc_macro::TokenStream> {
    let enum_ident = item.ident;
    let arms = item
        .variants
        .iter()
        .enumerate()
        .map(|(index, variant)| {
            let name = variant.ident.to_string();
            quote! { #index => #name, }
        })
        .collect::<Vec<_>>();

    Ok(quote! {
        impl ::plingo::visual::ast::PrettyTerminal for #enum_ident {
            fn pretty_terminal(
                terminal: ::std::option::Option<::plingo::component::parse::grammar::TerminalId>,
                source: &str,
            ) -> ::std::string::String {
                let name = terminal
                    .map(|terminal| match terminal.token_id as usize {
                        #(#arms)*
                        _ => "<unknown>",
                    })
                    .unwrap_or("<error>");
                ::std::format!("{name} {source:?}")
            }
        }
    }
    .into())
}

fn field_render(label: String, ty: &Type, binding: &syn::Ident) -> proc_macro2::TokenStream {
    if is_visual_field(ty) {
        quote! { renderer.field(#label, #binding)?; }
    } else {
        quote! { renderer.debug_field(#label, #binding)?; }
    }
}

fn is_visual_field(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return matches!(ty, Type::Tuple(tuple) if tuple.elems.iter().all(is_visual_field));
    };
    let Some(segment) = path.path.segments.last() else {
        return false;
    };
    match segment.ident.to_string().as_str() {
        "AstBox" | "AstToken" => true,
        "Option" | "Vec" => type_arguments(segment)
            .is_some_and(|types| types.len() == 1 && is_visual_field(types[0])),
        _ => false,
    }
}

fn type_arguments(segment: &syn::PathSegment) -> Option<Vec<&Type>> {
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    Some(
        arguments
            .args
            .iter()
            .filter_map(|argument| match argument {
                syn::GenericArgument::Type(ty) => Some(ty),
                _ => None,
            })
            .collect(),
    )
}
