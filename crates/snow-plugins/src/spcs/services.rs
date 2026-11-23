//! `snow spcs service`.

use super::{
    LogTail, already_exists, py_bool, query, simple, strip_empty_lines, tags_clause,
    validate_and_set_instances,
};
use crate::object::fqn_arg;
use snow_api::identifiers::{Fqn, is_valid_object_name, to_identifier, to_string_literal};
use snow_api::output::{Output, Pv};
use snow_api::{Cli, Result, SnowError};

/// `_service_name_callback`.
pub fn validate_name(raw: &str) -> Result<()> {
    let f = Fqn::from_string(raw)?;
    if !is_valid_object_name(&f.identifier(), 2, false) {
        return Err(SnowError::argument(format!(
            "'{}' is not a valid service name. Note service names must be unquoted identifiers. The same constraint also applies to database and schema names where you create a service.",
            f.identifier()
        )));
    }
    Ok(())
}

/// `_serialize_spec(yaml.safe_load(spec))`: compact JSON with `$$` escaped.
pub fn read_spec(path: &str) -> Result<String> {
    let text = std::fs::read_to_string(path).map_err(|e| SnowError::unexpected(e.to_string()))?;
    let v: serde_yaml::Value =
        serde_yaml::from_str(&text).map_err(|e| SnowError::unexpected(e.to_string()))?;
    let j = serde_json::to_value(&v).map_err(|e| SnowError::unexpected(e.to_string()))?;
    Ok(serialize_spec(&j))
}

pub fn serialize_spec(j: &serde_json::Value) -> String {
    let s = snow_api::pyfmt::json_dumps(j, None);
    // re.sub(r"\$(?=\$)", r"\u0024", ...)
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, c) in chars.iter().enumerate() {
        if *c == '$' && chars.get(i + 1) == Some(&'$') {
            out.push_str("\\u0024");
        } else {
            out.push(*c);
        }
    }
    out
}

fn name(cli: &Cli) -> Result<String> {
    Ok(fqn_arg(cli, "name")?.identifier())
}

pub(super) fn eai_list(v: &[String]) -> String {
    v.iter()
        .map(|e| to_identifier(e))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn create(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    let min = cli.int("min_instances").unwrap_or(1);
    let max = validate_and_set_instances(min, cli.int("max_instances"), "instances")?;
    let spec = read_spec(&cli.str("spec_path").unwrap_or_default())?;
    let stmt = if cli.flag("if_not_exists") {
        "CREATE SERVICE IF NOT EXISTS"
    } else {
        "CREATE SERVICE"
    };
    let mut q = vec![
        format!("{stmt} {n}"),
        format!(
            "IN COMPUTE POOL {}",
            to_identifier(&cli.str("compute_pool").unwrap_or_default())
        ),
        "FROM SPECIFICATION $$".to_string(),
        spec,
        "$$".to_string(),
        format!("MIN_INSTANCES = {min}"),
        format!("MAX_INSTANCES = {max}"),
        format!(
            "AUTO_RESUME = {}",
            py_bool(cli.opt_bool("auto_resume").unwrap_or(true))
        ),
    ];
    let eai = cli.list("external_access_integrations");
    if !eai.is_empty() {
        q.push(format!(
            "EXTERNAL_ACCESS_INTEGRATIONS = ({})",
            eai_list(&eai)
        ));
    }
    if let Some(w) = cli.str("query_warehouse").filter(|w| !w.is_empty()) {
        q.push(format!("QUERY_WAREHOUSE = {}", to_identifier(&w)));
    }
    if let Some(c) = cli.str("comment").filter(|c| !c.is_empty()) {
        q.push(format!("COMMENT = {c}"));
    }
    let tags = cli.list("tags");
    if !tags.is_empty() {
        q.push(tags_clause(&tags));
    }
    let verbose = cli.connector_info;
    let c = cli
        .try_execute(&strip_empty_lines(&q))
        .map_err(|e| already_exists(e, "service", &n, false, verbose))?;
    Ok(cli.single_query_result(&c))
}

pub fn execute_job(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    let spec = read_spec(&cli.str("spec_path").unwrap_or_default())?;
    let mut q = vec![
        "EXECUTE JOB SERVICE".to_string(),
        format!(
            "IN COMPUTE POOL {}",
            cli.str("compute_pool").unwrap_or_default()
        ),
        "FROM SPECIFICATION $$".to_string(),
        spec,
        "$$".to_string(),
        format!("NAME = {n}"),
    ];
    if cli.flag("async_mode") {
        q.push("ASYNC = TRUE".into());
    }
    if let Some(r) = cli.int("replicas") {
        q.push(format!("REPLICAS = {r}"));
    }
    let eai = cli.list("external_access_integrations");
    if !eai.is_empty() {
        q.push(format!(
            "EXTERNAL_ACCESS_INTEGRATIONS = ({})",
            eai.join(",")
        ));
    }
    if let Some(w) = cli.str("query_warehouse").filter(|w| !w.is_empty()) {
        q.push(format!("QUERY_WAREHOUSE = {w}"));
    }
    if let Some(c) = cli.str("comment").filter(|c| !c.is_empty()) {
        q.push(format!("COMMENT = {c}"));
    }
    let verbose = cli.connector_info;
    let c = cli
        .try_execute(&strip_empty_lines(&q))
        .map_err(|e| already_exists(e, "service", &n, false, verbose))?;
    Ok(cli.single_query_result(&c))
}

pub fn drop(cli: &mut Cli) -> Result<Output> {
    let f = fqn_arg(cli, "name")?;
    let ie = if cli.flag("if_exists") {
        " if exists"
    } else {
        ""
    };
    let force = if cli.flag("force") { " force" } else { "" };
    simple(
        cli,
        format!("drop service{ie} {}{force}", f.sql_identifier()),
    )
}

pub fn status(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    let c = cli.execute_query(&format!(
        "CALL SYSTEM$GET_SERVICE_STATUS({})",
        to_string_literal(&n)
    ))?;
    let first = Cli::first_cell_opt(&c);
    let Some(text) = first else {
        return Ok(Output::Collection(Vec::new()));
    };
    let j: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| SnowError::unexpected(e.to_string()))?;
    match Pv::from_json(&j) {
        Pv::List(items) => Ok(Output::Collection(
            items
                .into_iter()
                .filter_map(|i| if let Pv::Dict(d) = i { Some(d) } else { None })
                .collect(),
        )),
        Pv::Dict(d) => Ok(Output::Collection(vec![d])),
        _ => Ok(Output::Collection(Vec::new())),
    }
}

pub fn list_endpoints(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    query(cli, format!("show endpoints in service {n}"))
}
pub fn list_instances(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    query(cli, format!("show service instances in service {n}"))
}
pub fn list_containers(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    query(cli, format!("show service containers in service {n}"))
}
pub fn list_roles(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    query(cli, format!("show roles in service {n}"))
}
pub fn suspend(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    simple(cli, format!("alter service {n} suspend"))
}
pub fn resume(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    simple(cli, format!("alter service {n} resume"))
}
pub fn upgrade(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    let spec = read_spec(&cli.str("spec_path").unwrap_or_default())?;
    simple(
        cli,
        format!("alter service {n} from specification $$ {spec} $$"),
    )
}

pub fn set(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    let min = cli.int("min_instances");
    let max = cli.int("max_instances");
    let wh = cli.str("query_warehouse");
    let ar = cli.opt_bool("auto_resume");
    let secs = cli.int("auto_suspend_secs");
    let eai = if cli
        .inv
        .leaf()
        .get(cli.spec, "external_access_integrations")
        .is_some_and(|v| !v.as_list().is_empty())
    {
        Some(cli.list("external_access_integrations"))
    } else {
        None
    };
    let comment = cli.str("comment");
    let sql = set_sql(&n, min, max, wh, ar, secs, eai, comment)?;
    simple(cli, sql)
}

/// `ServiceManager.set_property(...)`.
#[allow(clippy::too_many_arguments)]
pub fn set_sql(
    n: &str,
    min: Option<i64>,
    max: Option<i64>,
    wh: Option<String>,
    ar: Option<bool>,
    secs: Option<i64>,
    eai: Option<Vec<String>>,
    comment: Option<String>,
) -> Result<String> {
    if min.is_none()
        && max.is_none()
        && wh.is_none()
        && ar.is_none()
        && secs.is_none()
        && eai.is_none()
        && comment.is_none()
    {
        return Err(SnowError::click(format!(
            "No properties specified for service '{n}'. Please provide at least one property to set."
        )).named("NoPropertiesProvidedError"));
    }
    let mut q = vec![format!("alter service {n} set ")];
    if let Some(v) = min {
        q.push(format!(" min_instances = {v}"));
    }
    if let Some(v) = max {
        q.push(format!(" max_instances = {v}"));
    }
    if let Some(v) = wh {
        q.push(format!(" query_warehouse = {}", to_identifier(&v)));
    }
    if let Some(v) = ar {
        q.push(format!(" auto_resume = {}", py_bool(v)));
    }
    if let Some(v) = secs {
        q.push(format!(" auto_suspend_secs = {v}"));
    }
    if let Some(v) = eai {
        q.push(format!("external_access_integrations = ({})", eai_list(&v)));
    }
    if let Some(v) = comment {
        q.push(format!(" comment = {v}"));
    }
    Ok(strip_empty_lines(&q))
}

pub fn unset(cli: &mut Cli) -> Result<Output> {
    let n = name(cli)?;
    let keys = [
        "min_instances",
        "max_instances",
        "query_warehouse",
        "auto_resume",
        "auto_suspend_secs",
        "comment",
    ];
    let set: Vec<&str> = keys.iter().copied().filter(|k| cli.flag(k)).collect();
    if set.is_empty() {
        return Err(SnowError::click(format!(
            "No properties specified for service '{n}'. Please provide at least one property to reset to its default value."
        )).named("NoPropertiesProvidedError"));
    }
    simple(cli, format!("alter service {n} unset {}", set.join(",")))
}

const EVENT_COLUMNS: [&str; 13] = [
    "TIMESTAMP",
    "START_TIMESTAMP",
    "OBSERVED_TIMESTAMP",
    "TRACE",
    "RESOURCE",
    "RESOURCE_ATTRIBUTES",
    "SCOPE",
    "SCOPE_ATTRIBUTES",
    "RECORD_TYPE",
    "RECORD",
    "RECORD_ATTRIBUTES",
    "VALUE",
    "EXEMPLARS",
];

/// `ServiceManager.logs`, and `ServiceManager.stream_logs` for `--follow`.
pub fn logs(cli: &mut Cli) -> Result<Output> {
    let follow = cli.flag("follow");
    let num_lines = cli.int("num_lines").unwrap_or(500);
    let previous = cli.flag("previous_logs");
    if follow {
        if num_lines != 500 {
            return Err(SnowError::incompatible(&["--follow", "--num-lines"]));
        }
        if previous {
            return Err(SnowError::incompatible(&["--follow", "--previous-logs"]));
        }
    }
    let f = fqn_arg(cli, "name")?;
    let (instance, container) = (
        cli.str("instance_id").unwrap_or_default(),
        cli.str("container_name").unwrap_or_default(),
    );
    let include_ts = cli.flag("include_timestamps");
    let query = |since: &str, prev_logs: bool, with_ts: bool| {
        format!(
            "call SYSTEM$GET_SERVICE_LOGS({}, {}, {}, {num_lines}, {}, {}, {});",
            to_string_literal(&f.identifier()),
            to_string_literal(&instance),
            to_string_literal(&container),
            py_bool(prev_logs),
            to_string_literal(since),
            py_bool(with_ts)
        )
    };
    if follow {
        // `ServiceManager.stream_logs`: poll, de-duplicate by timestamp, until interrupted.
        let interval = cli.int("follow_interval").unwrap_or(2).max(0) as u64;
        let mut tail = LogTail {
            since: cli.str("since_timestamp").unwrap_or_default(),
            ..Default::default()
        };
        loop {
            let c = cli.execute_query(&query(&tail.since, false, true))?;
            for l in tail.take_new(&c) {
                let shown = if include_ts {
                    l
                } else {
                    l.split_once(' ').map_or(l.clone(), |x| x.1.to_string())
                };
                crate::app::print_output(cli, &Output::message(shown), None);
            }
            std::thread::sleep(std::time::Duration::from_secs(interval));
        }
    }
    let q = query(
        &cli.str("since_timestamp").unwrap_or_default(),
        previous,
        include_ts,
    );
    let c = cli.execute_query(&q)?;
    let out: Vec<Output> = c
        .rows
        .iter()
        .map(|r| {
            Output::message(
                r.first()
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| "None".into()),
            )
        })
        .collect();
    Ok(Output::Stream(out))
}

fn event_table(cli: &mut Cli) -> Result<String> {
    let c = cli.execute_query("show parameters like 'event_table' in account")?;
    let (k, v) = (c.column_index("key"), c.column_index("value"));
    let found = c.rows.iter().find_map(|r| {
        let key = k.and_then(|i| r.get(i).cloned().flatten())?;
        (key == "EVENT_TABLE").then(|| {
            v.and_then(|i| r.get(i).cloned().flatten())
                .unwrap_or_default()
        })
    });
    match found.filter(|s| !s.is_empty()) {
        Some(t) => Ok(t),
        None => Err(
            SnowError::click("No SPCS event table configured in the account.")
                .named("SPCSEventTableError"),
        ),
    }
}

fn resource_clause(service: &str, instance: Option<&str>, container: Option<&str>) -> String {
    let mut f = Vec::new();
    if !service.is_empty() {
        f.push(format!(
            "upper(resource_attributes:\"snow.service.name\") = upper({})",
            to_string_literal(service)
        ));
    }
    if let Some(i) = instance.filter(|s| !s.is_empty()) {
        let l = to_string_literal(i);
        f.push(format!(
            "(resource_attributes:\"snow.service.instance\" = {l} OR resource_attributes:\"snow.service.container.instance\" = {l})"
        ));
    }
    if let Some(c) = container.filter(|s| !s.is_empty()) {
        f.push(format!(
            "resource_attributes:\"snow.service.container.name\" = {}",
            to_string_literal(c)
        ));
    }
    if f.is_empty() {
        "1=1".into()
    } else {
        f.join(" and ")
    }
}

fn time_clauses(since: &str, until: &str) -> (String, String) {
    let s = if since.is_empty() {
        String::new()
    } else {
        format!(
            "and timestamp >= sysdate() - interval {}",
            to_string_literal(since)
        )
    };
    let u = if until.is_empty() {
        String::new()
    } else {
        format!(
            "and timestamp <= sysdate() - interval {}",
            to_string_literal(until)
        )
    };
    (s, u)
}

fn db_schema_clause(db: &str, schema: Option<&str>) -> String {
    format!(
        "\n        and resource_attributes:\"snow.database.name\" = '{db}'\n        and resource_attributes:\"snow.schema.name\" = '{}'\n",
        schema.unwrap_or("PUBLIC")
    )
}

fn json_obj(v: Option<&Pv>) -> Option<serde_json::Map<String, serde_json::Value>> {
    let s = match v {
        None | Some(Pv::None) => "{}".to_string(),
        Some(p) => p.py_str(),
    };
    let s = if s.is_empty() { "{}".to_string() } else { s };
    match serde_json::from_str::<serde_json::Value>(&s).ok()? {
        serde_json::Value::Object(m) => Some(m),
        _ => Some(Default::default()),
    }
}

fn attr(m: &serde_json::Map<String, serde_json::Value>, k: &str, default: &str) -> Pv {
    match m.get(k) {
        None => Pv::s(default),
        Some(serde_json::Value::String(s)) => Pv::s(s.clone()),
        Some(v) => Pv::from_json(v),
    }
}

fn format_row(e: &indexmap::IndexMap<String, Pv>, metric: bool) -> Result<snow_api::output::Row> {
    let err = || {
        SnowError::click(if metric {
            "Error processing metric row."
        } else {
            "Error processing event row."
        })
        .named("RecordProcessingError")
    };
    let ra = json_obj(e.get("RESOURCE_ATTRIBUTES")).ok_or_else(err)?;
    let rec = json_obj(e.get("RECORD")).ok_or_else(err)?;
    let mut r = snow_api::output::Row::new();
    r.insert(
        "TIMESTAMP".into(),
        e.get("TIMESTAMP").cloned().unwrap_or(Pv::s("N/A")),
    );
    r.insert(
        "DATABASE NAME".into(),
        attr(&ra, "snow.database.name", "N/A"),
    );
    r.insert("SCHEMA NAME".into(), attr(&ra, "snow.schema.name", "N/A"));
    r.insert("SERVICE NAME".into(), attr(&ra, "snow.service.name", "N/A"));
    let value = e.get("VALUE").cloned().unwrap_or(Pv::s("Unknown Value"));
    if metric {
        r.insert(
            "INSTANCE ID".into(),
            attr(&ra, "snow.service.container.instance", "N/A"),
        );
        r.insert(
            "CONTAINER NAME".into(),
            attr(&ra, "snow.service.container.name", "N/A"),
        );
        let name = match rec
            .get("name")
            .filter(|v| !matches!(v, serde_json::Value::Null) && v.as_str() != Some(""))
        {
            Some(v) => Pv::from_json(v),
            None => rec
                .get("metric")
                .and_then(|m| m.get("name"))
                .map(Pv::from_json)
                .unwrap_or(Pv::s("Unknown Metric")),
        };
        r.insert("METRIC NAME".into(), name);
        let unwrapped = match &value {
            Pv::Str(s) => match serde_json::from_str::<serde_json::Value>(s) {
                Ok(serde_json::Value::Object(m)) => ["int_value", "double_value"]
                    .iter()
                    .find_map(|k| m.get(*k))
                    .map(|v| {
                        Pv::s(match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                    })
                    .unwrap_or(value.clone()),
                _ => value.clone(),
            },
            other => other.clone(),
        };
        r.insert("METRIC VALUE".into(), unwrapped);
    } else {
        let ratt = json_obj(e.get("RECORD_ATTRIBUTES")).ok_or_else(err)?;
        r.insert(
            "INSTANCE ID".into(),
            attr(&ra, "snow.service.instance", "N/A"),
        );
        r.insert(
            "CONTAINER NAME".into(),
            attr(&ra, "snow.service.container.name", "N/A"),
        );
        r.insert(
            "SEVERITY".into(),
            attr(&rec, "severity_text", "Unknown Severity"),
        );
        let name = match rec
            .get("name")
            .filter(|v| !matches!(v, serde_json::Value::Null) && v.as_str() != Some(""))
        {
            Some(v) => Pv::from_json(v),
            None => attr(&ratt, "event.name", "Unknown Event"),
        };
        r.insert("EVENT NAME".into(), name);
        r.insert("EVENT VALUE".into(), value);
    }
    Ok(r)
}

fn rows_of(cli: &mut Cli, q: &str, all: bool, metric: bool) -> Result<Vec<snow_api::output::Row>> {
    let c = cli.execute_query(q)?;
    cli.rows(&c)
        .into_iter()
        .map(|r| {
            let e: indexmap::IndexMap<String, Pv> = EVENT_COLUMNS
                .iter()
                .map(|k| k.to_string())
                .zip(r.into_values())
                .collect();
            if all { Ok(e) } else { format_row(&e, metric) }
        })
        .collect()
}

pub fn events(cli: &mut Cli) -> Result<Output> {
    let (first, last) = (cli.int("first"), cli.int("last"));
    if first.is_some() && last.is_some() {
        return Err(SnowError::incompatible(&["--first", "--last"]));
    }
    let (container, instance) = (cli.str("container_name"), cli.str("instance_id"));
    if container.is_some() && instance.is_none() {
        return Err(SnowError::argument(
            "--instance-id is required when --container-name is specified.",
        ));
    }
    let f = fqn_arg(cli, "name")?;
    let table = event_table(cli)?;
    let rc = resource_clause(&f.identifier(), instance.as_deref(), container.as_deref());
    let (sc, uc) = time_clauses(
        &cli.str("since").unwrap_or_default(),
        &cli.str("until").unwrap_or_default(),
    );
    let fc = first.map(|n| format!("limit {n}")).unwrap_or_default();
    let lc = last.map(|n| format!("limit {n}")).unwrap_or_default();
    let q = format!(
        "                     select *\n                    from (\n                        select *\n                        from {table}\n                        where (\n                            {rc}\n                            {sc}\n                            {uc}\n                        )\n                        and record_type = 'EVENT'\n                        and scope['name'] = 'snow.spcs.platform'\n                        order by timestamp desc\n                        {lc}\n                    )\n                    order by timestamp asc\n                    {fc}\n                "
    );
    let rows = rows_of(cli, &q, cli.flag("show_all_columns"), false)?;
    if rows.is_empty() {
        return Ok(Output::message("No events found."));
    }
    Ok(Output::Collection(rows))
}

pub fn metrics(cli: &mut Cli) -> Result<Output> {
    let f = fqn_arg(cli, "name")?;
    let (container, instance) = (cli.str("container_name"), cli.str("instance_id"));
    let (since, until) = (
        cli.str("since").unwrap_or_default(),
        cli.str("until").unwrap_or_default(),
    );
    let table = event_table(cli)?;
    let rc = resource_clause(&f.name, instance.as_deref(), container.as_deref());
    let dbc = f
        .database
        .as_deref()
        .map(|d| db_schema_clause(d, f.schema.as_deref()))
        .unwrap_or_default();
    let q = if !since.is_empty() || !until.is_empty() {
        let (sc, uc) = time_clauses(&since, &until);
        format!(
            "                    select *\n                    from {table}\n                    where (\n                        {rc}\n                        {dbc}\n                        {sc}\n                        {uc}\n                    )\n                    and record_type = 'METRIC'\n                    and scope['name'] = 'snow.spcs.platform'\n                    order by timestamp desc\n                "
        )
    } else {
        format!(
            "\n            with rankedmetrics as (\n                select\n                    *,\n                    row_number() over (\n                        partition by COALESCE(record['name'], record['metric']['name'])\n                        order by timestamp desc\n                    ) as rank\n                from {table}\n                where\n                    record_type = 'METRIC'\n                    and scope['name'] = 'snow.spcs.platform'\n                    and {rc}\n                    {dbc}\n                    and timestamp > dateadd('hour', -1, current_timestamp)\n            )\n            select *\n            from rankedmetrics\n            where rank = 1\n            order by timestamp desc;\n        "
        )
    };
    let rows = rows_of(cli, &q, cli.flag("show_all_columns"), true)?;
    if rows.is_empty() {
        return Ok(Output::message("No metrics found."));
    }
    Ok(Output::Collection(rows))
}

/// The connector error for a retryable HTTP status under `no_retry`
/// (`get_http_retryable_error`) as `str(err)`.
pub fn retryable_http_error(code: u16) -> Option<String> {
    let msg = match code {
        500 => "HTTP 500: Internal Server Error",
        503 => "HTTP 503: Service Unavailable",
        504 => "HTTP 504: Gateway Timeout",
        403 => "HTTP 403: Forbidden",
        400 => "HTTP 400: Bad Request",
        502 => "HTTP 502: Bad Gateway",
        405 => "HTTP 405: Method not allowed",
        429 => "HTTP 429: Too Many Requests",
        307 | 308 | 408 | 501..=599 => "HTTP n/a",
        _ => return None,
    };
    let errno = 290_000 + code as u32;
    Some(format!("{errno}: {errno}: {msg}"))
}

pub(super) fn remote_build_error(code: u16, op: &str, url: &str) -> SnowError {
    if let Some(e) = retryable_http_error(code) {
        // The connector logs the cause (with a traceback) before raising it.
        crate::logs::log_plain(crate::logs::ERROR, &e);
        if code == 400 {
            return SnowError::cli(format!(
                "Remote build API returned 400 Bad Request while trying to {op}. Check that all required fields (build_source, image-repository, etc.) are correct."
            ))
            .named("RemoteBuildPermanentError");
        }
        return SnowError::cli(format!("Remote build API error while trying to {op}: {e}"))
            .named("CliConnectionError");
    }
    let m = match code {
        401 => "Remote build API returned 401 Unauthorized. Check your credentials.".to_string(),
        404 => format!("Remote build API returned 404 Not Found while trying to {op}."),
        other => {
            let kind = if other >= 500 { "Server" } else { "Client" };
            return SnowError::cli(format!(
                "Remote build API returned HTTP {other} while trying to {op}: {other} {kind} Error: {} for url: {url}",
                snow_connector::status_text(other)
            ));
        }
    };
    SnowError::cli(m).named("RemoteBuildPermanentError")
}

/// `RemoteBuildManager.get_remote_builder(job)`: the job, or `None` when there is no
/// such job (a 404 or an empty body).
pub(super) fn fetch_remote_job(cli: &mut Cli, job: &str) -> Result<Option<serde_json::Value>> {
    let path = format!(
        "/api/v2/remote-build/jobs/{}",
        snow_api::identifiers::encode_uri_component(job).replace("%24", "$")
    );
    let (code, v) = cli
        .connection()?
        .rest("GET", &path, None)
        .map_err(|e| SnowError::cli(e.msg()))?;
    if code == 404 || (code < 300 && (v.is_null() || v.as_object().is_some_and(|o| o.is_empty()))) {
        return Ok(None);
    }
    if code >= 300 {
        let url = format!("{}{path}", cli.connection()?.base_url.clone());
        return Err(remote_build_error(code, &format!("get job '{job}'"), &url));
    }
    Ok(Some(v))
}

pub fn remote_build_status(cli: &mut Cli) -> Result<Output> {
    let job = cli.str("job_name").unwrap_or_default();
    let Some(v) = fetch_remote_job(cli, &job)? else {
        return Ok(Output::message(format!(
            "No remote build job found with name '{job}'."
        )));
    };
    let s = |k: &str, d: Pv| v.get(k).map(Pv::from_json).unwrap_or(d);
    Ok(Output::object(vec![
        ("job_name", s("job_name", Pv::s(""))),
        ("status", s("job_status", Pv::s("UNKNOWN"))),
        ("creation_time", s("creation_time", Pv::None)),
        ("end_time", s("end_time", Pv::None)),
    ]))
}

pub fn remote_build_history(cli: &mut Cli) -> Result<Output> {
    use base64::Engine;
    let page = cli.int("page_size").unwrap_or(50);
    let mut token = cli.str("start_token");
    let cutoff = chrono::Utc::now().timestamp_millis() - 30 * 24 * 60 * 60 * 1000;
    let mut rows = Vec::new();
    let mut pages = 0;
    loop {
        let mut q = vec![format!("limit={page}")];
        if let Some(t) = token.as_ref().filter(|t| !t.is_empty()) {
            q.push(format!(
                "page_token={}",
                url::form_urlencoded::byte_serialize(t.as_bytes()).collect::<String>()
            ));
        }
        let path = format!("/api/v2/remote-build/jobs?{}", q.join("&"));
        let (code, v) = cli
            .connection()?
            .rest("GET", &path, None)
            .map_err(|e| SnowError::cli(e.msg()))?;
        if code >= 300 {
            let url = format!("{}{path}", cli.connection()?.base_url.clone());
            return Err(remote_build_error(code, "list jobs", &url));
        }
        pages += 1;
        for j in v["jobs"].as_array().cloned().unwrap_or_default() {
            let g = |k: &str| j.get(k).map(Pv::from_json).unwrap_or(Pv::s(""));
            let mut r = snow_api::output::Row::new();
            r.insert("job_name".into(), g("job_name"));
            r.insert("status".into(), g("job_status"));
            r.insert("creation_time".into(), g("creation_time"));
            r.insert("end_time".into(), g("end_time"));
            rows.push(r);
        }
        token = v["next_page_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        let Some(t) = token.clone() else { break };
        if pages >= 1000 {
            cli.console.warning(&format!(
                "Stopped after 1000 pages to avoid an unbounded loop. Resume with: --start-token {t}"
            ));
            break;
        }
        let padded = format!("{t}{}", "=".repeat((4 - t.len() % 4) % 4));
        let ms = base64::engine::general_purpose::URL_SAFE
            .decode(padded)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.trim().parse::<i64>().ok());
        match ms {
            None => {
                cli.console.warning(
                    "Received an unrecognised page token from the server; stopping pagination.",
                );
                break;
            }
            Some(m) if m < cutoff => break,
            _ => {}
        }
    }
    if rows.is_empty() {
        return Ok(Output::message(
            "No remote build jobs found in the past 30 days.",
        ));
    }
    Ok(Output::Collection(rows))
}
