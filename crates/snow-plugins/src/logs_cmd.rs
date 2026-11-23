//! `snow logs`.

use crate::spcs::py_bool;
use snow_api::output::Output;
use snow_api::{Cli, Result, SnowError};

const LOG_LEVELS: [&str; 6] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR", "FATAL"];

fn logs_literal(v: &str) -> String {
    let re = regex::Regex::new(r"(\\*)'").expect("re");
    let mut e = re
        .replace_all(v, |c: &regex::Captures| {
            let mut s = c[1].to_string();
            if s.len() % 2 == 1 {
                s.push('\\');
            }
            s + "''"
        })
        .into_owned();
    if (e.len() - e.trim_end_matches('\\').len()) % 2 == 1 {
        e.push('\\');
    }
    format!("'{e}'")
}

/// `datetime.fromisoformat(s).isoformat()` for the common forms.
fn iso(s: &str, name: &str) -> Result<(String, i64)> {
    let err = || {
        SnowError::click(format!(
            "Incorrect format for '{name}'. Please use one of approved ISO formats."
        ))
    };
    let t = s.trim().replace('T', " ").replace('Z', "+00:00");
    let (dt, tz) = match t.rfind(['+', '-']).filter(|i| *i > 10) {
        Some(i) => (t[..i].to_string(), t[i..].to_string()),
        None => (t.clone(), String::new()),
    };
    let fmts = [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d %H",
    ];
    let naive = if dt.len() == 10 {
        chrono::NaiveDate::parse_from_str(&dt, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    } else {
        fmts.iter()
            .find_map(|f| chrono::NaiveDateTime::parse_from_str(&dt, f).ok())
    }
    .ok_or_else(err)?;
    use chrono::Timelike;
    let base = naive.format("%Y-%m-%dT%H:%M:%S").to_string();
    let us = naive.nanosecond() / 1000;
    let frac = if us > 0 {
        format!(".{us:06}")
    } else {
        String::new()
    };
    let tz = if tz.len() == 3 {
        format!("{tz}:00")
    } else {
        tz
    };
    let key = naive.and_utc().timestamp_micros();
    Ok((format!("{base}{frac}{tz}"), key))
}

pub fn logs(cli: &mut Cli) -> Result<Output> {
    let object_type = cli.str("object_type").unwrap_or_default();
    let name = crate::object::fqn_arg(cli, "object_name")?;
    let level = cli.str("log_level").unwrap_or_else(|| "INFO".into());
    if !level.is_empty() && !LOG_LEVELS.contains(&level.to_uppercase().as_str()) {
        return Err(SnowError::argument(format!(
            "Invalid log level. Please choose from {}",
            LOG_LEVELS.join(", ")
        )));
    }
    if cli.int("refresh_time").is_some_and(|r| r != 0) && cli.str("to").is_some() {
        return Err(SnowError::click(
            "You cannot set both --refresh and --to parameters. Please check the values",
        ));
    }
    let from = cli.str("from_").map(|s| iso(&s, "--from")).transpose()?;
    let to = cli.str("to").map(|s| iso(&s, "--to")).transpose()?;
    if let (Some(f), Some(t)) = (&from, &to)
        && f.1 > t.1
    {
        return Err(SnowError::argument(
            "From_time cannot be later than to_time. Please check the values",
        ));
    }
    let table = cli
        .str("event_table")
        .unwrap_or_else(|| "SNOWFLAKE.TELEMETRY.EVENTS".into());
    let name_text = name.identifier();
    let cond = if cli.flag("partial_match") {
        let pat = format!("%{}%", name_text.replace('%', "\\%").replace('_', "\\_"));
        format!("object_name ILIKE {}", logs_literal(&pat))
    } else {
        format!("object_name = {}", logs_literal(&name_text))
    };
    let lvl = if level.is_empty() {
        "INFO".to_string()
    } else {
        level.to_uppercase()
    };
    let idx = LOG_LEVELS.iter().position(|l| *l == lvl).unwrap_or(2);
    let levels = LOG_LEVELS[idx..]
        .iter()
        .map(|l| format!("'{l}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let build = |from: Option<&str>| -> String {
        let mut ts = String::new();
        if let Some(f) = from {
            ts.push_str(&format!("AND timestamp >= TO_TIMESTAMP_LTZ('{f}')\n"));
        }
        if let Some(t) = &to {
            ts.push_str(&format!("AND timestamp <= TO_TIMESTAMP_LTZ('{}')\n", t.0));
        }
        let i = " ".repeat(12);
        let j = " ".repeat(16);
        let q = format!(
            "\n{i}SELECT\n{j}timestamp,\n{j}resource_attributes:\"snow.database.name\"::string as database_name,\n{j}resource_attributes:\"snow.schema.name\"::string as schema_name,\n{j}resource_attributes:\"snow.{object_type}.name\"::string as object_name,\n{j}record:severity_text::string as log_level,\n{j}value::string as log_message\n{i}FROM {table}\n{i}WHERE record_type = 'LOG'\n{i}AND (record:severity_text IN ({levels}) or record:severity_text is NULL )\n{i}AND {cond}\n{i}{ts}\n{i}ORDER BY timestamp;\n"
        );
        snow_api::sql::dedent(&q).trim().to_string()
    };
    let check = |c: &snow_connector::Cursor| -> Result<()> {
        if c.columns.len() != 6 && !c.rows.is_empty() {
            return Err(SnowError::sql(
                "Logs table has incorrect format. Please check the logs_table in your database",
            ));
        }
        Ok(())
    };
    let msg = |r: &Vec<Option<String>>| {
        Output::message(r.get(5).cloned().flatten().unwrap_or_else(|| "None".into()))
    };
    let refresh = cli.int("refresh_time").filter(|r| *r != 0);
    if let Some(secs) = refresh {
        // `LogsManager.stream_logs`: poll until interrupted, printing as rows arrive.
        let mut prev = from.as_ref().map(|f| f.0.clone());
        loop {
            let c = cli.execute_query(&build(prev.as_deref()))?;
            check(&c)?;
            for r in &c.rows {
                crate::app::print_output(cli, &msg(r), None);
            }
            if let Some(last) = c.rows.last() {
                prev = last.first().cloned().flatten().map(|t| t.replace(' ', "T"));
            }
            std::thread::sleep(std::time::Duration::from_secs(secs.max(0) as u64));
        }
    }
    let c = cli.execute_query(&build(from.as_ref().map(|f| f.0.as_str())))?;
    check(&c)?;
    let _ = py_bool;
    Ok(Output::Stream(c.rows.iter().map(msg).collect()))
}
