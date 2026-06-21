mod context_callable;
mod layer;
mod non_terminal;
mod shared;
mod tokens;

use proc_macro::TokenStream;
use syn::{ItemEnum, parse_macro_input};

#[proc_macro_attribute]
pub fn layer(attr: TokenStream, item: TokenStream) -> TokenStream {
    if attr.is_empty() {
        layer::expand_layer_struct(item)
    } else {
        layer::expand_layer_impl(attr, item)
    }
}

#[proc_macro_attribute]
pub fn context_callable(_attr: TokenStream, item: TokenStream) -> TokenStream {
    context_callable::expand_context_callable(item)
}

#[proc_macro_attribute]
pub fn tokens(_attr: TokenStream, item: TokenStream) -> TokenStream {
    match tokens::expand_tokens_attr(parse_macro_input!(item as ItemEnum)) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error().into(),
    }
}

#[proc_macro_derive(NonTerminal, attributes(rule, from, parse_err))]
pub fn derive_non_terminal(item: TokenStream) -> TokenStream {
    match non_terminal::expand_non_terminal_derive(parse_macro_input!(item as ItemEnum)) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error().into(),
    }
}
