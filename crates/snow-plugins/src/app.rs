//! The `snow` entry point: parse, run the command, print the result or the
//! error, and return the exit code. Agent mode wraps everything in one JSON
//! envelope on stdout.

use crate::registry;
use serde_json::{Value as J, json};
use snow_api::config::Config;
use snow_api::output::{Format, Output, Sink, print_result};
use snow_api::{Cli, SnowError};
use snow_click::help::{HelpEnv, print_to_string, render_error, render_help, shared_panels};
use snow_click::spec::Param;
use snow_click::{ClickError, Hooks, Invocation, Processor, Spec, Value};
use std::io::{IsTerminal, Write};
use std::sync::LazyLock;

pub static SPEC: LazyLock<Spec> = LazyLock::new(|| Spec {
    commands: snow_spec::COMMANDS,
    params: snow_spec::PARAMS,
    md: snow_spec::MD,
    shared_connection: snow_spec::SHARED_CONNECTION_PARAMS,
    shared_global: snow_spec::SHARED_GLOBAL_PARAMS,
});

/// State shared by the parser hooks and the command.
pub struct App {
    pub cli: Cli,
    /// Root `--help` / `--structure` are deferred until all root options ran.
    deferred_root: Option<&'static str>,
    config_ready: bool,
}

impl App {
    fn ensure_config(&mut self, file: Option<String>) -> Result<(), ClickError> {
        if self.config_ready {
            return Ok(());
        }
        let mut warnings = Vec::new();
        match Config::init(file.map(Into::into), &mut warnings) {
            Ok(c) => {
                self.cli.config = c;
                self.cli.warnings.extend(warnings);
                self.config_ready = true;
                crate::logs::init(&mut self.cli, false, false);
                self.cli.connector_info = false;
                crate::helpers::startup_encoding_check(&mut self.cli).map_err(to_click)
            }
            Err(e) => Err(to_click(e)),
        }
    }

    fn help_env(&self) -> HelpEnv {
        HelpEnv {
            condensed: self
                .cli
                .config
                .feature("ENABLE_CONDENSED_COMMAND_HELP", Some(true))
                == Some(true),
            docs_in_help: self
                .cli
                .config
                .feature("ENABLE_COMMAND_DOCS_IN_HELP", Some(true))
                == Some(true),
        }
    }
}

/// Carry a SnowError through the parser as a ClickError.
fn to_click(e: SnowError) -> ClickError {
    match e {
        SnowError::Click(c) => c,
        other => {
            let code = other.exit_code(false);
            PENDING.with(|p| *p.borrow_mut() = Some(other.clone()));
            ClickError::Exception {
                message: other.message(),
                code,
            }
        }
    }
}

thread_local! {
    static PENDING: std::cell::RefCell<Option<SnowError>> = const { std::cell::RefCell::new(None) };
}

impl Hooks for App {
    fn feature_enabled(&self, flag: &str) -> bool {
        let default = match flag {
            "ENABLE_COMMAND_DOCS_IN_HELP"
            | "ENABLE_CONDENSED_COMMAND_HELP"
            | "ENABLE_DBT_GIT_METADATA" => Some(true),
            "ENABLE_RELEASE_CHANNELS" => None,
            _ => Some(false),
        };
        self.cli.config.feature(flag, default) == Some(true)
    }

    fn convert_custom(
        &mut self,
        _spec: &Spec,
        _inv: &Invocation,
        param: &'static Param,
        ty: &'static str,
        raw: &str,
    ) -> Result<Value, ClickError> {
        crate::types::convert(&mut self.cli, param, ty, raw).map_err(to_click)
    }

    fn callback(
        &mut self,
        spec: &Spec,
        inv: &mut Invocation,
        param: &'static Param,
        value: Value,
    ) -> Result<Value, ClickError> {
        self.cli.inv = inv.clone();
        let depth = inv.levels.len() - 1;
        if depth == 0 {
            return self.root_callback(spec, inv, param, value);
        }
        if !self.config_ready {
            self.ensure_config(None)?;
        }
        crate::callbacks::run(&mut self.cli, param, value).map_err(to_click)
    }

    fn prompt(&mut self, param: &'static Param) -> Result<Value, ClickError> {
        let _ = param;
        if self.cli.agent {
            return Err(ClickError::exception(
                "MFA passcode is required; pass --mfa-passcode VALUE",
            ));
        }
        eprint!("MFA passcode: ");
        let _ = std::io::stderr().flush();
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        Ok(Value::Str(s.trim_end_matches(['\n', '\r']).to_string()))
    }

    fn list_commands(&self, spec: &Spec, cmd: u16) -> Option<Vec<u16>> {
        let c = spec.cmd(cmd);
        if c.smart_app_group {
            return crate::nativeapp::visible_app_commands(spec, cmd);
        }
        None
    }

    fn print_help(
        &mut self,
        spec: &Spec,
        inv: &Invocation,
        depth: usize,
    ) -> Result<(), ClickError> {
        if !self.config_ready {
            self.ensure_config(None)?;
        }
        let argv = &inv.argv;
        let has_help = argv.iter().any(|a| a == "--help" || a == "-h");
        if has_help && argv.iter().any(|a| a == "--help-all") {
            return Err(to_click(SnowError::argument(
                "Cannot use --help with --help-all. Use one or the other.",
            )));
        }
        let env = self.help_env();
        let mut rs = render_help(spec, self, &env, inv, depth);
        if depth == 0 {
            rs.extend(shared_panels(spec, &env));
        }
        emit_text(&self.cli, &print_to_string(&rs, false));
        Ok(())
    }
}

impl App {
    fn root_callback(
        &mut self,
        spec: &Spec,
        inv: &mut Invocation,
        param: &'static Param,
        value: Value,
    ) -> Result<Value, ClickError> {
        match param.name {
            "configuration_file" => {
                self.ensure_config(value.as_str().map(str::to_string))?;
            }
            "custom_help" if value.as_bool() => self.deferred_root = Some("help"),
            "structure" if value.as_bool() => self.deferred_root = Some("structure"),
            "docs" if value.as_bool() => self.deferred_root = Some("docs"),
            "version" if value.as_bool() => {
                let out = Output::message(format!(
                    "Snowflake CLI version: {}",
                    snow_api::display_version()
                ));
                print_output(&mut self.cli, &out, None);
                return Err(ClickError::Exit(0));
            }
            "info" if value.as_bool() => {
                self.ensure_config(None)?;
                let out = crate::root::info(&self.cli);
                print_output(&mut self.cli, &out, Some(Format::Json));
                return Err(ClickError::Exit(0));
            }
            "install_completion" | "show_completion" if value.as_bool() => {
                self.ensure_config(None)?;
                crate::completion::run(&self.cli, param.name == "install_completion")
                    .map_err(to_click)?;
                return Err(ClickError::Exit(0));
            }
            "commands_registration" => {
                self.ensure_config(None)?;
                match self.deferred_root.take() {
                    Some("help") => {
                        self.print_help(spec, inv, 0)?;
                        return Err(ClickError::Exit(0));
                    }
                    Some("structure") => {
                        let out = crate::root::structure(spec, self);
                        print_output(&mut self.cli, &out, Some(Format::Json));
                        return Err(ClickError::Exit(0));
                    }
                    Some("docs") => {
                        crate::root::docs(&mut self.cli).map_err(to_click)?;
                        return Err(ClickError::Exit(0));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(value)
    }
}

fn emit_text(cli: &Cli, s: &str) {
    if cli.agent {
        let plain = strip_ansi(s);
        AGENT_DATA.with(|d| d.borrow_mut().push(json!({ "help": plain })));
        return;
    }
    let _ = std::io::stdout().write_all(s.as_bytes());
}

fn strip_ansi(s: &str) -> String {
    snow_api::sanitize::for_terminal(s)
}

/// Print a command result in the current (or given) format.
pub fn print_output(cli: &mut Cli, out: &Output, format: Option<Format>) {
    let fmt = format.unwrap_or(cli.format);
    if cli.agent {
        AGENT_DATA.with(|d| d.borrow_mut().push(out.to_json_value()));
        return;
    }
    let r = write_result(cli, |sink| print_result(out, fmt, sink));
    if let Err(e) = r {
        cli.console.warning(&format!(
            "\nAn unexpected exception occurred. Use --debug option to see the traceback. Exception message:\n\n{}",
            e.0
        ));
        std::process::exit(1);
    }
}

/// Write query-result text through `_result_stream()`: stdout unless
/// `quiet`, and the open spool file. Both get the non-terminal layout
/// while quiet or spooling.
pub fn write_result<T>(cli: &mut Cli, f: impl FnOnce(&mut Sink) -> T) -> T {
    let stdout = std::io::stdout();
    if !cli.quiet && cli.spool.is_none() {
        let is_tty = stdout.is_terminal();
        let mut lock = stdout.lock();
        let mut sink = Sink {
            out: &mut lock,
            is_tty,
            width: snow_click::term::rich_width(),
            color: snow_click::term::color(false),
        };
        let r = f(&mut sink);
        let _ = lock.flush();
        return r;
    }
    let mut buf: Vec<u8> = Vec::new();
    let r = {
        let mut sink = Sink {
            out: &mut buf,
            is_tty: false,
            width: snow_click::term::rich_width(),
            color: false,
        };
        f(&mut sink)
    };
    if !cli.quiet {
        let mut lock = stdout.lock();
        let _ = lock.write_all(&buf);
        let _ = lock.flush();
    }
    if let Some(file) = cli.spool.as_mut() {
        let _ = file.write_all(&buf);
        let _ = file.flush();
    }
    r
}

thread_local! {
    static AGENT_DATA: std::cell::RefCell<Vec<J>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Strip `--agent` (before `--agent-args` and `--`) and report whether it was present.
pub fn take_agent_flag(argv: &mut Vec<String>) -> bool {
    let mut agent = std::env::var("SNOW_AGENT")
        .is_ok_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"));
    let mut out = Vec::with_capacity(argv.len());
    let mut passthrough = false;
    for a in argv.drain(..) {
        if !passthrough && (a == "--agent-args" || a == "--") {
            passthrough = true;
        }
        if !passthrough && a == "--agent" {
            agent = true;
            continue;
        }
        out.push(a);
    }
    *argv = out;
    agent
}

fn agent_error_code(e: &SnowError, enhanced: bool) -> (&'static str, i32) {
    let _ = enhanced;
    match e {
        SnowError::Click(ClickError::Usage { message, .. }) => {
            if message.starts_with("No such command") {
                ("unknown", 4)
            } else {
                ("invalid", 3)
            }
        }
        SnowError::Click(_) => ("invalid", 3),
        SnowError::Cli {
            name,
            class,
            message,
            ..
        } => {
            if *name == "SnowflakeConnectionError" {
                let lower = message.to_lowercase();
                if lower.contains("password")
                    || lower.contains("authenticat")
                    || lower.contains("token")
                    || lower.contains("390")
                {
                    return ("auth", 2);
                }
                return ("api", 1);
            }
            if *name == "MissingConfigurationError" {
                return ("auth", 2);
            }
            if *name == "ConfirmationRequired" {
                return ("confirmation_required", 3);
            }
            if *name == "InputRequired" {
                return ("input_required", 3);
            }
            match class {
                snow_api::errors::ErrorClass::Sql => ("api", 1),
                snow_api::errors::ErrorClass::Argument => ("invalid", 3),
                _ => ("api", 1),
            }
        }
        SnowError::Unexpected(..) => ("internal", 5),
    }
}

/// Render an error the way Typer and Snowflake CLI do.
fn report_error(app: &mut App, inv: &Invocation, e: &SnowError, from_command: bool) -> i32 {
    let cli = &mut app.cli;
    let enhanced = cli.enhanced_exit_codes;
    if cli.agent {
        let (code, exit) = agent_error_code(e, enhanced);
        let mut err = json!({"code": code, "message": e.message()});
        if let SnowError::Cli { after, .. } = e
            && !after.is_empty()
        {
            err["remediation"] = json!(after);
        }
        let env = json!({"ok": false, "command": cli.command_name(), "error": err});
        println!("{env}");
        return exit;
    }
    let structured = cli.format.is_structured();
    match e {
        SnowError::Click(ClickError::Exit(c)) => return *c,
        // Typer's `rich_abort_error()`: a plain red "Aborted." on stderr.
        SnowError::Click(ClickError::Abort) => {
            if snow_click::term::color(true) {
                eprintln!("\x1b[31mAborted.\x1b[0m");
            } else {
                eprintln!("Aborted.");
            }
            return 1;
        }
        SnowError::Click(c) => {
            let c = &match c.clone() {
                ClickError::Usage {
                    message,
                    depth: None,
                    no_such_option,
                    attach: true,
                    class,
                } if from_command => ClickError::Usage {
                    message,
                    depth: Some(inv.levels.len().saturating_sub(1)),
                    no_such_option,
                    attach: true,
                    class,
                },
                other => other,
            };
            let s = print_to_string(&render_error(&SPEC, inv, c), true);
            let _ = std::io::stderr().write_all(s.as_bytes());
            if matches!(c, ClickError::Exception { .. }) && from_command && !structured {
                eprintln!("To send feedback about Snowflake CLI: snow feedback");
            }
            return c.exit_code();
        }
        SnowError::Cli {
            message,
            after,
            class,
            ..
        } => {
            let s = print_to_string(
                &render_error(
                    &SPEC,
                    inv,
                    &ClickError::Exception {
                        message: message.clone(),
                        code: 1,
                    },
                ),
                true,
            );
            let _ = std::io::stderr().write_all(s.as_bytes());
            for a in after {
                eprintln!("{a}");
            }
            if from_command
                && !structured
                && !matches!(
                    class,
                    snow_api::errors::ErrorClass::Click
                        | snow_api::errors::ErrorClass::ExitZeroClick
                )
            {
                eprintln!("To send feedback about Snowflake CLI: snow feedback");
            }
        }
        SnowError::Unexpected(m, _) => {
            if cli.debug {
                eprintln!("Traceback (most recent call last):\n{m}");
                return 1;
            }
            cli.console.silent = false;
            cli.console.warning(&format!(
                "\nAn unexpected exception occurred. Use --debug option to see the traceback. Exception message:\n\n{m}"
            ));
            if from_command && !structured {
                eprintln!("To send feedback about Snowflake CLI: snow feedback");
            }
        }
    }
    e.exit_code(enhanced)
}

fn flush_warnings(cli: &mut Cli) {
    for w in cli.warnings.drain(..) {
        if cli.agent {
            continue;
        }
        eprintln!("{w}");
    }
}

/// Run `snow` with `argv` (without the program name) and return the exit code.
pub fn run(mut argv: Vec<String>) -> i32 {
    let agent = take_agent_flag(&mut argv);
    if argv.first().is_some_and(|a| a == "skill") {
        return crate::skill::run(&argv[1..], agent);
    }
    if argv.first().is_some_and(|a| a == "update") {
        return crate::update::run(&argv[1..], agent);
    }
    let spec: &'static Spec = &SPEC;
    let inv0 = Invocation {
        levels: vec![],
        argv: argv.clone(),
        spec: Some(spec),
    };
    let mut app = App {
        cli: Cli::new(spec, inv0, agent),
        deferred_root: None,
        config_ready: false,
    };
    if agent {
        app.cli.console.collected = Some(Vec::new());
    }
    app.cli.console.color = snow_click::term::color(false);
    if !agent {
        for w in snow_api::config::connector_import_warnings() {
            eprintln!("{w}");
        }
    }
    // Apply --config-file before anything else, like `_maybe_init_config_from_args`.
    let cut = argv
        .iter()
        .position(|a| a == "--agent-args")
        .unwrap_or(argv.len());
    let mut early: Option<String> = None;
    for (i, a) in argv[..cut].iter().enumerate() {
        if a == "--config-file" && i + 1 < cut {
            early = Some(argv[i + 1].clone());
            break;
        }
        if let Some(v) = a.strip_prefix("--config-file=") {
            early = Some(v.to_string());
            break;
        }
    }
    if let Some(f) = early.filter(|f| std::path::Path::new(f).exists())
        && let Err(e) = app.ensure_config(Some(f))
    {
        let err = PENDING
            .with(|p| p.borrow_mut().take())
            .unwrap_or(SnowError::from(e));
        let inv = Invocation {
            levels: vec![],
            argv,
            spec: Some(spec),
        };
        return report_error(&mut app, &inv, &err, false);
    }
    let parsed = {
        let mut p = Processor {
            spec,
            hooks: &mut app,
        };
        p.run(0, argv.clone())
    };
    let inv = match parsed {
        Ok(inv) => inv,
        Err((ClickError::Exit(code), _)) => {
            flush_warnings(&mut app.cli);
            finish_agent(&app.cli, None);
            return code;
        }
        Err((e, inv)) => {
            app.cli.inv = inv.clone();
            flush_warnings(&mut app.cli);
            let err = match &e {
                ClickError::Exception { .. } => PENDING
                    .with(|p| p.borrow_mut().take())
                    .unwrap_or(SnowError::from(e)),
                _ => SnowError::Click(e),
            };
            return report_error(&mut app, &inv, &err, false);
        }
    };
    app.cli.inv = inv.clone();
    flush_warnings(&mut app.cli);
    if inv.levels.len() == 1 {
        // `snow` with no command prints the root help.
        let _ = app.print_help(spec, &inv, 0);
        finish_agent(&app.cli, Some(0));
        return 0;
    }
    let path = inv.command_path();
    let (v, d) = (app.cli.verbose, app.cli.debug);
    crate::logs::init(&mut app.cli, v, d);
    let result = if app.cli.verbose && app.cli.debug {
        Err(SnowError::Click(ClickError::usage_class(
            "Invalid value: Only one parameter `verbose` or `debug` is possible".into(),
            "BadParameter",
        )))
    } else {
        let leaf = spec.cmd(inv.leaf().cmd);
        if leaf.deprecated {
            eprintln!(
                "DeprecationWarning: The command {} is deprecated.",
                snow_click::py_repr(leaf.name)
            );
        }
        crate::telemetry::pre_execute(&mut app.cli);
        registry::dispatch(&path, &mut app.cli)
    };
    let code = match result {
        Ok(out) => {
            print_output(&mut app.cli, &out, None);
            crate::telemetry::command_succeeded(&mut app.cli);
            0
        }
        Err(SnowError::Click(ClickError::Exit(c))) => {
            finish_agent(&app.cli, Some(c));
            c
        }
        Err(e) => {
            crate::telemetry::command_failed(&mut app.cli, &e);
            crate::feedback::record_last_error(&app.cli, &e);
            flush_warnings(&mut app.cli);
            let code = report_error(&mut app, &inv, &e, true);
            app.cli.close_connection();
            return code;
        }
    };
    app.cli.close_connection();
    flush_warnings(&mut app.cli);
    finish_agent(&app.cli, Some(code));
    code
}

fn finish_agent(cli: &Cli, _code: Option<i32>) {
    if !cli.agent {
        return;
    }
    let data = AGENT_DATA.with(|d| std::mem::take(&mut *d.borrow_mut()));
    let data = match data.len() {
        0 => J::Null,
        1 => data.into_iter().next().unwrap_or(J::Null),
        _ => J::Array(data),
    };
    let mut env = json!({"ok": true, "command": cli.command_name(), "data": data});
    if let Some(msgs) = &cli.console.collected
        && !msgs.is_empty()
    {
        env["meta"] = json!({"messages": msgs});
    }
    println!("{env}");
}
