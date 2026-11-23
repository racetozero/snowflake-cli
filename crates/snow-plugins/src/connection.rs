//! `snow connection`.

use crate::common::{cv_to_pv, mask, prompt};
use indexmap::IndexMap;
use snow_api::config::{CONNECTION_FIELDS, Cv, connection_table, normalize_connection};
use snow_api::output::{Output, Pv, Row};
use snow_api::{Cli, Result, SnowError};
use toml_edit::value;

pub fn list(cli: &mut Cli) -> Result<Output> {
    let default = cli.config.default_connection_name();
    let mut rows = Vec::new();
    for (name, raw) in cli.config.connections() {
        let params: IndexMap<String, Pv> = normalize_connection(raw)
            .iter()
            .filter(|(k, _)| CONNECTION_FIELDS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), mask(k, cv_to_pv(v))))
            .collect();
        let mut r = Row::new();
        r.insert("connection_name".into(), Pv::s(name.clone()));
        r.insert("parameters".into(), Pv::Dict(params));
        r.insert("is_default".into(), Pv::Bool(name == default));
        rows.push(r);
    }
    if cli.config.connections_file.exists() {
        let msg = format!(
            "Reading connections from {}. Entries from config.toml are ignored.",
            cli.config.connections_file.display()
        );
        cli.console.warning(&msg);
    }
    Ok(Output::Collection(rows))
}

pub fn add(cli: &mut Cli) -> Result<Output> {
    let fields = [
        "connection_name",
        "account",
        "user",
        "password",
        "role",
        "warehouse",
        "database",
        "schema",
        "host",
        "port",
        "protocol",
        "region",
        "authenticator",
        "workload_identity_provider",
        "private_key_file",
        "token_file_path",
        "secondary_roles",
        "server_session_keep_alive",
        "client_store_temporary_credential",
    ];
    let mut opts: IndexMap<&str, Option<Cv>> = IndexMap::new();
    for f in fields {
        let v = match cli.param(f) {
            snow_click::Value::Str(s) => Some(Cv::Str(s.clone())),
            snow_click::Value::Int(i) => Some(Cv::Int(*i)),
            snow_click::Value::Bool(b) => Some(Cv::Bool(*b)),
            _ => None,
        };
        opts.insert(f, v);
    }
    if cli.agent && opts["connection_name"].is_none() {
        return Err(
            SnowError::cli("--connection-name is required (agent mode never prompts)")
                .named("InputRequired"),
        );
    }
    let no_interactive = cli.flag("no_interactive") || cli.agent;
    if !no_interactive {
        for f in fields {
            if matches!(
                f,
                "server_session_keep_alive" | "client_store_temporary_credential"
            ) {
                continue;
            }
            if opts[f].is_none() {
                let text = format!("Enter {}", f.replace('_', " "));
                let v = prompt(cli, &text, f == "password")?;
                opts.insert(f, v.map(Cv::Str));
            }
            if let Some(Cv::Str(s)) = &opts[f] {
                let t = s.trim().to_string();
                opts.insert(f, Some(Cv::Str(t)));
            }
        }
    }
    if let Some(v) = opts["port"].clone() {
        let n = match v {
            Cv::Int(i) => i,
            Cv::Str(s) => s.trim().parse::<i64>().map_err(|_| {
                SnowError::unexpected(format!(
                    "invalid literal for int() with base 10: {}",
                    snow_click::py_repr(&s)
                ))
            })?,
            _ => 0,
        };
        opts.insert("port", Some(Cv::Int(n)));
    }
    for k in ["private_key_file", "token_file_path"] {
        if let Some(v) = &opts[k] {
            let p = v.py_str();
            if !std::path::Path::new(&p).exists() {
                return Err(SnowError::usage(format!("Path {p} does not exist.")));
            }
        }
    }
    let name = opts
        .shift_remove("connection_name")
        .flatten()
        .map(|c| c.py_str())
        .unwrap_or_else(|| "None".into());
    if cli.config.connection_exists(&name) {
        return Err(SnowError::usage(format!(
            "Connection {name} already exists"
        )));
    }
    let known = opts
        .into_iter()
        .filter_map(|(k, v)| Some((k.to_string(), v?)))
        .collect();
    let file = cli.config.add_connection(&name, connection_table(known))?;
    if cli.flag("set_as_default") {
        cli.config
            .set_value(&["default_connection_name"], value(name.as_str()))?;
    }
    Ok(Output::message(format!(
        "Wrote new connection {name} to {}",
        file.display()
    )))
}

pub fn remove(cli: &mut Cli) -> Result<Output> {
    let name = cli.str("connection_name").unwrap_or_default();
    if !cli.config.connection_exists(&name) {
        return Err(SnowError::usage(format!(
            "Connection {name} does not exist."
        )));
    }
    let is_default = cli.config.default_connection_name() == name;
    if is_default {
        cli.config.unset_value(&["default_connection_name"])?;
    }
    let file = cli.config.remove_connection(&name)?;
    Ok(Output::message(format!(
        "Removed connection {name} from {}.{}",
        file.display(),
        if is_default {
            " It was the default connection, so default connection is now unset."
        } else {
            ""
        }
    )))
}

pub fn set_default(cli: &mut Cli) -> Result<Output> {
    let name = cli.str("name").unwrap_or_default();
    cli.config.connection_dict(&name)?;
    cli.config
        .set_value(&["default_connection_name"], value(name.as_str()))?;
    Ok(Output::message(format!(
        "Default connection set to: {name}"
    )))
}

pub fn test(cli: &mut Cli) -> Result<Output> {
    let print_diag = cli.flag("print_diag");
    if cli.conn.enable_diag && print_diag {
        cli.console
            .message("Running connector connectivity probe...");
    }
    let conn = cli.connection()?;
    let s = conn.session.clone();
    let host = conn.host.clone();
    let account = conn.account.clone();
    let user = conn.user.clone();
    let mut uses = Vec::new();
    if let Some(r) = &s.role {
        uses.push(format!("use role \"{r}\""));
    }
    if let Some(d) = &s.database {
        uses.push(format!("use database \"{d}\""));
    }
    if let Some(sc) = &s.schema {
        uses.push(format!("use schema \"{sc}\""));
    }
    if let Some(w) = &s.warehouse {
        uses.push(format!("use warehouse \"{w}\""));
    }
    for u in uses {
        if let Err(e) = cli.try_execute(&u) {
            return Err(SnowError::click(e.msg()));
        }
    }
    let s = cli.connection()?.session.clone();
    let mut r = Row::new();
    r.insert(
        "Connection name".into(),
        Pv::opt(cli.conn.connection_name.clone()),
    );
    r.insert("Status".into(), Pv::s("OK"));
    r.insert("Host".into(), Pv::s(host));
    r.insert("Account".into(), Pv::s(account));
    r.insert("User".into(), Pv::opt(user));
    r.insert(
        "Role".into(),
        Pv::s(s.role.unwrap_or_else(|| "not set".into())),
    );
    r.insert(
        "Database".into(),
        Pv::s(s.database.unwrap_or_else(|| "not set".into())),
    );
    r.insert(
        "Warehouse".into(),
        Pv::s(s.warehouse.unwrap_or_else(|| "not set".into())),
    );
    if cli.conn.enable_diag {
        let p = cli.conn.diag_log_path.clone().unwrap_or_default();
        r.insert(
            "Diag Report Location".into(),
            Pv::s(format!(
                "{}/SnowflakeConnectionTestReport.txt",
                p.trim_end_matches('/')
            )),
        );
    }
    Ok(Output::Object(Some(r)))
}

/// `update_from_config()`: the connection context merged with its config.
fn context_details(cli: &mut Cli) -> Result<IndexMap<String, Cv>> {
    let mut p = IndexMap::new();
    for (k, v) in &cli.conn.fields {
        p.insert(k.clone(), v.clone());
    }
    if !cli.conn.temporary_connection {
        if cli.conn.connection_name.is_none() {
            cli.conn.connection_name = Some(cli.config.default_connection_name());
        }
        let name = cli.conn.connection_name.clone().unwrap_or_default();
        let mut cfg = cli.config.connection_dict(&name)?;
        if let Some(v) = cfg.shift_remove("private_key_path") {
            cfg.insert("private_key_file".into(), v);
        }
        for (k, v) in cfg {
            p.entry(k).or_insert(v);
        }
    }
    Ok(p)
}

pub fn generate_jwt(cli: &mut Cli) -> Result<Output> {
    let d = context_details(cli)?;
    let tmpl = |w: &str| {
        SnowError::usage(format!(
            "{w} is not set in the connection context, but required for JWT generation."
        ))
    };
    let user = d.get("user").map(Cv::py_str).ok_or_else(|| tmpl("User"))?;
    let account = d
        .get("account")
        .map(Cv::py_str)
        .ok_or_else(|| tmpl("Account"))?;
    let key = d
        .get("private_key_file")
        .map(Cv::py_str)
        .ok_or_else(|| tmpl("Private key file"))?;
    let pem = std::fs::read_to_string(&key).map_err(|e| SnowError::click(e.to_string()))?;
    let pass = std::env::var("PRIVATE_KEY_PASSPHRASE")
        .ok()
        .or_else(|| d.get("private_key_passphrase").map(Cv::py_str));
    let der = match snow_connector::auth::pem_to_der(&pem, pass.as_deref()) {
        Ok(d) => d,
        Err(_) if pass.is_none() => {
            let p = prompt(
                cli,
                "Enter private key file password (press enter for empty)",
                true,
            )?
            .unwrap_or_default();
            snow_connector::auth::pem_to_der(&pem, Some(&p)).map_err(SnowError::click)?
        }
        Err(e) => return Err(SnowError::click(e)),
    };
    let token = snow_connector::auth::keypair_jwt(&account, &user, &der).map_err(token_error)?;
    Ok(Output::message(token))
}

pub fn generate_workload_identity_token(cli: &mut Cli) -> Result<Output> {
    let d = context_details(cli)?;
    let Some(provider) = d.get("workload_identity_provider").map(Cv::py_str) else {
        return Err(SnowError::usage(
            "Workload identity provider is not set in the connection context, but required for workload identity token generation.",
        ));
    };
    let provider = provider.to_uppercase();
    if provider == "OIDC" {
        let token = if let Some(t) = d.get("token") {
            t.py_str()
        } else if let Some(p) = d.get("token_file_path") {
            std::fs::read_to_string(p.py_str())
                .map_err(|e| SnowError::click(e.to_string()))?
                .trim()
                .to_string()
        } else {
            return Err(SnowError::usage(
                "OIDC provider requires a token. Set --token-file-path or configure 'token' in the connection.",
            ));
        };
        validate_jwt(&token)?;
        return Ok(Output::message(token));
    }
    let token = match provider.as_str() {
        // `create_attestation(provider=provider, token=token)`: no connection options.
        "AWS" => snow_connector::wif::aws_attestation(&Default::default()),
        "AZURE" => snow_connector::wif::azure_attestation(&Default::default()),
        "GCP" => snow_connector::wif::gcp_attestation(&Default::default()),
        _ => {
            return Err(SnowError::click(format!(
                "Unknown workload_identity_provider: '{provider}'. Expected one of: AWS, AZURE, GCP, OIDC"
            )));
        }
    }
    .map_err(token_error)?;
    Ok(Output::message(token))
}

/// A token generation error as a CLI error. Errors that are not
/// ProgrammingErrors (botocore's) show their own text.
fn token_error(e: snow_connector::DbError) -> SnowError {
    match e.errno {
        -1 => SnowError::click(e.raw_msg),
        -2 => match e.raw_msg.split_once('\u{1}') {
            Some((class, msg)) => SnowError::unexpected(msg.to_string()).named(match class {
                "InvalidConfigError" => "InvalidConfigError",
                "SSOTokenLoadError" => "SSOTokenLoadError",
                "UnauthorizedSSOTokenError" => "UnauthorizedSSOTokenError",
                _ => "CredentialRetrievalError",
            }),
            None => SnowError::unexpected(e.raw_msg).named("CredentialRetrievalError"),
        },
        n => SnowError::click(format!("{n}: {n}: {}", e.raw_msg)),
    }
}

/// PyJWT `decode(verify_signature=False)` plus the iss/sub check of `wif_util`.
fn validate_jwt(token: &str) -> Result<()> {
    use base64::Engine;
    let fail = |m: &str| SnowError::click(format!("251017: 251017: Invalid JWT token: {m}"));
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(fail("Not enough segments"));
    }
    let dec = |s: &str| {
        let mut t = s.to_string();
        while !t.len().is_multiple_of(4) {
            t.push('=');
        }
        base64::engine::general_purpose::URL_SAFE.decode(t.as_bytes())
    };
    let header = dec(parts[0]).map_err(|_| fail("Invalid header padding"))?;
    let _: serde_json::Value = serde_json::from_slice(&header)
        .map_err(|_| fail("Invalid header string: must be a json object"))?;
    let payload = dec(parts[1]).map_err(|_| fail("Invalid payload padding"))?;
    let claims: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|_| fail("Invalid payload string: must be a json object"))?;
    if claims.get("iss").is_none() || claims.get("sub").is_none() {
        return Err(SnowError::click(
            "251018: 251018: Token is missing 'iss' or 'sub' claims.",
        ));
    }
    Ok(())
}
