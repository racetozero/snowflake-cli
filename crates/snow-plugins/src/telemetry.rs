//! In-band telemetry sent through the open connection (`_app/telemetry.py`).

use snow_api::{Cli, SnowError};

const INTERACTIVE: &[&str] = &[
    "EXTERNALBROWSER",
    "OAUTH_AUTHORIZATION_CODE",
    "USERNAME_PASSWORD_MFA",
];

/// The authenticator a connection would use, without connecting.
fn configured_authenticator(cli: &mut Cli) -> Option<String> {
    let params = cli.connection_parameters_preview().ok()?;
    if let Some(a) = params
        .get("authenticator")
        .map(|v| v.py_str())
        .filter(|a| !a.trim().is_empty())
    {
        return Some(a.trim().to_uppercase());
    }
    Some(
        std::env::var("SNOWFLAKE_AUTHENTICATOR")
            .unwrap_or_default()
            .trim()
            .to_uppercase(),
    )
}

/// `log_command_usage`: telemetry rides on the connection, so like the
/// Python CLI this logs in when that would not prompt (errors are ignored and
/// remembered, so the command does not dial a second time).
pub fn pre_execute(cli: &mut Cli) {
    // `snow auth login` connects outside the CLI context; its events never open
    // (or borrow) the ambient connection.
    if is_auth_login(cli) {
        return;
    }
    if cli.connection_if_open().is_some() {
        send(cli, "executing_command", None);
        return;
    }
    let Some(auth) = configured_authenticator(cli) else {
        return;
    };
    if INTERACTIVE.contains(&auth.as_str()) {
        return;
    }
    if cli.connection().is_ok() {
        send(cli, "executing_command", None);
    }
}

fn is_auth_login(cli: &Cli) -> bool {
    let names: Vec<&str> = cli
        .inv
        .levels
        .iter()
        .skip(1)
        .map(|l| l.info_name.as_str())
        .collect();
    names == ["auth", "login"]
}

pub fn command_succeeded(cli: &mut Cli) {
    send(cli, "executing_command", None);
}

pub fn command_failed(cli: &mut Cli, e: &SnowError) {
    send(cli, "executing_command", Some(e));
}

fn send(cli: &mut Cli, kind: &str, err: Option<&SnowError>) {
    if is_auth_login(cli) {
        return;
    }
    let command: Vec<String> = cli
        .inv
        .levels
        .iter()
        .skip(1)
        .map(|l| l.info_name.clone())
        .collect();
    let elapsed = cli.started.elapsed().as_millis() as u64;
    let format = cli.format.name();
    let Some(conn) = cli.connection_if_open() else {
        return;
    };
    let mut message = serde_json::json!({
        "type": kind,
        "source": "snowcli",
        "version_cli": snow_api::VERSION,
        "command": command,
        "command_output_type": format,
        "command_execution_time": elapsed,
        "command_result_status": if err.is_some() { "failure" } else { "success" },
    });
    if let Some(e) = err {
        message["error_type"] = serde_json::json!(e.class_name());
    }
    let ts = chrono::Utc::now().timestamp_millis().to_string();
    conn.send_telemetry(serde_json::json!([{ "message": message, "timestamp": ts }]));
}
