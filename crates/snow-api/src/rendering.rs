//! Client-side SQL templating (`snowflake.cli.api.rendering.sql_templates`).
//!
//! `<% var %>` (standard), `&{ var }` (legacy) and `{{ var }}` (Jinja) are
//! rendered with MiniJinja configured like upstream's Jinja environments:
//! logic blocks disabled for the SQL syntaxes, strict undefined values and
//! dictionary-only attribute lookup.

use crate::console::Console;
use crate::errors::SnowError;
use indexmap::IndexMap;
use minijinja::value::{Object, ObjectRepr, Value};
use minijinja::{Environment, ErrorKind, UndefinedBehavior};
use std::sync::{Arc, Mutex};

const NO_BLOCKS: &str = "___very___unique___block___to___disable___logic___blocks___";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxConfig {
    pub legacy: bool,
    pub standard: bool,
    pub jinja: bool,
}

impl Default for SyntaxConfig {
    fn default() -> Self {
        SyntaxConfig {
            legacy: true,
            standard: true,
            jinja: false,
        }
    }
}

/// `transpile_snowsql_templates`: `string.Template` with `&` as delimiter,
/// mapping every name to `&{ name }`.
pub fn transpile_snowsql_templates(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut i = 0;
    let ident = |at: usize| -> usize {
        let mut j = at;
        if j < b.len() && (b[j] == b'_' || b[j].is_ascii_alphabetic()) {
            j += 1;
            while j < b.len() && (b[j] == b'_' || b[j].is_ascii_alphanumeric()) {
                j += 1;
            }
        }
        j
    };
    while i < b.len() {
        if b[i] != b'&' {
            i += 1;
            continue;
        }
        out.push_str(&text[last..i]);
        if b.get(i + 1) == Some(&b'&') {
            out.push('&');
            i += 2;
        } else if ident(i + 1) > i + 1 {
            let end = ident(i + 1);
            out.push_str("&{ ");
            out.push_str(&text[i + 1..end]);
            out.push_str(" }");
            i = end;
        } else if b.get(i + 1) == Some(&b'{')
            && ident(i + 2) > i + 2
            && b.get(ident(i + 2)) == Some(&b'}')
        {
            let end = ident(i + 2);
            out.push_str("&{ ");
            out.push_str(&text[i + 2..end]);
            out.push_str(" }");
            i = end + 1;
        } else {
            out.push('&');
            i += 1;
        }
        last = i;
    }
    out.push_str(&text[last..]);
    out
}

/// Why rendering failed.
#[derive(Debug)]
pub enum RenderError {
    /// `jinja2.UndefinedError` (message only).
    Undefined(String),
    Fatal(SnowError),
}

impl RenderError {
    /// The error as raised when nothing catches `UndefinedError`.
    pub fn into_snow(self) -> SnowError {
        match self {
            RenderError::Undefined(m) => SnowError::raised("UndefinedError", m),
            RenderError::Fatal(e) => e,
        }
    }
}

type Misses = Arc<Mutex<Option<String>>>;

/// `ProjectEnvironment`: `--env` overrides, then environment variables, then the
/// definition's `env` section.
#[derive(Debug)]
struct EnvObject {
    overrides: IndexMap<String, String>,
    defaults: IndexMap<String, Value>,
    misses: Misses,
}

impl Object for EnvObject {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let k = key.as_str()?;
        if let Some(v) = self.overrides.get(k) {
            return Some(Value::from(v.clone()));
        }
        if let Ok(v) = std::env::var(k) {
            return Some(Value::from(v));
        }
        if let Some(v) = self.defaults.get(k) {
            return Some(v.clone());
        }
        *self.misses.lock().unwrap() = Some(format!(
            "'snowflake.cli.api.utils.models.ProjectEnvironment object' has no attribute '{k}'"
        ));
        None
    }
}

/// A dict whose missing keys are reported like Jinja does.
#[derive(Debug)]
struct DictObject {
    items: IndexMap<String, Value>,
    misses: Misses,
    root: bool,
}

impl Object for DictObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let k = key.as_str()?;
        if let Some(v) = self.items.get(k) {
            return Some(v.clone());
        }
        *self.misses.lock().unwrap() = Some(if self.root {
            format!("'{k}' is undefined")
        } else {
            format!("'dict object' has no attribute '{k}'")
        });
        None
    }
    fn enumerate(self: &Arc<Self>) -> minijinja::value::Enumerator {
        let keys: Vec<Value> = self.items.keys().map(|k| Value::from(k.clone())).collect();
        minijinja::value::Enumerator::Values(keys)
    }
}

/// The values templates can see.
pub struct TemplateData<'a> {
    /// `-D` variables.
    pub data: &'a IndexMap<String, String>,
    /// `--env` overrides for `ctx.env`.
    pub env_overrides: &'a IndexMap<String, String>,
}

fn env_for(start: &str, end: &str, sql: bool) -> Environment<'static> {
    let mut env = Environment::new();
    if sql {
        let syntax = minijinja::syntax::SyntaxConfig::builder()
            .block_delimiters(NO_BLOCKS, NO_BLOCKS)
            .variable_delimiters(start.to_string(), end.to_string())
            .build()
            .expect("valid delimiters");
        env.set_syntax(syntax);
    }
    env.set_keep_trailing_newline(true);
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env
}

/// Jinja's wording for a MiniJinja syntax error.
pub fn syntax_message(e: &minijinja::Error, src: &str) -> String {
    let d = e.detail().unwrap_or("syntax error").to_string();
    let token = |t: &str| match t {
        "end of variable block" => "end of print statement".to_string(),
        "end of block" => "end of statement block".to_string(),
        "end of input" => "end of template".to_string(),
        other => other.to_string(),
    };
    let got = d
        .strip_prefix("unexpected ")
        .map(|r| r.split_once(", expected ").map(|x| x.0).unwrap_or(r));
    let Some(got) = got else { return d };
    // Jinja's `parse_tuple` reports a missing expression right after a
    // delimiter or keyword; `parse_primary` reports anything else.
    let before = e
        .range()
        .and_then(|r| src.get(..r.start))
        .unwrap_or("")
        .trim_end();
    let after_keyword = ["{{", "<%", "&{", "{%", "if", "elif", "in", "=", "("]
        .iter()
        .any(|k| before.ends_with(k));
    if after_keyword {
        format!("Expected an expression, got '{}'", token(got))
    } else {
        format!("unexpected '{}'", token(got))
    }
}

fn fatal_in(e: minijinja::Error, src: &str) -> RenderError {
    match e.kind() {
        ErrorKind::SyntaxError => RenderError::Fatal(SnowError::raised(
            "TemplateSyntaxError",
            syntax_message(&e, src),
        )),
        _ => RenderError::Fatal(SnowError::raised(
            "TemplateError",
            e.detail()
                .map(str::to_string)
                .unwrap_or_else(|| e.to_string()),
        )),
    }
}

fn undeclared(env: &Environment, content: &str) -> Result<bool, RenderError> {
    let t = env
        .template_from_str(content)
        .map_err(|e| fatal_in(e, content))?;
    Ok(!t.undeclared_variables(false).is_empty())
}

fn render_with(
    env: &Environment,
    content: &str,
    root: &IndexMap<String, Value>,
    data: &IndexMap<String, String>,
    misses: &Misses,
) -> Result<String, RenderError> {
    let t = env
        .template_from_str(content)
        .map_err(|e| fatal_in(e, content))?;
    *misses.lock().unwrap() = None;
    let ctx = Value::from_object(DictObject {
        items: root.clone(),
        misses: misses.clone(),
        root: true,
    });
    t.render(ctx).map_err(|e| {
        if e.kind() == ErrorKind::UndefinedError {
            if let Some(m) = misses.lock().unwrap().take() {
                return RenderError::Undefined(m);
            }
            // An attribute of a plain string (`<% name.x %>`).
            if let Some(r) = e.range()
                && let Some(src) = content.get(r)
                && let Some(c) = regex::Regex::new(r"^\s*(\w+)\s*\.\s*(\w+)")
                    .unwrap()
                    .captures(src)
                && data.contains_key(&c[1])
            {
                return RenderError::Undefined(format!(
                    "'str object' has no attribute '{}'",
                    &c[2]
                ));
            }
            return RenderError::Undefined(e.detail().unwrap_or("undefined value").to_string());
        }
        fatal_in(e, content)
    })
}

fn pyv_value(v: &crate::project::PyV, misses: &Misses) -> Value {
    use crate::project::PyV;
    match v {
        PyV::None => Value::from(()),
        PyV::Bool(b) => Value::from(*b),
        PyV::Int(i) => Value::from(*i),
        PyV::Float(f) => Value::from(*f),
        PyV::Str(s) | PyV::Timestamp(s) => Value::from(s.clone()),
        PyV::List(l) => Value::from(l.iter().map(|x| pyv_value(x, misses)).collect::<Vec<_>>()),
        PyV::Dict(d) => Value::from_object(DictObject {
            items: d
                .iter()
                .map(|(k, x)| (k.clone(), pyv_value(x, misses)))
                .collect(),
            misses: misses.clone(),
            root: false,
        }),
        PyV::Override(x) => pyv_value(x, misses),
        PyV::Model(m) => Value::from_object(DictObject {
            items: m
                .fields
                .iter()
                .map(|(k, x)| (k.clone(), pyv_value(x, misses)))
                .collect(),
            misses: misses.clone(),
            root: false,
        }),
    }
}

/// `render_script_template` with the project template context (`{"ctx": ...}`) and
/// the SQL environment chosen by `choose_sql_jinja_env_based_on_template_syntax`.
/// Errors carry Jinja's message and (for syntax errors) the line number.
pub fn render_project_sql(
    content: &str,
    reference_name: &str,
    ctx: &crate::project::PyV,
    env_overrides: &IndexMap<String, String>,
    console: &mut Console,
    sql: bool,
) -> Result<String, (String, Option<usize>)> {
    let misses: Misses = Arc::new(Mutex::new(None));
    let mut items: IndexMap<String, Value> = IndexMap::new();
    if let crate::project::PyV::Dict(d) = ctx {
        for (k, v) in d {
            if k == "env" {
                let defaults = match v {
                    crate::project::PyV::Dict(e) => e
                        .iter()
                        .map(|(k, x)| (k.clone(), pyv_value(x, &misses)))
                        .collect(),
                    _ => IndexMap::new(),
                };
                items.insert(
                    k.clone(),
                    Value::from_object(EnvObject {
                        overrides: env_overrides.clone(),
                        defaults,
                        misses: misses.clone(),
                    }),
                );
            } else {
                items.insert(k.clone(), pyv_value(v, &misses));
            }
        }
    }
    let mut root: IndexMap<String, Value> = IndexMap::new();
    root.insert(
        "ctx".into(),
        Value::from_object(DictObject {
            items,
            misses: misses.clone(),
            root: false,
        }),
    );
    let fatal = |e: RenderError| -> (String, Option<usize>) {
        match e {
            RenderError::Undefined(m) => (m, None),
            RenderError::Fatal(e) => (e.message(), None),
        }
    };
    let legacy = env_for("&{", "}", true);
    let standard = env_for("<%", "%>", true);
    let has_old = sql && undeclared(&legacy, content).map_err(fatal)?;
    let has_new = sql && undeclared(&standard, content).map_err(fatal)?;
    if has_old && has_new {
        return Err((
            format!(
                "The SQL query in {reference_name} mixes &{{ ... }} syntax and <% ... %> syntax."
            ),
            None,
        ));
    }
    let env = if has_old {
        console.warning(&format!(
            "Warning: &{{ ... }} syntax in {reference_name} is deprecated and will no longer be supported. Use <% ... %> syntax instead."
        ));
        legacy
    } else {
        standard
    };
    let t = env
        .template_from_str(content)
        .map_err(|e| (syntax_message(&e, content), e.line()))?;
    *misses.lock().unwrap() = None;
    let root = Value::from_object(DictObject {
        items: root,
        misses: misses.clone(),
        root: true,
    });
    t.render(root).map_err(|e| {
        if e.kind() == ErrorKind::UndefinedError
            && let Some(m) = misses.lock().unwrap().take()
        {
            return (m, None);
        }
        (e.detail().unwrap_or("undefined value").to_string(), None)
    })
}

pub fn has_sql_templates(content: &str) -> bool {
    content.contains("&{") || content.contains("<%") || content.contains("{{")
}

/// `snowflake_sql_jinja_render`.
pub fn sql_render(
    content: &str,
    cfg: SyntaxConfig,
    vars: &TemplateData,
    console: &mut Console,
) -> Result<String, RenderError> {
    for reserved in ["ctx", "fn"] {
        if vars.data.contains_key(reserved) {
            return Err(RenderError::Fatal(SnowError::click(format!(
                "{reserved} in user defined data. The `{reserved}` variable is reserved for CLI usage."
            ))));
        }
    }
    let misses: Misses = Arc::new(Mutex::new(None));
    let mut root: IndexMap<String, Value> = IndexMap::new();
    if has_sql_templates(content) {
        let env_obj = Value::from_object(EnvObject {
            overrides: vars.env_overrides.clone(),
            defaults: IndexMap::new(),
            misses: misses.clone(),
        });
        let ctx = Value::from_object(DictObject {
            items: [("env".to_string(), env_obj)].into_iter().collect(),
            misses: misses.clone(),
            root: false,
        });
        root.insert("ctx".into(), ctx);
    }
    for (k, v) in vars.data {
        root.insert(k.clone(), Value::from(v.clone()));
    }
    let legacy = || env_for("&{", "}", true);
    let standard = || env_for("<%", "%>", true);
    let env = match (cfg.legacy, cfg.standard) {
        (true, true) => {
            let has_old = undeclared(&legacy(), content)?;
            let has_new = undeclared(&standard(), content)?;
            if has_old && has_new {
                return Err(RenderError::Fatal(
                    SnowError::cli("The SQL query mixes &{ ... } syntax and <% ... %> syntax.")
                        .named("InvalidTemplateError"),
                ));
            }
            if has_old {
                console.warning(
                    "Warning: &{ ... } syntax is deprecated and will no longer be supported. Use <% ... %> syntax instead.",
                );
                Some(legacy())
            } else {
                Some(standard())
            }
        }
        (true, false) => Some(legacy()),
        (false, true) => Some(standard()),
        (false, false) => None,
    };
    let mut out = content.to_string();
    if let Some(env) = env {
        out = render_with(&env, &out, &root, vars.data, &misses)?;
    }
    if cfg.jinja {
        out = render_with(&env_for("{{", "}}", false), &out, &root, vars.data, &misses)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::transpile_snowsql_templates as t;

    #[test]
    fn transpiles_like_string_template() {
        assert_eq!(
            t("&a &&b &{c} &{ d } &1 & x"),
            "&{ a } &b &{ c } &{ d } &1 & x"
        );
        assert_eq!(t("&A_b9."), "&{ A_b9 }.");
    }
}
