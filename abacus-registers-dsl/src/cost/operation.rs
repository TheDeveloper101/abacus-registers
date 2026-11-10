use super::model::{Operation, Peripheral};
use crate::parsing::Cost;
use std::collections::BTreeMap;
use syn::{
    parse::{Parse, ParseStream},
    spanned::Spanned,
    Attribute, Error, FnArg, Ident, Item, LitInt, Path, Result, Token, Type,
};

#[derive(Clone, Debug)]
pub(super) struct Contract {
    pub(super) model: usize,
    pub(super) from: String,
    pub(super) to: String,
    pub(super) cost: Cost,
    pub(super) operation: String,
    pub(super) background_steps: usize,
    pub(super) parameter_types: Vec<String>,
}

struct Arguments {
    peripheral: Path,
    from: Ident,
    to: Ident,
    cost: Cost,
    background_steps: usize,
}

impl Parse for Arguments {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut peripheral = None;
        let mut from = None;
        let mut to = None;
        let mut cost = None;
        let mut background_steps = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "peripheral" => peripheral = Some(input.parse()?),
                "from" => from = Some(input.parse()?),
                "to" => to = Some(input.parse()?),
                "cost" => cost = Some(input.parse()?),
                "background_steps" => {
                    background_steps = Some(input.parse::<LitInt>()?.base10_parse()?)
                }
                _ => return Err(Error::new(key.span(), "unknown heliod_operation argument")),
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        Ok(Self {
            peripheral: peripheral.ok_or_else(|| input.error("missing `peripheral`"))?,
            from: from.ok_or_else(|| input.error("missing `from`"))?,
            to: to.ok_or_else(|| input.error("missing `to`"))?,
            cost: cost.ok_or_else(|| input.error("missing `cost`"))?,
            background_steps: background_steps.unwrap_or(1),
        })
    }
}

fn attribute(attributes: &[Attribute]) -> Result<Option<Arguments>> {
    let mut found = None;
    for attribute in attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("heliod_operation"))
    {
        if found.is_some() {
            return Err(Error::new(attribute.span(), "duplicate heliod_operation"));
        }
        found = Some(attribute.parse_args()?);
    }
    Ok(found)
}

pub(super) fn collect(item: &Item, models: &[Peripheral]) -> Result<BTreeMap<String, Contract>> {
    fn visit(
        item: &Item,
        scope: &str,
        models: &[Peripheral],
        contracts: &mut BTreeMap<String, Contract>,
    ) -> Result<()> {
        match item {
            Item::Fn(function) => {
                let Some(arguments) = attribute(&function.attrs)? else {
                    return Ok(());
                };
                let provider = arguments
                    .peripheral
                    .segments
                    .last()
                    .ok_or_else(|| Error::new(arguments.peripheral.span(), "empty provider path"))?
                    .ident
                    .to_string();
                let model = models
                    .iter()
                    .position(|model| {
                        model
                            .registers
                            .strip_suffix("Registers")
                            .is_some_and(|name| format!("{name}Cost") == provider)
                    })
                    .ok_or_else(|| {
                        Error::new(
                            arguments.peripheral.span(),
                            "heliod_operation peripheral is not a selected cost provider",
                        )
                    })?;
                let from = arguments.from.to_string();
                let to = arguments.to.to_string();
                if !models[model].states.contains_key(&from) {
                    return Err(Error::new(arguments.from.span(), "unknown source state"));
                }
                if !models[model].states.contains_key(&to) {
                    return Err(Error::new(arguments.to.span(), "unknown target state"));
                }
                let name = qualify(scope, &function.sig.ident.to_string());
                let operation = format!("__heliod_operation::{name}");
                let parameter_types = function
                    .sig
                    .inputs
                    .iter()
                    .filter_map(|argument| match argument {
                        FnArg::Typed(argument) => type_name(&argument.ty),
                        FnArg::Receiver(_) => None,
                    })
                    .collect();
                if contracts
                    .insert(
                        name,
                        Contract {
                            model,
                            from,
                            to,
                            cost: arguments.cost,
                            operation,
                            background_steps: arguments.background_steps,
                            parameter_types,
                        },
                    )
                    .is_some()
                {
                    return Err(Error::new(
                        function.sig.span(),
                        "duplicate operation contract",
                    ));
                }
            }
            Item::Mod(module) => {
                let Some((_, items)) = &module.content else {
                    return Err(Error::new(
                        module.span(),
                        "the checked module must be inline",
                    ));
                };
                let scope = qualify(scope, &module.ident.to_string());
                for item in items {
                    visit(item, &scope, models, contracts)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let mut contracts = BTreeMap::new();
    visit(item, "", models, &mut contracts)?;
    Ok(contracts)
}

fn type_name(ty: &Type) -> Option<String> {
    let ty = match ty {
        Type::Reference(reference) => reference.elem.as_ref(),
        ty => ty,
    };
    let Type::Path(path) = ty else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

pub(super) fn install(
    contracts: &BTreeMap<String, Contract>,
    models: &mut [Peripheral],
) -> Result<()> {
    for contract in contracts.values() {
        let key = (contract.from.clone(), contract.operation.clone());
        if models[contract.model]
            .operations
            .insert(
                key,
                Operation {
                    target: contract.to.clone(),
                    cost: contract.cost.clone(),
                    transition: contract.from != contract.to,
                },
            )
            .is_some()
        {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "duplicate summarized operation",
            ));
        }
    }
    Ok(())
}

fn qualify(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{scope}::{name}")
    }
}
