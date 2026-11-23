//! The `snow` entry point: parse, run the command, print the result or the
//! error, and return the exit code. Agent mode wraps everything in one JSON
//! envelope on stdout.

use crate::registry;
use serde_json::{Value as J, json};
use snow_api::config::{Config, FeatureFlag};
use snow_api::output::{Format, JsonTypeError, Output, Sink, print_result};
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

/// A root option that runs after all root options were processed.
#[derive(Clone, Copy)]
enum RootAction {
    Help,
    Structure,
    Docs,
}

/// State shared by the parser hooks and the command.
pub struct App {
    pub cli: Cli,
    deferred_root: Option<RootAction>,
    config_ready: bool,
    /// The SnowError behind the last `ClickError::Exception` given to the parser.
    pending: Option<SnowError>,
}

/// How a run ended, before it is printed.
enum Outcome {
    Exit(i32),
    Ok(Output),
    Failed { err: SnowError, from_command: bool },
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
                crate::helpers::startup_encoding_check(&mut self.cli)
                    .map_err(|e| self.park_error(e))
            }
            Err(e) => Err(self.park_error(e)),
        }
    }

    fn help_env(&self) -> HelpEnv {
        HelpEnv {
            condensed: self
                .cli
                .config
                .feature(FeatureFlag::EnableCondensedCommandHelp),
            docs_in_help: self
                .cli
                .config
                .feature(FeatureFlag::EnableCommandDocsInHelp),
        }
    }

    /// Carry a SnowError through the parser as a ClickError.
    fn park_error(&mut self, e: SnowError) -> ClickError {
        match e {
            SnowError::Click(c) => c,
            other => {
                let err = ClickError::Exception {
                    message: other.message(),
                    code: other.exit_code(false),
                };
                self.pending = Some(other);
                err
            }
        }
    }
}

impl Hooks for App {
    fn feature_enabled(&self, flag: &str) -> bool {
        FeatureFlag::from_name(flag).is_some_and(|f| self.cli.config.feature(f))
    }

    fn convert_custom(
        &mut self,
        _param: &'static Param,
        ty: &'static str,
        raw: &str,
    ) -> Result<Value, ClickError> {
        crate::types::convert(ty, raw).map_err(|e| self.park_error(e))
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
        crate::callbacks::run(&mut self.cli, param, value).map_err(|e| self.park_error(e))
    }

    fn prompt(&mut self) -> Result<Value, ClickError> {
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
            return Err(self.park_error(SnowError::argument(
                "Cannot use --help with --help-all. Use one or the other.",
            )));
        }
        let env = self.help_env();
        let mut rs = render_help(spec, self, &env, inv, depth);
        if depth == 0 {
            rs.extend(shared_panels(spec, &env));
        }
        emit_text(&mut self.cli, &print_to_string(&rs, false));
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
            "custom_help" if value.as_bool() => self.deferred_root = Some(RootAction::Help),
            "structure" if value.as_bool() => self.deferred_root = Some(RootAction::Structure),
            "docs" if value.as_bool() => self.deferred_root = Some(RootAction::Docs),
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
            // Typer's `--install-completion` / `--show-completion` do nothing here.
            "install_completion" | "show_completion" if value.as_bool() => {
                self.ensure_config(None)?;
                return Err(ClickError::Exit(0));
            }
            "commands_registration" => {
                self.ensure_config(None)?;
                match self.deferred_root.take() {
                    Some(RootAction::Help) => {
                        self.print_help(spec, inv, 0)?;
                        return Err(ClickError::Exit(0));
                    }
                    Some(RootAction::Structure) => {
                        let out = crate::root::structure(spec);
                        print_output(&mut self.cli, &out, Some(Format::Json));
                        return Err(ClickError::Exit(0));
                    }
                    Some(RootAction::Docs) => return Err(ClickError::Exit(0)),
                    None => {}
                }
            }
            _ => {}
        }
        Ok(value)
    }
}

fn emit_text(cli: &mut Cli, s: &str) {
    if cli.agent {
        let plain = snow_api::sanitize::for_terminal(s);
        cli.agent_data.push(json!({ "help": plain }));
        return;
    }
    let _ = std::io::stdout().write_all(s.as_bytes());
}

/// The agent-mode envelope: `ok` with `data`, or `ok: false` with `error`.
pub fn envelope(command: &str, result: std::result::Result<J, J>) -> J {
    match result {
        Ok(data) => json!({"ok": true, "command": command, "data": data}),
        Err(error) => json!({"ok": false, "command": command, "error": error}),
    }
}

/// A result that cannot be printed as JSON ends the run like Python's
/// unhandled `TypeError`.
pub fn die_on_json_error(cli: &mut Cli, e: JsonTypeError) -> ! {
    cli.console.warning(&format!(
        "\nAn unexpected exception occurred. Use --debug option to see the traceback. Exception message:\n\n{}",
        e.0
    ));
    cli.flush_telemetry();
    std::process::exit(1);
}

pub fn print_output(cli: &mut Cli, out: &Output, format: Option<Format>) {
    let fmt = format.unwrap_or(cli.format);
    if cli.agent {
        cli.agent_data.push(out.to_json_value());
        return;
    }
    if let Err(e) = write_result(cli, |sink| print_result(out, fmt, sink)) {
        die_on_json_error(cli, e);
    }
}

/// Write query-result text through `_result_stream()`: stdout unless
/// `quiet`, and the open spool file. Both get the non-terminal layout
/// while quiet or spooling.
pub fn write_result<T>(cli: &mut Cli, f: impl FnOnce(&mut Sink) -> T) -> T {
    let stdout = std::io::stdout();
    let sql = &mut cli.sql;
    if !sql.quiet && sql.spool.is_none() {
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
    if !sql.quiet {
        let mut lock = stdout.lock();
        let _ = lock.write_all(&buf);
        let _ = lock.flush();
    }
    if let Some(file) = sql.spool.as_mut() {
        let _ = file.write_all(&buf);
        let _ = file.flush();
    }
    r
}

/// Strip `--agent` (before `--agent-args` and `--`) and report whether it was present.
pub fn take_agent_flag(argv: &mut Vec<String>) -> bool {
    let mut agent =
        std::env::var("SNOW_AGENT").is_ok_and(|v| snow_api::config::parse_on_off(&v) == Some(true));
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

fn agent_error_code(e: &SnowError) -> (&'static str, i32) {
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
fn report_error(cli: &mut Cli, inv: &Invocation, e: &SnowError, from_command: bool) -> i32 {
    if cli.agent {
        let (code, exit) = agent_error_code(e);
        let mut err = json!({"code": code, "message": e.message()});
        if let SnowError::Cli { after, .. } = e
            && !after.is_empty()
        {
            err["remediation"] = json!(after);
        }
        println!("{}", envelope(&cli.command_name(), Err(err)));
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
            let mut c = c.clone();
            if from_command
                && let ClickError::Usage {
                    depth: d @ None,
                    attach: true,
                    ..
                } = &mut c
            {
                *d = Some(inv.levels.len().saturating_sub(1));
            }
            let s = print_to_string(&render_error(&SPEC, inv, &c), true);
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
    e.exit_code(cli.enhanced_exit_codes)
}

fn flush_warnings(cli: &mut Cli) {
    for w in cli.warnings.drain(..) {
        if !cli.agent {
            eprintln!("{w}");
        }
    }
}

/// Run `snow` with `argv` (without the program name and `--agent`) and
/// return the exit code.
pub fn run(argv: Vec<String>, agent: bool) -> i32 {
    if argv.first().is_some_and(|a| a == "skill") {
        return crate::skill::run(&argv[1..], agent);
    }
    if argv.first().is_some_and(|a| a == "update") {
        return crate::update::run(&argv[1..], agent);
    }
    let spec: &'static Spec = &SPEC;
    let mut app = App {
        cli: Cli::new(
            spec,
            Invocation {
                levels: vec![],
                argv: argv.clone(),
                spec,
            },
            agent,
        ),
        deferred_root: None,
        config_ready: false,
        pending: None,
    };
    app.cli.console.color = snow_click::term::color(false);
    if !agent {
        for w in snow_api::config::connector_import_warnings() {
            eprintln!("{w}");
        }
    }
    let (inv, outcome) = execute(&mut app, argv);
    finish(&mut app.cli, &inv, outcome)
}

/// Parse and run the command; the invocation is the one errors are rendered against.
fn execute(app: &mut App, argv: Vec<String>) -> (Invocation, Outcome) {
    let spec: &'static Spec = &SPEC;
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
        let err = app.pending.take().unwrap_or(SnowError::from(e));
        let inv = Invocation {
            levels: vec![],
            argv,
            spec,
        };
        let failed = Outcome::Failed {
            err,
            from_command: false,
        };
        return (inv, failed);
    }
    let parsed = Processor {
        spec,
        hooks: &mut *app,
    }
    .run(0, argv);
    let inv = match parsed {
        Ok(inv) => inv,
        Err((ClickError::Exit(code), inv)) => return (inv, Outcome::Exit(code)),
        Err((e, inv)) => {
            app.cli.inv = inv.clone();
            let err = match &e {
                ClickError::Exception { .. } => app.pending.take().unwrap_or(SnowError::from(e)),
                _ => SnowError::Click(e),
            };
            let failed = Outcome::Failed {
                err,
                from_command: false,
            };
            return (inv, failed);
        }
    };
    app.cli.inv = inv.clone();
    flush_warnings(&mut app.cli);
    if inv.levels.len() == 1 {
        let _ = app.print_help(spec, &inv, 0);
        return (inv, Outcome::Exit(0));
    }
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
        registry::dispatch(&inv.command_path(), &mut app.cli)
    };
    let outcome = match result {
        Ok(out) => Outcome::Ok(out),
        Err(SnowError::Click(ClickError::Exit(c))) => Outcome::Exit(c),
        Err(err) => Outcome::Failed {
            err,
            from_command: true,
        },
    };
    (inv, outcome)
}

/// Print the outcome (result, error, or agent envelope), close the
/// connection and return the exit code.
fn finish(cli: &mut Cli, inv: &Invocation, outcome: Outcome) -> i32 {
    let code = match &outcome {
        Outcome::Exit(c) => *c,
        Outcome::Ok(out) => {
            print_output(cli, out, None);
            crate::telemetry::send(cli, None);
            0
        }
        Outcome::Failed { err, from_command } => {
            if *from_command {
                crate::telemetry::send(cli, Some(err));
                crate::feedback::record_last_error(cli, err);
            }
            flush_warnings(cli);
            report_error(cli, inv, err, *from_command)
        }
    };
    cli.close_connection();
    flush_warnings(cli);
    if cli.agent && !matches!(outcome, Outcome::Failed { .. }) {
        let data = std::mem::take(&mut cli.agent_data);
        let data = match data.len() {
            0 => J::Null,
            1 => data.into_iter().next().unwrap_or(J::Null),
            _ => J::Array(data),
        };
        let mut env = envelope(&cli.command_name(), Ok(data));
        if code != 0 {
            env["ok"] = json!(false);
            env["error"] =
                json!({"code": "exit", "message": format!("The command exited with code {code}.")});
        }
        if let Some(msgs) = &cli.console.collected
            && !msgs.is_empty()
        {
            env["meta"] = json!({"messages": msgs});
        }
        println!("{env}");
    }
    code
}
