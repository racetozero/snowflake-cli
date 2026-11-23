//! `snow spcs image-registry`.

use snow_api::output::{Output, Pv};
use snow_api::{Cli, Result, SnowError};

pub fn token(cli: &mut Cli) -> Result<serde_json::Value> {
    cli.execute_query("alter session set PYTHON_CONNECTOR_QUERY_RESULT_FORMAT = 'json'")?;
    let v = cli
        .connection()?
        .issue_token()
        .map_err(|e| SnowError::unexpected(e.msg()))?;
    Ok(serde_json::json!({
        "token": v["data"]["sessionToken"],
        "expires_in": v["data"]["validityInSecondsST"],
    }))
}

pub fn token_cmd(cli: &mut Cli) -> Result<Output> {
    let t = token(cli)?;
    Ok(Output::object(vec![
        ("token", Pv::from_json(&t["token"])),
        ("expires_in", Pv::from_json(&t["expires_in"])),
    ]))
}

pub fn registry_url(cli: &mut Cli, private_link: bool) -> Result<String> {
    let c = cli.execute_query("show image repositories in schema snowflake.images;")?;
    let mut rows = cli.rows(&c);
    if rows.is_empty() {
        let c = cli.execute_query("show image repositories in account")?;
        rows = cli.rows(&c);
        if rows.is_empty() {
            return Err(SnowError::click(
                "No image repository found. To run this command, please switch to a role with read access to at least one image repository or create a new image repository first.",
            )
            .named("NoImageRepositoriesFoundError"));
        }
    }
    let first = &rows[0];
    let mut url = if private_link {
        match first.get("privatelink_repository_url") {
            Some(v) => v.py_str(),
            None => return Ok(String::new()),
        }
    } else {
        first
            .get("repository_url")
            .map(|v| v.py_str())
            .unwrap_or_default()
    };
    if !regex::Regex::new(r"^.*//.+$").expect("re").is_match(&url) {
        url = format!("//{url}");
    }
    let after = url.split_once("//").map(|(_, r)| r).unwrap_or("");
    Ok(after
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_string())
}

pub fn url(cli: &mut Cli) -> Result<Output> {
    let pl = cli.flag("private_link");
    Ok(Output::message(registry_url(cli, pl)?))
}

pub fn login(cli: &mut Cli) -> Result<Output> {
    let pl = cli.flag("private_link");
    let url = registry_url(cli, pl)?;
    let t = token(cli)?;
    let payload = snow_api::pyfmt::json_dumps(&t, None);
    let child = std::process::Command::new("docker")
        .args([
            "login",
            "--username",
            "0sessiontoken",
            "--password-stdin",
            &url,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => return Err(SnowError::click("Docker is not installed.")),
    };
    {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.as_bytes());
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| SnowError::unexpected(e.to_string()))?;
    if !out.status.success() {
        return Err(SnowError::click(
            format!("Login Failed: {}", String::from_utf8_lossy(&out.stderr))
                .trim()
                .to_string(),
        ));
    }
    Ok(Output::message(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

/// `RegistryManager.login_to_registry`: basic auth with the session token, returns a bearer token.
pub fn login_to_registry(cli: &mut Cli, repo_url: &str) -> Result<String> {
    use base64::Engine;
    let t = token(cli)?;
    let tok = snow_api::pyfmt::json_dumps(&t, None);
    let u = url::Url::parse(repo_url).map_err(|e| SnowError::unexpected(e.to_string()))?;
    let host = match u.port() {
        Some(p) => format!("{}:{p}", u.host_str().unwrap_or_default()),
        None => u.host_str().unwrap_or_default().to_string(),
    };
    let login = format!("{}://{host}/login", u.scheme());
    let creds = base64::engine::general_purpose::STANDARD.encode(format!("0sessiontoken:{tok}"));
    let agent = snow_connector::http_agent(std::time::Duration::from_secs(60));
    match agent
        .get(&login)
        .set("Authorization", &format!("Basic {creds}"))
        .call()
    {
        Ok(r) => {
            let body = r.into_string().unwrap_or_default();
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            Ok(v["token"].as_str().unwrap_or_default().to_string())
        }
        Err(ureq::Error::Status(_, r)) => Err(SnowError::click(format!(
            "Failed to login to the repository {}",
            r.into_string().unwrap_or_default()
        ))),
        Err(e) => Err(SnowError::unexpected(e.to_string())),
    }
}
