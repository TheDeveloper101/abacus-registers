//! Frontend for actual Rust API items. The original item is emitted unchanged.
use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use syn::{
    braced, bracketed, parenthesized,
    parse::{Parse, ParseStream, Parser},
    punctuated::Punctuated,
    spanned::Spanned,
    Block, Error, Expr, FnArg, GenericArgument, Item, Pat, Path, PathArguments, Result, ReturnType,
    Signature, Stmt, Token, Type as RustType,
};

use super::model::{error, Cost, Instance, Peripheral, Type};
use crate::macro_input::MacroInput;

pub fn start(attr: TokenStream, item: TokenStream) -> Result<TokenStream> {
    let providers = Punctuated::<Path, Token![,]>::parse_terminated.parse2(attr)?;
    if providers.is_empty() {
        return Err(error(
            "select generated cost metadata, e.g. #[abacus_cost(SensorCost)]",
        ));
    }
    Ok(quote! { ::abacus_registers::__abacus_cost! { [] [#providers] {#item} } })
}
struct Request {
    models: Vec<(MacroInput, syn::DeriveInput)>,
    remaining: Punctuated<Path, Token![,]>,
    item: Item,
}
impl Parse for Request {
    fn parse(input: ParseStream) -> Result<Self> {
        let entries;
        bracketed!(entries in input);
        let mut models = Vec::new();
        while !entries.is_empty() {
            let entry;
            parenthesized!(entry in entries);
            let attr;
            parenthesized!(attr in entry);
            let item;
            parenthesized!(item in entry);
            models.push((attr.parse()?, item.parse()?));
            if !entry.is_empty() {
                return Err(entry.error("invalid cost metadata"));
            }
        }
        let remaining;
        bracketed!(remaining in input);
        let remaining = remaining.parse_terminated(Path::parse, Token![,])?;
        let body;
        braced!(body in input);
        let item = body.parse()?;
        if !body.is_empty() || !input.is_empty() {
            return Err(input.error("expected one API item"));
        }
        Ok(Self {
            models,
            remaining,
            item,
        })
    }
}
pub fn expand(tokens: TokenStream) -> Result<TokenStream> {
    let request: Request = syn::parse2(tokens)?;
    let item = &request.item;
    if let Some(next) = request.remaining.first() {
        let remaining = request.remaining.iter().skip(1);
        let specs = request
            .models
            .iter()
            .map(|(m, ast)| super::specification(m, ast));
        return Ok(quote! { #next! { [#(#specs)*] [#(#remaining),*] {#item} } });
    }
    let mut models = BTreeMap::new();
    for (input, ast) in &request.models {
        let syn::Data::Struct(data) = &ast.data else {
            return Err(error("invalid register metadata"));
        };
        let (_, registers) = crate::process_struct_fields(&data.fields)?;
        let model = super::peripheral(input, &registers)?;
        if models.insert(model.register_type.clone(), model).is_some() {
            return Err(error("ambiguous register type name in cost providers"));
        }
    }
    let reports = analyze(item, &models)?;
    for (name, cost) in reports {
        eprintln!("{}", report(&name, cost));
    }
    Ok(quote! { #item })
}

pub fn report(name: &str, cost: Cost) -> String {
    return format!("Cost for {name}: {}", cost.reported());
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Unit,
    Int,
    Bool,
    Peripheral(usize, bool), // device identity, borrowed
    Tuple(Vec<Value>),
    Wrapped(String, Box<Value>),
    Alternatives(Vec<Value>),
    Object(BTreeMap<String, Value>),
    Cell(usize),
}
#[derive(Clone)]
struct Context {
    devices: BTreeMap<usize, Instance>,
    vars: BTreeMap<String, Value>,
}
struct Eval {
    value: Value,
    cost: Cost,
    returned: bool,
}
impl Eval {
    fn pure(value: Value) -> Self {
        Self {
            value,
            cost: Cost::ZERO,
            returned: false,
        }
    }
}
#[derive(Clone)]
struct Function {
    sig: Signature,
    body: Block,
    owner: Option<String>,
}
struct Analyzer<'a> {
    models: &'a BTreeMap<String, Peripheral>,
    functions: BTreeMap<String, Function>,
    fields: BTreeMap<String, BTreeMap<String, RustType>>,
    calls: Vec<String>,
}

pub fn analyze(item: &Item, models: &BTreeMap<String, Peripheral>) -> Result<Vec<(String, Cost)>> {
    let mut analyzer = Analyzer {
        models,
        functions: BTreeMap::new(),
        fields: BTreeMap::new(),
        calls: Vec::new(),
    };
    analyzer.collect(item)?;
    let mut reports = Vec::new();
    for (name, f) in analyzer.functions.clone() {
        let mut context = Context {
            devices: BTreeMap::new(),
            vars: BTreeMap::new(),
        };
        for arg in &f.sig.inputs {
            match arg {
                FnArg::Typed(arg) => {
                    let value = analyzer.initial(&arg.ty, &mut context)?;
                    bind(&arg.pat, value, &mut context.vars)?;
                }
                FnArg::Receiver(_) => {
                    let mut values = BTreeMap::new();
                    if let Some(fields) = f.owner.as_ref().and_then(|o| analyzer.fields.get(o)) {
                        for (name, ty) in fields {
                            values.insert(name.clone(), analyzer.initial(ty, &mut context)?);
                        }
                    }
                    context.vars.insert("self".into(), Value::Object(values));
                }
            }
        }
        analyzer.calls.push(name.clone());
        let result = analyzer.block(&f.body, &mut context)?;
        analyzer.calls.pop();
        analyzer.check_return(&f.sig.output, &result.value, &context)?;
        let label = if let Item::Mod(module) = item {
            module.ident.to_string()
        } else {
            name
        };
        reports.push((label, result.cost));
    }
    if reports.is_empty() {
        return Err(error(
            "abacus_cost requires a function, inherent impl, or inline module containing functions",
        ));
    }
    Ok(reports)
}
impl Analyzer<'_> {
    fn collect(&mut self, item: &Item) -> Result<()> {
        match item {
            Item::Fn(f) => self.insert(f.sig.ident.to_string(), Function { sig: f.sig.clone(), body: *f.block.clone(), owner: None })?,
            Item::Impl(imp) => {
                if imp.trait_.is_some() || !imp.generics.params.is_empty() { return Err(Error::new(imp.span(), "cost checking currently requires a nongeneric inherent impl")); }
                let owner = type_key(&imp.self_ty)?;
                for item in &imp.items {
                    if let syn::ImplItem::Fn(f) = item {
                        self.insert(format!("{owner}::{}", f.sig.ident), Function { sig: f.sig.clone(), body: f.block.clone(), owner: Some(owner.clone()) })?;
                    }
                }
            }
            Item::Mod(module) => {
                let (_, items) = module.content.as_ref().ok_or_else(|| Error::new(module.span(), "use an inline module so API bodies are available"))?;
                for item in items { self.collect(item)?; }
            }
            Item::Struct(s) => {
                let mut fields = BTreeMap::new();
                for f in &s.fields {
                    let name = f.ident.as_ref().ok_or_else(|| Error::new(f.span(), "tuple structs are unsupported in checked APIs"))?;
                    fields.insert(name.to_string(), f.ty.clone());
                }
                self.fields.insert(s.ident.to_string(), fields);
            }
            Item::Use(_) | Item::Const(_) | Item::Type(_) => {}
            _ => return Err(Error::new(item.span(), "unsupported item in checked API; keep register declarations outside the annotated API module")),
        }
        Ok(())
    }
    fn insert(&mut self, name: String, function: Function) -> Result<()> {
        if function.sig.asyncness.is_some()
            || function.sig.unsafety.is_some()
            || !function.sig.generics.params.is_empty()
        {
            return Err(Error::new(
                function.sig.span(),
                "async, unsafe, and generic API functions require additional cost rules",
            ));
        }
        if self.functions.insert(name, function).is_some() {
            return Err(error("ambiguous function name in checked API"));
        }
        Ok(())
    }
    fn model_type(&self, ty: &RustType) -> Result<Option<(String, BTreeSet<String>, bool)>> {
        if let RustType::Reference(r) = ty {
            return Ok(self.model_type(&r.elem)?.map(|(m, s, _)| (m, s, true)));
        }
        let RustType::Path(path) = ty else {
            return Ok(None);
        };
        let segment = path
            .path
            .segments
            .last()
            .ok_or_else(|| error("empty type path"))?;
        let name = segment.ident.to_string();
        for (key, model) in self.models {
            if name == model.enum_type {
                return Ok(Some((
                    key.clone(),
                    model.background.keys().cloned().collect(),
                    false,
                )));
            }
            if name == model.register_type {
                let PathArguments::AngleBracketed(args) = &segment.arguments else {
                    return Err(Error::new(
                        ty.span(),
                        "register arguments need a concrete typestate",
                    ));
                };
                let Some(GenericArgument::Type(state)) = args.args.first() else {
                    return Err(Error::new(ty.span(), "expected concrete typestate"));
                };
                let state = type_key(state)?;
                let state = model
                    .state_types
                    .get(&state)
                    .ok_or_else(|| Error::new(ty.span(), "unknown or generic peripheral state"))?;
                return Ok(Some((key.clone(), BTreeSet::from([state.clone()]), false)));
            }
        }
        Ok(None)
    }
    fn initial(&self, ty: &RustType, context: &mut Context) -> Result<Value> {
        if let Some((model, states, borrowed)) = self.model_type(ty)? {
            let spec = &self.models[&model];
            if states.iter().any(|s| spec.transient.contains(s)) {
                return Err(Error::new(
                    ty.span(),
                    "transient states require hardware-concurrency analysis",
                ));
            }
            let id = context.devices.len();
            context.devices.insert(id, Instance { model, states });
            return Ok(Value::Peripheral(id, borrowed));
        }
        if let RustType::Reference(r) = ty {
            return self.initial(&r.elem, context);
        }
        if let RustType::Tuple(tuple) = ty {
            if tuple.elems.is_empty() {
                return Ok(Value::Unit);
            }
            return Ok(Value::Tuple(
                tuple
                    .elems
                    .iter()
                    .map(|t| self.initial(t, context))
                    .collect::<Result<_>>()?,
            ));
        }
        let key = type_key(ty)?;
        if key == "bool" {
            return Ok(Value::Bool);
        }
        if integer_type(&key) {
            return Ok(Value::Int);
        }
        if let Some(inner) = generic_type(ty, "AbacusCell", 0) {
            let value = self.initial(inner, context)?;
            if let Value::Peripheral(id, _) = value {
                return Ok(Value::Cell(id));
            }
        }
        Err(Error::new(
            ty.span(),
            "unsupported parameter/field type in cost-checked API",
        ))
    }
    fn check_type(&self, ty: &RustType, value: &Value, context: &Context) -> Result<()> {
        if let Value::Alternatives(values) = value {
            for v in values {
                self.check_type(ty, v, context)?;
            }
            return Ok(());
        }
        if let Some((model, states, borrowed)) = self.model_type(ty)? {
            if let Value::Peripheral(id, actual_borrow) = value {
                let actual = &context.devices[id];
                if actual.model == model
                    && actual.states.is_subset(&states)
                    && (borrowed || !actual_borrow)
                {
                    return Ok(());
                }
            }
            return Err(Error::new(
                ty.span(),
                "peripheral argument/return does not satisfy its Rust typestate signature",
            ));
        }
        if let RustType::Reference(r) = ty {
            return self.check_type(&r.elem, value, context);
        }
        if let (Some(inner), Value::Cell(id)) = (generic_type(ty, "AbacusCell", 0), value) {
            return self.check_type(inner, &Value::Peripheral(*id, false), context);
        }
        if let RustType::Tuple(tuple) = ty {
            if tuple.elems.is_empty() && *value == Value::Unit {
                return Ok(());
            }
            if let Value::Tuple(values) = value {
                if values.len() == tuple.elems.len() {
                    for (t, v) in tuple.elems.iter().zip(values) {
                        self.check_type(t, v, context)?;
                    }
                    return Ok(());
                }
            }
        }
        if let Ok(key) = type_key(ty) {
            if key == "bool" && *value == Value::Bool || integer_type(&key) && *value == Value::Int
            {
                return Ok(());
            }
        }
        if let Value::Wrapped(kind, inner) = value {
            let expected = match kind.as_str() {
                "Ok" => generic_type(ty, "Result", 0),
                "Err" => generic_type(ty, "Result", 1),
                "Some" => generic_type(ty, "Option", 0),
                "None" if generic_type(ty, "Option", 0).is_some() => return Ok(()),
                _ => None,
            };
            if let Some(expected) = expected {
                return self.check_type(expected, inner, context);
            }
        }
        Err(Error::new(
            ty.span(),
            "value does not match the checked Rust type",
        ))
    }
    fn check_return(&self, output: &ReturnType, value: &Value, context: &Context) -> Result<()> {
        match output {
            ReturnType::Default if *value == Value::Unit => Ok(()),
            ReturnType::Type(_, ty) => self.check_type(ty, value, context),
            _ => Err(error("function with no return type must return unit")),
        }
    }
    fn background(&self, context: &Context) -> Result<Cost> {
        let mut cost = Cost::ZERO;
        for device in context.devices.values() {
            let model = &self.models[&device.model];
            let mut maximum = Cost::ZERO;
            for state in &device.states {
                if model.transient.contains(state) {
                    return Err(error(
                        "transient states require hardware-concurrency analysis",
                    ));
                }
                maximum = maximum
                    .max(model.background[state].ok_or_else(|| {
                        error(format!("state `{state}` is missing cost = ..."))
                    })?)?;
            }
            cost = cost.add(maximum)?;
        }
        Ok(cost)
    }
    fn block(&mut self, block: &Block, context: &mut Context) -> Result<Eval> {
        let mut result = Eval::pure(Value::Unit);
        let mut shadows = BTreeMap::new();
        for stmt in &block.stmts {
            if !matches!(stmt, Stmt::Expr(Expr::Block(_), _)) {
                result.cost = result.cost.add(self.background(context)?)?;
            }
            let next = match stmt {
                Stmt::Local(local) => {
                    let init = local.init.as_ref().ok_or_else(|| {
                        Error::new(local.span(), "uninitialized locals are unsupported")
                    })?;
                    if init.diverge.is_some() {
                        return Err(Error::new(local.span(), "let-else is unsupported"));
                    }
                    let eval = self.expr(&init.expr, context)?;
                    if eval.returned {
                        return Err(Error::new(
                            local.span(),
                            "return inside a let initializer is unsupported",
                        ));
                    }
                    for name in names(&local.pat)? {
                        shadows
                            .entry(name.clone())
                            .or_insert_with(|| context.vars.get(&name).cloned());
                    }
                    if let Pat::Type(typed) = &local.pat {
                        self.check_type(&typed.ty, &eval.value, context)?;
                    }
                    bind(&local.pat, eval.value, &mut context.vars)?;
                    Eval {
                        value: Value::Unit,
                        ..eval
                    }
                }
                Stmt::Expr(expr, semi) => {
                    let mut eval = self.expr(expr, context)?;
                    if semi.is_some() && !eval.returned {
                        eval.value = Value::Unit;
                    }
                    eval
                }
                _ => {
                    return Err(Error::new(
                        stmt.span(),
                        "unsupported statement in cost-checked function",
                    ))
                }
            };
            result.cost = result.cost.add(next.cost)?;
            result.value = next.value;
            result.returned = next.returned;
            if result.returned {
                break;
            }
        }
        for (name, old) in shadows {
            if let Some(value) = old {
                context.vars.insert(name, value);
            } else {
                context.vars.remove(&name);
            }
        }
        Ok(result)
    }
    // A bare closure/arm expression is itself a command, just like a block's
    // tail expression. Braces must not change background charging.
    fn body_expr(&mut self, expr: &Expr, context: &mut Context) -> Result<Eval> {
        if let Expr::Block(block) = expr {
            return self.block(&block.block, context);
        }
        let background = self.background(context)?;
        let mut result = self.expr(expr, context)?;
        result.cost = background.add(result.cost)?;
        Ok(result)
    }
    fn expr(&mut self, expr: &Expr, context: &mut Context) -> Result<Eval> {
        match expr {
            Expr::Path(path) => {
                let name = path
                    .path
                    .get_ident()
                    .ok_or_else(|| Error::new(expr.span(), "unsupported value path"))?
                    .to_string();
                if name == "None" {
                    return Ok(Eval::pure(Value::Wrapped(name, Box::new(Value::Unit))));
                }
                let value = context.vars.get(&name).cloned().ok_or_else(|| {
                    Error::new(expr.span(), format!("unknown or moved variable `{name}`"))
                })?;
                if owned(&value) {
                    context.vars.remove(&name);
                }
                Ok(Eval::pure(value))
            }
            Expr::Lit(lit) => Ok(Eval::pure(match lit.lit {
                syn::Lit::Int(_) => Value::Int,
                syn::Lit::Bool(_) => Value::Bool,
                _ => return Err(Error::new(expr.span(), "unsupported literal")),
            })),
            Expr::Paren(p) => self.expr(&p.expr, context),
            Expr::Group(p) => self.expr(&p.expr, context),
            Expr::Block(b) => self.block(&b.block, context),
            Expr::Reference(reference) => {
                let mut value = lookup(&reference.expr, context)?;
                if let Value::Peripheral(_, borrowed) = &mut value {
                    *borrowed = true;
                }
                Ok(Eval::pure(value))
            }
            Expr::Field(_) => Ok(Eval::pure(lookup(expr, context)?)),
            Expr::Tuple(tuple) => {
                let mut values = Vec::new();
                let mut cost = Cost::ZERO;
                for expr in &tuple.elems {
                    let eval = self.expr(expr, context)?;
                    if eval.returned {
                        return Err(error("return within a tuple is unsupported"));
                    }
                    values.push(eval.value);
                    cost = cost.add(eval.cost)?;
                }
                Ok(Eval {
                    value: if values.is_empty() {
                        Value::Unit
                    } else {
                        Value::Tuple(values)
                    },
                    cost,
                    returned: false,
                })
            }
            Expr::Return(ret) => {
                let mut result = if let Some(expr) = &ret.expr {
                    self.expr(expr, context)?
                } else {
                    Eval::pure(Value::Unit)
                };
                result.returned = true;
                Ok(result)
            }
            Expr::If(branch) => {
                let cond = self.expr(&branch.cond, context)?;
                expect(&cond.value, &Value::Bool, &branch.cond)?;
                let mut left = context.clone();
                let mut right = context.clone();
                let a = self.block(&branch.then_branch, &mut left)?;
                let b = if let Some((_, other)) = &branch.else_branch {
                    self.body_expr(other, &mut right)?
                } else {
                    Eval::pure(Value::Unit)
                };
                if a.returned != b.returned {
                    return Err(Error::new(expr.span(), "mixed early-return/fallthrough branches require additional control-flow analysis"));
                }
                *context = join_context(&left, &right)?;
                Ok(Eval {
                    value: join_value(&a.value, &b.value)?,
                    cost: cond.cost.add(a.cost.max(b.cost)?)?,
                    returned: a.returned,
                })
            }
            Expr::Match(branch) => {
                let subject = self.expr(&branch.expr, context)?;
                let Value::Peripheral(id, borrowed) = subject.value else {
                    return Err(Error::new(
                        expr.span(),
                        "match currently requires an Abacus state enum",
                    ));
                };
                let device = &context.devices[&id];
                let model = &self.models[&device.model];
                let enum_type = model.enum_type.clone();
                let mut remaining = device.states.clone();
                let mut joined: Option<(Context, Eval)> = None;
                for arm in &branch.arms {
                    if arm.guard.is_some() {
                        return Err(Error::new(arm.span(), "match guards are unsupported"));
                    }
                    let (accepted, binding) = match &arm.pat {
                        Pat::TupleStruct(p) => {
                            if p.path.segments.len() < 2
                                || p.path.segments.iter().rev().nth(1).unwrap().ident != enum_type
                                || p.elems.len() != 1
                            {
                                return Err(Error::new(
                                    p.span(),
                                    "expected PeripheralStateEnum::State(binding)",
                                ));
                            }
                            let state = p.path.segments.last().unwrap().ident.to_string();
                            if !model.background.contains_key(&state) {
                                return Err(Error::new(
                                    p.span(),
                                    "unknown peripheral enum variant",
                                ));
                            }
                            (BTreeSet::from([state]), Some(&p.elems[0]))
                        }
                        Pat::Wild(_) => (remaining.clone(), None),
                        Pat::Ident(_) => (remaining.clone(), Some(&arm.pat)),
                        _ => {
                            return Err(Error::new(
                                arm.pat.span(),
                                "unsupported peripheral match pattern",
                            ))
                        }
                    };
                    let reachable: BTreeSet<_> =
                        remaining.intersection(&accepted).cloned().collect();
                    if reachable.is_empty() {
                        continue;
                    }
                    remaining = remaining.difference(&reachable).cloned().collect();
                    let mut arm_context = context.clone();
                    arm_context.devices.get_mut(&id).unwrap().states = reachable;
                    let old = arm_context.vars.clone();
                    if let Some(pat) = binding {
                        bind(pat, Value::Peripheral(id, borrowed), &mut arm_context.vars)?;
                    }
                    let result = self.body_expr(&arm.body, &mut arm_context)?;
                    if let Some(pat) = binding {
                        for name in names(pat)? {
                            if let Some(v) = old.get(&name) {
                                arm_context.vars.insert(name, v.clone());
                            } else {
                                arm_context.vars.remove(&name);
                            }
                        }
                    }
                    joined = Some(if let Some((prior, previous)) = joined {
                        if previous.returned != result.returned {
                            return Err(Error::new(
                                expr.span(),
                                "mixed early-return/fallthrough match arms are unsupported",
                            ));
                        }
                        (
                            join_context(&prior, &arm_context)?,
                            Eval {
                                value: join_value(&previous.value, &result.value)?,
                                cost: previous.cost.max(result.cost)?,
                                returned: result.returned,
                            },
                        )
                    } else {
                        (arm_context, result)
                    });
                }
                if !remaining.is_empty() {
                    return Err(Error::new(
                        expr.span(),
                        "non-exhaustive peripheral state match",
                    ));
                }
                let (ctx, mut result) =
                    joined.ok_or_else(|| Error::new(expr.span(), "empty match"))?;
                *context = ctx;
                result.cost = result.cost.add(subject.cost)?;
                Ok(result)
            }
            Expr::MethodCall(call) => self.method(call, context),
            Expr::Call(call) => {
                let Expr::Path(path) = call.func.as_ref() else {
                    return Err(Error::new(
                        call.span(),
                        "indirect calls require a cost summary",
                    ));
                };
                let name = path.path.segments.last().unwrap().ident.to_string();
                if matches!(name.as_str(), "Ok" | "Err" | "Some") {
                    if call.args.len() != 1 {
                        return Err(Error::new(call.span(), "expected one constructor argument"));
                    }
                    let mut result = self.expr(&call.args[0], context)?;
                    result.value = Value::Wrapped(name, Box::new(result.value));
                    return Ok(result);
                }
                if path.path.segments.len() == 2 {
                    let enum_name = path.path.segments.first().unwrap().ident.to_string();
                    if let Some(model) = self.models.values().find(|m| m.enum_type == enum_name) {
                        if !model.background.contains_key(&name) || call.args.len() != 1 {
                            return Err(Error::new(
                                call.span(),
                                "invalid peripheral enum constructor",
                            ));
                        }
                        let result = self.expr(&call.args[0], context)?;
                        if let Value::Peripheral(id, _) = result.value {
                            if context.devices[&id].model == model.register_type
                                && context.devices[&id].states == BTreeSet::from([name])
                            {
                                return Ok(result);
                            }
                        }
                        return Err(Error::new(
                            call.span(),
                            "peripheral enum constructor state mismatch",
                        ));
                    }
                }
                let key = self.resolve(&path.path)?;
                let mut cost = Cost::ZERO;
                let mut args = Vec::new();
                for arg in &call.args {
                    let result = self.expr(arg, context)?;
                    cost = cost.add(result.cost)?;
                    args.push(result.value);
                }
                let mut result = self.call(&key, args, context)?;
                result.cost = result.cost.add(cost)?;
                Ok(result)
            }
            Expr::Binary(binary) => {
                let left = self.expr(&binary.left, context)?;
                let before = context.clone();
                let mut right_context = before.clone();
                let right = self.expr(&binary.right, &mut right_context)?;
                use syn::BinOp::*;
                let value = match binary.op {
                    And(_) | Or(_) => {
                        expect(&left.value, &Value::Bool, expr)?;
                        expect(&right.value, &Value::Bool, expr)?;
                        *context = join_context(&before, &right_context)?;
                        Value::Bool
                    }
                    Eq(_) | Ne(_) => {
                        if !matches!(left.value, Value::Int | Value::Bool)
                            || left.value != right.value
                        {
                            return Err(Error::new(expr.span(), "incompatible equality operands"));
                        }
                        *context = right_context;
                        Value::Bool
                    }
                    Lt(_) | Le(_) | Gt(_) | Ge(_) => {
                        expect(&left.value, &Value::Int, expr)?;
                        expect(&right.value, &Value::Int, expr)?;
                        *context = right_context;
                        Value::Bool
                    }
                    Add(_) | Sub(_) | Mul(_) | Div(_) | Rem(_) => {
                        expect(&left.value, &Value::Int, expr)?;
                        expect(&right.value, &Value::Int, expr)?;
                        *context = right_context;
                        Value::Int
                    }
                    _ => return Err(Error::new(expr.span(), "unsupported binary operation")),
                };
                Ok(Eval {
                    value,
                    cost: left.cost.add(right.cost)?,
                    returned: false,
                })
            }
            Expr::Unary(unary) => {
                let result = self.expr(&unary.expr, context)?;
                match unary.op {
                    syn::UnOp::Neg(_) => expect(&result.value, &Value::Int, expr)?,
                    syn::UnOp::Not(_) => {
                        if !matches!(result.value, Value::Bool | Value::Int) {
                            return Err(Error::new(expr.span(), "unsupported negation operand"));
                        }
                    }
                    _ => return Err(Error::new(expr.span(), "unsupported unary operation")),
                };
                Ok(result)
            }
            _ => Err(Error::new(
                expr.span(),
                "unsupported expression in cost-checked API; no bound can be reported",
            )),
        }
    }
    fn method(&mut self, call: &syn::ExprMethodCall, context: &mut Context) -> Result<Eval> {
        if call.turbofish.is_some() {
            return Err(Error::new(
                call.span(),
                "generic method calls are unsupported",
            ));
        }
        let name = call.method.to_string();
        if name == "map" {
            if let Ok(Value::Cell(id)) = lookup(&call.receiver, context) {
                if call.args.len() != 1 {
                    return Err(Error::new(
                        call.span(),
                        "AbacusCell::map expects one closure",
                    ));
                }
                let Expr::Closure(closure) = &call.args[0] else {
                    return Err(Error::new(
                        call.span(),
                        "AbacusCell::map requires an inline closure",
                    ));
                };
                if closure.inputs.len() != 1 {
                    return Err(Error::new(
                        closure.span(),
                        "map closure requires one register parameter",
                    ));
                }
                let vars = context.vars.clone();
                let before = context.clone();
                bind(
                    &closure.inputs[0],
                    Value::Peripheral(id, false),
                    &mut context.vars,
                )?;
                let mut result = self.body_expr(&closure.body, context)?;
                context.vars = vars;
                let Value::Tuple(mut values) = result.value else {
                    return Err(Error::new(
                        closure.span(),
                        "map closure must return (registers, result)",
                    ));
                };
                if values.len() != 2 || values[0] != Value::Peripheral(id, false) {
                    return Err(Error::new(
                        closure.span(),
                        "map must return ownership of the captured peripheral",
                    ));
                }
                result.value = Value::Alternatives(vec![
                    Value::Wrapped("Some".into(), Box::new(values.pop().unwrap())),
                    Value::Wrapped("None".into(), Box::new(Value::Unit)),
                ]);
                // map may return None when the cell is already borrowed. Preserve
                // the no-callback state as well as the callback's output states.
                *context = join_context(&before, context)?;
                result.returned = false;
                return Ok(result);
            }
        }
        if name == "into" && call.args.is_empty() {
            let result = self.expr(&call.receiver, context)?;
            if matches!(result.value, Value::Peripheral(_, false)) {
                return Ok(result);
            }
            return Err(Error::new(
                call.span(),
                "only Abacus typestate-to-enum conversions are modeled",
            ));
        }
        let (receiver, operation) = if let Expr::Field(field) = call.receiver.as_ref() {
            // A field can be a register field (p.data.get) or a peripheral held by self.
            if let Ok(Value::Peripheral(_, _)) = lookup(&call.receiver, context) {
                (call.receiver.as_ref(), name.clone())
            } else {
                let syn::Member::Named(field_name) = &field.member else {
                    return Err(Error::new(field.span(), "expected register name"));
                };
                (field.base.as_ref(), format!("{field_name}.{name}"))
            }
        } else {
            (call.receiver.as_ref(), name.clone())
        };
        if let Ok(Value::Peripheral(id, borrowed)) = lookup(receiver, context) {
            let device = context.devices[&id].clone();
            let model = &self.models[&device.model];
            let mut targets = BTreeSet::new();
            let mut cost = Cost::ZERO;
            let mut result = Type::Unit;
            for source in &device.states {
                let spec = model
                    .operations
                    .get(&(operation.clone(), source.clone()))
                    .ok_or_else(|| {
                        Error::new(
                            call.span(),
                            format!(
                                "Abacus operation `{operation}` is unavailable from `{source}`"
                            ),
                        )
                    })?;
                if call.args.len() != spec.arity {
                    return Err(Error::new(
                        call.span(),
                        "incorrect register operation arity",
                    ));
                }
                targets.insert(spec.target.clone());
                result = spec.result;
                cost = cost.max(spec.cost.ok_or_else(|| {
                    Error::new(
                        call.span(),
                        format!("RegAttributes for `{operation}` needs cost = ..."),
                    )
                })?)?;
            }
            if targets.iter().any(|s| model.transient.contains(s)) {
                return Err(Error::new(
                    call.span(),
                    "operation enters a transient state; hardware-concurrency analysis is required",
                ));
            }
            for arg in &call.args {
                // Bitfield constants are pure Rust paths; rustc checks their actual type.
                if matches!(arg,Expr::Path(p) if p.path.segments.len()>1) {
                    continue;
                }
                let value = self.expr(arg, context)?;
                cost = cost.add(value.cost)?;
            }
            let transition = !operation.contains('.');
            if transition {
                if borrowed {
                    return Err(Error::new(
                        call.span(),
                        "a state change needs an owned peripheral",
                    ));
                }
                let consumed = self.expr(receiver, context)?;
                if consumed.value != Value::Peripheral(id, false) {
                    return Err(Error::new(
                        call.span(),
                        "state change must consume its peripheral",
                    ));
                }
                cost = cost.add(consumed.cost)?;
            }
            context.devices.get_mut(&id).unwrap().states = targets;
            return Ok(Eval {
                value: if transition {
                    Value::Peripheral(id, false)
                } else {
                    match result {
                        Type::Unit => Value::Unit,
                        Type::Int => Value::Int,
                        Type::Bool => Value::Bool,
                    }
                },
                cost,
                returned: false,
            });
        }
        if let Ok(Value::Object(_)) = lookup(&call.receiver, context) {
            let owner = self
                .calls
                .last()
                .and_then(|k| self.functions.get(k))
                .and_then(|f| f.owner.clone())
                .ok_or_else(|| Error::new(call.span(), "unknown method receiver"))?;
            let mut args = vec![lookup(&call.receiver, context)?];
            let mut cost = Cost::ZERO;
            for arg in &call.args {
                let result = self.expr(arg, context)?;
                cost = cost.add(result.cost)?;
                args.push(result.value);
            }
            let mut result = self.call(&format!("{owner}::{name}"), args, context)?;
            result.cost = result.cost.add(cost)?;
            return Ok(result);
        }
        Err(Error::new(
            call.span(),
            "unknown method or moved peripheral; its cost cannot be inferred",
        ))
    }
    fn resolve(&self, path: &Path) -> Result<String> {
        let mut parts: Vec<_> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if parts.first().is_some_and(|s| s == "Self") {
            let owner = self
                .calls
                .last()
                .and_then(|k| self.functions.get(k))
                .and_then(|f| f.owner.as_ref())
                .ok_or_else(|| error("Self call outside an impl"))?;
            parts[0] = owner.clone();
        }
        let key = parts.join("::");
        if self.functions.contains_key(&key) {
            Ok(key)
        } else {
            Err(Error::new(path.span(),"callee body is unavailable; put caller and helper in the same #[abacus_cost(...)] inline module or impl"))
        }
    }
    fn call(&mut self, name: &str, args: Vec<Value>, context: &mut Context) -> Result<Eval> {
        if self.calls.iter().any(|n| n == name) {
            return Err(error("recursive calls have no finite checked bound"));
        }
        let function = self
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| error("unknown API helper"))?;
        if args.len() != function.sig.inputs.len() {
            return Err(error("helper call argument count mismatch"));
        }
        let saved = std::mem::take(&mut context.vars);
        for (param, value) in function.sig.inputs.iter().zip(args) {
            match param {
                FnArg::Typed(param) => {
                    self.check_type(&param.ty, &value, context)?;
                    bind(&param.pat, value, &mut context.vars)?;
                }
                FnArg::Receiver(_) => {
                    context.vars.insert("self".into(), value);
                }
            }
        }
        self.calls.push(name.into());
        let mut result = self.block(&function.body, context)?;
        self.calls.pop();
        self.check_return(&function.sig.output, &result.value, context)?;
        context.vars = saved;
        result.returned = false;
        Ok(result)
    }
}
fn owned(value: &Value) -> bool {
    match value {
        Value::Peripheral(_, false) => true,
        Value::Tuple(v) | Value::Alternatives(v) => v.iter().any(owned),
        Value::Wrapped(_, v) => owned(v),
        _ => false,
    }
}
fn lookup(expr: &Expr, context: &Context) -> Result<Value> {
    match expr {
        Expr::Path(p) if p.path.is_ident("self") => context
            .vars
            .get("self")
            .cloned()
            .ok_or_else(|| error("unknown self")),
        Expr::Path(p) => p
            .path
            .get_ident()
            .and_then(|i| context.vars.get(&i.to_string()))
            .cloned()
            .ok_or_else(|| Error::new(expr.span(), "unknown or moved variable")),
        Expr::Field(f) => {
            let base = lookup(&f.base, context)?;
            match (base,&f.member){(Value::Object(fields),syn::Member::Named(name))=>fields.get(&name.to_string()).cloned().ok_or_else(||Error::new(expr.span(),"self field type is unavailable; annotate an inline module containing the struct and impl")),_=>Err(Error::new(expr.span(),"unknown field"))}
        }
        Expr::Paren(p) => lookup(&p.expr, context),
        _ => Err(Error::new(
            expr.span(),
            "expected a named peripheral receiver; bind intermediate transitions with let",
        )),
    }
}
fn bind(pat: &Pat, value: Value, vars: &mut BTreeMap<String, Value>) -> Result<()> {
    match pat {
        Pat::Ident(p) if p.by_ref.is_none() && p.subpat.is_none() => {
            vars.insert(p.ident.to_string(), value);
            Ok(())
        }
        Pat::Wild(_) => Ok(()),
        Pat::Type(p) => bind(&p.pat, value, vars),
        Pat::Tuple(p) => {
            if let Value::Tuple(values) = value {
                if p.elems.len() == values.len() {
                    for (pat, value) in p.elems.iter().zip(values) {
                        bind(pat, value, vars)?;
                    }
                    return Ok(());
                }
            }
            Err(Error::new(pat.span(), "tuple binding mismatch"))
        }
        _ => Err(Error::new(pat.span(), "unsupported binding pattern")),
    }
}
fn names(pat: &Pat) -> Result<Vec<String>> {
    match pat {
        Pat::Ident(p) => Ok(vec![p.ident.to_string()]),
        Pat::Wild(_) => Ok(vec![]),
        Pat::Type(p) => names(&p.pat),
        Pat::Tuple(p) => {
            let mut result = vec![];
            for p in &p.elems {
                result.extend(names(p)?);
            }
            Ok(result)
        }
        _ => Err(Error::new(pat.span(), "unsupported binding")),
    }
}
fn join_value(a: &Value, b: &Value) -> Result<Value> {
    if a == b {
        return Ok(a.clone());
    }
    match (a, b) {
        (Value::Tuple(a), Value::Tuple(b)) if a.len() == b.len() => Ok(Value::Tuple(
            a.iter()
                .zip(b)
                .map(|(a, b)| join_value(a, b))
                .collect::<Result<_>>()?,
        )),
        (Value::Wrapped(a, x), Value::Wrapped(b, y)) if a == b => {
            Ok(Value::Wrapped(a.clone(), Box::new(join_value(x, y)?)))
        }
        (Value::Wrapped(..), Value::Wrapped(..)) => {
            Ok(Value::Alternatives(vec![a.clone(), b.clone()]))
        }
        _ => Err(error(
            "branches return incompatible values or different peripheral identities",
        )),
    }
}
fn join_context(a: &Context, b: &Context) -> Result<Context> {
    let mut result = a.clone();
    if a.devices.keys().ne(b.devices.keys()) {
        return Err(error("branches change peripheral identities"));
    }
    for (id, device) in &mut result.devices {
        device.states = a.devices[id]
            .states
            .union(&b.devices[id].states)
            .cloned()
            .collect();
    }
    result.vars.clear();
    for (name, value) in &a.vars {
        if let Some(other) = b.vars.get(name) {
            if let Ok(value) = join_value(value, other) {
                result.vars.insert(name.clone(), value);
            }
        }
    }
    Ok(result)
}
fn expect(value: &Value, expected: &Value, expr: &Expr) -> Result<()> {
    if value == expected {
        Ok(())
    } else {
        Err(Error::new(expr.span(), "incompatible expression type"))
    }
}
fn integer_type(name: &str) -> bool {
    return matches!(
        name,
        "u8" | "u16"
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
    );
}
fn generic_type<'a>(ty: &'a RustType, name: &str, index: usize) -> Option<&'a RustType> {
    let RustType::Path(p) = ty else {
        return None;
    };
    let segment = p.path.segments.last()?;
    if segment.ident != name {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    match args.args.iter().nth(index)? {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    }
}
fn type_key(ty: &RustType) -> Result<String> {
    let RustType::Path(p) = ty else {
        return Err(Error::new(ty.span(), "unsupported type"));
    };
    let segment = p.path.segments.last().ok_or_else(|| error("empty type"))?;
    Ok(segment.to_token_stream().to_string().replace(' ', ""))
}
