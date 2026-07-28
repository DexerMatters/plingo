mod generate;
mod non_terminal;
mod scope_regex;
mod shared;
mod terminal;

use proc_macro::TokenStream;
use syn::{ItemEnum, parse_macro_input};

#[proc_macro]
pub fn generate(input: TokenStream) -> TokenStream {
    generate::expand_generate(input)
}

/// Builds a typed [`PathExpr`](::plingo::component::scope::PathExpr) with
/// regex-style alternation (`|`), concatenation, grouping, and `*`/`+`/`?`.
/// Label paths are atoms; use `{ expression }` for a non-path label value.
#[proc_macro]
pub fn label_regex(input: TokenStream) -> TokenStream {
    match scope_regex::expand_label_regex(input.into()) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Builds a [`RelativeRegex`](::plingo::component::scope::RelativeRegex) for
/// use with `Here::resolve`, using the same syntax as [`label_regex`].
#[proc_macro]
pub fn relative_label_regex(input: TokenStream) -> TokenStream {
    match scope_regex::expand_relative_label_regex(input.into()) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
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
