use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    Expr, Path, Result, Token, braced, parenthesized,
    parse::{Parse, ParseStream},
};

#[derive(Clone)]
enum Regex {
    Epsilon,
    Label(TokenStream),
    Or(Box<Self>, Box<Self>),
    Then(Box<Self>, Box<Self>),
    Star(Box<Self>),
}

impl Regex {
    fn plus(self) -> Self {
        Self::Then(Box::new(self.clone()), Box::new(Self::Star(Box::new(self))))
    }

    fn optional(self) -> Self {
        Self::Or(Box::new(Self::Epsilon), Box::new(self))
    }

    fn expand(&self) -> TokenStream {
        match self {
            Self::Epsilon => quote!(::plingo::framework::scope::PathExpr::Epsilon),
            Self::Label(label) => quote!(::plingo::framework::scope::PathExpr::label(#label)),
            Self::Or(left, right) => {
                let left = left.expand();
                let right = right.expand();
                quote!((#left).or(#right))
            }
            Self::Then(left, right) => {
                let left = left.expand();
                let right = right.expand();
                quote!((#left).then(#right))
            }
            Self::Star(inner) => {
                let inner = inner.expand();
                quote!((#inner).star())
            }
        }
    }
}

struct RegexInput(Regex);

impl Parse for RegexInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        Ok(Self(parse_alternation(input)?))
    }
}

fn parse_alternation(input: ParseStream<'_>) -> Result<Regex> {
    let mut expression = parse_concatenation(input)?;
    while input.peek(Token![|]) {
        input.parse::<Token![|]>()?;
        expression = Regex::Or(Box::new(expression), Box::new(parse_concatenation(input)?));
    }
    Ok(expression)
}

fn parse_concatenation(input: ParseStream<'_>) -> Result<Regex> {
    let mut expression = Regex::Epsilon;
    let mut has_atom = false;
    while !input.is_empty() && !input.peek(Token![|]) {
        let atom = parse_repetition(input)?;
        expression = if has_atom {
            Regex::Then(Box::new(expression), Box::new(atom))
        } else {
            atom
        };
        has_atom = true;
    }
    Ok(expression)
}

fn parse_repetition(input: ParseStream<'_>) -> Result<Regex> {
    let mut expression = parse_atom(input)?;
    let mut repeated = false;
    loop {
        if input.peek(Token![*]) {
            if repeated {
                return Err(input.error("a label regex atom may have only one repetition operator"));
            }
            input.parse::<Token![*]>()?;
            expression = Regex::Star(Box::new(expression));
            repeated = true;
        } else if input.peek(Token![+]) {
            if repeated {
                return Err(input.error("a label regex atom may have only one repetition operator"));
            }
            input.parse::<Token![+]>()?;
            expression = expression.plus();
            repeated = true;
        } else if input.peek(Token![?]) {
            if repeated {
                return Err(input.error("a label regex atom may have only one repetition operator"));
            }
            input.parse::<Token![?]>()?;
            expression = expression.optional();
            repeated = true;
        } else {
            return Ok(expression);
        }
    }
}

fn parse_atom(input: ParseStream<'_>) -> Result<Regex> {
    if input.peek(syn::token::Paren) {
        let content;
        parenthesized!(content in input);
        return parse_alternation(&content);
    }
    if input.peek(syn::token::Brace) {
        let content;
        braced!(content in input);
        let expression = content.parse::<Expr>()?;
        if !content.is_empty() {
            return Err(content.error("expected one Rust expression in label braces"));
        }
        return Ok(Regex::Label(quote!(#expression)));
    }

    // Paths keep the common `Label::Lexical* Label::Declaration` spelling
    // compact. Braces support labels carrying data: `{ Label::Import(kind) }`.
    let path = input.parse::<Path>()?;
    Ok(Regex::Label(quote!(#path)))
}

pub fn expand_label_regex(input: TokenStream) -> Result<TokenStream> {
    let RegexInput(regex) = syn::parse2(input)?;
    Ok(regex.expand())
}

pub fn expand_scope_path(input: TokenStream) -> Result<TokenStream> {
    let RegexInput(regex) = syn::parse2(input)?;
    let expression = regex.expand();
    Ok(quote!(::plingo::framework::scope::ScopePath::from(#expression)))
}
