//! `definition_conversion.convert_project_definition_to_v2` (snowpark, streamlit,
//! native app; template rewriting inside native-app files is not ported).

use super::value::PyV;
use crate::errors::{Result, SnowError};
use indexmap::IndexMap;

type D = IndexMap<String, PyV>;

fn d(pairs: Vec<(&str, PyV)>) -> PyV {
    PyV::Dict(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn f(m: &PyV, k: &str) -> PyV {
    m.get(k).cloned().unwrap_or(PyV::None)
}

pub fn is_templated_name(n: &str) -> bool {
    (n.contains("<%") && n.contains("%>")) || (n.contains("<!") && n.contains("!>"))
}

fn template(t: &str) -> String {
    format!("<% {t} %>")
}

fn snowpark(sp: &PyV) -> Result<(D, D)> {
    let mut mapping = vec![("src", f(sp, "src"))];
    if f(sp, "project_name").truthy() {
        mapping.push(("dest", f(sp, "project_name")));
    }
    let stage = f(sp, "stage_name");
    let mut mixins = D::new();
    mixins.insert(
        "snowpark_shared".into(),
        d(vec![
            ("stage", stage.clone()),
            ("artifacts", PyV::List(vec![d(mapping)])),
        ]),
    );
    let mut entities = D::new();
    let procs = sp
        .get("procedures")
        .and_then(PyV::as_list)
        .cloned()
        .unwrap_or_default();
    let funcs = sp
        .get("functions")
        .and_then(PyV::as_list)
        .cloned()
        .unwrap_or_default();
    let n_procs = procs.len();
    for (i, e) in procs.into_iter().chain(funcs).enumerate() {
        let name = e.str_of("name").unwrap_or_default();
        let mut ident = vec![("name", PyV::s(name.clone()))];
        if !f(&e, "database").is_none() {
            ident.push(("database", f(&e, "database")));
        }
        if !f(&e, "schema_name").is_none() {
            ident.push(("schema", f(&e, "schema_name")));
        }
        let id = if is_templated_name(&name) {
            format!("snowpark_entity_{i}")
        } else {
            name
        };
        if entities.contains_key(&id) {
            return Err(SnowError::click(format!(
                "Entity with name {id} seems to be duplicated. Please rename it and try again."
            )));
        }
        let is_proc = i < n_procs;
        let mut v = vec![
            (
                "type",
                PyV::s(if is_proc { "procedure" } else { "function" }),
            ),
            ("stage", stage.clone()),
            ("handler", f(&e, "handler")),
            ("returns", f(&e, "returns")),
            ("signature", f(&e, "signature")),
            ("runtime", f(&e, "runtime")),
            (
                "external_access_integrations",
                f(&e, "external_access_integrations"),
            ),
            ("secrets", f(&e, "secrets")),
            ("imports", f(&e, "imports")),
            ("identifier", d(ident)),
            (
                "meta",
                d(vec![(
                    "use_mixins",
                    PyV::List(vec![PyV::s("snowpark_shared")]),
                )]),
            ),
        ];
        if is_proc {
            v.push(("execute_as_caller", f(&e, "execute_as_caller")));
        }
        entities.insert(id, d(v));
    }
    Ok((entities, mixins))
}

fn process_streamlit_file(name: &PyV, default: &str) -> Result<Option<String>> {
    match name {
        PyV::None => Ok(std::path::Path::new(default)
            .exists()
            .then(|| default.to_string())),
        v => {
            let s = v.py_str();
            if !std::path::Path::new(&s).exists() {
                return Err(SnowError::click(format!(
                    "Provided file {s} does not exist"
                )));
            }
            Ok(Some(s))
        }
    }
}

fn covered_by(file: &str, dir: &str) -> bool {
    let (file, dir) = (file.replace('\\', "/"), dir.replace('\\', "/"));
    file.starts_with(&format!("{}/", dir.trim_end_matches('/'))) || file == dir
}

fn streamlit(st: &PyV) -> Result<D> {
    let env = process_streamlit_file(&f(st, "env_file"), "environment.yml")?;
    let pages = process_streamlit_file(&f(st, "pages_dir"), "pages")?;
    let mut artifacts: Vec<PyV> = [Some(f(st, "main_file").py_str()), env, pages.clone()]
        .into_iter()
        .flatten()
        .map(PyV::Str)
        .collect();
    for a in st
        .get("additional_source_files")
        .and_then(PyV::as_list)
        .cloned()
        .unwrap_or_default()
    {
        let s = a.py_str();
        if pages.as_ref().is_some_and(|p| covered_by(&s, p)) {
            continue;
        }
        artifacts.push(PyV::Str(s));
    }
    let name = st.str_of("name").unwrap_or_default();
    let mut ident = vec![("name", PyV::s(name.clone()))];
    if f(st, "schema_name").truthy() {
        ident.push(("schema", f(st, "schema_name")));
    }
    if f(st, "database").truthy() {
        ident.push(("database", f(st, "database")));
    }
    let id = if is_templated_name(&name) {
        "streamlit_entity_1".to_string()
    } else {
        name
    };
    let mut out = D::new();
    out.insert(
        id,
        d(vec![
            ("type", PyV::s("streamlit")),
            ("identifier", d(ident)),
            ("title", f(st, "title")),
            ("comment", f(st, "comment")),
            ("query_warehouse", f(st, "query_warehouse")),
            ("main_file", PyV::Str(f(st, "main_file").py_str())),
            ("pages_dir", PyV::Str(f(st, "pages_dir").py_str())),
            ("stage", f(st, "stage")),
            ("artifacts", PyV::List(artifacts)),
        ]),
    );
    Ok(out)
}

fn defined(ctx: Option<&PyV>, path: &[&str]) -> bool {
    let Some(mut cur) = ctx else { return true };
    for k in path {
        match cur {
            PyV::Dict(m) => match m.get(*k) {
                Some(v) => cur = v,
                None => return false,
            },
            _ => return false,
        }
    }
    true
}

fn native_app(na: &PyV, ctx: Option<&PyV>) -> D {
    let meta = |o: &PyV| {
        let mut m = Vec::new();
        for k in ["role", "warehouse", "post_deploy"] {
            if f(o, k).truthy() {
                m.push((k, f(o, k)));
            }
        }
        m
    };
    let name = na.str_of("name").unwrap_or_default();
    let pkg = f(na, "package");
    let default_of = |model: &str| {
        super::model::model(model)
            .fields
            .iter()
            .find(|fd| fd.name == "name")
            .and_then(|fd| fd.default.as_ref())
            .and_then(|x| x.get("value"))
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };
    let pkg_id = match pkg.str_of("name").filter(|n| !n.is_empty()) {
        Some(n) if Some(&n) != default_of("PackageV11").as_ref() => n,
        _ => template(&format!(
            "fn.concat_ids('{name}', '_pkg_', fn.sanitize_id(fn.get_username('unknown_user')) | lower)"
        )),
    };
    let mut package = vec![
        ("type", PyV::s("application package")),
        ("identifier", PyV::Str(pkg_id)),
        ("artifacts", f(na, "artifacts")),
    ];
    for (k, out) in [
        ("bundle_root", "bundle_root"),
        ("generated_root", "generated_root"),
        ("deploy_root", "deploy_root"),
        ("source_stage", "stage"),
        ("scratch_stage", "scratch_stage"),
    ] {
        if defined(ctx, &["ctx", "native_app", k]) {
            package.push((out, f(na, k)));
        }
    }
    if !pkg.is_none() {
        if defined(ctx, &["ctx", "native_app", "package", "distribution"]) {
            package.push(("distribution", f(&pkg, "distribution")));
        }
        let m = meta(&pkg);
        if !m.is_empty() {
            package.push(("meta", d(m)));
        }
    }
    let app = f(na, "application");
    let app_id = match app.str_of("name").filter(|n| !n.is_empty()) {
        Some(n) if Some(&n) != default_of("ApplicationV11").as_ref() => n,
        _ => template(&format!(
            "fn.concat_ids('{name}', '_', fn.sanitize_id(fn.get_username('unknown_user')) | lower)"
        )),
    };
    let mut appd = vec![
        ("type", PyV::s("application")),
        ("identifier", PyV::Str(app_id)),
        ("from", d(vec![("target", PyV::s("pkg"))])),
    ];
    if !app.is_none() {
        let m = meta(&app);
        if !m.is_empty() {
            appd.push(("meta", d(m)));
        }
        if defined(ctx, &["ctx", "native_app", "application", "debug"]) {
            appd.push(("debug", f(&app, "debug")));
        }
    }
    let mut out = D::new();
    out.insert("pkg".into(), d(package));
    out.insert("app".into(), d(appd));
    out
}

/// Data for `ProjectDefinitionV2(**data)` from a v1 definition model.
pub fn to_v2_data(
    v1: &PyV,
    accept_templates: bool,
    ctx: Option<&PyV>,
    warn: &mut dyn FnMut(&str),
) -> Result<PyV> {
    let version = v1.str_of("definition_version").unwrap_or_default();
    if super::definition::version_ge(&version, "2") {
        return Err(SnowError::click(
            "Project definition is already at version 2.",
        ));
    }
    if v1.py_repr().contains("<%") {
        if !accept_templates {
            return Err(SnowError::click(
                "Project definition contains templates. They may not be migrated correctly, and require manual migration.You can try again with --accept-templates  option, to attempt automatic migration.",
            ));
        }
        warn(
            "Your V1 definition contains templates. We cannot guarantee the correctness of the migration.",
        );
    }
    let (sp_ents, mixins) = match v1.get_some("snowpark") {
        Some(sp) => {
            let (e, m) = snowpark(sp)?;
            (e, Some(m))
        }
        None => (D::new(), None),
    };
    let st_ents = match v1.get_some("streamlit") {
        Some(st) => streamlit(st)?,
        None => D::new(),
    };
    let na_ents = match v1.get_some("native_app") {
        Some(na) => native_app(na, ctx),
        None => D::new(),
    };
    for (types, a, b) in [
        ("streamlit and snowpark", &st_ents, &sp_ents),
        ("streamlit and native app", &st_ents, &na_ents),
        ("native app and snowpark", &na_ents, &sp_ents),
    ] {
        if a.keys().any(|k| b.contains_key(k)) {
            return Err(SnowError::click(format!(
                "In your project, {types} entities share the same name. Please rename them and try again."
            )));
        }
    }
    let mut entities = sp_ents;
    entities.extend(st_ents);
    entities.extend(na_ents);
    let mut data = vec![
        ("definition_version", PyV::s("2")),
        ("entities", PyV::Dict(entities)),
        ("mixins", mixins.map(PyV::Dict).unwrap_or(PyV::None)),
    ];
    if let Some(env) = v1.get_some("env").filter(|e| e.truthy()) {
        data.push(("env", env.clone()));
    }
    Ok(d(data))
}
