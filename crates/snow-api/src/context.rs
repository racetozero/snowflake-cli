//! The CLI global context (`snowflake.cli.api.cli_global_context`) and the
//! connection it lazily opens.

use crate::config::{Config, Cv};
use crate::console::Console;
use crate::errors::{Result, SnowError};
use crate::output::Format;
use indexmap::IndexMap;
use snow_click::{Invocation, Spec, Value};
use snow_connector::{ConnectParams, Connection};

/// `ConnectionContext`: command-line connection overrides.
#[derive(Debug, Clone, Default)]
pub struct ConnectionContext {
    pub connection_name: Option<String>,
    pub temporary_connection: bool,
    pub mfa_passcode: Option<String>,
    pub enable_diag: bool,
    pub diag_log_path: Option<String>,
    pub diag_allowlist_path: Option<String>,
    /// Every other field by name (host, port, account, user, ...).
    pub fields: IndexMap<String, Cv>,
}

impl ConnectionContext {
    pub fn set(&mut self, name: &str, v: Option<Cv>) {
        match v {
            Some(v) => {
                self.fields.insert(name.to_string(), v);
            }
            None => {
                self.fields.shift_remove(name);
            }
        }
    }
    pub fn get_str(&self, name: &str) -> Option<String> {
        self.fields.get(name).map(Cv::py_str)
    }
}

pub struct Cli {
    pub spec: &'static Spec,
    pub inv: Invocation,
    pub config: Config,
    pub format: Format,
    pub silent: bool,
    pub verbose: bool,
    pub debug: bool,
    pub enhanced_exit_codes: bool,
    pub decimal_precision: Option<i64>,
    pub conn: ConnectionContext,
    pub project_path: Option<String>,
    pub env_overrides: Vec<String>,
    pub console: Console,
    /// `--agent` / `SNOW_AGENT=1`.
    pub agent: bool,
    /// Python `warnings.warn` messages to print on stderr.
    pub warnings: Vec<String>,
    connection: Option<Connection>,
    connection_failure: Option<SnowError>,
    /// The time the command started (for telemetry and metrics).
    pub started: std::time::Instant,
    /// The connector logs at INFO or DEBUG (error messages then include the query id).
    pub connector_info: bool,
    /// SnowSQL `quiet`: query results are not written to the terminal.
    pub quiet: bool,
    /// The open `!spool` / `output_file` (results are appended to it).
    pub spool: Option<std::fs::File>,
    /// REPL `!set timing` (default on), `timing_in_output_file` and `echo`.
    pub timing: bool,
    pub timing_in_output_file: bool,
    pub echo: bool,
    /// The loaded project definition (`cli_context.project_definition`), once read.
    project: Option<Option<crate::project::Project>>,
}

struct ProjectHost {
    root: std::path::PathBuf,
}

impl crate::project::render::Host for ProjectHost {
    fn username(&self) -> Option<String> {
        ["USER", "USERNAME", "LOGNAME"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
    }
    fn read_file(&self, filter: &str, name: &str) -> std::result::Result<String, String> {
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let cand = crate::config::expand_user(name);
        let target = if cand.is_absolute() {
            cand
        } else {
            root.join(cand)
        };
        let target = target.canonicalize().unwrap_or(target);
        if !target.starts_with(&root) {
            return Err(format!(
                "{filter}: path '{name}' is outside the project root."
            ));
        }
        std::fs::read_to_string(&target)
            .map_err(|e| crate::errors::python_os_error(&e, Some(&target.display().to_string())))
    }
}

impl Cli {
    pub fn new(spec: &'static Spec, inv: Invocation, agent: bool) -> Cli {
        Cli {
            spec,
            inv,
            config: Config::empty(),
            format: Format::Table,
            silent: false,
            verbose: false,
            debug: false,
            enhanced_exit_codes: false,
            decimal_precision: None,
            conn: ConnectionContext::default(),
            project_path: None,
            env_overrides: Vec::new(),
            console: Console::default(),
            agent,
            warnings: Vec::new(),
            connection: None,
            connection_failure: None,
            started: std::time::Instant::now(),
            connector_info: false,
            quiet: false,
            spool: None,
            timing: true,
            timing_in_output_file: false,
            echo: false,
            project: None,
        }
    }

    /// The project root (`--project` or the working directory), absolute.
    pub fn project_root(&self) -> std::path::PathBuf {
        let cwd = std::env::current_dir().unwrap_or_default();
        match &self.project_path {
            Some(p) => {
                let p = std::path::Path::new(p);
                let abs = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    cwd.join(p)
                };
                crate::project::model::pure_path(&abs.to_string_lossy()).into()
            }
            None => cwd,
        }
    }

    /// `--env key=value` overrides.
    pub fn env_override_map(&self) -> IndexMap<String, String> {
        self.env_overrides
            .iter()
            .filter_map(|p| {
                p.split_once('=')
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            })
            .collect()
    }

    /// `cli_context.project_definition` (loaded on first use). `None` only when
    /// `optional` and there is no snowflake.yml.
    pub fn project(&mut self, optional: bool) -> Result<Option<&crate::project::Project>> {
        if self.project.is_none() {
            let root = self.project_root();
            let files = crate::project::definition::definition_files(&root);
            if files.is_empty() && !optional {
                return Err(SnowError::cli(
                    "Cannot find project definition (snowflake.yml). Please provide a path to the project or run this command in a valid project directory.",
                )
                .named("MissingConfigurationError"));
            }
            let features: Vec<String> =
                ["ENABLE_SNOWPARK_GLOB_SUPPORT", "ENABLE_NATIVE_APP_CHILDREN"]
                    .iter()
                    .filter(|f| self.config.feature(f, Some(false)) == Some(true))
                    .map(|f| f.to_string())
                    .collect();
            let host = ProjectHost { root: root.clone() };
            let overrides = self.env_override_map();
            let mut warns = Vec::new();
            let p =
                crate::project::definition::load(&root, &overrides, &features, &host, &mut |w| {
                    warns.push(w.to_string())
                });
            for w in warns {
                self.console.warning(&w);
            }
            self.project = Some(p?);
        }
        let p = self.project.as_ref().and_then(|p| p.as_ref());
        if p.is_none() && !optional {
            return Err(SnowError::cli(
                "Cannot find project definition (snowflake.yml). Please provide a path to the project or run this command in a valid project directory.",
            )
            .named("MissingConfigurationError"));
        }
        Ok(p)
    }

    /// `convert_project_definition_to_v2(..., in_memory=True)` for a v1 project:
    /// the v2 definition replaces the project's model, while `ctx` (the template
    /// context) keeps describing the v1 definition. v2 projects are returned as is.
    pub fn project_v2_in_memory(&mut self) -> Result<crate::project::Project> {
        let p = self.project(false)?.expect("required").clone();
        if p.meets_version("2") {
            return Ok(p);
        }
        let mut warns = Vec::new();
        let data = crate::project::conversion::to_v2_data(&p.definition, false, None, &mut |w| {
            warns.push(w.to_string())
        })?;
        let features: Vec<String> = ["ENABLE_SNOWPARK_GLOB_SUPPORT", "ENABLE_NATIVE_APP_CHILDREN"]
            .iter()
            .filter(|f| self.config.feature(f, Some(false)) == Some(true))
            .map(|f| f.to_string())
            .collect();
        let host = ProjectHost {
            root: p.root.clone(),
        };
        let (definition, _) = crate::project::definition::render_definition(
            data,
            &IndexMap::new(),
            &features,
            &host,
            &mut |w| warns.push(w.to_string()),
        )?;
        for w in warns {
            self.console.warning(&w);
        }
        Ok(crate::project::Project { definition, ..p })
    }

    /// The value of a parameter of the leaf command (or an ancestor).
    pub fn param(&self, name: &str) -> &Value {
        for level in self.inv.levels.iter().rev() {
            if let Some(v) = level.get(self.spec, name) {
                return v;
            }
        }
        &Value::None
    }
    pub fn str(&self, name: &str) -> Option<String> {
        match self.param(name) {
            Value::Str(s) => Some(s.clone()),
            Value::Int(i) => Some(i.to_string()),
            Value::None => None,
            Value::Bool(b) => Some(if *b { "True" } else { "False" }.into()),
            other => Some(format!("{other:?}")),
        }
    }
    pub fn flag(&self, name: &str) -> bool {
        self.param(name).as_bool()
    }
    pub fn opt_bool(&self, name: &str) -> Option<bool> {
        match self.param(name) {
            Value::Bool(b) => Some(*b),
            Value::None => None,
            other => Some(other.as_bool()),
        }
    }
    pub fn int(&self, name: &str) -> Option<i64> {
        self.param(name).as_int()
    }
    pub fn list(&self, name: &str) -> Vec<String> {
        self.param(name).strings()
    }
    /// Whether the parameter came from the command line.
    pub fn given(&self, name: &str) -> bool {
        for level in self.inv.levels.iter().rev() {
            if let Some(s) = level.source(self.spec, name) {
                return s == snow_click::Source::CommandLine;
            }
        }
        false
    }
    /// Click's command path without `snow` (`stage list-files`,
    /// `dbt execute NAME run`).
    pub fn command_name(&self) -> String {
        if self.inv.levels.is_empty() {
            return String::new();
        }
        let full = self.inv.command_path_to(self.inv.levels.len() - 1);
        full.split_once(' ')
            .map(|(_, r)| r.to_string())
            .unwrap_or_default()
    }

    pub fn feature(&self, name: &str, default: Option<bool>) -> bool {
        self.config.feature(name, default) == Some(true)
    }

    /// Resolved connection parameters (`resolve_connection_parameters`).
    pub fn connection_parameters(&mut self) -> Result<IndexMap<String, Cv>> {
        if !self.conn.temporary_connection && self.conn.connection_name.is_none() {
            self.conn.connection_name = Some(self.config.default_connection_name());
        }
        let name = self.conn.connection_name.clone();
        if self.conn.temporary_connection && name.is_some() {
            return Err(SnowError::click(
                "Can't use connection name and temporary connection.",
            ));
        }
        let session = self.conn.fields.get("session_token").is_some();
        let master = self.conn.fields.get("master_token").is_some();
        if !self.conn.temporary_connection && (session || master) {
            return Err(SnowError::click(
                "When using a session or master token, you must use a temporary connection",
            ));
        }
        if session && !master {
            return Err(SnowError::click(
                "When using a session token, you must provide the corresponding master token",
            ));
        }
        if master && !session {
            return Err(SnowError::click(
                "When using a master token, you must provide the corresponding session token",
            ));
        }
        let alias = |k: &str| match k {
            "private_key_path" => "private_key_file".to_string(),
            "private_key_file_pwd" => "private_key_passphrase".to_string(),
            other => other.to_string(),
        };
        let mut params: IndexMap<String, Cv> = IndexMap::new();
        if let Some(n) = &name {
            if !self.config.connection_exists(n) {
                return Err(SnowError::cli(format!("Connection {n} is not configured"))
                    .named("MissingConfigurationError"));
            }
            for (k, v) in self.config.connection_dict(n)? {
                params.insert(alias(&k), v);
            }
        }
        for (k, v) in &self.conn.fields {
            params.insert(alias(k), v.clone());
        }
        const ENV: &[&str] = &[
            "account",
            "user",
            "password",
            "host",
            "port",
            "protocol",
            "authenticator",
            "workload_identity_provider",
            "private_key_file",
            "private_key_path",
            "private_key_raw",
            "database",
            "schema",
            "role",
            "warehouse",
            "session_token",
            "master_token",
            "token",
            "token_file_path",
            "oauth_client_id",
            "oauth_client_secret",
            "oauth_authorization_url",
            "oauth_token_request_url",
            "oauth_redirect_uri",
            "oauth_scope",
            "oauth_disable_pkce",
            "oauth_enable_refresh_tokens",
            "oauth_enable_single_use_refresh_tokens",
            "client_store_temporary_credential",
            "secondary_roles",
            "server_session_keep_alive",
        ];
        for key in ENV {
            let k = alias(key);
            if !params.contains_key(&k)
                && let Some(v) = crate::config::env_value(&[], key).filter(|v| !v.is_empty())
            {
                params.insert(k, Cv::Str(v));
            }
        }
        Ok(params)
    }

    /// The connection parameters without changing the context (telemetry uses
    /// this to decide whether a login could prompt).
    pub fn connection_parameters_preview(&mut self) -> Result<IndexMap<String, Cv>> {
        let saved = self.conn.connection_name.clone();
        let r = self.connection_parameters();
        self.conn.connection_name = saved;
        r
    }

    /// The open connection, created on first use.
    pub fn connection(&mut self) -> Result<&mut Connection> {
        if self.connection.is_none() {
            if let Some(f) = &self.connection_failure {
                return Err(f.clone());
            }
            match self.open_connection() {
                Ok(c) => self.connection = Some(c),
                Err(e) => {
                    self.connection_failure = Some(e.clone());
                    return Err(e);
                }
            }
        }
        Ok(self.connection.as_mut().expect("just set"))
    }

    pub fn connection_if_open(&mut self) -> Option<&mut Connection> {
        self.connection.as_mut()
    }

    pub fn close_connection(&mut self) {
        if let Some(mut c) = self.connection.take() {
            c.close();
        }
    }

    fn open_connection(&mut self) -> Result<Connection> {
        let params = self.connection_parameters()?;
        let p = crate::connect::build_params(self, params)?;
        crate::connect::connect(self, p)
    }

    /// Replace the connection (e.g. after `snow sql` switches autocommit).
    pub fn set_connection(&mut self, c: Connection) {
        self.connection = Some(c);
    }

    pub fn take_connection(&mut self) -> Option<Connection> {
        self.connection.take()
    }

    pub fn connect_params(&mut self) -> Result<ConnectParams> {
        let params = self.connection_parameters()?;
        crate::connect::build_params(self, params)
    }
}
