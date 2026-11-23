//! The `@field_validator` / `@model_validator` functions of upstream's
//! project-definition models (the template-skipping `wrap` validators that
//! `UpdatableModel` adds are applied by the engine for every field).

use super::model::{Engine, Loc, VErr, VResult, value_error};
use super::value::{Model, PyV};
use crate::identifiers as ids;
use indexmap::IndexMap;

type Data = IndexMap<String, PyV>;

/// A non-`ValueError` raised from a validator (pydantic does not catch it).
pub fn fatal(kind: &'static str, msg: impl Into<String>) -> Vec<VErr> {
    vec![VErr {
        kind,
        loc: vec![],
        msg: msg.into(),
        input: PyV::None,
    }]
}

fn reject_backslashes(
    field: &str,
    values: &[&str],
    loc: &[Loc],
    input: &PyV,
) -> Result<(), Vec<VErr>> {
    for v in values {
        if v.contains('\\') {
            return Err(value_error(
                loc,
                &format!(
                    "{field} must not contain a backslash, got {}. Remove it from snowflake.yml (stage paths use forward slashes).",
                    snow_click::py_repr(v)
                ),
                input,
            ));
        }
    }
    Ok(())
}

fn strs(v: &PyV) -> Vec<&str> {
    v.as_list()
        .map(|l| l.iter().filter_map(PyV::as_str).collect())
        .unwrap_or_default()
}

fn has_glob_magic(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

fn append_test_resource_suffix(identifier: &str) -> String {
    let suffix = std::env::var("SNOWFLAKE_CLI_TEST_RESOURCE_SUFFIX").unwrap_or_default();
    if ids::identifier_to_str(identifier).ends_with(&ids::identifier_to_str(&suffix)) {
        return identifier.to_string();
    }
    if ids::is_valid_quoted_identifier(identifier) || ids::is_valid_quoted_identifier(&suffix) {
        return ids::concat_identifiers(&[identifier, &suffix]);
    }
    format!("{identifier}{suffix}")
}

fn path_mappings(e: &Engine, v: PyV, loc: &mut Vec<Loc>) -> VResult {
    let mut out = Vec::new();
    for a in v.as_list().cloned().unwrap_or_default() {
        match a {
            PyV::Model(_) => out.push(a),
            other => {
                let mut d = Data::new();
                d.insert("src".into(), other);
                out.push(e.construct("PathMapping", d, loc)?);
            }
        }
    }
    Ok(PyV::List(out))
}

pub(crate) fn field_before(e: &Engine, func: &str, v: PyV, loc: &mut [Loc]) -> VResult {
    let s = v.as_str().map(str::to_string);
    Ok(match func {
        "ensure_use_mixins_is_a_list" => match v {
            PyV::Str(x) => PyV::List(vec![PyV::Str(x)]),
            other => other,
        },
        "arguments_validator" => {
            if e.ctx.duplicated_run {
                return Ok(v);
            }
            let mut v = v;
            if let PyV::Dict(args) = &mut v {
                for a in args.values_mut() {
                    if let PyV::Dict(ad) = a {
                        for x in ad.values_mut() {
                            if let PyV::Str(sv) = x {
                                let low = sv.to_lowercase();
                                *x = if low == "true" || low == "false" {
                                    PyV::Bool(low == "true")
                                } else if let Ok(i) = sv.trim().parse::<i64>() {
                                    PyV::Int(i)
                                } else {
                                    PyV::Str(sv.clone())
                                };
                            }
                        }
                    }
                }
            }
            v
        }
        "canonicalize_execute_as" => match s {
            Some(x) => PyV::Str(
                x.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_uppercase(),
            ),
            None => v,
        },
        "strip_app_warehouse" | "strip_compute_pool" => match s {
            Some(x) => {
                let t = x.trim().to_string();
                let name = if func == "strip_app_warehouse" {
                    "app_warehouse"
                } else {
                    "compute_pool"
                };
                if t.is_empty() {
                    return Err(value_error(loc, &format!("{name} must not be empty"), &v));
                }
                reject_backslashes(name, &[&t], loc, &v)?;
                PyV::Str(t)
            }
            None => v,
        },
        "canonicalize_runtime_name" => match s {
            Some(x) => {
                reject_backslashes("runtime_name", &[&x], loc, &v)?;
                let n = x.trim().to_uppercase();
                let known = [
                    "SYSTEM$ST_CONTAINER_RUNTIME_PY3_11",
                    "SYSTEM$WAREHOUSE_RUNTIME",
                ];
                let family = n
                    .strip_prefix("SYSTEM$ST_CONTAINER_RUNTIME")
                    .is_some_and(|r| {
                        r.chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    });
                if known.contains(&n.as_str()) || family {
                    PyV::Str(n)
                } else {
                    let mut sorted = known.to_vec();
                    sorted.sort();
                    return Err(value_error(
                        loc,
                        &format!(
                            "Unknown runtime_name '{x}'. Supported values are: {} (or SYSTEM$ST_CONTAINER_RUNTIME followed by letters, digits, or '_')",
                            sorted.join(", ")
                        ),
                        &v,
                    ));
                }
            }
            None => v,
        },
        "_validate_compute_pool" => {
            if v.is_none() || s.as_deref() == Some("null") {
                PyV::None
            } else {
                v
            }
        }
        "_validate_eai" => match &v {
            PyV::None => PyV::None,
            PyV::Str(x) if x == "null" => PyV::None,
            PyV::Str(x) => PyV::Dict(
                [("name".to_string(), PyV::Str(x.clone()))]
                    .into_iter()
                    .collect(),
            ),
            _ => v,
        },
        "_validate_code_storage" => match &v {
            PyV::None => PyV::None,
            PyV::Str(x) if x == "null" => PyV::None,
            PyV::Str(x) => match ids::Fqn::from_string(x) {
                Ok(f) => {
                    let mut d = Data::new();
                    d.insert("name".into(), PyV::Str(f.name.clone()));
                    if let Some(db) = &f.database {
                        d.insert("database".into(), PyV::Str(db.clone()));
                    }
                    if let Some(sc) = &f.schema {
                        d.insert("schema".into(), PyV::Str(sc.clone()));
                    }
                    PyV::Dict(d)
                }
                Err(err) => return Err(fatal("fatal:click", err.message())),
            },
            _ => v,
        },
        "_validate_spcs_test_project_type" => match &v {
            PyV::None => PyV::None,
            PyV::Str(x) if x == "null" => PyV::None,
            PyV::Str(x) => PyV::Str(x.trim().to_string()),
            _ => {
                return Err(value_error(
                    loc,
                    "spcs_test_project_type must be a string or null",
                    &v,
                ));
            }
        },
        // app.yml (Snowflake App Runtime).
        "_coerce_scalar_value" => match v {
            PyV::Bool(b) => PyV::s(if b { "true" } else { "false" }),
            PyV::Int(i) => PyV::Str(i.to_string()),
            PyV::Float(f) => PyV::Str(crate::pyfmt::float_repr(f)),
            other => other,
        },
        "_normalize_authentication_mode" => match &v {
            PyV::None => PyV::None,
            PyV::Str(x) => {
                let n = x.trim().to_lowercase();
                if n != "password" && n != "token" {
                    return Err(value_error(
                        loc,
                        "authentication_mode must be one of: password, token",
                        &v,
                    ));
                }
                PyV::Str(n)
            }
            _ => return Err(value_error(loc, "authentication_mode must be a string", &v)),
        },
        "_validate_compute_resource" => match &v {
            PyV::None => PyV::None,
            PyV::Str(x) if x == "null" => PyV::None,
            PyV::Str(x) => {
                let n = x.trim().to_uppercase();
                if n != "SERVERLESS" && n != "MANAGED_COMPUTE_POOL" {
                    return Err(value_error(
                        loc,
                        "compute_resource must be one of: SERVERLESS, MANAGED_COMPUTE_POOL",
                        &v,
                    ));
                }
                PyV::Str(n)
            }
            _ => {
                return Err(value_error(
                    loc,
                    "compute_resource must be a string or null",
                    &v,
                ));
            }
        },
        "_validate_runtime" => {
            let entries: Vec<PyV> = match &v {
                PyV::None => return Ok(PyV::s("node")),
                PyV::Str(x) if x == "null" => return Ok(PyV::s("node")),
                PyV::Str(_) => vec![v.clone()],
                PyV::List(l) if !l.is_empty() => l.clone(),
                _ => {
                    return Err(value_error(
                        loc,
                        "runtime must be a string, a non-empty list, or null",
                        &v,
                    ));
                }
            };
            let mut names = std::collections::BTreeSet::new();
            for e in &entries {
                let PyV::Str(x) = e else {
                    return Err(value_error(loc, "runtime must be a list of strings", &v));
                };
                let n = x.trim().to_lowercase();
                if n != "python" && n != "node" {
                    return Err(value_error(
                        loc,
                        "runtime must be one of: python, node (or a list of them)",
                        &v,
                    ));
                }
                names.insert(n);
            }
            PyV::Str(names.into_iter().collect::<Vec<_>>().join(","))
        }
        "_validate_streamlit" => match &v {
            PyV::None => PyV::Bool(false),
            PyV::Bool(_) => v,
            _ => {
                return Err(value_error(
                    loc,
                    "streamlit must be a boolean (true or false) or null",
                    &v,
                ));
            }
        },
        "_coerce_empty_targets" => match v {
            PyV::Dict(d) => PyV::Dict(
                d.into_iter()
                    .map(|(k, x)| {
                        (
                            k,
                            if x.is_none() {
                                PyV::Dict(Data::new())
                            } else {
                                x
                            },
                        )
                    })
                    .collect(),
            ),
            other => other,
        },
        other => unreachable!("field validator {other} is not ported"),
    })
}

pub(crate) fn field_after(
    e: &Engine,
    func: &str,
    model: &str,
    field: &str,
    v: PyV,
    raw: &PyV,
    loc: &mut Vec<Loc>,
) -> VResult {
    Ok(match func {
        "_is_supported_version" => {
            let ver = v.py_str();
            let versions = &super::model::MODELS.versions;
            if !versions.contains_key(&ver) {
                return Err(value_error(
                    loc,
                    &format!(
                        "Version {ver} is not supported. Supported versions: {}",
                        versions.keys().cloned().collect::<Vec<_>>().join(", ")
                    ),
                    raw,
                ));
            }
            PyV::Str(ver)
        }
        "transform_artifacts" => path_mappings(e, v, loc)?,
        "_convert_artifacts" if field == "comment" => match &v {
            PyV::Str(c) if !c.is_empty() => PyV::Str(ids::to_string_literal(c)),
            _ => v,
        },
        "_convert_artifacts" => {
            for a in v.as_list().cloned().unwrap_or_default() {
                let src = match &a {
                    PyV::Str(s) => Some(s.clone()),
                    PyV::Model(m) => m
                        .fields
                        .get("src")
                        .and_then(|x| x.as_str().map(str::to_string)),
                    _ => None,
                };
                if src.is_some_and(|s| has_glob_magic(&s))
                    && !e.feature("ENABLE_SNOWPARK_GLOB_SUPPORT")
                {
                    return Err(value_error(
                        loc,
                        "If you want to use glob patterns in artifacts, you need to enable the Snowpark new build feature flag (enable_snowpark_glob_support=true)",
                        raw,
                    ));
                }
            }
            path_mappings(e, v, loc)?
        }
        "transform_processors" => {
            let mut out = Vec::new();
            for p in v.as_list().cloned().unwrap_or_default() {
                match p {
                    PyV::Str(name) => {
                        let mut d = Data::new();
                        d.insert("name".into(), PyV::Str(name));
                        out.push(e.construct("ProcessorMapping", d, loc)?);
                    }
                    PyV::Dict(d) => out.push(e.construct("ProcessorMapping", d, loc)?),
                    other => out.push(other),
                }
            }
            PyV::List(out)
        }
        "_imports_have_no_backslash" => {
            reject_backslashes("imports", &strs(&v), loc, raw)?;
            v
        }
        "_integrations_have_no_backslash" => {
            reject_backslashes("external_access_integrations", &strs(&v), loc, raw)?;
            v
        }
        "_secrets_have_no_backslash" => {
            if let PyV::Dict(d) = &v {
                for (k, x) in d {
                    reject_backslashes("secrets", &[k, x.as_str().unwrap_or_default()], loc, raw)?;
                }
            }
            v
        }
        "convert_runtime" => match v {
            PyV::Float(f) => PyV::Str(crate::pyfmt::float_repr(f)),
            other => other,
        },
        "validate_sql_script_path" => {
            let s = v.as_str().unwrap_or_default();
            if s.is_empty() {
                return Err(value_error(loc, "sql_script must not be empty", raw));
            }
            if s.contains('\0') {
                return Err(value_error(
                    loc,
                    "sql_script must not contain null bytes",
                    raw,
                ));
            }
            let win_abs = {
                let b = s.as_bytes();
                (b.len() >= 3
                    && b[0].is_ascii_alphabetic()
                    && b[1] == b':'
                    && (b[2] == b'\\' || b[2] == b'/'))
                    || s.starts_with("\\\\")
            };
            if s.starts_with('/') || win_abs {
                return Err(value_error(
                    loc,
                    &format!(
                        "sql_script must be a relative path within the project root, got absolute path: {}",
                        snow_click::py_repr(s)
                    ),
                    raw,
                ));
            }
            if s.split(['/', '\\']).any(|p| p == "..") {
                return Err(value_error(
                    loc,
                    &format!(
                        "sql_script must not contain parent directory references ('..'), got: {}",
                        snow_click::py_repr(s)
                    ),
                    raw,
                ));
            }
            v
        }
        "validate_privilege" => {
            let p = v.as_str().unwrap_or_default();
            if !p.split(' ').all(ids::is_valid_unquoted_identifier) {
                return Err(value_error(
                    loc,
                    &format!(
                        "Invalid privilege '{p}'. Use a SQL privilege name, such as USAGE or IMPORTED PRIVILEGES, not a full statement."
                    ),
                    raw,
                ));
            }
            v
        }
        "strip_grantee" => match v {
            PyV::Str(s) => PyV::Str(s.trim().to_string()),
            other => other,
        },
        "strip_cron_exprs" => {
            let stripped: Vec<String> = strs(&v).iter().map(|s| s.trim().to_string()).collect();
            if stripped.iter().any(String::is_empty) {
                return Err(value_error(
                    loc,
                    "cron_exprs must not contain an empty expression",
                    raw,
                ));
            }
            if stripped
                .iter()
                .any(|s| s.contains(['\r', '\n', '\'', '\\']))
            {
                return Err(value_error(
                    loc,
                    "cron_exprs must not contain a line break, quote, or backslash",
                    raw,
                ));
            }
            PyV::List(stripped.into_iter().map(PyV::Str).collect())
        }
        "strip_timezone" => match &v {
            PyV::Str(s) => {
                let t = s.trim();
                if t.is_empty()
                    || t.chars()
                        .any(|c| c.is_whitespace() || c == '\'' || c == '\\')
                {
                    return Err(value_error(
                        loc,
                        "timezone must be one IANA name, such as UTC or America/Los_Angeles",
                        raw,
                    ));
                }
                PyV::Str(t.to_string())
            }
            _ => v,
        },
        "strip_artifact_repositories" => match &v {
            PyV::List(_) => {
                let stripped: Vec<String> = strs(&v).iter().map(|s| s.trim().to_string()).collect();
                if stripped.iter().any(String::is_empty) {
                    return Err(value_error(
                        loc,
                        "artifact_repositories must not contain an empty name",
                        raw,
                    ));
                }
                let refs: Vec<&str> = stripped.iter().map(String::as_str).collect();
                reject_backslashes("artifact_repositories", &refs, loc, raw)?;
                PyV::List(stripped.into_iter().map(PyV::Str).collect())
            }
            _ => v,
        },
        "ensure_app_roles_is_a_set" => match v {
            PyV::Str(s) => PyV::List(vec![PyV::Str(s)]),
            other => other,
        },
        "verify_children_behind_flag" => {
            if v.truthy() && !e.feature("ENABLE_NATIVE_APP_CHILDREN") {
                return Err(fatal(
                    "fatal:unexpected",
                    "Application package children are not supported yet",
                ));
            }
            v
        }
        "append_test_resource_suffix_to_identifier" => match v {
            PyV::Str(s) => PyV::Str(append_test_resource_suffix(&s)),
            PyV::Model(mut m) => {
                let n = m
                    .fields
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                m.fields
                    .insert("name".into(), PyV::Str(append_test_resource_suffix(&n)));
                PyV::Model(m)
            }
            other => other,
        },
        "append_test_resource_suffix_to_name" => match v {
            PyV::Str(s) => PyV::Str(append_test_resource_suffix(&s)),
            other => other,
        },
        "validate_stage" => {
            let s = v.as_str().unwrap_or_default();
            if !s.is_empty()
                && ids::is_valid_object_name(s, 1, true)
                && !ids::is_valid_identifier(s)
            {
                v
            } else {
                return Err(value_error(
                    loc,
                    "Incorrect value for stage of native_app. Expected format for this field is {schema_name}.{stage_name} ",
                    raw,
                ));
            }
        }
        "validate_source_stage" => {
            let s = v.as_str().unwrap_or_default();
            if !s.is_empty()
                && ids::is_valid_object_name(s, 1, true)
                && !ids::is_valid_identifier(s)
            {
                v
            } else {
                return Err(value_error(
                    loc,
                    "Incorrect value for source_stage value of native_app",
                    raw,
                ));
            }
        }
        "validate_scratch_stage" => {
            let s = v.as_str().unwrap_or_default();
            if !s.is_empty() && ids::is_valid_object_name(s, 1, true) {
                v
            } else if model == "ApplicationPackageEntityModel" {
                return Err(value_error(
                    loc,
                    "Incorrect value for scratch_stage of native_app. Expected a single identifier or {schema_name}.{stage_name}",
                    raw,
                ));
            } else {
                return Err(value_error(
                    loc,
                    "Incorrect value for scratch_stage value of native_app. Expected a single identifier or schema.stage",
                    raw,
                ));
            }
        }
        "validate_scripts" => {
            let l = v.as_list().cloned().unwrap_or_default();
            let mut seen = Vec::new();
            for x in &l {
                if seen.contains(x) {
                    return Err(value_error(
                        loc,
                        "package.scripts field should contain unique values. Check the list for duplicates and try again",
                        raw,
                    ));
                }
                seen.push(x.clone());
            }
            v
        }
        "validate_optional_shared_events" => {
            if let PyV::List(l) = &v
                && l.iter().any(|x| !x.truthy())
            {
                return Err(fatal(
                    "fatal:click",
                    "Events listed in 'telemetry.optional_shared_events' must not be blank",
                ));
            }
            v
        }
        "validate_entities" => {
            if let PyV::Dict(ents) = &v {
                for ent in ents.values() {
                    validate_single_entity(ent, ents, loc, raw)?;
                }
            }
            v
        }
        other => unreachable!("field validator {other} is not ported"),
    })
}

fn class_of(m: &PyV) -> &str {
    m.as_model().map(|m| m.name.as_str()).unwrap_or("")
}

fn validate_single_entity(
    ent: &PyV,
    ents: &IndexMap<String, PyV>,
    loc: &[Loc],
    raw: &PyV,
) -> Result<(), Vec<VErr>> {
    let ty = ent.str_of("type").unwrap_or_default();
    let check = |key: &str, allowed: &[&str]| -> Result<(), Vec<VErr>> {
        let Some(target) = ents.get(key) else {
            return Err(value_error(loc, &format!("No such target: {key}"), raw));
        };
        let actual = class_of(target);
        if allowed.len() > 1 {
            if !allowed.contains(&actual) {
                return Err(value_error(
                    loc,
                    &format!(
                        "Target type mismatch. Expected one of [{}], got {actual}",
                        allowed.join(", ")
                    ),
                    raw,
                ));
            }
        } else if allowed.first().is_some_and(|a| *a != actual) {
            return Err(value_error(
                loc,
                &format!(
                    "Target type mismatch. Expected {}, got {actual}",
                    allowed[0]
                ),
                raw,
            ));
        }
        Ok(())
    };
    if ty == "application" {
        if let Some(from) = ent.get("from_").filter(|f| f.as_model().is_some()) {
            check(
                &from.str_of("target").unwrap_or_default(),
                &["ApplicationPackageEntityModel"],
            )?;
        }
    } else if ty == "application package"
        && let Some(PyV::List(children)) = ent.get("children")
    {
        for c in children {
            check(
                &c.str_of("target").unwrap_or_default(),
                &[
                    "StreamlitEntityModel",
                    "FunctionEntityModel",
                    "ProcedureEntityModel",
                ],
            )?;
        }
    }
    Ok(())
}

pub(crate) fn model_before(
    e: &Engine,
    func: &str,
    data: Data,
    loc: &mut [Loc],
) -> Result<Data, Vec<VErr>> {
    match func {
        "apply_mixins" => apply_mixins(e, data, loc),
        "check_artifact_repository" => {
            let has = |k: &str| data.get(k).is_some_and(PyV::truthy);
            let input = PyV::Dict(data.clone());
            if has("artifact_repository_packages") && has("packages") {
                return Err(value_error(
                    loc,
                    "You cannot specify both artifact_repository_packages and packages.",
                    &input,
                ));
            }
            if (has("artifact_repository_packages") || has("packages"))
                && !has("artifact_repository")
            {
                return Err(value_error(
                    loc,
                    "You specified packages / artifact_repository_packages without setting artifact_repository.",
                    &input,
                ));
            }
            Ok(data)
        }
        "_default_node_phases" => {
            let declared: Vec<PyV> = match data.get("runtime") {
                Some(PyV::List(l)) => l.clone(),
                Some(x) => vec![x.clone()],
                None => vec![PyV::None],
            };
            if declared
                .iter()
                .any(|r| matches!(r, PyV::Str(x) if x.trim().to_lowercase() == "python"))
            {
                return Ok(data);
            }
            let cmds = |c: &[&[&str]]| {
                PyV::List(
                    c.iter()
                        .map(|x| PyV::List(x.iter().map(|y| PyV::s(*y)).collect()))
                        .collect(),
                )
            };
            let one = |c: &[&str]| PyV::List(c.iter().map(|y| PyV::s(*y)).collect());
            let mut data = data;
            let defaults = [
                ("install", ("commands", cmds(&[&["npm", "ci"]]))),
                ("build", ("commands", cmds(&[&["npm", "run", "build"]]))),
                ("run", ("command", one(&["npm", "start"]))),
                ("dev", ("command", one(&["npm", "run", "dev"]))),
            ];
            for (k, (inner, val)) in defaults {
                if data.get(k).is_none_or(PyV::is_none) {
                    let mut d = Data::new();
                    d.insert(inner.to_string(), val);
                    data.insert(k.to_string(), PyV::Dict(d));
                }
            }
            Ok(data)
        }
        "_reject_legacy_targets_default" => {
            if let Some(PyV::Dict(t)) = data.get("targets")
                && t.contains_key("default")
            {
                return Err(value_error(
                    loc,
                    "'targets.default' is no longer supported; set a top-level 'default_target' instead (e.g. 'default_target: dev').",
                    &PyV::Dict(data.clone()),
                ));
            }
            Ok(data)
        }
        _ => Ok(data),
    }
}

pub(crate) fn model_after(func: &str, v: PyV, raw: &PyV, loc: &mut [Loc]) -> VResult {
    let m: &Model = v.as_model().expect("model");
    let f = |k: &str| m.fields.get(k).cloned().unwrap_or(PyV::None);
    match func {
        "_reject_conflicting_code_storage" => {
            if !f("code_stage").is_none() && !f("code_workspace").is_none() {
                return Err(value_error(
                    loc,
                    "Set only one of 'code_stage' or 'code_workspace'; they are mutually exclusive.",
                    raw,
                ));
            }
            Ok(v)
        }
        "_reject_streamlit_with_node" => {
            let python = f("runtime").py_str().split(',').any(|r| r == "python");
            if matches!(f("streamlit"), PyV::Bool(true)) && !python {
                return Err(value_error(
                    loc,
                    "'streamlit: true' requires 'runtime: python' (or a runtime list that includes python); runtime defaults to 'node'.",
                    raw,
                ));
            }
            Ok(v)
        }
        "validate_dependencies" => {
            let mut missing: IndexMap<String, Vec<String>> = IndexMap::new();
            if let PyV::Dict(ents) = f("entities") {
                for (id, ent) in &ents {
                    if let Some(meta) = ent.get_some("meta")
                        && let Some(PyV::List(deps)) = meta.get("depends_on")
                    {
                        for d in deps {
                            let d = d.py_str();
                            if !ents.contains_key(&d) {
                                missing.entry(id.clone()).or_default().push(d);
                            }
                        }
                    }
                }
            }
            if !missing.is_empty() {
                let msg: String = missing
                    .iter()
                    .map(|(id, deps)| {
                        format!(
                            "\n Entity {id} depends on non-existing entities: {}",
                            deps.join(", ")
                        )
                    })
                    .collect();
                return Err(value_error(loc, &msg, raw));
            }
            Ok(v)
        }
        "validate_entities_identifiers" => {
            if let PyV::Dict(ents) = f("entities") {
                for (id, ent) in &ents {
                    if id.is_empty() && !ent.get("identifier").is_some_and(PyV::truthy) {
                        return Err(value_error(loc, "Missing entity identifier", raw));
                    }
                }
            }
            Ok(v)
        }
        "validate_grantee" => {
            if f("role").truthy() == f("user").truthy() {
                return Err(value_error(
                    loc,
                    "Each grant needs exactly one of role or user.",
                    raw,
                ));
            }
            Ok(v)
        }
        "validate_no_scripts_and_post_deploy" => {
            if f("scripts").truthy() && f("post_deploy").truthy() {
                return Err(value_error(
                    loc,
                    "package.scripts and package.post_deploy fields cannot be used together. We recommend using package.post_deploy for all post package deploy scripts",
                    raw,
                ));
            }
            Ok(v)
        }
        "validate_telemetry_event_sharing" => {
            if f("optional_shared_events").truthy() && !f("share_mandatory_events").truthy() {
                return Err(fatal(
                    "fatal:click",
                    "'telemetry.share_mandatory_events' must be set to 'true' when sharing optional events through 'telemetry.optional_shared_events'.",
                ));
            }
            Ok(v)
        }
        "_validate_single_code_storage" => {
            if !f("code_stage").is_none() && !f("code_workspace").is_none() {
                return Err(value_error(
                    loc,
                    "Specify either code_stage or code_workspace, not both.",
                    raw,
                ));
            }
            Ok(v)
        }
        "validate_container_setup" => {
            let (cp, rn, rev) = (
                f("compute_pool").truthy(),
                f("runtime_name").truthy(),
                f("runtime_environment_version").truthy(),
            );
            if cp && !rn {
                return Err(value_error(
                    loc,
                    "compute_pool is specified without runtime_name",
                    raw,
                ));
            }
            if rn && !cp {
                return Err(value_error(
                    loc,
                    "runtime_name is specified without compute_pool",
                    raw,
                ));
            }
            if cp && rev {
                return Err(value_error(
                    loc,
                    "runtime_environment_version is only applicable for notebooks using warehouse, not compute pool",
                    raw,
                ));
            }
            Ok(v)
        }
        "validate_notebook_file" => {
            let nb = f("notebook_file").py_str();
            if !std::path::Path::new(&nb).exists() {
                return Err(value_error(
                    loc,
                    &format!("Notebook file {nb} does not exist"),
                    raw,
                ));
            }
            if !nb.to_lowercase().ends_with(".ipynb") {
                return Err(fatal(
                    "fatal:click",
                    format!(
                        "Cannot extract notebook file name from path={}",
                        snow_click::py_repr(&nb)
                    ),
                ));
            }
            Ok(v)
        }
        _ => Ok(v),
    }
}

fn merge_data(left: Option<&PyV>, right: &PyV) -> Result<PyV, String> {
    if let PyV::Override(d) = right {
        return Ok((**d).clone());
    }
    let Some(left) = left.filter(|l| !l.is_none()) else {
        return Ok(right.clone());
    };
    match (left, right) {
        (PyV::Dict(l), PyV::Dict(r)) => {
            let mut data = l.clone();
            for (k, v) in r {
                let merged = merge_data(data.get(k), v)?;
                data.insert(k.clone(), merged);
            }
            Ok(PyV::Dict(data))
        }
        (PyV::List(l), PyV::List(r)) => {
            let mut out = l.clone();
            for x in r {
                if l.iter().all(|y| y != x) {
                    out.push(x.clone());
                }
            }
            Ok(PyV::List(out))
        }
        _ if left.type_name() != right.type_name() => Err(format!(
            "Could not merge <class '{}'> and <class '{}'>.",
            right.type_name(),
            left.type_name()
        )),
        _ => Ok(right.clone()),
    }
}

fn apply_mixins(e: &Engine, mut data: Data, loc: &mut [Loc]) -> Result<Data, Vec<VErr>> {
    if !data.contains_key("mixins") || !data.contains_key("entities") || e.ctx.duplicated_run {
        return Ok(data);
    }
    let input = PyV::Dict(data.clone());
    let fail = |m: String| value_error(loc, &m, &input);
    let mixin_defs = data.get("mixins").cloned().unwrap_or(PyV::None);
    let Some(PyV::Dict(mut entities)) = data.get("entities").cloned() else {
        return Ok(data);
    };
    for (id, entity) in entities.iter_mut() {
        let mixins: Vec<String> = match entity.get("meta").and_then(|m| m.get("use_mixins")) {
            None | Some(PyV::None) => vec![],
            Some(PyV::Str(s)) => vec![s.clone()],
            Some(PyV::List(l)) => l.iter().map(PyV::py_str).collect(),
            Some(other) => vec![other.py_str()],
        };
        let defs = mixin_defs.as_dict().cloned().unwrap_or_default();
        for m in &mixins {
            if !defs.contains_key(m) {
                return Err(fail(format!("Mixin {m} not defined")));
            }
        }
        let mut merged = PyV::Dict(Data::new());
        for m in &mixins {
            merged = merge_data(Some(&merged), &defs[m]).map_err(&fail)?;
        }
        let ety = entity.get("type").map(PyV::py_str);
        let allowed: Vec<String> = match &ety {
            None => return Err(fail("Entity is missing type declaration.".into())),
            Some(t) => super::model::entity_model_for_type(t)
                .map(|m| {
                    super::model::model(m)
                        .fields
                        .iter()
                        .map(|f| f.name.clone())
                        .collect()
                })
                .unwrap_or_default(),
        };
        if let PyV::Dict(md) = &merged {
            for (k, ov) in md {
                if !allowed.contains(k) {
                    return Err(fail(format!(
                        "Unsupported key '{k}' for entity {id} of type {} ",
                        ety.clone().unwrap_or_default()
                    )));
                }
                if let Some(ev) = entity.get(k)
                    && !ev.is_none()
                    && !matches!(ev, PyV::Override(_))
                    && ev.type_name() != ov.type_name()
                {
                    return Err(fail(format!(
                        "Value from mixins for property {k} is of type '{}' while entity {id} expects value of type '{}'",
                        ov.type_name(),
                        ev.type_name()
                    )));
                }
            }
        }
        *entity = merge_data(Some(&merged), entity).map_err(&fail)?;
    }
    data.insert("entities".into(), PyV::Dict(entities));
    Ok(data)
}
