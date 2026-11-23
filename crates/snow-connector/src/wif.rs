//! Workload identity attestation (`wif_util.py`) for AWS, Azure and GCP.
//!
//! AWS follows botocore's default credential chain for the cases a workload
//! uses (environment, web identity token, shared credentials/config files,
//! ECS container credentials, EC2 instance metadata) and signs a
//! `GetCallerIdentity` request with SigV4 exactly as `SigV4Auth` does.

use crate::cloud::xml_text;
use crate::{DbError, ErrorKind, hex};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

const AUDIENCE: &str = "snowflakecomputing.com";
const ENTRA_RESOURCE: &str = "api://fd3f753b-eed3-462c-b6a7-a4b5bb650aad";
/// `ER_WIF_CREDENTIALS_NOT_FOUND`.
const NOT_FOUND: i64 = 251_018;

fn fail(msg: impl Into<String>) -> DbError {
    DbError::new(ErrorKind::Programming, NOT_FOUND, None, msg.into())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build()
}

#[derive(Debug, Clone)]
pub struct AwsCreds {
    pub key_id: String,
    pub secret: String,
    pub token: Option<String>,
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

type Ini = std::collections::HashMap<String, std::collections::HashMap<String, String>>;

fn ini(path: &std::path::Path) -> Ini {
    let mut out = Ini::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    let mut section = String::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if l.starts_with('[') && l.ends_with(']') {
            section = l[1..l.len() - 1].trim().to_string();
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            out.entry(section.clone())
                .or_default()
                .insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

/// The active profile (`AWS_PROFILE`) and the parsed `~/.aws/config` (`AWS_CONFIG_FILE`).
fn aws_profile_config() -> (String, Ini) {
    let profile = env("AWS_PROFILE")
        .or_else(|| env("AWS_DEFAULT_PROFILE"))
        .unwrap_or_else(|| "default".into());
    let config_path: std::path::PathBuf = env("AWS_CONFIG_FILE")
        .map(Into::into)
        .unwrap_or_else(|| home().join(".aws/config"));
    (profile, ini(&config_path))
}

/// The `~/.aws/config` section of a profile.
fn profile_section(profile: &str) -> String {
    if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    }
}

fn home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
}

/// IMDSv2 token, then a metadata path.
fn imds(path: &str) -> Option<String> {
    if env("AWS_EC2_METADATA_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        return None;
    }
    // botocore: 1 second timeout, 1 attempt.
    let a = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(1))
        .timeout(Duration::from_secs(1))
        .build();
    let token = a
        .put("http://169.254.169.254/latest/api/token")
        .set("x-aws-ec2-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(1))
        .call()
        .ok()
        .and_then(|r| r.into_string().ok());
    let mut req = a
        .get(&format!("http://169.254.169.254{path}"))
        .timeout(Duration::from_secs(1));
    if let Some(t) = &token {
        req = req.set("x-aws-ec2-metadata-token", t);
    }
    req.call().ok()?.into_string().ok()
}

/// `get_aws_region()`.
pub fn aws_region() -> Result<String, DbError> {
    env("AWS_REGION")
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .or_else(|| imds("/latest/meta-data/placement/region").filter(|r| !r.trim().is_empty()))
        .ok_or_else(|| fail("No AWS region was found. Ensure the application is running on AWS."))
}

/// botocore's default credential chain, for the providers a workload uses.
pub fn aws_credentials(region: &str) -> Option<AwsCreds> {
    if let (Some(k), Some(s)) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
        return Some(AwsCreds {
            key_id: k,
            secret: s,
            token: env("AWS_SESSION_TOKEN").or_else(|| env("AWS_SECURITY_TOKEN")),
        });
    }
    if let (Some(file), Some(role)) = (env("AWS_WEB_IDENTITY_TOKEN_FILE"), env("AWS_ROLE_ARN")) {
        let token = std::fs::read_to_string(&file).ok()?;
        let session = env("AWS_ROLE_SESSION_NAME")
            .unwrap_or_else(|| format!("botocore-session-{}", chrono::Utc::now().timestamp()));
        let url = format!(
            "https://sts.{region}.amazonaws.com/?Action=AssumeRoleWithWebIdentity&Version=2011-06-15&RoleArn={}&RoleSessionName={}&WebIdentityToken={}",
            url::form_urlencoded::byte_serialize(role.as_bytes()).collect::<String>(),
            url::form_urlencoded::byte_serialize(session.as_bytes()).collect::<String>(),
            url::form_urlencoded::byte_serialize(token.trim().as_bytes()).collect::<String>()
        );
        let xml = agent().get(&url).call().ok()?.into_string().ok()?;
        return Some(AwsCreds {
            key_id: xml_text(&xml, "AccessKeyId")?,
            secret: xml_text(&xml, "SecretAccessKey")?,
            token: xml_text(&xml, "SessionToken"),
        });
    }
    let (profile, config) = aws_profile_config();
    let static_creds = |p: &str| -> Option<AwsCreds> {
        let creds_file: std::path::PathBuf = env("AWS_SHARED_CREDENTIALS_FILE")
            .map(Into::into)
            .unwrap_or_else(|| home().join(".aws/credentials"));
        let from = |m: &std::collections::HashMap<String, String>| -> Option<AwsCreds> {
            Some(AwsCreds {
                key_id: m.get("aws_access_key_id")?.clone(),
                secret: m.get("aws_secret_access_key")?.clone(),
                token: m
                    .get("aws_session_token")
                    .or_else(|| m.get("aws_security_token"))
                    .cloned(),
            })
        };
        ini(&creds_file)
            .get(p)
            .and_then(from)
            .or_else(|| config.get(&profile_section(p)).and_then(from))
    };
    // assume-role profile (`role_arn` + `source_profile`).
    if let Some(sec) = config.get(&profile_section(&profile))
        && let (Some(role), Some(source)) = (sec.get("role_arn"), sec.get("source_profile"))
    {
        let base = static_creds(source)?;
        let session = sec
            .get("role_session_name")
            .cloned()
            .unwrap_or_else(|| format!("botocore-session-{}", chrono::Utc::now().timestamp()));
        let auth = sts_authority(region);
        let xml = sts_call(
            &base,
            region,
            &format!("https://{auth}"),
            &auth,
            &[
                ("Action", "AssumeRole"),
                ("Version", "2011-06-15"),
                ("RoleArn", role),
                ("RoleSessionName", &session),
            ],
        )
        .ok()?;
        return Some(AwsCreds {
            key_id: xml_text(&xml, "AccessKeyId")?,
            secret: xml_text(&xml, "SecretAccessKey")?,
            token: xml_text(&xml, "SessionToken"),
        });
    }
    // SSO (`SSOProvider`): a profile with sso_account_id / sso_role_name.
    if let Some(sec) = config.get(&profile_section(&profile))
        && (sec.contains_key("sso_role_name") || sec.contains_key("sso_account_id"))
    {
        return sso_credentials(&profile, sec, &config);
    }
    if let Some(c) = static_creds(&profile) {
        return Some(c);
    }
    // `credential_process` (botocore ProcessProvider): shell-split, JSON on stdout.
    if let Some(cmd) = config
        .get(&profile_section(&profile))
        .and_then(|s| s.get("credential_process"))
    {
        PROCESS_ERROR.with(|e| *e.borrow_mut() = None);
        let fail_with = |m: String| {
            PROCESS_ERROR.with(|e| *e.borrow_mut() = Some(m));
            None
        };
        let argv = shell_split(cmd);
        let Some(prog) = argv.first() else {
            return fail_with("Error when retrieving credentials from custom-process: ".into());
        };
        let out = match std::process::Command::new(prog).args(&argv[1..]).output() {
            Ok(o) => o,
            Err(e) => {
                return fail_with(format!(
                    "[Errno {}] {}: '{prog}'",
                    e.raw_os_error().unwrap_or(2),
                    "No such file or directory"
                ));
            }
        };
        if !out.status.success() {
            return fail_with(format!(
                "Error when retrieving credentials from custom-process: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let v: Value = match serde_json::from_slice(&out.stdout) {
            Ok(v) => v,
            // A ValueError (the CLI shows it as an error panel), not a CredentialRetrievalError.
            Err(e) => {
                return fail_with(format!(
                    "\u{0}{}",
                    py_json_error(&String::from_utf8_lossy(&out.stdout), &e)
                ));
            }
        };
        if v.get("Version") != Some(&Value::from(1)) {
            let shown = match v.get("Version") {
                None => "<Version key not provided>".to_string(),
                Some(Value::String(s)) => s.clone(),
                Some(x) => x.to_string(),
            };
            return fail_with(format!(
                "Error when retrieving credentials from custom-process: Unsupported version '{shown}' for credential process provider, supported versions: 1"
            ));
        }
        for k in ["AccessKeyId", "SecretAccessKey"] {
            if v.get(k).is_none() {
                return fail_with(format!(
                    "Error when retrieving credentials from custom-process: Missing required key in response: '{k}'"
                ));
            }
        }
        return Some(AwsCreds {
            key_id: v["AccessKeyId"].as_str()?.into(),
            secret: v["SecretAccessKey"].as_str()?.into(),
            token: v["SessionToken"].as_str().map(Into::into),
        });
    }
    let container = env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
        .map(|r| format!("http://169.254.170.2{r}"))
        .or_else(|| env("AWS_CONTAINER_CREDENTIALS_FULL_URI"));
    if let Some(url) = container {
        let mut req = agent().get(&url);
        if let Some(t) = env("AWS_CONTAINER_AUTHORIZATION_TOKEN") {
            req = req.set("Authorization", &t);
        } else if let Some(f) = env("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE")
            && let Ok(t) = std::fs::read_to_string(f)
        {
            req = req.set("Authorization", t.trim());
        }
        let v: Value = serde_json::from_reader(req.call().ok()?.into_reader()).ok()?;
        return Some(AwsCreds {
            key_id: v["AccessKeyId"].as_str()?.into(),
            secret: v["SecretAccessKey"].as_str()?.into(),
            token: v["Token"].as_str().map(Into::into),
        });
    }
    let role = imds("/latest/meta-data/iam/security-credentials/")?;
    let role = role.lines().next()?.trim().to_string();
    let v: Value = serde_json::from_str(&imds(&format!(
        "/latest/meta-data/iam/security-credentials/{role}"
    ))?)
    .ok()?;
    Some(AwsCreds {
        key_id: v["AccessKeyId"].as_str()?.into(),
        secret: v["SecretAccessKey"].as_str()?.into(),
        token: v["Token"].as_str().map(Into::into),
    })
}

/// The regional STS authority (`_default_sts_endpoint`).
pub fn sts_authority(region: &str) -> String {
    if region.starts_with("cn-") {
        format!("sts.{region}.amazonaws.com.cn")
    } else {
        format!("sts.{region}.amazonaws.com")
    }
}

/// `SigV4Auth(creds, "sts", region).add_auth(request)` for the GetCallerIdentity POST;
/// the credential is base64 of `{"url", "method", "headers"}`.
fn aws_attestation_with_base(
    creds: &AwsCreds,
    region: &str,
    authority: &str,
    base_url: &str,
    amzdate: &str,
) -> String {
    let url = format!("{base_url}/?Action=GetCallerIdentity&Version=2011-06-15");
    let mut headers: Vec<(String, String)> = vec![
        ("Host".into(), authority.to_string()),
        ("X-Snowflake-Audience".into(), AUDIENCE.into()),
        ("X-Amz-Date".into(), amzdate.to_string()),
    ];
    if let Some(t) = &creds.token {
        headers.push(("X-Amz-Security-Token".into(), t.clone()));
    }
    let mut lower: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.trim().to_string()))
        .collect();
    lower.sort();
    let canon_headers: String = lower.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = lower
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let payload = hex(&Sha256::digest(b""));
    let canonical = format!(
        "POST\n/\nAction=GetCallerIdentity&Version=2011-06-15\n{canon_headers}\n{signed}\n{payload}"
    );
    headers.push((
        "Authorization".into(),
        crate::sigv4::authorization(
            &canonical,
            &signed,
            amzdate,
            region,
            "sts",
            &creds.key_id,
            &creds.secret,
        ),
    ));
    let mut h = serde_json::Map::new();
    for (k, v) in headers {
        h.insert(k, Value::String(v));
    }
    let doc = serde_json::json!({"url": url, "method": "POST", "headers": Value::Object(h)});
    B64.encode(py_json(&doc))
}

/// `json.dumps(obj)` (", " / ": " separators, insertion order).
fn py_json(v: &Value) -> String {
    match v {
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}: {}", Value::String(k.clone()), py_json(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Array(a) => format!("[{}]", a.iter().map(py_json).collect::<Vec<_>>().join(", ")),
        Value::String(s) => {
            let mut out = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                        let mut buf = [0u16; 2];
                        for u in c.encode_utf16(&mut buf) {
                            out.push_str(&format!("\\u{u:04x}"));
                        }
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        other => other.to_string(),
    }
}

/// `ER_INVALID_WIF_SETTINGS`.
const INVALID: i64 = 251_017;

fn invalid(msg: impl Into<String>) -> DbError {
    DbError::new(ErrorKind::Programming, INVALID, None, msg.into())
}

/// Connection options of workload identity federation.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub entra_resource: Option<String>,
    pub impersonation_path: Vec<String>,
    pub aws_use_outbound_token: bool,
    pub host: Option<String>,
}

/// `extract_iss_and_sub_without_signature_verification(jwt)` (PyJWT messages).
pub fn jwt_claims(token: &str) -> Result<(String, String), DbError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(invalid("Invalid JWT token: Not enough segments"));
    }
    let dec = |s: &str| {
        let mut t = s.to_string();
        while !t.len().is_multiple_of(4) {
            t.push('=');
        }
        base64::engine::general_purpose::URL_SAFE.decode(t.as_bytes())
    };
    let header = dec(parts[0]).map_err(|_| invalid("Invalid JWT token: Invalid header padding"))?;
    serde_json::from_slice::<serde_json::Map<String, Value>>(&header)
        .map_err(|_| invalid("Invalid JWT token: Invalid header string: must be a json object"))?;
    let payload =
        dec(parts[1]).map_err(|_| invalid("Invalid JWT token: Invalid payload padding"))?;
    let claims: serde_json::Map<String, Value> = serde_json::from_slice(&payload)
        .map_err(|_| invalid("Invalid JWT token: Invalid payload string: must be a json object"))?;
    match (claims.get("iss"), claims.get("sub")) {
        (Some(i), Some(s)) => Ok((
            i.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| i.to_string()),
            s.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| s.to_string()),
        )),
        _ => Err(fail("Token is missing 'iss' or 'sub' claims.")),
    }
}

/// `parse_workload_identity_host(host)`: `(authority, base_url)`.
pub fn parse_host(host: &str) -> Result<(String, String), DbError> {
    let h = host.trim();
    if h.is_empty() {
        return Err(invalid("workload_identity_host is empty"));
    }
    let full = if h.contains("://") {
        h.to_string()
    } else {
        format!("https://{h}")
    };
    let u = url::Url::parse(&full).map_err(|_| {
        invalid(format!(
            "workload_identity_host \"{full}\" does not contain a hostname"
        ))
    })?;
    if u.scheme() != "https" && u.scheme() != "http" {
        return Err(invalid(format!(
            "workload_identity_host \"{full}\" must use https or http, got scheme \"{}\"",
            u.scheme()
        )));
    }
    let Some(hostname) = u.host_str().filter(|h| !h.is_empty()) else {
        return Err(invalid(format!(
            "workload_identity_host \"{full}\" does not contain a hostname"
        )));
    };
    if !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(invalid(format!(
            "workload_identity_host \"{full}\" must not contain user info, a query or a fragment"
        )));
    }
    if !u.path().trim_end_matches('/').is_empty() {
        return Err(invalid(format!(
            "workload_identity_host \"{full}\" must not contain a path"
        )));
    }
    let authority = match u.port() {
        Some(p) => format!("{hostname}:{p}"),
        None => hostname.to_string(),
    };
    Ok((authority.clone(), format!("{}://{authority}", u.scheme())))
}

/// SigV4 (`sts`) for a form-encoded STS POST; returns the response XML.
fn sts_call(
    creds: &AwsCreds,
    region: &str,
    base_url: &str,
    authority: &str,
    form: &[(&str, &str)],
) -> Result<String, String> {
    let body: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter())
        .finish();
    let amzdate = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let ctype = "application/x-www-form-urlencoded; charset=utf-8";
    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), ctype.into()),
        ("host".into(), authority.into()),
        ("x-amz-date".into(), amzdate.clone()),
    ];
    if let Some(t) = &creds.token {
        headers.push(("x-amz-security-token".into(), t.clone()));
    }
    headers.sort();
    let canon: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "POST\n/\n\n{canon}\n{signed}\n{}",
        hex(&Sha256::digest(body.as_bytes()))
    );
    let auth = crate::sigv4::authorization(
        &canonical,
        &signed,
        &amzdate,
        region,
        "sts",
        &creds.key_id,
        &creds.secret,
    );
    let mut req = agent()
        .post(&format!("{base_url}/"))
        .set("Content-Type", ctype)
        .set("X-Amz-Date", &amzdate)
        .set("Authorization", &auth);
    if let Some(t) = &creds.token {
        req = req.set("X-Amz-Security-Token", t);
    }
    match req.send_string(&body) {
        Ok(r) => r.into_string().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(code, r)) => Err(format!(
            "HTTP {code}: {}",
            r.into_string().unwrap_or_default()
        )),
        Err(e) => Err(e.to_string()),
    }
}

thread_local! {
    /// A botocore error raised while reading credentials (it does not fall through).
    static PROCESS_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// `shlex.split(s)` (POSIX): quotes group, backslash escapes outside single quotes.
fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                has = true;
                for d in chars.by_ref() {
                    if d == '\'' {
                        break;
                    }
                    cur.push(d);
                }
            }
            '"' => {
                has = true;
                while let Some(d) = chars.next() {
                    match d {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                            cur.push(chars.next().unwrap_or_default())
                        }
                        d => cur.push(d),
                    }
                }
            }
            '\\' => {
                has = true;
                if let Some(d) = chars.next() {
                    cur.push(d);
                }
            }
            c if c.is_whitespace() => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                has = true;
                cur.push(c);
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// Python `json.loads` error text for the common failures.
fn py_json_error(text: &str, e: &serde_json::Error) -> String {
    let (line, col) = (e.line(), e.column());
    let offset: usize = text
        .split('\n')
        .take(line.saturating_sub(1))
        .map(|l| l.chars().count() + 1)
        .sum::<usize>()
        + col.saturating_sub(1);
    let m = e.to_string();
    let kind = if m.starts_with("key must be a string") {
        "Expecting property name enclosed in double quotes"
    } else if m.starts_with("trailing characters") {
        "Extra data"
    } else {
        "Expecting value"
    };
    if text.trim().is_empty() {
        return format!(
            "Expecting value: line 1 column {} (char {})",
            text.chars().count() + 1,
            text.chars().count()
        );
    }
    format!("{kind}: line {line} column {col} (char {offset})")
}

fn chain_error(m: String) -> Option<AwsCreds> {
    PROCESS_ERROR.with(|e| *e.borrow_mut() = Some(m));
    None
}

/// botocore `SSOProvider` + `SSOCredentialFetcher` with the cached token from
/// `~/.aws/sso/cache/<sha1>.json` (run `aws sso login` first; no token refresh).
fn sso_credentials(
    profile: &str,
    sec: &std::collections::HashMap<String, String>,
    config: &std::collections::HashMap<String, std::collections::HashMap<String, String>>,
) -> Option<AwsCreds> {
    let mut cfg = sec.clone();
    let session = sec.get("sso_session").cloned();
    let mut required = vec![
        "sso_role_name",
        "sso_account_id",
        "sso_start_url",
        "sso_region",
    ];
    if let Some(name) = &session {
        let Some(s) = config.get(&format!("sso-session {name}")) else {
            return chain_error(format!(
                "InvalidConfigError\u{1}The specified sso-session does not exist: \"{name}\""
            ));
        };
        for (k, v) in s {
            if let Some(cur) = cfg.get(k)
                && cur != v
            {
                return chain_error(format!(
                    "InvalidConfigError\u{1}The value for {k} is inconsistent between profile ({cur}) and sso-session ({v})."
                ));
            }
            cfg.insert(k.clone(), v.clone());
        }
        required = vec!["sso_role_name", "sso_account_id", "sso_region"];
    }
    let missing: Vec<&str> = required
        .iter()
        .filter(|k| !cfg.contains_key(**k))
        .copied()
        .collect();
    if !missing.is_empty() {
        return chain_error(format!(
            "InvalidConfigError\u{1}The profile \"{profile}\" is configured to use SSO but is missing required configuration: {}",
            missing.join(", ")
        ));
    }
    let start = cfg.get("sso_start_url").cloned().unwrap_or_default();
    let key_src = session.clone().unwrap_or_else(|| start.clone());
    use sha1::Digest as _;
    let key = hex(&sha1::Sha1::digest(key_src.as_bytes()));
    let path = home().join(".aws/sso/cache").join(format!("{key}.json"));
    let Some(tok) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return chain_error(format!(
            "SSOTokenLoadError\u{1}Error loading SSO Token: Token for {key_src} does not exist"
        ));
    };
    let (Some(access), Some(expires)) = (tok["accessToken"].as_str(), tok["expiresAt"].as_str())
    else {
        return chain_error(format!(
            "SSOTokenLoadError\u{1}Error loading SSO Token: Token for {start} is invalid"
        ));
    };
    let parse = |t: &str| {
        chrono::DateTime::parse_from_rfc3339(&t.replace("UTC", "Z"))
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    let remaining = parse(expires)
        .map(|t| (t - chrono::Utc::now()).num_seconds())
        .unwrap_or(-1);
    let unauthorized = "UnauthorizedSSOTokenError\u{1}The SSO session associated with this profile has expired or is otherwise invalid. To refresh this SSO session run aws sso login with the corresponding profile.";
    let region = cfg.get("sso_region").cloned().unwrap_or_default();
    let mut access = access.to_string();
    if session.is_none() {
        // Legacy profiles: an expired token is rejected before the call.
        if remaining <= 0 {
            return chain_error(unauthorized.into());
        }
    } else if remaining < 15 * 60 {
        // `SSOTokenProvider._refresher`: refresh with the cached client registration.
        let keys = [
            "refreshToken",
            "clientId",
            "clientSecret",
            "registrationExpiresAt",
        ];
        let registered = tok["registrationExpiresAt"]
            .as_str()
            .and_then(parse)
            .is_some_and(|t| t > chrono::Utc::now());
        if keys.iter().all(|k| tok.get(*k).is_some()) && registered {
            let oidc = env("AWS_ENDPOINT_URL_SSO_OIDC")
                .or_else(|| env("AWS_ENDPOINT_URL"))
                .unwrap_or_else(|| format!("https://oidc.{region}.amazonaws.com"));
            let body = serde_json::json!({"grantType": "refresh_token", "clientId": tok["clientId"], "clientSecret": tok["clientSecret"], "refreshToken": tok["refreshToken"]});
            if let Ok(r) = agent()
                .post(&format!("{oidc}/token"))
                .set("Content-Type", "application/json")
                .send_string(&body.to_string())
                && let Ok(v) = serde_json::from_reader::<_, Value>(r.into_reader())
                && let Some(new) = v["accessToken"].as_str()
            {
                let exp = chrono::Utc::now()
                    + chrono::Duration::seconds(v["expiresIn"].as_i64().unwrap_or(0));
                let mut cached = serde_json::json!({
                    "startUrl": start, "region": region, "accessToken": new,
                    "expiresAt": exp.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                    "clientId": tok["clientId"], "clientSecret": tok["clientSecret"], "registrationExpiresAt": tok["registrationExpiresAt"],
                });
                if let Some(rt) = v.get("refreshToken") {
                    cached["refreshToken"] = rt.clone();
                }
                let _ = std::fs::write(&path, cached.to_string());
                access = new.to_string();
            }
        }
    }
    let access = access.as_str();
    let base = env("AWS_ENDPOINT_URL_SSO")
        .or_else(|| env("AWS_ENDPOINT_URL"))
        .unwrap_or_else(|| format!("https://portal.sso.{region}.amazonaws.com"));
    let url = format!(
        "{base}/federation/credentials?role_name={}&account_id={}",
        url::form_urlencoded::byte_serialize(cfg["sso_role_name"].as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(cfg["sso_account_id"].as_bytes()).collect::<String>()
    );
    let v: Value = match agent()
        .get(&url)
        .set("x-amz-sso_bearer_token", access)
        .call()
    {
        Ok(r) => serde_json::from_reader(r.into_reader()).ok()?,
        Err(ureq::Error::Status(401, _)) => return chain_error(unauthorized.into()),
        Err(e) => return chain_error(e.to_string()),
    };
    let c = &v["roleCredentials"];
    Some(AwsCreds {
        key_id: c["accessKeyId"].as_str()?.into(),
        secret: c["secretAccessKey"].as_str()?.into(),
        token: c["sessionToken"].as_str().map(Into::into),
    })
}

/// `create_aws_attestation(impersonation_path, aws_use_outbound_token, workload_identity_host)`.
pub fn aws_attestation(opts: &Options) -> Result<String, DbError> {
    let region = aws_region()?;
    let (authority, base_url, overridden) = match &opts.host {
        Some(h) if !h.trim().is_empty() => {
            let (a, b) = parse_host(h)?;
            (a, b, true)
        }
        _ => {
            let a = sts_authority(&region);
            (a.clone(), format!("https://{a}"), false)
        }
    };
    // botocore builds an STS client to resolve the endpoint; an SSO profile naming a
    // missing sso-session fails there first.
    if !overridden {
        let (profile, config) = aws_profile_config();
        if let Some(name) = config
            .get(&profile_section(&profile))
            .and_then(|s| s.get("sso_session"))
            && !config.contains_key(&format!("sso-session {name}"))
        {
            return Err(fail(format!(
                "Could not resolve an STS endpoint for region '{region}': The profile \"{profile}\" is configured to use the SSO token provider but the \"{name}\" sso_session configuration does not exist."
            )));
        }
    }
    let creds = match aws_credentials(&region) {
        Some(c) => c,
        None => {
            if let Some(m) = PROCESS_ERROR.with(|e| e.borrow_mut().take()) {
                // -1: ValueError; -2: another exception (botocore's CredentialRetrievalError).
                return Err(match m.strip_prefix('\u{0}') {
                    Some(v) => DbError::new(ErrorKind::Programming, -1, None, v.to_string()),
                    None => DbError::new(ErrorKind::Programming, -2, None, m),
                });
            }
            return Err(fail(
                "No AWS credentials were found. Ensure the application is running on AWS with an IAM role attached.",
            ));
        }
    };
    let outbound = opts.aws_use_outbound_token
        || env("SNOWFLAKE_ENABLE_AWS_WIF_OUTBOUND_TOKEN")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    aws_attestation_from(&region, creds, opts, (authority, base_url), outbound)
}

fn aws_attestation_from(
    region: &str,
    mut creds: AwsCreds,
    opts: &Options,
    endpoint: (String, String),
    outbound: bool,
) -> Result<String, DbError> {
    let (authority, base_url) = endpoint;
    let region = region.to_string();
    // Role chaining (`get_aws_session`): each AssumeRole uses the previous credentials.
    for arn in &opts.impersonation_path {
        let xml = sts_call(&creds, &region, &base_url, &authority, &[("Action", "AssumeRole"), ("Version", "2011-06-15"), ("RoleArn", arn), ("RoleSessionName", "identity-federation-session")])
            .map_err(|e| fail(format!("An error occurred (AssumeRole) when calling the AssumeRole operation for role '{arn}': {e}")))?;
        creds = AwsCreds {
            key_id: xml_text(&xml, "AccessKeyId")
                .ok_or_else(|| fail("AssumeRole returned no credentials"))?,
            secret: xml_text(&xml, "SecretAccessKey")
                .ok_or_else(|| fail("AssumeRole returned no credentials"))?,
            token: xml_text(&xml, "SessionToken"),
        };
    }
    if outbound {
        let xml = sts_call(&creds, &region, &base_url, &authority, &[("Action", "GetWebIdentityToken"), ("Version", "2011-06-15"), ("Audience.member.1", AUDIENCE), ("SigningAlgorithm", "ES384")])
            .map_err(|e| fail(format!("An error occurred (GetWebIdentityToken) when calling the GetWebIdentityToken operation: {e}")))?;
        return xml_text(&xml, "WebIdentityToken")
            .ok_or_else(|| fail("GetWebIdentityToken returned no token"));
    }
    let amzdate = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    Ok(aws_attestation_with_base(
        &creds, &region, &authority, &base_url, &amzdate,
    ))
}

fn get_json(req: ureq::Request) -> Result<(Value, String), String> {
    match req.call() {
        Ok(r) => {
            let text = r.into_string().map_err(|e| e.to_string())?;
            Ok((serde_json::from_str(&text).unwrap_or(Value::Null), text))
        }
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            Err(format!("{code} Client Error. Response: {text}"))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// `create_azure_attestation(entra_resource, impersonation_path)`.
pub fn azure_attestation(opts: &Options) -> Result<String, DbError> {
    let resource = opts
        .entra_resource
        .clone()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| ENTRA_RESOURCE.to_string());
    let fed = env("AZURE_FEDERATED_TOKEN_FILE").unwrap_or_default();
    let aks = env("AZURE_CLIENT_ID").is_some()
        && env("AZURE_TENANT_ID").is_some()
        && !fed.is_empty()
        && std::path::Path::new(&fed).exists();
    let token = if aks {
        if !opts.impersonation_path.is_empty() {
            return Err(invalid(
                "workload_identity_impersonation_path is not supported on AKS.",
            ));
        }
        // azure-identity WorkloadIdentityCredential: client assertion from the federated token file.
        let assertion = std::fs::read_to_string(&fed).map_err(|e| fail(format!("Error fetching Azure MI token via WorkloadIdentityCredential: {e}. Ensure the application is running on AKS with workload identity configured.")))?;
        let tenant = env("AZURE_TENANT_ID").unwrap_or_default();
        let authority = env("AZURE_AUTHORITY_HOST")
            .unwrap_or_else(|| "https://login.microsoftonline.com".into());
        let r = agent()
            .post(&format!(
                "{}/{tenant}/oauth2/v2.0/token",
                authority.trim_end_matches('/')
            ))
            .send_form(&[
                ("client_assertion", assertion.trim()),
                (
                    "client_assertion_type",
                    "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                ),
                ("client_id", &env("AZURE_CLIENT_ID").unwrap_or_default()),
                ("grant_type", "client_credentials"),
                ("scope", &format!("{resource}/.default")),
            ]);
        let v: Value = r
            .map_err(|e| fail(format!("Error fetching Azure MI token via WorkloadIdentityCredential: {e}. Ensure the application is running on AKS with workload identity configured.")))
            .and_then(|r| serde_json::from_reader(r.into_reader()).map_err(|e| fail(e.to_string())))?;
        v["access_token"].as_str().unwrap_or_default().to_string()
    } else {
        if opts.impersonation_path.len() > 1 {
            return Err(invalid(
                "Azure WIF impersonation only supports a single service principal (single-hop). impersonation_path must contain exactly one client_id.",
            ));
        }
        let mi_resource = if opts.impersonation_path.is_empty() {
            resource.clone()
        } else {
            "api://AzureADTokenExchange".to_string()
        };
        let mut url = "http://169.254.169.254/metadata/identity/oauth2/token".to_string();
        let mut query = format!("api-version=2018-02-01&resource={mi_resource}");
        let mut headers = vec![("Metadata", "true".to_string())];
        if let Some(ep) = env("IDENTITY_ENDPOINT") {
            let Some(h) = env("IDENTITY_HEADER") else {
                return Err(fail(
                    "Managed identity is not enabled on this Azure function.",
                ));
            };
            url = ep;
            headers = vec![("X-IDENTITY-HEADER", h)];
            query = format!("api-version=2019-08-01&resource={mi_resource}");
        }
        if let Some(c) = env("MANAGED_IDENTITY_CLIENT_ID") {
            query.push_str(&format!("&client_id={c}"));
        }
        let mut req = agent().get(&format!("{url}?{query}"));
        for (k, v) in &headers {
            req = req.set(k, v);
        }
        let (v, _) = get_json(req).map_err(|e| {
            fail(format!(
                "Error fetching Azure metadata: {e}. Ensure the application is running on Azure."
            ))
        })?;
        let mut jwt = v["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| fail("No access token found in Azure metadata service response."))?
            .to_string();
        if let Some(sp) = opts.impersonation_path.first() {
            let claims = jwt
                .split('.')
                .nth(1)
                .and_then(|p| {
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(p.trim_end_matches('='))
                        .ok()
                })
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .unwrap_or(Value::Null);
            let Some(tid) = claims["tid"].as_str().map(str::to_string) else {
                return Err(fail(
                    "MI token is missing 'tid' claim; cannot determine tenant ID for impersonation.",
                ));
            };
            let r = agent()
                .post(&format!(
                    "https://login.microsoftonline.com/{tid}/oauth2/v2.0/token"
                ))
                .send_form(&[
                    ("grant_type", "client_credentials"),
                    ("client_id", sp),
                    (
                        "client_assertion_type",
                        "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                    ),
                    ("client_assertion", &jwt),
                    ("scope", &format!("{resource}/.default")),
                ]);
            let text = match r {
                Ok(r) => r.into_string().unwrap_or_default(),
                Err(ureq::Error::Status(code, r)) => {
                    let t = r.into_string().unwrap_or_default();
                    return Err(fail(format!(
                        "Error fetching SP token for Azure client_id '{sp}': {code} Client Error. Response: {t}"
                    )));
                }
                Err(e) => {
                    return Err(fail(format!(
                        "Error fetching SP token for Azure client_id '{sp}': {e}. Response: None"
                    )));
                }
            };
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            jwt = v["access_token"]
                .as_str()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    fail(format!(
                        "No access token found in Entra ID response for client_id '{sp}'."
                    ))
                })?
                .to_string();
        }
        jwt
    };
    jwt_claims(&token)?;
    Ok(token)
}

const GCP_BASE: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default";

/// `create_gcp_attestation(impersonation_path)`.
pub fn gcp_attestation(opts: &Options) -> Result<String, DbError> {
    let token = if opts.impersonation_path.is_empty() {
        let r = agent()
            .get(&format!("{GCP_BASE}/identity?audience={AUDIENCE}"))
            .set("Metadata-Flavor", "Google")
            .call();
        r.map_err(|e| {
            fail(format!(
                "Error fetching GCP identity token: {e}. Ensure the application is running on GCP."
            ))
        })?
        .into_string()
        .map_err(|e| fail(e.to_string()))?
    } else {
        let (v, _) = get_json(
            agent()
                .get(&format!("{GCP_BASE}/token"))
                .set("Metadata-Flavor", "Google"),
        )
        .map_err(|e| {
            fail(format!(
                "Error fetching GCP access token: {e}. Ensure the application is running on GCP."
            ))
        })?;
        let access = v["access_token"].as_str().unwrap_or_default().to_string();
        let path: Vec<String> = opts
            .impersonation_path
            .iter()
            .map(|c| format!("projects/-/serviceAccounts/{c}"))
            .collect();
        let last = path.last().cloned().unwrap_or_default();
        let body = serde_json::json!({"delegates": path[..path.len() - 1], "audience": AUDIENCE});
        let r = agent()
            .post(&format!(
                "https://iamcredentials.googleapis.com/v1/{last}:generateIdToken"
            ))
            .set("Authorization", &format!("Bearer {access}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let v: Value = r
            .map_err(|e| fail(format!("Error fetching GCP identity token for impersonated GCP service account '{last}': {e}. Ensure the application is running on GCP.")))
            .and_then(|r| serde_json::from_reader(r.into_reader()).map_err(|e| fail(e.to_string())))?;
        v["token"].as_str().unwrap_or_default().to_string()
    };
    jwt_claims(&token)?;
    Ok(token)
}

/// The connector's checks before a WORKLOAD_IDENTITY login.
pub fn validate(provider: &str, opts: &Options) -> Result<(), DbError> {
    if provider.is_empty() {
        return Err(invalid(
            "workload_identity_provider must be set to one of AWS,AZURE,GCP,OIDC when authenticator is WORKLOAD_IDENTITY.",
        ));
    }
    if !opts.impersonation_path.is_empty() && !["GCP", "AWS", "AZURE"].contains(&provider) {
        return Err(invalid(
            "workload_identity_impersonation_path is currently only supported for GCP, AWS, and AZURE.",
        ));
    }
    if let Some(h) = opts.host.as_ref().filter(|h| !h.is_empty()) {
        if provider != "AWS" {
            return Err(invalid("workload_identity_host is supported only for AWS"));
        }
        parse_host(h)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Credential produced by botocore's SigV4Auth for the same inputs (frozen time).
    #[test]
    fn aws_attestation_matches_botocore() {
        let c = AwsCreds {
            key_id: "AKIDEXAMPLE".into(),
            secret: "SECRET".into(),
            token: Some("TOK".into()),
        };
        let got = aws_attestation_with_base(
            &c,
            "us-west-2",
            "sts.us-west-2.amazonaws.com",
            "https://sts.us-west-2.amazonaws.com",
            "20240102T030405Z",
        );
        assert_eq!(
            String::from_utf8(B64.decode(got).unwrap()).unwrap(),
            EXPECTED
        );
    }

    const EXPECTED: &str = include_str!("../tests/data/aws_attestation.json");

    /// AssumeRole chain against a stub STS (workload_identity_host), then the signed
    /// GetCallerIdentity uses the last role's credentials and the overridden host.
    #[test]
    fn aws_role_chain_uses_overridden_host() {
        use std::io::{BufRead, BufReader, Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let b2 = bodies.clone();
        std::thread::spawn(move || {
            for i in 1..=2 {
                let (mut s, _) = l.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut len = 0;
                let mut auth = String::new();
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    let lower = h.to_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if lower.starts_with("authorization:") {
                        auth = h.trim().to_string();
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                b2.lock()
                    .unwrap()
                    .push((String::from_utf8(body).unwrap(), auth));
                let xml = format!(
                    "<R><Credentials><AccessKeyId>K{i}</AccessKeyId><SecretAccessKey>S{i}</SecretAccessKey><SessionToken>T{i}</SessionToken></Credentials></R>"
                );
                write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}",
                    xml.len()
                )
                .unwrap();
            }
        });
        let opts = Options {
            impersonation_path: vec![
                "arn:aws:iam::1:role/a".into(),
                "arn:aws:iam::1:role/b".into(),
            ],
            host: Some(format!("http://{addr}")),
            ..Default::default()
        };
        let (a, b) = parse_host(opts.host.as_deref().unwrap()).unwrap();
        let creds = AwsCreds {
            key_id: "K0".into(),
            secret: "S0".into(),
            token: None,
        };
        let cred = aws_attestation_from("us-west-2", creds, &opts, (a.clone(), b), false).unwrap();
        let bodies = bodies.lock().unwrap();
        assert_eq!(
            bodies[0].0,
            "Action=AssumeRole&Version=2011-06-15&RoleArn=arn%3Aaws%3Aiam%3A%3A1%3Arole%2Fa&RoleSessionName=identity-federation-session"
        );
        assert!(bodies[0].1.contains("Credential=K0/"));
        assert!(bodies[1].1.contains("Credential=K1/"));
        let doc: Value = serde_json::from_slice(&B64.decode(cred).unwrap()).unwrap();
        assert_eq!(
            doc["url"],
            format!("http://{a}/?Action=GetCallerIdentity&Version=2011-06-15")
        );
        assert_eq!(doc["headers"]["X-Amz-Security-Token"], "T2");
        assert!(
            doc["headers"]["Authorization"]
                .as_str()
                .unwrap()
                .contains("Credential=K2/")
        );
    }

    #[test]
    fn host_parsing_matches_connector() {
        assert_eq!(
            parse_host("sts.example.com").unwrap(),
            ("sts.example.com".into(), "https://sts.example.com".into())
        );
        assert_eq!(
            parse_host("http://h:8080/").unwrap(),
            ("h:8080".into(), "http://h:8080".into())
        );
        assert!(
            parse_host("https://h/p")
                .unwrap_err()
                .raw_msg
                .contains("must not contain a path")
        );
        assert!(
            parse_host("https://u@h")
                .unwrap_err()
                .raw_msg
                .contains("user info")
        );
    }
}
