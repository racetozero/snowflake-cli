//! `DefinitionManager` / `load_project` / `render_definition_template`.

use super::model::{self, Ctx, Engine, Loc, MODELS, VErr};
use super::render::{self, Host};
use super::value::{PyV, deep_merge};
use super::yaml::{self, LoadError};
use crate::errors::SnowError;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};

pub const BASE: &str = "snowflake.yml";
pub const USER: &str = "snowflake.local.yml";

/// A loaded project: the validated definition and the template context.
#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
    /// The validated `DefinitionV10`/`V11`/`V20` model.
    pub definition: PyV,
    /// `ctx`: the rendered definition with defaults; `ctx.env` resolves
    /// `--env` overrides, then OS environment, then the definition's `env`.
    pub ctx: PyV,
    pub env_overrides: IndexMap<String, String>,
}

impl Project {
    pub fn version(&self) -> String {
        self.definition
            .str_of("definition_version")
            .unwrap_or_default()
    }
    pub fn meets_version(&self, required: &str) -> bool {
        version_ge(&self.version(), required)
    }
    /// `ctx.env.<name>` (`ProjectEnvironment.__getitem__`).
    pub fn env_value(&self, name: &str) -> Option<PyV> {
        if let Some(v) = self.env_overrides.get(name) {
            return Some(PyV::Str(v.clone()));
        }
        if let Ok(v) = std::env::var(name) {
            return Some(PyV::Str(v));
        }
        self.ctx.get("env").and_then(|e| e.get(name)).cloned()
    }
    pub fn entities(&self) -> IndexMap<String, PyV> {
        self.definition
            .get("entities")
            .and_then(|e| e.as_dict())
            .cloned()
            .unwrap_or_default()
    }
}

/// `packaging.version.Version(a) >= Version(b)` for `1`, `1.1`, `2`.
pub fn version_ge(a: &str, b: &str) -> bool {
    let p = |s: &str| -> Vec<u64> {
        let mut v: Vec<u64> = s
            .split('.')
            .map(|x| x.trim().parse().unwrap_or(0))
            .collect();
        while v.last() == Some(&0) {
            v.pop();
        }
        v
    };
    p(a) >= p(b)
}

pub fn definition_files(root: &Path) -> Vec<PathBuf> {
    let base = root.join(BASE);
    if !base.is_file() {
        return vec![];
    }
    let mut out = vec![base];
    let user = root.join(USER);
    if user.is_file() {
        out.push(user);
    }
    out
}

fn load_yaml(p: &Path) -> Result<PyV, SnowError> {
    let text = std::fs::read_to_string(p)?;
    match yaml::load(&text) {
        Ok(v) => Ok(v.unwrap_or_else(|| PyV::Dict(IndexMap::new()))),
        Err(LoadError::Duplicate(k)) => Err(SnowError::click(format!(
            "While loading the project definition file, duplicate key was found: {k}"
        ))),
        Err(LoadError::Syntax(m)) => Err(SnowError::unexpected(m).named("ScannerError")),
    }
}

fn merged(paths: &[PathBuf]) -> Result<Option<PyV>, SnowError> {
    let Some(first) = paths.first() else {
        return Ok(None);
    };
    let mut def = load_yaml(first)?;
    for p in &paths[1..] {
        let o = load_yaml(p)?;
        match (&mut def, &o) {
            (PyV::Dict(d), PyV::Dict(od)) => deep_merge(d, od),
            _ => {
                return Err(
                    SnowError::unexpected("Arguments are not of type dict").named("ValueError")
                );
            }
        }
    }
    Ok(Some(def))
}

fn first_fatal(errs: &[VErr]) -> Option<SnowError> {
    errs.iter().find_map(|e| match e.kind {
        "fatal:click" => Some(SnowError::click(e.msg.clone())),
        "fatal:unexpected" => Some(SnowError::unexpected(e.msg.clone()).named("AttributeError")),
        _ => None,
    })
}

/// `build_project_definition(**data)`.
pub fn build(data: &PyV, ctx: Ctx, features: &[String]) -> Result<PyV, SnowError> {
    let engine = Engine::new(ctx, features.to_vec());
    let version = data.get("definition_version").map(PyV::py_str);
    let name = match version.as_deref().and_then(|v| MODELS.versions.get(v)) {
        Some(n) => n.as_str(),
        None => "_ProjectDefinitionBase",
    };
    let mut loc = Vec::<Loc>::new();
    engine.model(name, data, &mut loc).map_err(|errs| {
        first_fatal(&errs).unwrap_or_else(|| {
            SnowError::click(model::schema_error_message(
                &model::model(name).title,
                &errs,
            ))
            .named("SchemaValidationError")
        })
    })
}

fn invalid_template(m: impl Into<String>) -> SnowError {
    SnowError::cli(m.into()).named("InvalidTemplateError")
}

fn scalars<'a>(v: &'a PyV, out: &mut Vec<&'a PyV>) {
    match v {
        PyV::Dict(d) => d.values().for_each(|x| scalars(x, out)),
        PyV::List(l) => l.iter().for_each(|x| scalars(x, out)),
        PyV::Override(x) => scalars(x, out),
        other => out.push(other),
    }
}

fn referenced(def: &PyV) -> Result<Vec<Vec<String>>, SnowError> {
    let mut vals = Vec::new();
    scalars(def, &mut vals);
    let mut out: Vec<Vec<String>> = Vec::new();
    for v in vals {
        let s = v.py_str();
        match render::referenced_vars(&s) {
            Ok(vars) => {
                for c in vars {
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
            }
            Err(render::TemplateError::Syntax(e)) => {
                return Err(invalid_template(format!(
                    "Error parsing template from project definition file. Value: '{s}'. Error: {e}"
                )));
            }
            Err(render::TemplateError::Unexpected) => {
                return Err(invalid_template(format!(
                    "Unexpected template syntax in {s}"
                )));
            }
        }
    }
    Ok(out)
}

fn map_render(
    v: &mut PyV,
    f: &mut dyn FnMut(&PyV) -> Result<PyV, SnowError>,
) -> Result<(), SnowError> {
    match v {
        PyV::Dict(d) => {
            for x in d.values_mut() {
                map_render(x, f)?;
            }
        }
        PyV::List(l) => {
            for x in l.iter_mut() {
                map_render(x, f)?;
            }
        }
        PyV::Override(x) => map_render(x, f)?,
        other => *other = f(other)?,
    }
    Ok(())
}

const FUNCTIONS: [&str; 5] = [
    "concat_ids",
    "get_username",
    "id_to_str",
    "sanitize_id",
    "str_to_id",
];

fn read_from_context(ctx: &PyV, chain: &[String]) -> Result<PyV, SnowError> {
    let key = chain.join(".");
    if chain[0] == "fn" {
        if chain.len() == 2 && FUNCTIONS.contains(&chain[1].as_str()) {
            return Ok(PyV::Str(format!("<function {}>", chain[1])));
        }
        return Err(invalid_template(format!(
            "Could not find template variable {key}"
        )));
    }
    let mut cur = ctx;
    for k in chain {
        match cur {
            PyV::Dict(d) if d.contains_key(k) => cur = &d[k],
            _ => {
                return Err(invalid_template(format!(
                    "Could not find template variable {key}"
                )));
            }
        }
    }
    match cur {
        PyV::None => Err(invalid_template(format!(
            "Template variable {key} does not have a value"
        ))),
        PyV::Dict(_) | PyV::List(_) => Err(invalid_template(format!(
            "Template variable {key} does not have a scalar value"
        ))),
        other => Ok(other.clone()),
    }
}

fn add_to_context(ctx: &mut IndexMap<String, PyV>, chain: &[String], v: PyV) {
    let mut cur = ctx;
    for k in &chain[..chain.len() - 1] {
        let e = cur
            .entry(k.clone())
            .or_insert_with(|| PyV::Dict(IndexMap::new()));
        if !matches!(e, PyV::Dict(_)) {
            *e = PyV::Dict(IndexMap::new());
        }
        cur = e.as_dict_mut().expect("dict");
    }
    cur.insert(chain[chain.len() - 1].clone(), v);
}

struct Node {
    chain: Vec<String>,
    templated: Option<PyV>,
    rendered: Option<PyV>,
    deps: Vec<usize>,
}

/// `render_definition_template` + `build_project_definition`.
pub fn render_definition(
    original: PyV,
    env_overrides: &IndexMap<String, String>,
    features: &[String],
    host: &dyn Host,
    warn: &mut dyn FnMut(&str),
) -> Result<(PyV, PyV), SnowError> {
    let mut definition = original;
    let version = definition.get("definition_version").map(PyV::py_str);
    if version.as_deref().is_none_or(|v| !version_ge(v, "1.1")) {
        let has = referenced(&definition)
            .map(|r| !r.is_empty())
            .unwrap_or(true);
        if has {
            warn(
                "Ignoring template pattern in project definition file. Update 'definition_version' to 1.1 or later in snowflake.yml to enable template expansion.",
            );
        }
        let model = build(&definition, Ctx::default(), features)?;
        return Ok((model, definition));
    }
    // _add_defaults_to_definition
    let with_defaults = build(
        &definition,
        Ctx {
            skip_templates: true,
            duplicated_run: false,
        },
        features,
    )?;
    let mut defaults = model::dump(&with_defaults, true);
    if let (PyV::Dict(d), PyV::Dict(o)) = (&mut defaults, &definition) {
        deep_merge(d, o);
    }
    definition = defaults;
    match definition.get("env") {
        None => {}
        Some(PyV::Dict(env)) => {
            for (k, v) in env {
                if matches!(v, PyV::None | PyV::Dict(_) | PyV::List(_)) {
                    return Err(invalid_template(format!(
                        "Variable {k} in env section of project definition file should be a scalar"
                    )));
                }
            }
        }
        Some(_) => {
            return Err(invalid_template(
                "env section in project definition file should be a mapping",
            ));
        }
    }
    let ctx_root = PyV::Dict(
        [("ctx".to_string(), definition.clone())]
            .into_iter()
            .collect(),
    );
    let vars = referenced(&definition)?;
    let mut nodes: Vec<Node> = vars
        .iter()
        .map(|c| Node {
            chain: c.clone(),
            templated: None,
            rendered: None,
            deps: vec![],
        })
        .collect();
    #[allow(clippy::needless_range_loop)]
    for i in 0..nodes.len() {
        let chain = nodes[i].chain.clone();
        let is_env = chain.len() == 3 && chain[0] == "ctx" && chain[1] == "env";
        let env_hit = is_env
            .then(|| {
                env_overrides
                    .get(&chain[2])
                    .cloned()
                    .or_else(|| std::env::var(&chain[2]).ok())
            })
            .flatten();
        if let Some(v) = env_hit {
            nodes[i].rendered = Some(PyV::Str(v.clone()));
            nodes[i].templated = Some(PyV::Str(v));
            continue;
        }
        let t = read_from_context(&ctx_root, &chain)?;
        let deps = if chain[0] == "fn" {
            vec![]
        } else {
            referenced(&t)?
        };
        for d in deps {
            match vars.iter().position(|c| *c == d) {
                Some(j) => nodes[i].deps.push(j),
                None => {
                    return Err(
                        SnowError::unexpected(format!("'{}'", d.join("."))).named("KeyError")
                    );
                }
            }
        }
        nodes[i].templated = Some(t);
    }
    // DFS
    let mut status = vec![0u8; nodes.len()];
    fn visit(
        i: usize,
        nodes: &mut Vec<Node>,
        status: &mut Vec<u8>,
        host: &dyn Host,
    ) -> Result<(), SnowError> {
        if status[i] == 2 {
            return Ok(());
        }
        status[i] = 1;
        for d in nodes[i].deps.clone() {
            if status[d] == 1 {
                return Err(SnowError::cli(format!(
                    "Cycle detected in template variable {}",
                    nodes[i].chain.join(".")
                ))
                .named("CycleDetectedError"));
            }
            visit(d, nodes, status, host)?;
        }
        if nodes[i].rendered.is_none() && nodes[i].chain[0] != "fn" {
            let mut ctx = IndexMap::new();
            for d in &nodes[i].deps {
                if let Some(v) = &nodes[*d].rendered {
                    add_to_context(&mut ctx, &nodes[*d].chain, v.clone());
                }
            }
            let t = nodes[i].templated.clone().unwrap_or(PyV::None);
            let r = render::render(&t, &ctx, host)
                .map_err(|e| SnowError::unexpected(e).named("UndefinedError"))?;
            nodes[i].rendered = Some(r);
        }
        status[i] = 2;
        Ok(())
    }
    for i in 0..nodes.len() {
        visit(i, &mut nodes, &mut status, host)?;
    }
    let mut final_ctx = IndexMap::new();
    for n in &nodes {
        if let Some(v) = &n.rendered
            && n.chain[0] != "fn"
        {
            add_to_context(&mut final_ctx, &n.chain, v.clone());
        }
    }
    map_render(&mut definition, &mut |v| {
        render::render(v, &final_ctx, host)
            .map_err(|e| SnowError::unexpected(e).named("UndefinedError"))
    })?;
    let model = build(
        &definition,
        Ctx {
            skip_templates: false,
            duplicated_run: true,
        },
        features,
    )?;
    Ok((model, definition))
}

/// `DefinitionManager(project_root, {ctx: {env: overrides}}).project_definition`.
pub fn load(
    root: &Path,
    env_overrides: &IndexMap<String, String>,
    features: &[String],
    host: &dyn Host,
    warn: &mut dyn FnMut(&str),
) -> Result<Option<Project>, SnowError> {
    let files = definition_files(root);
    let Some(raw) = merged(&files)? else {
        return Ok(None);
    };
    let (definition, ctx) = render_definition(raw, env_overrides, features, host, warn)?;
    Ok(Some(Project {
        root: root.to_path_buf(),
        definition,
        ctx,
        env_overrides: env_overrides.clone(),
    }))
}

/// `DefinitionManager.unrendered_project_definition` for the given files.
pub fn load_unrendered(files: &[PathBuf], features: &[String]) -> Result<Option<PyV>, SnowError> {
    let Some(raw) = merged(files)? else {
        return Ok(None);
    };
    build(&raw, Ctx::default(), features).map(Some)
}
