//! Python-like values for project definitions (`dict`/`list`/`str`/… after YAML
//! loading, pydantic validation and template rendering).

use indexmap::IndexMap;

#[derive(Clone, Debug, PartialEq)]
pub enum PyV {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<PyV>),
    Dict(IndexMap<String, PyV>),
    /// `!override` (`YamlOverride(data=…)`), consumed by mixin merging.
    Override(Box<PyV>),
    /// A validated pydantic model: its name and fields (Python names, model order).
    Model(Box<Model>),
    /// A YAML timestamp loaded by `SafeLoader` (`datetime.date`/`datetime`), kept as
    /// the text `safe_dump` writes back (ISO, `T` replaced by a space).
    Timestamp(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub name: String,
    pub fields: IndexMap<String, PyV>,
    /// `model_fields_set`: fields given in the input (not defaults).
    pub set: Vec<String>,
}

impl PyV {
    pub fn s(v: impl Into<String>) -> PyV {
        PyV::Str(v.into())
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            PyV::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_dict(&self) -> Option<&IndexMap<String, PyV>> {
        match self {
            PyV::Dict(d) => Some(d),
            _ => None,
        }
    }
    pub(crate) fn as_dict_mut(&mut self) -> Option<&mut IndexMap<String, PyV>> {
        match self {
            PyV::Dict(d) => Some(d),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&Vec<PyV>> {
        match self {
            PyV::List(l) => Some(l),
            _ => None,
        }
    }
    pub fn as_model(&self) -> Option<&Model> {
        match self {
            PyV::Model(m) => Some(m),
            _ => None,
        }
    }
    pub fn is_none(&self) -> bool {
        matches!(self, PyV::None)
    }
    /// A model field (Python name), or a dict key.
    pub fn get(&self, key: &str) -> Option<&PyV> {
        match self {
            PyV::Model(m) => m.fields.get(key),
            PyV::Dict(d) => d.get(key),
            _ => None,
        }
    }
    /// `get` that treats `None` as absent.
    pub fn get_some(&self, key: &str) -> Option<&PyV> {
        self.get(key).filter(|v| !v.is_none())
    }
    /// The `str()` of each item of the list at `key` (empty when it is not a list).
    pub fn str_list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .and_then(PyV::as_list)
            .map(|l| l.iter().map(PyV::py_str).collect())
            .unwrap_or_default()
    }
    pub fn str_of(&self, key: &str) -> Option<String> {
        match self.get(key)? {
            PyV::None => None,
            PyV::Str(s) => Some(s.clone()),
            other => Some(other.py_str()),
        }
    }
    /// `type(x).__name__`.
    pub fn type_name(&self) -> &str {
        match self {
            PyV::None => "NoneType",
            PyV::Bool(_) => "bool",
            PyV::Int(_) => "int",
            PyV::Float(_) => "float",
            PyV::Str(_) => "str",
            PyV::List(_) => "list",
            PyV::Dict(_) => "dict",
            PyV::Override(_) => "YamlOverride",
            PyV::Timestamp(t) => {
                if t.len() == 10 {
                    "date"
                } else {
                    "datetime"
                }
            }
            PyV::Model(m) => &m.name,
        }
    }
    /// Python `str(x)`.
    pub fn py_str(&self) -> String {
        match self {
            PyV::Str(s) | PyV::Timestamp(s) => s.clone(),
            other => other.py_repr(),
        }
    }
    /// Python `repr(x)`.
    pub fn py_repr(&self) -> String {
        match self {
            PyV::None => "None".into(),
            PyV::Bool(b) => if *b { "True" } else { "False" }.into(),
            PyV::Int(i) => i.to_string(),
            PyV::Float(f) => crate::pyfmt::float_repr(*f),
            PyV::Str(s) => snow_click::py_repr(s),
            PyV::List(l) => format!(
                "[{}]",
                l.iter().map(PyV::py_repr).collect::<Vec<_>>().join(", ")
            ),
            PyV::Dict(d) => format!(
                "{{{}}}",
                d.iter()
                    .map(|(k, v)| format!("{}: {}", snow_click::py_repr(k), v.py_repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            PyV::Override(v) => format!("YamlOverride(data={})", v.py_repr()),
            PyV::Timestamp(t) => t.clone(),
            PyV::Model(m) => format!(
                "{}({})",
                m.name,
                m.fields
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.py_repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            PyV::None => false,
            PyV::Bool(b) => *b,
            PyV::Int(i) => *i != 0,
            PyV::Float(f) => *f != 0.0,
            PyV::Str(s) => !s.is_empty(),
            PyV::List(l) => !l.is_empty(),
            PyV::Dict(d) => !d.is_empty(),
            PyV::Override(_) | PyV::Model(_) | PyV::Timestamp(_) => true,
        }
    }
}

/// `deep_merge_dicts(original, overrides)`.
pub(crate) fn deep_merge(original: &mut IndexMap<String, PyV>, overrides: &IndexMap<String, PyV>) {
    for (k, v) in overrides {
        match (original.get_mut(k), v) {
            (Some(PyV::Dict(o)), PyV::Dict(n)) => deep_merge(o, n),
            _ => {
                original.insert(k.clone(), v.clone());
            }
        }
    }
}
