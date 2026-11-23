//! `SnowflakeAppManager` (`_plugins/apps/manager.py`): the SQL the Snowflake App
//! Runtime commands run.

use super::app_fqn;
use indexmap::IndexMap;
use snow_api::identifiers::{Fqn, to_identifier, to_string_literal};
use snow_api::output::Pv;
use snow_api::sql::sanitize_for_terminal as san;
use snow_api::{Cli, Result};

pub const ACCOUNT_ADMIN_SETUP_URL: &str = "https://docs.snowflake.com/en/developer-guide/snowflake-app-runtime/account-admin-setup#after-setup";
pub const PER_ACCOUNT_CERT_ISSUE_FUNCTION: &str =
    "SYSTEM$ISSUE_PER_ACCOUNT_APP_SERVICE_CERTIFICATE";
const DEFAULTS_FN: &str = "SYSTEM$GET_APPLICATION_SERVICE_DEFAULTS";
const APP_SPACE_FN: &str = "SNOWFLAKE.APPS.GET_DEFAULT_APP_SPACE";

/// The first row of `sql` as a lower-cased dict; `Err` is the query error.
pub fn first_row(
    cli: &mut Cli,
    sql: &str,
) -> Result<std::result::Result<IndexMap<String, Pv>, snow_connector::DbError>> {
    Ok(match cli.try_query(sql)? {
        Ok(c) => {
            let mut m = IndexMap::new();
            if let Some(r) = c.rows.first() {
                for (i, col) in c.columns.iter().enumerate() {
                    let v = r.get(i).cloned().flatten().map(Pv::Str).unwrap_or(Pv::None);
                    m.insert(col.name.to_lowercase(), v);
                }
            }
            Ok(m)
        }
        Err(e) => Err(e),
    })
}

fn pv(m: &IndexMap<String, Pv>, k: &str) -> String {
    match m.get(k) {
        None | Some(Pv::None) => String::new(),
        Some(v) => v.py_str(),
    }
}

/// `describe_app_service(service_fqn)`: `{}` for no row.
pub fn describe_app_service(
    cli: &mut Cli,
    f: &Fqn,
) -> Result<std::result::Result<IndexMap<String, Pv>, snow_connector::DbError>> {
    first_row(
        cli,
        &format!("DESCRIBE APPLICATION SERVICE {}", f.identifier()),
    )
}

/// `resolve_application_service_url_from_describe(desc)`.
pub fn url_from_describe(desc: &IndexMap<String, Pv>) -> Option<String> {
    if desc.is_empty() {
        return None;
    }
    if ["true", "1", "yes"].contains(&pv(desc, "is_upgrading").to_lowercase().as_str()) {
        return None;
    }
    let url = pv(desc, "url").trim().to_string();
    if url.is_empty() || url.to_lowercase().contains("provisioning in progress") {
        return None;
    }
    Some(
        if url.starts_with("http://") || url.starts_with("https://") {
            url
        } else {
            format!("https://{url}")
        },
    )
}

/// `fetch_app_service_defaults(app_name)`.
pub fn fetch_app_service_defaults(
    cli: &mut Cli,
    app_name: &str,
) -> Result<IndexMap<String, String>> {
    // App Spaces first.
    if let Ok(Ok(row)) = first_row(cli, &format!("SELECT * FROM TABLE({APP_SPACE_FN}())"))
        && !row.is_empty()
    {
        let mut out = IndexMap::new();
        for (col, key) in [
            ("database_name", "database"),
            ("query_warehouse", "query_warehouse"),
            ("build_external_access_integration", "build_eai"),
        ] {
            let v = pv(&row, col);
            if !v.is_empty() {
                out.insert(key.to_string(), to_identifier(&v));
            }
        }
        if !out.is_empty() {
            out.insert("schema".into(), to_identifier(app_name));
            return Ok(out);
        }
    }
    match cli.try_query(&format!("SELECT {DEFAULTS_FN}()"))? {
        Ok(c) => {
            let cell = c
                .rows
                .first()
                .and_then(|r| r.first().cloned().flatten())
                .unwrap_or_default();
            if cell.is_empty() {
                return Ok(IndexMap::new());
            }
            let Ok(serde_json::Value::Object(payload)) =
                serde_json::from_str::<serde_json::Value>(&cell)
            else {
                return Ok(IndexMap::new());
            };
            let mut out = IndexMap::new();
            for k in ["database", "schema", "query_warehouse", "build_eai"] {
                if let Some(v) = payload.get(k).filter(|v| {
                    !v.is_null() && v.as_str() != Some("") && v.as_bool() != Some(false)
                }) {
                    out.insert(
                        k.to_string(),
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string()),
                    );
                }
            }
            Ok(out)
        }
        Err(e) => {
            let m = e.msg_with(true);
            if m.to_lowercase().contains("unknown function") && m.contains(DEFAULTS_FN) {
                let params = fetch_snow_apps_parameters(cli)?;
                return filter_accessible_remote_defaults(cli, params);
            }
            Ok(IndexMap::new())
        }
    }
}

/// `fetch_snow_apps_parameters()`.
pub fn fetch_snow_apps_parameters(cli: &mut Cli) -> Result<IndexMap<String, String>> {
    let mut out = IndexMap::new();
    let Ok(c) = cli.try_query("SHOW PARAMETERS LIKE 'DEFAULT_SNOWFLAKE_APPS_%' IN USER")? else {
        return Ok(out);
    };
    let idx = |n: &str| {
        c.columns
            .iter()
            .position(|col| col.name.eq_ignore_ascii_case(n))
    };
    let (ki, vi, li) = (idx("key"), idx("value"), idx("level"));
    for r in &c.rows {
        let get = |i: Option<usize>| {
            i.and_then(|i| r.get(i).cloned().flatten())
                .unwrap_or_default()
        };
        let (k, v, l) = (get(ki).to_uppercase(), get(vi), get(li));
        let mapped = match k.as_str() {
            "DEFAULT_SNOWFLAKE_APPS_QUERY_WAREHOUSE" => "query_warehouse",
            "DEFAULT_SNOWFLAKE_APPS_BUILD_EXTERNAL_ACCESS_INTEGRATION" => "build_eai",
            "DEFAULT_SNOWFLAKE_APPS_DESTINATION_DATABASE" => "database",
            "DEFAULT_SNOWFLAKE_APPS_DESTINATION_SCHEMA" => "schema",
            _ => continue,
        };
        if !v.is_empty() && !l.is_empty() {
            out.insert(mapped.to_string(), v);
        }
    }
    Ok(out)
}

/// `current_role()`.
pub fn current_role(cli: &mut Cli) -> Option<String> {
    let c = cli.try_query("SELECT CURRENT_ROLE()").ok()?.ok()?;
    c.rows.first()?.first()?.clone().filter(|s| !s.is_empty())
}

fn flatten_missing(
    node: &serde_json::Value,
    out: &mut Vec<serde_json::Map<String, serde_json::Value>>,
) {
    let Some(o) = node.as_object() else {
        return;
    };
    if o.get("authorized") == Some(&serde_json::Value::Bool(true)) {
        return;
    }
    if ["privilege", "objectType", "objectName"]
        .iter()
        .any(|k| o.contains_key(*k))
    {
        out.push(o.clone());
        return;
    }
    for g in ["allOf", "oneOf"] {
        if let Some(serde_json::Value::Array(a)) = o.get(g) {
            for c in a {
                flatten_missing(c, out);
            }
        }
    }
}

/// `get_missing_privileges(statement, role)`; `Err` when the statement cannot be analyzed.
fn missing_privileges(
    cli: &mut Cli,
    stmt: &str,
    role: Option<&str>,
) -> Result<std::result::Result<Vec<serde_json::Map<String, serde_json::Value>>, ()>> {
    let mut args = vec![
        format!("statement => {}", to_string_literal(stmt)),
        "missing_only => true".to_string(),
    ];
    if let Some(r) = role {
        args.push(format!("for_role => {}", to_string_literal(r)));
    }
    let c = match cli.try_query(&format!("CALL EXPLAIN_PRIVILEGES({})", args.join(", ")))? {
        Ok(c) => c,
        Err(_) => return Ok(Err(())),
    };
    let cell = c
        .rows
        .first()
        .and_then(|r| r.first().cloned().flatten())
        .unwrap_or_default();
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&cell) {
        flatten_missing(&v, &mut out);
    }
    Ok(Ok(out))
}

/// `_filter_accessible_remote_defaults(manager, params)`.
fn filter_accessible_remote_defaults(
    cli: &mut Cli,
    params: IndexMap<String, String>,
) -> Result<IndexMap<String, String>> {
    let Some(database) = params.get("database").cloned().filter(|d| !d.is_empty()) else {
        return Ok(params);
    };
    let schema = params
        .get("schema")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "PUBLIC".into());
    let role = current_role(cli);
    cli.console.step(&format!(
        "Checking deploy privileges on the account-configured destination {}.{}...",
        san(&database),
        san(&schema)
    ));
    let placeholder = app_fqn(
        Some(&database),
        Some(&schema),
        "SNOWFLAKE_CLI_PRIVILEGE_CHECK",
    )
    .identifier();
    let statements = [
        format!("CREATE STAGE {placeholder} ENCRYPTION = (TYPE = 'SNOWFLAKE_SSE')"),
        format!("CREATE ARTIFACT REPOSITORY {placeholder} TYPE=APPLICATION"),
    ];
    let mut missing = Vec::new();
    let mut failed = false;
    for s in &statements {
        match missing_privileges(cli, s, role.as_deref())? {
            Err(()) => failed = true,
            Ok(m) if !m.is_empty() => {
                failed = true;
                missing.extend(m);
            }
            Ok(_) => {}
        }
    }
    if !failed {
        return Ok(params);
    }
    let _ = missing;
    let role_label = role.map(|r| format!(" '{}'", san(&r))).unwrap_or_default();
    cli.console.warning(&format!(
        "Your current role{role_label} is missing privileges required to deploy to the account-configured Snowflake App Runtime destination '{}.{}'. Falling back to your personal database. Ask your account administrator to grant access: {ACCOUNT_ADMIN_SETUP_URL}",
        san(&database),
        san(&schema)
    ));
    let mut out = params;
    out.shift_remove("database");
    out.shift_remove("schema");
    Ok(out)
}

fn per_account_domain(cli: &Cli) -> String {
    cli.config
        .section(&["cli"])
        .and_then(|s| s.get("per_account_app_domain").map(|v| v.py_str()))
        .unwrap_or_else(|| "snowflake.app".to_string())
}

/// `per_account_cert_status_for_host(host)`: `None` is UNKNOWN.
pub fn cert_status_for_host(cli: &Cli, host: &str) -> Option<bool> {
    let domain = per_account_domain(cli);
    if host.is_empty() || !host.ends_with(&format!(".{domain}")) {
        return None;
    }
    snow_connector::ws::tls_probe(host, 443, std::time::Duration::from_secs(5))
}

fn url_hostname(url: &str) -> String {
    let full = if url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    let rest = full.split("://").nth(1).unwrap_or("");
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = auth.rsplit('@').next().unwrap_or("");
    host.split(':').next().unwrap_or("").to_lowercase()
}

/// `host_labels(host)`: `None` for IP addresses.
fn host_labels(host: &str) -> Option<Vec<String>> {
    let mut h = host.trim().to_string();
    if let Some(rest) = h.strip_prefix('[') {
        h = rest.split(']').next()?.to_string();
    } else if h.matches(':').count() == 1 {
        h = h.split(':').next()?.to_string();
    }
    if h.is_empty() || h.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let labels: Vec<String> = h.split('.').map(str::to_string).collect();
    if labels.last().is_some_and(|l| l == "local") {
        return Some(vec!["local".into()]);
    }
    Some(labels)
}

fn air_gapped(cli: &Cli) -> bool {
    cli.config
        .section(&["cli"])
        .and_then(|s| s.get("air_gapped").map(|v| v.py_str().to_lowercase()))
        .is_some_and(|v| v == "true")
}

/// `_infra_from_sql_host(host)`.
fn infra_from_sql_host(cli: &Cli, host: Option<&str>) -> Option<String> {
    let labels = host_labels(host?)?;
    if labels == ["local"] || labels.len() < 4 {
        return None;
    }
    if !air_gapped(cli)
        && labels[labels.len() - 2..] != ["snowflakecomputing".to_string(), "com".to_string()]
    {
        return None;
    }
    Some(labels[1..labels.len() - 2].join("."))
}

/// `_parse_dns_region_id(payload)`.
fn parse_dns_region_id(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let r = v.get("dnsRegionId")?.as_str()?.trim().to_lowercase();
    if r.is_empty() || r == "unknownregion.unknowncsp" || r.starts_with('.') || r.ends_with('.') {
        return None;
    }
    let re = regex::Regex::new(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$").expect("re");
    r.split('.').all(|l| re.is_match(l)).then_some(r)
}

/// `per_account_cert_probe_host()`: `snowflake-cli-cert-check.<org>-<account>.<infra>.<domain>`.
pub fn per_account_cert_probe_host(cli: &mut Cli) -> Option<String> {
    let host = Some(cli.connection().ok()?.host.clone());
    let region = first_row(cli, "SELECT SYSTEM$GET_ACCOUNT_DEPLOYMENT_INFO() AS info")
        .ok()?
        .ok()
        .and_then(|r| parse_dns_region_id(&pv(&r, "info")));
    let infra = match region {
        Some(r) => {
            let pl = host
                .as_deref()
                .and_then(host_labels)
                .is_some_and(|l| l.iter().any(|x| x.eq_ignore_ascii_case("privatelink")));
            Some(if pl { format!("{r}.privatelink") } else { r })
        }
        None => infra_from_sql_host(cli, host.as_deref()).or_else(|| {
            let row = first_row(cli, "SELECT SYSTEM$ALLOWLIST() AS result")
                .ok()?
                .ok()?;
            let list: Vec<serde_json::Value> = serde_json::from_str(&pv(&row, "result")).ok()?;
            let h = list.iter().find(|t| {
                t["type"].as_str() == Some("SNOWFLAKE_DEPLOYMENT")
                    && t["host"]
                        .as_str()
                        .is_some_and(|h| crate::snowsight::host_region_pub(h).is_some())
            })?["host"]
                .as_str()?
                .to_string();
            infra_from_sql_host(cli, Some(&h))
        }),
    }?;
    let row = first_row(
        cli,
        "SELECT CURRENT_ORGANIZATION_NAME() AS org, CURRENT_ACCOUNT_NAME() AS acct",
    )
    .ok()?
    .ok()?;
    let (org, acct) = (pv(&row, "org"), pv(&row, "acct"));
    if org.is_empty() || acct.is_empty() || org == "None" || acct == "None" {
        return None;
    }
    let slug = format!("{}-{}", org.to_uppercase(), acct.to_uppercase())
        .to_lowercase()
        .replace('_', "-");
    let domain = per_account_domain(cli);
    Some(format!("snowflake-cli-cert-check.{slug}.{infra}.{domain}"))
}

pub fn cert_not_provisioned_for_url(cli: &Cli, url: &str) -> bool {
    cert_status_for_host(cli, &url_hostname(url)) == Some(false)
}

/// `_warn_if_cng_url_cert_missing(manager, url)`.
pub fn warn_if_cert_missing(cli: &mut Cli, url: &str) {
    if cert_not_provisioned_for_url(cli, url) {
        cli.console.warning(&format!(
            "This account does not yet have a per-account URL certificate, so the app URL may show a browser TLS warning. Start provisioning by running:\n  SELECT {PER_ACCOUNT_CERT_ISSUE_FUNCTION}();\nThis can take up to 3 hours."
        ));
    }
}
