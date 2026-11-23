//! `snow object` and the list/describe/drop aliases other groups reuse.

use crate::common::parse_key_value_variables;
use snow_api::identifiers::{Fqn, is_valid_identifier, to_string_literal};
use snow_api::output::Output;
use snow_api::{Cli, Result, SnowError};
use snow_connector::Cursor;

/// (cli name, sf name, sf plural name).
pub const OBJECT_TO_NAMES: &[(&str, &str, &str)] = &[
    ("compute-pool", "compute pool", "compute pools"),
    ("dbt-project", "dbt project", "dbt projects"),
    ("dcm", "DCM Project", "DCM Projects"),
    ("database", "database", "databases"),
    ("function", "function", "functions"),
    ("integration", "integration", "integrations"),
    (
        "external-access-integration",
        "external access integration",
        "external access integrations",
    ),
    ("network-rule", "network rule", "network rules"),
    ("notebook", "notebook", "notebooks"),
    ("procedure", "procedure", "procedures"),
    ("role", "role", "roles"),
    ("schema", "schema", "schemas"),
    ("service", "service", "services"),
    ("secret", "secret", "secrets"),
    ("stage", "stage", "stages"),
    ("stream", "stream", "streams"),
    ("streamlit", "streamlit", "streamlits"),
    ("table", "table", "tables"),
    ("task", "task", "tasks"),
    ("user", "user", "users"),
    ("warehouse", "warehouse", "warehouses"),
    ("view", "view", "views"),
    ("image-repository", "image repository", "image repositories"),
    ("git-repository", "git repository", "git repositories"),
    ("application", "application", "applications"),
    (
        "application-package",
        "application package",
        "application packages",
    ),
];

const VALID_SCOPES: &[&str] = &["database", "schema", "compute-pool"];
const INTEGRATION_OBJECTS: &[&str] = &["external-access-integration", "integration"];

fn names(object_type: &str) -> Result<(&'static str, &'static str)> {
    let t = object_type.to_lowercase();
    OBJECT_TO_NAMES
        .iter()
        .find(|(c, _, _)| *c == t)
        .map(|(_, n, p)| (*n, *p))
        .ok_or_else(|| SnowError::click(format!("Object of type {t} is not supported.")))
}

pub fn sf_name(object_type: &str) -> &'static str {
    names(object_type).map(|n| n.0).unwrap_or("object")
}

pub type Scope = (Option<String>, Option<String>);

pub fn scope_of(cli: &Cli) -> Scope {
    let v = cli.list("scope");
    let l = cli.param("scope").as_list().to_vec();
    let get = |i: usize| l.get(i).and_then(|x| x.as_str()).map(str::to_string);
    let _ = v;
    (get(0), get(1))
}

fn effective_scope(scope: Scope, in_account: bool, object_type: &str) -> Result<Scope> {
    if in_account && scope.0.is_some() {
        return Err(SnowError::incompatible(&["--in-account", "--in"]));
    }
    if in_account {
        if INTEGRATION_OBJECTS.contains(&object_type) {
            return Ok((None, None));
        }
        return Ok((Some("account".into()), None));
    }
    Ok(scope)
}

fn scope_validate(object_type: &str, scope: &Scope) -> Result<()> {
    let Some(st) = &scope.0 else { return Ok(()) };
    let st = st.to_lowercase();
    if st == "account" {
        return Ok(());
    }
    if !VALID_SCOPES.contains(&st.as_str()) {
        return Err(SnowError::cli(format!(
            "Scope type must be one of the following: {}.",
            VALID_SCOPES.join(", ")
        )));
    }
    if let Some(n) = &scope.1
        && !is_valid_identifier(n)
    {
        return Err(SnowError::cli("Scope name must be a valid identifier."));
    }
    if st == "compute-pool" && object_type != "service" {
        return Err(SnowError::cli(
            "compute-pool scope is only supported for listing service.",
        ));
    }
    Ok(())
}

pub fn show(
    cli: &mut Cli,
    object_type: &str,
    like: Option<&str>,
    scope: &Scope,
    terse: bool,
    limit: Option<i64>,
) -> Result<Cursor> {
    let (_, plural) = names(object_type)?;
    let mut q = String::from("show");
    if terse {
        q.push_str(" terse");
    }
    q.push(' ');
    q.push_str(plural);
    if let Some(l) = like.filter(|l| !l.is_empty()) {
        q.push_str(&format!(" like {}", to_string_literal(l)));
    }
    if let Some(st) = &scope.0 {
        let st = st.replace('-', " ");
        match &scope.1 {
            Some(n) => q.push_str(&format!(" in {st} {n}")),
            None => q.push_str(&format!(" in {st}")),
        }
    }
    if let Some(l) = limit {
        q.push_str(&format!(" limit {l}"));
    }
    cli.execute_query(&q)
}

pub fn drop_object(cli: &mut Cli, object_type: &str, fqn: &Fqn, if_exists: bool) -> Result<Cursor> {
    let (name, _) = names(object_type)?;
    let ie = if if_exists { " if exists" } else { "" };
    cli.execute_query(&format!("drop {name}{ie} {}", fqn.sql_identifier()))
}

pub fn describe_object(cli: &mut Cli, object_type: &str, fqn: &Fqn) -> Result<Cursor> {
    if object_type == "image-repository" {
        return Err(SnowError::click(
            "Describe is currently not supported for object of type image-repository",
        ));
    }
    let (name, _) = names(object_type)?;
    cli.execute_query(&format!("describe {name} {}", fqn.sql_identifier()))
}

pub fn object_exists(cli: &mut Cli, object_type: &str, fqn: &Fqn) -> bool {
    let Ok((name, _)) = names(object_type) else {
        return false;
    };
    cli.try_execute(&format!("describe {name} {}", fqn.sql_identifier()))
        .is_ok()
}

/// `list_` shared by `snow object list` and the group aliases.
pub fn list_for(cli: &mut Cli, object_type: &str) -> Result<Output> {
    let like = cli.str("like");
    let scope = if cli.inv.leaf().get(cli.spec, "scope").is_some() {
        scope_of(cli)
    } else {
        (None, None)
    };
    let in_account = cli.flag("in_account");
    let terse = cli.flag("terse");
    let limit = cli.int("limit");
    let eff = effective_scope(scope, in_account, object_type)?;
    scope_validate(object_type, &eff)?;
    let c = show(cli, object_type, like.as_deref(), &eff, terse, limit)?;
    Ok(cli.query_result(&c))
}

pub fn fqn_arg(cli: &Cli, name: &str) -> Result<Fqn> {
    let raw = cli.str(name).unwrap_or_default();
    let is_stage = cli.inv.leaf().get(cli.spec, name).is_some()
        && cli
            .spec
            .cmd_params(cli.inv.leaf().cmd)
            .iter()
            .map(|i| cli.spec.param(*i))
            .any(|p| {
                p.name == name
                    && matches!(p.ty, snow_click::spec::Ty::Custom("IdentifierStageType"))
            });
    if is_stage {
        Fqn::from_stage(&raw)
    } else {
        Fqn::from_string(&raw)
    }
}

pub fn drop_for(cli: &mut Cli, object_type: &str, arg: &str) -> Result<Output> {
    let fqn = fqn_arg(cli, arg)?;
    let ie = cli.flag("if_exists");
    let c = drop_object(cli, object_type, &fqn, ie)?;
    Ok(cli.query_result(&c))
}

pub fn describe_for(cli: &mut Cli, object_type: &str, arg: &str) -> Result<Output> {
    let fqn = fqn_arg(cli, arg)?;
    let c = describe_object(cli, object_type, &fqn)?;
    Ok(cli.query_result(&c))
}

pub fn list(cli: &mut Cli) -> Result<Output> {
    let t = cli.str("object_type").unwrap_or_default();
    list_for(cli, &t)
}

pub fn drop(cli: &mut Cli) -> Result<Output> {
    let t = cli.str("object_type").unwrap_or_default();
    drop_for(cli, &t, "object_name")
}

pub fn describe(cli: &mut Cli) -> Result<Output> {
    let t = cli.str("object_type").unwrap_or_default();
    describe_for(cli, &t, "object_name")
}

fn pluralize(t: &str) -> String {
    match t.strip_suffix('y') {
        Some(s) => format!("{}ies", s.to_lowercase()),
        None => format!("{}s", t.to_lowercase()),
    }
}

/// Click's `HTTPError` text for a failed REST call.
fn rest_error(code: u16) -> SnowError {
    match code {
        400 => SnowError::click(
            "400 bad request: Incorrect object definition (arguments misspelled or malformatted).",
        ),
        401 => SnowError::click(
            "401 unauthorized: role you are using does not have permissions to create this object.",
        ),
        408 => SnowError::click(
            "408 timeout: the request timed out and was not completed by the server.",
        ),
        409 => SnowError::click("409 conflict: object you're trying to create already exists."),
        429 => {
            SnowError::click("429 too many requests. The number of requests hit the rate limit.")
        }
        500 | 503 | 504 => SnowError::click(format!("{code} internal server error.")),
        c => SnowError::unexpected(format!("{c} Client Error")),
    }
}

fn endpoint_exists(cli: &mut Cli, url: &str) -> Result<bool> {
    let (code, _) = cli
        .connection()?
        .rest("GET", url, None)
        .map_err(|e| SnowError::unexpected(e.msg()))?;
    Ok(match code {
        200..=299 | 400 => true,
        404 => false,
        c => return Err(rest_error(c)),
    })
}

fn fetch_exists(cli: &mut Cli, url: &str) -> Result<bool> {
    let (code, body) = cli
        .connection()?
        .rest("GET", url, None)
        .map_err(|e| SnowError::unexpected(e.msg()))?;
    Ok(match code {
        200..=299 => {
            !(body.is_null()
                || body.as_object().is_some_and(|m| m.is_empty())
                || body.as_array().is_some_and(|a| a.is_empty()))
        }
        404 | 400 => false,
        c => return Err(rest_error(c)),
    })
}

fn create_url(
    cli: &mut Cli,
    object_type: &str,
    replace: bool,
    if_not_exists: bool,
) -> Result<String> {
    let plural = pluralize(object_type);
    let q = if if_not_exists {
        "?createMode=ifNotExists"
    } else if replace {
        "?createMode=orReplace"
    } else {
        ""
    };
    let url = format!("/api/v2/{plural}/");
    if endpoint_exists(cli, &url)? {
        return Ok(format!("{url}{q}"));
    }
    let session = cli.connection()?.session.clone();
    let Some(db) = session.database else {
        return Err(SnowError::click(
            "Database not defined in connection. Please try again with `--database` flag.",
        ));
    };
    if !fetch_exists(cli, &format!("/api/v2/databases/{db}"))? {
        return Err(SnowError::click(format!("Database '{db}' does not exist.")));
    }
    let url = format!("/api/v2/databases/{db}/{plural}/");
    if endpoint_exists(cli, &url)? {
        return Ok(format!("{url}{q}"));
    }
    let Some(schema) = session.schema else {
        return Err(SnowError::click(
            "Schema not defined in connection. Please try again with `--schema` flag.",
        ));
    };
    let url = format!("/api/v2/databases/{db}/schemas/{schema}/{plural}/");
    if endpoint_exists(cli, &url)? {
        return Ok(format!("{url}{q}"));
    }
    Err(SnowError::click(format!(
        "Create operation for type {object_type} is not supported. Try using `sql -q 'CREATE ...'` command."
    )))
}

pub fn create(cli: &mut Cli) -> Result<Output> {
    let object_type = cli.str("object_type").unwrap_or_default();
    let attrs = cli.list("object_attributes");
    let json = cli.str("object_json");
    if !attrs.is_empty() && json.is_some() {
        return Err(SnowError::incompatible(&["object_attributes", "--json"]));
    }
    let data: serde_json::Value = if let Some(j) = json {
        serde_json::from_str(&j).map_err(|e| SnowError::unexpected(e.to_string()))?
    } else if !attrs.is_empty() {
        let mut m = serde_json::Map::new();
        for v in parse_key_value_variables(&attrs)? {
            let parsed = serde_json::from_str::<serde_json::Value>(&v.value)
                .unwrap_or(serde_json::Value::String(v.value.clone()));
            m.insert(v.key, parsed);
        }
        serde_json::Value::Object(m)
    } else {
        return Err(SnowError::cli(
            "Provide either list of object attributes, or object definition in JSON format",
        ));
    };
    let url = create_url(
        cli,
        &object_type,
        cli.flag("replace"),
        cli.flag("if_not_exists"),
    )?;
    let (code, body) = cli
        .connection()?
        .rest("POST", &url, Some(&data))
        .map_err(|e| SnowError::unexpected(e.msg()))?;
    if !(200..300).contains(&code) {
        return Err(rest_error(code));
    }
    Ok(Output::message(
        body["status"].as_str().unwrap_or("").to_string(),
    ))
}
