mod generate;
mod non_terminal;
mod shared;
mod terminal;

use proc_macro::TokenStream;
use syn::{ItemEnum, parse_macro_input};

#[proc_macro]
pub fn generate(input: TokenStream) -> TokenStream {
    generate::expand_generate(input)
}

#[proc_macro_derive(
    Terminal,
    attributes(
        scope_slots,
        scopes,
        regex,
        empty,
        one_of,
        enter,
        exit,
        with,
        leave,
        leave_when,
        recover_when,
        when,
        skip,
        parse,
        error
    )
)]
pub fn derive_terminal(item: TokenStream) -> TokenStream {
    match terminal::expand_terminal_derive(parse_macro_input!(item as ItemEnum)) {
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
