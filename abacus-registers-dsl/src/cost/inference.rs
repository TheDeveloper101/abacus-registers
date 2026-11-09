//! State inference for the sequential Heliod fragment.
//!
//! This is the finite-state implementation of the paper's inference judgments:
//! inputs start at top, operations add subset constraints, transitions replace the
//! current state, branch requirements are conjoined and branch results are joined,
//! and calls apply the inferred input/output sets of the callee.

use super::{model::Peripheral, operation};
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use syn::{
    spanned::Spanned, Block, Error, Expr, FnArg, GenericArgument, Item, Pat, PathArguments, Result,
    Signature, Stmt, Type,
};

type States = BTreeSet<String>;

#[derive(Clone, PartialEq, Eq)]
enum Value {
    Scalar,
    Register(usize),
    Task(usize),
    Guard(usize),
    Tuple(Vec<Value>),
    Object(String, BTreeMap<String, Value>),
}

#[derive(Clone)]
enum StateExpression {
    Input(usize),
    Known(States),
    Union(Vec<StateExpression>),
}

#[derive(Clone)]
struct Device {
    model: usize,
    state: StateExpression,
}

#[derive(Clone, Default)]
struct Context {
    scope: String,
    devices: Vec<Device>,
    inputs: Vec<States>,
    bindings: BTreeMap<String, Value>,
    captured: BTreeSet<usize>,
    tasks: BTreeMap<usize, InferenceTask>,
    next_task: usize,
    guards: BTreeMap<usize, InferenceGuard>,
    next_guard: usize,
}

#[derive(Clone)]
struct InferenceGuard {
    devices: BTreeMap<usize, States>,
}

#[derive(Clone)]
struct InferenceTask {
    device: usize,
    transient: States,
    normal: States,
}

#[derive(Clone)]
struct Function {
    signature: Signature,
    body: Block,
    owner: Option<String>,
}

#[derive(Clone)]
pub(super) struct Contract {
    parameters: Vec<Value>,
    pub(super) inputs: Vec<(usize, States)>,
    outputs: Vec<(usize, States)>,
    result: Value,
}

pub(super) fn infer(
    item: &Item,
    models: &[Peripheral],
    operations: &BTreeMap<String, operation::Contract>,
) -> Result<BTreeMap<String, Contract>> {
    let mut checker = Checker {
        models,
        functions: BTreeMap::new(),
        contracts: BTreeMap::new(),
        structs: BTreeMap::new(),
        active: BTreeSet::new(),
        operations,
    };
    checker.collect(item, "")?;
    let names = checker.functions.keys().cloned().collect::<Vec<_>>();
    for name in names {
        checker.summarize(&name)?;
    }
    Ok(checker.contracts)
}

struct Checker<'a> {
    models: &'a [Peripheral],
    functions: BTreeMap<String, Function>,
    contracts: BTreeMap<String, Contract>,
    structs: BTreeMap<String, BTreeMap<String, Type>>,
    active: BTreeSet<String>,
    operations: &'a BTreeMap<String, operation::Contract>,
}

impl Checker<'_> {
    fn collect(&mut self, item: &Item, scope: &str) -> Result<()> {
        match item {
            Item::Fn(function) => self.add_function(scope, &function.sig, &function.block, None)?,
            Item::Impl(implementation) => {
                let owner = qualify(scope, &type_name(&implementation.self_ty)?);
                for item in &implementation.items {
                    if let syn::ImplItem::Fn(function) = item {
                        self.add_function(
                            &owner,
                            &function.sig,
                            &function.block,
                            Some(owner.clone()),
                        )?;
                    }
                }
            }
            Item::Mod(module) => {
                let Some((_, items)) = &module.content else {
                    return Err(Error::new(item.span(), "the checked module must be inline"));
                };
                let scope = qualify(scope, &module.ident.to_string());
                for item in items {
                    self.collect(item, &scope)?;
                }
            }
            Item::Struct(item) => {
                let mut fields = BTreeMap::new();
                for field in &item.fields {
                    let name = field
                        .ident
                        .as_ref()
                        .ok_or_else(|| Error::new(field.span(), "expected named fields"))?;
                    fields.insert(name.to_string(), field.ty.clone());
                }
                self.structs
                    .insert(qualify(scope, &item.ident.to_string()), fields);
            }
            Item::Use(_) => {}
            _ => {}
        }
        Ok(())
    }

    fn add_function(
        &mut self,
        scope: &str,
        signature: &Signature,
        body: &Block,
        owner: Option<String>,
    ) -> Result<()> {
        let name = qualify(scope, &signature.ident.to_string());
        self.functions.insert(
            name,
            Function {
                signature: signature.clone(),
                body: body.clone(),
                owner,
            },
        );
        Ok(())
    }

    fn summarize(&mut self, name: &str) -> Result<()> {
        if self.contracts.contains_key(name) {
            return Ok(());
        }
        if !self.active.insert(name.to_owned()) {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "recursive functions are not supported by state inference",
            ));
        }
        let function = self
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| failure("unknown function"))?;
        if let Some(operation) = self.operations.get(name) {
            let parameters = function
                .signature
                .inputs
                .iter()
                .map(|argument| match argument {
                    FnArg::Typed(argument)
                        if type_name(&argument.ty)? == self.models[operation.model].registers =>
                    {
                        Ok(Value::Register(0))
                    }
                    FnArg::Typed(_) => Ok(Value::Scalar),
                    FnArg::Receiver(receiver) => Err(Error::new(
                        receiver.span(),
                        "heliod_operation is supported on free functions",
                    )),
                })
                .collect::<Result<Vec<_>>>()?;
            self.contracts.insert(
                name.to_owned(),
                Contract {
                    parameters,
                    inputs: vec![(operation.model, States::from([operation.from.clone()]))],
                    outputs: vec![(operation.model, States::from([operation.to.clone()]))],
                    result: Value::Scalar,
                },
            );
            self.active.remove(name);
            return Ok(());
        }
        let result = (|| {
            let mut context = Context {
                scope: name
                    .rsplit_once("::")
                    .map(|(scope, _)| scope)
                    .unwrap_or("")
                    .into(),
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
                        for (field, ty) in function
                            .owner
                            .as_ref()
                            .and_then(|owner| self.structs.get(owner))
                            .cloned()
                            .unwrap_or_default()
                        {
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
            let value = self.block(&function.body, &mut context)?;
            if !context.guards.is_empty() {
                return Err(Error::new(
                    function.body.span(),
                    "every OnDrop fallback must be defused on the normal path",
                ));
            }
            if !context.tasks.is_empty() {
                return Err(Error::new(
                    function.body.span(),
                    "every heliod_async task must be consumed by heliod_await",
                ));
            }
            for device in &context.devices {
                let states = possible(&device.state, &context.inputs);
                require_phase(
                    &self.models[device.model],
                    &states,
                    false,
                    function.body.span(),
                )?;
            }
            let inputs = context
                .devices
                .iter()
                .enumerate()
                .map(|(id, device)| (device.model, context.inputs[id].clone()))
                .collect();
            let outputs = context
                .devices
                .iter()
                .map(|device| (device.model, possible(&device.state, &context.inputs)))
                .collect();
            self.contracts.insert(
                name.to_owned(),
                Contract {
                    parameters,
                    inputs,
                    outputs,
                    result: value,
                },
            );
            Ok(())
        })();
        self.active.remove(name);
        result
    }

    fn initial(&self, ty: &Type, context: &mut Context) -> Result<Value> {
        if let Type::Reference(reference) = ty {
            return self.initial(&reference.elem, context);
        }
        if let Type::Tuple(tuple) = ty {
            return Ok(Value::Tuple(
                tuple
                    .elems
                    .iter()
                    .map(|ty| self.initial(ty, context))
                    .collect::<Result<_>>()?,
            ));
        }
        let name = type_name(ty)?;
        if is_scalar(&name) {
            return Ok(Value::Scalar);
        }
        for (model_index, model) in self.models.iter().enumerate() {
            let states = if name == model.registers {
                match type_argument_optional(ty)? {
                    None => model.states.keys().cloned().collect(),
                    Some(argument) => {
                        let key = type_key(argument)?;
                        let state = model
                            .states
                            .iter()
                            .find(|(_, state)| state.ty == key)
                            .ok_or_else(|| Error::new(ty.span(), "unknown typestate"))?;
                        BTreeSet::from([state.0.clone()])
                    }
                }
            } else if name == model.state_enum {
                model.states.keys().cloned().collect()
            } else {
                continue;
            };
            let id = context.devices.len();
            context.inputs.push(states);
            context.devices.push(Device {
                model: model_index,
                state: StateExpression::Input(id),
            });
            return Ok(Value::Register(id));
        }
        Err(Error::new(
            ty.span(),
            "unmodeled parameter or field type; select its cost provider",
        ))
    }

    fn block(&mut self, block: &Block, context: &mut Context) -> Result<Value> {
        let bindings = context.bindings.clone();
        let mut result = Value::Scalar;
        for statement in &block.stmts {
            result = match statement {
                Stmt::Local(local) => {
                    let initializer = local
                        .init
                        .as_ref()
                        .ok_or_else(|| Error::new(local.span(), "expected an initialized local"))?;
                    let value = self.expression(&initializer.expr, context)?;
                    bind(&local.pat, value, context)?;
                    Value::Scalar
                }
                Stmt::Expr(expression, semicolon) => {
                    let value = self.expression(expression, context)?;
                    if semicolon.is_some() {
                        Value::Scalar
                    } else {
                        value
                    }
                }
                _ => return Err(Error::new(statement.span(), "unsupported statement")),
            };
        }
        context.bindings = bindings;
        Ok(result)
    }

    fn expression(&mut self, expression: &Expr, context: &mut Context) -> Result<Value> {
        match expression {
            Expr::Path(_) | Expr::Field(_) => lookup(expression, context),
            Expr::Lit(_) => Ok(Value::Scalar),
            Expr::Reference(reference) => self.expression(&reference.expr, context),
            Expr::Paren(parenthesized) => self.expression(&parenthesized.expr, context),
            Expr::Block(block) => self.block(&block.block, context),
            Expr::Return(result) => result
                .expr
                .as_ref()
                .map_or(Ok(Value::Scalar), |value| self.expression(value, context)),
            Expr::Tuple(tuple) => Ok(Value::Tuple(
                tuple
                    .elems
                    .iter()
                    .map(|value| self.expression(value, context))
                    .collect::<Result<_>>()?,
            )),
            Expr::Unary(unary) => self.expression(&unary.expr, context),
            Expr::Binary(binary) => {
                self.expression(&binary.left, context)?;
                self.expression(&binary.right, context)?;
                Ok(Value::Scalar)
            }
            Expr::If(branch) => {
                self.expression(&branch.cond, context)?;
                let before = context.clone();
                let mut left = before.clone();
                let left_value = self.block(&branch.then_branch, &mut left)?;
                let mut right = before;
                let right_value = if let Some((_, expression)) = &branch.else_branch {
                    self.expression(expression, &mut right)?
                } else {
                    Value::Scalar
                };
                if left_value != right_value {
                    return Err(Error::new(
                        branch.span(),
                        "branches return different peripheral identities",
                    ));
                }
                *context = join(left, right)?;
                Ok(left_value)
            }
            Expr::MethodCall(call) => self.method(call, context),
            Expr::Await(awaited) => {
                let Expr::Call(call) = awaited.base.as_ref() else {
                    return Err(Error::new(
                        awaited.span(),
                        "state inference supports immediately awaited calls",
                    ));
                };
                if inference_call_name(call).as_deref() == Some("with_timeout") {
                    self.embassy_timeout(call, context)
                } else {
                    self.function_call(call, context)
                }
            }
            Expr::Call(call) => self.function_call(call, context),
            _ => Err(Error::new(
                expression.span(),
                "unsupported expression in state inference",
            )),
        }
    }

    fn function_call(&mut self, call: &syn::ExprCall, context: &mut Context) -> Result<Value> {
        let Expr::Path(path) = call.func.as_ref() else {
            return Err(Error::new(call.span(), "indirect calls are unsupported"));
        };
        let name = path
            .path
            .segments
            .last()
            .ok_or_else(|| Error::new(path.span(), "expected function name"))?
            .ident
            .to_string();
        if name == "new"
            && path
                .path
                .segments
                .iter()
                .rev()
                .nth(1)
                .is_some_and(|segment| segment.ident == "OnDrop")
        {
            return self.on_drop(call, context);
        }
        if name == "heliod_async" {
            return self.async_spawn(call, context);
        }
        if name == "heliod_await" {
            return self.await_timeout(call, context);
        }
        let values = call
            .args
            .iter()
            .map(|argument| self.expression(argument, context))
            .collect::<Result<Vec<_>>>()?;
        if matches!(name.as_str(), "Ok" | "Err" | "Some") {
            return Ok(Value::Scalar);
        }
        let requested = path
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        self.call(&requested, values, context, call.span())
    }

    fn on_drop(&mut self, call: &syn::ExprCall, context: &mut Context) -> Result<Value> {
        let Some(Expr::Closure(closure)) = call.args.first() else {
            return Err(Error::new(
                call.span(),
                "OnDrop::new expects an inline closure",
            ));
        };
        if !closure.inputs.is_empty() {
            return Err(Error::new(
                closure.span(),
                "OnDrop fallback takes no arguments",
            ));
        }
        let before = context.clone();
        let mut fallback = context.clone();
        self.expression(&closure.body, &mut fallback)?;
        let mut devices = BTreeMap::new();
        for (id, (source, target)) in before.devices.iter().zip(&fallback.devices).enumerate() {
            let source_states = possible(&source.state, &before.inputs);
            let target_states = possible(&target.state, &fallback.inputs);
            if source_states != target_states {
                require_phase(
                    &self.models[source.model],
                    &source_states,
                    true,
                    closure.span(),
                )?;
                require_phase(
                    &self.models[target.model],
                    &target_states,
                    false,
                    closure.span(),
                )?;
                devices.insert(id, target_states);
            }
        }
        if devices.is_empty() {
            return Err(Error::new(
                closure.span(),
                "OnDrop fallback must restore a transient peripheral",
            ));
        }
        let guard = context.next_guard;
        context.next_guard += 1;
        context.guards.insert(guard, InferenceGuard { devices });
        Ok(Value::Guard(guard))
    }

    fn embassy_timeout(&mut self, call: &syn::ExprCall, context: &mut Context) -> Result<Value> {
        if call.args.len() != 2 {
            return Err(Error::new(
                call.span(),
                "with_timeout expects two arguments",
            ));
        }
        inference_duration_ticks(&call.args[0])?;
        let Expr::Call(future) = &call.args[1] else {
            return Err(Error::new(
                call.args[1].span(),
                "with_timeout needs an immediately called async function",
            ));
        };
        let guarded = context
            .guards
            .values()
            .flat_map(|guard| guard.devices.keys().copied())
            .collect::<BTreeSet<_>>();
        for (id, device) in context.devices.iter().enumerate() {
            let states = possible(&device.state, &context.inputs);
            if states
                .iter()
                .any(|state| self.models[device.model].states[state].transient)
                && !guarded.contains(&id)
            {
                return Err(Error::new(
                    call.span(),
                    "transient peripheral needs an active OnDrop fallback",
                ));
            }
        }
        let value = self.function_call(future, context)?;
        for device in &context.devices {
            let states = possible(&device.state, &context.inputs);
            require_phase(&self.models[device.model], &states, false, call.span())?;
        }
        for guard in context.guards.values() {
            for (device, states) in &guard.devices {
                context.devices[*device].state = StateExpression::Union(vec![
                    context.devices[*device].state.clone(),
                    StateExpression::Known(states.clone()),
                ]);
            }
        }
        Ok(value)
    }

    fn async_spawn(&mut self, call: &syn::ExprCall, context: &mut Context) -> Result<Value> {
        if call.args.len() != 2 {
            return Err(Error::new(
                call.span(),
                "heliod_async expects a peripheral and a one-argument closure",
            ));
        }
        let Value::Register(device) = lookup(&call.args[0], context)? else {
            return Err(Error::new(call.args[0].span(), "expected a peripheral"));
        };
        if context.captured.contains(&device) {
            return Err(Error::new(call.span(), "peripheral is already captured"));
        }
        let Expr::Closure(closure) = &call.args[1] else {
            return Err(Error::new(
                call.args[1].span(),
                "heliod_async needs an inline closure",
            ));
        };
        if closure.inputs.len() != 1 {
            return Err(Error::new(
                closure.span(),
                "heliod_async closure needs one peripheral parameter",
            ));
        }
        let model = &self.models[context.devices[device].model];
        let transient = model
            .states
            .iter()
            .filter(|(_, state)| state.transient)
            .map(|(name, _)| name.clone())
            .collect();
        restrict(
            &context.devices[device].state,
            &transient,
            &mut context.inputs,
            call.args[0].span(),
        )?;
        let transient = possible(&context.devices[device].state, &context.inputs);

        let mut asynchronous = context.clone();
        asynchronous.captured = (0..asynchronous.devices.len())
            .filter(|other| *other != device)
            .collect();
        bind(
            &closure.inputs[0],
            Value::Register(device),
            &mut asynchronous,
        )?;
        self.expression(&closure.body, &mut asynchronous)?;
        let normal = possible(&asynchronous.devices[device].state, &asynchronous.inputs);
        require_phase(model, &normal, false, closure.span())?;

        let task = context.next_task;
        context.next_task += 1;
        context.captured.insert(device);
        context.tasks.insert(
            task,
            InferenceTask {
                device,
                transient,
                normal,
            },
        );
        Ok(Value::Task(task))
    }

    fn await_timeout(&mut self, call: &syn::ExprCall, context: &mut Context) -> Result<Value> {
        if call.args.len() != 3 {
            return Err(Error::new(
                call.span(),
                "heliod_await expects a task, timeout, and one-argument fallback closure",
            ));
        }
        let Value::Task(task_id) = lookup(&call.args[0], context)? else {
            return Err(Error::new(call.args[0].span(), "expected a Heliod task"));
        };
        integer_literal(&call.args[1])?;
        let task = context
            .tasks
            .get(&task_id)
            .cloned()
            .ok_or_else(|| Error::new(call.args[0].span(), "task was already awaited"))?;
        let Expr::Closure(closure) = &call.args[2] else {
            return Err(Error::new(
                call.args[2].span(),
                "heliod_await needs an inline fallback closure",
            ));
        };
        if closure.inputs.len() != 1 {
            return Err(Error::new(
                closure.span(),
                "heliod_await fallback needs one peripheral parameter",
            ));
        }
        let mut fallback = context.clone();
        fallback.captured.remove(&task.device);
        fallback.devices[task.device].state = StateExpression::Known(task.transient);
        bind(
            &closure.inputs[0],
            Value::Register(task.device),
            &mut fallback,
        )?;
        self.expression(&closure.body, &mut fallback)?;
        let fallback_states = possible(&fallback.devices[task.device].state, &fallback.inputs);
        let model = &self.models[context.devices[task.device].model];
        require_phase(model, &fallback_states, false, closure.span())?;

        context.devices[task.device].state = StateExpression::Union(vec![
            StateExpression::Known(task.normal),
            StateExpression::Known(fallback_states),
        ]);
        context.captured.remove(&task.device);
        context.tasks.remove(&task_id);
        Ok(Value::Scalar)
    }

    fn method(&mut self, call: &syn::ExprMethodCall, context: &mut Context) -> Result<Value> {
        let name = call.method.to_string();
        if matches!(name.as_str(), "unwrap" | "expect") {
            let value = self.expression(&call.receiver, context)?;
            if value != Value::Scalar {
                return Err(Error::new(
                    call.receiver.span(),
                    "unwrap expects a scalar result",
                ));
            }
            for argument in &call.args {
                self.expression(argument, context)?;
            }
            return Ok(Value::Scalar);
        }
        if name == "defuse" && call.args.is_empty() {
            let Value::Guard(guard) = lookup(&call.receiver, context)? else {
                return Err(Error::new(
                    call.receiver.span(),
                    "defuse expects an OnDrop guard",
                ));
            };
            let guard = context
                .guards
                .remove(&guard)
                .ok_or_else(|| Error::new(call.span(), "OnDrop guard was already defused"))?;
            for device in guard.devices.keys() {
                let current = &context.devices[*device];
                let states = possible(&current.state, &context.inputs);
                require_phase(&self.models[current.model], &states, false, call.span())?;
            }
            return Ok(Value::Scalar);
        }
        if name == "into" && call.args.is_empty() {
            return self.expression(&call.receiver, context);
        }
        if name == "sync_state" && call.args.is_empty() {
            if let Ok(Value::Register(device)) = lookup(&call.receiver, context) {
                if context.captured.contains(&device) {
                    return Err(Error::new(
                        call.span(),
                        "captured peripheral is unavailable until heliod_await",
                    ));
                }
                let model = &self.models[context.devices[device].model];
                let states = possible(&context.devices[device].state, &context.inputs);
                require_phase(model, &states, true, call.span())?;
                let stable = synchronization_targets(model, &states, call.span())?;
                context.devices[device].state = StateExpression::Known(stable);
                return Ok(Value::Register(device));
            }
        }
        let (receiver, operation) = match call.receiver.as_ref() {
            Expr::Field(field)
                if !matches!(lookup(&call.receiver, context), Ok(Value::Register(_))) =>
            {
                let syn::Member::Named(field_name) = &field.member else {
                    return Err(Error::new(field.span(), "expected named register field"));
                };
                (field.base.as_ref(), format!("{field_name}.{name}"))
            }
            _ => (call.receiver.as_ref(), name.clone()),
        };
        if let Ok(Value::Register(id)) = lookup(receiver, context) {
            if context.captured.contains(&id) {
                return Err(Error::new(
                    call.span(),
                    "captured peripheral is unavailable until heliod_await",
                ));
            }
            for argument in &call.args {
                self.expression(argument, context)?;
            }
            let model = &self.models[context.devices[id].model];
            let allowed = model
                .operations
                .keys()
                .filter(|(_, candidate)| candidate == &operation)
                .map(|(state, _)| state.clone())
                .collect::<States>();
            if allowed.is_empty() {
                return Err(Error::new(call.span(), "unknown peripheral operation"));
            }
            restrict(
                &context.devices[id].state,
                &allowed,
                &mut context.inputs,
                call.span(),
            )?;
            let sources = possible(&context.devices[id].state, &context.inputs);
            let mut targets = States::new();
            let mut transition = false;
            for source in sources {
                let operation = &model.operations[&(source, operation.clone())];
                targets.insert(operation.target.clone());
                transition |= operation.transition;
            }
            if transition {
                if targets.iter().any(|state| model.states[state].transient) {
                    targets = hardware_reachable(model, &targets);
                }
                context.devices[id].state = StateExpression::Known(targets);
                Ok(Value::Register(id))
            } else {
                Ok(Value::Scalar)
            }
        } else if let Ok(Value::Object(owner, fields)) = lookup(&call.receiver, context) {
            let mut values = vec![Value::Object(owner.clone(), fields)];
            for argument in &call.args {
                values.push(self.expression(argument, context)?);
            }
            self.call(&qualify(&owner, &name), values, context, call.span())
        } else {
            Err(Error::new(call.span(), "unknown method in state inference"))
        }
    }

    fn call(
        &mut self,
        requested: &str,
        arguments: Vec<Value>,
        context: &mut Context,
        span: proc_macro2::Span,
    ) -> Result<Value> {
        let name = resolve(&context.scope, requested, &self.functions);
        self.summarize(&name)?;
        let contract = self.contracts[&name].clone();
        if contract.parameters.len() != arguments.len() {
            return Err(Error::new(span, "helper argument count mismatch"));
        }
        let mut mapped = vec![None; contract.inputs.len()];
        for (formal, actual) in contract.parameters.iter().zip(&arguments) {
            map_argument(formal, actual, &mut mapped, span)?;
        }
        let mapped = mapped
            .into_iter()
            .map(|device| device.ok_or_else(|| Error::new(span, "unmapped peripheral argument")))
            .collect::<Result<Vec<_>>>()?;
        for (formal, actual) in contract.inputs.iter().zip(&mapped) {
            if context.devices[*actual].model != formal.0 {
                return Err(Error::new(span, "peripheral argument type mismatch"));
            }
            restrict(
                &context.devices[*actual].state,
                &formal.1,
                &mut context.inputs,
                span,
            )?;
        }
        for (formal, actual) in contract.outputs.iter().zip(&mapped) {
            context.devices[*actual].state = StateExpression::Known(formal.1.clone());
        }
        map_result(&contract.result, &mapped, span)
    }
}

fn possible(expression: &StateExpression, inputs: &[States]) -> States {
    match expression {
        StateExpression::Input(id) => inputs[*id].clone(),
        StateExpression::Known(states) => states.clone(),
        StateExpression::Union(parts) => parts
            .iter()
            .flat_map(|part| possible(part, inputs))
            .collect(),
    }
}

fn restrict(
    expression: &StateExpression,
    allowed: &States,
    inputs: &mut [States],
    span: proc_macro2::Span,
) -> Result<()> {
    match expression {
        StateExpression::Input(id) => {
            inputs[*id] = inputs[*id].intersection(allowed).cloned().collect();
            if inputs[*id].is_empty() {
                return Err(Error::new(
                    span,
                    "peripheral state constraints are unsatisfiable",
                ));
            }
        }
        StateExpression::Known(states) => {
            if !states.is_subset(allowed) {
                return Err(Error::new(
                    span,
                    format!(
                        "operation is unavailable from inferred states: {}",
                        states
                            .difference(allowed)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
        }
        StateExpression::Union(parts) => {
            for part in parts {
                restrict(part, allowed, inputs, span)?;
            }
        }
    }
    Ok(())
}

fn join(left: Context, right: Context) -> Result<Context> {
    if left.devices.len() != right.devices.len() {
        return Err(failure("branches change peripheral identities"));
    }
    if left.captured != right.captured
        || left.tasks.keys().collect::<Vec<_>>() != right.tasks.keys().collect::<Vec<_>>()
        || left.guards.keys().collect::<Vec<_>>() != right.guards.keys().collect::<Vec<_>>()
    {
        return Err(failure("branches must leave the same async tasks pending"));
    }
    let mut joined = left.clone();
    for ((device, other), (input, other_input)) in joined
        .devices
        .iter_mut()
        .zip(&right.devices)
        .zip(joined.inputs.iter_mut().zip(&right.inputs))
    {
        if device.model != other.model {
            return Err(failure("branches change peripheral types"));
        }
        device.state = StateExpression::Union(vec![device.state.clone(), other.state.clone()]);
        *input = input.intersection(other_input).cloned().collect();
        if input.is_empty() {
            return Err(failure("branch state constraints are unsatisfiable"));
        }
    }
    joined
        .bindings
        .retain(|name, value| right.bindings.get(name) == Some(value));
    for (id, task) in &mut joined.tasks {
        let other = &right.tasks[id];
        if task.device != other.device {
            return Err(failure("branches change captured peripheral identities"));
        }
        task.transient.extend(other.transient.iter().cloned());
        task.normal.extend(other.normal.iter().cloned());
    }
    joined.next_task = left.next_task.max(right.next_task);
    Ok(joined)
}

fn lookup(expression: &Expr, context: &Context) -> Result<Value> {
    match expression {
        // Associated constants such as `Task::ENABLE::SET` are scalar values,
        // not local bindings whose peripheral identity must be resolved.
        Expr::Path(path) if path.path.segments.len() > 1 => Ok(Value::Scalar),
        Expr::Path(path) if path.path.segments.len() == 1 => context
            .bindings
            .get(&path.path.segments[0].ident.to_string())
            .cloned()
            .ok_or_else(|| Error::new(path.span(), "unknown binding")),
        Expr::Field(field) => {
            let Value::Object(_, fields) = lookup(&field.base, context)? else {
                return Err(Error::new(field.span(), "expected object field"));
            };
            let syn::Member::Named(name) = &field.member else {
                return Err(Error::new(field.span(), "expected named field"));
            };
            fields
                .get(&name.to_string())
                .cloned()
                .ok_or_else(|| Error::new(field.span(), "unknown field"))
        }
        Expr::Paren(value) => lookup(&value.expr, context),
        _ => Err(Error::new(expression.span(), "expected a named receiver")),
    }
}

fn bind(pattern: &Pat, value: Value, context: &mut Context) -> Result<()> {
    match pattern {
        Pat::Ident(pattern) => {
            context.bindings.insert(pattern.ident.to_string(), value);
            Ok(())
        }
        Pat::Wild(_) => Ok(()),
        Pat::Type(pattern) => bind(&pattern.pat, value, context),
        Pat::Tuple(pattern) => {
            let Value::Tuple(values) = value else {
                return Err(Error::new(pattern.span(), "tuple binding mismatch"));
            };
            if values.len() != pattern.elems.len() {
                return Err(Error::new(pattern.span(), "tuple binding mismatch"));
            }
            for (pattern, value) in pattern.elems.iter().zip(values) {
                bind(pattern, value, context)?;
            }
            Ok(())
        }
        _ => Err(Error::new(pattern.span(), "unsupported binding")),
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
        (Value::Task(_), _) | (_, Value::Task(_)) => Err(Error::new(
            span,
            "Heliod tasks cannot cross function boundaries",
        )),
        (Value::Guard(_), _) | (_, Value::Guard(_)) => Err(Error::new(
            span,
            "OnDrop guards cannot cross function boundaries",
        )),
        (Value::Register(formal), Value::Register(actual)) => {
            match devices[*formal] {
                Some(existing) if existing != *actual => {
                    return Err(Error::new(span, "inconsistent peripheral mapping"))
                }
                _ => devices[*formal] = Some(*actual),
            }
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
                map_argument(formal, &actual[name], devices, span)?;
            }
            Ok(())
        }
        _ => Err(Error::new(span, "helper argument type mismatch")),
    }
}

fn map_result(value: &Value, devices: &[usize], span: proc_macro2::Span) -> Result<Value> {
    Ok(match value {
        Value::Scalar => Value::Scalar,
        Value::Register(id) => Value::Register(
            *devices
                .get(*id)
                .ok_or_else(|| Error::new(span, "invalid peripheral result"))?,
        ),
        Value::Task(_) => {
            return Err(Error::new(
                span,
                "Heliod tasks cannot cross function boundaries",
            ))
        }
        Value::Guard(_) => {
            return Err(Error::new(
                span,
                "OnDrop guards cannot cross function boundaries",
            ))
        }
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

fn require_phase(
    model: &Peripheral,
    states: &States,
    transient: bool,
    span: proc_macro2::Span,
) -> Result<()> {
    if states.is_empty()
        || states
            .iter()
            .any(|state| model.states[state].transient != transient)
    {
        let expected = if transient { "transient" } else { "stable" };
        return Err(Error::new(
            span,
            format!("peripheral must be in only {expected} states"),
        ));
    }
    Ok(())
}

fn hardware_reachable(model: &Peripheral, initial: &States) -> States {
    let mut reachable = initial.clone();
    let mut work = initial.iter().cloned().collect::<Vec<_>>();
    while let Some(source) = work.pop() {
        for operation in model
            .operations
            .iter()
            .filter(|((state, _), operation)| {
                state == &source
                    && operation.transition
                    && model.states[&operation.target].transient
            })
            .map(|(_, operation)| operation)
        {
            if reachable.insert(operation.target.clone()) {
                work.push(operation.target.clone());
            }
        }
    }
    reachable
}

fn synchronization_targets(
    model: &Peripheral,
    states: &States,
    span: proc_macro2::Span,
) -> Result<States> {
    let mut targets = States::new();
    for state in states {
        let from_state = model
            .operations
            .iter()
            .filter(|((source, _), operation)| {
                source == state
                    && operation.transition
                    && !model.states[&operation.target].transient
            })
            .map(|(_, operation)| operation.target.clone())
            .collect::<States>();
        if from_state.is_empty() {
            return Err(Error::new(
                span,
                format!(
                    "transient state `{state}` has no declared stable synchronization transition"
                ),
            ));
        }
        targets.extend(from_state);
    }
    Ok(targets)
}

fn integer_literal(expression: &Expr) -> Result<usize> {
    let Expr::Lit(literal) = expression else {
        return Err(Error::new(
            expression.span(),
            "timeout must be a nonnegative integer literal",
        ));
    };
    let syn::Lit::Int(integer) = &literal.lit else {
        return Err(Error::new(
            expression.span(),
            "timeout must be a nonnegative integer literal",
        ));
    };
    integer.base10_parse()
}

fn inference_call_name(call: &syn::ExprCall) -> Option<String> {
    let Expr::Path(path) = call.func.as_ref() else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn inference_duration_ticks(expression: &Expr) -> Result<usize> {
    let Expr::Call(call) = expression else {
        return Err(Error::new(
            expression.span(),
            "timeout must use Duration::from_ticks with a literal",
        ));
    };
    let Expr::Path(path) = call.func.as_ref() else {
        return Err(Error::new(call.span(), "expected Duration::from_ticks"));
    };
    if path
        .path
        .segments
        .last()
        .is_none_or(|segment| segment.ident != "from_ticks")
        || call.args.len() != 1
    {
        return Err(Error::new(call.span(), "expected Duration::from_ticks(N)"));
    }
    integer_literal(&call.args[0])
}

fn resolve(scope: &str, requested: &str, functions: &BTreeMap<String, Function>) -> String {
    if let Some(method) = requested.strip_prefix("Self::") {
        return qualify(scope, method);
    }
    let mut scope = scope;
    loop {
        let candidate = qualify(scope, requested.trim_start_matches("crate::"));
        if functions.contains_key(&candidate) || scope.is_empty() {
            return candidate;
        }
        scope = scope
            .rsplit_once("::")
            .map(|(parent, _)| parent)
            .unwrap_or("");
    }
}

fn type_name(ty: &Type) -> Result<String> {
    if let Type::Reference(reference) = ty {
        return type_name(&reference.elem);
    }
    let Type::Path(path) = ty else {
        return Err(Error::new(ty.span(), "unsupported type"));
    };
    Ok(path
        .path
        .segments
        .last()
        .ok_or_else(|| Error::new(ty.span(), "expected type name"))?
        .ident
        .to_string())
}

fn type_argument_optional(ty: &Type) -> Result<Option<&Type>> {
    let Type::Path(path) = ty else {
        return Err(Error::new(ty.span(), "unsupported type"));
    };
    let segment = path
        .path
        .segments
        .last()
        .ok_or_else(|| Error::new(ty.span(), "expected type name"))?;
    match &segment.arguments {
        PathArguments::None => Ok(None),
        PathArguments::AngleBracketed(arguments) => match arguments.args.first() {
            Some(GenericArgument::Type(ty)) => Ok(Some(ty)),
            _ => Err(Error::new(arguments.span(), "expected a type argument")),
        },
        _ => Err(Error::new(segment.span(), "unsupported type arguments")),
    }
}

fn type_key(ty: &Type) -> Result<String> {
    if matches!(ty, Type::Path(_)) {
        Ok(ty.to_token_stream().to_string().replace(' ', ""))
    } else {
        Err(Error::new(ty.span(), "unsupported typestate"))
    }
}

fn is_scalar(name: &str) -> bool {
    matches!(
        name,
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
            | "()"
    )
}

fn qualify(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{scope}::{name}")
    }
}

fn failure(message: &str) -> Error {
    Error::new(proc_macro2::Span::call_site(), message)
}
