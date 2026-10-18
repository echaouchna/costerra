//! Terraform variable type constraints, used to coerce inputs and apply
//! `optional(type, default)` attribute defaults.

use super::value::{Map, Value};
use hcl::expr::{Expression, ObjectKey};

#[derive(Debug, Clone)]
pub enum TypeSpec {
    Any,
    String,
    Number,
    Bool,
    List(Box<TypeSpec>),
    Map(Box<TypeSpec>),
    Tuple(Vec<TypeSpec>),
    Object(Vec<ObjAttr>),
}

#[derive(Debug, Clone)]
pub struct ObjAttr {
    pub name: String,
    pub ty: TypeSpec,
    pub optional: bool,
    pub default: Option<Value>,
}

impl TypeSpec {
    pub fn parse(expr: &Expression) -> TypeSpec {
        match expr {
            Expression::Variable(v) => match v.as_str() {
                "string" => TypeSpec::String,
                "number" => TypeSpec::Number,
                "bool" => TypeSpec::Bool,
                _ => TypeSpec::Any,
            },
            Expression::FuncCall(fc) => {
                let a0 = fc.args.first();
                match fc.name.name.as_str() {
                    "list" | "set" => TypeSpec::List(Box::new(a0.map(Self::parse).unwrap_or(TypeSpec::Any))),
                    "map" => TypeSpec::Map(Box::new(a0.map(Self::parse).unwrap_or(TypeSpec::Any))),
                    "tuple" => match a0 {
                        Some(Expression::Array(items)) => TypeSpec::Tuple(items.iter().map(Self::parse).collect()),
                        _ => TypeSpec::Any,
                    },
                    "object" => match a0 {
                        Some(Expression::Object(obj)) => TypeSpec::Object(
                            obj.iter()
                                .filter_map(|(k, v)| {
                                    let name = match k {
                                        ObjectKey::Identifier(i) => i.to_string(),
                                        ObjectKey::Expression(Expression::String(s)) => s.clone(),
                                        ObjectKey::Expression(Expression::Variable(v)) => v.to_string(),
                                        _ => return None,
                                    };
                                    Some(parse_attr(name, v))
                                })
                                .collect(),
                        ),
                        _ => TypeSpec::Any,
                    },
                    _ => TypeSpec::Any,
                }
            }
            _ => TypeSpec::Any,
        }
    }

    /// Coerce a value to this type, applying optional attribute defaults.
    pub fn convert(&self, v: Value) -> Value {
        match (self, v) {
            (_, Value::Unknown) => Value::Unknown,
            (_, Value::Null) => Value::Null,
            (TypeSpec::String, v @ Value::String(_)) => v,
            (TypeSpec::String, v @ (Value::Number(_) | Value::Bool(_))) => Value::String(v.to_interp_string().unwrap_or_default()),
            (TypeSpec::Number, v) => v.as_f64().map(Value::Number).unwrap_or(v),
            (TypeSpec::Bool, v) => v.as_bool().map(Value::Bool).unwrap_or(v),
            (TypeSpec::List(t), Value::List(l)) => Value::List(l.into_iter().map(|x| t.convert(x)).collect()),
            (TypeSpec::Map(t), Value::Map(m)) => Value::Map(m.into_iter().map(|(k, x)| (k, t.convert(x))).collect()),
            (TypeSpec::Tuple(ts), Value::List(l)) => {
                Value::List(l.into_iter().enumerate().map(|(i, x)| ts.get(i).map(|t| t.convert(x.clone())).unwrap_or(x)).collect())
            }
            (TypeSpec::Object(attrs), Value::Map(mut m)) => {
                let mut out = Map::with_capacity(attrs.len().max(m.len()));
                for a in attrs {
                    let v = match m.shift_remove(&a.name) {
                        Some(Value::Null) | None => match &a.default {
                            Some(d) => a.ty.convert(d.clone()),
                            None => Value::Null,
                        },
                        Some(v) => a.ty.convert(v),
                    };
                    out.insert(a.name.clone(), v);
                }
                Value::Map(out)
            }
            (_, v) => v,
        }
    }
}

fn parse_attr(name: String, expr: &Expression) -> ObjAttr {
    if let Expression::FuncCall(fc) = expr
        && fc.name.name.as_str() == "optional"
    {
        let ty = fc.args.first().map(TypeSpec::parse).unwrap_or(TypeSpec::Any);
        let default = fc.args.get(1).map(|d| {
            let empty = Map::new();
            let ev = super::eval::Evaluator::new(&empty, std::path::Path::new("."));
            ev.eval_lenient(d)
        });
        return ObjAttr { name, ty, optional: true, default };
    }
    ObjAttr { name, ty: TypeSpec::parse(expr), optional: false, default: None }
}
