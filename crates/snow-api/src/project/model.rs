//! A pydantic-v2-compatible validator driven by `data/project_models.json`
//! (generated from upstream's models by `scripts/gen_project_models.py`).
//!
//! Inputs come from `yaml.BaseLoader`, so scalars are strings and lax-mode
//! coercion (str -> int/bool/float) matters; unions use pydantic's smart mode
//! (first strict match, then first lax match). Error types, messages and
//! locations follow pydantic-core so `SchemaValidationError` text matches.

use super::value::{Model, PyV};
use indexmap::IndexMap;
use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Deserialize, Debug)]
pub struct ModelsFile {
    pub versions: IndexMap<String, String>,
    pub models: IndexMap<String, ModelDesc>,
}

#[derive(Deserialize, Debug)]
pub struct ModelDesc {
    pub module: String,
    pub title: String,
    pub extra: Option<String>,
    pub fields: Vec<FieldDesc>,
    pub model_validators: Vec<ValDesc>,
}

#[derive(Deserialize, Debug)]
pub struct ValDesc {
    #[serde(rename = "fn")]
    pub func: String,
    pub mode: String,
}

#[derive(Deserialize, Debug, Default)]
pub struct Constraints {
    pub max_length: Option<usize>,
    pub min_length: Option<usize>,
    pub pattern: Option<String>,
    pub ge: Option<f64>,
    pub le: Option<f64>,
    pub gt: Option<f64>,
    pub lt: Option<f64>,
}

#[derive(Deserialize, Debug)]
pub struct FieldDesc {
    pub name: String,
    pub alias: Option<String>,
    pub required: bool,
    pub default: Option<serde_json::Value>,
    #[serde(rename = "type")]
    pub ty: TypeDesc,
    #[serde(default)]
    pub constraints: Constraints,
    pub discriminator_field: bool,
    pub validators: Vec<ValDesc>,
}

impl FieldDesc {
    pub fn key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "k", rename_all = "lowercase")]
pub enum TypeDesc {
    None,
    Any,
    Str,
    Int,
    Bool,
    Float,
    Path,
    Union {
        of: Vec<TypeDesc>,
        #[serde(default)]
        discriminator: Option<String>,
    },
    Literal {
        values: Vec<serde_json::Value>,
    },
    Enum {
        name: String,
        values: Vec<serde_json::Value>,
    },
    List {
        of: Box<TypeDesc>,
    },
    Set {
        of: Box<TypeDesc>,
    },
    Dict {
        key: Box<TypeDesc>,
        of: Box<TypeDesc>,
    },
    Model {
        name: String,
    },
    Dataclass {
        name: String,
        fields: Vec<(String, TypeDesc)>,
    },
}

pub static MODELS: LazyLock<ModelsFile> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../data/project_models.json"))
        .expect("project_models.json")
});

pub fn model(name: &str) -> &'static ModelDesc {
    &MODELS.models[name]
}

#[derive(Clone, Debug, PartialEq)]
pub enum Loc {
    Key(String),
    Idx(usize),
}

#[derive(Clone, Debug)]
pub struct VErr {
    pub kind: &'static str,
    pub loc: Vec<Loc>,
    pub msg: String,
    pub input: PyV,
}

#[derive(Clone, Copy, Default)]
pub struct Ctx {
    pub skip_templates: bool,
    pub duplicated_run: bool,
}

pub type VResult = Result<PyV, Vec<VErr>>;

fn err(kind: &'static str, loc: &[Loc], msg: impl Into<String>, input: &PyV) -> Vec<VErr> {
    vec![VErr {
        kind,
        loc: loc.to_vec(),
        msg: msg.into(),
        input: input.clone(),
    }]
}

/// A `ValueError` raised in a validator.
pub fn value_error(loc: &[Loc], msg: &str, input: &PyV) -> Vec<VErr> {
    err("value_error", loc, format!("Value error, {msg}"), input)
}

fn json_to_py(v: &serde_json::Value) -> PyV {
    match v {
        serde_json::Value::Null => PyV::None,
        serde_json::Value::Bool(b) => PyV::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => PyV::Int(i),
            None => PyV::Float(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => PyV::Str(s.clone()),
        serde_json::Value::Array(a) => PyV::List(a.iter().map(json_to_py).collect()),
        serde_json::Value::Object(o) => {
            PyV::Dict(o.iter().map(|(k, v)| (k.clone(), json_to_py(v))).collect())
        }
    }
}

fn default_of(f: &FieldDesc) -> PyV {
    match &f.default {
        Some(serde_json::Value::Object(o)) => o
            .get("value")
            .or_else(|| o.get("factory"))
            .map(json_to_py)
            .unwrap_or(PyV::None),
        _ => PyV::None,
    }
}

fn lit_repr(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => snow_click::py_repr(s),
        other => json_to_py(other).py_repr(),
    }
}

fn expected_list(values: &[serde_json::Value]) -> String {
    let r: Vec<String> = values.iter().map(lit_repr).collect();
    match r.len() {
        0 => String::new(),
        1 => r[0].clone(),
        n => format!("{} or {}", r[..n - 1].join(", "), r[n - 1]),
    }
}

/// pydantic's union member tag (`str`, `int`, `Identifier`, `list[Argument]`, ...).
fn tag(t: &TypeDesc) -> String {
    match t {
        TypeDesc::None => "none".into(),
        TypeDesc::Any => "any".into(),
        TypeDesc::Str => "str".into(),
        TypeDesc::Int => "int".into(),
        TypeDesc::Bool => "bool".into(),
        TypeDesc::Float => "float".into(),
        TypeDesc::Path => "lax-or-strict[lax=union[json-or-python[json=function-after[path_validator(), str],python=is-instance[Path]],function-after[path_validator(), str]],strict=json-or-python[json=function-after[path_validator(), str],python=is-instance[Path]]]".into(),
        TypeDesc::Union { of, .. } => format!("union[{}]", of.iter().map(tag).collect::<Vec<_>>().join(",")),
        TypeDesc::Literal { values } => {
            format!("literal[{}]", values.iter().map(lit_repr).collect::<Vec<_>>().join(","))
        }
        TypeDesc::Enum { name, .. } => name.clone(),
        TypeDesc::List { of } => format!("list[{}]", tag(of)),
        TypeDesc::Set { of } => format!("set[{}]", tag(of)),
        TypeDesc::Dict { key, of } => format!("dict[{},{}]", tag(key), tag(of)),
        TypeDesc::Model { name } => name.replace('<', "[").replace('>', "]"),
        TypeDesc::Dataclass { name, .. } => name.clone(),
    }
}

fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let t = t.strip_prefix('+').unwrap_or(t);
    if t.is_empty() || t.starts_with('_') || t.ends_with('_') || t.contains("__") {
        return None;
    }
    let body = t.strip_prefix('-').unwrap_or(t);
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit() || c == '_') {
        return None;
    }
    t.replace('_', "").parse().ok()
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "0" | "off" | "f" | "false" | "n" | "no" => Some(false),
        "1" | "on" | "t" | "true" | "y" | "yes" => Some(true),
        _ => None,
    }
}

/// `pathlib.PurePath(s)` string form.
pub fn pure_path(s: &str) -> String {
    if s.is_empty() {
        return ".".into();
    }
    let abs = s.starts_with('/');
    let parts: Vec<&str> = s
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let body = parts.join("/");
    match (abs, body.is_empty()) {
        (true, _) => format!("/{body}"),
        (false, true) => ".".into(),
        (false, false) => body,
    }
}

pub struct Engine {
    pub ctx: Ctx,
    /// Enabled feature flags (`[cli.features]`), upper-case names.
    pub features: Vec<String>,
}

impl Engine {
    pub fn new(ctx: Ctx, features: Vec<String>) -> Engine {
        Engine { ctx, features }
    }

    pub fn feature(&self, name: &str) -> bool {
        self.features.iter().any(|f| f == name)
    }

    pub fn validate(&self, t: &TypeDesc, v: &PyV, loc: &mut Vec<Loc>, strict: bool) -> VResult {
        match t {
            TypeDesc::Any => Ok(v.clone()),
            TypeDesc::None => match v {
                PyV::None => Ok(PyV::None),
                _ => Err(err("none_required", loc, "Input should be None", v)),
            },
            TypeDesc::Str => match v {
                PyV::Str(_) => Ok(v.clone()),
                _ => Err(err("string_type", loc, "Input should be a valid string", v)),
            },
            TypeDesc::Path => match v {
                PyV::Str(s) => Ok(PyV::Str(pure_path(s))),
                _ => Err(err("path_type", loc, "Input is not a valid path", v)),
            },
            TypeDesc::Int => match v {
                PyV::Int(_) => Ok(v.clone()),
                PyV::Bool(b) if !strict => Ok(PyV::Int(*b as i64)),
                PyV::Str(s) if !strict => parse_int(s).map(PyV::Int).ok_or_else(|| {
                    err(
                        "int_parsing",
                        loc,
                        "Input should be a valid integer, unable to parse string as an integer",
                        v,
                    )
                }),
                PyV::Float(f) if !strict => {
                    if f.fract() == 0.0 {
                        Ok(PyV::Int(*f as i64))
                    } else {
                        Err(err(
                            "int_from_float",
                            loc,
                            "Input should be a valid integer, got a number with a fractional part",
                            v,
                        ))
                    }
                }
                _ => Err(err("int_type", loc, "Input should be a valid integer", v)),
            },
            TypeDesc::Bool => match v {
                PyV::Bool(_) => Ok(v.clone()),
                PyV::Str(s) if !strict => parse_bool(s).map(PyV::Bool).ok_or_else(|| {
                    err(
                        "bool_parsing",
                        loc,
                        "Input should be a valid boolean, unable to interpret input",
                        v,
                    )
                }),
                PyV::Int(i) if !strict && (*i == 0 || *i == 1) => Ok(PyV::Bool(*i == 1)),
                PyV::Int(_) if !strict => Err(err(
                    "bool_parsing",
                    loc,
                    "Input should be a valid boolean, unable to interpret input",
                    v,
                )),
                _ => Err(err("bool_type", loc, "Input should be a valid boolean", v)),
            },
            TypeDesc::Float => match v {
                PyV::Float(_) => Ok(v.clone()),
                PyV::Int(i) => Ok(PyV::Float(*i as f64)),
                PyV::Str(s) if !strict => s.trim().parse::<f64>().map(PyV::Float).map_err(|_| {
                    err(
                        "float_parsing",
                        loc,
                        "Input should be a valid number, unable to parse string as a number",
                        v,
                    )
                }),
                _ => Err(err("float_type", loc, "Input should be a valid number", v)),
            },
            TypeDesc::Literal { values } => {
                let hit = values.iter().find(|x| json_to_py(x) == *v);
                match hit {
                    Some(x) => Ok(json_to_py(x)),
                    None => Err(err(
                        "literal_error",
                        loc,
                        format!("Input should be {}", expected_list(values)),
                        v,
                    )),
                }
            }
            TypeDesc::Enum { values, .. } => {
                let hit = values.iter().find(|x| json_to_py(x) == *v);
                match hit {
                    Some(x) => Ok(json_to_py(x)),
                    None => Err(err(
                        "enum",
                        loc,
                        format!("Input should be {}", expected_list(values)),
                        v,
                    )),
                }
            }
            TypeDesc::List { of } | TypeDesc::Set { of } => {
                let items = match v {
                    PyV::List(l) => l,
                    _ => {
                        let (kind, what) = if matches!(t, TypeDesc::Set { .. }) {
                            ("set_type", "Input should be a valid set")
                        } else {
                            ("list_type", "Input should be a valid list")
                        };
                        return Err(err(kind, loc, what, v));
                    }
                };
                let mut out = Vec::new();
                let mut errs = Vec::new();
                for (i, item) in items.iter().enumerate() {
                    loc.push(Loc::Idx(i));
                    match self.validate(of, item, loc, strict) {
                        Ok(x) => {
                            if !matches!(t, TypeDesc::Set { .. }) || !out.contains(&x) {
                                out.push(x)
                            }
                        }
                        Err(e) => errs.extend(e),
                    }
                    loc.pop();
                }
                if errs.is_empty() {
                    Ok(PyV::List(out))
                } else {
                    Err(errs)
                }
            }
            TypeDesc::Dict { key, of } => {
                let d = match v {
                    PyV::Dict(d) => d,
                    _ => {
                        return Err(err(
                            "dict_type",
                            loc,
                            "Input should be a valid dictionary",
                            v,
                        ));
                    }
                };
                let mut out = IndexMap::new();
                let mut errs = Vec::new();
                for (k, item) in d {
                    loc.push(Loc::Key(k.clone()));
                    if let Err(e) = self.validate(key, &PyV::Str(k.clone()), loc, strict) {
                        errs.extend(e.into_iter().map(|mut x| {
                            x.loc.push(Loc::Key("[key]".into()));
                            x
                        }));
                    }
                    match self.validate(of, item, loc, strict) {
                        Ok(x) => {
                            out.insert(k.clone(), x);
                        }
                        Err(e) => errs.extend(e),
                    }
                    loc.pop();
                }
                if errs.is_empty() {
                    Ok(PyV::Dict(out))
                } else {
                    Err(errs)
                }
            }
            TypeDesc::Union { of, discriminator } => {
                self.union(of, discriminator.as_deref(), v, loc, strict)
            }
            TypeDesc::Model { name } => self.model(name, v, loc),
            TypeDesc::Dataclass { name, fields } => {
                let d = match v {
                    PyV::Dict(d) => d,
                    PyV::Model(m) if m.name == *name => return Ok(v.clone()),
                    _ => {
                        return Err(err(
                            "dataclass_type",
                            loc,
                            format!("Input should be a dictionary or an instance of {name}"),
                            v,
                        ));
                    }
                };
                let mut m = Model {
                    name: name.clone(),
                    fields: IndexMap::new(),
                    set: Vec::new(),
                };
                let mut errs = Vec::new();
                for (fname, ft) in fields {
                    loc.push(Loc::Key(fname.clone()));
                    match d.get(fname) {
                        None => errs.extend(err("missing", loc, "Field required", v)),
                        Some(x) => match self.validate(ft, x, loc, strict) {
                            Ok(x) => {
                                m.fields.insert(fname.clone(), x);
                            }
                            Err(e) => errs.extend(e),
                        },
                    }
                    loc.pop();
                }
                for k in d.keys().filter(|k| !fields.iter().any(|(f, _)| f == *k)) {
                    loc.push(Loc::Key(k.clone()));
                    errs.extend(err(
                        "unexpected_keyword_argument",
                        loc,
                        "Unexpected keyword argument",
                        &d[k],
                    ));
                    loc.pop();
                }
                if errs.is_empty() {
                    Ok(PyV::Model(Box::new(m)))
                } else {
                    Err(errs)
                }
            }
        }
    }

    fn union(
        &self,
        of: &[TypeDesc],
        disc: Option<&str>,
        v: &PyV,
        loc: &mut Vec<Loc>,
        strict: bool,
    ) -> VResult {
        let members: Vec<&TypeDesc> = of.iter().filter(|t| !matches!(t, TypeDesc::None)).collect();
        let nullable = members.len() < of.len();
        if nullable && v.is_none() {
            return Ok(PyV::None);
        }
        if members.len() == 1 {
            return self.validate(members[0], v, loc, strict);
        }
        if let Some(d) = disc {
            let tags: Vec<(String, &TypeDesc)> = members
                .iter()
                .map(|t| match t {
                    TypeDesc::Model { name } => (discriminator_value(name, d), *t),
                    _ => (String::new(), *t),
                })
                .collect();
            let dict = match v {
                PyV::Dict(x) => x,
                _ => {
                    return Err(err(
                        "model_attributes_type",
                        loc,
                        "Input should be a valid dictionary or object to extract fields from",
                        v,
                    ));
                }
            };
            let Some(tv) = dict.get(d) else {
                return Err(err(
                    "union_tag_not_found",
                    loc,
                    format!("Unable to extract tag using discriminator '{d}'"),
                    v,
                ));
            };
            let tv_s = tv.py_str();
            return match tags.iter().find(|(t, _)| *t == tv_s) {
                Some((t, ty)) => {
                    loc.push(Loc::Key(t.clone()));
                    let r = self.validate(ty, v, loc, strict);
                    loc.pop();
                    r
                }
                None => Err(err(
                    "union_tag_invalid",
                    loc,
                    format!(
                        "Input tag '{tv_s}' found using '{d}' does not match any of the expected tags: {}",
                        tags.iter()
                            .map(|(t, _)| format!("'{t}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    v,
                )),
            };
        }
        if !strict {
            for m in &members {
                if let Ok(x) = self.validate(m, v, loc, true) {
                    return Ok(x);
                }
            }
        }
        let mut errs = Vec::new();
        for m in &members {
            loc.push(Loc::Key(tag(m)));
            match self.validate(m, v, loc, strict) {
                Ok(x) => {
                    loc.pop();
                    return Ok(x);
                }
                Err(e) => errs.extend(e),
            }
            loc.pop();
        }
        Err(errs)
    }

    /// Validate `v` against model `name` (`Model(**v)`).
    pub fn model(&self, name: &str, v: &PyV, loc: &mut Vec<Loc>) -> VResult {
        let desc = model(name);
        let mut data = match v {
            PyV::Dict(d) => d.clone(),
            PyV::Model(m) if m.name == name => return Ok(v.clone()),
            _ => {
                return Err(err(
                    "model_type",
                    loc,
                    format!(
                        "Input should be a valid dictionary or instance of {}",
                        desc.title
                    ),
                    v,
                ));
            }
        };
        // pydantic wraps each `before` validator around the next, so the last defined runs first;
        // errors report the model's original input.
        let original = PyV::Dict(data.clone());
        for mv in desc
            .model_validators
            .iter()
            .filter(|m| m.mode == "before")
            .rev()
        {
            data = super::validators::model_before(self, &mv.func, name, data, loc)?;
        }
        let mut m = Model {
            name: name.to_string(),
            fields: IndexMap::new(),
            set: Vec::new(),
        };
        let mut errs = Vec::new();
        for f in &desc.fields {
            let key = f.key();
            loc.push(Loc::Key(key.to_string()));
            match data.get(key) {
                None => {
                    if f.required {
                        errs.extend(err(
                            "missing",
                            loc,
                            "Field required",
                            &PyV::Dict(data.clone()),
                        ));
                    } else {
                        m.fields.insert(f.name.clone(), default_of(f));
                    }
                }
                Some(x) => match self.field(name, f, x, loc) {
                    Ok(val) => {
                        m.fields.insert(f.name.clone(), val);
                        m.set.push(f.name.clone());
                    }
                    Err(e) => errs.extend(e),
                },
            }
            loc.pop();
        }
        if desc.extra.as_deref() == Some("forbid") {
            for (k, x) in &data {
                if !desc.fields.iter().any(|f| f.key() == k) {
                    loc.push(Loc::Key(k.clone()));
                    errs.extend(err(
                        "extra_forbidden",
                        loc,
                        "Extra inputs are not permitted",
                        x,
                    ));
                    loc.pop();
                }
            }
        }
        if !errs.is_empty() {
            return Err(errs);
        }
        let mut out = PyV::Model(Box::new(m));
        let raw = if name == "DefinitionV20" {
            PyV::Dict(data)
        } else {
            original
        };
        for mv in desc.model_validators.iter().filter(|m| m.mode == "after") {
            out = super::validators::model_after(self, &mv.func, out, &raw, loc)?;
        }
        Ok(out)
    }

    fn field(&self, model: &str, f: &FieldDesc, v: &PyV, loc: &mut Vec<Loc>) -> VResult {
        if self.ctx.skip_templates
            && !f.discriminator_field
            && v.as_str().is_some_and(|s| s.contains("<%"))
        {
            return Ok(v.clone());
        }
        let mut v = v.clone();
        for val in f.validators.iter().filter(|x| x.mode == "before") {
            v = super::validators::field_before(self, &val.func, model, &f.name, v, loc)?;
        }
        let mut out = self.validate(&f.ty, &v, loc, false)?;
        self.constraints(&f.constraints, &out, loc)?;
        for val in f.validators.iter().filter(|x| x.mode == "after") {
            out = super::validators::field_after(self, &val.func, model, &f.name, out, &v, loc)?;
        }
        Ok(out)
    }

    fn constraints(&self, c: &Constraints, v: &PyV, loc: &[Loc]) -> Result<(), Vec<VErr>> {
        if let PyV::Str(s) = v {
            let n = s.chars().count();
            if let Some(max) = c.max_length
                && n > max
            {
                let unit = if max == 1 { "character" } else { "characters" };
                return Err(err(
                    "string_too_long",
                    loc,
                    format!("String should have at most {max} {unit}"),
                    v,
                ));
            }
            if let Some(min) = c.min_length
                && n < min
            {
                let unit = if min == 1 { "character" } else { "characters" };
                return Err(err(
                    "string_too_short",
                    loc,
                    format!("String should have at least {min} {unit}"),
                    v,
                ));
            }
            if let Some(p) = &c.pattern {
                let re = fancy_pattern(p);
                if !re.is_match(s) {
                    return Err(err(
                        "string_pattern_mismatch",
                        loc,
                        format!("String should match pattern '{p}'"),
                        v,
                    ));
                }
            }
        }
        let num = match v {
            PyV::Int(i) => Some(*i as f64),
            PyV::Float(f) => Some(*f),
            _ => None,
        };
        if let Some(x) = num {
            let fmt = |b: f64| {
                if b.fract() == 0.0 {
                    format!("{}", b as i64)
                } else {
                    b.to_string()
                }
            };
            if let Some(b) = c.gt
                && x <= b
            {
                return Err(err(
                    "greater_than",
                    loc,
                    format!("Input should be greater than {}", fmt(b)),
                    v,
                ));
            }
            if let Some(b) = c.ge
                && x < b
            {
                return Err(err(
                    "greater_than_equal",
                    loc,
                    format!("Input should be greater than or equal to {}", fmt(b)),
                    v,
                ));
            }
            if let Some(b) = c.lt
                && x >= b
            {
                return Err(err(
                    "less_than",
                    loc,
                    format!("Input should be less than {}", fmt(b)),
                    v,
                ));
            }
            if let Some(b) = c.le
                && x > b
            {
                return Err(err(
                    "less_than_equal",
                    loc,
                    format!("Input should be less than or equal to {}", fmt(b)),
                    v,
                ));
            }
        }
        Ok(())
    }

    /// Build a model from Python values (`Model(**data)` inside a validator).
    pub fn construct(
        &self,
        name: &str,
        data: IndexMap<String, PyV>,
        loc: &mut Vec<Loc>,
    ) -> VResult {
        self.model(name, &PyV::Dict(data), loc)
    }
}

fn fancy_pattern(p: &str) -> regex::Regex {
    regex::Regex::new(p).unwrap_or_else(|_| regex::Regex::new("^$").expect("re"))
}

/// The v2 entity model whose `type` literal is `ty`.
pub fn entity_model_for_type(ty: &str) -> Option<&'static str> {
    let v2 = model(&MODELS.versions["2"]);
    let ents = v2.fields.iter().find(|f| f.name == "entities")?;
    let TypeDesc::Dict { of, .. } = &ents.ty else {
        return None;
    };
    let TypeDesc::Union { of, .. } = of.as_ref() else {
        return None;
    };
    of.iter().find_map(|t| match t {
        TypeDesc::Model { name } if discriminator_value(name, "type") == ty => Some(name.as_str()),
        _ => None,
    })
}

/// The `Literal` value of a model's discriminator field.
pub fn discriminator_value(model_name: &str, field: &str) -> String {
    model(model_name)
        .fields
        .iter()
        .find(|f| f.name == field)
        .and_then(|f| match &f.ty {
            TypeDesc::Literal { values } => {
                values.first().and_then(|v| v.as_str().map(str::to_string))
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// `model_dump(exclude_unset=True, exclude_none=True, by_alias=True)`.
pub fn dump_set(v: &PyV) -> PyV {
    match v {
        PyV::Model(m) => {
            let desc = MODELS.models.get(&m.name);
            let mut out = IndexMap::new();
            for (k, x) in &m.fields {
                if x.is_none() || !m.set.contains(k) {
                    continue;
                }
                let key = desc
                    .and_then(|d| d.fields.iter().find(|f| f.name == *k))
                    .map(|f| f.key().to_string())
                    .unwrap_or_else(|| k.clone());
                out.insert(key, dump_set(x));
            }
            PyV::Dict(out)
        }
        PyV::Dict(d) => PyV::Dict(
            d.iter()
                .filter(|(_, x)| !x.is_none())
                .map(|(k, x)| (k.clone(), dump_set(x)))
                .collect(),
        ),
        PyV::List(l) => PyV::List(l.iter().map(dump_set).collect()),
        PyV::Override(x) => dump_set(x),
        other => other.clone(),
    }
}

/// `model_dump(by_alias=True, mode=…, exclude_none=…)`.
pub fn dump(v: &PyV, exclude_none: bool) -> PyV {
    match v {
        PyV::Model(m) => {
            let desc = MODELS.models.get(&m.name);
            let mut out = IndexMap::new();
            for (k, x) in &m.fields {
                if exclude_none && x.is_none() {
                    continue;
                }
                let key = desc
                    .and_then(|d| d.fields.iter().find(|f| f.name == *k))
                    .map(|f| f.key().to_string())
                    .unwrap_or_else(|| k.clone());
                out.insert(key, dump(x, exclude_none));
            }
            PyV::Dict(out)
        }
        PyV::Dict(d) => PyV::Dict(
            d.iter()
                .filter(|(_, x)| !(exclude_none && x.is_none()))
                .map(|(k, x)| (k.clone(), dump(x, exclude_none)))
                .collect(),
        ),
        PyV::List(l) => PyV::List(l.iter().map(|x| dump(x, exclude_none)).collect()),
        PyV::Override(x) => dump(x, exclude_none),
        other => other.clone(),
    }
}

/// `SchemaValidationError` text for a failed `title` model.
/// `str(pydantic.ValidationError)` for `title`.
pub fn validation_error_str(title: &str, errs: &[VErr]) -> String {
    let n = errs.len();
    let mut out = format!(
        "{n} validation error{} for {title}",
        if n == 1 { "" } else { "s" }
    );
    for e in errs {
        let loc: Vec<String> = e
            .loc
            .iter()
            .map(|l| match l {
                Loc::Key(k) => k.clone(),
                Loc::Idx(i) => i.to_string(),
            })
            .collect();
        let r = e.input.py_repr();
        let chars: Vec<char> = r.chars().collect();
        let input = if chars.len() <= 50 {
            r
        } else {
            format!(
                "{}...{}",
                chars[..25].iter().collect::<String>(),
                chars[chars.len() - 24..].iter().collect::<String>()
            )
        };
        let ty = match &e.input {
            PyV::None => "NoneType",
            PyV::Bool(_) => "bool",
            PyV::Int(_) => "int",
            PyV::Float(_) => "float",
            PyV::List(_) => "list",
            PyV::Dict(_) => "dict",
            _ => "str",
        };
        let kind = e.kind.strip_prefix("fatal:").unwrap_or(e.kind);
        if !loc.is_empty() {
            out.push('\n');
            out.push_str(&loc.join("."));
        }
        out.push_str(&format!(
            "\n  {} [type={kind}, input_value={input}, input_type={ty}]\n    For further information visit https://errors.pydantic.dev/2.12/v/{kind}",
            e.msg
        ));
    }
    out
}

pub fn schema_error_message(title: &str, errs: &[VErr]) -> String {
    let mut lines = vec![format!(
        "During evaluation of {title} in project definition following errors were encountered:"
    )];
    for e in errs {
        let mut loc = String::new();
        for l in &e.loc {
            match l {
                Loc::Key(k) => {
                    loc.push('.');
                    loc.push_str(k);
                }
                Loc::Idx(i) => loc.push_str(&format!("[{i}]")),
            }
        }
        let loc = loc.strip_prefix('.').map(str::to_string).unwrap_or(loc);
        let input = e.input.py_str();
        lines.push(match e.kind {
            "string_type" => format!("{} for field '{loc}', you provided '{input}'.", e.msg),
            "extra_forbidden" => format!(
                "{}. You provided field '{loc}' with value '{input}' that is not supported in given version.",
                e.msg
            ),
            "missing" => format!("Your project definition is missing the following field: '{loc}'"),
            _ => format!("For field {loc} you provided '{input}'. This caused: {}", e.msg),
        });
    }
    crate::sql::dedent(&lines.join("\n"))
}
