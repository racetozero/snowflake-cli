//! Parameter callbacks (`api/commands/flags.py` and the plugins' own).

use snow_api::config::Cv;
use snow_api::output::Format;
use snow_api::{Cli, Result, SnowError};
use snow_click::Value;
use snow_click::spec::Param;

const PLAIN_PASSWORD_MSG: &str =
    "WARNING! Using --password via the CLI is insecure. Use environment variables instead.";

fn cv(v: &Value) -> Option<Cv> {
    match v {
        Value::None => None,
        Value::Bool(b) => Some(Cv::Bool(*b)),
        Value::Int(i) => Some(Cv::Int(*i)),
        Value::Float(f) => Some(Cv::Float(*f)),
        Value::Str(s) => Some(Cv::Str(s.clone())),
        Value::List(l) => Some(Cv::List(l.iter().filter_map(cv).collect())),
    }
}

fn set_connection_prop(cli: &mut Cli, prop: &str, value: &Value) {
    match prop {
        "connection_name" => cli.conn.connection_name = value.as_str().map(str::to_string),
        "temporary_connection" => cli.conn.temporary_connection = value.as_bool(),
        "mfa_passcode" => cli.conn.mfa_passcode = value.as_str().map(str::to_string),
        "enable_diag" => cli.conn.enable_diag = value.as_bool(),
        "diag_log_path" => cli.conn.diag_log_path = value.as_str().map(str::to_string),
        "diag_allowlist_path" => cli.conn.diag_allowlist_path = value.as_str().map(str::to_string),
        other => cli.conn.set(other, cv(value)),
    }
}

/// `IncompatibleParametersError` for `mutually_exclusive` options.
fn check_mutex(cli: &Cli, param: &'static Param, value: &Value) -> Result<()> {
    if param.mutex.is_empty() || !value.as_bool() {
        return Ok(());
    }
    let level = cli.inv.leaf();
    for name in param.mutex {
        if *name == param.name {
            continue;
        }
        if level.get(cli.spec, name).is_some_and(Value::as_bool) {
            let other = cli
                .spec
                .cmd_params(level.cmd)
                .into_iter()
                .map(|i| cli.spec.param(i))
                .find(|p| p.name == *name);
            let curr = param.opts.first().copied().unwrap_or(param.name);
            let other = other.and_then(|p| p.opts.first().copied()).unwrap_or(name);
            return Err(SnowError::incompatible(&[curr, other]));
        }
    }
    Ok(())
}

pub fn run(cli: &mut Cli, param: &'static Param, value: Value) -> Result<Value> {
    check_mutex(cli, param, &value)?;
    let inner = param.inner_callback.unwrap_or("");
    if let Some(prop) = inner.strip_prefix("prop:") {
        match prop {
            "silent" => {
                cli.silent = value.as_bool();
                cli.console.silent = cli.silent || cli.format.is_structured();
            }
            "verbose" => cli.verbose = value.as_bool(),
            "enable_tracebacks" => cli.debug = value.as_bool(),
            "enhanced_exit_codes" => cli.enhanced_exit_codes = value.as_bool(),
            "experimental" => {}
            p => set_connection_prop(cli, p, &value),
        }
        return Ok(value);
    }
    match inner {
        "_password_callback" => {
            if param.panel.is_some() {
                if value.as_bool() {
                    cli.console.message(PLAIN_PASSWORD_MSG);
                }
                set_connection_prop(cli, "password", &value);
            } else if value.as_bool() && cli.given(param.name) {
                cli.console.warning(PLAIN_PASSWORD_MSG);
            }
        }
        "_workload_identity_provider_callback" => {
            if let Some(v) = value.as_str()
                && !["AWS", "AZURE", "GCP", "OIDC"].contains(&v)
            {
                return Err(SnowError::click(format!(
                    "Invalid workload identity provider '{v}'. Valid values are: AWS, AZURE, GCP, OIDC"
                )));
            }
            set_connection_prop(cli, "workload_identity_provider", &value);
        }
        "_diag_log_path_callback" => {
            let mut p = value.as_str().unwrap_or("").to_string();
            if p == "<system_temporary_directory>" {
                p = std::env::temp_dir()
                    .to_string_lossy()
                    .trim_end_matches('/')
                    .to_string();
            }
            let abs = absolute(&p);
            if !std::path::Path::new(&abs).exists() {
                let _ = snow_api::secure::mkdir_p(std::path::Path::new(&abs));
            }
            cli.conn.diag_log_path = Some(abs.clone());
            return Ok(Value::Str(abs));
        }
        "_diag_log_allowlist_path_callback" => {
            let v = value.as_str().map(absolute);
            cli.conn.diag_allowlist_path = v.clone();
            return Ok(v.map(Value::Str).unwrap_or(Value::None));
        }
        "_output_format_callback" => {
            let mut v = value.as_str().unwrap_or("TABLE").to_string();
            if !cli.given(param.name)
                && let Some(c) = cli.config.value(&["cli"], "output_format")
            {
                match c {
                    Cv::Str(s) => v = s,
                    other => {
                        return Err(SnowError::argument(format!(
                            "Invalid value for cli.output_format: {}. Must be one of: TABLE, JSON, JSON_EXT, CSV, PLAIN.",
                            other.py_repr()
                        )));
                    }
                }
            }
            match Format::parse(v.trim()) {
                Some(f) => cli.format = f,
                None => {
                    return Err(SnowError::argument(format!(
                        "Invalid output format: {}. Must be one of: TABLE, JSON, JSON_EXT, CSV, PLAIN.",
                        snow_click::py_repr(&v)
                    )));
                }
            }
            cli.console.silent = cli.silent || cli.format.is_structured();
            return Ok(Value::Str(cli.format.name().into()));
        }
        "_decimal_precision_callback" => {
            let mut raw: Option<String> = match &value {
                Value::Int(i) => Some(i.to_string()),
                Value::Str(s) => Some(s.clone()),
                _ => None,
            };
            if raw.is_none() {
                raw = cli
                    .config
                    .value(&["cli"], "decimal_precision")
                    .map(|c| c.py_str());
                if let Ok(e) = std::env::var("SNOWFLAKE_DECIMAL_PRECISION")
                    && !e.is_empty()
                {
                    raw = Some(e);
                }
            }
            if let Some(r) = raw {
                match r.trim().parse::<i64>() {
                    Ok(n) if n > 0 => {
                        cli.decimal_precision = Some(n);
                        return Ok(Value::Int(n));
                    }
                    _ => {
                        return Err(SnowError::click(format!(
                            "Invalid value for decimal_precision: '{r}'. Must be a positive integer."
                        )));
                    }
                }
            }
        }
        "project_definition_option.<locals>.project_path_callback" => {
            cli.project_path = value.as_str().map(str::to_string);
        }
        "project_env_overrides_option.<locals>.project_env_overrides_callback" => {
            cli.env_overrides = value.strings();
            crate::common::parse_key_value_variables(&cli.env_overrides)?;
        }
        other => return crate::plugin_callbacks::run(cli, param, other, value),
    }
    Ok(value)
}

fn absolute(p: &str) -> String {
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        p.to_string()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path).to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.to_string())
    }
}
