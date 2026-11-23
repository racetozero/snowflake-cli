//! `snow stage execute` / `snow git execute` (`StageManager.execute` and the
//! `StagePathParts` family it uses).

use snow_api::identifiers::{
    Fqn, is_valid_quoted_identifier, is_valid_string_literal, to_string_literal,
};
use snow_api::output::{Output, Pv, Row};
use snow_api::stage_path::StagePath;
use snow_api::{Cli, Result, SnowError};

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Default,
    User,
    VStage,
    Git,
}

#[derive(Clone)]
struct Parts {
    kind: Kind,
    stage: String,
    stage_name: String,
    directory: String,
    is_directory: bool,
}

fn split_git_path(path: &str) -> Result<Vec<String>> {
    match path.matches('"').count() {
        0 => Ok(path
            .split('/')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()),
        2 | 4 => Ok(regex::Regex::new(r#""[^"]*"|[^/]+"#)
            .expect("re")
            .find_iter(path)
            .map(|m| m.as_str().to_string())
            .collect()),
        _ => Err(SnowError::usage(format!(
            "Invalid string {path}, too much \" in path, expected 2 or 4."
        ))),
    }
}

pub fn standard_prefix(name: &str) -> String {
    if name.starts_with("snow://") || name.starts_with('@') {
        name.to_string()
    } else {
        format!("@{name}")
    }
}

fn path_parts_omit_first(p: &str) -> String {
    let parts = snow_api::stage_path::posix_parts(p);
    let skip = if parts.first().map(String::as_str) == Some("/") {
        2
    } else {
        1
    };
    parts
        .iter()
        .skip(skip)
        .cloned()
        .collect::<Vec<_>>()
        .join("/")
}

impl Parts {
    fn from_str(stage_path: &str, git: bool) -> Result<Parts> {
        let sp = standard_prefix(stage_path);
        if sp.starts_with("@~") {
            let first = snow_api::stage_path::posix_parts(&sp)
                .first()
                .cloned()
                .unwrap_or_default();
            let directory = if first == "@~" {
                path_parts_omit_first(&sp)
            } else {
                sp.clone()
            };
            return Ok(Parts {
                kind: Kind::User,
                stage: "@~".into(),
                stage_name: "@~".into(),
                directory,
                is_directory: sp.ends_with('/'),
            });
        }
        if git {
            let parts = split_git_path(&sp)?;
            let stage = format!(
                "{}/",
                parts.iter().take(3).cloned().collect::<Vec<_>>().join("/")
            );
            let mut repo = parts[0].rsplit('.').next().unwrap_or_default().to_string();
            if let Some(r) = repo.strip_prefix('@') {
                repo = r.to_string();
            }
            let mut name_parts = vec![repo];
            name_parts.extend(parts.iter().skip(1).take(2).cloned());
            name_parts.push(String::new());
            return Ok(Parts {
                kind: Kind::Git,
                stage,
                stage_name: name_parts.join("/"),
                directory: parts.iter().skip(3).cloned().collect::<Vec<_>>().join("/"),
                is_directory: sp.ends_with('/'),
            });
        }
        let v = snow_api::identifiers::valid_identifier_regex();
        if sp.starts_with("snow://") {
            let re = regex::Regex::new(&format!(
                r"^(?P<prefix>snow://)(?P<rt>[a-zA-Z0-9\-]+)/(?:(?P<f>{v})\.)?(?:(?P<s>{v})\.)?(?P<name>{v})/?(?P<directory>([^/]*/?)*)?$"
            ))
            .expect("re");
            let Some(m) = re.captures(&sp) else {
                return Err(SnowError::cli(format!("Invalid vstage path: {sp}.")));
            };
            let directory = m
                .name("directory")
                .map(|x| x.as_str().to_string())
                .unwrap_or_default();
            let stage = sp
                .strip_suffix(&directory)
                .unwrap_or(&sp)
                .trim_end_matches('/')
                .to_string();
            return Ok(Parts {
                kind: Kind::VStage,
                stage_name: stage.strip_prefix("snow://").unwrap_or(&stage).to_string(),
                stage,
                directory,
                is_directory: sp.ends_with('/'),
            });
        }
        let re = regex::Regex::new(&format!(
            r"^(?P<prefix>(@|snow://))?(?:(?P<f>{v})\.)?(?:(?P<s>{v})\.)?(?P<name>{v})/?(?P<directory>([^/]*/?)*)?$"
        ))
        .expect("re");
        let Some(m) = re.captures(&sp) else {
            return Err(SnowError::cli("Invalid stage path"));
        };
        let directory = m
            .name("directory")
            .map(|x| x.as_str().to_string())
            .unwrap_or_default();
        let prefix = m.name("prefix").map(|x| x.as_str()).unwrap_or("@");
        let stage = sp
            .strip_suffix(&directory)
            .unwrap_or(&sp)
            .trim_end_matches('/')
            .to_string();
        let mut stage_name = Fqn::from_stage(&stage)?.name;
        if let Some(s) = stage_name.strip_prefix(prefix) {
            stage_name = s.to_string();
        }
        Ok(Parts {
            kind: Kind::Default,
            stage,
            stage_name,
            directory,
            is_directory: sp.ends_with('/'),
        })
    }

    fn path(&self) -> String {
        match self.kind {
            Kind::User => self.directory.clone(),
            Kind::VStage => format!(
                "snow://{}/{}",
                self.stage_name.trim_end_matches('/'),
                self.directory
            )
            .trim_end_matches('/')
            .to_string(),
            Kind::Git => format!(
                "{}/{}",
                self.stage_name.trim_end_matches('/'),
                self.directory
            ),
            Kind::Default => format!(
                "{}/{}",
                self.stage_name.trim_end_matches('/'),
                self.directory
            )
            .trim_end_matches('/')
            .to_string(),
        }
    }

    fn get_directory(&self, file: &str) -> Result<String> {
        Ok(match self.kind {
            Kind::Git => {
                let parts = split_git_path(file)?;
                let skip = if file.starts_with('/') { 2 } else { 3 };
                parts
                    .iter()
                    .skip(skip)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("/")
            }
            Kind::User => {
                let first = snow_api::stage_path::posix_parts(file)
                    .first()
                    .cloned()
                    .unwrap_or_default();
                if first == "@~" {
                    path_parts_omit_first(file)
                } else {
                    file.to_string()
                }
            }
            _ => path_parts_omit_first(file),
        })
    }

    fn add_stage_prefix(&self, file: &str) -> String {
        match self.kind {
            Kind::User => format!("{}/{file}", self.stage),
            Kind::VStage => format!(
                "snow://{}/{}",
                self.stage_name.trim_end_matches('/'),
                self.directory
            )
            .trim_end_matches('/')
            .to_string(),
            _ => format!(
                "{}/{}",
                self.stage.trim_end_matches('/'),
                file.trim_start_matches('/')
            ),
        }
    }

    fn full_stage_path(&self, path: &str) -> String {
        match Fqn::from_stage_path(&self.stage).map(|f| f.prefix()) {
            Ok(p) if !p.is_empty() => format!("{p}.{path}"),
            _ => path.to_string(),
        }
    }

    fn standard_stage_path(&self) -> String {
        if self.kind == Kind::VStage {
            return self.path();
        }
        let path = self.full_stage_path(&self.path());
        let slash = if self.is_directory && !path.ends_with('/') {
            "/"
        } else {
            ""
        };
        format!("@{path}{slash}")
    }
}

/// `(stage_name, directory, full_path)` of `stage_path_parts_from_str(path)`.
pub fn parts_for(path: &str) -> Result<(String, String, String)> {
    let p = Parts::from_str(path, false)?;
    let full = match p.kind {
        Kind::User => format!("{}/{}", p.stage, p.directory)
            .trim_end_matches('/')
            .to_string(),
        Kind::VStage => p.path(),
        _ => format!("{}/{}", p.stage.trim_end_matches('/'), p.directory)
            .trim_end_matches('/')
            .to_string(),
    };
    Ok((p.stage_name.clone(), p.directory.clone(), full))
}

/// `FQN.from_stage(stage_path_parts.stage)`.
pub fn stage_of(path: &str) -> Result<Fqn> {
    Fqn::from_stage(&Parts::from_str(path, false)?.stage)
}

/// `stage_path_parts.path`.
pub fn parts_path(path: &str) -> Result<String> {
    Ok(Parts::from_str(path, false)?.path())
}

/// `StageManager.quote_stage_name`.
pub fn quote_stage_name(name: &str) -> String {
    if is_valid_string_literal(name) {
        return name.to_string();
    }
    let s = standard_prefix(name);
    if s.starts_with('@')
        && !regex::Regex::new(r"^@([\w./$])+$")
            .expect("re")
            .is_match(&s)
    {
        return to_string_literal(&s);
    }
    s
}

fn result(cli: &mut Cli, file: &str, error: Option<String>) -> Row {
    let ok = error.is_none();
    cli.console.warning(&format!(
        "{} - {file}",
        if ok { "SUCCESS" } else { "FAILURE" }
    ));
    let mut r = Row::new();
    r.insert("File".into(), Pv::s(file));
    r.insert(
        "Status".into(),
        Pv::s(if ok { "SUCCESS" } else { "FAILURE" }),
    );
    r.insert("Error".into(), error.map(Pv::s).unwrap_or(Pv::None));
    r
}

fn fnmatch_filter(files: &[String], pat: &str) -> Vec<String> {
    let mut re = String::from("(?s)^");
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => match chars[i..].iter().position(|c| *c == ']') {
                Some(j) if j > 1 => {
                    let inner: String = chars[i + 1..i + j].iter().collect();
                    let inner = inner
                        .strip_prefix('!')
                        .map(|x| format!("^{x}"))
                        .unwrap_or(inner);
                    re.push_str(&format!("[{}]", inner.replace('\\', "\\\\")));
                    i += j;
                }
                _ => re.push_str("\\["),
            },
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    let r = regex::Regex::new(&re).unwrap_or_else(|_| regex::Regex::new("^$").expect("re"));
    files.iter().filter(|f| r.is_match(f)).cloned().collect()
}

fn supported(files: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|f| {
            let name = f.rsplit('/').next().unwrap_or("");
            match name.rfind('.') {
                Some(i) if i > 0 => [".sql", ".py"].contains(&&name[i..]),
                _ => false,
            }
        })
        .cloned()
        .collect()
}

/// `[variables]` from config merged with `-D key=value` (`get_merged_variables`).
pub fn merged_variables(cli: &Cli, vars: &[String]) -> Result<Vec<(String, String)>> {
    let mut out: indexmap::IndexMap<String, String> = cli
        .config
        .section(&["variables"])
        .map(|s| s.into_iter().map(|(k, v)| (k, v.py_str())).collect())
        .unwrap_or_default();
    for v in crate::common::parse_key_value_variables(vars)? {
        out.insert(v.key, v.value);
    }
    Ok(out.into_iter().collect())
}

pub fn execute(cli: &mut Cli, path_arg: &str, git: bool) -> Result<Output> {
    let on_error = cli.str("on_error").unwrap_or_else(|| "break".into());
    let variables = cli.list("variables");
    let (parts, original) = if git {
        let original = Parts::from_str(path_arg, true)?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let tmp_fqn =
            cli.using_connection(Fqn::from_stage(&format!("snowflake_cli_tmp_stage_{ts}"))?)?;
        let tmp = tmp_fqn.identifier();
        let parts = Parts::from_str(&format!("{tmp}/{}", original.directory), false)?;
        cli.execute_query(&crate::stage::create_query(
            &tmp_fqn, true, None, false, None,
        ))?;
        let q = crate::stage::copy_files_query_from(
            StagePath::from_git_str(&original.full_stage_path(&original.stage_name)),
            &parts.full_stage_path(&parts.stage_name),
        )?;
        cli.execute_query(&q)?;
        (parts, Some(original))
    } else {
        (Parts::from_str(path_arg, false)?, None)
    };
    let stage_path = if git {
        StagePath::from_stage_str(&parts.standard_stage_path())
    } else {
        StagePath::from_stage_str(path_arg)
    };
    let c = cli.execute_query(&crate::stage::list_files_query(
        &stage_path.root_path(),
        None,
    ))?;
    let idx = c.column_index("name").unwrap_or(0);
    let all: Vec<String> = c
        .rows
        .iter()
        .map(|r| r.get(idx).cloned().flatten().unwrap_or_default())
        .collect();
    if all.is_empty() {
        return Err(SnowError::cli(format!(
            "No files found on stage '{}'",
            stage_path.absolute_path()
        )));
    }
    let with_prefix: Vec<String> = all
        .iter()
        .map(|f| parts.get_directory(f))
        .collect::<Result<_>>()?;
    let dir = parts.directory.trim_end_matches('/').to_string();
    let filtered = if dir.is_empty() {
        supported(&with_prefix)
    } else if with_prefix.contains(&dir) {
        let f = supported(std::slice::from_ref(&dir));
        if f.is_empty() {
            return Err(SnowError::cli(
                "Invalid file extension, only .sql, .py files are allowed.",
            ));
        }
        f
    } else if dir.contains(['*', '?', '[']) {
        supported(&fnmatch_filter(&with_prefix, &dir))
    } else {
        supported(
            &with_prefix
                .iter()
                .filter(|f| **f == dir || f.starts_with(&format!("{dir}/")))
                .cloned()
                .collect::<Vec<_>>(),
        )
    };
    if filtered.is_empty() {
        return Err(SnowError::cli(format!(
            "No files matched pattern '{}'",
            stage_path.absolute_path()
        )));
    }
    let mut sorted = filtered;
    sorted.sort_by(|a, b| {
        let split = |s: &str| match s.rfind('/') {
            Some(i) => (s[..i].to_string(), s[i + 1..].to_string()),
            None => (String::new(), s.to_string()),
        };
        split(a).cmp(&split(b))
    });
    let vars = merged_variables(cli, &variables)?;
    let sql_vars = if vars.is_empty() {
        None
    } else {
        Some(format!(
            " using ({})",
            vars.iter()
                .map(|(k, v)| format!("{k}=>{v}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    };
    let unwrap = |s: &str| {
        if is_valid_string_literal(s) || is_valid_quoted_identifier(s) {
            s[1..s.len() - 1].to_string()
        } else {
            s.to_string()
        }
    };
    let py_vars: serde_json::Map<String, serde_json::Value> = vars
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::json!(unwrap(v))))
        .collect();
    let mut proc_name = None;
    if sorted.iter().any(|f| f.ends_with(".py")) {
        proc_name = Some(bootstrap_python(cli)?);
    }
    let mut results = Vec::new();
    for f in &sorted {
        let file_stage_path = parts.add_stage_prefix(f);
        let original_path = original
            .as_ref()
            .map(|o| o.add_stage_prefix(f))
            .unwrap_or_else(|| file_stage_path.clone());
        let attempt = if f.ends_with(".py") {
            crate::logs::info(
                "snowflake.cli._plugins.stage.manager",
                &format!("Executing Python file: {file_stage_path}"),
            );
            let vars_json =
                snow_api::pyfmt::json_dumps(&serde_json::Value::Object(py_vars.clone()), None);
            cli.try_query(&format!(
                "CALL {}({}, PARSE_JSON({}))",
                proc_name.as_deref().unwrap_or_default(),
                to_string_literal(&standard_prefix(&file_stage_path)),
                to_string_literal(&vars_json)
            ))?
        } else {
            crate::logs::info(
                "snowflake.cli._plugins.stage.manager",
                &format!("Executing SQL file: {file_stage_path}"),
            );
            let mut q = format!(
                "execute immediate from {}",
                quote_stage_name(&file_stage_path)
            );
            if let Some(v) = &sql_vars {
                q.push_str(v);
            }
            cli.try_query(&q)?
        };
        match attempt {
            Ok(_) => results.push(result(cli, &original_path, None)),
            Err(e) => {
                if on_error == "break" {
                    return Err(snow_api::connect::db_error_to_cli(&e, cli.connector_info));
                }
                let msg = e.msg_with(true);
                results.push(result(cli, &original_path, Some(msg)));
            }
        }
    }
    Ok(Output::Collection(results))
}

/// A temporary procedure that runs a staged Python file with `os.environ`
/// updated from the variables (upstream registers the same body via Snowpark).
fn bootstrap_python(cli: &mut Cli) -> Result<String> {
    let name = "SNOWFLAKE_CLI_PYTHON_EXECUTION_PROCEDURE";
    let body = "import json, os\nfrom snowflake.snowpark.files import SnowflakeFile\n\ndef run(session, file_path, variables):\n    with SnowflakeFile.open(file_path, require_scoped_url=False) as f:\n        content = f.read()\n    os.environ.update({k: str(v) for k, v in (variables or {}).items()})\n    exec(content)\n";
    cli.execute_query(&format!(
        "CREATE OR REPLACE TEMPORARY PROCEDURE {name}(file_path STRING, variables VARIANT)\nRETURNS STRING\nLANGUAGE PYTHON\nRUNTIME_VERSION = '3.11'\nPACKAGES = ('snowflake-snowpark-python', 'snowflake.core')\nHANDLER = 'run'\nAS $$\n{body}$$"
    ))?;
    Ok(name.into())
}
