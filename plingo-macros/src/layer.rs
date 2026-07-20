use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Fields, GenericArgument, ItemImpl, ItemStruct, PathArguments, Type, parse::Parse,
    parse::ParseStream, parse_macro_input, parse_quote, spanned::Spanned,
};

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
                let Type::Path(path) = &field.ty else {
                    return syn::Error::new(
                        field.ty.span(),
                        "#[snapshot] field must be Arc<State>",
                    )
                    .to_compile_error()
                    .into();
                };
                let Some(segment) = path.path.segments.last() else {
                    unreachable!()
                };
                let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                    return syn::Error::new(
                        field.ty.span(),
                        "#[snapshot] field must be Arc<State>",
                    )
                    .to_compile_error()
                    .into();
                };
                let Some(GenericArgument::Type(state)) = arguments.args.first() else {
                    return syn::Error::new(
                        field.ty.span(),
                        "#[snapshot] field must be Arc<State>",
                    )
                    .to_compile_error()
                    .into();
                };
                if segment.ident != "Arc" {
                    return syn::Error::new(
                        field.ty.span(),
                        "#[snapshot] field must be Arc<State>",
                    )
                    .to_compile_error()
                    .into();
                }
                snapshot_ty = Some(state.clone());
                snapshot_field_ident = Some(field_ident);
            } else {
                keep_attrs.push(attr);
            }
        }
        field.attrs = keep_attrs;
    }

    if fields.iter().any(|field| {
        field
            .ident
            .as_ref()
            .is_some_and(|ident| ident == "_snapshot")
    }) {
        return syn::Error::new(
            item_struct.span(),
            "layer structs cannot define a field named _snapshot",
        )
        .to_compile_error()
        .into();
    }

    if let Some(ref snapshot_ty) = snapshot_ty {
        fields.push(parse_quote! {
            pub(crate) _snapshot: ::plingo::scheme::snapshot::SnapshotStore<#snapshot_ty>
        });
    }

    let snapshot_impl = match (snapshot_field_ident, snapshot_ty.as_ref()) {
        (Some(field_ident), Some(snapshot_ty)) => quote! {
            impl #impl_generics ::plingo::scheme::layer::SnapshotLayer for #self_ident #ty_generics #where_clause {
                type State = #snapshot_ty;

                fn initialize_snapshots(&mut self) {
                    self._snapshot.initialize(::std::sync::Arc::clone(&self.#field_ident));
                }

                fn push_state(&mut self, snapshot: ::plingo::scheme::context::SnapshotId) {
                    self._snapshot.insert(snapshot, ::std::sync::Arc::clone(&self.#field_ident));
                }

                fn rollback_state(&mut self, revision: ::plingo::scheme::change::Revision) -> bool {
                    let Some(state) = self._snapshot.rollback(revision) else {
                        return false;
                    };
                    self.#field_ident = state;
                    true
                }

                fn state(
                    &self,
                    snapshot: ::std::option::Option<::plingo::scheme::context::SnapshotId>,
                ) -> ::std::option::Option<&Self::State> {
                    match snapshot {
                        Some(snapshot) => self._snapshot.get(snapshot),
                        None => Some(self.#field_ident.as_ref()),
                    }
                }

                fn latest_state(&self) -> &Self::State {
                    self.#field_ident.as_ref()
                }

                fn latest_state_mut(&mut self) -> &mut Self::State {
                    ::std::sync::Arc::make_mut(&mut self.#field_ident)
                }

                fn set_snapshot_retention(&mut self, retention: ::plingo::scheme::snapshot::SnapshotRetention) {
                    self._snapshot.set_retention(retention);
                }

                fn snapshot_retention(&self) -> ::plingo::scheme::snapshot::SnapshotRetention {
                    self._snapshot.retention()
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
            impl #impl_generics ::plingo::scheme::layer::FallibleLayer for #self_type #where_clause {
                type __Error = <Self as ::plingo::scheme::layer::TopLayer>::Error;
            }
        },
        LayerRole::Middle => quote! {
            impl #impl_generics ::plingo::scheme::layer::FallibleLayer for #self_type #where_clause {
                type __Error = <Self as ::plingo::scheme::layer::MiddleLayer>::Error;
            }
            impl #impl_generics ::plingo::scheme::layer::NonTopLayer for #self_type #where_clause {
                type _Error = <Self as ::plingo::scheme::layer::MiddleLayer>::Error;
                type Address = <Self as ::plingo::scheme::layer::MiddleLayer>::Address;
                type Unit = <Self as ::plingo::scheme::layer::MiddleLayer>::Unit;
            }
        },
        LayerRole::Bottom => quote! {
            impl #impl_generics ::plingo::scheme::layer::FallibleLayer for #self_type #where_clause {
                type __Error = <Self as ::plingo::scheme::layer::BottomLayer>::Error;
            }
            impl #impl_generics ::plingo::scheme::layer::NonTopLayer for #self_type #where_clause {
                type _Error = <Self as ::plingo::scheme::layer::BottomLayer>::Error;
                type Address = <Self as ::plingo::scheme::layer::BottomLayer>::Address;
                type Unit = <Self as ::plingo::scheme::layer::BottomLayer>::Unit;
            }
        },
    };

    quote! {
        #item_impl
        #conduit_impls
    }
    .into()
}
