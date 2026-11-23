//! Parsed parameter values (what Click passes to a command callback).

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Tuples, `multiple=True` values and `nargs=-1` values.
    List(Vec<Value>),
}

impl Value {
    pub fn is_none(&self) -> bool {
        matches!(self, Value::None)
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Str(s) => !s.is_empty(),
            Value::List(l) => !l.is_empty(),
            _ => false,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_list(&self) -> &[Value] {
        match self {
            Value::List(l) => l,
            _ => &[],
        }
    }
    pub fn strings(&self) -> Vec<String> {
        match self {
            Value::List(l) => l
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Value::Str(s) => vec![s.clone()],
            _ => Vec::new(),
        }
    }

    /// Parse the JSON default recorded by the generator.
    pub fn from_json(v: &serde_json::Value) -> Value {
        match v {
            serde_json::Value::Null => Value::None,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => n
                .as_i64()
                .map(Value::Int)
                .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
            serde_json::Value::String(s) => Value::Str(s.clone()),
            serde_json::Value::Array(a) => Value::List(a.iter().map(Value::from_json).collect()),
            serde_json::Value::Object(_) => Value::None,
        }
    }
}
