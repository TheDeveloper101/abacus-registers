use crate::{
    macro_input::MacroInput,
    parsing::{Cost, RegisterAttributes, RegisterType, State},
};
use std::collections::BTreeMap;
use syn::{spanned::Spanned, DeriveInput, Error, Result};

#[derive(Clone)]
pub struct StateInfo {
    pub ty: String,
    pub cost: Cost,
    pub transient: bool,
}
#[derive(Clone)]
pub struct Operation {
    pub target: String,
    pub cost: Cost,
    pub transition: bool,
}
#[derive(Clone)]
pub struct Peripheral {
    pub registers: String,
    pub state_enum: String,
    pub states: BTreeMap<String, StateInfo>,
    // (source state shortname, generated method name)
    pub operations: BTreeMap<(String, String), Operation>,
}
impl Peripheral {
    pub fn is_symbolic(&self) -> bool {
        self.states.values().any(|state| state.cost.is_symbolic())
            || self
                .operations
                .values()
                .any(|operation| operation.cost.is_symbolic())
    }

    pub fn new(input: MacroInput, item: DeriveInput) -> Result<Self> {
        let mut model = Self {
            registers: format!("{}Registers", input.peripheral_name),
            state_enum: format!("{}StateEnum", input.peripheral_name),
            states: BTreeMap::new(),
            operations: BTreeMap::new(),
        };
        for state in &input.state_definitions {
            if state.state.contains_any() {
                return Err(Error::new(
                    item.span(),
                    "declare concrete states; use Any only in RegAttributes",
                ));
            }
            let info = StateInfo {
                ty: state
                    .state
                    .form_concrete_state_type()
                    .to_string()
                    .replace(' ', ""),
                cost: state.cost.clone(),
                transient: state.transient,
            };
            if model
                .states
                .insert(state.state_shortname.to_string(), info)
                .is_some()
            {
                return Err(Error::new(item.span(), "duplicate state shortname"));
            }
        }
        let syn::Data::Struct(data) = &item.data else {
            return Err(Error::new(item.span(), "expected a register struct"));
        };
        for field in &data.fields {
            for attribute in field
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("RegAttributes"))
            {
                let attribute: RegisterAttributes = attribute.parse_args()?;
                for pattern in &attribute.valid_states {
                    let mut matched = false;
                    for source in &input.state_definitions {
                        let Some(bindings) = match_state(pattern, &source.state) else {
                            continue;
                        };
                        matched = true;
                        let source_name = source.state_shortname.to_string();
                        let mut add =
                            |name: String, target: String, transition: bool| -> Result<()> {
                                let operation = Operation {
                                    target,
                                    transition,
                                    cost: attribute.cost.clone(),
                                };
                                if model
                                    .operations
                                    .insert((source_name.clone(), name), operation)
                                    .is_some()
                                {
                                    return Err(Error::new(
                                        field.span(),
                                        "overlapping register operation specifications",
                                    ));
                                }
                                Ok(())
                            };
                        match &attribute.register_type {
                            RegisterType::StateChange(target, _, _) => {
                                let mut target = target.clone();
                                let mut bindings = bindings.iter();
                                for substate in &mut target.substates {
                                    if substate == "Any" {
                                        *substate = bindings
                                            .next()
                                            .ok_or_else(|| {
                                                Error::new(
                                                    field.span(),
                                                    "target Any needs a source binding",
                                                )
                                            })?
                                            .clone();
                                    }
                                }
                                let target = input
                                    .state_definitions
                                    .iter()
                                    .find(|s| s.state == target)
                                    .ok_or_else(|| {
                                        Error::new(
                                            field.span(),
                                            "StateChange target is not declared",
                                        )
                                    })?;
                                add(
                                    attribute
                                        .register_type
                                        .state_change_method()
                                        .ok_or_else(|| {
                                            Error::new(
                                                field.span(),
                                                "expected a StateChange operation",
                                            )
                                        })?
                                        .to_string(),
                                    target.state_shortname.to_string(),
                                    true,
                                )?;
                            }
                            kind => {
                                let methods: &[&str] = match kind {
                                    RegisterType::ReadOnly => &["get", "read"],
                                    RegisterType::WriteOnly if pattern.contains_any() => {
                                        &["set", "write"]
                                    }
                                    RegisterType::WriteOnly => &["set", "write", "modify_no_read"],
                                    RegisterType::ReadWrite => &[
                                        "get",
                                        "read",
                                        "set",
                                        "write",
                                        "modify",
                                        "modify_no_read",
                                        "is_set",
                                    ],
                                    _ => unreachable!(),
                                };
                                let field_name = field.ident.as_ref().ok_or_else(|| {
                                    Error::new(field.span(), "expected named register field")
                                })?;
                                for method in methods {
                                    add(
                                        format!("{field_name}.{method}"),
                                        source_name.clone(),
                                        false,
                                    )?;
                                }
                            }
                        }
                    }
                    if !matched {
                        return Err(Error::new(
                            field.span(),
                            "valid-state pattern matches no declared state",
                        ));
                    }
                }
            }
        }
        Ok(model)
    }
}

fn match_state(pattern: &State, state: &State) -> Option<Vec<syn::Ident>> {
    if pattern.state_name != state.state_name || pattern.substates.len() != state.substates.len() {
        return None;
    }
    let mut bindings = Vec::new();
    for (pattern, state) in pattern.substates.iter().zip(&state.substates) {
        if pattern == "Any" {
            bindings.push(state.clone());
        } else if pattern != state {
            return None;
        }
    }
    Some(bindings)
}
