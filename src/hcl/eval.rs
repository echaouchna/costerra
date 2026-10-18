//! Terraform expression evaluator with unknown-value propagation.

use super::functions;
use super::value::{Map, Value};
use hcl::expr::{BinaryOperator, Expression, ForExpr, ObjectKey, Operation, TemplateExpr, TraversalOperator, UnaryOperator};
use hcl::template::{Directive, Element, Template};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct EvalError(pub String);

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type EvalResult = Result<Value, EvalError>;

pub fn err<T>(msg: impl Into<String>) -> Result<T, EvalError> {
    Err(EvalError(msg.into()))
}

/// Extension point for additional functions (e.g. Terragrunt built-ins).
pub trait FuncExt: Sync {
    fn call(&self, name: &str, args: &[Value], ev: &Evaluator) -> Option<EvalResult>;
}

/// Lexically scoped bindings introduced by `for` expressions and template
/// `for` directives.
pub struct Scope<'s> {
    name: &'s str,
    value: Value,
    parent: Option<&'s Scope<'s>>,
}

impl<'s> Scope<'s> {
    pub fn new(name: &'s str, value: Value, parent: Option<&'s Scope<'s>>) -> Self {
        Scope { name, value, parent }
    }

    fn lookup(&self, name: &str) -> Option<&Value> {
        let mut cur = Some(self);
        while let Some(s) = cur {
            if s.name == name {
                return Some(&s.value);
            }
            cur = s.parent;
        }
        None
    }
}

pub struct Evaluator<'a> {
    pub root: &'a Map,
    pub base_dir: &'a Path,
    pub ext: Option<&'a dyn FuncExt>,
}

impl<'a> Evaluator<'a> {
    pub fn new(root: &'a Map, base_dir: &'a Path) -> Self {
        Evaluator { root, base_dir, ext: None }
    }

    pub fn with_ext(mut self, ext: &'a dyn FuncExt) -> Self {
        self.ext = Some(ext);
        self
    }

    /// Evaluate an expression. Any evaluation error degrades to `Unknown`.
    pub fn eval_lenient(&self, expr: &Expression) -> Value {
        match self.eval(expr) {
            Ok(v) => v,
            Err(e) => {
                tracing::trace!("eval error: {e}");
                Value::Unknown
            }
        }
    }

    pub fn eval(&self, expr: &Expression) -> EvalResult {
        self.eval_in(expr, None)
    }

    pub fn eval_in(&self, expr: &Expression, scope: Option<&Scope>) -> EvalResult {
        match expr {
            Expression::Null => Ok(Value::Null),
            Expression::Bool(b) => Ok(Value::Bool(*b)),
            Expression::Number(n) => Ok(Value::Number(n.as_f64().unwrap_or(0.0))),
            Expression::String(s) => Ok(Value::String(s.clone())),
            Expression::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for i in items {
                    out.push(self.eval_in(i, scope)?);
                }
                Ok(Value::List(out))
            }
            Expression::Object(obj) => {
                let mut out = Map::with_capacity(obj.len());
                for (k, v) in obj.iter() {
                    let key = match k {
                        ObjectKey::Identifier(id) => id.to_string(),
                        ObjectKey::Expression(e) => match self.eval_in(e, scope)? {
                            Value::Unknown => return Ok(Value::Unknown),
                            // A bare identifier-like traversal as object key is
                            // treated literally by Terraform when unparenthesized.
                            other => other.to_interp_string().ok_or_else(|| EvalError("invalid object key".into()))?,
                        },
                        _ => return err("unsupported object key"),
                    };
                    out.insert(key, self.eval_in(v, scope)?);
                }
                Ok(Value::Map(out))
            }
            Expression::TemplateExpr(t) => self.eval_template_expr(t, scope),
            Expression::Variable(v) => self.lookup_var(v.as_str(), scope),
            Expression::Traversal(t) => {
                // `var.x`, `local.y[k]`…: walk the path by reference, clone only the result.
                if let Expression::Variable(v) = &t.expr {
                    return self.traverse_var(v.as_str(), &t.operators, scope);
                }
                let base = self.eval_in(&t.expr, scope)?;
                self.apply_traversal(base, &t.operators, scope)
            }
            Expression::FuncCall(fc) => {
                let name = if fc.name.namespace.is_empty() {
                    fc.name.name.to_string()
                } else {
                    // provider-defined functions: provider::google::... -> unknown
                    return Ok(Value::Unknown);
                };
                match name.as_str() {
                    "try" => return self.eval_try(&fc.args, scope),
                    "can" => {
                        let r = fc.args.first().map(|a| self.eval_in(a, scope));
                        return Ok(match r {
                            Some(Ok(Value::Unknown)) => Value::Unknown,
                            Some(Ok(_)) => Value::Bool(true),
                            _ => Value::Bool(false),
                        });
                    }
                    _ => {}
                }
                let mut args = Vec::with_capacity(fc.args.len());
                for a in &fc.args {
                    args.push(self.eval_in(a, scope)?);
                }
                if fc.expand_final {
                    match args.pop() {
                        Some(Value::List(l)) => args.extend(l),
                        Some(Value::Unknown) => return Ok(Value::Unknown),
                        Some(other) => args.push(other),
                        None => {}
                    }
                }
                if let Some(ext) = self.ext
                    && let Some(r) = ext.call(&name, &args, self)
                {
                    return r;
                }
                functions::call(&name, &args, self)
            }
            Expression::Parenthesis(e) => self.eval_in(e, scope),
            Expression::Conditional(c) => {
                let cond = self.eval_in(&c.cond_expr, scope)?;
                match cond.as_bool() {
                    Some(true) => self.eval_in(&c.true_expr, scope),
                    Some(false) => self.eval_in(&c.false_expr, scope),
                    None => {
                        if cond.is_unknown() {
                            let t = self.eval_in(&c.true_expr, scope).unwrap_or(Value::Unknown);
                            let f = self.eval_in(&c.false_expr, scope).unwrap_or(Value::Unknown);
                            if t == f { Ok(t) } else { Ok(Value::Unknown) }
                        } else {
                            err(format!("condition is not a bool: {}", cond.type_name()))
                        }
                    }
                }
            }
            Expression::Operation(op) => self.eval_operation(op, scope),
            Expression::ForExpr(f) => self.eval_for(f, scope),
            _ => err("unsupported expression"),
        }
    }

    fn lookup_var(&self, name: &str, scope: Option<&Scope>) -> EvalResult {
        if let Some(s) = scope
            && let Some(v) = s.lookup(name)
        {
            return Ok(v.clone());
        }
        match self.root.get(name) {
            Some(v) => Ok(v.clone()),
            None => Ok(Value::Unknown),
        }
    }

    /// A traversal from a variable, without cloning the containers it goes
    /// through (`local.x` would otherwise copy every local). Same results as
    /// `lookup_var` + `apply_traversal`.
    fn traverse_var(&self, name: &str, ops: &[TraversalOperator], scope: Option<&Scope>) -> EvalResult {
        let Some(mut cur) = scope.and_then(|s| s.lookup(name)).or_else(|| self.root.get(name)) else {
            return Ok(Value::Unknown);
        };
        for (i, op) in ops.iter().enumerate() {
            let next = match op {
                TraversalOperator::GetAttr(id) => match cur {
                    Value::Map(m) => m.get(id.as_str()),
                    _ => return get_attr(cur, id.as_str()).and_then(|v| self.apply_traversal(v, &ops[i + 1..], scope)),
                },
                TraversalOperator::Index(e) => {
                    let idx = self.eval_in(e, scope)?;
                    match index_ref(cur, &idx)? {
                        Some(v) => Some(v),
                        None => return Ok(Value::Unknown),
                    }
                }
                TraversalOperator::LegacyIndex(n) => match index_ref(cur, &Value::Number(*n as f64))? {
                    Some(v) => Some(v),
                    None => return Ok(Value::Unknown),
                },
                TraversalOperator::AttrSplat | TraversalOperator::FullSplat => return self.apply_traversal(cur.clone(), &ops[i..], scope),
            };
            match next {
                Some(v) => cur = v,
                // A missing attribute is unknown (as in `get_attr`).
                None => return Ok(Value::Unknown),
            }
        }
        Ok(cur.clone())
    }

    fn eval_try(&self, args: &[Expression], scope: Option<&Scope>) -> EvalResult {
        let mut saw_unknown = false;
        for a in args {
            match self.eval_in(a, scope) {
                Ok(Value::Unknown) => saw_unknown = true,
                Ok(v) => return Ok(v),
                Err(_) => {}
            }
        }
        if saw_unknown { Ok(Value::Unknown) } else { err("no expression in try() succeeded") }
    }

    fn apply_traversal(&self, mut cur: Value, ops: &[TraversalOperator], scope: Option<&Scope>) -> EvalResult {
        for (i, op) in ops.iter().enumerate() {
            match op {
                TraversalOperator::GetAttr(id) => cur = get_attr(&cur, id.as_str())?,
                TraversalOperator::Index(e) => {
                    let idx = self.eval_in(e, scope)?;
                    cur = index(&cur, &idx)?;
                }
                TraversalOperator::LegacyIndex(n) => cur = index(&cur, &Value::Number(*n as f64))?,
                TraversalOperator::AttrSplat | TraversalOperator::FullSplat => {
                    let rest = &ops[i + 1..];
                    let (apply, after): (&[TraversalOperator], &[TraversalOperator]) = if matches!(op, TraversalOperator::AttrSplat) {
                        let n = rest.iter().take_while(|o| matches!(o, TraversalOperator::GetAttr(_))).count();
                        (&rest[..n], &rest[n..])
                    } else {
                        (rest, &[])
                    };
                    let items = match cur {
                        Value::List(l) => l,
                        Value::Null => vec![],
                        Value::Unknown => return Ok(Value::Unknown),
                        other => vec![other],
                    };
                    let mut out = Vec::with_capacity(items.len());
                    for it in items {
                        out.push(self.apply_traversal(it, apply, scope)?);
                    }
                    return self.apply_traversal(Value::List(out), after, scope);
                }
            }
        }
        Ok(cur)
    }

    fn eval_operation(&self, op: &Operation, scope: Option<&Scope>) -> EvalResult {
        match op {
            Operation::Unary(u) => {
                let v = self.eval_in(&u.expr, scope)?;
                if v.is_unknown() {
                    return Ok(Value::Unknown);
                }
                match u.operator {
                    UnaryOperator::Neg => v.as_f64().map(|n| Value::Number(-n)).ok_or_else(|| EvalError("negation of non-number".into())),
                    UnaryOperator::Not => v.as_bool().map(|b| Value::Bool(!b)).ok_or_else(|| EvalError("not of non-bool".into())),
                }
            }
            Operation::Binary(b) => {
                use BinaryOperator::*;
                let l = self.eval_in(&b.lhs_expr, scope)?;
                // short-circuit logic that is still valid with unknowns
                if matches!(b.operator, And | Or) {
                    let r = self.eval_in(&b.rhs_expr, scope)?;
                    let (lb, rb) = (l.as_bool(), r.as_bool());
                    return Ok(match (b.operator, lb, rb) {
                        (And, Some(false), _) | (And, _, Some(false)) => Value::Bool(false),
                        (Or, Some(true), _) | (Or, _, Some(true)) => Value::Bool(true),
                        (And, Some(x), Some(y)) => Value::Bool(x && y),
                        (Or, Some(x), Some(y)) => Value::Bool(x || y),
                        _ if l.is_unknown() || r.is_unknown() => Value::Unknown,
                        _ => return err("logical operator on non-bool"),
                    });
                }
                let r = self.eval_in(&b.rhs_expr, scope)?;
                if l.is_unknown() || r.is_unknown() {
                    return Ok(Value::Unknown);
                }
                match b.operator {
                    Eq => Ok(Value::Bool(values_equal(&l, &r))),
                    NotEq => Ok(Value::Bool(!values_equal(&l, &r))),
                    _ => {
                        let (x, y) = match (l.as_f64(), r.as_f64()) {
                            (Some(x), Some(y)) => (x, y),
                            _ => return err("arithmetic on non-number"),
                        };
                        Ok(match b.operator {
                            Plus => Value::Number(x + y),
                            Minus => Value::Number(x - y),
                            Mul => Value::Number(x * y),
                            Div => Value::Number(x / y),
                            Mod => Value::Number(x % y),
                            Less => Value::Bool(x < y),
                            LessEq => Value::Bool(x <= y),
                            Greater => Value::Bool(x > y),
                            GreaterEq => Value::Bool(x >= y),
                            _ => unreachable!(),
                        })
                    }
                }
            }
        }
    }

    fn eval_for(&self, f: &ForExpr, scope: Option<&Scope>) -> EvalResult {
        let coll = self.eval_in(&f.collection_expr, scope)?;
        let items: Vec<(Value, Value)> = match coll {
            Value::List(l) => l.into_iter().enumerate().map(|(i, v)| (Value::Number(i as f64), v)).collect(),
            Value::Map(m) => m.into_iter().map(|(k, v)| (Value::String(k), v)).collect(),
            Value::Unknown => return Ok(Value::Unknown),
            Value::Null => return err("for over null"),
            _ => return err("for over non-collection"),
        };
        let key_name = f.key_var.as_ref().map(|k| k.as_str());
        let val_name = f.value_var.as_str();

        let mut list_out = Vec::new();
        let mut map_out = Map::new();
        let mut group_out: indexmap::IndexMap<String, Vec<Value>> = indexmap::IndexMap::new();
        for (k, v) in items {
            let s1;
            let s2;
            let inner: &Scope = if let Some(kn) = key_name {
                s1 = Scope { name: kn, value: k, parent: scope };
                s2 = Scope { name: val_name, value: v, parent: Some(&s1) };
                &s2
            } else {
                s1 = Scope { name: val_name, value: v, parent: scope };
                &s1
            };
            if let Some(c) = &f.cond_expr {
                match self.eval_in(c, Some(inner)).unwrap_or(Value::Unknown) {
                    Value::Bool(true) => {}
                    Value::Bool(false) => continue,
                    // A filter that depends on values only known after apply
                    // (remote state, data sources): keep the item, so that the
                    // collection stays known and resources are estimated.
                    Value::Unknown => {}
                    other => match other.as_bool() {
                        Some(true) => {}
                        Some(false) => continue,
                        None => return err("for condition not bool"),
                    },
                }
            }
            let val = self.eval_in(&f.value_expr, Some(inner))?;
            if let Some(ke) = &f.key_expr {
                let key = self.eval_in(ke, Some(inner))?;
                let key = match key.to_interp_string() {
                    Some(k) if !key.is_unknown() => k,
                    _ => return Ok(Value::Unknown),
                };
                if f.grouping {
                    group_out.entry(key).or_default().push(val);
                } else {
                    map_out.insert(key, val);
                }
            } else {
                list_out.push(val);
            }
        }
        if f.key_expr.is_some() {
            if f.grouping {
                Ok(Value::Map(group_out.into_iter().map(|(k, v)| (k, Value::List(v))).collect()))
            } else {
                Ok(Value::Map(map_out))
            }
        } else {
            Ok(Value::List(list_out))
        }
    }

    fn eval_template_expr(&self, t: &TemplateExpr, scope: Option<&Scope>) -> EvalResult {
        if let TemplateExpr::QuotedString(s) = t
            && !s.contains("${")
            && !s.contains("%{")
        {
            return Ok(Value::String(s.clone()));
        }
        let tpl = Template::from_expr(t).map_err(|e| EvalError(e.to_string()))?;
        self.eval_template(&tpl, scope)
    }

    pub fn eval_template(&self, tpl: &Template, scope: Option<&Scope>) -> EvalResult {
        let els = tpl.elements();
        // A template consisting of a single interpolation yields the raw value.
        if els.len() == 1
            && let Element::Interpolation(i) = &els[0]
        {
            return self.eval_in(&i.expr, scope);
        }
        let mut out = String::new();
        for el in els {
            match el {
                Element::Literal(s) => out.push_str(s),
                Element::Interpolation(i) => {
                    let v = self.eval_in(&i.expr, scope)?;
                    if v.is_unknown() {
                        return Ok(Value::Unknown);
                    }
                    match v.to_interp_string() {
                        Some(s) => out.push_str(&s),
                        None => return err("cannot interpolate collection"),
                    }
                }
                Element::Directive(d) => match d.as_ref() {
                    Directive::If(ifd) => {
                        let c = self.eval_in(&ifd.cond_expr, scope)?;
                        let branch = match c.as_bool() {
                            Some(true) => Some(&ifd.true_template),
                            Some(false) => ifd.false_template.as_ref(),
                            None => return Ok(Value::Unknown),
                        };
                        if let Some(b) = branch {
                            match self.eval_template(b, scope)? {
                                Value::Unknown => return Ok(Value::Unknown),
                                v => out.push_str(&v.to_interp_string().unwrap_or_default()),
                            }
                        }
                    }
                    Directive::For(fd) => {
                        let coll = self.eval_in(&fd.collection_expr, scope)?;
                        let items: Vec<(Value, Value)> = match coll {
                            Value::List(l) => l.into_iter().enumerate().map(|(i, v)| (Value::Number(i as f64), v)).collect(),
                            Value::Map(m) => m.into_iter().map(|(k, v)| (Value::String(k), v)).collect(),
                            _ => return Ok(Value::Unknown),
                        };
                        for (k, v) in items {
                            let s1;
                            let s2;
                            let inner: &Scope = if let Some(kn) = &fd.key_var {
                                s1 = Scope { name: kn.as_str(), value: k, parent: scope };
                                s2 = Scope { name: fd.value_var.as_str(), value: v, parent: Some(&s1) };
                                &s2
                            } else {
                                s1 = Scope { name: fd.value_var.as_str(), value: v, parent: scope };
                                &s1
                            };
                            match self.eval_template(&fd.template, Some(inner))? {
                                Value::Unknown => return Ok(Value::Unknown),
                                v => out.push_str(&v.to_interp_string().unwrap_or_default()),
                            }
                        }
                    }
                },
            }
        }
        Ok(Value::String(out))
    }

    /// Evaluate a template string (e.g. for templatefile()) with extra vars.
    pub fn eval_template_str(&self, src: &str, vars: &Map) -> EvalResult {
        let tpl: Template = src.parse().map_err(|e: hcl::Error| EvalError(e.to_string()))?;
        let ev = Evaluator { root: vars, base_dir: self.base_dir, ext: self.ext };
        match ev.eval_template(&tpl, None)? {
            Value::String(s) => Ok(Value::String(s)),
            Value::Unknown => Ok(Value::Unknown),
            other => Ok(Value::String(other.to_interp_string().unwrap_or_default())),
        }
    }
}

fn get_attr(v: &Value, name: &str) -> EvalResult {
    match v {
        Value::Map(m) => Ok(m.get(name).cloned().unwrap_or(Value::Unknown)),
        Value::Unknown => Ok(Value::Unknown),
        Value::Null => err(format!("attempt to get attribute '{name}' from null")),
        // Splatting over lists of objects without [*] (legacy): unknown
        _ => err(format!("cannot access attribute '{name}' on {}", v.type_name())),
    }
}

/// `index` by reference: `Ok(None)` for an unknown index or value.
fn index_ref<'v>(v: &'v Value, idx: &Value) -> Result<Option<&'v Value>, EvalError> {
    if idx.is_unknown() {
        return Ok(None);
    }
    match v {
        Value::List(l) => {
            let i = idx.as_f64().ok_or_else(|| EvalError("list index must be number".into()))?;
            l.get(i as usize).map(Some).ok_or_else(|| EvalError(format!("index {i} out of range")))
        }
        Value::Map(m) => {
            let k = idx.to_interp_string().ok_or_else(|| EvalError("invalid map key".into()))?;
            m.get(&k).map(Some).ok_or_else(|| EvalError(format!("key '{k}' not found")))
        }
        Value::Unknown => Ok(None),
        _ => err(format!("cannot index {}", v.type_name())),
    }
}

fn index(v: &Value, idx: &Value) -> EvalResult {
    if idx.is_unknown() {
        return Ok(Value::Unknown);
    }
    match v {
        Value::List(l) => {
            let i = idx.as_f64().ok_or_else(|| EvalError("list index must be number".into()))?;
            l.get(i as usize).cloned().ok_or_else(|| EvalError(format!("index {i} out of range")))
        }
        Value::Map(m) => {
            let k = idx.to_interp_string().ok_or_else(|| EvalError("invalid map key".into()))?;
            m.get(&k).cloned().ok_or_else(|| EvalError(format!("key '{k}' not found")))
        }
        Value::Unknown => Ok(Value::Unknown),
        _ => err(format!("cannot index {}", v.type_name())),
    }
}

pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => (x - y).abs() < f64::EPSILON,
        _ => a == b,
    }
}

/// Collect `root.attr` reference pairs used by an expression, e.g.
/// `var.x` -> ("var","x"), `google_compute_network.vpc.id` -> ("google_compute_network","vpc").
pub fn collect_refs(expr: &Expression, out: &mut Vec<(String, Option<String>)>) {
    match expr {
        Expression::Variable(v) => out.push((v.to_string(), None)),
        Expression::Traversal(t) => {
            if let Expression::Variable(v) = &t.expr {
                let attr = t.operators.first().and_then(|o| match o {
                    TraversalOperator::GetAttr(id) => Some(id.to_string()),
                    _ => None,
                });
                // data.TYPE.NAME -> ("data.TYPE", NAME)
                if v.as_str() == "data" {
                    if let (Some(ty), Some(TraversalOperator::GetAttr(n))) = (attr.clone(), t.operators.get(1)) {
                        out.push((format!("data.{ty}"), Some(n.to_string())));
                    }
                } else {
                    out.push((v.to_string(), attr));
                }
            } else {
                collect_refs(&t.expr, out);
            }
            for op in &t.operators {
                if let TraversalOperator::Index(e) = op {
                    collect_refs(e, out);
                }
            }
        }
        Expression::Array(a) => a.iter().for_each(|e| collect_refs(e, out)),
        Expression::Object(o) => o.iter().for_each(|(k, v)| {
            if let ObjectKey::Expression(e) = k {
                collect_refs(e, out);
            }
            collect_refs(v, out);
        }),
        Expression::TemplateExpr(t) => {
            if let Ok(tpl) = Template::from_expr(t) {
                collect_template_refs(&tpl, out);
            }
        }
        Expression::FuncCall(f) => f.args.iter().for_each(|e| collect_refs(e, out)),
        Expression::Parenthesis(e) => collect_refs(e, out),
        Expression::Conditional(c) => {
            collect_refs(&c.cond_expr, out);
            collect_refs(&c.true_expr, out);
            collect_refs(&c.false_expr, out);
        }
        Expression::Operation(op) => match op.as_ref() {
            Operation::Unary(u) => collect_refs(&u.expr, out),
            Operation::Binary(b) => {
                collect_refs(&b.lhs_expr, out);
                collect_refs(&b.rhs_expr, out);
            }
        },
        Expression::ForExpr(f) => {
            collect_refs(&f.collection_expr, out);
            if let Some(k) = &f.key_expr {
                collect_refs(k, out);
            }
            collect_refs(&f.value_expr, out);
            if let Some(c) = &f.cond_expr {
                collect_refs(c, out);
            }
        }
        _ => {}
    }
}

fn collect_template_refs(tpl: &Template, out: &mut Vec<(String, Option<String>)>) {
    for el in tpl.elements() {
        match el {
            Element::Interpolation(i) => collect_refs(&i.expr, out),
            Element::Directive(d) => match d.as_ref() {
                Directive::If(i) => {
                    collect_refs(&i.cond_expr, out);
                    collect_template_refs(&i.true_template, out);
                    if let Some(f) = &i.false_template {
                        collect_template_refs(f, out);
                    }
                }
                Directive::For(f) => {
                    collect_refs(&f.collection_expr, out);
                    collect_template_refs(&f.template, out);
                }
            },
            _ => {}
        }
    }
}
