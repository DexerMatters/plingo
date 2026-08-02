mod generate;
mod non_terminal;
mod pretty;
mod scope_domain;
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
pub fn lregex(input: TokenStream) -> TokenStream {
    match scope_regex::expand_label_regex(input.into()) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Builds a [`ScopePath`](::plingo::component::scope::ScopePath) for
/// `ScopeView::query_from`, using regex-style alternation (`|`),
/// concatenation, grouping, and `*`/`+`/`?`.
#[proc_macro]
pub fn scope_path(input: TokenStream) -> TokenStream {
    match scope_regex::expand_scope_path(input.into()) {
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
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}


#[proc_macro_derive(PrettyNonTerminal)]
pub fn derive_pretty_non_terminal(item: TokenStream) -> TokenStream {
    match pretty::expand_pretty_non_terminal_derive(parse_macro_input!(item as ItemEnum)) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error().into(),
    }
}

#[proc_macro_derive(PrettyTerminal)]
pub fn derive_pretty_terminal(item: TokenStream) -> TokenStream {
    match pretty::expand_pretty_terminal_derive(parse_macro_input!(item as ItemEnum)) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error().into(),
    }
}

/// Implements [`ScopeDomain`](::plingo::component::scope::ScopeDomain) from
/// explicit associated-type attributes.
#[proc_macro_derive(ScopeDomain, attributes(scope_domain))]
pub fn derive_scope_domain(item: TokenStream) -> TokenStream {
    match scope_domain::expand_scope_domain(parse_macro_input!(item as syn::DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}
