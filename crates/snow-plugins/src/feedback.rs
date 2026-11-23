//! `last_cli_error.json` (consumed by `snow feedback`).

use snow_api::output::Output;
use snow_api::{Cli, SnowError};

pub fn record_last_error(cli: &Cli, e: &SnowError) {
    if matches!(
        e,
        SnowError::Click(snow_click::ClickError::Exit(_))
            | SnowError::Click(snow_click::ClickError::Abort)
    ) {
        return;
    }
    let command = cli.command_name();
    if command == "feedback" || command.starts_with("feedback ") {
        return;
    }
    let Some(dir) = cli.config.file_path.parent() else {
        return;
    };
    let payload = serde_json::json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "error_type": e.class_name(),
        "command": command,
        "timestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    });
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(
        dir.join("last_cli_error.json"),
        snow_api::pyfmt::json_dumps(&payload, None),
    );
}

const PRIVACY_NOTICE: &str = "This command sends your message to Snowflake to improve Snowflake CLI.\nDo not include passwords, access tokens, private keys, account locators,\ndata from Snowflake accounts, SQL that contains customer data, or other\npersonal or confidential information.";
const ISSUES_URL: &str = "https://github.com/snowflakedb/snowflake-cli/issues";
const MAX_MESSAGE_BYTES: usize = 4000;
const RATE_LIMIT_MAX: usize = 5;
const RATE_WINDOW: f64 = 3600.0;

fn env_truthy(n: &str) -> bool {
    matches!(
        std::env::var(n).unwrap_or_default().to_lowercase().as_str(),
        "yes" | "true" | "1" | "on"
    )
}
fn env_set(n: &str) -> bool {
    !std::env::var(n).unwrap_or_default().trim().is_empty()
}
fn env_eq(n: &str, v: &str) -> bool {
    std::env::var(n).unwrap_or_default().trim() == v
}
fn cortex_token(n: &str) -> bool {
    let t = std::env::var(n).unwrap_or_default().trim().to_lowercase();
    let mut c = t.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_lowercase() || f.is_ascii_digit())
        && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_' || x == '-')
}

/// `_detect_agent_environment() != "UNKNOWN"` (telemetry agent detection).
pub fn is_coding_agent() -> bool {
    use std::io::IsTerminal;
    let has = |n: &str| std::env::var_os(n).is_some();
    let claude = env_truthy("CLAUDECODE") || env_set("CLAUDE_CODE");
    let path = std::env::var("PATH").unwrap_or_default();
    has("CORTEX_SESSION_ID")
        || cortex_token("CORTEX_CODE_CLIENT_SURFACE")
        || cortex_token("CORTEX_TERMINAL_LAUNCHER_SOURCE")
        || env_truthy("COCO_AGENT")
        || env_truthy("CURSOR_AGENT")
        || env_eq("CURSOR_EXTENSION_HOST_ROLE", "agent-exec")
        || env_eq("GROK_AGENT", "1")
        || env_set("GROK_PLUGIN_ROOT")
        || env_set("GROK_PLUGIN_DATA")
        || claude
        || env_truthy("GEMINI_CLI")
        || env_truthy("OPENCODE")
        || env_set("OPENCODE_CLIENT")
        || has("CODEX_API_KEY")
        || [
            "CODEX_SANDBOX",
            "CODEX_CI",
            "CODEX_THREAD_ID",
            "CODEX_SANDBOX_NETWORK_DISABLED",
        ]
        .iter()
        .any(|n| env_set(n))
        || env_truthy("COPILOT_CLI")
        || env_set("KIMI_PLUGIN_ROOT")
        || env_set("CLINE_ACTIVE")
        || env_set("ANTIGRAVITY_AGENT")
        || env_set("ANTIGRAVITY_CLI_ALIAS")
        || env_set("AUGMENT_AGENT")
        || env_eq("GOOSE_TERMINAL", "1")
        || env_set("JUNIE_SHIM_PATH")
        || path.contains(".pi/agent")
        || path.contains(".pi\\agent")
        || (std::env::var("TERM_PROGRAM")
            .unwrap_or_default()
            .to_lowercase()
            .contains("kiro")
            && !std::io::stdout().is_terminal())
        || env_set("OPENCLAW_SHELL")
        || std::path::Path::new("/opt/.devin").exists()
        || env_set("AI_AGENT")
}

/// `prepare_message`: strip, drop ANSI/controls, mask secrets, cap the UTF-8 size.
fn prepare_message(m: &str) -> (String, bool) {
    static ANSI: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?s)\x1b(?:\[[0-9:;<=>?]*[ -/]*[@-~]|\].*?(?:\x07|\x1b\\)|[P_^].*?(?:\x07|\x1b\\))",
        )
        .expect("re")
    });
    static CTRL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f-\u{9f}]").expect("re")
    });
    let mut t = m.trim().to_string();
    t = ANSI.replace_all(&t, "").into_owned();
    t = CTRL.replace_all(&t, "").into_owned();
    t = snow_api::sanitize::mask_secrets(&t);
    static EXTRA: std::sync::LazyLock<Vec<regex::Regex>> = std::sync::LazyLock::new(|| {
        [
            r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b",
            r"\b(?:sk-|gho_|ghs_|ghp_|ghu_|github_pat_|glpat-)[A-Za-z0-9_-]{8,}",
            r"(?i)-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("re"))
        .collect()
    });
    for re in EXTRA.iter() {
        t = re.replace_all(&t, regex::NoExpand("********")).into_owned();
    }
    if t.len() > MAX_MESSAGE_BYTES {
        let mut end = MAX_MESSAGE_BYTES;
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        t.truncate(end);
        return (t, true);
    }
    (t, false)
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn recent_timestamps(dir: &std::path::Path, now: f64) -> Vec<f64> {
    let v: serde_json::Value = std::fs::read_to_string(dir.join("feedback_rate.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let list = if v.is_object() {
        v["timestamps"].clone()
    } else {
        v
    };
    list.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| if x.is_boolean() { None } else { x.as_f64() })
                .filter(|t| *t > now - RATE_WINDOW)
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Default)]
struct Outcome {
    status: &'static str,
    exit: i32,
    feedback_id: Option<String>,
    category: Option<String>,
    rating: Option<i64>,
    truncated: Option<bool>,
    retry_after: Option<i64>,
    issue_url: bool,
    reason: Option<String>,
}

impl Outcome {
    fn fail(status: &'static str, reason: Option<String>) -> Outcome {
        Outcome {
            status,
            exit: 1,
            reason,
            ..Default::default()
        }
    }
    fn message(&self) -> String {
        let n = PRIVACY_NOTICE;
        match self.status {
            "submitted" => format!(
                "{n}\nFeedback submitted ({}).",
                self.feedback_id.as_deref().unwrap_or_default()
            ),
            "confirmation_required" => {
                format!("{n}\nPass --yes to acknowledge the storage warning.")
            }
            "rate_limited" => {
                format!(
                    "{n}\nRate limit exceeded. Try again in {} seconds.",
                    self.retry_after.unwrap_or_default()
                )
            }
            "no_connection" => {
                format!(
                    "{n}\nA Snowflake connection is required to send feedback. Open an issue at {ISSUES_URL}"
                )
            }
            "send_failed" => format!(
                "{n}\n{}",
                self.reason.as_deref().unwrap_or("Failed to send feedback.")
            ),
            "rejected" => format!("{n}\n{}", self.reason.as_deref().unwrap_or("None")),
            _ => n.to_string(),
        }
    }
    fn payload(&self) -> Output {
        use snow_api::output::Pv;
        let mut v = vec![
            ("status", Pv::s(self.status)),
            ("privacy_notice", Pv::s(PRIVACY_NOTICE)),
        ];
        if let Some(x) = &self.feedback_id {
            v.push(("feedback_id", Pv::s(x.clone())));
        }
        if let Some(x) = &self.category {
            v.push(("category", Pv::s(x.clone())));
        }
        if let Some(x) = self.rating {
            v.push(("rating", Pv::Int(x.into())));
        }
        if let Some(x) = self.truncated {
            v.push(("truncated", Pv::Bool(x)));
        }
        if let Some(x) = self.retry_after {
            v.push(("retry_after_seconds", Pv::Int(x.into())));
        }
        if self.issue_url {
            v.push(("issue_url", Pv::s(ISSUES_URL)));
        }
        if let Some(x) = &self.reason {
            v.push(("reason", Pv::s(x.clone())));
        }
        Output::object(v)
    }
}

fn submit(cli: &mut Cli) -> Outcome {
    let rating = cli.int("rating").unwrap_or_default();
    let category = cli.str("category").unwrap_or_else(|| "other".into());
    let (message, truncated) = match cli.str("message") {
        None if is_coding_agent() => {
            return Outcome::fail(
                "rejected",
                Some("Message is required when running as an AI agent.".into()),
            );
        }
        None => (None, false),
        Some(m) => {
            if m.trim().is_empty() {
                return Outcome::fail("rejected", Some("Message is empty.".into()));
            }
            let (r, t) = prepare_message(&m);
            if r.trim().is_empty() {
                return Outcome::fail("rejected", Some("Message is empty.".into()));
            }
            (Some(r), t)
        }
    };
    let dir = cli
        .config
        .file_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let last_error = if cli.flag("last_error") {
        std::fs::read_to_string(dir.join("last_cli_error.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| {
                let id = v["id"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())?
                    .to_string();
                let ty = v["error_type"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())?
                    .to_string();
                Some((id, ty))
            })
    } else {
        None
    };
    let t = now();
    let recent = recent_timestamps(&dir, t);
    if recent.len() >= RATE_LIMIT_MAX {
        let min = recent.iter().copied().fold(f64::INFINITY, f64::min);
        let retry = ((min + RATE_WINDOW - t).ceil() as i64).max(1);
        return Outcome {
            status: "rate_limited",
            exit: 1,
            retry_after: Some(retry),
            ..Default::default()
        };
    }
    if let Err(e) = cli.connection() {
        let text = e.message();
        let low = text.to_lowercase();
        if e.class_name() != "SnowflakeConnectionError"
            && (e.class_name() == "MissingConfigurationError"
                || low.contains("is not configured")
                || low.contains("connection name or temporary connection is required"))
        {
            return Outcome {
                status: "no_connection",
                exit: 1,
                issue_url: true,
                ..Default::default()
            };
        }
        return Outcome::fail("send_failed", Some(text));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let mut msg = serde_json::json!({
        "type": "cli_user_feedback",
        "source": "snowcli",
        "version_cli": snow_api::VERSION,
        "feedback_id": id,
        "category": category,
        "rating": rating,
        "truncated": truncated,
    });
    if let Some(m) = &message {
        msg["message"] = serde_json::json!(m);
    }
    if let Some((i, ty)) = &last_error {
        msg["last_error_id"] = serde_json::json!(i);
        msg["last_error_type"] = serde_json::json!(ty);
    }
    let ts = chrono::Utc::now().timestamp_millis().to_string();
    let sent = cli.connection_if_open().is_some_and(|c| {
        c.send_telemetry_checked(serde_json::json!([{ "message": msg, "timestamp": ts }]))
    });
    if !sent {
        return Outcome::fail("send_failed", None);
    }
    if last_error.is_some() {
        let _ = std::fs::remove_file(dir.join("last_cli_error.json"));
    }
    let mut all = recent;
    all.push(t);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join("feedback_rate.json"),
        snow_api::pyfmt::json_dumps(&serde_json::json!({ "timestamps": all }), None),
    );
    Outcome {
        status: "submitted",
        feedback_id: Some(id),
        category: Some(category),
        rating: Some(rating),
        truncated: Some(truncated),
        ..Default::default()
    }
}

pub fn feedback(cli: &mut Cli) -> snow_api::Result<Output> {
    use std::io::IsTerminal;
    let outcome = if !cli.flag("yes") {
        let explicit = cli.console.silent
            || cli.format.is_structured()
            || !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
        if explicit {
            Outcome {
                status: "confirmation_required",
                exit: 2,
                ..Default::default()
            }
        } else {
            cli.console.plain_message(PRIVACY_NOTICE);
            if !crate::common::confirm(cli, "Send this feedback to Snowflake?", false)? {
                return Err(SnowError::Click(snow_click::ClickError::Abort));
            }
            submit(cli)
        }
    } else {
        submit(cli)
    };
    let out = if cli.format.is_structured() {
        outcome.payload()
    } else {
        Output::message(outcome.message())
    };
    if outcome.exit != 0 {
        crate::app::print_output(cli, &out, None);
        return Err(SnowError::Click(snow_click::ClickError::Exit(outcome.exit)));
    }
    Ok(out)
}
