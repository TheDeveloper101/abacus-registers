//! Concrete checking over the same declarations that generate Abacus typestates.
pub(crate) mod api;
pub(crate) mod model;

use crate::{
    macro_input::MacroInput,
    parsing::{RegisterType, State},
    register::Register,
};
use model::{error, Operation, Peripheral, Type};
use proc_macro2::TokenStream;
use quote::quote;
use std::collections::{BTreeMap, BTreeSet};
use syn::{Error, Result};

/// Expand source `Any` substates against a declared concrete state. The captured
/// variables follow the same left-to-right convention as register generation.
fn bindings(pattern: &State, concrete: &State) -> Option<Vec<syn::Ident>> {
    if pattern.state_name != concrete.state_name
        || pattern.substates.len() != concrete.substates.len()
    {
        return None;
    }
    let mut result = Vec::new();
    for (expected, actual) in pattern.substates.iter().zip(&concrete.substates) {
        if expected == "Any" {
            result.push(actual.clone());
        } else if expected != actual {
            return None;
        }
    }
    Some(result)
}
fn instantiate(target: &State, variables: &[syn::Ident]) -> Result<State> {
    let mut target = target.clone();
    let mut variables = variables.iter();
    for state in &mut target.substates {
        if state == "Any" {
            *state = variables
                .next()
                .ok_or_else(|| error("target Any has no matching source substate"))?
                .clone();
        }
    }
    Ok(target)
}

fn peripheral(input: &MacroInput, registers: &[Register]) -> Result<Peripheral> {
    let mut background = BTreeMap::new();
    let mut transient = BTreeSet::new();
    for state in &input.state_definitions {
        if state.state.substates.iter().any(|s| s == "Any") {
            return Err(error(
                "declare concrete states; Any belongs in RegAttributes patterns",
            ));
        }
        let name = state.state_shortname.to_string();
        if background
            .insert(name.clone(), state.background_cost)
            .is_some()
        {
            return Err(error("duplicate state shortname in checked register block"));
        }
        if state.transient {
            transient.insert(name);
        }
    }
    let mut operations = BTreeMap::new();
    for reg in registers {
        for pattern in &reg.valid_states {
            let sources: Vec<_> = input
                .state_definitions
                .iter()
                .filter_map(|state| bindings(pattern, &state.state).map(|vars| (state, vars)))
                .collect();
            if sources.is_empty() {
                return Err(Error::new(
                    reg.name.span(),
                    "register valid-state pattern matches no declared state",
                ));
            }
            for (source, variables) in sources {
                let mut add =
                    |name: String, target: String, result: Type, arity: usize| -> Result<()> {
                        let key = (name, source.state_shortname.to_string());
                        if operations
                            .insert(
                                key,
                                Operation {
                                    target,
                                    cost: reg.cost,
                                    result,
                                    arity,
                                },
                            )
                            .is_some()
                        {
                            return Err(Error::new(
                                reg.name.span(),
                                "ambiguous register operation for the same source state",
                            ));
                        }
                        Ok(())
                    };
                match &reg.register_type {
                    RegisterType::StateChange(target, _, _) => {
                        let concrete = instantiate(target, &variables)?;
                        let target = input
                            .state_definitions
                            .iter()
                            .find(|s| s.state == concrete)
                            .ok_or_else(|| {
                                Error::new(
                                    reg.name.span(),
                                    "StateChange target is not a declared state",
                                )
                            })?;
                        add(
                            reg.register_type.state_change_method().unwrap().to_string(),
                            target.state_shortname.to_string(),
                            Type::Unit,
                            0,
                        )?;
                    }
                    other => {
                        let methods: &[(&str, Type, usize)] = match other {
                            RegisterType::ReadOnly => {
                                &[("get", Type::Int, 0), ("read", Type::Int, 1)]
                            }
                            RegisterType::WriteOnly
                                if pattern.substates.iter().any(|s| s == "Any") =>
                            {
                                &[("set", Type::Unit, 1), ("write", Type::Unit, 1)]
                            }
                            RegisterType::WriteOnly => &[
                                ("set", Type::Unit, 1),
                                ("write", Type::Unit, 1),
                                ("modify_no_read", Type::Unit, 2),
                            ],
                            RegisterType::ReadWrite => &[
                                ("get", Type::Int, 0),
                                ("read", Type::Int, 1),
                                ("set", Type::Unit, 1),
                                ("write", Type::Unit, 1),
                                ("modify", Type::Unit, 1),
                                ("modify_no_read", Type::Unit, 2),
                                ("is_set", Type::Bool, 1),
                            ],
                            _ => unreachable!(),
                        };
                        for (method, ty, arity) in methods {
                            add(
                                format!("{}.{method}", reg.name),
                                source.state_shortname.to_string(),
                                *ty,
                                *arity,
                            )?;
                        }
                    }
                }
            }
        }
    }
    Ok(Peripheral {
        background,
        transient,
        operations,
        register_type: format!("{}Registers", input.peripheral_name),
        enum_type: format!("{}StateEnum", input.peripheral_name),
        state_types: input
            .state_definitions
            .iter()
            .map(|s| {
                (
                    s.state
                        .form_concrete_state_type()
                        .to_string()
                        .replace(' ', ""),
                    s.state_shortname.to_string(),
                )
            })
            .collect(),
    })
}

/// Macro metadata travels through token expansion, never process-global state.
/// The public re-export can be selected from a different module or crate.
pub fn specification(input: &MacroInput, ast: &syn::DeriveInput) -> TokenStream {
    let name = &input.peripheral_name;
    let states = input.state_definitions.iter().map(|s| {
        let base = &s.state.state_name;
        let subs = &s.state.substates;
        let state = if subs.is_empty() {
            quote!(#base)
        } else {
            let short = &s.state_shortname;
            quote!(#base(#subs), #short)
        };
        let transient = s.transient.then(|| quote!(, *T*));
        let cost = s
            .background_cost
            .map(|model::Cost(n, d)| quote!(, cost = #n / #d));
        quote!((#state #transient #cost))
    });
    quote! { ((peripheral_name = #name, register_base_addr = 0, states = [#(#states),*]) (#ast)) }
}

pub fn metadata(input: &MacroInput, ast: &syn::DeriveInput) -> TokenStream {
    let public = quote::format_ident!("{}Cost", input.peripheral_name);
    let internal = quote::format_ident!("__abacus_{}_cost_metadata", input.peripheral_name);
    let spec = specification(input, ast);
    quote! {
        #[doc(hidden)]
        #[macro_export]
        macro_rules! #internal {
            ([$($models:tt)*] [$($remaining:tt)*] {$($item:tt)*}) => {
                ::abacus_registers::__abacus_cost! {
                    [$($models)*
                        #spec
                    ]
                    [$($remaining)*]
                    {$($item)*}
                }
            };
        }
        #[doc(hidden)]
        pub use #internal as #public;
    }
}

#[cfg(test)]
mod tests;
