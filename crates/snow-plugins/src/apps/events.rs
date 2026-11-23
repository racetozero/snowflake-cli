//! `snow app events` for Snowflake App Runtime projects (`_plugins/apps/events.py`).

use super::Flow;
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use snow_api::identifiers::to_string_literal;
use snow_api::output::{Output, Pv, Row};
use snow_api::{Cli, Result, SnowError};

const EVENT_TABLE_FN: &str = "SYSTEM$GET_APPLICATION_SERVICE_EVENT_TABLE_DATA";

#[derive(Clone, Copy, PartialEq)]
enum Stream {
    Log,
    Metric,
    Lifecycle,
}

impl Stream {
    fn value(self) -> &'static str {
        match self {
            Stream::Log => "log",
            Stream::Metric => "metric",
            Stream::Lifecycle => "lifecycle",
        }
    }
    fn table_type(self) -> &'static str {
        match self {
            Stream::Log => "LOG",
            Stream::Metric => "METRIC",
            Stream::Lifecycle => "EVENT",
        }
    }
}

/// `_parse_bound(value, now)`.
fn parse_bound(v: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let re = regex::Regex::new(r"(?i)^\s*(\d+)\s*([smhdw])\s*$").expect("re");
    if let Some(c) = re.captures(v) {
        let n: i64 = c[1].parse().unwrap_or(0);
        let mult = match c[2].to_lowercase().as_str() {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86400,
            _ => 604800,
        };
        return Ok(now - chrono::Duration::seconds(n.saturating_mul(mult)));
    }
    let t = v.trim();
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(t, fmt) {
            return Ok(Utc.from_utc_datetime(&d));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        return Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight")));
    }
    Err(SnowError::cli(format!(
        "Could not parse time value '{v}'. Use relative shorthand (e.g. 30m, 6h, 2d) or an absolute timestamp (e.g. '2026-07-16 18:00:00')."
    )))
}

/// `resolve_time_window(since, until)`.
fn time_window(since: Option<&str>, until: Option<&str>) -> Result<(String, String)> {
    let now = Utc::now();
    let end = match until {
        Some(u) => parse_bound(u, now)?,
        None => now,
    };
    let start = match since {
        Some(s) => parse_bound(s, now)?,
        None => end - chrono::Duration::hours(1),
    };
    if start > end {
        return Err(SnowError::cli(
            "The start of the requested window is after its end. Check --since and --until.",
        ));
    }
    let f = "%Y-%m-%dT%H:%M:%SZ";
    Ok((start.format(f).to_string(), end.format(f).to_string()))
}

fn load_rows(raw: &str) -> Vec<Vec<serde_json::Value>> {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Array(rows)) => rows
            .into_iter()
            .filter_map(|r| r.as_array().cloned())
            .collect(),
        _ => vec![],
    }
}

/// Python `str(value)` of a JSON cell.
fn s(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(x) => x.clone(),
        serde_json::Value::Null => "None".into(),
        serde_json::Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        other => other.to_string(),
    }
}

fn pv_of(v: &serde_json::Value) -> Pv {
    Pv::from_json(v)
}

/// `_epoch_to_iso(value)`.
fn epoch_to_iso(v: &serde_json::Value) -> String {
    let f = match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(x) => x.trim().parse::<f64>().ok(),
        _ => None,
    };
    match f.and_then(|f| {
        DateTime::<Utc>::from_timestamp(f.floor() as i64, ((f - f.floor()) * 1e9) as u32)
    }) {
        Some(d) => d.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        None => s(v),
    }
}

/// Python `str(float)`.
fn py_float(f: f64) -> String {
    snow_api::pyfmt::float_repr(f)
}

/// `format_bytes(num_bytes)`.
fn format_bytes(n: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n;
    for (i, u) in units.iter().enumerate() {
        if v.abs() < 1024.0 || i == units.len() - 1 {
            return if *u == "B" {
                format!("{} {u}", v as i64)
            } else {
                format!("{v:.1} {u}")
            };
        }
        v /= 1024.0;
    }
    format!("{v:.1} PiB")
}

/// Python `f"{x:g}"`.
fn fmt_g(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let exp = x.abs().log10().floor() as i32;
    if !(-4..6).contains(&exp) {
        let m = x / 10f64.powi(exp);
        let ms = format!("{m:.5}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string();
        return format!("{ms}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    let decimals = (5 - exp).max(0) as usize;
    let out = format!("{x:.decimals$}");
    if out.contains('.') {
        out.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        out
    }
}

fn sort_latest_first(records: &mut [Row]) {
    records.sort_by(|a, b| {
        let ka = a.get("time").map(Pv::py_str).unwrap_or_default();
        let kb = b.get("time").map(Pv::py_str).unwrap_or_default();
        kb.cmp(&ka)
    });
}

/// `parse_metric_records(raw, category, raw_values)`.
fn metric_records(raw: &str, category: Option<&str>, raw_values: bool) -> Vec<Row> {
    let prefix = category.map(|c| {
        if c == "network" {
            "network.".to_string()
        } else {
            format!("container.{c}.")
        }
    });
    let mut out = Vec::new();
    for row in load_rows(raw) {
        if row.len() <= 5 {
            continue;
        }
        let name = s(&row[1]);
        if let Some(p) = &prefix
            && !name.starts_with(p.as_str())
        {
            continue;
        }
        let numeric = match &row[2] {
            serde_json::Value::Number(n) => n.as_f64(),
            serde_json::Value::String(x) => x.trim().parse::<f64>().ok(),
            serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        };
        let Some(numeric) = numeric else {
            continue;
        };
        let mut r = Row::new();
        r.insert("time".into(), Pv::Str(epoch_to_iso(&row[0])));
        r.insert("metric".into(), pv_of(&row[1]));
        let unit = s(&row[3]).to_lowercase();
        let (raw_key, raw_num, human) = match unit.as_str() {
            "byte" => {
                let n = numeric.trunc();
                ("bytes", Pv::Int(n as i128), format_bytes(n))
            }
            "cpu" => (
                "cores",
                Pv::Float(numeric),
                format!("{} cores", fmt_g(numeric)),
            ),
            _ => ("raw", Pv::Float(numeric), py_float(numeric)),
        };
        r.insert(
            "value".into(),
            if raw_values {
                raw_num.clone()
            } else {
                Pv::Str(human)
            },
        );
        r.insert(raw_key.into(), raw_num);
        r.insert(
            "instance".into(),
            if row[4].is_null() {
                Pv::s("")
            } else {
                pv_of(&row[4])
            },
        );
        r.insert(
            "container".into(),
            if row[5].is_null() || row[5].as_str() == Some("") {
                Pv::s("")
            } else {
                pv_of(&row[5])
            },
        );
        out.push(r);
    }
    sort_latest_first(&mut out);
    out
}

/// `parse_lifecycle_records(raw)`.
fn lifecycle_records(raw: &str) -> Vec<Row> {
    let mut out = Vec::new();
    for row in load_rows(raw) {
        if row.len() <= 3 {
            continue;
        }
        let mut r = Row::new();
        r.insert("time".into(), Pv::Str(epoch_to_iso(&row[0])));
        r.insert("event".into(), pv_of(&row[2]));
        let (status, message) = match &row[3] {
            serde_json::Value::String(b) => match serde_json::from_str::<serde_json::Value>(b) {
                Ok(serde_json::Value::Object(o)) => (
                    o.get("status").map(s).unwrap_or_default(),
                    o.get("message").map(s).unwrap_or_default(),
                ),
                Ok(_) | Err(_) => (String::new(), b.clone()),
            },
            serde_json::Value::Object(o) => (
                o.get("status").map(s).unwrap_or_default(),
                o.get("message").map(s).unwrap_or_default(),
            ),
            _ => (String::new(), String::new()),
        };
        r.insert("status".into(), Pv::Str(status));
        r.insert("message".into(), Pv::Str(message));
        r.insert("severity".into(), pv_of(&row[1]));
        let inst = row.get(4).cloned().unwrap_or(serde_json::Value::Null);
        r.insert(
            "instance".into(),
            if inst.is_null() {
                Pv::s("")
            } else {
                pv_of(&inst)
            },
        );
        let cont = row.get(5).cloned().unwrap_or(serde_json::Value::Null);
        r.insert(
            "container".into(),
            if cont.is_null() || cont.as_str() == Some("") {
                Pv::s("")
            } else {
                pv_of(&cont)
            },
        );
        out.push(r);
    }
    sort_latest_first(&mut out);
    out
}

/// `format_log_lines(raw)`.
fn log_lines(raw: &str) -> String {
    let mut lines = Vec::new();
    for row in load_rows(raw).into_iter().rev() {
        if row.len() <= 3 {
            continue;
        }
        let ts = epoch_to_iso(&row[0]);
        let (inst, cont, body) = (&row[1], &row[2], s(&row[3]));
        if inst.is_null() && cont.is_null() {
            lines.push(format!("{ts} {body}").trim_end().to_string());
        } else {
            lines.push(
                format!("{ts} [{}/{}] {body}", s(inst), s(cont))
                    .trim_end()
                    .to_string(),
            );
        }
    }
    lines.join("\n")
}

/// `get_event_table_data(service_fqn, event_type, start, end, limit)`.
fn event_table_data(
    cli: &mut Cli,
    ident: &str,
    ty: &str,
    start: &str,
    end: &str,
    limit: Option<i64>,
) -> Result<std::result::Result<String, snow_connector::DbError>> {
    let args = [ident, ty, start, end]
        .iter()
        .map(|a| to_string_literal(a))
        .collect::<Vec<_>>();
    let cap = match limit {
        None => None,
        Some(_) => {
            let c =
                cli.try_query("SHOW PARAMETERS LIKE 'APPLICATION_SERVICE_EVENT_TABLE_MAX_ROWS'")?;
            let v = c.ok().and_then(|c| {
                let i = c
                    .columns
                    .iter()
                    .position(|col| col.name.eq_ignore_ascii_case("value"))?;
                c.rows.first()?.get(i)?.clone()
            });
            Some(v.and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(500))
        }
    };
    let first_cell = |c: &snow_connector::Cursor| {
        c.rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .unwrap_or_default()
    };
    match (limit, cap) {
        (Some(l), Some(cap)) if l > cap => {
            let q = format!(
                "CALL {EVENT_TABLE_FN}({})",
                [args.clone(), vec![to_string_literal("true")]]
                    .concat()
                    .join(", ")
            );
            let c = match cli.try_query(&q)? {
                Ok(c) => c,
                Err(e) => return Ok(Err(e)),
            };
            let qid = first_cell(&c);
            if qid.is_empty() {
                return Ok(Ok(String::new()));
            }
            let c = match cli.try_query(&format!(
                "SELECT * FROM TABLE(RESULT_SCAN({})) ORDER BY TIMESTAMP DESC LIMIT {l}",
                to_string_literal(&qid)
            ))? {
                Ok(c) => c,
                Err(e) => return Ok(Err(e)),
            };
            let ts_col = c
                .columns
                .first()
                .is_some_and(|col| col.type_name.to_lowercase().starts_with("timestamp"));
            let tuples: Vec<serde_json::Value> = c
                .rows
                .iter()
                .map(|r| {
                    serde_json::Value::Array(
                        r.iter()
                            .enumerate()
                            .map(|(i, v)| match v {
                                None => serde_json::Value::Null,
                                Some(x) if i == 0 && ts_col => {
                                    let ts =
                                        NaiveDateTime::parse_from_str(x, "%Y-%m-%d %H:%M:%S%.f")
                                            .map(|d| {
                                                Utc.from_utc_datetime(&d).timestamp_micros() as f64
                                                    / 1e6
                                            })
                                            .map(py_float)
                                            .unwrap_or_else(|_| x.clone());
                                    serde_json::Value::String(ts)
                                }
                                Some(x) => serde_json::Value::String(x.clone()),
                            })
                            .collect(),
                    )
                })
                .collect();
            Ok(Ok(snow_api::pyfmt::json_dumps(
                &serde_json::Value::Array(tuples),
                None,
            )))
        }
        _ => match cli.try_query(&format!("CALL {EVENT_TABLE_FN}({})", args.join(", ")))? {
            Ok(c) => Ok(Ok(first_cell(&c))),
            Err(e) => Ok(Err(e)),
        },
    }
}

/// `app_events` SAR branch + `snowflake_app_events(...)`.
pub fn events(cli: &mut Cli, flow: Flow) -> Result<Output> {
    let mut native: Vec<&str> = Vec::new();
    if !cli.list("scopes").is_empty() {
        native.push("--scope");
    }
    for (opt, k) in [
        ("--consumer-org", "consumer_org"),
        ("--consumer-account", "consumer_account"),
        ("--consumer-app-hash", "consumer_app_hash"),
    ] {
        if cli.str(k).is_some_and(|v| !v.is_empty()) {
            native.push(opt);
        }
    }
    if cli.int("first").is_some_and(|f| f >= 0) {
        native.push("--first");
    }
    if cli.flag("follow") {
        native.push("--follow");
    }
    if cli.int("follow_interval").is_some() {
        native.push("--follow-interval");
    }
    if !native.is_empty() {
        native.sort();
        return Err(SnowError::click(format!(
            "'snow app events' was invoked against a Snowflake App Runtime entity (type: snowflake-app), but the following Native App options were provided: {}. Remove them and try again.",
            native.join(", ")
        )));
    }
    let types = cli.list("record_types");
    if types.len() > 1 {
        return Err(SnowError::incompatible(&["--type", "--type"]));
    }
    let last = cli.int("last").filter(|l| *l != -1);
    let stream = match types.first().map(|t| t.to_lowercase()).as_deref() {
        None | Some("") | Some("log") => Stream::Log,
        Some("metric") => Stream::Metric,
        Some("lifecycle") => Stream::Lifecycle,
        Some(other) => {
            return Err(SnowError::cli(format!(
                "Invalid --type '{other}'. Valid values for Snowflake App Runtime projects are: log, metric, lifecycle."
            )));
        }
    };
    let metric = cli.str("metric").filter(|m| !m.is_empty());
    let category = match metric.as_deref().map(str::to_lowercase).as_deref() {
        None => None,
        Some(c @ ("cpu" | "memory" | "network")) => Some(c.to_string()),
        Some(_) => {
            return Err(SnowError::cli(format!(
                "Invalid --metric '{}'. Valid values are: cpu, memory, network.",
                metric.unwrap_or_default()
            )));
        }
    };
    let raw = cli.flag("raw");
    if category.is_some() && stream != Stream::Metric {
        return Err(SnowError::cli(
            "--metric can only be used with --type metric.",
        ));
    }
    if raw && stream != Stream::Metric {
        return Err(SnowError::cli("--raw can only be used with --type metric."));
    }
    let since = cli.str("since").filter(|s| !s.is_empty());
    let until = cli.str("until").filter(|s| !s.is_empty());
    let instance = cli.int("instance");
    if instance.is_some_and(|i| i < 0) {
        return Err(SnowError::cli(
            "--instance must be a non-negative integer (0-based service instance index).",
        ));
    }
    if instance.is_some() && (stream != Stream::Log || since.is_some() || until.is_some()) {
        return Err(SnowError::cli(
            "--instance can only be used with the live log tail (--type log with no --since/--until).",
        ));
    }
    let svc = super::command_service_pub(cli, flow)?;
    let ident = svc.identifier();
    if stream == Stream::Log && since.is_none() && until.is_none() {
        let (sql, params) = match (instance, last) {
            (Some(i), l) => (
                "CALL SYSTEM$GET_APPLICATION_SERVICE_LOGS(?, ?, ?)",
                vec![ident.clone(), l.unwrap_or(500).to_string(), i.to_string()],
            ),
            (None, Some(l)) => (
                "CALL SYSTEM$GET_APPLICATION_SERVICE_LOGS(?, ?)",
                vec![ident.clone(), l.to_string()],
            ),
            (None, None) => (
                "CALL SYSTEM$GET_APPLICATION_SERVICE_LOGS(?)",
                vec![ident.clone()],
            ),
        };
        let r = cli.connection()?.execute_bound(sql, &params);
        return match r {
            Ok(c) => Ok(Output::message(
                c.rows
                    .first()
                    .and_then(|r| r.first().cloned().flatten())
                    .unwrap_or_default(),
            )),
            Err(e) => {
                let hint = match instance {
                    Some(i) => format!(" Instance {i} may not exist — verify the index is valid and the service is running."),
                    None => " Verify that the app is deployed and the service is running. If the service exists, this can also happen when the active role cannot read application service logs.".to_string(),
                };
                Err(SnowError::cli(format!(
                    "Could not retrieve logs for '{ident}': {}.{hint}",
                    e.msg_with(true)
                )))
            }
        };
    }
    let (start, end) = time_window(since.as_deref(), until.as_deref())?;
    let payload = match event_table_data(cli, &ident, stream.table_type(), &start, &end, last)? {
        Ok(p) => p,
        Err(e) => {
            if e.errno == 2 || e.errno == 94550 {
                return Err(SnowError::cli(
                    "Health monitoring is not yet available for serverless apps in this account. Use snow app events without --type metric, --type lifecycle, --since, or --until to get live logs.",
                ));
            }
            return Err(SnowError::cli(format!(
                "Could not retrieve {} data for '{ident}'. Verify that the app is deployed and that the active role can read the application service's event table. Recently emitted telemetry may not appear immediately due to ingestion lag.",
                stream.value()
            )));
        }
    };
    let cap = |mut v: Vec<Row>| {
        if let Some(l) = last {
            let l = if l < 0 {
                v.len().saturating_sub(l.unsigned_abs() as usize)
            } else {
                l as usize
            };
            v.truncate(l);
        }
        v
    };
    match stream {
        Stream::Metric => Ok(Output::Collection(cap(metric_records(
            &payload,
            category.as_deref(),
            raw,
        )))),
        Stream::Lifecycle => Ok(Output::Collection(cap(lifecycle_records(&payload)))),
        Stream::Log => {
            let text = log_lines(&payload);
            let mut lines: Vec<&str> = text.split('\n').filter(|_| !text.is_empty()).collect();
            if let Some(l) = last {
                lines = if l > 0 {
                    lines[lines.len().saturating_sub(l as usize)..].to_vec()
                } else {
                    vec![]
                };
            }
            Ok(Output::message(lines.join("\n")))
        }
    }
}
