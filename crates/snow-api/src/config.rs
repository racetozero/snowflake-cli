//! Configuration files: `config.toml` and `connections.toml` under
//! `SNOWFLAKE_HOME`, with `SNOWFLAKE_*` environment overrides.
//!
//! Mirrors `snowflake.cli.api.config` on top of the connector's
//! `ConfigManager`: `connections.toml`, when it exists, replaces the
//! `[connections]` section of `config.toml`; every key can be overridden by
//! `SNOWFLAKE_<SECTION>_<KEY>`; writes preserve the file's formatting.

use crate::errors::{Result, SnowError};
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

pub const CONNECTIONS_SECTION: &str = "connections";
pub const CLI_SECTION: &str = "cli";

/// A configuration value as the Python code sees it after `unwrap()`.
#[derive(Debug, Clone, PartialEq)]
pub enum Cv {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<Cv>),
    Table(IndexMap<String, Cv>),
}

impl Cv {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Cv::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_table(&self) -> Option<&IndexMap<String, Cv>> {
        match self {
            Cv::Table(t) => Some(t),
            _ => None,
        }
    }
    /// Python `str(value)`.
    pub fn py_str(&self) -> String {
        match self {
            Cv::Str(s) => s.clone(),
            Cv::Int(i) => i.to_string(),
            Cv::Float(f) => crate::pyfmt::float_repr(*f),
            Cv::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            Cv::List(l) => format!(
                "[{}]",
                l.iter().map(Cv::py_repr).collect::<Vec<_>>().join(", ")
            ),
            Cv::Table(t) => format!(
                "{{{}}}",
                t.iter()
                    .map(|(k, v)| format!("{}: {}", snow_click::py_repr(k), v.py_repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    pub fn py_repr(&self) -> String {
        match self {
            Cv::Str(s) => snow_click::py_repr(s),
            other => other.py_str(),
        }
    }
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Cv::Str(s) => serde_json::Value::String(s.clone()),
            Cv::Int(i) => serde_json::json!(i),
            Cv::Float(f) => serde_json::json!(f),
            Cv::Bool(b) => serde_json::Value::Bool(*b),
            Cv::List(l) => serde_json::Value::Array(l.iter().map(Cv::to_json).collect()),
            Cv::Table(t) => {
                serde_json::Value::Object(t.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
            }
        }
    }
}

fn value_to_cv(v: &TomlValue) -> Cv {
    match v {
        TomlValue::String(s) => Cv::Str(s.value().clone()),
        TomlValue::Integer(i) => Cv::Int(*i.value()),
        TomlValue::Float(f) => Cv::Float(*f.value()),
        TomlValue::Boolean(b) => Cv::Bool(*b.value()),
        TomlValue::Datetime(d) => Cv::Str(d.value().to_string()),
        TomlValue::Array(a) => Cv::List(a.iter().map(value_to_cv).collect()),
        TomlValue::InlineTable(t) => Cv::Table(
            t.iter()
                .map(|(k, v)| (k.to_string(), value_to_cv(v)))
                .collect(),
        ),
    }
}

pub fn item_to_cv(i: &Item) -> Option<Cv> {
    match i {
        Item::None => None,
        Item::Value(v) => Some(value_to_cv(v)),
        Item::Table(t) => Some(Cv::Table(
            t.iter()
                .filter_map(|(k, v)| item_to_cv(v).map(|c| (k.to_string(), c)))
                .collect(),
        )),
        Item::ArrayOfTables(a) => Some(Cv::List(
            a.iter()
                .map(|t| {
                    Cv::Table(
                        t.iter()
                            .filter_map(|(k, v)| item_to_cv(v).map(|c| (k.to_string(), c)))
                            .collect(),
                    )
                })
                .collect(),
        )),
    }
}

/// `snowflake.connector.sf_dirs`: the directory holding the config files.
pub fn snowflake_home_dir() -> PathBuf {
    let raw = std::env::var("SNOWFLAKE_HOME").unwrap_or_else(|_| "~/.snowflake/".into());
    let home = expand_user(&raw);
    if home.exists() {
        return home;
    }
    platform_config_dir()
}

pub fn expand_user(p: &str) -> PathBuf {
    if (p == "~" || p.starts_with("~/"))
        && let Some(h) = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
    {
        return if p == "~" { h } else { h.join(&p[2..]) };
    }
    PathBuf::from(p)
}

fn platform_config_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_default();
        home.join("Library/Application Support/snowflake")
    } else if cfg!(windows) {
        dirs::data_local_dir().unwrap_or_default().join("snowflake")
    } else {
        match std::env::var("XDG_CONFIG_HOME") {
            Ok(x) if !x.trim().is_empty() => PathBuf::from(x).join("snowflake"),
            _ => expand_user("~/.config").join("snowflake"),
        }
    }
}

pub fn env_var_name(path: &[&str], key: &str) -> String {
    let mut parts = vec!["snowflake"];
    parts.extend_from_slice(path);
    parts.push(key);
    parts.join("_").to_uppercase()
}

pub fn env_value(path: &[&str], key: &str) -> Option<String> {
    std::env::var(env_var_name(path, key)).ok()
}

/// Python `try_cast_to_bool`.
pub fn try_cast_to_bool(v: &Cv) -> Option<bool> {
    match v {
        Cv::Bool(b) => Some(*b),
        Cv::Int(i) if *i == 0 || *i == 1 => Some(*i == 1),
        Cv::Str(s) => match s.to_lowercase().as_str() {
            "true" | "1" | "yes" | "y" | "on" | "t" => Some(true),
            "false" | "0" | "no" | "n" | "off" | "f" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[derive(Debug)]
pub struct Config {
    /// The file `--config-file` names, or `<home>/config.toml`.
    pub file_path: PathBuf,
    pub connections_file: PathBuf,
    pub is_override: bool,
    doc: DocumentMut,
    /// `connections.toml` parsed, when it exists.
    connections_doc: Option<DocumentMut>,
}

fn mode_of(p: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).ok().map(|m| m.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        None
    }
}

/// The connector reads `config.toml` and `connections.toml` from SNOWFLAKE_HOME when
/// it is imported, before any CLI code runs, warning about files others can read.
pub fn connector_import_warnings() -> Vec<String> {
    let skip = || {
        for var in [
            "SF_SKIP_TOKEN_FILE_PERMISSIONS_VERIFICATION",
            "SKIP_TOKEN_FILE_PERMISSIONS_VERIFICATION",
        ] {
            if let Ok(v) = std::env::var(var) {
                return v.to_lowercase() == "true";
            }
        }
        std::env::var("SF_SKIP_WARNING_FOR_READ_PERMISSIONS_ON_CONFIG_FILE")
            .is_ok_and(|v| v.to_lowercase() == "true")
    };
    let home = snowflake_home_dir();
    let mut out = Vec::new();
    for f in [home.join("config.toml"), home.join("connections.toml")] {
        let Some(mode) = mode_of(&f) else { continue };
        if !f.is_file() {
            continue;
        }
        if mode & 0o022 != 0 {
            break;
        }
        if mode & 0o044 != 0 && !skip() {
            let p = f.display();
            out.push(format!(
                "UserWarning: Bad owner or permissions on {p}.\n * To change owner, run `chown $USER \"{p}\"`.\n * To restrict permissions, run `chmod 0600 \"{p}\"`.\n * To skip this warning, set environment variable SF_SKIP_TOKEN_FILE_PERMISSIONS_VERIFICATION=true.\n"
            ));
        }
    }
    out
}

pub fn permissions_are_strict(p: &Path) -> bool {
    mode_of(p).is_none_or(|m| m & 0o077 == 0)
}

fn skip_permission_warning() -> bool {
    for var in [
        "SF_SKIP_TOKEN_FILE_PERMISSIONS_VERIFICATION",
        "SKIP_TOKEN_FILE_PERMISSIONS_VERIFICATION",
    ] {
        if let Ok(v) = std::env::var(var) {
            return try_cast_to_bool(&Cv::Str(v)).unwrap_or(false);
        }
    }
    false
}

fn too_wide(path: &Path) -> SnowError {
    SnowError::Cli {
        message: format!(
            "Configuration file {} has too wide permissions. Run the command below exactly as printed.",
            path.display()
        ),
        class: crate::errors::ErrorClass::Generic,
        after: vec![format!("chmod 0600 \"{}\"", path.display())],
        name: "ConfigFileTooWidePermissionsError",
    }
}

/// Unix permission policy for config files in SNOWFLAKE_HOME.
fn apply_permission_policy(path: &Path, warnings: &mut Vec<String>) -> Result<()> {
    let Some(mode) = mode_of(path) else {
        return Ok(());
    };
    if mode & 0o022 != 0 {
        return Err(too_wide(path));
    }
    if mode & 0o044 != 0 {
        if skip_permission_warning() {
            warnings.push(format!("UserWarning: {}", unix_permission_warning(path)));
        } else {
            return Err(too_wide(path));
        }
    }
    Ok(())
}

pub fn unix_permission_warning(path: &Path) -> String {
    format!(
        "Bad owner or permissions on {p}.\n * To change owner, run `chown $USER \"{p}\"`.\n * To restrict permissions, run `chmod 0600 \"{p}\"`.\n * In future versions of Snowflake CLI strict configuration file permissions will be mandatory. To test if your files have correct permissions set SNOWFLAKE_CLI_FEATURES_ENFORCE_STRICT_CONFIG_PERMISSIONS=1 and try again.",
        p = path.display()
    )
}

fn enforce_strict() -> bool {
    std::env::var("SNOWFLAKE_CLI_FEATURES_ENFORCE_STRICT_CONFIG_PERMISSIONS")
        .ok()
        .and_then(|v| try_cast_to_bool(&Cv::Str(v.to_lowercase())))
        .unwrap_or(false)
}

fn parse_doc(path: &Path) -> Result<DocumentMut> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        SnowError::click(format!(
            "Configuration file seems to be corrupted. {}",
            crate::errors::python_os_error(&e, Some(&path.display().to_string()))
        ))
    })?;
    text.parse::<DocumentMut>().map_err(|e| {
        SnowError::click(format!(
            "Configuration file seems to be corrupted. {}",
            toml_error_message(&text, &e)
        ))
    })
}

/// tomlkit's parse error text, approximated (`Unexpected character: 'x' at line 1 col 5`).
fn toml_error_message(text: &str, e: &toml_edit::TomlError) -> String {
    // tomlkit: `Key "k" already exists.` (with the position for a repeated table).
    if let Some(rest) = e.message().split("duplicate key `").nth(1)
        && let Some(key) = rest.split('`').next()
    {
        let table = e.message().starts_with("invalid table header")
            || e.span().is_some_and(|s| {
                let line_start = text[..s.start.min(text.len())]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                text[line_start..].trim_start().starts_with('[')
            });
        if table {
            let line = text.matches('\n').count();
            return format!("Key \"{key}\" already exists. at line {line} col 0");
        }
        return format!("Key \"{key}\" already exists.");
    }
    match e.span() {
        Some(span) => {
            let before = &text[..span.start.min(text.len())];
            let line = before.matches('\n').count() + 1;
            let col = before
                .rsplit('\n')
                .next()
                .map(|l| l.chars().count())
                .unwrap_or(0)
                + 1;
            let ch = text[span.start.min(text.len())..].chars().next();
            match ch {
                Some(c) => format!(
                    "Unexpected character: {} at line {line} col {col}",
                    snow_click::py_repr(&c.to_string())
                ),
                None => format!("Unexpected end of file at line {line} col {col}"),
            }
        }
        None => e.message().to_string(),
    }
}

impl Config {
    /// `config_init`: check permissions, create a missing file, read both files.
    pub fn init(config_file: Option<PathBuf>, warnings: &mut Vec<String>) -> Result<Config> {
        let home = snowflake_home_dir();
        let connections_file = home.join("connections.toml");
        let default_config = home.join("config.toml");
        let is_override = config_file.is_some();
        let file_path = config_file.unwrap_or_else(|| default_config.clone());
        if is_override {
            if enforce_strict() && file_path.exists() && !permissions_are_strict(&file_path) {
                apply_permission_policy(&file_path, warnings)?;
            }
        } else {
            for f in [&connections_file, &default_config] {
                if f.exists() && !permissions_are_strict(f) {
                    apply_permission_policy(f, warnings)?;
                }
            }
        }
        let mut cfg = Config {
            file_path: file_path.clone(),
            connections_file,
            is_override,
            doc: DocumentMut::new(),
            connections_doc: None,
        };
        if !file_path.exists() {
            cfg.initialise()?;
        }
        cfg.read(warnings)?;
        Ok(cfg)
    }

    /// An empty configuration (used before `config_init` and in tests).
    pub fn empty() -> Config {
        let home = snowflake_home_dir();
        Config {
            file_path: home.join("config.toml"),
            connections_file: home.join("connections.toml"),
            is_override: false,
            doc: DocumentMut::new(),
            connections_doc: None,
        }
    }

    fn initialise(&mut self) -> Result<()> {
        if let Some(parent) = self.file_path.parent() {
            crate::secure::mkdir_p(parent)?;
        }
        crate::secure::touch(&self.file_path)?;
        let logs_path = self
            .file_path
            .parent()
            .map(|p| p.join("logs"))
            .unwrap_or_default();
        let mut doc = DocumentMut::new();
        let mut cli = Table::new();
        cli.insert("ignore_new_version_warning", toml_edit::value(false));
        cli.insert("air_gapped", toml_edit::value(false));
        cli.insert("auto_upgrade", toml_edit::value(false));
        cli.insert("auto_upgrade_allow_major", toml_edit::value(false));
        let mut logs = Table::new();
        logs.insert("save_logs", toml_edit::value(true));
        logs.insert("path", toml_edit::value(logs_path.display().to_string()));
        logs.insert("level", toml_edit::value("info"));
        cli.insert("logs", Item::Table(logs));
        doc.insert("cli", Item::Table(cli));
        std::fs::write(&self.file_path, crate::tomlfmt::dumps(&doc))?;
        Ok(())
    }

    fn read(&mut self, warnings: &mut Vec<String>) -> Result<()> {
        let check = !self.is_override && enforce_strict();
        for f in [&self.file_path, &self.connections_file] {
            if check
                && f.exists()
                && let Some(m) = mode_of(f)
            {
                if m & 0o022 != 0 {
                    return Err(SnowError::click(format!(
                        "Configuration file seems to be corrupted. file '{}' is writable by group or others — this poses a security risk because it allows unauthorized users to modify sensitive settings. Your Permission: {:o}",
                        f.display(),
                        m & 0o777
                    )));
                }
                let _ = warnings;
            }
        }
        self.doc = if self.file_path.exists() {
            parse_doc(&self.file_path)?
        } else {
            DocumentMut::new()
        };
        self.connections_doc = if self.connections_file.exists() {
            Some(parse_doc(&self.connections_file)?)
        } else {
            None
        };
        Ok(())
    }

    /// The merged document as plain values (connections.toml replaces `[connections]`).
    fn root_get(&self, key: &str) -> Option<Cv> {
        if key == CONNECTIONS_SECTION
            && let Some(c) = &self.connections_doc
        {
            return item_to_cv(c.as_item());
        }
        self.doc.get(key).and_then(item_to_cv)
    }

    /// `_find_section(*path)`; `None` when missing.
    pub fn find(&self, path: &[&str]) -> Option<Cv> {
        let (first, rest) = path.split_first()?;
        let mut cur = self.root_get(first)?;
        for p in rest {
            cur = match cur {
                Cv::Table(mut t) => t.shift_remove(*p)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    pub fn section_exists(&self, path: &[&str]) -> bool {
        self.find(path).is_some()
    }

    /// `get_config_section`: a table merged with `SNOWFLAKE_<PATH>_*` env vars.
    pub fn section(&self, path: &[&str]) -> Option<IndexMap<String, Cv>> {
        let sec = self.find(path)?;
        let Cv::Table(mut t) = sec else { return None };
        let prefix = format!(
            "SNOWFLAKE_{}_",
            path.iter()
                .map(|p| p.to_uppercase())
                .collect::<Vec<_>>()
                .join("_")
        );
        let mut envs: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| (k.replacen(&prefix, "", 1).to_lowercase(), v))
            .collect();
        envs.sort();
        for (k, v) in envs {
            t.insert(k, Cv::Str(v));
        }
        Some(t)
    }

    /// `get_config_value(*path, key=key, default=None)`.
    pub fn value(&self, path: &[&str], key: &str) -> Option<Cv> {
        if let Some(v) = env_value(path, key).filter(|v| !v.is_empty()) {
            return Some(Cv::Str(v));
        }
        if path.is_empty() {
            return self.root_get(key);
        }
        self.section(path)?.shift_remove(key)
    }

    /// `get_config_value` without environment overrides.
    pub fn value_without_env(&self, path: &[&str], key: &str) -> Option<Cv> {
        let mut full: Vec<&str> = path.to_vec();
        full.push(key);
        self.find(&full)
    }

    /// `get_config_bool_value`.
    pub fn bool_value(
        &self,
        path: &[&str],
        key: &str,
        default: Option<bool>,
    ) -> Result<Option<bool>> {
        match self.value(path, key) {
            None => Ok(default),
            Some(v) => try_cast_to_bool(&v).map(Some).ok_or_else(|| {
                let mut p: Vec<&str> = path.to_vec();
                p.push(key);
                SnowError::click(format!(
                    "Expected boolean value for {} option.",
                    p.join(".")
                ))
            }),
        }
    }

    /// Feature flag value: `[cli.features] <name>` or `SNOWFLAKE_CLI_FEATURES_<NAME>`.
    pub fn feature(&self, name: &str, default: Option<bool>) -> Option<bool> {
        self.bool_value(&["cli", "features"], &name.to_lowercase(), default)
            .unwrap_or(default)
    }

    pub fn default_connection_name(&self) -> String {
        if let Ok(v) = std::env::var("SNOWFLAKE_DEFAULT_CONNECTION_NAME")
            && !v.is_empty()
        {
            return v;
        }
        match self.doc.get("default_connection_name").and_then(item_to_cv) {
            Some(Cv::Str(s)) => s,
            Some(other) => other.py_str(),
            None => "default".into(),
        }
    }

    /// Connection names and tables, in file order.
    pub fn connections(&self) -> IndexMap<String, IndexMap<String, Cv>> {
        let mut out = IndexMap::new();
        if let Some(Cv::Table(t)) = self.root_get(CONNECTIONS_SECTION) {
            for (name, v) in t {
                if let Cv::Table(mut tab) = v {
                    let prefix = format!("SNOWFLAKE_CONNECTIONS_{}_", name.to_uppercase());
                    let mut envs: Vec<(String, String)> = std::env::vars()
                        .filter(|(k, _)| k.starts_with(&prefix))
                        .map(|(k, v)| (k.replacen(&prefix, "", 1).to_lowercase(), v))
                        .collect();
                    envs.sort();
                    for (k, v) in envs {
                        tab.insert(k, Cv::Str(v));
                    }
                    out.insert(name, tab);
                }
            }
        }
        out
    }

    pub fn connection_exists(&self, name: &str) -> bool {
        self.section_exists(&[CONNECTIONS_SECTION, name])
    }

    /// `get_connection_dict`: aliases normalised, empty values dropped.
    pub fn connection_dict(&self, name: &str) -> Result<IndexMap<String, Cv>> {
        let Some(raw) = self.connections().shift_remove(name) else {
            if !self.connection_exists(name) {
                return Err(
                    SnowError::cli(format!("Connection {name} is not configured"))
                        .named("MissingConfigurationError"),
                );
            }
            return Ok(IndexMap::new());
        };
        Ok(normalize_connection(raw))
    }

    // ------------------------------------------------------------------
    // Writing
    // ------------------------------------------------------------------

    fn write_config(&self) -> Result<()> {
        crate::secure::write_private(&self.file_path, &crate::tomlfmt::dumps(&self.doc))
    }

    fn write_connections(&self) -> Result<()> {
        if let Some(c) = &self.connections_doc {
            std::fs::write(&self.connections_file, crate::tomlfmt::dumps(c))?;
        }
        Ok(())
    }

    /// `set_config_value(path, value)`.
    pub fn set_value(&mut self, path: &[&str], value: Item) -> Result<()> {
        if path.first() == Some(&CONNECTIONS_SECTION)
            && let Some(doc) = self.connections_doc.as_mut()
        {
            set_in(doc.as_table_mut(), &path[1..], value);
            self.write_connections()?;
        } else {
            set_in(self.doc.as_table_mut(), path, value);
            self.write_config()?;
        }
        Ok(())
    }

    /// `unset_config_value(path)`.
    pub fn unset_value(&mut self, path: &[&str]) -> Result<()> {
        if path.first() == Some(&CONNECTIONS_SECTION)
            && let Some(doc) = self.connections_doc.as_mut()
        {
            remove_in(doc.as_table_mut(), &path[1..]);
            self.write_connections()?;
        } else {
            remove_in(self.doc.as_table_mut(), path);
            self.write_config()?;
        }
        Ok(())
    }

    /// `add_connection_to_proper_file`; returns the file written.
    pub fn add_connection(&mut self, name: &str, values: Table) -> Result<PathBuf> {
        if let Some(doc) = self.connections_doc.as_mut() {
            doc.as_table_mut().insert(name, Item::Table(values));
            self.write_connections()?;
            Ok(self.connections_file.clone())
        } else {
            set_in(
                self.doc.as_table_mut(),
                &[CONNECTIONS_SECTION, name],
                Item::Table(values),
            );
            self.write_config()?;
            Ok(self.file_path.clone())
        }
    }

    /// `remove_connection_from_proper_file`.
    pub fn remove_connection(&mut self, name: &str) -> Result<PathBuf> {
        if let Some(doc) = self.connections_doc.as_mut() {
            if !doc.as_table().contains_key(name) {
                return Err(
                    SnowError::cli(format!("Connection {name} is not configured"))
                        .named("MissingConfigurationError"),
                );
            }
            doc.as_table_mut().remove(name);
            self.write_connections()?;
            Ok(self.connections_file.clone())
        } else {
            remove_in(self.doc.as_table_mut(), &[CONNECTIONS_SECTION, name]);
            self.write_config()?;
            Ok(self.file_path.clone())
        }
    }

    pub fn doc(&self) -> &DocumentMut {
        &self.doc
    }
    pub fn has_connections_file(&self) -> bool {
        self.connections_doc.is_some()
    }
}

/// Insert like tomlkit: new tables get a blank line before them, a new
/// top-level value goes above the first table followed by a blank line.
fn set_in(table: &mut Table, path: &[&str], value: Item) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let root_has_content = !table.is_empty();
    let mut cur = table;
    for p in parents {
        if !cur.contains_key(p) || !cur[*p].is_table_like() {
            let mut t = Table::new();
            t.set_implicit(true);
            cur.insert(p, Item::Table(t));
        }
        cur = cur[*p].as_table_mut().expect("table");
    }
    let mut value = value;
    let is_new = !cur.contains_key(last);
    if let Item::Table(t) = &mut value
        && is_new
        && (root_has_content || !parents.is_empty())
    {
        t.decor_mut().set_prefix("\n");
    }
    let inserting_root_value = parents.is_empty() && is_new && !value.is_table_like();
    cur.insert(last, value);
    if inserting_root_value {
        // tomlkit separates root values from the first table by a blank line.
        let has_values_before = cur.iter().filter(|(_, v)| !v.is_table_like()).count() > 1;
        if !has_values_before {
            let mut pos: Vec<(usize, String)> = cur
                .iter()
                .filter_map(|(k, v)| {
                    v.as_table()
                        .and_then(|t| t.position())
                        .map(|p| (p, k.to_string()))
                })
                .collect();
            pos.sort();
            if let Some((_, first)) = pos.first().cloned()
                && let Some(t) = cur.get_mut(&first).and_then(Item::as_table_mut)
            {
                if t.is_implicit() {
                    // The first visible child table carries the decor.
                    let mut child: Vec<(usize, String)> = t
                        .iter()
                        .filter_map(|(k, v)| {
                            v.as_table()
                                .and_then(|t| t.position())
                                .map(|p| (p, k.to_string()))
                        })
                        .collect();
                    child.sort();
                    if let Some((_, c)) = child.first().cloned()
                        && let Some(ct) = t.get_mut(&c).and_then(Item::as_table_mut)
                    {
                        let prefix = ct
                            .decor()
                            .prefix()
                            .and_then(|p| p.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !prefix.starts_with('\n') {
                            ct.decor_mut().set_prefix(format!("\n{prefix}"));
                        }
                    }
                } else {
                    let prefix = t
                        .decor()
                        .prefix()
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !prefix.starts_with('\n') {
                        t.decor_mut().set_prefix(format!("\n{prefix}"));
                    }
                }
            }
        }
    }
}

fn remove_in(table: &mut Table, path: &[&str]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cur = table;
    for p in parents {
        match cur.get_mut(p).and_then(Item::as_table_mut) {
            Some(t) => cur = t,
            None => return,
        }
    }
    cur.remove(last);
}

/// `ConnectionConfig.from_dict(...).to_dict_of_all_non_empty_values()`.
pub fn normalize_connection(raw: IndexMap<String, Cv>) -> IndexMap<String, Cv> {
    const KNOWN: &[&str] = &[
        "account",
        "user",
        "password",
        "host",
        "region",
        "port",
        "protocol",
        "database",
        "schema",
        "warehouse",
        "role",
        "authenticator",
        "workload_identity_provider",
        "private_key_file",
        "private_key_raw",
        "private_key_passphrase",
        "token",
        "session_token",
        "master_token",
        "token_file_path",
        "oauth_client_id",
        "oauth_client_secret",
        "oauth_authorization_url",
        "oauth_token_request_url",
        "oauth_redirect_uri",
        "oauth_scope",
        "oauth_enable_pkce",
        "oauth_enable_refresh_tokens",
        "oauth_enable_single_use_refresh_tokens",
        "client_store_temporary_credential",
        "secondary_roles",
        "server_session_keep_alive",
    ];
    let mut known: IndexMap<String, Cv> = IndexMap::new();
    let mut other: IndexMap<String, Cv> = IndexMap::new();
    for (k, v) in raw {
        let nk = match k.as_str() {
            "oatuh_enable_pkce" => "oauth_enable_pkce".to_string(),
            "private_key_file_pwd" => "private_key_passphrase".to_string(),
            _ => k.clone(),
        };
        if KNOWN.contains(&nk.as_str()) {
            known.insert(nk, v);
        } else {
            other.insert(k, v);
        }
    }
    // Known fields come out in dataclass declaration order.
    let mut out = IndexMap::new();
    for k in KNOWN {
        if let Some(v) = known.shift_remove(*k) {
            out.insert((*k).to_string(), v);
        }
    }
    out.extend(other);
    out
}
