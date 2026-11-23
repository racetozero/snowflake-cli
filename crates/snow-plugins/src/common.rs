//! Helpers shared by many commands.

use snow_api::{Result, SnowError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    pub key: String,
    pub value: String,
}

/// `parse_key_value_variables`.
pub fn parse_key_value_variables(vars: &[String]) -> Result<Vec<Variable>> {
    let mut out = Vec::new();
    for p in vars {
        let Some((k, v)) = p.split_once('=') else {
            return Err(SnowError::click(format!("Invalid variable: '{p}'")));
        };
        out.push(Variable {
            key: k.trim().to_string(),
            value: v.trim().to_string(),
        });
    }
    Ok(out)
}

/// `typer.prompt(text, default="", show_default=False)`: read one line from stdin.
/// Returns `None` for an empty answer. EOF aborts like Click.
pub fn prompt(cli: &mut snow_api::Cli, text: &str, hide_input: bool) -> Result<Option<String>> {
    use std::io::{BufRead, Write};
    if cli.agent {
        return Err(
            SnowError::cli(format!("{text} is required (agent mode never prompts)"))
                .named("InputRequired"),
        );
    }
    let p = format!("{text}: ");
    if hide_input {
        eprint!("{p}");
        let _ = std::io::stderr().flush();
    } else {
        print!("{p}");
        let _ = std::io::stdout().flush();
    }
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line).unwrap_or(0);
    if n == 0 {
        println!();
        return Err(SnowError::Click(snow_click::ClickError::Abort));
    }
    let v = line.trim_end_matches(['\n', '\r']).to_string();
    Ok(if v.is_empty() { None } else { Some(v) })
}

/// `typer.confirm(text)`.
pub fn confirm(cli: &mut snow_api::Cli, text: &str, default: bool) -> Result<bool> {
    use std::io::{BufRead, Write};
    if cli.agent {
        return Err(
            SnowError::cli(format!("{text} Re-run with --force to confirm."))
                .named("ConfirmationRequired"),
        );
    }
    let suffix = if default { " [Y/n]: " } else { " [y/N]: " };
    loop {
        print!("{text}{suffix}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let n = std::io::stdin().lock().read_line(&mut line).unwrap_or(0);
        if n == 0 {
            println!();
            return Err(SnowError::Click(snow_click::ClickError::Abort));
        }
        match line.trim().to_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Error: invalid input"),
        }
    }
}

pub fn cv_to_pv(v: &snow_api::config::Cv) -> snow_api::Pv {
    use snow_api::Pv;
    use snow_api::config::Cv;
    match v {
        Cv::Str(s) => Pv::Str(s.clone()),
        Cv::Int(i) => Pv::Int(*i as i128),
        Cv::Float(f) => Pv::Float(*f),
        Cv::Bool(b) => Pv::Bool(*b),
        Cv::List(l) => Pv::List(l.iter().map(cv_to_pv).collect()),
        Cv::Table(t) => Pv::Dict(t.iter().map(|(k, v)| (k.clone(), cv_to_pv(v))).collect()),
    }
}

/// `mask_sensitive_value`.
pub fn mask(key: &str, v: snow_api::Pv) -> snow_api::Pv {
    let k = key.to_lowercase();
    if ["private_key_file", "private_key_path", "token_file_path"]
        .iter()
        .any(|p| k.contains(p))
    {
        return v;
    }
    if [
        "password",
        "pwd",
        "oauth_client_secret",
        "token",
        "session_token",
        "master_token",
        "mfa_passcode",
        "private_key",
        "passphrase",
        "secret",
    ]
    .iter()
    .any(|f| k.contains(f))
    {
        return snow_api::Pv::s("****");
    }
    v
}

/// `snowflake.core` `APIError` (and subclasses) for a non-2xx REST response.
pub fn core_api_error(code: u16, raw: &str) -> SnowError {
    let mut msg = format!("({code})\nReason: {}\n", snow_connector::status_text(code));
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        let s = |k: &str| match &v[k] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        let message = s("message").trim().to_string();
        let code_s = if s("error_code").is_empty() {
            s("code")
        } else {
            s("error_code")
        };
        if !message.is_empty() {
            msg.push_str(&format!(
                "Error Message: {message}\nHTTP response code: {code}\nRequest ID: {}\nError Code: {code_s}",
                s("request_id")
            ));
        }
    }
    let name = match code {
        401 => "UnauthorizedError",
        403 => "ForbiddenError",
        404 => "NotFoundError",
        409 => "ConflictError",
        500..=599 => "ServerError",
        _ => "APIError",
    };
    SnowError::unexpected(msg).named(name)
}

/// `typer.prompt(text, default=default)`: `text [default]: `, empty answer means the default.
pub fn prompt_default(cli: &mut snow_api::Cli, text: &str, default: &str) -> Result<String> {
    let t = if default.is_empty() {
        text.to_string()
    } else {
        format!("{text} [{default}]")
    };
    Ok(prompt(cli, &t, false)?.unwrap_or_else(|| default.to_string()))
}

/// `click.launch(url)`: `open` (waited for) on macOS, `xdg-open` elsewhere.
pub fn launch(url: &str) {
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("open")
            .arg(url)
            .stderr(std::process::Stdio::null())
            .status();
    } else {
        snow_connector::auth::open_browser(url);
    }
}
