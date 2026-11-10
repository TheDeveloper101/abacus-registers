use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{
    bracketed,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    Ident, Path, Token,
};

use std::collections::{hash_set::HashSet, HashMap};

use crate::generating;
use crate::{
    parsing::{Cost, State, StateDefinition},
    rational::Rational,
};

mod custom_keywords {
    syn::custom_keyword!(peripheral_name);
    syn::custom_keyword!(registers);
    syn::custom_keyword!(states);
    syn::custom_keyword!(register_base_addr);
    syn::custom_keyword!(backend);
    syn::custom_keyword!(external);
    syn::custom_keyword!(transitions);
    syn::custom_keyword!(clock_costs);
}

pub enum Backend {
    Internal,
    External(Path),
}

pub struct Transition {
    pub from: State,
    pub to: State,
}

pub struct MacroInput {
    pub peripheral_name: String,
    pub backend: Backend,
    pub state_definitions: Vec<StateDefinition>,
    pub transitions: Vec<Transition>,
    pub clock_costs: Vec<ClockCost>,
}

pub struct ClockCost {
    pub from: Rational,
    pub to: Rational,
    pub cost: Cost,
}

fn rational(input: ParseStream) -> syn::Result<Rational> {
    let numerator = input.parse::<syn::LitInt>()?.base10_parse::<u128>()?;
    let denominator = if input.peek(Token![/]) {
        input.parse::<Token![/]>()?;
        input.parse::<syn::LitInt>()?.base10_parse::<u128>()?
    } else {
        1
    };
    Rational::new(numerator, denominator)
        .ok_or_else(|| input.error("clock speeds must be positive rational numbers"))
}

impl MacroInput {
    pub fn generate_states(
        &self,
        register_name: &Ident,
        state_enum_name: &Ident,
    ) -> syn::Result<(proc_macro2::TokenStream, HashMap<String, State>)> {
        let mut output = proc_macro2::TokenStream::new();

        // The other state hashes include the substates. This is strictly
        // looking at the State name
        let mut strict_state_hash = HashSet::new();

        let mut created_states: HashSet<syn::Ident> = HashSet::new();

        let mut unique_substates = HashSet::new();
        unique_substates.insert(format_ident!("Any"));

        let mut state_hash = HashSet::new();

        let mut state_map = HashMap::new();
        for state_def in &self.state_definitions {
            let state = state_def.state.clone();
            if !strict_state_hash.contains(state.state_name.to_string().as_str()) {
                if state.substates.is_empty() {
                    output.extend(quote! {});
                } else if state.substates.len() == 1 {
                    output.extend(quote! {})
                } else if state.substates.len() == 2 {
                    output.extend(quote! {});
                } else {
                    return Err(syn::Error::new(
                        state.state_name.span(),
                        "only two substates are supported",
                    ));
                }

                strict_state_hash.insert(state.state_name.to_string());
            }

            // State hash map used for name mapping later.
            state_map.insert(state.form_state_marker_type().to_string(), state.clone());

            let mut state = state.clone();
            let original_substates = state.substates.clone();

            // We need to generate each substate as:
            // 1. The specified state.
            // 2. As potentially an any state.
            // 3. As potentially all being any states.
            // TODO: We need to add logic for if there are 3 substates (e.g. <Any, Any, Tx>)
            for iter in 0..(&state.substates.len() + 2) {
                let mut _any_positions: Option<Vec<(usize, Ident)>> = None;

                // Case (1)
                state.substates = original_substates.clone();

                // Case (2)
                if iter < state.substates.len() {
                    _any_positions = Some(vec![(iter, state.substates[iter].clone())]);
                    state.substates[iter] = format_ident!("Any");
                }

                // Case (3)
                if iter == state.substates.len() {
                    // Update substates to all be "Any" and record positions with prior ident value
                    let mut vec: Vec<(usize, Ident)> = Vec::new();
                    for (pos, substate) in state.substates.iter().enumerate() {
                        vec.push((pos, substate.clone()));
                    }

                    _any_positions = Some(vec);
                    state
                        .substates
                        .iter_mut()
                        .for_each(|substate| *substate = format_ident!("Any"));
                }

                let state_ident = state.state_name.clone();

                for substate in &state.substates {
                    unique_substates.insert(substate.clone());
                }

                let struct_name = if state.substates.is_empty() {
                    quote! {#state_ident}
                } else {
                    let generic_params = state.substates.iter().enumerate().map(|(index, _)| {
                        let entry = format!("T{}", index);
                        let generic = syn::Ident::new(&entry, Span::call_site());

                        quote! {
                            #generic: SubState
                        }
                    });

                    quote! {
                        #state_ident<#(#generic_params),*>
                    }
                };

                let fields = state.substates.iter().enumerate().map(|(index, _)| {
                    let field_name = format!("associated_{}", index);
                    let generic_name = format!("T{}", index);

                    let generic = syn::Ident::new(&generic_name, Span::call_site());
                    let field = syn::Ident::new(&field_name, Span::call_site());

                    quote! {
                        #field: PhantomData<#generic>
                    }
                });

                // To avoid duplicate implementations for a type, check if it has already been used.
                let concrete_state_str = state.form_concrete_state_type().to_string();
                if state_hash.contains(&concrete_state_str) {
                    // if any_positions.is_some() {
                    //     output.extend(state.generate_state(register_name, store_name, &struct_name, all_states_vec.clone(), any_positions, true));
                    // }
                    continue; // Skip creating this state, as it has already been created.
                } else {
                    state_hash.insert(concrete_state_str.clone());
                }

                if !created_states.contains(&state.state_name) {
                    output.extend(quote! {
                        #[allow(missing_docs)]
                        pub struct #struct_name {
                            #(#fields),*
                        }
                    });

                    created_states.insert(state.state_name.clone());
                }

                // Generate State/Reg/From/TryFrom impls. Avoid overlapping impls (E0119):
                // - Concrete states without substates (e.g. Off): full impls
                // - Concrete states with substates (e.g. Active<RxIdle, TxIdle>): From/TryFrom
                //   only (State/Reg provided by generic impl from (Any, Any))
                // - Fully generic (Any, Any): State and Reg only
                // - Partially generic (Any, TxIdle), (RxIdle, Any): skip
                let is_concrete = !state.contains_any();
                let is_fully_generic = state.substates.iter().all(|s| s.to_string() == "Any");
                let has_substates = !state.substates.is_empty();
                if is_concrete {
                    let synthetic_state_def = StateDefinition {
                        state: state.clone(),
                        transient: state_def.transient,
                        state_shortname: state_def.state_shortname.clone(),
                        cost: state_def.cost.clone(),
                    };
                    if has_substates {
                        output.extend(generating::generate_state_conversion_impls_only(
                            &synthetic_state_def,
                            state_enum_name,
                            register_name,
                        ));
                    } else {
                        output.extend(generating::generate_state(
                            &synthetic_state_def,
                            state_enum_name,
                            register_name,
                        ));
                    }
                } else if is_fully_generic {
                    let synthetic_state_def = StateDefinition {
                        state: state.clone(),
                        transient: state_def.transient,
                        state_shortname: state_def.state_shortname.clone(),
                        cost: state_def.cost.clone(),
                    };
                    output.extend(generating::generate_state_trait_impls_only(
                        &synthetic_state_def,
                        register_name,
                        state_enum_name,
                    ));
                }
            }
        }

        // create substates
        for substate in unique_substates {
            let substate_ident = format_ident!("{}", substate);

            output.extend(quote! {
                #[allow(missing_docs)]
                pub struct #substate_ident {}

                impl SubState for #substate_ident {}
            });
        }

        Ok((output, state_map))
    }
}

impl Parse for MacroInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let _: custom_keywords::peripheral_name = input.parse().map_err(|_| {
            syn::Error::new(
                input.span(),
                "Abacus Macro - Expected 'peripheral_name' keyword",
            )
        })?;
        let _: syn::Token![=] = input.parse().map_err(|_| {
            syn::Error::new(
                input.span(),
                "Abacus Macro - Expected '=' after peripheral_name",
            )
        })?;
        let peripheral_name: syn::LitStr = input.parse().map_err(|_| {
            syn::Error::new(
                input.span(),
                "Abacus Macro - Expected string literal for peripheral_name",
            )
        })?;
        syn::parse_str::<Ident>(&peripheral_name.value()).map_err(|_| {
            syn::Error::new(
                peripheral_name.span(),
                "peripheral_name must be a Rust identifier",
            )
        })?;
        let _: syn::Token![,] = input.parse().map_err(|_| {
            syn::Error::new(
                input.span(),
                "Abacus Macro - Expected ',' after peripheral_name value",
            )
        })?;

        let backend = if input.peek(custom_keywords::backend) {
            input.parse::<custom_keywords::backend>()?;
            input.parse::<Token![=]>()?;
            input.parse::<custom_keywords::external>()?;
            let content;
            syn::parenthesized!(content in input);
            Backend::External(content.parse()?)
        } else {
            let _: custom_keywords::register_base_addr = input.parse().map_err(|_| {
                syn::Error::new(
                    input.span(),
                    "Abacus Macro - Expected 'register_base_addr' or 'backend = external(...)'",
                )
            })?;
            input.parse::<Token![=]>()?;
            input.parse::<syn::LitInt>().map_err(|_| {
                syn::Error::new(
                    input.span(),
                    "Abacus Macro - Expected integer literal for register_base_addr",
                )
            })?;
            Backend::Internal
        };
        let _: syn::Token![,] = input.parse().map_err(|_| {
            syn::Error::new(
                input.span(),
                "Abacus Macro - Expected ',' after backend specification",
            )
        })?;

        let _: custom_keywords::states = input.parse().map_err(|_| {
            syn::Error::new(input.span(), "Abacus Macro - Expected 'states' keyword")
        })?;
        let _: syn::Token![=] = input.parse().map_err(|_| {
            syn::Error::new(input.span(), "Abacus Macro - Expected '=' after states")
        })?;
        let states_content;
        let _: syn::token::Bracket = bracketed!(states_content in input);
        let state_definitions: Punctuated<StateDefinition, syn::Token![,]> = states_content
            .parse_terminated(StateDefinition::parse, syn::Token![,])
            .map_err(|err| {
                syn::Error::new(
                    states_content.span(),
                    format!(
                        "Abacus Macro - Error parsing state definitions list: {}",
                        err
                    ),
                )
            })?;

        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }

        let transitions = if input.peek(custom_keywords::transitions) {
            input.parse::<custom_keywords::transitions>()?;
            input.parse::<Token![=]>()?;
            let content;
            bracketed!(content in input);
            let transitions = content.parse_terminated(
                |entry| {
                    let from = entry.parse()?;
                    entry.parse::<Token![=>]>()?;
                    let to = entry.parse()?;
                    Ok(Transition { from, to })
                },
                Token![,],
            )?;
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
            transitions.into_iter().collect()
        } else {
            Vec::new()
        };

        let clock_costs = if input.peek(custom_keywords::clock_costs) {
            input.parse::<custom_keywords::clock_costs>()?;
            input.parse::<Token![:]>()?;
            let content;
            bracketed!(content in input);
            let entries = content.parse_terminated(
                |entry| {
                    let pair;
                    syn::parenthesized!(pair in entry);
                    let from = rational(&pair)?;
                    pair.parse::<Token![,]>()?;
                    let to = rational(&pair)?;
                    if !pair.is_empty() {
                        return Err(pair.error("expected `(from, to)`"));
                    }
                    entry.parse::<Token![:]>()?;
                    let cost = entry.parse()?;
                    Ok(ClockCost { from, to, cost })
                },
                Token![,],
            )?;
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
            entries.into_iter().collect()
        } else {
            Vec::new()
        };

        if !input.is_empty() {
            return Err(input.error("unexpected process_register_block option"));
        }
        if matches!(backend, Backend::Internal) && !transitions.is_empty() {
            return Err(input.error("transitions is only valid with an external backend"));
        }
        Ok(MacroInput {
            peripheral_name: peripheral_name.value(),
            backend,
            state_definitions: state_definitions.into_iter().collect(),
            transitions,
            clock_costs,
        })
    }
}
