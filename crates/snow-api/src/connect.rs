//! `snowflake.cli._app.snow_connector.connect_to_snowflake`.

use crate::config::{Cv, FeatureFlag, try_cast_to_bool};
use crate::context::Cli;
use crate::errors::{Result, SnowError};
use indexmap::IndexMap;
use snow_connector::{ConnectParams, Connection, DbError};

pub fn build_params(cli: &mut Cli, p: IndexMap<String, Cv>) -> Result<ConnectParams> {
    let s = |p: &IndexMap<String, Cv>, k: &str| p.get(k).map(Cv::py_str);
    let authenticator = s(&p, "authenticator");
    let mut cp = ConnectParams {
        account: s(&p, "account"),
        user: s(&p, "user"),
        password: s(&p, "password"),
        host: s(&p, "host"),
        port: p.get("port").and_then(|v| match v {
            Cv::Int(i) => Some(*i as u16),
            other => other.py_str().parse().ok(),
        }),
        protocol: s(&p, "protocol"),
        database: s(&p, "database"),
        schema: s(&p, "schema"),
        warehouse: s(&p, "warehouse"),
        role: s(&p, "role"),
        authenticator: authenticator.clone(),
        token: s(&p, "token"),
        token_file_path: s(&p, "token_file_path"),
        session_token: s(&p, "session_token"),
        master_token: s(&p, "master_token"),
        workload_identity_provider: s(&p, "workload_identity_provider"),
        workload_identity_entra_resource: s(&p, "workload_identity_entra_resource"),
        workload_identity_impersonation_path: match p.get("workload_identity_impersonation_path") {
            Some(Cv::List(l)) => l.iter().map(Cv::py_str).collect(),
            Some(Cv::Str(v)) => v
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect(),
            _ => vec![],
        },
        workload_identity_aws_use_outbound_token: p
            .get("workload_identity_aws_use_outbound_token")
            .and_then(try_cast_to_bool)
            .unwrap_or(false),
        workload_identity_host: s(&p, "workload_identity_host"),
        oauth_client_id: s(&p, "oauth_client_id"),
        oauth_client_secret: s(&p, "oauth_client_secret"),
        oauth_authorization_url: s(&p, "oauth_authorization_url"),
        oauth_token_request_url: s(&p, "oauth_token_request_url"),
        oauth_redirect_uri: s(&p, "oauth_redirect_uri"),
        oauth_scope: s(&p, "oauth_scope"),
        oauth_disable_pkce: p
            .get("oauth_disable_pkce")
            .and_then(try_cast_to_bool)
            .unwrap_or(false),
        client_store_temporary_credential: p
            .get("client_store_temporary_credential")
            .and_then(try_cast_to_bool),
        server_session_keep_alive: p
            .get("server_session_keep_alive")
            .and_then(try_cast_to_bool),
        secondary_roles: s(&p, "secondary_roles"),
        application: Some(application_for(cli)),
        application_name: Some("snowcli".into()),
        ..ConnectParams::default()
    };
    let key_file = s(&p, "private_key_file");
    let key_raw = s(&p, "private_key_raw");
    if key_file.is_some() || key_raw.is_some() {
        if authenticator.as_deref().map(str::to_uppercase).as_deref() != Some("SNOWFLAKE_JWT") {
            return Err(SnowError::cli(
                "Private Key authentication requires authenticator set to SNOWFLAKE_JWT",
            ));
        }
        let pem = match key_file {
            Some(file) => {
                std::fs::read_to_string(crate::config::expand_user(&file)).map_err(|e| {
                    SnowError::unexpected(crate::errors::python_os_error(&e, Some(&file)))
                })?
            }
            None => key_raw.unwrap_or_default(),
        };
        let passphrase = std::env::var("PRIVATE_KEY_PASSPHRASE")
            .ok()
            .or_else(|| s(&p, "private_key_passphrase"));
        if pem.starts_with("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
            match &passphrase {
                None => {
                    return Err(SnowError::cli(
                        "Encrypted private key, you must provide the passphrase via the `private_key_file_pwd` key in your connection config or the `PRIVATE_KEY_PASSPHRASE` environment variable.",
                    ));
                }
                Some(x) if x.is_empty() => {
                    return Err(SnowError::cli(
                        "PRIVATE_KEY_PASSPHRASE environment variable is set but empty. Provide a non-empty passphrase or use an unencrypted private key.",
                    ));
                }
                _ => {}
            }
        } else if !pem.starts_with("-----BEGIN PRIVATE KEY-----") {
            return Err(SnowError::cli(
                "Private key provided is not in PKCS#8 format. Please use correct format.",
            ));
        }
        let der = snow_connector::auth::pem_to_der(&pem, passphrase.as_deref())
            .map_err(SnowError::unexpected)?;
        cp.private_key = Some(der);
    }
    if let Some(code) = &cli.conn.mfa_passcode {
        cp.passcode = Some(code.clone());
    }
    if authenticator.as_deref().map(str::to_uppercase).as_deref() == Some("USERNAME_PASSWORD_MFA") {
        cp.client_request_mfa_token = true;
    }
    if cp.session_token.is_some() && cp.master_token.is_some() {
        cp.server_session_keep_alive = Some(true);
        cp.client_session_keep_alive = Some(true);
    }
    if cli
        .config
        .feature(FeatureFlag::EnableSeparateAuthenticationPolicyId)
    {
        cp.internal_application_name = Some("SNOWFLAKE_CLI".into());
        cp.internal_application_version = Some(crate::VERSION.into());
    }
    Ok(cp)
}

/// `command_info()`: `SNOWCLI.<COMMAND.PATH>`.
pub(crate) fn application_for(cli: &Cli) -> String {
    let parts: Vec<String> = cli
        .inv
        .levels
        .iter()
        .skip(1)
        .map(|l| l.info_name.clone())
        .collect();
    if parts.is_empty() {
        "SNOWCLI".into()
    } else {
        format!("snowcli.{}", parts.join(".")).to_uppercase()
    }
}

pub fn db_error_to_cli(e: &DbError, verbose: bool) -> SnowError {
    SnowError::sql(e.msg_with(verbose)).named(match e.kind {
        snow_connector::ErrorKind::Programming => "ProgrammingError",
        snow_connector::ErrorKind::Database => "DatabaseError",
        snow_connector::ErrorKind::Operational => "OperationalError",
        snow_connector::ErrorKind::Forbidden => "ForbiddenError",
    })
}

pub fn connect(cli: &mut Cli, p: ConnectParams) -> Result<Connection> {
    let oauth = p.authenticator.as_deref().map(str::to_uppercase);
    let refresh = cli
        .conn
        .fields
        .get("oauth_enable_refresh_tokens")
        .and_then(try_cast_to_bool);
    let store = p.client_store_temporary_credential;
    Connection::connect(p).map_err(|e| {
        let mut msg = e.msg_with(cli.verbose || cli.debug);
        if let Some(h) = oauth_hint(&e, oauth.as_deref(), refresh, store) {
            msg = format!("{msg}\n{h}");
        }
        SnowError::cli(format!("Could not connect to Snowflake. Reason: {msg}"))
            .named("SnowflakeConnectionError")
    })
}

fn oauth_hint(
    e: &DbError,
    auth: Option<&str>,
    refresh: Option<bool>,
    store: Option<bool>,
) -> Option<String> {
    let auth = auth?;
    if auth != "OAUTH" && auth != "OAUTH_AUTHORIZATION_CODE" {
        return None;
    }
    let lower = e.raw_msg.to_lowercase();
    let oauth_error = matches!(e.errno, 390303 | 390318)
        || (e.errno == 250001
            && (lower.contains("oauth access token expired")
                || lower.contains("oauth access token invalid")
                || lower.contains("invalid oauth access token")));
    if !oauth_error {
        return None;
    }
    if auth == "OAUTH" {
        return Some("Snowflake CLI cannot renew a raw OAuth token supplied by its caller. Refresh it in the application or credential provider that supplied it, then retry. The refresh-token flag does not apply to authenticator=oauth.".into());
    }
    let mut hints = Vec::new();
    if refresh != Some(true) {
        hints.push("Silent OAuth refresh is disabled. To enable it, set oauth_enable_refresh_tokens = true in this connection, or pass --oauth-enable-refresh-tokens for this invocation.");
    }
    if store == Some(false) {
        hints.push("Credential caching is disabled. To reuse refresh tokens between commands, set client_store_temporary_credential = true.");
    }
    hints.push("Sign in again if no usable refresh token is available. Enabling refresh does not restore revoked credentials.");
    Some(hints.join(" "))
}
