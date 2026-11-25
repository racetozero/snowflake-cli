//! `snow auth list`, `snow auth oidc read-token`.

use snow_api::config::Cv;
use snow_api::output::{Output, Pv, Row};
use snow_api::{Cli, Result, SnowError};

fn s(c: &indexmap::IndexMap<String, Cv>, k: &str) -> String {
    match c.get(k) {
        None => String::new(),
        Some(v) if v.as_str() == Some("") => String::new(),
        Some(v) => v.py_str(),
    }
}

/// Snowflake OAuth for local applications: the built-in client, no custom id/secret.
pub fn is_local_oauth(c: &indexmap::IndexMap<String, Cv>) -> bool {
    s(c, "authenticator").to_uppercase() == "OAUTH_AUTHORIZATION_CODE"
        && s(c, "oauth_client_id").is_empty()
        && s(c, "oauth_client_secret").is_empty()
}

pub fn list(cli: &mut Cli) -> Result<Output> {
    if cli.config.has_connections_file() {
        let p =
            snow_api::sanitize::for_terminal(&cli.config.connections_file.display().to_string());
        cli.console.warning(&format!(
            "Reading connections from {p}. Entries from config.toml are ignored."
        ));
    }
    let default = cli.config.default_connection_name();
    let rows: Vec<Row> = cli
        .config
        .connections()
        .into_iter()
        .filter(|(_, c)| is_local_oauth(c))
        .map(|(name, c)| {
            let mut r = Row::new();
            r.insert("connection_name".into(), Pv::s(name.clone()));
            for k in [
                "account",
                "user",
                "host",
                "database",
                "schema",
                "warehouse",
                "role",
            ] {
                r.insert(k.into(), Pv::s(s(&c, k)));
            }
            r.insert("is_default".into(), Pv::Bool(name == default));
            r
        })
        .collect();
    Ok(Output::Collection(rows))
}

pub fn oidc_read_token(cli: &mut Cli) -> Result<Output> {
    let ty = cli.str("_type").unwrap_or_else(|| "auto".into());
    let token = snow_api::oidc::read_token(cli, &ty).inspect_err(|e| {
        crate::logs::log(
            crate::logs::ERROR,
            "snowflake.cli._plugins.auth.oidc.manager",
            &format!("OIDC provider error: {}", e.message()),
        );
    })?;
    Ok(Output::message(token))
}

fn arg_err(m: impl Into<String>) -> snow_api::SnowError {
    snow_api::SnowError::cli(m.into()).named("CliArgumentError")
}

/// `SignInTarget._hostname`.
fn hostname(value: &str) -> Result<String> {
    let t = value.trim();
    if !t.contains("://") {
        return Ok(t.split('/').next().unwrap_or_default().to_string());
    }
    match url::Url::parse(t)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    {
        Some(h) => Ok(h.to_lowercase()),
        None => Err(arg_err(format!(
            "Could not read an account host from {value}. Pass an account host such as myorg-myacct.privatelink.snowflakecomputing.com."
        ))),
    }
}

/// `SignInTarget.from_login(account, host)`: (account identifier, host to store).
fn sign_in_target(account: &str, host: Option<&str>) -> Result<(String, Option<String>)> {
    let text = account.trim();
    if text.is_empty() {
        return Err(arg_err("Enter an account to sign in."));
    }
    let explicit = match host.filter(|h| !h.trim().is_empty()) {
        Some(h) => Some(hostname(h)?),
        None => None,
    };
    if text.contains("://") || text.to_lowercase().contains("snowflakecomputing") {
        let h = hostname(text)?;
        let parts: Vec<&str> = h.split('.').collect();
        let id = if parts.len() <= 1 {
            h.clone()
        } else if parts[1] == "global" {
            match parts[0].rfind('-') {
                Some(i) => parts[0][..i].to_string(),
                None => parts[0].to_string(),
            }
        } else {
            parts[0].to_string()
        };
        let id = id.trim().to_string();
        if id.is_empty() {
            return Err(arg_err("Enter an account to sign in."));
        }
        return Ok((id, Some(explicit.unwrap_or(h))));
    }
    Ok((text.to_string(), explicit))
}

/// `snow auth login`: argument checks, then the browser sign-in.
pub fn login(cli: &mut Cli) -> Result<Output> {
    let account = match cli.str("account") {
        Some(a) => a,
        None => crate::common::prompt(cli, "Enter account", false)?.unwrap_or_default(),
    };
    if let Some(r) = cli.str("role")
        && r.trim().is_empty()
    {
        return Err(arg_err("Role is blank. Enter a role, or omit --role."));
    }
    let name = match cli.str("connection_name") {
        Some(n) if n.trim().is_empty() => {
            return Err(arg_err(
                "Enter a connection name, or omit --connection-name.",
            ));
        }
        Some(n) => Some(n.trim().to_string()),
        None => None,
    };
    if let Some(n) = &name
        && let Some(owner) = cli.config.connections().get(n)
        && !is_local_oauth(owner)
    {
        return Err(snow_api::SnowError::cli(format!(
            "Connection {n} already exists and does not use Snowflake OAuth for local applications. Choose a different connection name."
        )));
    }
    login_after_checks(cli, &account, name)
}

type Cfg = indexmap::IndexMap<String, Cv>;

struct Edits {
    role: Option<String>,
    warehouse: Option<String>,
    database: Option<String>,
    schema: Option<String>,
    clear_warehouse: bool,
    clear_database: bool,
    clear_schema: bool,
}

impl Edits {
    fn apply(&self, f: &mut Cfg) {
        if let Some(r) = &self.role {
            f.insert("role".into(), Cv::Str(r.clone()));
        }
        for (k, v, clear) in [
            ("warehouse", &self.warehouse, self.clear_warehouse),
            ("database", &self.database, self.clear_database),
            ("schema", &self.schema, self.clear_schema),
        ] {
            if clear {
                f.shift_remove(k);
            } else if let Some(v) = v {
                f.insert(k.into(), Cv::Str(v.clone()));
            }
        }
    }
}

fn oauth_scope(role: Option<&str>) -> String {
    match role {
        Some(r) => format!("refresh_token session:role:{r}"),
        None => "refresh_token".into(),
    }
}

fn local_oauth_for(all: &indexmap::IndexMap<String, Cfg>, account: &str) -> Vec<(String, Cfg)> {
    all.iter()
        .filter(|(_, c)| {
            is_local_oauth(c) && s(c, "account").to_lowercase() == account.to_lowercase()
        })
        .map(|(n, c)| (n.clone(), c.clone()))
        .collect()
}

/// `_shared_or_account_named_host`: a host every saved connection shares, else the
/// host of the connection named after the account.
fn shared_or_named_host(
    all: &indexmap::IndexMap<String, Cfg>,
    saved: &[(String, Cfg)],
    account: &str,
) -> Option<String> {
    if !saved.is_empty() {
        let hosts: Vec<String> = saved.iter().map(|(_, c)| s(c, "host")).collect();
        if !hosts[0].is_empty() && hosts.iter().all(|h| *h == hosts[0]) {
            return Some(hosts[0].clone());
        }
    }
    all.get(account)
        .map(|c| s(c, "host"))
        .filter(|h| !h.is_empty())
}

fn same_user(stored: &str, session_user: &str) -> bool {
    !stored.trim().is_empty() && stored.trim().to_uppercase() == session_user.trim().to_uppercase()
}

fn same_role(stored: &str, given: Option<&str>) -> bool {
    let sb = stored.trim().is_empty();
    let gb = given.is_none_or(|x| x.trim().is_empty());
    if sb || gb {
        return sb && gb;
    }
    let b = given.unwrap_or_default();
    let quoted = |r: &str| r.len() >= 2 && r.starts_with('"') && r.ends_with('"');
    if quoted(stored) || quoted(b) {
        return stored == b;
    }
    stored.to_lowercase() == b.to_lowercase()
}

fn matches(c: &Cfg, account: &str, session_user: &str, role: Option<&str>) -> bool {
    is_local_oauth(c)
        && s(c, "account").to_lowercase() == account.to_lowercase()
        && same_user(&s(c, "user"), session_user)
        && same_role(&s(c, "role"), role)
}

/// `_choose_connection_name(...)`: (name to write, source connection, note).
fn choose_name(
    cli: &mut Cli,
    all: &indexmap::IndexMap<String, Cfg>,
    who: (&str, &str, Option<&str>),
    existing: Option<String>,
    requested: Option<String>,
) -> Result<(String, Option<String>, Option<String>)> {
    let (account, user, role) = who;
    if let Some(req) = requested {
        if let Some(owner) = all.get(&req)
            && Some(&req) != existing.as_ref()
        {
            if matches(owner, account, user, role) {
                return Ok((req.clone(), Some(req), None));
            }
            return Err(SnowError::cli(format!(
                "Connection {req} belongs to a different user, account, or role. Choose a different connection name."
            )));
        }
        return match &existing {
            None => Ok((req, None, None)),
            Some(e) if *e == req => Ok((req.clone(), Some(req), None)),
            Some(e) => {
                use std::io::IsTerminal;
                if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
                    return Ok((
                        e.clone(),
                        Some(e.clone()),
                        Some(format!(
                            "Kept connection name {e}. A terminal is required to rename it to {req}."
                        )),
                    ));
                }
                if crate::common::confirm(cli, &format!("Rename connection {e} to {req}?"), false)?
                {
                    Ok((req, Some(e.clone()), None))
                } else {
                    Ok((e.clone(), Some(e.clone()), None))
                }
            }
        };
    }
    if let Some(e) = existing {
        return Ok((e.clone(), Some(e), None));
    }
    let derived = match role {
        Some(r) => format!("{account}-{user}-{r}"),
        None => format!("{account}-{user}"),
    };
    if !all.contains_key(&derived) {
        return Ok((derived, None, None));
    }
    for _ in 0..8 {
        let c = format!(
            "{derived}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..4]
        );
        if !all.contains_key(&c) {
            return Ok((c, None, None));
        }
    }
    Err(SnowError::cli(format!(
        "Connection name {derived} is already taken. Pass --connection-name to choose another."
    )))
}

/// One sign-in connect followed by `SELECT CURRENT_USER()`.
fn open_session(
    cli: &mut Cli,
    account: &str,
    host: Option<&str>,
    user: Option<&str>,
    store: bool,
    edits: &Edits,
) -> Result<String> {
    let mut p: Cfg = indexmap::IndexMap::new();
    p.insert("account".into(), Cv::Str(account.into()));
    p.insert(
        "authenticator".into(),
        Cv::Str("OAUTH_AUTHORIZATION_CODE".into()),
    );
    p.insert("client_store_temporary_credential".into(), Cv::Bool(store));
    p.insert(
        "oauth_scope".into(),
        Cv::Str(oauth_scope(edits.role.as_deref())),
    );
    p.insert("oauth_enable_refresh_tokens".into(), Cv::Bool(true));
    if let Some(u) = user {
        p.insert("user".into(), Cv::Str(u.into()));
    }
    if let Some(h) = host {
        p.insert("host".into(), Cv::Str(h.into()));
    }
    for (k, v) in [
        ("role", &edits.role),
        ("warehouse", &edits.warehouse),
        ("database", &edits.database),
        ("schema", &edits.schema),
    ] {
        if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
            p.insert(k.into(), Cv::Str(v.clone()));
        }
    }
    let fail = |e: String| {
        SnowError::cli(format!(
            "Sign-in failed: {}. Check the account and try again.",
            e.trim().trim_end_matches('.')
        ))
        .named("CliConnectionError")
    };
    let params = snow_api::connect::build_params(cli, p)?;
    let mut conn = snow_api::connect::connect(cli, params).map_err(|e| fail(e.message()))?;
    let c = conn
        .execute("SELECT CURRENT_USER()")
        .map_err(|e| fail(e.msg()))?;
    conn.close();
    let u = Cli::first_cell_opt(&c).unwrap_or_default();
    if u.trim().is_empty() {
        return Err(
            SnowError::cli("Sign-in did not return a user. Try again.").named("CliConnectionError")
        );
    }
    Ok(u.trim().to_string())
}

/// `AuthManager.login` after the argument checks: two connects (discover the
/// session user, then cache under it), then write and select the connection.
fn login_after_checks(cli: &mut Cli, raw_account: &str, name: Option<String>) -> Result<Output> {
    let (account, host) = sign_in_target(raw_account, cli.str("host").as_deref())?;
    let role = cli.str("role");
    let edits = Edits {
        role: role.clone(),
        warehouse: cli.str("warehouse"),
        database: cli.str("database"),
        schema: cli.str("schema"),
        clear_warehouse: cli.flag("clear_warehouse"),
        clear_database: cli.flag("clear_database"),
        clear_schema: cli.flag("clear_schema"),
    };
    let all = cli.config.connections();
    let saved = local_oauth_for(&all, &account);
    let discovery_host = host
        .clone()
        .or_else(|| shared_or_named_host(&all, &saved, &account));
    let session_user = open_session(
        cli,
        &account,
        discovery_host.as_deref(),
        None,
        false,
        &edits,
    )?;
    let mine: Vec<&(String, Cfg)> = saved
        .iter()
        .filter(|(_, c)| matches(c, &account, &session_user, role.as_deref()))
        .collect();
    let store_host = host.clone().or_else(|| {
        if mine.len() == 1 {
            return Some(s(&mine[0].1, "host")).filter(|h| !h.is_empty());
        }
        let users: Vec<(String, Cfg)> = saved
            .iter()
            .filter(|(_, c)| same_user(&s(c, "user"), &session_user))
            .cloned()
            .collect();
        shared_or_named_host(&all, &users, &account)
    });
    let confirmed = open_session(
        cli,
        &account,
        store_host.as_deref(),
        Some(&session_user),
        true,
        &edits,
    )?;
    if confirmed.to_uppercase() != session_user.to_uppercase() {
        return Err(SnowError::cli(format!(
            "Signed in as {confirmed}. The first sign-in was {session_user}. Sign in as the same user and try again."
        ))
        .named("CliConnectionError"));
    }
    let existing = if mine.len() == 1 {
        Some(mine[0].0.clone())
    } else {
        None
    };
    let (target, source, note) = choose_name(
        cli,
        &all,
        (&account, &session_user, role.as_deref()),
        existing,
        name,
    )?;
    let mut fields: Cfg = match source.as_ref().and_then(|n| all.get(n)) {
        Some(c) => c
            .iter()
            .filter(|(_, v)| !matches!(v, Cv::Str(x) if x.is_empty()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        None => indexmap::IndexMap::new(),
    };
    fields.insert("account".into(), Cv::Str(account.clone()));
    fields.insert("user".into(), Cv::Str(session_user.clone()));
    if let Some(h) = &store_host {
        fields.insert("host".into(), Cv::Str(h.clone()));
    }
    fields.insert(
        "authenticator".into(),
        Cv::Str("OAUTH_AUTHORIZATION_CODE".into()),
    );
    edits.apply(&mut fields);
    fields.insert("oauth_scope".into(), Cv::Str(oauth_scope(role.as_deref())));
    fields.insert("client_store_temporary_credential".into(), Cv::Bool(true));
    fields.insert("oauth_enable_refresh_tokens".into(), Cv::Bool(true));
    let table = snow_api::config::connection_table(fields);
    let written = cli.config.add_connection(&target, table)?;
    cli.config.set_value(
        &["default_connection_name"],
        toml_edit::value(target.clone()),
    )?;
    if let Some(src) = &source
        && *src != target
    {
        cli.config.remove_connection(src)?;
    }
    let mut msg = format!(
        "Wrote connection {target} to {} and changed the default connection to {target}.",
        written.display()
    );
    if let Some(n) = note {
        msg = format!("{n} {msg}");
    }
    Ok(Output::message(msg))
}
