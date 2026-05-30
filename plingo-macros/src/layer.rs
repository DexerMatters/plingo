use proc_macro::TokenStream;
use quote::quote;
use syn::{Fields, ItemImpl, ItemStruct, Type, parse::Parse, parse::ParseStream, parse_macro_input, parse_quote, spanned::Spanned};

pub enum LayerRole {
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

pub fn expand_layer_struct(item: TokenStream) -> TokenStream {
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

    if fields
        .iter()
        .any(|field| field.ident.as_ref().is_some_and(|ident| ident == "_snapshot"))
    {
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

pub fn expand_layer_impl(attr: TokenStream, item: TokenStream) -> TokenStream {
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
    let Some((_, trait_path, _)) = &item_impl.trait_ else {
        return syn::Error::new(
            item_impl.self_ty.span(),
            "#[layer(role)] can only be used on trait impl blocks",
        )
        .to_compile_error()
        .into();
    };

    let expected_trait = match role {
        LayerRole::Top => "TopLayer",
        LayerRole::Middle => "MiddleLayer",
        LayerRole::Bottom => "BottomLayer",
    };
    if let Some(seg) = trait_path.segments.last() {
        if seg.ident != expected_trait {
            return syn::Error::new(
                seg.ident.span(),
                format!("#[layer(...)] requires impl of {expected_trait}"),
            )
            .to_compile_error()
            .into();
        }
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
