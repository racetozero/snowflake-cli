//! `snow dbt`.

use crate::object;
use regex::Regex;
use snow_api::identifiers::{Fqn, to_string_literal};
use snow_api::output::Output;
use snow_api::{Cli, Result, SnowError};
use std::sync::LazyLock;

const SECRET_PREFIX: &str = "DBT_ENV_SECRET_";
static KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_]+$").expect("re"));
static CONTROL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\x00-\x1f\x7f]").expect("re"));
const QUOTED: &str = r"'(?:[^']|'')*'";
static SYSTEM_IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)^(?P<value>SYSTEM\$[A-Za-z_][A-Za-z0-9_]*\(\s*(?:'(?:[^']|'')*'(?:\s*,\s*'(?:[^']|'')*')*\s*)?\))(?:\s+[aA][sS]\s+(?P<alias>{QUOTED}|\S+))?$"
    ))
    .expect("re")
});
static LOCATION_IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)^(?P<value>{QUOTED}|@\S+|snow://\S+)(?:\s+[aA][sS]\s+(?P<alias>{QUOTED}|\S+))?$"
    ))
    .expect("re")
});
static MOUNT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").expect("re"));

pub fn reject_control_chars(value: Option<&str>, flag: &str) -> Result<()> {
    if let Some(v) = value
        && CONTROL_RE.is_match(v)
    {
        return Err(SnowError::cli(format!(
            "{flag} must not contain control characters (newlines, tabs, etc.)"
        )));
    }
    Ok(())
}

fn invalid_import(value: &str) -> SnowError {
    SnowError::cli(format!(
        "--import value {} is not valid. Provide a stage path (@stage/s1), a dbt snow URL (snow://dbt/my_db.my_schema.my_project/versions/live), or a SYSTEM$ function, optionally followed by \"as folder\". Stage paths, snow URLs, and folder names may be given with or without single quotes, but a value containing spaces must be single-quoted (e.g. '@\"my stage\"/dir'). Example: --import \"@stage/s1 as folder1\".",
        snow_click::py_repr(value)
    ))
}

pub fn render_import(entry: &str) -> Result<String> {
    reject_control_chars(Some(entry), "--import")?;
    let value = entry.trim();
    if value.is_empty() {
        return Err(SnowError::cli("--import value must not be empty."));
    }
    let (mut rendered, alias) = if let Some(c) = SYSTEM_IMPORT_RE.captures(value) {
        (
            c["value"].to_string(),
            c.name("alias").map(|m| m.as_str().to_string()),
        )
    } else {
        let Some(c) = LOCATION_IMPORT_RE.captures(value) else {
            return Err(invalid_import(value));
        };
        let raw = c["value"].to_string();
        let content = raw.strip_prefix('\'').unwrap_or(&raw).to_string();
        let lowered = content.to_lowercase();
        if lowered.starts_with("snow://") && !lowered.starts_with("snow://dbt/") {
            return Err(SnowError::cli(
                "--import snow URL must be a dbt project URL, e.g. \"snow://dbt/my_db.my_schema.my_project/versions/live\".",
            ));
        }
        if !(content.starts_with('@') || lowered.starts_with("snow://dbt/")) {
            return Err(invalid_import(value));
        }
        let inner = if raw.starts_with('\'') {
            raw[1..raw.len() - 1].to_string()
        } else {
            raw.clone()
        };
        if (inner.len() - inner.trim_end_matches('\\').len()) % 2 == 1 {
            return Err(SnowError::cli(
                "--import value must not end with a backslash; it would escape the closing quote and produce invalid SQL.",
            ));
        }
        let r = if raw.starts_with('\'') {
            raw.clone()
        } else {
            to_string_literal(&raw)
        };
        (r, c.name("alias").map(|m| m.as_str().to_string()))
    };
    if let Some(a) = alias {
        let folder = if a.starts_with('\'') {
            a[1..a.len() - 1].to_string()
        } else {
            a.clone()
        };
        if !MOUNT_RE.is_match(&folder) {
            return Err(SnowError::cli(
                "--import folder alias must be an ASCII name using only letters, digits, underscores, and hyphens, e.g. \"@stage/s1 as folder1\".",
            ));
        }
        rendered.push_str(" AS ");
        rendered.push_str(&if a.starts_with('\'') {
            a
        } else {
            to_string_literal(&a)
        });
    }
    Ok(rendered)
}

fn plural(n: usize, s: &str, p: &str) -> String {
    if n == 1 { s.into() } else { p.into() }
}

fn py_type_name(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "NoneType",
        serde_yaml::Value::Bool(_) => "bool",
        serde_yaml::Value::Number(n) => {
            if n.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        serde_yaml::Value::String(_) => "str",
        serde_yaml::Value::Sequence(_) => "list",
        serde_yaml::Value::Mapping(_) => "dict",
        serde_yaml::Value::Tagged(_) => "object",
    }
}

fn parse_env_vars(raw: &str) -> Result<Vec<(String, String)>> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(raw)
        .map_err(|e| SnowError::cli(format!("--env-vars must be valid YAML/JSON: {e}")))?;
    let serde_yaml::Value::Mapping(m) = &parsed else {
        return Err(SnowError::cli(format!(
            "--env-vars must be a YAML/JSON object, got {}",
            py_type_name(&parsed)
        )));
    };
    let mut out = Vec::new();
    for (k, v) in m {
        let serde_yaml::Value::String(k) = k else {
            return Err(SnowError::cli(format!(
                "--env-vars key must be a string, got {}",
                yaml_repr(k)
            )));
        };
        if k.is_empty() {
            return Err(SnowError::cli("--env-vars key must not be empty"));
        }
        let kr = snow_click::py_repr(k);
        if !KEY_RE.is_match(k) {
            return Err(SnowError::cli(format!(
                "--env-vars key {kr} must contain only ASCII letters, digits, and underscores"
            )));
        }
        if !k.starts_with("DBT_") {
            return Err(SnowError::cli(format!(
                "--env-vars key {kr} must start with 'DBT_'"
            )));
        }
        if *k != k.to_uppercase() {
            return Err(SnowError::cli(format!(
                "--env-vars key {kr} must be uppercase (e.g. {})",
                snow_click::py_repr(&k.to_uppercase())
            )));
        }
        match v {
            serde_yaml::Value::Null => {
                return Err(SnowError::cli(format!(
                    "--env-vars value for {kr} must not be null"
                )));
            }
            serde_yaml::Value::String(s) => {
                if CONTROL_RE.is_match(s) {
                    return Err(SnowError::cli(format!(
                        "--env-vars value for {kr} must not contain control characters (newlines, tabs, etc.)"
                    )));
                }
                out.push((k.clone(), s.clone()));
            }
            other => {
                return Err(SnowError::cli(format!(
                    "--env-vars value for {kr} must be a string, got {}; quote scalars in YAML/JSON (e.g. '{k}: \"1\"' instead of '{k}: 1')",
                    py_type_name(other)
                )));
            }
        }
    }
    Ok(out)
}

fn yaml_repr(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::Bool(b) => crate::spcs::py_bool(*b).into(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Null => "None".into(),
        other => format!("{other:?}"),
    }
}

fn shell_env_vars() -> (Vec<(String, String)>, usize, usize) {
    let mut fwd = Vec::new();
    let (mut secrets, mut skipped) = (0, 0);
    for (k, v) in std::env::vars() {
        let up = k.to_uppercase();
        if !up.starts_with("DBT_") {
            continue;
        }
        if up.starts_with(SECRET_PREFIX) {
            secrets += 1;
            continue;
        }
        if k == up && KEY_RE.is_match(&k) && !CONTROL_RE.is_match(&v) {
            fwd.push((k, v));
        } else {
            skipped += 1;
        }
    }
    fwd.sort();
    (fwd, secrets, skipped)
}

fn process_args(args: &[String]) -> String {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--vars" && i + 1 < args.len() {
            out.push("--vars".to_string());
            out.push(format!("'{}'", args[i + 1]));
            i += 2;
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out.join(" ")
}

pub fn execute(cli: &mut Cli) -> Result<Output> {
    let leaf = cli.inv.leaf().clone();
    let dbt_command = leaf.info_name.clone();
    let name = Fqn::from_string(&cli.str("name").unwrap_or_default())?;
    let run_async = cli.flag("run_async");
    let mut command = dbt_command.clone();
    if !leaf.args.is_empty() {
        command = format!("{command} {}", process_args(&leaf.args))
            .trim()
            .to_string();
    }
    let mut q = format!("EXECUTE DBT PROJECT {name}");
    let imports = cli.list("imports");
    if !imports.is_empty() {
        let r: Result<Vec<String>> = imports.iter().map(|e| render_import(e)).collect();
        q.push_str(&format!(" IMPORTS=({})", r?.join(", ")));
    }
    if let Some(v) = cli.str("dbt_version") {
        q.push_str(&format!(" dbt_version={}", to_string_literal(&v)));
    }
    if let Some(e) = cli.str("environment") {
        q.push_str(&format!(" ENVIRONMENT={}", to_string_literal(&e)));
    }
    if let Some(w) = cli.opt_bool("writeback") {
        q.push_str(&format!(" WRITEBACK={}", sql_bool(w)));
    }
    // The Python command runs the rest inside `cli_console.spinner()`.
    let r = execute_rest(cli, q, command, run_async);
    if !run_async {
        cli.console.spinner_done();
    }
    r
}

fn execute_rest(cli: &mut Cli, mut q: String, command: String, run_async: bool) -> Result<Output> {
    let mut merged: Vec<(String, String)> = Vec::new();
    if cli.flag("use_shell_env_vars") {
        let (vars, secrets, skipped) = shell_env_vars();
        if secrets > 0 {
            cli.console.message(&format!(
                "--use-shell-env-vars: dropped {secrets} {SECRET_PREFIX}* environment {} from shell. To forward secrets, use the secrets: block in env.yml referencing a Snowflake SECRET object, or if those are not sensitive, pass them explicitly via --env-vars.",
                plural(secrets, "variable", "variables")
            ));
        }
        if skipped > 0 {
            cli.console.message(&format!(
                "--use-shell-env-vars: skipped {skipped} DBT_* shell environment {} that can't be forwarded; keys must be uppercase and contain only letters, digits, and underscores (e.g. export DBT_FOO, not DBT_Foo).",
                plural(skipped, "variable", "variables")
            ));
        }
        if !vars.is_empty() {
            cli.console.warning(&format!(
                "--use-shell-env-vars: forwarded {} shell environment {} starting with DBT_* into query text. Never put credentials, tokens, passwords, or other confidential data in shell environment variables with the DBT_ prefix.",
                vars.len(),
                plural(vars.len(), "variable", "variables")
            ));
        } else if secrets == 0 && skipped == 0 {
            cli.console.message("--use-shell-env-vars: no DBT_* environment variables found in the shell. Make sure the variables are exported (see your shell's documentation for how to export environment variables).");
        }
        merged.extend(vars);
    }
    if let Some(ev) = cli.str("env_vars") {
        for (k, v) in parse_env_vars(&ev)? {
            if let Some(slot) = merged.iter_mut().find(|(mk, _)| *mk == k) {
                slot.1 = v;
            } else {
                merged.push((k, v));
            }
        }
    }
    if !merged.is_empty() {
        let secret: Vec<&str> = merged
            .iter()
            .filter(|(k, _)| k.starts_with(SECRET_PREFIX))
            .map(|(k, _)| k.as_str())
            .collect();
        if !secret.is_empty() {
            cli.console.warning(&format!(
                "--env-vars contains key(s) with the {SECRET_PREFIX} prefix ({}); these values will appear in the SQL text and query history. To avoid that, use the secrets: block in env.yml referencing a Snowflake SECRET object.",
                secret.join(", ")
            ));
        }
        let items: Vec<String> = merged
            .iter()
            .map(|(k, v)| format!("{}={}", to_string_literal(k), to_string_literal(v)))
            .collect();
        q.push_str(&format!(" ENV_VARS=({})", items.join(", ")));
    }
    q.push_str(&format!(" args={}", to_string_literal(&command)));
    if run_async {
        let verbose = cli.connector_info;
        let c = cli
            .connection()?
            .execute_async(&q)
            .map_err(|e| snow_api::connect::db_error_to_cli(&e, verbose))?;
        return Ok(Output::message(format!(
            "Command submitted. You can check the result with `snow sql -q \"select execution_status from table(information_schema.query_history_by_user()) where query_id in ('{}');\"`",
            c.sfqid.unwrap_or_default()
        )));
    }
    let c = cli.execute_query(&q)?;
    let (Some(si), Some(oi)) = (c.column_index("SUCCESS"), c.column_index("STDOUT")) else {
        return Err(SnowError::cli("Malformed server response"));
    };
    let Some(last) = c.rows.last() else {
        return Err(SnowError::cli("No data returned from server"));
    };
    let success = last
        .get(si)
        .and_then(|x| x.as_deref())
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let output = last
        .get(oi)
        .and_then(|x| x.clone())
        .unwrap_or_else(|| "None".into());
    if success {
        Ok(Output::message(output))
    } else {
        Err(SnowError::cli(output))
    }
}

pub fn list(cli: &mut Cli) -> Result<Output> {
    object::list_for(cli, "dbt-project")
}
pub fn describe(cli: &mut Cli) -> Result<Output> {
    object::describe_for(cli, "dbt-project", "name")
}
pub fn drop(cli: &mut Cli) -> Result<Output> {
    object::drop_for(cli, "dbt-project", "name")
}

const PROFILES: &str = "profiles.yml";
const PROJECTS_PROFILES: &str = "dbt_projects_profiles.yml";
const ENV_FILE: &str = "env.yml";

fn abs(p: &std::path::Path) -> std::path::PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

fn resolve_profiles_filename(dir: &std::path::Path) -> Option<&'static str> {
    [PROJECTS_PROFILES, PROFILES]
        .into_iter()
        .find(|f| dir.join(f).exists())
}

fn parse_profiles(text: &str, filename: &str) -> Result<snow_api::project::PyV> {
    snow_api::project::yaml::safe_load(text)
        .map(|v| v.unwrap_or(snow_api::project::PyV::None))
        .map_err(|e| match e {
            snow_api::project::yaml::LoadError::Syntax(m) => {
                SnowError::cli(format!("{filename} is not valid YAML: {m}"))
            }
            snow_api::project::yaml::LoadError::Duplicate(k) => {
                SnowError::cli(format!("{filename} is not valid YAML: duplicate key {k}"))
            }
        })
}

fn validate_role(cli: &mut Cli, role: &str) -> Result<bool> {
    let new = snow_api::identifiers::to_identifier(role);
    let prev = cli.current_role()?;
    let different = prev
        .as_ref()
        .is_none_or(|p| p.to_lowercase() != new.to_lowercase());
    if different && cli.try_query(&format!("use role {new}"))?.is_err() {
        return Ok(false);
    }
    let ok = cli.try_query("select 1")?.is_ok();
    if different && let Some(p) = prev {
        cli.execute_query(&format!("use role {p}"))?;
    }
    Ok(ok)
}

fn validate_profiles(
    cli: &mut Cli,
    dir: &std::path::Path,
    profile: &str,
    default_target: Option<&str>,
) -> Result<()> {
    use snow_api::project::PyV;
    let Some(filename) = resolve_profiles_filename(dir) else {
        return Err(SnowError::cli(format!(
            "{PROJECTS_PROFILES} or {PROFILES} does not exist in directory {}.",
            abs(dir).display()
        )));
    };
    let text = std::fs::read_to_string(dir.join(filename))?;
    let profiles = parse_profiles(&text, filename)?;
    let PyV::Dict(profiles) = profiles else {
        return Err(SnowError::cli(format!(
            "{filename} does not contain any profiles."
        )));
    };
    let Some(p) = profiles.get(profile) else {
        return Err(SnowError::cli(format!(
            "Profile {profile} is not defined in {filename}."
        )));
    };
    let target = default_target
        .map(str::to_string)
        .or_else(|| p.str_of("target"));
    let outputs = p
        .get("outputs")
        .and_then(|o| o.as_dict())
        .cloned()
        .unwrap_or_default();
    let mut issues = Vec::new();
    match target.as_ref().and_then(|t| outputs.get(t).map(|o| (t, o))) {
        Some((t, details)) => {
            let keys: Vec<&String> = details
                .as_dict()
                .map(|d| d.keys().collect())
                .unwrap_or_default();
            let mut missing: Vec<&str> = ["database", "role", "schema", "type"]
                .into_iter()
                .filter(|k| !keys.iter().any(|x| x == k))
                .collect();
            missing.sort();
            if !missing.is_empty() {
                issues.push(format!(
                    "Missing required fields: {} in target {t}",
                    missing.join(", ")
                ));
            }
            if let Some(role) = details.str_of("role").filter(|r| !r.is_empty()) {
                let jinja = regex::Regex::new(r"(?s)\{\{.*?\}\}")
                    .expect("re")
                    .is_match(&role);
                if !jinja && !validate_role(cli, &role)? {
                    issues.push(format!(
                        "Role '{role}' does not exist or is not accessible."
                    ));
                }
            }
        }
        None => {
            let mut names: Vec<&String> = outputs.keys().collect();
            names.sort();
            issues.push(format!(
                "Target '{}' is not defined in profile '{profile}'. Available targets: {}",
                target.clone().unwrap_or_else(|| "None".into()),
                names
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if !issues.is_empty() {
        return Err(SnowError::cli(format!(
            "Found following errors in {filename}. Please fix them before proceeding:\n{profile}\n * {}",
            issues.join("\n * ")
        )));
    }
    Ok(())
}

fn rewrite_yaml(path: &std::path::Path, src_text: &str, filename: &str) -> Result<()> {
    let v = parse_profiles(src_text, filename)?;
    let _ = std::fs::remove_file(path);
    std::fs::write(path, snow_api::project::yaml::safe_dump_default_width(&v))?;
    Ok(())
}

pub fn deploy(cli: &mut Cli) -> Result<Output> {
    let fqn = crate::object::fqn_arg(cli, "name")?;
    let project = cli
        .str("source")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let project = crate::paths::resolve(abs(&project));
    let profiles_dir = cli
        .str("profiles_dir")
        .map(|p| crate::paths::resolve(abs(std::path::Path::new(&p))))
        .unwrap_or_else(|| project.clone());
    let env_dir = cli
        .str("env_file_dir")
        .map(|p| crate::paths::resolve(abs(std::path::Path::new(&p))));
    let dbt_project = project.join("dbt_project.yml");
    if !dbt_project.exists() {
        return Err(SnowError::cli(format!(
            "dbt_project.yml does not exist in directory {}.",
            project.display()
        )));
    }
    let dp = snow_api::project::yaml::safe_load(&std::fs::read_to_string(&dbt_project)?)
        .ok()
        .flatten()
        .unwrap_or(snow_api::project::PyV::None);
    let Some(profile) = dp.str_of("profile") else {
        return Err(SnowError::cli(
            "`profile` is not defined in dbt_project.yml",
        ));
    };
    let default_target = cli.str("default_target");
    validate_profiles(cli, &profiles_dir, &profile, default_target.as_deref())?;
    let dbt_version = cli.str("dbt_version");
    if let Some(v) = &dbt_version {
        let c = cli.try_query("SELECT SYSTEM$SUPPORTED_DBT_VERSIONS()")?.map_err(|_| {
            SnowError::cli("Could not fetch supported dbt versions from server. Ensure your Snowflake account supports SYSTEM$SUPPORTED_DBT_VERSIONS().")
        })?;
        let raw = Cli::first_cell_opt(&c)
            .ok_or_else(|| SnowError::cli("Could not fetch supported dbt versions from server."))?;
        let entries: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|_| SnowError::cli("Could not parse supported dbt versions from server."))?;
        let versions: Vec<String> = entries
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e["dbt_version"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if versions.is_empty() {
            return Err(SnowError::cli("Server returned no supported dbt versions."));
        }
        if !versions.contains(v) {
            return Err(SnowError::argument(format!(
                "Invalid value '{v}' for --dbt-version. Supported versions: {}.",
                versions.join(", ")
            )));
        }
    }
    let env_source = env_dir.clone().unwrap_or_else(|| project.clone());
    let env_file = env_source.join(ENV_FILE);
    if env_dir.is_some() && !env_file.exists() {
        return Err(SnowError::cli(format!(
            "{ENV_FILE} does not exist in directory {}.",
            env_source.display()
        )));
    }
    let env_content = if env_file.exists() {
        let raw = std::fs::read_to_string(&env_file)?;
        if let Err(e) = snow_api::project::yaml::load(&raw) {
            return Err(SnowError::cli(match e {
                snow_api::project::yaml::LoadError::Duplicate(k) => {
                    format!("Failed to parse {ENV_FILE}: found duplicate key {k}")
                }
                snow_api::project::yaml::LoadError::Syntax(m) => {
                    format!("{ENV_FILE} is not valid YAML: {m}")
                }
            }));
        }
        Some(raw)
    } else {
        None
    };
    cli.console.phase_start("Creating temporary stage");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let safe = snow_api::identifiers::sanitize_identifier(
        &snow_api::identifiers::unquote_identifier(&fqn.name),
    );
    let stage_fqn =
        crate::stage::temp_stage_name(cli, &fqn, format!("DBT_PROJECT_{safe}_{ts}_STAGE"))?;
    cli.execute_query(&crate::stage::create_query(
        &stage_fqn, true, None, false, None,
    ))?;
    let stage_name = format!("@{}", stage_fqn.identifier());
    cli.console.phase_end(None);
    cli.console.phase_start("Copying project files to stage");
    let tmp = tempfile::tempdir()?;
    crate::stage::copy_to_tmp_dir(&project, tmp.path())?;
    for cand in [PROJECTS_PROFILES, PROFILES] {
        let f = tmp.path().join(cand);
        if f.exists() {
            let t = std::fs::read_to_string(&f)?;
            rewrite_yaml(&f, &t, cand)?;
        }
    }
    let filename = resolve_profiles_filename(&profiles_dir).unwrap_or(PROFILES);
    let is_root = profiles_dir == project;
    if filename == PROJECTS_PROFILES && profiles_dir.join(PROFILES).exists() {
        cli.console.warning(&if is_root {
            format!("Both {PROJECTS_PROFILES} and {PROFILES} found in the project root. {PROJECTS_PROFILES} takes precedence and will be used for deployment. Both files will be present in the deployed object.")
        } else {
            format!("Both {PROJECTS_PROFILES} and {PROFILES} found in {}. Snowflake CLI will only copy {PROJECTS_PROFILES} to the root of the deployed object — this file will be used for deployment.", profiles_dir.display())
        });
    }
    if filename != PROFILES && !is_root {
        for cand in [PROJECTS_PROFILES, PROFILES] {
            if cand != filename {
                let _ = std::fs::remove_file(tmp.path().join(cand));
            }
        }
    }
    let src_text = std::fs::read_to_string(profiles_dir.join(filename))?;
    rewrite_yaml(&tmp.path().join(filename), &src_text, filename)?;
    if let Some(c) = &env_content {
        let p = tmp.path().join(ENV_FILE);
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, c)?;
    }
    let rows = crate::stage::put_tree(cli, tmp.path(), &stage_name, 4, false, false)?;
    cli.console.step(&format!("Copied {} files", rows.len()));
    cli.console.phase_end(None);
    cli.console.phase_start("Creating DBT project");
    let q = deploy_query(cli, &fqn, &stage_name, dbt_version.as_deref())?;
    let c = cli.execute_query(&q);
    cli.console.phase_end(None);
    let c = c?;
    Ok(cli.query_result(&c))
}

fn sql_bool(b: bool) -> &'static str {
    if b { "TRUE" } else { "FALSE" }
}

fn deploy_query(
    cli: &mut Cli,
    fqn: &snow_api::identifiers::Fqn,
    stage: &str,
    dbt_version: Option<&str>,
) -> Result<String> {
    use snow_api::identifiers::to_string_literal as lit;
    let default_target = cli.str("default_target");
    let default_env = cli.str("default_env");
    let eais = cli
        .given("external_access_integrations")
        .then(|| cli.list("external_access_integrations"));
    let local_deps = cli.flag("install_local_deps");
    let writeback = cli.opt_bool("default_writeback");
    let auto_compile = cli.opt_bool("auto_compile");
    let id = fqn.identifier();
    let props = |q: &mut String| {
        if let Some(t) = &default_target {
            q.push_str(&format!(" DEFAULT_TARGET='{t}'"));
        }
        if let Some(e) = &default_env {
            q.push_str(&format!(" DEFAULT_ENVIRONMENT={}", lit(e)));
        }
        if let Some(v) = dbt_version {
            q.push_str(&format!(" DBT_VERSION={}", lit(v)));
        }
        if let Some(w) = writeback {
            q.push_str(&format!(" DEFAULT_WRITEBACK={}", sql_bool(w)));
        }
        if let Some(a) = auto_compile {
            q.push_str(&format!(" AUTO_COMPILE={}", sql_bool(a)));
        }
    };
    let eai_clause = |q: &mut String| match &eais {
        Some(e) if !e.is_empty() => q.push_str(&format!(
            "\nEXTERNAL_ACCESS_INTEGRATIONS = ({})",
            e.join(", ")
        )),
        _ if local_deps => q.push_str("\nEXTERNAL_ACCESS_INTEGRATIONS = ()"),
        _ => {}
    };
    if cli.flag("force") {
        let mut q = format!("CREATE OR REPLACE DBT PROJECT {id}\nFROM {stage}");
        props(&mut q);
        eai_clause(&mut q);
        return Ok(q);
    }
    let attrs = match cli.try_query(&format!("describe dbt project {}", fqn.sql_identifier()))? {
        Err(e)
            if e.msg_with(true).contains("DBT PROJECT")
                && e.msg_with(true).contains("does not exist") =>
        {
            None
        }
        Err(e) => return Err(snow_api::connect::db_error_to_cli(&e, cli.connector_info)),
        Ok(c) => cli.rows(&c).into_iter().next().map(|m| {
            m.into_iter()
                .map(|(k, v)| (k.to_lowercase(), v))
                .collect::<indexmap::IndexMap<_, _>>()
        }),
    };
    let Some(attrs) = attrs else {
        let mut q = format!("CREATE DBT PROJECT {id}\nFROM {stage}");
        props(&mut q);
        eai_clause(&mut q);
        return Ok(q);
    };
    let s = |k: &str| match attrs.get(k) {
        None | Some(snow_api::output::Pv::None) => None,
        Some(v) => Some(v.py_str()),
    };
    let bool_of = |k: &str| {
        s(k).and_then(|v| snow_api::config::try_cast_to_bool(&snow_api::config::Cv::Str(v)))
    };
    let mut set = Vec::new();
    let mut unset = Vec::new();
    let current_target = s("default_target");
    if cli.flag("unset_default_target") && current_target.is_some() {
        unset.push("DEFAULT_TARGET");
    } else if let Some(t) = &default_target
        && current_target
            .as_ref()
            .is_none_or(|c| c.to_lowercase() != t.to_lowercase())
    {
        set.push(format!("DEFAULT_TARGET='{t}'"));
    }
    if cli.flag("unset_default_env") {
        unset.push("DEFAULT_ENVIRONMENT");
    } else if let Some(e) = &default_env {
        set.push(format!("DEFAULT_ENVIRONMENT={}", lit(e)));
    }
    if let Some(v) = dbt_version {
        set.push(format!("DBT_VERSION={}", lit(v)));
    }
    if let Some(w) = writeback
        && bool_of("default_writeback") != Some(w)
    {
        set.push(format!("DEFAULT_WRITEBACK={}", sql_bool(w)));
    }
    if let Some(a) = auto_compile
        && bool_of("auto_compile") != Some(a)
    {
        set.push(format!("AUTO_COMPILE={}", sql_bool(a)));
    }
    let current: std::collections::BTreeSet<String> = s("external_access_integrations")
        .map(|v| {
            v.trim_matches(['[', ']'])
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let update = match &eais {
        Some(r) => current != r.iter().cloned().collect(),
        None if local_deps => !current.is_empty(),
        None => false,
    };
    if update {
        match &eais {
            Some(r) if !r.is_empty() => {
                let mut v = r.clone();
                v.sort();
                set.push(format!("EXTERNAL_ACCESS_INTEGRATIONS=({})", v.join(", ")));
            }
            _ if local_deps => set.push("EXTERNAL_ACCESS_INTEGRATIONS=()".into()),
            _ => {}
        }
    }
    if !set.is_empty() {
        cli.execute_query(&format!("ALTER DBT PROJECT {id} SET {}", set.join(", ")))?;
    }
    for u in unset {
        cli.execute_query(&format!("ALTER DBT PROJECT {id} UNSET {u}"))?;
    }
    Ok(format!("ALTER DBT PROJECT {id} ADD VERSION\nFROM {stage}"))
}
