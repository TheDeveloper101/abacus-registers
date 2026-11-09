//! Pass the existing register specification to checks on ordinary Rust APIs.
mod burn;
mod checking;
mod diagnostics;
mod inference;
mod model;
mod operation;

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
    braced, bracketed, parenthesized,
    parse::{Parse, ParseStream, Parser},
    punctuated::Punctuated,
    DeriveInput, Item, Path, Token,
};

use crate::macro_input::MacroInput;

pub fn metadata(attributes: TokenStream, item: TokenStream) -> TokenStream {
    let input: MacroInput = match syn::parse2(attributes.clone()) {
        Ok(input) => input,
        Err(error) => return error.into_compile_error(),
    };
    let public = format_ident!("{}Cost", input.peripheral_name);
    let exported = format_ident!("__heliod_{}_cost", input.peripheral_name);
    quote! {
        #[doc(hidden)]
        #[macro_export]
        macro_rules! #exported {
            ([$($models:tt)*] [$($remaining:tt)*] {$($api:tt)*}) => {
                ::abacus_registers::__heliod! {
                    [$($models)* ((#attributes) (#item))]
                    [$($remaining)*]
                    {$($api)*}
                }
            };
        }
        pub use #exported as #public;
    }
}

pub fn start(attributes: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let providers = Punctuated::<Path, Token![,]>::parse_terminated.parse2(attributes)?;
    if providers.is_empty() {
        return Err(syn::Error::new_spanned(
            item,
            "select a cost provider, e.g. #[heliod(SensorCost)]",
        ));
    }
    Ok(quote! { ::abacus_registers::__heliod! { [] [#providers] {#item} } })
}

struct Request {
    models: TokenStream,
    providers: Punctuated<Path, Token![,]>,
    item: Item,
}
impl Parse for Request {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let models;
        bracketed!(models in input);
        let providers;
        bracketed!(providers in input);
        let item;
        braced!(item in input);
        Ok(Self {
            models: models.parse()?,
            providers: providers.parse_terminated(Path::parse, Token![,])?,
            item: item.parse()?,
        })
    }
}
struct Specifications(Vec<model::Peripheral>);
impl Parse for Specifications {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut models = Vec::new();
        while !input.is_empty() {
            let entry;
            parenthesized!(entry in input);
            let attributes;
            parenthesized!(attributes in entry);
            let item;
            parenthesized!(item in entry);
            let attributes: MacroInput = attributes.parse()?;
            let item: DeriveInput = item.parse()?;
            let model = model::Peripheral::new(attributes, item)?;
            if models
                .iter()
                .any(|m: &model::Peripheral| m.registers == model.registers)
            {
                return Err(entry.error("cost providers must have distinct peripheral names"));
            }
            models.push(model);
        }
        Ok(Self(models))
    }
}

pub fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let Request {
        models,
        providers,
        item,
    } = syn::parse2(input)?;
    if let Some(provider) = providers.first() {
        let remaining = providers.iter().skip(1);
        return Ok(quote! { #provider! { [#models] [#(#remaining),*] {#item} } });
    }
    let Specifications(mut models) = syn::parse2(models)?;
    let operations = operation::collect(&item, &models)?;
    operation::install(&operations, &mut models)?;
    let report = checking::analyze_with_operations(&item, &models, &operations)?;
    for (name, cost) in report.reported_costs {
        eprintln!("Heliod cost for {name}: {cost}");
    }
    let warnings = diagnostics::warnings(report.diagnostics);
    Ok(quote! { #item #warnings })
}

#[cfg(test)]
mod tests;
