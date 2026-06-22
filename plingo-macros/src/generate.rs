use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Expr, Ident, Path, Token,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

struct GenerateInput {
    variant_path: Path,
    seed: Expr,
    dest: Expr,
}

impl Parse for GenerateInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let variant_path = input.parse()?;
        input.parse::<Token![,]>()?;
        let seed = input.parse()?;
        input.parse::<Token![,]>()?;
        let dest = input.parse()?;
        Ok(Self {
            variant_path,
            seed,
            dest,
        })
    }
}

pub fn expand_generate(input: TokenStream) -> TokenStream {
    let GenerateInput {
        mut variant_path,
        seed,
        dest,
    } = parse_macro_input!(input as GenerateInput);

    let Some(variant_segment) = variant_path.segments.pop() else {
        return syn::Error::new_spanned(&variant_path, "generate! requires `EnumPath::Variant`")
            .to_compile_error()
            .into();
    };
    variant_path.segments.pop_punct();

    if variant_path.segments.is_empty() {
        return syn::Error::new_spanned(
            &variant_path,
            "generate! requires `EnumPath::Variant`",
        )
        .to_compile_error()
        .into();
    }

    let variant_ident: Ident = variant_segment.into_value().ident;

    quote! {
        #variant_path :: __plingo_generate_variant(
            stringify!(#variant_ident),
            #seed,
            #dest,
        )
    }
    .into()
}
