//! `snow cortex` (SQL backend; `--backend rest` and `search` use the REST API).

use snow_api::output::Output;
use snow_api::{Cli, Result, SnowError};

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

fn query_result(cli: &mut Cli, sql: &str) -> Result<String> {
    let verbose = cli.connector_info;
    let text = snow_api::sql::dedent(sql);
    let stmt = snow_api::splitter::split_statements(&text, true)
        .into_iter()
        .last()
        .map(|s| s.text)
        .unwrap_or_default();
    let c = match cli.connection()?.execute(&stmt) {
        Ok(c) => c,
        Err(e) => return Err(SnowError::click(e.msg_with(verbose))),
    };
    let Some(i) = c.column_index("CORTEX_RESULT") else {
        return Err(SnowError::unexpected("'CORTEX_RESULT'"));
    };
    Ok(c.rows
        .first()
        .and_then(|r| r.get(i).cloned().flatten())
        .unwrap_or_else(|| "None".into()))
}

fn read_file(p: &str) -> Result<String> {
    std::fs::read_to_string(p).map_err(|e| SnowError::unexpected(e.to_string()))
}

fn text_or_file(cli: &Cli, arg: &str) -> Result<(String, bool)> {
    if let Some(t) = cli.str(arg).filter(|t| !t.is_empty()) {
        return Ok((t, false));
    }
    if let Some(f) = cli.str("file") {
        return Ok((read_file(&f)?, true));
    }
    let name = if arg == "text" {
        "TEXT"
    } else {
        "SOURCE_DOCUMENT_TEXT"
    };
    Err(SnowError::usage(format!(
        "Either --file option or {name} argument has to be provided."
    )))
}

fn unexpected_format() -> SnowError {
    SnowError::click("Unexpected format of response from Snowflake")
}

pub fn complete(cli: &mut Cli) -> Result<Output> {
    let (prompt, is_file) = text_or_file(cli, "text")?;
    let model = esc(&cli.str("model").unwrap_or_default());
    if cli.str("backend").as_deref() != Some("sql") {
        let model = cli.str("model").unwrap_or_default();
        return Ok(Output::message(
            rest_complete(cli, &model, &prompt)?.trim().to_string(),
        ));
    }
    let result = if !is_file {
        query_result(
            cli,
            &format!(
                "{0}SELECT SNOWFLAKE.CORTEX.COMPLETE(\n{1}'{model}',\n{1}'{2}'\n{0}) AS CORTEX_RESULT;",
                " ".repeat(16),
                " ".repeat(20),
                esc(&prompt)
            ),
        )?
    } else {
        let raw = query_result(
            cli,
            &format!(
                "{0}SELECT SNOWFLAKE.CORTEX.COMPLETE(\n{1}'{model}',\n{1}PARSE_JSON('{2}'),\n{1}{{}}\n{0}) AS CORTEX_RESULT;",
                " ".repeat(12),
                " ".repeat(16),
                esc(&prompt)
            ),
        )?;
        let j: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| SnowError::unexpected(e.to_string()))?;
        match &j["choices"][0]["messages"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => return Err(unexpected_format()),
            other => other.to_string(),
        }
    };
    Ok(Output::message(result.trim().to_string()))
}

pub fn extract_answer(cli: &mut Cli) -> Result<Output> {
    let (doc, _) = text_or_file(cli, "source_document_text")?;
    let q = cli.str("question").unwrap_or_else(|| "None".into());
    let raw = query_result(
        cli,
        &format!(
            "{0}SELECT SNOWFLAKE.CORTEX.EXTRACT_ANSWER(\n{1}'{2}',\n{1}'{3}'\n{0}) AS CORTEX_RESULT;",
            " ".repeat(12),
            " ".repeat(16),
            esc(&doc),
            esc(&q)
        ),
    )?;
    let j: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| SnowError::unexpected(e.to_string()))?;
    let ans = match &j[0]["answer"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => return Err(unexpected_format()),
        other => other.to_string(),
    };
    Ok(Output::message(ans.trim().to_string()))
}

fn single(cli: &mut Cli, func: &str) -> Result<Output> {
    let (text, _) = text_or_file(cli, "text")?;
    let r = query_result(
        cli,
        &format!(
            "{0}SELECT SNOWFLAKE.CORTEX.{func}(\n{1}'{2}'\n{0}) AS CORTEX_RESULT;",
            " ".repeat(12),
            " ".repeat(16),
            esc(&text)
        ),
    )?;
    Ok(Output::message(r.trim().to_string()))
}

pub fn sentiment(cli: &mut Cli) -> Result<Output> {
    single(cli, "SENTIMENT")
}
pub fn summarize(cli: &mut Cli) -> Result<Output> {
    single(cli, "SUMMARIZE")
}

pub fn translate(cli: &mut Cli) -> Result<Output> {
    let (text, _) = text_or_file(cli, "text")?;
    let src = cli
        .str("from_language")
        .map(|s| esc(&s))
        .unwrap_or_default();
    let dst = esc(&cli.str("to_language").unwrap_or_default());
    let r = query_result(
        cli,
        &format!(
            "{0}SELECT SNOWFLAKE.CORTEX.TRANSLATE(\n{1}'{2}',\n{1}'{src}',\n{1}'{dst}'\n{0}) AS CORTEX_RESULT;",
            " ".repeat(12),
            " ".repeat(16),
            esc(&text)
        ),
    )?;
    Ok(Output::message(r.trim().to_string()))
}

/// `CortexInferenceService.complete` (streamed server-sent events).
fn rest_complete(cli: &mut Cli, model: &str, text: &str) -> Result<String> {
    // snowflake.core's Root() probes the server's client support info first.
    let _ = cli
        .connection()?
        .execute("SELECT SYSTEM$CLIENT_VERSION_INFO()");
    let body = serde_json::json!({
        "anthropic": null, "guardrails": null, "max_output_tokens": null, "max_tokens": 4096,
        "messages": [{"content": text, "role": "user"}], "model": model, "openai": null,
        "provisioned_throughput_id": null, "response_format": null, "stream": true,
        "temperature": null, "tool_choice": null, "top_p": 1.0,
    });
    let (code, raw) = cli
        .connection()?
        .rest_text("POST", "/api/v2/cortex/inference:complete", Some(&body))
        .map_err(|e| SnowError::unexpected(e.msg()))?;
    if !(200..300).contains(&code) {
        return Err(crate::common::core_api_error(code, &raw));
    }
    let mut out = String::new();
    for line in raw.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let v: serde_json::Value = serde_json::from_str(data.trim())
            .map_err(|_| SnowError::cli("Server response cannot be parsed"))?;
        match v["choices"][0]["delta"]["content"].as_str() {
            Some(c) => out.push_str(c),
            None if !v["error"].is_null() => {
                return Err(SnowError::cli(format!("Mid-stream error: {}", data.trim())));
            }
            None => {}
        }
    }
    Ok(out)
}
