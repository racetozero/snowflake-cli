//! `snow streamlit logs`: stream developer logs over the app's WebSocket log service
//! (`_plugins/streamlit/log_streaming.py`, `proto_codec.py`).

use snow_api::identifiers::Fqn;
use snow_api::output::{Output, Pv, Row};
use snow_api::{Cli, Result, SnowError};

fn sql_err(m: String) -> SnowError {
    SnowError::cli(m).named("CliSqlError")
}

/// `StreamLogsRequest{tail_lines}` (field 1, varint).
fn encode_request(tail: i64) -> Vec<u8> {
    let mut out = Vec::new();
    if tail != 0 {
        out.push(0x08);
        let mut v = tail as u64;
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }
    out
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    while *i < b.len() {
        let x = b[*i];
        *i += 1;
        v |= ((x & 0x7F) as u64) << shift;
        if x & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
    }
    None
}

struct Entry {
    source: u64,
    content: String,
    secs: i64,
    nanos: i64,
    sequence: i64,
    level: u64,
}

/// `LogEntry` (log_source=1, content=2, timestamp=3 {seconds=1, nanos=2}, sequence=4, level=5).
fn decode_entry(b: &[u8]) -> Option<Entry> {
    let mut e = Entry {
        source: 0,
        content: String::new(),
        secs: 0,
        nanos: 0,
        sequence: 0,
        level: 0,
    };
    let mut i = 0;
    while i < b.len() {
        let key = varint(b, &mut i)?;
        let (field, wire) = (key >> 3, key & 7);
        match wire {
            0 => {
                let v = varint(b, &mut i)?;
                match field {
                    1 => e.source = v,
                    4 => e.sequence = v as i64,
                    5 => e.level = v,
                    _ => {}
                }
            }
            2 => {
                let n = varint(b, &mut i)? as usize;
                let data = b.get(i..i + n)?;
                i += n;
                match field {
                    2 => e.content = String::from_utf8(data.to_vec()).ok()?,
                    3 => {
                        let mut j = 0;
                        while j < data.len() {
                            let k = varint(data, &mut j)?;
                            let v = varint(data, &mut j)?;
                            match k >> 3 {
                                1 => e.secs = v as i64,
                                2 => e.nanos = v as i64,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            1 => i += 8,
            5 => i += 4,
            _ => return None,
        }
    }
    Some(e)
}

fn level_label(l: u64) -> &'static str {
    match l {
        1 => "DEBUG",
        2 => "INFO",
        3 => "WARN",
        4 => "ERROR",
        _ => "UNKNOWN",
    }
}

fn source_label(s: u64) -> &'static str {
    match s {
        1 => "APP",
        2 => "MGR",
        _ => "UNKNOWN",
    }
}

pub fn logs(cli: &mut Cli) -> Result<Output> {
    let entity_id = cli.str("entity_id");
    let name = cli.str("name");
    let tail = cli.int("tail").unwrap_or(100);
    let fqn = if let Some(n) = name {
        if entity_id.is_some() {
            return Err(SnowError::cli(
                "Cannot specify both --name and an entity ID. Use --name to identify the app directly, or use an entity ID to reference a snowflake.yml definition.",
            )
            .named("CliArgumentError"));
        }
        let f = Fqn::from_string(&n)?;
        cli.using_connection(f)?
    } else {
        if cli.project(true)?.is_none() {
            return Err(SnowError::cli(
                "No Streamlit app specified. Provide --name or run from a directory with a snowflake.yml project definition.",
            )
            .named("CliArgumentError"));
        }
        let (id, model) = crate::entities::for_operation(cli, entity_id.as_deref(), "streamlit")?;
        let f = crate::entities::fqn(&model, &id)?;
        cli.using_connection(f)?
    };
    // `validate_spcs_v2_runtime`.
    let c = crate::object::describe_object(cli, "streamlit", &fqn)?;
    let Some(row) = c.rows.first() else {
        return Err(sql_err(format!(
            "Could not describe Streamlit app {}. Verify the app exists and you have access.",
            fqn.identifier()
        )));
    };
    let runtime = c
        .columns
        .iter()
        .position(|col| col.name.to_lowercase() == "runtime_name")
        .and_then(|i| row.get(i).cloned().flatten());
    let norm = runtime.clone().unwrap_or_default().trim().to_uppercase();
    if !regex::Regex::new(r"^SYSTEM\$ST_CONTAINER_RUNTIME[A-Z0-9_]*$")
        .expect("re")
        .is_match(&norm)
    {
        return Err(SnowError::cli(format!(
            "Log streaming is only supported for Streamlit apps running on an SPCS container runtime (SYSTEM$ST_CONTAINER_RUNTIME*). App '{}' has runtime_name='{}'.",
            fqn.identifier(),
            runtime.unwrap_or_else(|| "None".into())
        )));
    }
    let json = cli.format.is_json() || cli.format == snow_api::output::Format::Csv;
    cli.console.step("Fetching developer API token...");
    let q = format!(
        "CALL SYSTEM$GET_STREAMLIT_DEVELOPER_API_TOKEN({}, false);",
        snow_api::identifiers::to_string_literal(&fqn.identifier())
    );
    let c = cli.execute_query(&q)?;
    let raw = c
        .rows
        .first()
        .map(|r| r.first().cloned().flatten())
        .ok_or_else(|| {
            sql_err("Empty response from SYSTEM$GET_STREAMLIT_DEVELOPER_API_TOKEN".into())
        })?;
    let resp: serde_json::Value = match raw.as_deref().map(serde_json::from_str) {
        Some(Ok(v)) => v,
        Some(Err(e)) => return Err(sql_err(format!("Failed to parse token response: {e}"))),
        None => return Err(sql_err("Failed to parse token response: the JSON object must be str, bytes or bytearray, not NoneType".into())),
    };
    let token = resp["token"].as_str().unwrap_or_default().to_string();
    let uri = resp["resourceUri"].as_str().unwrap_or_default().to_string();
    if token.is_empty() {
        return Err(sql_err("Empty token in developer API response".into()));
    }
    if uri.is_empty() {
        return Err(sql_err(
            "Empty resourceUri in developer API response".into(),
        ));
    }
    let ws_url = format!(
        "{}/logs",
        uri.replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
            .trim_end_matches('/')
    );
    cli.console
        .step(&format!("Connecting to log stream: {ws_url}"));
    let header = vec![format!("Authorization: Snowflake Token=\"{token}\"")];
    let mut ws = snow_connector::ws::WebSocket::connect(
        &ws_url,
        &header,
        std::time::Duration::from_secs(10),
    )
    .map_err(|e| {
        SnowError::cli(format!("Failed to connect to log stream: {e}")).named("CliConnectionError")
    })?;
    ws.send_binary(&encode_request(tail))
        .map_err(|e| SnowError::cli(e.to_string()))?;
    cli.console.step(&format!(
        "Streaming logs (tail={tail}). Press Ctrl+C to stop."
    ));
    let mut items = Vec::new();
    loop {
        match ws.recv() {
            Ok(snow_connector::ws::Frame::Binary(data)) => {
                let Some(e) = decode_entry(&data) else {
                    crate::logs::log(
                        crate::logs::WARNING,
                        "snowflake.cli._plugins.streamlit.log_streaming",
                        "Failed to decode log entry",
                    );
                    continue;
                };
                let ts = chrono::DateTime::from_timestamp(e.secs, (e.nanos / 1000 * 1000) as u32)
                    .unwrap_or_default();
                let content = snow_api::sanitize::for_terminal(&e.content);
                let item = if json {
                    let iso = if ts.timestamp_subsec_micros() == 0 {
                        ts.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
                    } else {
                        ts.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
                    };
                    let mut r = Row::new();
                    r.insert("timestamp".into(), Pv::s(iso));
                    r.insert("level".into(), Pv::s(level_label(e.level)));
                    r.insert("source".into(), Pv::s(source_label(e.source)));
                    r.insert("sequence".into(), Pv::Int(e.sequence as i128));
                    r.insert("content".into(), Pv::s(content));
                    Output::Object(Some(r))
                } else {
                    let micros = ts.format("%Y-%m-%d %H:%M:%S.%6f").to_string();
                    let ms = &micros[..micros.len() - 3];
                    Output::message(format!(
                        "[{ms}] [{}] [{}] [seq:{}] {content}",
                        level_label(e.level),
                        source_label(e.source),
                        e.sequence
                    ))
                };
                crate::app::print_output(cli, &Output::Stream(vec![item]), None);
                items.push(());
            }
            Ok(snow_connector::ws::Frame::Text(_)) => continue,
            Ok(snow_connector::ws::Frame::Close) => break,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    let _ = ws.close();
    cli.console.step("Log streaming stopped.");
    Ok(Output::Empty)
}
