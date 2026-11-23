//! `snow helpers`.

use indexmap::IndexMap;
use snow_api::output::{Output, Pv, Row};
use snow_api::{Cli, Result};

/// Extracted from `_plugins/helpers/snowsl_vars_reader.py` (`KNOWN_SNOWSQL_ENV_VARS`).
const KNOWN: &str = include_str!("../data/snowsql_env_vars.json");

pub fn check_snowsql_env_vars(_cli: &mut Cli) -> Result<Output> {
    let known: IndexMap<String, IndexMap<String, String>> =
        serde_json::from_str(KNOWN).unwrap_or_default();
    let mut discovered = Vec::new();
    let mut unused = Vec::new();
    for (k, _) in std::env::vars() {
        if !k.to_lowercase().starts_with("snowsql") {
            continue;
        }
        match known.get(&k) {
            Some(s) => discovered.push(
                s.iter()
                    .map(|(a, b)| (a.clone(), Pv::s(b.clone())))
                    .collect::<Row>(),
            ),
            None => unused.push(
                [
                    ("Found", k.as_str()),
                    ("Suggested", "n/a"),
                    ("Additional info", "Unused variable"),
                ]
                .iter()
                .map(|(a, b)| (a.to_string(), Pv::s(*b)))
                .collect::<Row>(),
            ),
        }
    }
    let summary = format!(
        "Found {} SnowSQL environment variables, {} with replacements, {} unused.",
        discovered.len() + unused.len(),
        discovered.len(),
        unused.len()
    );
    let mut out = Vec::new();
    if !discovered.is_empty() {
        out.push(Output::Collection(discovered));
    }
    if !unused.is_empty() {
        out.push(Output::Collection(unused));
    }
    out.push(Output::message(summary));
    Ok(Output::Multiple(out))
}

/// PEP 440 release tuple plus dev/pre marker for ordering (enough for CLI versions).
fn version_key(v: &str) -> (Vec<u64>, i64, u64) {
    let (rel, rest) = match v.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
        Some(i) => (&v[..i], &v[i..]),
        None => (v, ""),
    };
    let mut nums: Vec<u64> = rel
        .trim_end_matches('.')
        .split('.')
        .filter_map(|p| p.parse().ok())
        .collect();
    while nums.last() == Some(&0) {
        nums.pop();
    }
    let r = rest.trim_start_matches('.').to_lowercase();
    let digits = |s: &str| {
        s.chars()
            .filter(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    };
    let phase = if r.starts_with("dev") {
        -4
    } else if r.starts_with('a') {
        -3
    } else if r.starts_with('b') {
        -2
    } else if r.starts_with("rc") {
        -1
    } else if r.starts_with("post") {
        1
    } else {
        0
    };
    (nums, phase, digits(&r))
}

pub fn version_gt(a: &str, b: &str) -> bool {
    version_key(a) > version_key(b)
}

fn cache_path(cli: &Cli) -> std::path::PathBuf {
    cli.config
        .file_path
        .parent()
        .map(|p| p.join(".cli_version.cache"))
        .unwrap_or_default()
}

/// `time.time()`.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn fetch_latest() -> Option<String> {
    let agent = snow_connector::http_agent(std::time::Duration::from_secs(1));
    let brew = agent
        .get("https://formulae.brew.sh/api/formula/snowflake-cli.json")
        .call()
        .ok()
        .and_then(|r| r.into_string().ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["versions"]["stable"].as_str().map(str::to_string));
    brew.or_else(|| {
        agent
            .get("https://pypi.org/pypi/snowflake-cli/json")
            .call()
            .ok()
            .and_then(|r| r.into_string().ok())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["info"]["version"].as_str().map(str::to_string))
    })
}

pub fn check_version(cli: &mut Cli) -> Result<Output> {
    if cli.config.bool_value(&["cli"], "air_gapped", Some(false))? == Some(true) {
        return Err(snow_api::SnowError::cli(
            "Version checks are disabled because cli.air_gapped is set.",
        ));
    }
    let path = cache_path(cli);
    let mut data: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let fresh = data
        .get("last_time_check")
        .and_then(|v| v.as_f64())
        .is_some_and(|t| t > now() - 3600.0);
    let latest = if fresh && !cli.flag("refresh") {
        data.get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    } else {
        let v = fetch_latest();
        if let Some(v) = &v {
            data.insert("last_time_check".into(), serde_json::json!(now()));
            data.insert("version".into(), serde_json::json!(v));
        }
        v
    };
    let Some(latest) = latest else {
        return Err(snow_api::SnowError::cli(
            "Could not determine the latest Snowflake CLI version. Check your network connection and try again. Re-run with --debug to see the underlying error.",
        ));
    };
    data.insert("last_time_shown".into(), serde_json::json!(now()));
    let _ = std::fs::write(
        &path,
        snow_api::pyfmt::json_dumps(&serde_json::Value::Object(data), None),
    );
    Ok(Output::object(vec![
        ("current_version", Pv::s(snow_api::VERSION)),
        ("latest_version", Pv::s(latest.clone())),
        (
            "update_available",
            Pv::Bool(version_gt(&latest, snow_api::VERSION)),
        ),
    ]))
}

/// JSON Schemas of `snowflake.yml`, generated from upstream's pydantic models.
fn schema_for(version: &str) -> &'static str {
    match version {
        "1" => include_str!("../data/project_schema_v1.json"),
        "1.1" => include_str!("../data/project_schema_v1.1.json"),
        _ => include_str!("../data/project_schema_v2.json"),
    }
}

/// The `sort_keys=True` part of `json.dumps(x, indent=2, sort_keys=True)`: keys sorted at every level.
fn sorted(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            serde_json::Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted(&m[k])))
                    .collect(),
            )
        }
        serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

pub fn generate_project_schema(cli: &mut Cli) -> Result<Output> {
    let version = cli.str("version").unwrap_or_else(|| "2".into());
    let schema: serde_json::Value = serde_json::from_str(schema_for(&version)).unwrap_or_default();
    let text = snow_api::pyfmt::json_dumps(&sorted(&schema), Some(2));
    if let Some(out) = cli.str("output_file") {
        let p = std::path::Path::new(&out);
        let parent = p
            .parent()
            .filter(|x| !x.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        if !parent.exists() {
            return Err(snow_api::SnowError::click(format!(
                "Directory '{}' does not exist.",
                parent.display()
            )));
        }
        std::fs::write(p, format!("{text}\n"))?;
        return Ok(Output::message(format!(
            "Project definition schema written to {out}."
        )));
    }
    if cli.format.is_json() {
        return Ok(match Pv::from_json(&schema) {
            Pv::Dict(d) => Output::Object(Some(d)),
            _ => Output::Object(None),
        });
    }
    Ok(Output::message(text))
}

/// `codecs.lookup(enc).name` for the encodings a CLI user realistically configures.
fn canonical_encoding(enc: &str) -> Option<String> {
    let e = enc.trim().to_lowercase().replace(['_', ' '], "-");
    let name = match e.as_str() {
        "utf-8" | "utf8" | "u8" | "utf" | "cp65001" => "utf-8",
        "ascii" | "us-ascii" | "646" => "ascii",
        "latin-1" | "latin1" | "iso8859-1" | "iso-8859-1" | "l1" | "8859" | "cp819" => "iso8859-1",
        "utf-16" | "utf16" => "utf-16",
        "utf-32" | "utf32" => "utf-32",
        "utf-8-sig" => "utf-8-sig",
        "shift-jis" | "sjis" => "shift_jis",
        "euc-jp" | "eucjp" => "euc_jp",
        "gbk" | "gb2312" | "big5" | "gb18030" => return Some(e.replace('-', "")),
        _ => {
            if let Some(n) = e.strip_prefix("cp").or_else(|| e.strip_prefix("windows-")) {
                if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() {
                    return Some(format!("cp{n}"));
                }
                return None;
            }
            if let Some(n) = e
                .strip_prefix("iso-8859-")
                .or_else(|| e.strip_prefix("iso8859-"))
                && n.parse::<u8>().is_ok_and(|n| (1..=16).contains(&n))
            {
                return Some(format!("iso8859-{n}"));
            }
            return None;
        }
    };
    Some(name.to_string())
}

fn configured_encoding(cli: &Cli, key: &str) -> Result<Option<String>> {
    let Some(v) = cli.config.value(&["cli", "encoding"], key) else {
        return Ok(None);
    };
    let v = v.py_str();
    if canonical_encoding(&v).is_none() {
        return Err(snow_api::SnowError::click(format!(
            "Invalid encoding '{v}' configured for cli.encoding.{key}. Please use a valid Python codec name (e.g. 'utf-8', 'cp1252')."
        )));
    }
    Ok(Some(v))
}

/// Python's `locale.getpreferredencoding(False)` on POSIX.
fn locale_encoding() -> String {
    if std::env::var("PYTHONUTF8").as_deref() == Ok("1") {
        return "utf-8".into();
    }
    let loc = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    let base = loc.split('@').next().unwrap_or_default();
    match base.split_once('.') {
        Some((_, cs)) => {
            canonical_encoding(cs).unwrap_or_else(|| cs.to_lowercase().replace('_', "-"))
        }
        None => "utf-8".into(),
    }
}

/// `config_init`'s encoding validation and `detect_encoding_environment` warning.
pub fn startup_encoding_check(cli: &mut Cli) -> Result<()> {
    configured_encoding(cli, "stdout")?;
    if cli
        .config
        .bool_value(&["cli", "encoding"], "show_warnings", Some(true))?
        != Some(true)
    {
        return Ok(());
    }
    if !encodings_configured(cli)? && locale_encoding() != "utf-8" {
        cli.warnings.push(
            "UserWarning: Encoding mismatch detected. Run 'snow helpers detect-encoding' for more details.".into(),
        );
    }
    Ok(())
}

/// Whether `file_io`, `subprocess` and `stdout` all have a configured encoding.
fn encodings_configured(cli: &Cli) -> Result<bool> {
    for k in ["file_io", "subprocess", "stdout"] {
        if configured_encoding(cli, k)?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn detect_encoding(cli: &mut Cli) -> Result<Output> {
    if encodings_configured(cli)? {
        return Ok(Output::message(
            "No encoding issues - your system is properly configured.",
        ));
    }
    let (fs, default, loc) = ("utf-8", "utf-8", locale_encoding());
    let actionable = "This may cause file corruption when sharing projects across platforms.\nRecommended actions:\n1. Set environment variable: PYTHONUTF8=1\n2. Configure encoding in config.toml:\n   [cli.encoding]\n   file_io = \"utf-8\"\n   subprocess = \"utf-8\"\n   stdout = \"utf-8\"\n3. Set environment variables: SNOWFLAKE_CLI_ENCODING_FILE_IO='utf-8', SNOWFLAKE_CLI_ENCODING_SUBPROCESS='utf-8', and SNOWFLAKE_CLI_ENCODING_STDOUT='utf-8'";
    if loc != fs {
        return Ok(Output::message(format!(
            "Encoding mismatch detected:\n  Filesystem: {fs}\n  Default:    {default}\n  Locale:     {loc}\n\n{actionable}"
        )));
    }
    Ok(Output::message(
        "No encoding issues - your system is properly configured.",
    ))
}

pub fn show_config_sources(_cli: &mut Cli) -> Result<Output> {
    Ok(Output::message(
        "⚠️  Configuration resolution logging is not available.\n\nTo enable it, set the environment variable:\n    export SNOWFLAKE_CLI_CONFIG_V2_ENABLED=true\n\nThen run this command again to see where configuration values come from.",
    ))
}

mod installer_path {
    const HISTORICAL_COMMENT: &str = "# added by Snowflake SnowflakeCLI installer v1.0";
    const MACOS_APP_PATH: &str = "SnowflakeCLI.app/Contents/MacOS";
    const BEGIN_MARK: &str = "# snowflake-cli PATH begin";
    const END_MARK: &str = "# snowflake-cli PATH end";

    fn text(l: &str) -> &str {
        l.trim_end_matches(['\r', '\n'])
    }

    fn is_historical_path_line(line: &str) -> bool {
        let Some(rest) = line.strip_prefix("export PATH=") else {
            return false;
        };
        if line.contains("brew shellenv") {
            return false;
        }
        rest.split_once(":$PATH")
            .is_some_and(|(dest, _)| dest.contains(MACOS_APP_PATH))
    }

    /// Python `str.splitlines(keepends=True)` for \n, \r\n and \r.
    fn split_keep(s: &str) -> Vec<&str> {
        let b = s.as_bytes();
        let (mut out, mut start, mut i) = (Vec::new(), 0, 0);
        while i < b.len() {
            if b[i] == b'\n' || b[i] == b'\r' {
                let end = if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
                    i + 2
                } else {
                    i + 1
                };
                out.push(&s[start..end]);
                start = end;
                i = end;
            } else {
                i += 1;
            }
        }
        if start < s.len() {
            out.push(&s[start..]);
        }
        out
    }

    pub fn clean(contents: &str) -> (String, usize, Vec<usize>) {
        let lines = split_keep(contents);
        let mut protected = std::collections::HashSet::new();
        let mut inside = false;
        for (i, l) in lines.iter().enumerate() {
            if text(l) == BEGIN_MARK {
                inside = true;
            }
            if inside {
                protected.insert(i);
            }
            if text(l) == END_MARK && inside {
                inside = false;
            }
        }
        let mut removed = std::collections::HashSet::new();
        let (mut unpaired, mut pairs) = (Vec::new(), 0);
        for (i, l) in lines.iter().enumerate() {
            if text(l) != HISTORICAL_COMMENT {
                continue;
            }
            let mut f = i + 1;
            while f < lines.len() && text(lines[f]).trim().is_empty() {
                f += 1;
            }
            let (hist, crosses) = if f < lines.len() {
                (
                    is_historical_path_line(text(lines[f])),
                    (i..=f).any(|x| protected.contains(&x)),
                )
            } else {
                (false, false)
            };
            if hist && !crosses {
                removed.extend(i..=f);
                let t = f + 1;
                if t < lines.len() && !protected.contains(&t) && text(lines[t]).trim().is_empty() {
                    removed.insert(t);
                }
                pairs += 1;
            } else if !hist {
                unpaired.push(i + 1);
            }
        }
        let cleaned = lines
            .iter()
            .enumerate()
            .filter(|(i, _)| !removed.contains(i))
            .map(|(_, l)| *l)
            .collect();
        (cleaned, pairs, unpaired)
    }
}

pub fn clean_installer_path(cli: &mut Cli) -> Result<Output> {
    use snow_api::sanitize::for_terminal as san;
    if !cfg!(target_os = "macos") {
        return Ok(Output::message(
            "This command is intended only for macOS. Shell startup files were not scanned.",
        ));
    }
    if std::env::var("USER").as_deref() == Ok("root") || std::env::var("EUID").as_deref() == Ok("0")
    {
        return Err(snow_api::SnowError::cli(
            "This command must not be run as root. Run it as the user whose shell files should be cleaned.",
        ));
    }
    let apply = cli.flag("apply_changes");
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let (mut files, mut missing, mut skipped, mut links) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for name in [
        ".zprofile",
        ".zshrc",
        ".profile",
        ".bash_profile",
        ".bashrc",
    ] {
        let p = home.join(name);
        if !p.exists() {
            missing.push(p);
            continue;
        }
        if p.is_symlink() {
            links.push(p);
            continue;
        }
        let res = (|| -> std::io::Result<(usize, Vec<usize>)> {
            let contents = std::fs::read_to_string(&p)?;
            let (cleaned, pairs, unpaired) = installer_path::clean(&contents);
            if apply && pairs > 0 {
                let bak = p.with_file_name(format!("{name}-snowflake-cleanup.bak"));
                let _ = std::fs::remove_file(&bak);
                std::fs::copy(&p, &bak)?;
                let mode = std::fs::metadata(&p)?.permissions();
                let tmp = p.with_file_name(format!("{name}.snowflake-cleanup.tmp"));
                let _ = std::fs::remove_file(&tmp);
                let r = std::fs::write(&tmp, cleaned)
                    .and_then(|_| std::fs::set_permissions(&tmp, mode))
                    .and_then(|_| std::fs::rename(&tmp, &p));
                if r.is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    let _ = std::fs::remove_file(&bak);
                }
                r?;
            }
            Ok((pairs, unpaired))
        })();
        match res {
            Ok((pairs, unpaired)) => files.push((p, pairs, unpaired)),
            Err(_) => skipped.push(p),
        }
    }
    let action = if apply { "Removed" } else { "Would remove" };
    let mut msgs = Vec::new();
    let mut found = false;
    for (p, pairs, unpaired) in &files {
        let sp = san(&p.display().to_string());
        for n in unpaired {
            found = true;
            msgs.push(format!(
                "Unpaired historical installer comment in {sp} at line {n}; leaving it unchanged."
            ));
        }
        if *pairs > 0 {
            found = true;
            let noun = if *pairs == 1 { "pair" } else { "pairs" };
            msgs.push(format!(
                "{action} {pairs} historical installer PATH {noun} from {sp}."
            ));
        }
    }
    for p in &skipped {
        msgs.push(format!(
            "Skipped {}; the file could not be read or written.",
            san(&p.display().to_string())
        ));
    }
    for p in &links {
        msgs.push(format!(
            "Skipped {}: it is a symlink and is not modified by this command.",
            san(&p.display().to_string())
        ));
    }
    if !found {
        let scope = if skipped.is_empty() && links.is_empty() {
            ""
        } else {
            " in the files that could be scanned"
        };
        msgs.push(format!(
            "No historical installer PATH entries found{scope}."
        ));
    }
    let fmt = |ps: &mut dyn Iterator<Item = &std::path::PathBuf>| {
        ps.map(|p| san(&p.display().to_string()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !files.is_empty() {
        msgs.push(format!("Scanned {}.", fmt(&mut files.iter().map(|f| &f.0))));
    }
    if !missing.is_empty() {
        msgs.push(format!("Not present: {}.", fmt(&mut missing.iter())));
    }
    let summary = msgs.join("\n");
    if apply && (!skipped.is_empty() || !links.is_empty()) {
        return Err(snow_api::SnowError::cli(format!(
            "{summary}\nSome shell startup files could not be cleaned. Check their permissions and run the command again."
        )));
    }
    Ok(Output::message(summary))
}

/// `snow plugin …`: external plugins are Python entry points, so none is ever installed.
pub fn plugin_list(_cli: &mut Cli) -> Result<Output> {
    Ok(Output::Collection(Vec::new()))
}

pub fn plugin_toggle(cli: &mut Cli) -> Result<Output> {
    let name = cli.str("plugin_name").unwrap_or_default();
    Err(snow_api::SnowError::cli(format!(
        "Plugin {name} is not installed. Available plugins: ."
    ))
    .named("PluginNotInstalledError"))
}

pub fn v1_to_v2(cli: &mut Cli) -> Result<Output> {
    use snow_api::project::{PyV, conversion, definition, model, yaml};
    let root = cli.project_root();
    let mut files = definition::definition_files(&root);
    let local = root.join(definition::USER);
    let has_local = files.contains(&local);
    if has_local {
        match cli.opt_bool("migrate_local_yml") {
            None => {
                return Err(snow_api::SnowError::click(
                    "snowflake.local.yml file detected, please specify --migrate-local-overrides to include or --no-migrate-local-overrides to exclude its values.",
                ));
            }
            Some(false) => files.retain(|f| *f != local),
            Some(true) => {}
        }
    }
    if files.is_empty() {
        return Err(snow_api::SnowError::missing_project_definition());
    }
    let pd = definition::load_unrendered(&files, &[])?.unwrap_or(PyV::None);
    if definition::version_ge(&pd.str_of("definition_version").unwrap_or_default(), "2") {
        return Ok(Output::message(
            "Project definition is already at version 2.",
        ));
    }
    let ctx = cli
        .project(false)?
        .map(|p| p.ctx.clone())
        .unwrap_or(PyV::None);
    let template_ctx = PyV::Dict([("ctx".to_string(), ctx)].into_iter().collect());
    let mut warns = Vec::new();
    let data = conversion::to_v2_data(
        &pd,
        cli.flag("accept_templates"),
        Some(&template_ctx),
        &mut warns,
    )?;
    for w in warns {
        crate::logs::log(
            crate::logs::WARNING,
            "snowflake.cli.api.project.definition_conversion",
            &w,
        );
    }
    let v2 = definition::build(&data, model::Ctx::default(), &[])?;
    std::fs::rename("snowflake.yml", "snowflake_V1.yml")?;
    if has_local {
        std::fs::rename("snowflake.local.yml", "snowflake_V1.local.yml")?;
    }
    std::fs::write("snowflake.yml", yaml::safe_dump(&model::dump_set(&v2)))?;
    Ok(Output::message("Project definition migrated to version 2."))
}
