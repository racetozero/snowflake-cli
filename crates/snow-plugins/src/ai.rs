//! `snow ai claude` / `snow ai opencode`: launch a coding agent pointed at the
//! account's AI Gateway, with the connection's bearer token.

use serde_json::json;
use snow_api::output::Output;
use snow_api::pyfmt::json_dumps;
use snow_api::{Cli, Result, SnowError};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_GATEWAY: &str = "SNOWFLAKE";
const OPENCODE_PROVIDER: &str = "snowflake-cortex";
const OPENCODE_MODEL: &str = "openai-gpt-5.4";

fn err(m: impl Into<String>) -> SnowError {
    SnowError::cli(m.into())
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

fn find_agent(name: &str, fallback: Option<PathBuf>) -> Result<PathBuf> {
    which(name).or_else(|| fallback.filter(|f| is_executable(f))).ok_or_else(|| {
        err(format!("{name} command not found, make sure it is installed on your system and in your PATH"))
    })
}

fn refuse_conflicting_env(names: &[&str]) -> Result<()> {
    let found: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| std::env::var(n).is_ok_and(|v| !v.is_empty()))
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    let listed = found.join(", ");
    Err(err(format!(
        "Refusing to launch: {listed} is set in the environment.\nThe agent reads these in preference to the settings this command passes, so it would authenticate with a credential that was never validated, or send traffic somewhere other than the AI Gateway.\nUnset {listed} and try again."
    )))
}

struct Gateway {
    base_url: String,
    token: String,
    telemetry: bool,
}

fn resolve_gateway(cli: &mut Cli) -> Result<Gateway> {
    let name = cli
        .str("gateway_name")
        .unwrap_or_else(|| DEFAULT_GATEWAY.into());
    if !regex::Regex::new(r"^[A-Za-z0-9_$]+\n?$")
        .expect("re")
        .is_match(&name)
    {
        return Err(err(format!(
            "Invalid gateway name {}.\nA gateway name may contain only letters, digits, underscores and dollar signs (default: {DEFAULT_GATEWAY}). The gateway address is always built from the account host of the active connection.",
            snow_click::parse::py_repr(&name)
        )));
    }
    let (host, telemetry) = match cli.connection() {
        Ok(c) => (
            c.host.clone(),
            c.parameters
                .get("CLIENT_TELEMETRY_ENABLED")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ),
        Err(_) => {
            return Err(err(
                "Could not connect: Check authentication and network settings with snow connection test.\nThe gateway credential is derived from the connection, so fix the connection first (see `snow connection test`).",
            ));
        }
    };
    if host.is_empty() {
        return Err(err(
            "Could not derive the gateway URL: connection did not expose a host.",
        ));
    }
    let mut host = host;
    if regex::Regex::new(r"^[A-Za-z0-9_-]+\.snowflakecomputing\.(com|cn)$")
        .expect("re")
        .is_match(&host)
    {
        host = host.replace('_', "-");
    }
    let base_url = format!(
        "https://{}/api/v2/aigateways/{name}/",
        host.trim_end_matches('/').to_lowercase()
    );
    let token = bearer_token(cli)?;
    Ok(Gateway {
        base_url,
        token,
        telemetry,
    })
}

fn bearer_token(cli: &mut Cli) -> Result<String> {
    let params = match cli.connection_parameters_preview() {
        Ok(p) => p,
        Err(_) => {
            return Err(err(
                "Could not read the connection configuration: Check the selected connection's TOML and environment settings.\nThe gateway credential comes from the connection, so fix the connection first (see `snow connection test`).",
            ));
        }
    };
    let get = |k: &str| params.get(k).map(|v| v.py_str()).filter(|v| !v.is_empty());
    let authenticator = get("authenticator").unwrap_or_default().to_lowercase();
    if authenticator == "programmatic_access_token" || authenticator == "oauth" {
        let token = match get("token_file_path") {
            Some(path) => match std::fs::read(&path)
                .ok()
                .filter(|b| b.len() <= 1024 * 1024)
                .and_then(|b| String::from_utf8(b).ok())
            {
                Some(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
                Some(_) => Err("token_file_path is empty"),
                None => Err("could not read token_file_path; check its encoding and permissions"),
            },
            None => {
                get("token").ok_or("no token or token_file_path is configured for this connection")
            }
        };
        let token = token.map_err(|e| {
            err(format!(
                "Connection is configured for {authenticator} but {e}.\nSet token (or token_file_path) for the connection and try again."
            ))
        })?;
        if authenticator == "oauth" {
            cli.console.stderr_warning(
                "OAuth credentials are copied at launch and are not refreshed inside the agent. If the token expires, obtain a valid token and relaunch snow ai. Connector refresh settings do not renew a running agent's token.",
            );
        }
        return Ok(token);
    }
    if authenticator == "oauth_authorization_code" {
        // Upstream's renewal helper re-executes the Python interpreter; frozen
        // (standalone) builds refuse, and so does this binary.
        return Err(err(
            "Renewable OAuth is not yet supported by standalone Snow CLI executables.",
        ));
    }
    let shown = if authenticator.is_empty() {
        "not set".to_string()
    } else {
        authenticator
    };
    Err(err(format!(
        "Could not derive a credential for the AI Gateway from this connection (authenticator: {shown}).\nConfigure a programmatic access token: authenticator = PROGRAMMATIC_ACCESS_TOKEN with token or token_file_path.\nOAuth is also supported: oauth with token or token_file_path, or oauth_authorization_code with credential caching enabled.\nKeypair (SNOWFLAKE_JWT), OAuth client credentials and workload identity are not supported yet."
    )))
}

fn launch(bin: &Path, args: &[String], env: BTreeMap<String, String>) -> Result<Output> {
    let status = std::process::Command::new(bin)
        .args(args)
        .env_clear()
        .envs(env)
        .status()
        .map_err(|e| SnowError::unexpected(e.to_string()))?;
    #[cfg(unix)]
    let code = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        -status.signal().unwrap_or(1)
    });
    #[cfg(not(unix))]
    let code = status.code().unwrap_or(1);
    if code != 0 {
        return Err(SnowError::Click(snow_click::ClickError::Exit(code)));
    }
    Ok(Output::Empty)
}

fn agent_args(cli: &Cli) -> Vec<String> {
    cli.inv.leaf().args.clone()
}

pub fn claude(cli: &mut Cli) -> Result<Output> {
    let bin = find_agent("claude", None)?;
    refuse_conflicting_env(&[
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
    ])?;
    let gw = resolve_gateway(cli)?;
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    if gw.telemetry {
        for (k, v) in [
            ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
            ("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA", "1"),
            ("CLAUDE_CODE_PROPAGATE_TRACEPARENT", "1"),
            ("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1"),
            ("OTEL_TRACES_EXPORTER", "console"),
        ] {
            env.entry(k.into()).or_insert_with(|| v.into());
        }
    }
    env.insert("ANTHROPIC_BASE_URL".into(), gw.base_url);
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), gw.token);
    let existing = std::env::var("ANTHROPIC_CUSTOM_HEADERS").unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    let present: Vec<String> = lines
        .iter()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, _)| k.trim().to_lowercase())
        .collect();
    for (k, v) in [
        ("X-Snowflake-Application", "snowflake-cli"),
        ("snow-agent-name", "claude"),
    ] {
        if !present.contains(&k.to_lowercase()) {
            lines.push(format!("{k}: {v}"));
        }
    }
    env.insert("ANTHROPIC_CUSTOM_HEADERS".into(), lines.join("\n"));
    launch(&bin, &agent_args(cli), env)
}

pub fn opencode(cli: &mut Cli) -> Result<Output> {
    let fallback =
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".opencode/bin/opencode"));
    let bin = find_agent("opencode", fallback)?;
    refuse_conflicting_env(&["SNOWFLAKE_CORTEX_TOKEN", "SNOWFLAKE_CORTEX_PAT"])?;
    let gw = resolve_gateway(cli)?;
    let base_url = format!("{}/v1", gw.base_url.trim_end_matches('/'));
    let account = url::Url::parse(&base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| base_url.clone());
    let model = format!("{OPENCODE_PROVIDER}/{OPENCODE_MODEL}");
    let config = json!({
        "$schema": "https://opencode.ai/config.json",
        "model": model,
        "small_model": model,
        "provider": {
            OPENCODE_PROVIDER: {
                "options": {
                    "baseURL": base_url,
                    "account": account,
                    "apiKey": gw.token,
                    "headers": {"X-Snowflake-Application": "snowflake-cli", "snow-agent-name": "opencode"},
                },
                "models": {OPENCODE_MODEL: {"options": {"reasoningEffort": "none"}}},
            }
        },
    });
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    env.insert("OPENCODE_CONFIG_CONTENT".into(), json_dumps(&config, None));
    cli.console.stderr_warning(&format!(
        "Claude models are not currently served through the AI Gateway for OpenCode and fail with a 503. Defaulting to {OPENCODE_MODEL}.\nOverride with: snow ai opencode --agent-args -m {OPENCODE_PROVIDER}/<model>"
    ));
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        std::thread::sleep(std::time::Duration::from_secs(4));
    }
    launch(&bin, &agent_args(cli), env)
}
