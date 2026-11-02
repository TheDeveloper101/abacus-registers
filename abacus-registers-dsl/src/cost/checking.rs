//! Concrete state sets and integer costs. Rust remains responsible for value types
//! and ownership; this pass tracks physical peripherals across Rust bindings.
use super::{
    diagnostics::{self, Diagnostic},
    model::Peripheral,
};
use crate::parsing::Cost;
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use syn::{
    spanned::Spanned, Block, Error, Expr, FnArg, GenericArgument, Item, Pat, PathArguments, Result,
    Signature, Stmt, Type,
};

pub type States = BTreeSet<String>;
#[derive(Clone, PartialEq, Eq)]
enum Value {
    Scalar,
    Register(usize),
    Cell(usize),
    Tuple(Vec<Value>),
    Object(String, BTreeMap<String, Value>),
}
#[derive(Clone)]
pub(super) struct Device {
    pub(super) model: usize,
    pub(super) states: States,
    input_states: States,
}
#[derive(Clone, Default)]
struct Context {
    scope: String,
    devices: Vec<Device>,
    inferred_inputs: VecDeque<(usize, States)>,
    bindings: BTreeMap<String, Value>,
}

#[allow(dead_code)]
#[derive(Clone)]
pub enum ControlFlowSummary {
    Empty,
    Command(CommandSummary),
    Sequence(Vec<ControlFlowSummary>),
    Branch(Vec<ControlFlowSummary>),
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct CommandSummary {
    pub foreground: usize,
    pub background: Vec<BackgroundCharge>,
    pub events: Vec<PeripheralEvent>,
    pub span: proc_macro2::Span,
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct BackgroundCharge {
    pub device: usize,
    pub states: States,
    pub cost: usize,
    input_states: States,
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct PeripheralEvent {
    pub device: usize,
    pub operation: String,
    pub source: States,
    pub target: States,
    pub transition: bool,
    pub cost: usize,
    source_inputs: States,
    pub span: proc_macro2::Span,
}

impl ControlFlowSummary {
    pub fn cost(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Command(command) => {
                command.foreground
                    + command
                        .background
                        .iter()
                        .map(|charge| charge.cost)
                        .sum::<usize>()
            }
            Self::Sequence(parts) => parts.iter().map(Self::cost).sum(),
            Self::Branch(arms) => arms.iter().map(Self::cost).max().unwrap_or(0),
        }
    }
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
enum CostExpression {
    Zero,
    Number(usize),
    Symbol(String),
    Transition(String, String),
    Sum(Vec<CostExpression>),
    Maximum(Vec<CostExpression>),
}

impl CostExpression {
    fn sum(expressions: impl IntoIterator<Item = Self>) -> Self {
        let mut terms = Vec::new();
        let mut number = 0;
        for expression in expressions {
            match expression {
                Self::Zero => {}
                Self::Number(value) => number += value,
                Self::Sum(nested) => terms.extend(nested),
                expression => terms.push(expression),
            }
        }
        if number != 0 {
            terms.push(Self::Number(number));
        }
        terms.sort();
        match terms.len() {
            0 => Self::Zero,
            1 => terms.pop().unwrap(),
            _ => Self::Sum(terms),
        }
    }

    fn maximum(expressions: impl IntoIterator<Item = Self>) -> Self {
        let mut terms = expressions.into_iter().collect::<Vec<_>>();
        terms.sort();
        terms.dedup();
        match terms.len() {
            0 => Self::Zero,
            1 => terms.pop().unwrap(),
            _ => Self::Maximum(terms),
        }
    }
}

impl std::fmt::Display for CostExpression {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zero => write!(formatter, "0"),
            Self::Number(value) => write!(formatter, "{value}"),
            Self::Symbol(name) => write!(formatter, "{name}"),
            Self::Transition(source, target) => write!(formatter, "{source} -> {target}"),
            Self::Maximum(terms) => {
                write!(formatter, "max(")?;
                for (index, term) in terms.iter().enumerate() {
                    if index != 0 {
                        write!(formatter, ", ")?;
                    }
                    write!(formatter, "{term}")?;
                }
                write!(formatter, ")")
            }
            Self::Sum(terms) => {
                let mut index = 0;
                let mut first = true;
                while index < terms.len() {
                    let count = terms[index..]
                        .iter()
                        .take_while(|term| **term == terms[index])
                        .count();
                    if !first {
                        write!(formatter, " + ")?;
                    }
                    if count > 1 {
                        write!(formatter, "{count} * ")?;
                    }
                    if matches!(terms[index], Self::Transition(_, _)) {
                        write!(formatter, "({})", terms[index])?;
                    } else {
                        write!(formatter, "{}", terms[index])?;
                    }
                    first = false;
                    index += count;
                }
                Ok(())
            }
        }
    }
}

fn annotation(cost: &Cost) -> CostExpression {
    match cost {
        Cost::Concrete(0) => CostExpression::Zero,
        Cost::Concrete(value) => CostExpression::Number(*value),
        Cost::Symbolic(name) => CostExpression::Symbol(name.clone()),
    }
}

fn state_label(model: &Peripheral, state: &str) -> String {
    match &model.states[state].cost {
        Cost::Concrete(value) => value.to_string(),
        Cost::Symbolic(name) => name.clone(),
    }
}

fn event_cost(
    event: &PeripheralEvent,
    devices: &[Device],
    models: &[Peripheral],
) -> CostExpression {
    let model = &models[devices[event.device].model];
    if event.transition && model.states.values().any(|state| state.cost.is_symbolic()) {
        return CostExpression::maximum(event.source.iter().map(|source| {
            let target = &model.operations[&(source.clone(), event.operation.clone())].target;
            CostExpression::Transition(state_label(model, source), state_label(model, target))
        }));
    }
    CostExpression::maximum(event.source.iter().map(|source| {
        annotation(&model.operations[&(source.clone(), event.operation.clone())].cost)
    }))
}

fn symbolic_cost(
    summary: &ControlFlowSummary,
    devices: &[Device],
    models: &[Peripheral],
) -> CostExpression {
    match summary {
        ControlFlowSummary::Empty => CostExpression::Zero,
        ControlFlowSummary::Command(command) => CostExpression::sum(
            command
                .events
                .iter()
                .map(|event| event_cost(event, devices, models))
                .chain(command.background.iter().map(|charge| {
                    let model = &models[devices[charge.device].model];
                    CostExpression::maximum(
                        charge
                            .states
                            .iter()
                            .map(|state| annotation(&model.states[state].cost)),
                    )
                })),
        ),
        ControlFlowSummary::Sequence(parts) => CostExpression::sum(
            parts
                .iter()
                .map(|part| symbolic_cost(part, devices, models)),
        ),
        ControlFlowSummary::Branch(arms) => {
            CostExpression::maximum(arms.iter().map(|arm| symbolic_cost(arm, devices, models)))
        }
    }
}

fn sequence(parts: impl IntoIterator<Item = ControlFlowSummary>) -> ControlFlowSummary {
    let mut combined = Vec::new();
    for part in parts {
        match part {
            ControlFlowSummary::Empty => {}
            ControlFlowSummary::Sequence(nested) => combined.extend(nested),
            part => combined.push(part),
        }
    }
    match combined.len() {
        0 => ControlFlowSummary::Empty,
        1 => combined.pop().unwrap(),
        _ => ControlFlowSummary::Sequence(combined),
    }
}

#[derive(Clone)]
struct Evaluation {
    value: Value,
    cost: usize,
    summary: ControlFlowSummary,
    events: Vec<PeripheralEvent>,
    returned: bool,
    call_body_charged: bool,
}
impl Evaluation {
    fn new(value: Value, cost: usize) -> Self {
        Self {
            value,
            cost,
            summary: ControlFlowSummary::Empty,
            events: Vec::new(),
            returned: false,
            call_body_charged: false,
        }
    }
}
#[derive(Clone)]
struct Function {
    signature: Signature,
    body: Block,
    owner: Option<String>,
}

#[derive(Clone)]
pub(super) struct FunctionAnalysis {
    parameters: Vec<Value>,
    pub(super) initial_devices: Vec<Device>,
    final_devices: Vec<Device>,
    result: Value,
    pub(super) summary: ControlFlowSummary,
}
pub struct Report {
    #[allow(dead_code)]
    pub costs: Vec<(String, usize)>,
    pub reported_costs: Vec<(String, String)>,
    #[allow(dead_code)]
    pub summaries: Vec<(String, ControlFlowSummary)>,
    pub diagnostics: Vec<Diagnostic>,
}
struct Checker<'a> {
    function: String,
    diagnostics: Vec<Diagnostic>,
    models: &'a [Peripheral],
    functions: BTreeMap<String, Function>,
    summaries: BTreeMap<String, FunctionAnalysis>,
    structs: BTreeMap<String, BTreeMap<String, Type>>,
    inferred: BTreeMap<String, super::inference::Contract>,
}

#[cfg(test)]
pub fn check(item: &Item, models: &[Peripheral]) -> Result<Vec<(String, usize)>> {
    Ok(analyze(item, models)?.costs)
}

pub fn analyze(item: &Item, models: &[Peripheral]) -> Result<Report> {
    let inferred = if has_inferred_types(item, models) {
        super::inference::infer(item, models)?
    } else {
        BTreeMap::new()
    };
    let mut checker = Checker {
        function: String::new(),
        diagnostics: Vec::new(),
        models,
        functions: BTreeMap::new(),
        summaries: BTreeMap::new(),
        structs: BTreeMap::new(),
        inferred,
    };
    checker.collect(item, "")?;
    if checker.functions.is_empty() {
        return Err(Error::new(
            item.span(),
            "expected a function, inherent impl, or inline API module",
        ));
    }
    let names = checker.functions.keys().cloned().collect::<Vec<_>>();
    for name in &names {
        checker.summarize(name)?;
    }
    if !models.iter().any(Peripheral::is_symbolic) {
        for name in &names {
            let function = checker.summaries[name].clone();
            checker
                .diagnostics
                .extend(super::burn::analyze(name, &function, checker.models));
        }
    }
    let reports = names
        .iter()
        .map(|name| (name.clone(), checker.summaries[name].summary.cost()))
        .collect();
    let reported_costs = names
        .iter()
        .map(|name| {
            let analysis = &checker.summaries[name];
            let cost = if models.iter().any(Peripheral::is_symbolic) {
                symbolic_cost(&analysis.summary, &analysis.initial_devices, models).to_string()
            } else {
                analysis.summary.cost().to_string()
            };
            (name.clone(), cost)
        })
        .collect();
    let summaries = names
        .into_iter()
        .map(|name| (name.clone(), checker.summaries[&name].summary.clone()))
        .collect();
    Ok(Report {
        costs: reports,
        reported_costs,
        summaries,
        diagnostics: checker.diagnostics,
    })
}
impl Checker<'_> {
    fn summarize(&mut self, name: &str) -> Result<()> {
        if self.summaries.contains_key(name) {
            return Ok(());
        }
        let function = self
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| failure("unknown function"))?;
        let saved_function = std::mem::replace(&mut self.function, name.to_owned());
        let result = (|| {
            let mut context = Context {
                scope: name
                    .rsplit_once("::")
                    .map(|(scope, _)| scope)
                    .unwrap_or("")
                    .into(),
                inferred_inputs: self
                    .inferred
                    .get(name)
                    .map(|contract| contract.inputs.clone().into())
                    .unwrap_or_default(),
                ..Context::default()
            };
            let mut parameters = Vec::new();
            for parameter in &function.signature.inputs {
                let value = match parameter {
                    FnArg::Typed(parameter) => {
                        let value = self.initial(&parameter.ty, &mut context)?;
                        bind(&parameter.pat, value.clone(), &mut context)?;
                        value
                    }
                    FnArg::Receiver(_) => {
                        let mut fields = BTreeMap::new();
                        let types = function
                            .owner
                            .as_ref()
                            .and_then(|owner| self.structs.get(owner))
                            .cloned()
                            .unwrap_or_default();
                        for (field, ty) in types {
                            fields.insert(field, self.initial(&ty, &mut context)?);
                        }
                        let value =
                            Value::Object(function.owner.clone().unwrap_or_default(), fields);
                        context.bindings.insert("self".into(), value.clone());
                        value
                    }
                };
                parameters.push(value);
            }
            let initial_devices = context.devices.clone();
            let evaluation = self.block(&function.body, &mut context)?;
            if evaluation.summary.cost() != evaluation.cost {
                return Err(Error::new(
                    function.body.span(),
                    "internal error: control-flow summary does not match the cost bound",
                ));
            }
            self.summaries.insert(
                name.to_owned(),
                FunctionAnalysis {
                    parameters,
                    initial_devices,
                    final_devices: context.devices,
                    result: evaluation.value,
                    summary: evaluation.summary,
                },
            );
            Ok(())
        })();
        self.function = saved_function;
        result
    }

    fn collect(&mut self, item: &Item, scope: &str) -> Result<()> {
        match item {
            Item::Fn(f) => self.function(scope, &f.sig, &f.block, None)?,
            Item::Impl(implementation) => {
                if implementation.trait_.is_some() || !implementation.generics.params.is_empty() {
                    return Err(Error::new(
                        item.span(),
                        "generic and trait impls are not supported",
                    ));
                }
                let owner = qualify(scope, &type_name(&implementation.self_ty)?);
                for item in &implementation.items {
                    if let syn::ImplItem::Fn(f) = item {
                        self.function(&owner, &f.sig, &f.block, Some(owner.clone()))?;
                    }
                }
            }
            Item::Mod(module) => {
                let (_, items) = module
                    .content
                    .as_ref()
                    .ok_or_else(|| Error::new(item.span(), "the checked module must be inline"))?;
                let scope = qualify(scope, &module.ident.to_string());
                for item in items {
                    self.collect(item, &scope)?;
                }
            }
            Item::Struct(s) => {
                let mut fields = BTreeMap::new();
                for field in &s.fields {
                    let name = field
                        .ident
                        .as_ref()
                        .ok_or_else(|| Error::new(field.span(), "expected named fields"))?;
                    fields.insert(name.to_string(), field.ty.clone());
                }
                self.structs
                    .insert(qualify(scope, &s.ident.to_string()), fields);
            }
            Item::Use(_) => {}
            _ => {
                return Err(Error::new(
                    item.span(),
                    "unsupported item in checked API module",
                ))
            }
        }
        Ok(())
    }
    fn function(
        &mut self,
        scope: &str,
        signature: &Signature,
        body: &Block,
        owner: Option<String>,
    ) -> Result<()> {
        if signature.asyncness.is_some()
            || signature.unsafety.is_some()
            || !signature.generics.params.is_empty()
        {
            return Err(Error::new(
                signature.span(),
                "async, unsafe, and generic functionsare not supported yet",
            ));
        }
        let name = qualify(scope, &signature.ident.to_string());
        let function = Function {
            signature: signature.clone(),
            body: body.clone(),
            owner,
        };
        if self.functions.insert(name, function).is_some() {
            return Err(Error::new(signature.span(), "duplicate API function"));
        }
        Ok(())
    }
    fn initial(&self, ty: &Type, context: &mut Context) -> Result<Value> {
        if let Type::Reference(r) = ty {
            return self.initial(&r.elem, context);
        }
        if let Type::Tuple(tuple) = ty {
            return Ok(Value::Tuple(
                tuple
                    .elems
                    .iter()
                    .map(|t| self.initial(t, context))
                    .collect::<Result<_>>()?,
            ));
        }
        let name = type_name(ty)?;
        if matches!(
            name.as_str(),
            "bool"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
        ) {
            return Ok(Value::Scalar);
        }
        if name == "AbacusCell" {
            let Value::Register(id) = self.initial(type_argument(ty)?, context)? else {
                return Err(Error::new(
                    ty.span(),
                    "AbacusCell needs a generated state enum",
                ));
            };
            return Ok(Value::Cell(id));
        }
        for (index, model) in self.models.iter().enumerate() {
            let mut requires_inferred_input = false;
            let states = if name == model.registers {
                if let Some(argument) = type_argument_optional(ty)? {
                    let state_type = type_key(argument)?;
                    let state = model
                        .states
                        .iter()
                        .find(|(_, s)| s.ty == state_type)
                        .ok_or_else(|| Error::new(ty.span(), "unknown or generic typestate"))?;
                    BTreeSet::from([state.0.clone()])
                } else {
                    requires_inferred_input = true;
                    model.states.keys().cloned().collect()
                }
            } else if name == model.state_enum {
                model.states.keys().cloned().collect()
            } else {
                continue;
            };
            if states.iter().any(|s| model.states[s].transient) {
                return Err(Error::new(
                    ty.span(),
                    "transient states require hardware-concurrency analysis",
                ));
            }
            let states = if let Some(inferred) = context.inferred_inputs.pop_front() {
                if inferred.0 != index {
                    return Err(Error::new(ty.span(), "inferred peripheral type mismatch"));
                }
                inferred.1
            } else if requires_inferred_input {
                return Err(Error::new(ty.span(), "missing inferred peripheral input"));
            } else {
                states
            };
            let id = context.devices.len();
            context.devices.push(Device {
                model: index,
                input_states: states.clone(),
                states,
            });
            return Ok(Value::Register(id));
        }
        Err(Error::new(
            ty.span(),
            "unmodeled parameter or field type; select its cost provider",
        ))
    }
    fn background_charges(
        &self,
        context: &Context,
        transitioning: &BTreeSet<usize>,
    ) -> Vec<BackgroundCharge> {
        let mut charges = Vec::new();
        for (id, device) in context.devices.iter().enumerate() {
            if transitioning.contains(&id) {
                continue;
            }
            let states = &self.models[device.model].states;
            let cost = device
                .states
                .iter()
                .filter_map(|s| states[s].cost.concrete())
                .max()
                .unwrap_or(0);
            charges.push(BackgroundCharge {
                device: id,
                states: device.states.clone(),
                cost,
                input_states: device.input_states.clone(),
            });
        }
        charges
    }

    fn transitioning_devices(
        &self,
        expr: &Expr,
        context: &Context,
        transitioning: &mut BTreeSet<usize>,
    ) {
        match expr {
            Expr::MethodCall(call) => {
                let name = call.method.to_string();
                if let Ok(Value::Register(id)) = lookup(&call.receiver, context) {
                    let device = &context.devices[id];
                    let model = &self.models[device.model];
                    if device.states.iter().all(|state| {
                        model
                            .operations
                            .get(&(state.clone(), name.clone()))
                            .is_some_and(|operation| operation.transition)
                    }) {
                        transitioning.insert(id);
                    }
                }
                self.transitioning_devices(&call.receiver, context, transitioning);
                for argument in &call.args {
                    self.transitioning_devices(argument, context, transitioning);
                }
            }
            Expr::Call(call) => {
                for argument in &call.args {
                    self.transitioning_devices(argument, context, transitioning);
                }
            }
            Expr::Paren(expr) => self.transitioning_devices(&expr.expr, context, transitioning),
            Expr::Reference(expr) => self.transitioning_devices(&expr.expr, context, transitioning),
            Expr::Return(expr) => {
                if let Some(expr) = &expr.expr {
                    self.transitioning_devices(expr, context, transitioning);
                }
            }
            Expr::Tuple(tuple) => {
                for expr in &tuple.elems {
                    self.transitioning_devices(expr, context, transitioning);
                }
            }
            _ => {}
        }
    }

    fn block(&mut self, block: &Block, context: &mut Context) -> Result<Evaluation> {
        let original = context.bindings.clone();
        let mut result = Evaluation::new(Value::Scalar, 0);
        for statement in &block.stmts {
            let expression = match statement {
                Stmt::Local(local) => local.init.as_ref().map(|init| init.expr.as_ref()),
                Stmt::Expr(expression, _) => Some(expression),
                _ => None,
            };
            let has_outer_command = !matches!(statement, Stmt::Expr(Expr::Block(_), _));
            let background = if !has_outer_command {
                Vec::new()
            } else {
                let mut transitioning = BTreeSet::new();
                if let Some(expression) = expression {
                    self.transitioning_devices(expression, context, &mut transitioning);
                }
                self.background_charges(context, &transitioning)
            };
            let next = match statement {
                Stmt::Local(local) => {
                    let init = local
                        .init
                        .as_ref()
                        .ok_or_else(|| Error::new(local.span(), "expected an initialized local"))?;
                    if init.diverge.is_some() {
                        return Err(Error::new(local.span(), "let-else is not supported"));
                    }
                    let value = self.expression(&init.expr, context)?;
                    if value.returned {
                        return Err(Error::new(
                            local.span(),
                            "return inside an initializer is not supported",
                        ));
                    }
                    bind(&local.pat, value.value, context)?;
                    let mut local = Evaluation::new(Value::Scalar, value.cost);
                    local.summary = value.summary;
                    local.events = value.events;
                    local.call_body_charged = value.call_body_charged;
                    local
                }
                Stmt::Expr(expr, semicolon) => {
                    let mut value = self.expression(expr, context)?;
                    if semicolon.is_some() && !value.returned {
                        value.value = Value::Scalar;
                    }
                    value
                }
                _ => {
                    return Err(Error::new(
                        statement.span(),
                        "unsupported statement in cost-checked API",
                    ))
                }
            };
            let nested_cost = next.summary.cost();
            let foreground = next.cost - nested_cost;
            let mut statement_parts = Vec::new();
            if has_outer_command && !next.call_body_charged {
                result.cost += background.iter().map(|charge| charge.cost).sum::<usize>();
                statement_parts.push(ControlFlowSummary::Command(CommandSummary {
                    foreground,
                    background,
                    events: next.events,
                    span: statement.span(),
                }));
            } else if foreground != 0 || !next.events.is_empty() {
                statement_parts.push(ControlFlowSummary::Command(CommandSummary {
                    foreground,
                    background: Vec::new(),
                    events: next.events,
                    span: statement.span(),
                }));
            }
            statement_parts.push(next.summary);
            result.summary = sequence([result.summary, sequence(statement_parts)]);
            result.cost += next.cost;
            result.value = next.value;
            result.returned = next.returned;
            if result.returned {
                break;
            }
        }
        context.bindings = original;
        Ok(result)
    }
    // A bare match-arm or closure body is a command too; adding braces must not
    // change its background cost.
    fn body(&mut self, expr: &Expr, context: &mut Context) -> Result<Evaluation> {
        if let Expr::Block(block) = expr {
            return self.block(&block.block, context);
        }
        let mut transitioning = BTreeSet::new();
        self.transitioning_devices(expr, context, &mut transitioning);
        let background = self.background_charges(context, &transitioning);
        let mut result = self.expression(expr, context)?;
        let foreground = result.cost - result.summary.cost();
        let mut parts = Vec::new();
        if !result.call_body_charged {
            result.cost += background.iter().map(|charge| charge.cost).sum::<usize>();
            parts.push(ControlFlowSummary::Command(CommandSummary {
                foreground,
                background,
                events: std::mem::take(&mut result.events),
                span: expr.span(),
            }));
        } else if foreground != 0 || !result.events.is_empty() {
            parts.push(ControlFlowSummary::Command(CommandSummary {
                foreground,
                background: Vec::new(),
                events: std::mem::take(&mut result.events),
                span: expr.span(),
            }));
        }
        parts.push(std::mem::replace(
            &mut result.summary,
            ControlFlowSummary::Empty,
        ));
        result.summary = sequence(parts);
        Ok(result)
    }
    fn expression(&mut self, expr: &Expr, context: &mut Context) -> Result<Evaluation> {
        match expr {
            Expr::Path(_) | Expr::Field(_) => Ok(Evaluation::new(lookup(expr, context)?, 0)),
            Expr::Lit(_) => Ok(Evaluation::new(Value::Scalar, 0)),
            Expr::Reference(r) => self.expression(&r.expr, context),
            Expr::Paren(p) => self.expression(&p.expr, context),
            Expr::Block(b) => self.block(&b.block, context),
            Expr::Return(r) => {
                let mut result = if let Some(expr) = &r.expr {
                    self.expression(expr, context)?
                } else {
                    Evaluation::new(Value::Scalar, 0)
                };
                result.returned = true;
                Ok(result)
            }
            Expr::Tuple(tuple) => {
                let mut values = Vec::new();
                let mut cost = 0;
                let mut summaries = Vec::new();
                let mut events = Vec::new();
                for expr in &tuple.elems {
                    let value = self.expression(expr, context)?;
                    cost += value.cost;
                    summaries.push(value.summary);
                    events.extend(value.events);
                    values.push(value.value);
                }
                let mut result = Evaluation::new(
                    if values.is_empty() {
                        Value::Scalar
                    } else {
                        Value::Tuple(values)
                    },
                    cost,
                );
                result.summary = sequence(summaries);
                result.events = events;
                Ok(result)
            }
            Expr::Unary(unary) if !matches!(unary.op, syn::UnOp::Deref(_)) => {
                let result = self.expression(&unary.expr, context)?;
                if result.value != Value::Scalar {
                    return Err(Error::new(
                        expr.span(),
                        "overloaded operators need a cost summary",
                    ));
                }
                Ok(result)
            }
            Expr::Binary(binary) => {
                if matches!(
                    binary.op,
                    syn::BinOp::AddAssign(_)
                        | syn::BinOp::SubAssign(_)
                        | syn::BinOp::MulAssign(_)
                        | syn::BinOp::DivAssign(_)
                        | syn::BinOp::RemAssign(_)
                        | syn::BinOp::BitXorAssign(_)
                        | syn::BinOp::BitAndAssign(_)
                        | syn::BinOp::BitOrAssign(_)
                        | syn::BinOp::ShlAssign(_)
                        | syn::BinOp::ShrAssign(_)
                ) {
                    return Err(Error::new(expr.span(), "assignment is not supported"));
                }
                let left = self.expression(&binary.left, context)?;
                let before = context.clone();
                let right = self.expression(&binary.right, context)?;
                if left.value != Value::Scalar || right.value != Value::Scalar {
                    return Err(Error::new(
                        expr.span(),
                        "overloaded operators need a cost summary",
                    ));
                }
                if matches!(binary.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) {
                    *context = join_context(&before, context)?;
                }
                let mut result = Evaluation::new(Value::Scalar, left.cost + right.cost);
                result.summary = sequence([left.summary, right.summary]);
                result.events = left.events;
                result.events.extend(right.events);
                Ok(result)
            }
            Expr::If(branch) => {
                let condition = self.expression(&branch.cond, context)?;
                let mut left_context = context.clone();
                let mut right_context = context.clone();
                let left = self.block(&branch.then_branch, &mut left_context)?;
                let right = if let Some((_, other)) = &branch.else_branch {
                    self.body(other, &mut right_context)?
                } else {
                    Evaluation::new(Value::Scalar, 0)
                };
                let mut result = join_result(left, right)?;
                result.summary = sequence([condition.summary, result.summary]);
                result.events = condition.events;
                *context = join_context(&left_context, &right_context)?;
                result.cost += condition.cost;
                Ok(result)
            }
            Expr::Match(branch) => self.match_states(branch, context),
            Expr::MethodCall(call) => self.method(call, context),
            Expr::Call(call) => {
                let Expr::Path(path) = call.func.as_ref() else {
                    return Err(Error::new(
                        expr.span(),
                        "indirect calls have no cost summary",
                    ));
                };
                let name = path
                    .path
                    .segments
                    .last()
                    .ok_or_else(|| Error::new_spanned(&path.path, "expected a nonempty path"))?
                    .ident
                    .to_string();
                let mut values = Vec::new();
                let mut cost = 0;
                let mut summaries = Vec::new();
                let mut events = Vec::new();
                for argument in &call.args {
                    let value = self.expression(argument, context)?;
                    cost += value.cost;
                    summaries.push(value.summary);
                    events.extend(value.events);
                    values.push(value.value);
                }
                if matches!(name.as_str(), "Ok" | "Err" | "Some") {
                    let mut result = Evaluation::new(Value::Scalar, cost);
                    result.summary = sequence(summaries);
                    result.events = events;
                    return Ok(result);
                }
                let key = path
                    .path
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect::<Vec<_>>()
                    .join("::");
                let mut result = self.call(&key, values, context, call.span())?;
                result.cost += cost;
                result.summary = sequence([sequence(summaries), result.summary]);
                result.events.extend(events);
                result.call_body_charged = true;
                Ok(result)
            }
            _ => Err(Error::new(
                expr.span(),
                "unsupported expression; no cost bound can be reported",
            )),
        }
    }
    fn method(&mut self, call: &syn::ExprMethodCall, context: &mut Context) -> Result<Evaluation> {
        if call.turbofish.is_some() {
            return Err(Error::new(
                call.span(),
                "generic method calls are not supported",
            ));
        }
        let name = call.method.to_string();
        if name == "into" && call.args.is_empty() {
            let value = self.expression(&call.receiver, context)?;
            if matches!(value.value, Value::Register(_)) {
                return Ok(value);
            }
            return Err(Error::new(
                call.span(),
                "only generated register-to-enum conversions are modeled",
            ));
        }
        if name == "map" {
            if let Ok(Value::Cell(id)) = lookup(&call.receiver, context) {
                return self.cell_map(id, call, context);
            }
        }
        let (receiver, operation) = match call.receiver.as_ref() {
            Expr::Field(field)
                if !matches!(lookup(&call.receiver, context), Ok(Value::Register(_))) =>
            {
                let syn::Member::Named(field_name) = &field.member else {
                    return Err(Error::new(field.span(), "expected register field"));
                };
                (field.base.as_ref(), format!("{field_name}.{name}"))
            }
            _ => (call.receiver.as_ref(), name.clone()),
        };
        if let Ok(Value::Register(id)) = lookup(receiver, context) {
            let device = &context.devices[id];
            let model = &self.models[device.model];
            let mut targets = BTreeSet::new();
            let mut cost = 0;
            let mut transition = false;
            for source in &device.states {
                let op = model
                    .operations
                    .get(&(source.clone(), operation.clone()))
                    .ok_or_else(|| {
                        Error::new(
                            call.span(),
                            format!("`{operation}` is unavailable from state `{source}`"),
                        )
                    })?;
                targets.insert(op.target.clone());
                cost = cost.max(op.cost.concrete().unwrap_or(0));
                transition = op.transition;
            }
            if targets.iter().any(|s| model.states[s].transient) {
                return Err(Error::new(
                    call.span(),
                    "transient states require hardware-concurrency analysis",
                ));
            }
            if !transition && !model.is_symbolic() {
                let current_states = device
                    .states
                    .iter()
                    .map(|state| (state.clone(), model.states[state].cost.concrete().unwrap()))
                    .collect::<Vec<_>>();
                if let Some(diagnostic) = diagnostics::operation(
                    &self.function,
                    call.span(),
                    &operation,
                    &model.registers,
                    &current_states,
                ) {
                    self.diagnostics.push(diagnostic);
                }
            }
            let source = device.states.clone();
            let source_inputs = device.input_states.clone();
            let operation_cost = cost;
            let mut summaries = Vec::new();
            let mut argument_events = Vec::new();
            for arg in &call.args {
                let argument = self.expression(arg, context)?;
                cost += argument.cost;
                summaries.push(argument.summary);
                argument_events.extend(argument.events);
            }
            context.devices[id].states = targets;
            if transition {
                context.devices[id].input_states.clear();
            }
            let target = context.devices[id].states.clone();
            let mut result = Evaluation::new(
                if transition {
                    Value::Register(id)
                } else {
                    Value::Scalar
                },
                cost,
            );
            result.summary = sequence(summaries);
            result.events = argument_events;
            result.events.push(PeripheralEvent {
                device: id,
                operation,
                source,
                target,
                transition,
                cost: operation_cost,
                source_inputs,
                span: call.span(),
            });
            return Ok(result);
        }
        if let Ok(Value::Object(owner, fields)) = lookup(&call.receiver, context) {
            let value = Value::Object(owner.clone(), fields);
            let mut values = vec![value];
            let mut cost = 0;
            let mut summaries = Vec::new();
            let mut events = Vec::new();
            for arg in &call.args {
                let value = self.expression(arg, context)?;
                values.push(value.value);
                cost += value.cost;
                summaries.push(value.summary);
                events.extend(value.events);
            }
            let mut result = self.call(&qualify(&owner, &name), values, context, call.span())?;
            result.cost += cost;
            result.summary = sequence([sequence(summaries), result.summary]);
            result.events.extend(events);
            result.call_body_charged = true;
            return Ok(result);
        }
        Err(Error::new(
            call.span(),
            "unknown method; bind intermediate registers or include the helper in the checked API",
        ))
    }
    fn cell_map(
        &mut self,
        id: usize,
        call: &syn::ExprMethodCall,
        context: &mut Context,
    ) -> Result<Evaluation> {
        if call.args.len() != 1 {
            return Err(Error::new(call.span(), "map expects one closure"));
        }
        let Expr::Closure(closure) = &call.args[0] else {
            return Err(Error::new(call.span(), "map needs an inline closure"));
        };
        if closure.inputs.len() != 1 {
            return Err(Error::new(
                closure.span(),
                "map needs one peripheral parameter",
            ));
        }
        let before = context.clone();
        bind(&closure.inputs[0], Value::Register(id), context)?;
        let result = self.body(&closure.body, context)?;
        let Value::Tuple(values) = result.value else {
            return Err(Error::new(
                closure.span(),
                "map must return (registers, result)",
            ));
        };
        if values.len() != 2 || values[0] != Value::Register(id) {
            return Err(Error::new(
                closure.span(),
                "map must return the captured peripheral",
            ));
        }
        context.bindings = before.bindings.clone();
        // None means the callback did not run. Keep both outcomes for later work.
        *context = join_context(&before, context)?;
        let mut mapped = Evaluation::new(Value::Scalar, result.cost);
        mapped.summary = result.summary;
        mapped.events = result.events;
        Ok(mapped)
    }
    fn match_states(
        &mut self,
        branch: &syn::ExprMatch,
        context: &mut Context,
    ) -> Result<Evaluation> {
        let subject = self.expression(&branch.expr, context)?;
        let Value::Register(id) = subject.value else {
            return Err(Error::new(
                branch.span(),
                "match needs a generated state enum",
            ));
        };
        let device = &context.devices[id];
        let model = &self.models[device.model];
        let mut remaining = device.states.clone();
        let mut joined: Option<(Context, Evaluation)> = None;
        for arm in &branch.arms {
            if arm.guard.is_some() {
                return Err(Error::new(arm.span(), "match guards are not supported"));
            }
            let (accepted, binding) = match &arm.pat {
                Pat::TupleStruct(pattern) if pattern.elems.len() == 1 => {
                    let mut segments = pattern.path.segments.iter().rev();
                    let state = segments
                        .next()
                        .ok_or_else(|| {
                            Error::new_spanned(&pattern.path, "expected a state variant")
                        })?
                        .ident
                        .to_string();
                    if segments.next().map(|s| s.ident.to_string())
                        != Some(model.state_enum.clone())
                        || !model.states.contains_key(&state)
                    {
                        return Err(Error::new(
                            pattern.span(),
                            "expected a generated state enum variant",
                        ));
                    }
                    (BTreeSet::from([state]), Some(&pattern.elems[0]))
                }
                Pat::Wild(_) => (remaining.clone(), None),
                Pat::Ident(_) => (remaining.clone(), Some(&arm.pat)),
                _ => return Err(Error::new(arm.pat.span(), "unsupported state pattern")),
            };
            let reachable: States = remaining.intersection(&accepted).cloned().collect();
            if reachable.is_empty() {
                continue;
            }
            remaining = remaining.difference(&reachable).cloned().collect();
            let mut arm_context = context.clone();
            arm_context.devices[id].states = reachable;
            let reachable = arm_context.devices[id].states.clone();
            arm_context.devices[id]
                .input_states
                .retain(|state| reachable.contains(state));
            if let Some(pattern) = binding {
                bind(pattern, Value::Register(id), &mut arm_context)?;
            }
            let result = self.body(&arm.body, &mut arm_context)?;
            arm_context.bindings = context.bindings.clone();
            joined = Some(if let Some((previous, value)) = joined {
                (
                    join_context(&previous, &arm_context)?,
                    join_result(value, result)?,
                )
            } else {
                (arm_context, result)
            });
        }
        if !remaining.is_empty() {
            return Err(Error::new(branch.span(), "non-exhaustive state match"));
        }
        let (updated, mut result) =
            joined.ok_or_else(|| Error::new(branch.span(), "empty state match"))?;
        *context = updated;
        result.cost += subject.cost;
        result.summary = sequence([subject.summary, result.summary]);
        result.events = subject.events;
        Ok(result)
    }
    fn call(
        &mut self,
        requested: &str,
        values: Vec<Value>,
        context: &mut Context,
        span: proc_macro2::Span,
    ) -> Result<Evaluation> {
        let scope = context.scope.as_str();
        let name = if let Some(method) = requested.strip_prefix("Self::") {
            qualify(scope, method)
        } else {
            let mut scope = scope;
            loop {
                let candidate = qualify(scope, requested.trim_start_matches("crate::"));
                if self.functions.contains_key(&candidate) || scope.is_empty() {
                    break candidate;
                }
                scope = scope
                    .rsplit_once("::")
                    .map(|(parent, _)| parent)
                    .unwrap_or("");
            }
        };
        let function = self.functions.get(&name).cloned().ok_or_else(|| {
            failure("unknown callee; include its body in the same checked module or impl")
        })?;
        if function.signature.inputs.len() != values.len() {
            return Err(failure("helper argument count mismatch"));
        }
        self.summarize(&name)?;
        let summary = self.summaries[&name].clone();
        self.instantiate(&summary, values, context, span)
    }

    fn instantiate(
        &self,
        function: &FunctionAnalysis,
        arguments: Vec<Value>,
        context: &mut Context,
        span: proc_macro2::Span,
    ) -> Result<Evaluation> {
        let mut devices = vec![None; function.initial_devices.len()];
        for (parameter, argument) in function.parameters.iter().zip(&arguments) {
            map_argument(parameter, argument, &mut devices, span)?;
        }
        let mapped = devices
            .into_iter()
            .map(|device| {
                device.ok_or_else(|| {
                    Error::new(span, "function summary contains an unmapped peripheral")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let unique = mapped.iter().copied().collect::<BTreeSet<_>>();
        if unique.len() != mapped.len() {
            return Err(Error::new(
                span,
                "passing the same peripheral as multiple arguments is not supported",
            ));
        }
        for (formal, actual) in function.initial_devices.iter().zip(&mapped) {
            let actual = context
                .devices
                .get(*actual)
                .ok_or_else(|| Error::new(span, "unknown peripheral argument"))?;
            if formal.model != actual.model || !actual.states.is_subset(&formal.states) {
                return Err(Error::new(
                    span,
                    "peripheral argument does not satisfy the function input states",
                ));
            }
        }

        let call_inputs = context.devices.clone();
        let summary =
            self.instantiate_control_flow(&function.summary, &mapped, context, &unique)?;
        for (formal, actual) in function.final_devices.iter().zip(&mapped) {
            let input = &call_inputs[*actual];
            let mut states: States = formal
                .states
                .difference(&formal.input_states)
                .cloned()
                .collect();
            states.extend(formal.input_states.intersection(&input.states).cloned());
            context.devices[*actual].states = states;
            context.devices[*actual].input_states = formal
                .input_states
                .intersection(&input.input_states)
                .cloned()
                .collect();
        }
        let value = map_result(&function.result, &mapped, span)?;
        let cost = summary.cost();
        let mut result = Evaluation::new(value, cost);
        result.summary = summary;
        Ok(result)
    }

    fn instantiate_control_flow(
        &self,
        summary: &ControlFlowSummary,
        devices: &[usize],
        context: &Context,
        mapped: &BTreeSet<usize>,
    ) -> Result<ControlFlowSummary> {
        Ok(match summary {
            ControlFlowSummary::Empty => ControlFlowSummary::Empty,
            ControlFlowSummary::Command(command) => {
                let mut background = command
                    .background
                    .iter()
                    .map(|charge| {
                        let device = *devices.get(charge.device).ok_or_else(|| {
                            Error::new(command.span, "invalid summarized peripheral")
                        })?;
                        let actual = &context.devices[device];
                        let mut states: States = charge
                            .states
                            .difference(&charge.input_states)
                            .cloned()
                            .collect();
                        states.extend(charge.input_states.intersection(&actual.states).cloned());
                        let model = &self.models[actual.model];
                        let cost = states
                            .iter()
                            .filter_map(|state| model.states[state].cost.concrete())
                            .max()
                            .unwrap_or(0);
                        Ok(BackgroundCharge {
                            device,
                            states,
                            cost,
                            input_states: charge
                                .input_states
                                .intersection(&actual.input_states)
                                .cloned()
                                .collect(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                for (device, state) in context.devices.iter().enumerate() {
                    if mapped.contains(&device) {
                        continue;
                    }
                    let states = &self.models[state.model].states;
                    background.push(BackgroundCharge {
                        device,
                        states: state.states.clone(),
                        cost: state
                            .states
                            .iter()
                            .filter_map(|name| states[name].cost.concrete())
                            .max()
                            .unwrap_or(0),
                        input_states: state.input_states.clone(),
                    });
                }
                let events: Vec<PeripheralEvent> = command
                    .events
                    .iter()
                    .map(|event| {
                        let device = *devices.get(event.device).ok_or_else(|| {
                            Error::new(command.span, "invalid summarized peripheral event")
                        })?;
                        let actual = &context.devices[device];
                        let mut source: States = event
                            .source
                            .difference(&event.source_inputs)
                            .cloned()
                            .collect();
                        source.extend(event.source_inputs.intersection(&actual.states).cloned());
                        let model = &self.models[actual.model];
                        let cost = source
                            .iter()
                            .filter_map(|state| {
                                model.operations[&(state.clone(), event.operation.clone())]
                                    .cost
                                    .concrete()
                            })
                            .max()
                            .unwrap_or(0);
                        Ok(PeripheralEvent {
                            device,
                            operation: event.operation.clone(),
                            source,
                            target: event.target.clone(),
                            transition: event.transition,
                            cost,
                            source_inputs: event
                                .source_inputs
                                .intersection(&actual.input_states)
                                .cloned()
                                .collect(),
                            span: event.span,
                        })
                    })
                    .collect::<Result<_>>()?;
                ControlFlowSummary::Command(CommandSummary {
                    foreground: events.iter().map(|event| event.cost).sum(),
                    background,
                    events,
                    span: command.span,
                })
            }
            ControlFlowSummary::Sequence(parts) => ControlFlowSummary::Sequence(
                parts
                    .iter()
                    .map(|part| self.instantiate_control_flow(part, devices, context, mapped))
                    .collect::<Result<_>>()?,
            ),
            ControlFlowSummary::Branch(arms) => ControlFlowSummary::Branch(
                arms.iter()
                    .map(|arm| self.instantiate_control_flow(arm, devices, context, mapped))
                    .collect::<Result<_>>()?,
            ),
        })
    }
}

fn map_argument(
    formal: &Value,
    actual: &Value,
    devices: &mut [Option<usize>],
    span: proc_macro2::Span,
) -> Result<()> {
    match (formal, actual) {
        (Value::Scalar, Value::Scalar) => Ok(()),
        (Value::Register(formal), Value::Register(actual))
        | (Value::Cell(formal), Value::Cell(actual)) => {
            let mapped = devices
                .get_mut(*formal)
                .ok_or_else(|| Error::new(span, "invalid formal peripheral"))?;
            if mapped.is_some_and(|mapped| mapped != *actual) {
                return Err(Error::new(span, "inconsistent peripheral argument"));
            }
            *mapped = Some(*actual);
            Ok(())
        }
        (Value::Tuple(formal), Value::Tuple(actual)) if formal.len() == actual.len() => {
            for (formal, actual) in formal.iter().zip(actual) {
                map_argument(formal, actual, devices, span)?;
            }
            Ok(())
        }
        (Value::Object(formal_owner, formal), Value::Object(actual_owner, actual))
            if formal_owner == actual_owner =>
        {
            for (name, formal) in formal {
                let actual = actual
                    .get(name)
                    .ok_or_else(|| Error::new(span, "receiver field is unavailable"))?;
                map_argument(formal, actual, devices, span)?;
            }
            Ok(())
        }
        _ => Err(Error::new(
            span,
            "function argument does not match its parameter",
        )),
    }
}

fn map_result(value: &Value, devices: &[usize], span: proc_macro2::Span) -> Result<Value> {
    Ok(match value {
        Value::Scalar => Value::Scalar,
        Value::Register(device) => Value::Register(
            *devices
                .get(*device)
                .ok_or_else(|| Error::new(span, "invalid summarized return peripheral"))?,
        ),
        Value::Cell(device) => Value::Cell(
            *devices
                .get(*device)
                .ok_or_else(|| Error::new(span, "invalid summarized return cell"))?,
        ),
        Value::Tuple(values) => Value::Tuple(
            values
                .iter()
                .map(|value| map_result(value, devices, span))
                .collect::<Result<_>>()?,
        ),
        Value::Object(owner, fields) => Value::Object(
            owner.clone(),
            fields
                .iter()
                .map(|(name, value)| Ok((name.clone(), map_result(value, devices, span)?)))
                .collect::<Result<_>>()?,
        ),
    })
}
fn lookup(expr: &Expr, context: &Context) -> Result<Value> {
    match expr {
        Expr::Path(p) if p.path.segments.len() > 1 => Ok(Value::Scalar), // pure bitfield constants
        Expr::Path(p) => {
            let name = p.path.segments[0].ident.to_string();
            if name == "None" {
                return Ok(Value::Scalar);
            }
            context
                .bindings
                .get(&name)
                .cloned()
                .ok_or_else(|| Error::new(expr.span(), "unknown variable"))
        }
        Expr::Field(field) => {
            if let (Value::Object(_, fields), syn::Member::Named(name)) =
                (lookup(&field.base, context)?, &field.member)
            {
                return fields.get(&name.to_string()).cloned().ok_or_else(|| Error::new(expr.span(), "field type unavailable; annotate the module containing the struct and impl"));
            }
            Err(Error::new(expr.span(), "unknown field"))
        }
        Expr::Paren(p) => lookup(&p.expr, context),
        _ => Err(Error::new(expr.span(), "expected a named receiver")),
    }
}
fn bind(pattern: &Pat, value: Value, context: &mut Context) -> Result<()> {
    match pattern {
        Pat::Ident(p) if p.by_ref.is_none() && p.subpat.is_none() => {
            context.bindings.insert(p.ident.to_string(), value);
            Ok(())
        }
        Pat::Wild(_) => Ok(()),
        Pat::Type(p) => bind(&p.pat, value, context),
        Pat::Tuple(p) => {
            if let Value::Tuple(values) = value {
                if p.elems.len() == values.len() {
                    for (p, v) in p.elems.iter().zip(values) {
                        bind(p, v, context)?;
                    }
                    return Ok(());
                }
            }
            Err(Error::new(pattern.span(), "tuple binding mismatch"))
        }
        _ => Err(Error::new(pattern.span(), "unsupported binding")),
    }
}
fn join_context(left: &Context, right: &Context) -> Result<Context> {
    if left.devices.len() != right.devices.len() {
        return Err(failure("branches change peripheral identities"));
    }
    let mut joined = left.clone();
    for (device, other) in joined.devices.iter_mut().zip(&right.devices) {
        device.states.extend(other.states.iter().cloned());
        device
            .input_states
            .extend(other.input_states.iter().cloned());
    }
    joined
        .bindings
        .retain(|name, value| right.bindings.get(name) == Some(value));
    Ok(joined)
}
fn join_result(left: Evaluation, right: Evaluation) -> Result<Evaluation> {
    if left.returned != right.returned {
        return Err(failure(
            "mixed early-return and fallthrough branches are not supported",
        ));
    }
    if left.value != right.value {
        return Err(failure("branches return different peripheral identities"));
    }
    let summary = ControlFlowSummary::Branch(vec![left.summary, right.summary]);
    Ok(Evaluation {
        value: left.value,
        cost: left.cost.max(right.cost),
        summary,
        events: Vec::new(),
        returned: left.returned,
        call_body_charged: false,
    })
}
fn type_name(ty: &Type) -> Result<String> {
    if let Type::Path(path) = ty {
        return Ok(path
            .path
            .segments
            .last()
            .ok_or_else(|| Error::new_spanned(&path.path, "expected a nonempty path"))?
            .ident
            .to_string());
    }
    Err(Error::new(ty.span(), "unsupported type"))
}

fn has_inferred_types(item: &Item, models: &[Peripheral]) -> bool {
    fn inferred_type(ty: &Type, models: &[Peripheral]) -> bool {
        match ty {
            Type::Reference(reference) => inferred_type(&reference.elem, models),
            Type::Tuple(tuple) => tuple.elems.iter().any(|ty| inferred_type(ty, models)),
            Type::Path(path) => path.path.segments.last().is_some_and(|segment| {
                models.iter().any(|model| {
                    segment.ident == model.registers
                        && matches!(segment.arguments, PathArguments::None)
                })
            }),
            _ => false,
        }
    }
    fn signature(signature: &Signature, models: &[Peripheral]) -> bool {
        signature.inputs.iter().any(|argument| match argument {
            FnArg::Typed(argument) => inferred_type(&argument.ty, models),
            FnArg::Receiver(_) => false,
        }) || match &signature.output {
            syn::ReturnType::Default => false,
            syn::ReturnType::Type(_, ty) => inferred_type(ty, models),
        }
    }
    match item {
        Item::Fn(function) => signature(&function.sig, models),
        Item::Impl(implementation) => implementation.items.iter().any(
            |item| matches!(item, syn::ImplItem::Fn(function) if signature(&function.sig, models)),
        ),
        Item::Mod(module) => module
            .content
            .as_ref()
            .is_some_and(|(_, items)| items.iter().any(|item| has_inferred_types(item, models))),
        Item::Struct(item) => item
            .fields
            .iter()
            .any(|field| inferred_type(&field.ty, models)),
        _ => false,
    }
}
fn type_argument(ty: &Type) -> Result<&Type> {
    if let Type::Path(path) = ty {
        if let PathArguments::AngleBracketed(args) = &path
            .path
            .segments
            .last()
            .ok_or_else(|| Error::new_spanned(&path.path, "expected a nonempty path"))?
            .arguments
        {
            if let Some(GenericArgument::Type(ty)) = args.args.first() {
                return Ok(ty);
            }
        }
    }
    Err(Error::new(ty.span(), "expected a concrete type argument"))
}

fn type_argument_optional(ty: &Type) -> Result<Option<&Type>> {
    if let Type::Path(path) = ty {
        let segment = path
            .path
            .segments
            .last()
            .ok_or_else(|| Error::new_spanned(&path.path, "expected a nonempty path"))?;
        return match &segment.arguments {
            PathArguments::None => Ok(None),
            PathArguments::AngleBracketed(arguments) => match arguments.args.first() {
                Some(GenericArgument::Type(ty)) => Ok(Some(ty)),
                _ => Err(Error::new(arguments.span(), "expected a type argument")),
            },
            _ => Err(Error::new(segment.span(), "unsupported type arguments")),
        };
    }
    Err(Error::new(ty.span(), "unsupported type"))
}
fn type_key(ty: &Type) -> Result<String> {
    if let Type::Path(path) = ty {
        return Ok(path
            .path
            .segments
            .last()
            .ok_or_else(|| Error::new_spanned(&path.path, "expected a concrete state path"))?
            .to_token_stream()
            .to_string()
            .replace(' ', ""));
    }
    Err(Error::new(ty.span(), "expected concrete state type"))
}
fn qualify(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.into()
    } else {
        return format!("{scope}::{name}");
    }
}
fn failure(message: &str) -> Error {
    Error::new(proc_macro2::Span::call_site(), message)
}
