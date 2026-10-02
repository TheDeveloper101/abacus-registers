//! Concrete state sets and integer costs. Rust remains responsible for value types
//! and ownership; this pass tracks physical peripherals across Rust bindings.
use super::model::Peripheral;
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
    Cell(usize),
    Tuple(Vec<Value>),
    Object(String, BTreeMap<String, Value>),
}
#[derive(Clone)]
struct Device {
    model: usize,
    states: States,
}
#[derive(Clone, Default)]
struct Context {
    scope: String,
    devices: Vec<Device>,
    bindings: BTreeMap<String, Value>,
}
struct Evaluation {
    value: Value,
    cost: usize,
    returned: bool,
}
impl Evaluation {
    fn new(value: Value, cost: usize) -> Self {
        Self {
            value,
            cost,
            returned: false,
        }
    }
}
#[derive(Clone)]
struct Function {
    signature: Signature,
    body: Block,
    owner: Option<String>,
}
struct Checker<'a> {
    models: &'a [Peripheral],
    functions: BTreeMap<String, Function>,
    structs: BTreeMap<String, BTreeMap<String, Type>>,
}

pub fn check(item: &Item, models: &[Peripheral]) -> Result<Vec<(String, usize)>> {
    let mut checker = Checker {
        models,
        functions: BTreeMap::new(),
        structs: BTreeMap::new(),
    };
    checker.collect(item, "")?;
    let mut reports = Vec::new();
    for (name, function) in checker.functions.clone() {
        let mut context = Context {
            scope: name
                .rsplit_once("::")
                .map(|(scope, _)| scope)
                .unwrap_or("")
                .into(),
            ..Context::default()
        };
        for parameter in &function.signature.inputs {
            match parameter {
                FnArg::Typed(parameter) => {
                    let value = checker.initial(&parameter.ty, &mut context)?;
                    bind(&parameter.pat, value, &mut context)?;
                }
                FnArg::Receiver(_) => {
                    let mut fields = BTreeMap::new();
                    if let Some(types) =
                        function.owner.as_ref().and_then(|o| checker.structs.get(o))
                    {
                        for (name, ty) in types {
                            fields.insert(name.clone(), checker.initial(ty, &mut context)?);
                        }
                    }
                    context.bindings.insert(
                        "self".into(),
                        Value::Object(function.owner.clone().unwrap_or_default(), fields),
                    );
                }
            }
        }
        let result = checker.block(&function.body, &mut context)?;
        reports.push((name, result.cost));
    }
    if reports.is_empty() {
        return Err(Error::new(
            item.span(),
            "expected a function, inherent impl, or inline API module",
        ));
    }
    Ok(reports)
}
impl Checker<'_> {
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
            let states = if name == model.registers {
                let state_type = type_key(type_argument(ty)?)?;
                let state = model
                    .states
                    .iter()
                    .find(|(_, s)| s.ty == state_type)
                    .ok_or_else(|| Error::new(ty.span(), "unknown or generic typestate"))?;
                BTreeSet::from([state.0.clone()])
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
            let id = context.devices.len();
            context.devices.push(Device {
                model: index,
                states,
            });
            return Ok(Value::Register(id));
        }
        Err(Error::new(
            ty.span(),
            "unmodeled parameter or field type; select its cost provider",
        ))
    }
    fn background(&self, context: &Context) -> usize {
        let mut total = 0;
        for device in &context.devices {
            let states = &self.models[device.model].states;
            let cost = device
                .states
                .iter()
                .map(|s| states[s].cost)
                .max()
                .unwrap_or(0);
            total += cost;
        }
        total
    }
    fn block(&mut self, block: &Block, context: &mut Context) -> Result<Evaluation> {
        let original = context.bindings.clone();
        let mut result = Evaluation::new(Value::Scalar, 0);
        for statement in &block.stmts {
            if !matches!(statement, Stmt::Expr(Expr::Block(_), _)) {
                result.cost += self.background(context);
            }
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
                    Evaluation::new(Value::Scalar, value.cost)
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
        let background = self.background(context);
        let mut result = self.expression(expr, context)?;
        result.cost += background;
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
                for expr in &tuple.elems {
                    let value = self.expression(expr, context)?;
                    cost += value.cost;
                    values.push(value.value);
                }
                Ok(Evaluation::new(
                    if values.is_empty() {
                        Value::Scalar
                    } else {
                        Value::Tuple(values)
                    },
                    cost,
                ))
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
                Ok(Evaluation::new(Value::Scalar, left.cost + right.cost))
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
                for argument in &call.args {
                    let value = self.expression(argument, context)?;
                    cost += value.cost;
                    values.push(value.value);
                }
                if matches!(name.as_str(), "Ok" | "Err" | "Some") {
                    return Ok(Evaluation::new(Value::Scalar, cost));
                }
                let key = path
                    .path
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect::<Vec<_>>()
                    .join("::");
                let mut result = self.call(&key, values, context)?;
                result.cost += cost;
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
                cost = cost.max(op.cost);
                transition = op.transition;
            }
            if targets.iter().any(|s| model.states[s].transient) {
                return Err(Error::new(
                    call.span(),
                    "transient states require hardware-concurrency analysis",
                ));
            }
            for arg in &call.args {
                cost += self.expression(arg, context)?.cost;
            }
            context.devices[id].states = targets;
            return Ok(Evaluation::new(
                if transition {
                    Value::Register(id)
                } else {
                    Value::Scalar
                },
                cost,
            ));
        }
        if let Ok(Value::Object(owner, fields)) = lookup(&call.receiver, context) {
            let value = Value::Object(owner.clone(), fields);
            let mut values = vec![value];
            let mut cost = 0;
            for arg in &call.args {
                let value = self.expression(arg, context)?;
                values.push(value.value);
                cost += value.cost;
            }
            let mut result = self.call(&qualify(&owner, &name), values, context)?;
            result.cost += cost;
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
        Ok(Evaluation::new(Value::Scalar, result.cost))
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
        Ok(result)
    }
    fn call(
        &mut self,
        requested: &str,
        values: Vec<Value>,
        context: &mut Context,
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
        let saved = std::mem::take(&mut context.bindings);
        for (argument, value) in function.signature.inputs.iter().zip(values) {
            match argument {
                FnArg::Typed(argument) => bind(&argument.pat, value, context)?,
                FnArg::Receiver(_) => {
                    context.bindings.insert("self".into(), value);
                }
            }
        }
        let scope = name.rsplit_once("::").map(|(scope, _)| scope).unwrap_or("");
        let saved_scope = std::mem::replace(&mut context.scope, scope.into());
        let result = self.block(&function.body, context);
        context.scope = saved_scope;
        context.bindings = saved;
        let mut result = result?;
        result.returned = false;
        Ok(result)
    }
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
    Ok(Evaluation {
        value: left.value,
        cost: left.cost.max(right.cost),
        returned: left.returned,
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
