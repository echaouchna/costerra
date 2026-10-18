use indexmap::IndexMap;
use std::fmt;

/// A Terraform/HCL runtime value.
///
/// `Unknown` models values that cannot be determined statically (computed
/// attributes, remote state, data sources, ...). It propagates through
/// operations so that cost models can decide how to handle missing data.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Unknown,
    Bool(bool),
    Number(f64),
    String(String),
    List(Vec<Value>),
    Map(IndexMap<String, Value>),
}

pub type Map = IndexMap<String, Value>;

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::String(s.into())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Value::Unknown)
    }

    /// True if the value and all nested values are known.
    pub fn is_wholly_known(&self) -> bool {
        match self {
            Value::Unknown => false,
            Value::List(l) => l.iter().all(Value::is_wholly_known),
            Value::Map(m) => m.values().all(Value::is_wholly_known),
            _ => true,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Unknown => "unknown",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::List(_) => "list",
            Value::Map(_) => "map",
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// Lenient numeric conversion (numbers, numeric strings, bools).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            Value::String(s) => s.trim().parse().ok(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    /// Lenient boolean conversion.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::String(s) => match s.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&Vec<Value>> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Map> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(m) => m.get(key),
            _ => None,
        }
    }

    /// Follow a dotted path through maps; numeric segments index lists.
    pub fn path(&self, path: &str) -> Option<&Value> {
        let mut cur = self;
        for seg in path.split('.') {
            cur = match cur {
                Value::Map(m) => m.get(seg)?,
                Value::List(l) => l.get(seg.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    /// True if any string inside this value equals `needle` (no allocation).
    pub fn contains_string(&self, needle: &str) -> bool {
        match self {
            Value::String(s) => s == needle,
            Value::List(l) => l.iter().any(|v| v.contains_string(needle)),
            Value::Map(m) => m.values().any(|v| v.contains_string(needle)),
            _ => false,
        }
    }

    /// Convert to the string form Terraform uses for interpolation.
    pub fn to_interp_string(&self) -> Option<String> {
        match self {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(fmt_number(*n)),
            Value::Bool(b) => Some(b.to_string()),
            Value::Null => Some(String::new()),
            _ => None,
        }
    }

    pub fn len(&self) -> Option<usize> {
        match self {
            Value::List(l) => Some(l.len()),
            Value::Map(m) => Some(m.len()),
            Value::String(s) => Some(s.chars().count()),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }

    pub fn truthy(&self) -> Option<bool> {
        self.as_bool()
    }

    pub fn from_json(v: &serde_json::Value) -> Value {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(s) => Value::String(s.clone()),
            serde_json::Value::Array(a) => Value::List(a.iter().map(Value::from_json).collect()),
            serde_json::Value::Object(o) => Value::Map(o.iter().map(|(k, v)| (k.clone(), Value::from_json(v))).collect()),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null | Value::Unknown => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Number(n) => {
                if n.fract() == 0.0 && n.abs() < 9.0e15 {
                    serde_json::Value::from(*n as i64)
                } else {
                    serde_json::Number::from_f64(*n).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null)
                }
            }
            Value::String(s) => serde_json::Value::String(s.clone()),
            Value::List(l) => serde_json::Value::Array(l.iter().map(Value::to_json).collect()),
            Value::Map(m) => serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
        }
    }

    /// Parse a YAML document (YAML 1.2, pure Rust parser) into a value.
    pub fn parse_yaml(src: &str) -> Result<Value, String> {
        if src.trim().is_empty() {
            return Ok(Value::Null);
        }
        crate::util::from_yaml::<serde_json::Value>(src).map(|j| Value::from_json(&j)).map_err(|e| e.to_string())
    }
}

pub fn fmt_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n}");
        s
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Unknown => write!(f, "(known after apply)"),
            Value::String(s) => write!(f, "{s}"),
            Value::Number(n) => write!(f, "{}", fmt_number(*n)),
            other => write!(f, "{}", other.to_json()),
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}
impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Number(n)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
