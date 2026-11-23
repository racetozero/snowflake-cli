//! SnowSQL-style `!` commands (`snowflake.cli._plugins.sql.repl_commands`).

use crate::app::print_output;
use indexmap::IndexMap;
use regex::Regex;
use snow_api::output::{Format, Output, Pv, Row};
use snow_api::{Cli, Result, SnowError};
use std::sync::LazyLock;

static COMMAND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(!\w+)(?:\s+(.*))?$").expect("re"));
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").expect("re")
});

#[derive(Debug, Clone, Default)]
pub struct Queries {
    help: bool,
    session: bool,
    amount: i64,
    user: Option<String>,
    warehouse: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    duration: Option<String>,
    stmt_type: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ReplCommand {
    Queries(Queries),
    Result(String),
    Abort(String),
    Set(Setting),
    Spool(Option<String>),
    Edit,
    Rehash,
}

/// A `!set option=value`, validated when it is parsed.
#[derive(Debug, Clone)]
pub enum Setting {
    OutputFile(String),
    Quiet(bool),
    Timing(bool),
    TimingInOutputFile(bool),
    Echo(bool),
    OutputFormat(Format),
}

type Kwargs = IndexMap<String, String>;

/// `ReplCommand._parse_args`.
fn parse_args(raw: &str) -> std::result::Result<(Vec<String>, Kwargs), String> {
    let mut args = Vec::new();
    let mut kwargs = Kwargs::new();
    for a in raw.split_whitespace() {
        match a.split_once('=') {
            None => args.push(a.to_string()),
            Some((k, v)) => {
                let k = k.to_lowercase();
                if kwargs.contains_key(&k) {
                    return Err(format!("Duplicated argument {}", snow_click::py_repr(&k)));
                }
                kwargs.insert(k, v.to_string());
            }
        }
    }
    Ok((args, kwargs))
}

fn kwargs_error(name: &str, kwargs: &Kwargs) -> Option<String> {
    if kwargs.is_empty() {
        return None;
    }
    let items: Vec<String> = kwargs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    Some(if items.len() == 1 {
        format!("Invalid argument passed to '{name}' command: {}", items[0])
    } else {
        format!(
            "Invalid arguments passed to '{name}' command: {}",
            items.join(", ")
        )
    })
}

fn py_int(s: &str) -> Option<i64> {
    let t = s.trim().replace('_', "");
    t.parse::<i64>().ok()
}

/// `datetime.fromisoformat(s).timestamp()` in milliseconds.
fn iso_to_ms(s: &str) -> Option<i64> {
    use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis());
    }
    for f in [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M%:z",
    ] {
        if let Ok(d) = DateTime::parse_from_str(s, f) {
            return Some(d.timestamp_millis());
        }
    }
    let naive = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H",
        "%Y-%m-%d %H",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
    .or_else(|| {
        NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    })?;
    let local = Local.from_local_datetime(&naive).earliest()?;
    Some(local.timestamp_millis())
}

fn queries_from(args: Vec<String>, mut kwargs: Kwargs) -> std::result::Result<ReplCommand, String> {
    if args.iter().any(|a| a == "help") {
        return Ok(ReplCommand::Queries(Queries {
            help: true,
            ..Queries::default()
        }));
    }
    let session = args.iter().any(|a| a == "session") || kwargs.is_empty();
    let amount = kwargs.shift_remove("amount").unwrap_or_else(|| "25".into());
    if amount.is_empty() || !amount.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "Invalid argument passed to 'amount' filter: {amount}"
        ));
    }
    let user = kwargs.shift_remove("user");
    let warehouse = kwargs.shift_remove("warehouse");
    let duration = kwargs.shift_remove("duration");
    let mut start = None;
    if let Some(s) = kwargs.shift_remove("start").filter(|s| !s.is_empty()) {
        start = Some(py_int(&s).ok_or(format!("Invalid argument passed to 'start' filter: {s}"))?);
    }
    let mut end = None;
    if let Some(s) = kwargs.shift_remove("end").filter(|s| !s.is_empty()) {
        end = Some(py_int(&s).ok_or(format!("Invalid argument passed to 'end' filter: {s}"))?);
    }
    if let Some(d) = kwargs.shift_remove("start_date").filter(|s| !s.is_empty()) {
        if start.is_some_and(|v| v != 0) {
            return Err("'start_date' filter cannot be used with 'start' filter".into());
        }
        start = Some(iso_to_ms(&d).ok_or(format!(
            "Invalid date format passed to 'start_date' filter: {d}"
        ))?);
    }
    if let Some(d) = kwargs.shift_remove("end_date").filter(|s| !s.is_empty()) {
        if end.is_some_and(|v| v != 0) {
            return Err("'end_date' filter cannot be used with 'end' filter".into());
        }
        end = Some(iso_to_ms(&d).ok_or(format!(
            "Invalid date format passed to 'end_date' filter: {d}"
        ))?);
    }
    let mut stmt_type = kwargs.shift_remove("type").filter(|s| !s.is_empty());
    if let Some(t) = stmt_type.as_mut() {
        *t = t.to_uppercase();
        const TYPES: &[&str] = &[
            "ANY",
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "MERGE",
            "MULTI_TABLE_INSERT",
            "COPY",
            "COMMIT",
            "ROLLBACK",
            "BEGIN_TRANSACTION",
            "SHOW",
            "GRANT",
            "CREATE",
            "ALTER",
        ];
        if !TYPES.contains(&t.as_str()) {
            return Err(format!("Invalid argument passed to 'type' filter: {t}"));
        }
    }
    let mut status = kwargs.shift_remove("status").filter(|s| !s.is_empty());
    if let Some(t) = status.as_mut() {
        *t = t.to_uppercase();
        const STATUS: &[&str] = &[
            "RUNNING",
            "SUCCEEDED",
            "FAILED",
            "BLOCKED",
            "QUEUED",
            "ABORTED",
        ];
        if !STATUS.contains(&t.as_str()) {
            return Err(format!("Invalid argument passed to 'status' filter: {t}"));
        }
    }
    for a in &args {
        let l = a.to_lowercase();
        if l != "session" && l != "help" {
            return Err(format!("Invalid argument passed to 'queries' command: {a}"));
        }
    }
    if let Some(e) = kwargs_error("queries", &kwargs) {
        return Err(e);
    }
    Ok(ReplCommand::Queries(Queries {
        help: false,
        session,
        amount: amount.parse().unwrap_or(25),
        user,
        warehouse,
        start,
        end,
        duration,
        stmt_type,
        status,
    }))
}

fn query_id_from(
    name: &str,
    args: &[String],
    kwargs: &Kwargs,
) -> std::result::Result<String, String> {
    if let Some(e) = kwargs_error(name, kwargs) {
        return Err(e);
    }
    if args.len() != 1 {
        let amount = if args.is_empty() { "No" } else { "Too many" };
        return Err(format!(
            "{amount} arguments passed to '{name}' command. Usage: `!{name} <query id>`"
        ));
    }
    if !UUID.is_match(&args[0]) {
        return Err(format!(
            "Invalid query ID passed to '{name}' command: {}",
            args[0]
        ));
    }
    Ok(args[0].clone())
}

const SET_USAGE: &str = "Usage: `!set output_format=<format>` | `!set quiet=<true|false>` | `!set timing=<true|false>` | `!set timing_in_output_file=<true|false>` | `!set echo=<true|false>` | `!set output_file=<path>`";
const SET_SUPPORTED: &str =
    "Only output_format, quiet, timing, timing_in_output_file, echo and output_file are supported.";
const SET_OPTIONS: &[&str] = &[
    "quiet",
    "timing",
    "timing_in_output_file",
    "output_format",
    "echo",
    "output_file",
];

pub fn parse_bool_value(option: &str, value: &str) -> std::result::Result<bool, String> {
    snow_api::config::parse_on_off(value.trim()).ok_or_else(|| {
        format!(
            "Invalid value for {option}: {}. Use true or false.",
            snow_click::py_repr(value)
        )
    })
}

/// `OutputFormat.from_string`.
pub fn parse_format(value: &str) -> std::result::Result<Format, String> {
    Format::parse(&value.trim().to_uppercase()).ok_or_else(|| {
        format!(
            "Invalid output format: {}. Must be one of: TABLE, JSON, JSON_EXT, CSV, PLAIN.",
            snow_click::py_repr(value)
        )
    })
}

pub fn strip_matching_quotes(v: &str) -> &str {
    let b = v.as_bytes();
    if b.len() >= 2 && b[0] == b[b.len() - 1] && (b[0] == b'"' || b[0] == b'\'') {
        return &v[1..v.len() - 1];
    }
    v
}

fn output_file_value(value: &str) -> std::result::Result<ReplCommand, String> {
    let v = strip_matching_quotes(value.trim());
    if v.trim().is_empty() {
        return Err(format!(
            "You must specify a value for output_file. {SET_USAGE}"
        ));
    }
    Ok(ReplCommand::Set(Setting::OutputFile(v.to_string())))
}

fn set_from(raw: &str) -> std::result::Result<ReplCommand, String> {
    let stripped = raw.trim();
    if stripped.to_lowercase().starts_with("output_file=") {
        return output_file_value(stripped.split_once('=').map(|x| x.1).unwrap_or(""));
    }
    let (args, kwargs) = parse_args(raw)?;
    if !args.is_empty() {
        return Err(format!(
            "Too many arguments passed to 'set' command. {SET_USAGE}"
        ));
    }
    if kwargs.is_empty() {
        return Err(format!("No arguments passed to 'set' command. {SET_USAGE}"));
    }
    let unknown: Vec<String> = kwargs
        .keys()
        .filter(|k| !SET_OPTIONS.contains(&k.as_str()))
        .map(|k| snow_click::py_repr(k))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "Unknown option passed to 'set' command: {}. {SET_SUPPORTED}",
            unknown.join(", ")
        ));
    }
    if kwargs.len() > 1 {
        return Err(format!(
            "Too many arguments passed to 'set' command. {SET_USAGE}"
        ));
    }
    let (option, value) = kwargs.into_iter().next().expect("one");
    let setting = match option.as_str() {
        "output_file" => return output_file_value(&value),
        "quiet" => Setting::Quiet(parse_bool_value(&option, &value)?),
        "timing" => Setting::Timing(parse_bool_value(&option, &value)?),
        "timing_in_output_file" => Setting::TimingInOutputFile(parse_bool_value(&option, &value)?),
        "echo" => match value.to_lowercase().as_str() {
            "true" => Setting::Echo(true),
            "false" => Setting::Echo(false),
            _ => {
                return Err(format!(
                    "Invalid value for 'echo': {}. Must be true or false.",
                    snow_click::py_repr(&value)
                ));
            }
        },
        _ => Setting::OutputFormat(parse_format(&value)?),
    };
    Ok(ReplCommand::Set(setting))
}

pub fn compile_repl_command(text: &str) -> std::result::Result<ReplCommand, String> {
    let Some(c) = COMMAND.captures(text.trim()) else {
        return Err("Not a command".into());
    };
    let name = c[1].to_lowercase();
    let raw = c.get(2).map(|m| m.as_str()).unwrap_or("");
    match name.as_str() {
        "!queries" => {
            let (args, kwargs) = parse_args(raw)?;
            queries_from(args, kwargs)
        }
        "!result" | "!abort" => {
            let (args, kwargs) = parse_args(raw)?;
            let id = query_id_from(&name[1..], &args, &kwargs)?;
            Ok(if name == "!result" {
                ReplCommand::Result(id)
            } else {
                ReplCommand::Abort(id)
            })
        }
        "!set" => set_from(raw),
        "!spool" => {
            let r = strip_matching_quotes(raw.trim());
            if r.is_empty() {
                return Err(
                    "No arguments passed to 'spool' command. Usage: `!spool <file>` | `!spool off`"
                        .into(),
                );
            }
            Ok(ReplCommand::Spool(if r.eq_ignore_ascii_case("off") {
                None
            } else {
                Some(r.to_string())
            }))
        }
        "!edit" => Ok(ReplCommand::Edit),
        "!rehash" => Ok(ReplCommand::Rehash),
        _ => Err(format!("Unknown command '{}'", &c[1])),
    }
}

fn collection(headers: &[&str], rows: Vec<Vec<Pv>>) -> Output {
    Output::Collection(
        rows.into_iter()
            .map(|r| {
                headers
                    .iter()
                    .map(|h| h.to_string())
                    .zip(r)
                    .collect::<Row>()
            })
            .collect(),
    )
}

fn quote_plus(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Spool file state (`open_spool` / `close_spool`).
pub fn open_spool(cli: &mut Cli, path: &str) -> Result<()> {
    cli.sql.spool = None;
    let p = std::path::Path::new(path);
    let existed = p.exists();
    let shown = snow_api::sanitize::for_terminal(&snow_api::stage_path::pure_path(path));
    let fail = |e: std::io::Error| {
        let detail = snow_api::errors::python_os_error(&e, None);
        let detail = detail
            .split_once("] ")
            .map(|x| x.1.to_string())
            .unwrap_or(detail);
        SnowError::cli(format!("Cannot write spool file '{shown}': {detail}"))
    };
    if !existed {
        let _ = snow_api::secure::touch(p);
    }
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(p)
        .map_err(fail)?;
    if existed {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(p)
                .map(|m| m.permissions().mode())
                .unwrap_or(0);
            let readable = mode & 0o044 != 0;
            let writable = mode & 0o022 != 0;
            let detail = match (readable, writable) {
                (true, true) => Some(
                    "Other users on this machine can read query results written to this file, and can write to it.",
                ),
                (true, false) => {
                    Some("Other users on this machine can read query results written to this file.")
                }
                (false, true) => Some("Other users on this machine can write to this file."),
                _ => None,
            };
            if let Some(d) = detail {
                cli.console.warning(&format!(
                    "Spool file '{shown}' is broader than owner-only. {d}"
                ));
            }
        }
    }
    cli.sql.spool = Some(file);
    Ok(())
}

impl ReplCommand {
    pub fn execute(&self, cli: &mut Cli, is_repl: bool) -> Result<()> {
        match self {
            ReplCommand::Queries(q) if q.help => {
                let rows: Vec<Vec<Pv>> = [
                    ["amount", "integer", "25"],
                    ["status", "string", "any"],
                    ["warehouse", "string", "any"],
                    ["user", "string", "any"],
                    [
                        "start_date",
                        "datetime in ISO format (for example YYYY-MM-DDTHH:mm:ss.sss)",
                        "any",
                    ],
                    [
                        "end_date",
                        "datetime in ISO format (for example YYYY-MM-DDTHH:mm:ss.sss)",
                        "any",
                    ],
                    ["start", "timestamp in milliseconds (integer)", "any"],
                    ["end", "timestamp in milliseconds (integer)", "any"],
                    ["type", "string", "any"],
                    ["duration", "time in milliseconds", "any"],
                    ["session", "No arguments", "any"],
                ]
                .iter()
                .map(|r| r.iter().map(|s| Pv::s(*s)).collect())
                .collect();
                let out = collection(&["FILTER", "ARGUMENT", "DEFAULT"], rows);
                print_output(cli, &out, None);
            }
            ReplCommand::Queries(q) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();
                let mut params: Vec<(&str, String)> = vec![
                    ("_dc", snow_api::pyfmt::float_repr(now)),
                    ("includeDDL", "false".into()),
                    ("max", q.amount.to_string()),
                ];
                let conn = cli.connection()?;
                let push = |params: &mut Vec<(&str, String)>, k, v: &Option<String>| {
                    if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
                        params.push((k, v.clone()));
                    }
                };
                push(&mut params, "user", &q.user);
                push(&mut params, "wh", &q.warehouse);
                if let Some(s) = q.start.filter(|v| *v != 0) {
                    params.push(("start", s.to_string()));
                }
                if let Some(s) = q.end.filter(|v| *v != 0) {
                    params.push(("end", s.to_string()));
                }
                push(&mut params, "min_duration", &q.duration);
                if q.session {
                    params.push((
                        "session_id",
                        conn.session_id
                            .map(|i| i.to_string())
                            .unwrap_or("None".into()),
                    ));
                }
                push(&mut params, "subset", &q.status);
                push(&mut params, "stmt_type", &q.stmt_type);
                let qs: Vec<String> = params
                    .iter()
                    .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
                    .collect();
                let url = format!("/monitoring/queries?{}", qs.join("&"));
                let v = conn
                    .rest_get(&url)
                    .map_err(|e| snow_api::connect::db_error_to_cli(&e, false))?;
                if let Some(qs) = v["data"]["queries"].as_array().filter(|a| !a.is_empty()) {
                    let rows = qs
                        .iter()
                        .map(|q| {
                            ["id", "sqlText", "state", "totalDuration"]
                                .iter()
                                .map(|k| Pv::from_json(&q[*k]))
                                .collect()
                        })
                        .collect();
                    let out = collection(&["QUERY ID", "SQL TEXT", "STATUS", "DURATION_MS"], rows);
                    print_output(cli, &out, None);
                }
            }
            ReplCommand::Result(id) => {
                let conn = cli.connection()?;
                let v = conn
                    .result_response(id)
                    .map_err(|e| snow_api::connect::db_error_to_cli(&e, false))?;
                if v["success"].as_bool() == Some(true) && v["data"].is_null() {
                    return Err(SnowError::raised(
                        "TypeError",
                        "argument of type 'NoneType' is not iterable",
                    ));
                }
                let c = conn
                    .cursor_from("", v)
                    .map_err(|e| snow_api::connect::db_error_to_cli(&e, false))?;
                let out = cli.query_result(&c);
                print_output(cli, &out, None);
            }
            ReplCommand::Abort(id) => {
                let c = cli.execute_one(&format!("SELECT SYSTEM$CANCEL_QUERY('{id}')"))?;
                let out = cli.query_result(&c);
                print_output(cli, &out, None);
            }
            ReplCommand::Set(setting) => match setting {
                Setting::OutputFile(p) => open_spool(cli, p)?,
                Setting::Quiet(b) => cli.sql.quiet = *b,
                Setting::Timing(b) => cli.sql.timing = *b,
                Setting::TimingInOutputFile(b) => cli.sql.timing_in_output_file = *b,
                Setting::Echo(b) => cli.sql.echo = *b,
                Setting::OutputFormat(f) => cli.set_format(*f),
            },
            ReplCommand::Spool(None) => cli.sql.spool = None,
            ReplCommand::Spool(Some(p)) => open_spool(cli, p)?,
            ReplCommand::Edit => {
                if !is_repl {
                    return Err(SnowError::cli(
                        "The edit command can only be used in interactive mode.",
                    ));
                }
            }
            ReplCommand::Rehash => {
                // `Repl.refresh_completion_catalog()`; outside the REPL there is no catalog.
                let auto = cli
                    .config
                    .bool_value(&["cli"], "auto_completion", Some(true))
                    .ok()
                    .flatten()
                    .unwrap_or(true);
                if is_repl && auto {
                    super::complete::clear();
                    cli.console.message("Object cache cleared.");
                } else {
                    cli.console.message("Autocomplete is disabled.");
                }
            }
        }
        Ok(())
    }
}
