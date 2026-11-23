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
    let names: Vec<&str> = cli
        .inv
        .levels
        .iter()
        .skip(1)
        .map(|l| l.info_name.as_str())
        .collect();
    if names == ["auth", "login"] {
        return;
    }
    if cli.connection_if_open().is_none() {
        let Some(auth) = configured_authenticator(cli) else {
            return;
        };
        if INTERACTIVE.contains(&auth.as_str()) {
            return;
        }
        let _ = cli.connection();
    }
    send(cli, None);
}

/// Queue an `executing_command` event; without an open connection it is dropped.
pub fn send(cli: &mut Cli, err: Option<&SnowError>) {
    let command: Vec<String> = cli
        .inv
        .levels
        .iter()
        .skip(1)
        .map(|l| l.info_name.clone())
        .collect();
    let elapsed = cli.started.elapsed().as_millis() as u64;
    let mut message = serde_json::json!({
        "type": "executing_command",
        "source": "snowcli",
        "version_cli": snow_api::VERSION,
        "command": command,
        "command_output_type": cli.format.name(),
        "command_execution_time": elapsed,
        "command_result_status": if err.is_some() { "failure" } else { "success" },
    });
    if let Some(e) = err {
        message["error_type"] = serde_json::json!(e.class_name());
    }
    let ts = chrono::Utc::now().timestamp_millis().to_string();
    cli.queue_telemetry(serde_json::json!({ "message": message, "timestamp": ts }));
}
