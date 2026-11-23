//! Snowsight URLs and UI parameters (`_plugins/connection/util.py`).

use snow_api::{Cli, Result, SnowError};

pub const SNOWSIGHT_COMMERCIAL_HOST: &str = "https://app.snowflake.com";

fn first_cell(cli: &mut Cli, sql: &str, col: Option<&str>) -> Option<String> {
    let c = cli.try_execute(sql).ok()?;
    let idx = match col {
        Some(n) => c.column_index(n)?,
        None => 0,
    };
    c.rows.first()?.get(idx)?.clone()
}

thread_local! {
    /// `@lru_cache get_ui_parameters(conn)`: one bootstrap query per connection.
    static UI_PARAMS: std::cell::RefCell<Option<Option<Vec<serde_json::Value>>>> =
        const { std::cell::RefCell::new(None) };
}

fn ui_parameters(cli: &mut Cli) -> Option<Vec<serde_json::Value>> {
    if let Some(v) = UI_PARAMS.with(|c| c.borrow().clone()) {
        return v;
    }
    let fetched = first_cell(
        cli,
        "call system$bootstrap_data_request('CLIENT_PARAMS_INFO')",
        None,
    )
    .and_then(|cell| serde_json::from_str::<serde_json::Value>(&cell).ok())
    .and_then(|v| v["clientParamsInfo"].as_array().cloned());
    UI_PARAMS.with(|c| *c.borrow_mut() = Some(fetched.clone()));
    fetched
}

/// `get_ui_parameters` → the value of one parameter, or `None` when unreadable.
#[allow(clippy::result_unit_err)]
pub fn ui_parameter(
    cli: &mut Cli,
    name: &str,
) -> std::result::Result<Option<serde_json::Value>, ()> {
    let list = ui_parameters(cli).ok_or(())?;
    Ok(list
        .iter()
        .find(|r| r["name"].as_str() == Some(name))
        .map(|r| r["value"].clone()))
}

/// `get_ui_parameter(conn, parameter, default)` rendered like Python's `str(value)`.
pub fn ui_parameter_or(cli: &mut Cli, name: &str, default: serde_json::Value) -> serde_json::Value {
    match ui_parameter(cli, name) {
        Ok(Some(v)) => v,
        _ => default,
    }
}

fn is_regionless_redirect(cli: &mut Cli) -> bool {
    match ui_parameter(cli, "UI_SNOWSIGHT_ENABLE_REGIONLESS_REDIRECT") {
        Ok(Some(v)) => v
            .as_bool()
            .unwrap_or_else(|| !matches!(v.as_str(), Some("false") | Some("False"))),
        _ => true,
    }
}

fn host_region(host: &str) -> Option<String> {
    let h = host.split(':').next().unwrap_or(host);
    if h.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let labels: Vec<&str> = h.split('.').collect();
    if labels.last() == Some(&"local") {
        return Some("local".into());
    }
    if labels.len() < 3 || labels[labels.len() - 2..] != ["snowflakecomputing", "com"] {
        return None;
    }
    let rem = &labels[..labels.len() - 2];
    if rem.len() < 2 {
        return None;
    }
    let (account, region) = (rem[0], &rem[1..]);
    if account.contains('-') && region.len() <= 2 {
        return None;
    }
    match region.len() {
        1 => Some(region[0].to_string()),
        2 if ["aws", "azure", "gcp"].contains(&region[1]) => Some(region.join(".")),
        3 => Some(region.join(".")),
        _ => None,
    }
}

fn context(cli: &mut Cli) -> Result<String> {
    if is_regionless_redirect(cli) {
        let org = first_cell(
            cli,
            "select system$return_current_org_name() as result",
            Some("RESULT"),
        );
        return org.ok_or_else(|| SnowError::unexpected("'RESULT'").named("KeyError"));
    }
    let host = cli.connection()?.host.clone();
    host_region(&host)
        .ok_or_else(|| SnowError::cli(format!("Could not determine region from host {host}")))
}

fn account(cli: &mut Cli) -> Result<String> {
    if let Some(a) = first_cell(
        cli,
        "select current_account_name() as result",
        Some("RESULT"),
    ) {
        return Ok(a.to_lowercase());
    }
    let c = cli.connection()?;
    if !c.account.is_empty() {
        return Ok(c.account.clone());
    }
    Ok(c.host.split('.').next().unwrap_or("").to_string())
}

/// `make_snowsight_url(conn, path)`.
pub fn make_url(cli: &mut Cli, path: &str) -> Result<String> {
    let host = first_cell(
        cli,
        "select system$get_snowsight_host() as result",
        Some("RESULT"),
    )
    .unwrap_or_else(|| SNOWSIGHT_COMMERCIAL_HOST.to_string());
    let deployment = context(cli)?;
    let account = account(cli)?;
    let p = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok(format!("{host}/{deployment}/{account}{p}"))
}

/// `get_host_region(host)` for callers outside this module.
pub fn host_region_pub(host: &str) -> Option<String> {
    host_region(host)
}
