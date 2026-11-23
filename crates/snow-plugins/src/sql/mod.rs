//! `snow sql` (`snowflake.cli._plugins.sql`).

pub mod commands;
pub mod complete;
pub mod reader;
pub mod repl;

use crate::app::{die_on_json_error, print_output, write_result};
use crate::rendering::SyntaxConfig;
use commands::{open_spool, parse_bool_value, parse_format, strip_matching_quotes};
use indexmap::IndexMap;
use reader::{Compiled, Reader, Renderer, compile_statements};
use snow_api::connect::db_error_to_cli;
use snow_api::output::{Format, JsonStream, Output, Pv, Row, print_csv};
use snow_api::splitter::split_statements;
use snow_api::{Cli, Result, SnowError};
use snow_connector::Cursor;
use std::collections::VecDeque;
use std::io::{IsTerminal, Read};

fn template_config(values: &[String]) -> Result<SyntaxConfig> {
    let v: Vec<String> = values.iter().map(|s| s.to_uppercase()).collect();
    let has = |x: &str| v.iter().any(|s| s == x);
    if (has("ALL") || has("NONE")) && v.len() > 1 {
        return Err(SnowError::usage(
            "ALL and NONE template syntax options should not be used with other options.",
        ));
    }
    if has("ALL") {
        return Ok(SyntaxConfig {
            legacy: true,
            standard: true,
            jinja: true,
        });
    }
    if has("NONE") {
        return Ok(SyntaxConfig {
            legacy: false,
            standard: false,
            jinja: false,
        });
    }
    Ok(SyntaxConfig {
        legacy: has("LEGACY"),
        standard: has("STANDARD"),
        jinja: has("JINJA"),
    })
}

/// `_resolve_startup_options` + `_apply_startup_options`.
fn apply_startup_options(cli: &mut Cli, raw: &[String]) -> Result<()> {
    let mut output_file: Option<String> = None;
    let mut quiet: Option<bool> = None;
    let mut format: Option<Format> = None;
    for r in raw {
        let text = r.trim();
        let Some((k, v)) = text.split_once('=') else {
            return Err(SnowError::argument(format!(
                "Invalid option '{text}'. Use -o <option>=<value>."
            )));
        };
        let key = k.trim();
        match key.to_lowercase().as_str() {
            "output_file" => {
                let n = strip_matching_quotes(v.trim()).to_string();
                output_file = (!n.trim().is_empty()).then_some(n);
            }
            "quiet" => quiet = Some(parse_bool_value("quiet", v).map_err(SnowError::argument)?),
            "output_format" => format = Some(parse_format(v).map_err(SnowError::argument)?),
            _ => {
                return Err(SnowError::argument(format!(
                    "Unknown option '{key}'. Only output_file, quiet and output_format are supported."
                )));
            }
        }
    }
    if format.is_some() && cli.given("format") {
        return Err(SnowError::argument(
            "--format cannot be combined with -o output_format. Use only one of them.",
        ));
    }
    if let Some(f) = output_file {
        open_spool(cli, &f)?;
    }
    if let Some(q) = quiet {
        cli.sql.quiet = q;
    }
    if let Some(f) = format {
        cli.set_format(f);
    }
    Ok(())
}

fn env_overrides(cli: &Cli) -> IndexMap<String, String> {
    cli.env_overrides
        .iter()
        .filter_map(|s| {
            s.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// Runs compiled statements lazily, the way `_execute_compiled_statements`
/// and `execute_stream` interleave with result printing.
struct Executor {
    stmts: Vec<Compiled>,
    idx: usize,
    pending: VecDeque<String>,
    /// `!` commands run with `is_repl` (e.g. `!edit`).
    repl: bool,
}

impl Executor {
    fn new(stmts: Vec<Compiled>, repl: bool) -> Executor {
        Executor {
            stmts,
            idx: 0,
            pending: VecDeque::new(),
            repl,
        }
    }

    /// Run one statement. `VerboseCursor` echoes it; the REPL uses a plain cursor.
    fn execute(&self, cli: &mut Cli, sql: &str, is_async: bool) -> Result<Cursor> {
        if !self.repl {
            cli.console.plain_message(sql);
        }
        let verbose = cli.connector_info;
        let conn = cli.connection()?;
        let c = if is_async {
            conn.execute_async(sql)
        } else {
            conn.execute(sql)
        };
        c.map_err(|e| db_error_to_cli(&e, verbose))
    }

    fn next(&mut self, cli: &mut Cli) -> Result<Option<Cursor>> {
        loop {
            if let Some(sql) = self.pending.pop_front() {
                // `_echo_repl_statement`: the REPL `!set echo=true` prints the SQL first.
                if self.repl && cli.sql.echo && !cli.format.is_structured() {
                    write_result(cli, |s| {
                        s.write_str(&sql);
                        s.write_str("\n");
                    });
                }
                return self.execute(cli, &sql, false).map(Some);
            }
            let Some(st) = self.stmts.get(self.idx) else {
                return Ok(None);
            };
            self.idx += 1;
            match st {
                Compiled::Sql(s) => {
                    self.pending = split_statements(&snow_rich::render::dedent(s), false)
                        .into_iter()
                        .map(|s| s.text)
                        .collect();
                }
                Compiled::Async(sql) => {
                    let c = self.execute(cli, sql, true)?;
                    let row: Row = [("scheduled query ID".to_string(), Pv::opt(c.sfqid.clone()))]
                        .into_iter()
                        .collect();
                    print_output(cli, &Output::Collection(vec![row]), None);
                }
                Compiled::Command(cmd) => cmd.execute(cli, self.repl)?,
            }
        }
    }

    fn drain(&mut self, cli: &mut Cli) -> Result<()> {
        while self.next(cli)?.is_some() {}
        Ok(())
    }
}

/// Print `MultipleResults` as it is produced.
fn print_multiple(cli: &mut Cli, ex: &mut Executor) -> Result<()> {
    let format = cli.format;
    if cli.agent {
        let mut all = Vec::new();
        while let Some(c) = ex.next(cli)? {
            all.push(cli.query_result(&c));
        }
        print_output(cli, &Output::Multiple(all), None);
        return Ok(());
    }
    match format {
        Format::Csv => {
            while let Some(c) = ex.next(cli)? {
                let out = cli.query_result(&c);
                write_result(cli, |s| {
                    print_csv(&out, s);
                    s.write_str("\n");
                });
            }
        }
        Format::Json | Format::JsonExt => {
            let mut stream = write_result(cli, JsonStream::open);
            while let Some(c) = ex.next(cli)? {
                let out = cli.query_result(&c);
                if let Err(e) = write_result(cli, |s| stream.item(&out, s)) {
                    die_on_json_error(cli, e);
                }
            }
            write_result(cli, |s| {
                JsonStream::close(s);
                s.write_str("\n");
            });
        }
        _ => {
            while let Some(c) = ex.next(cli)? {
                let out = cli.query_result(&c);
                print_output(cli, &out, None);
            }
        }
    }
    Ok(())
}

/// Print the template and `!` command errors and fail with "SQL rendering error".
fn render_failed(cli: &mut Cli, errors: &[String]) -> SnowError {
    for e in errors {
        cli.console.warning(e);
    }
    SnowError::sql("SQL rendering error")
}

pub fn execute(cli: &mut Cli) -> Result<Output> {
    let r = run(cli);
    cli.sql = snow_api::context::SqlSession::default();
    r
}

fn run(cli: &mut Cli) -> Result<Output> {
    let mut data: IndexMap<String, String> = IndexMap::new();
    for v in crate::common::parse_key_value_variables(&cli.list("data_override"))? {
        data.insert(v.key, v.value);
    }
    let cfg = template_config(&cli.list("enabled_templating"))?;
    let retain_comments = cli.flag("retain_comments");
    let single_transaction = cli.flag("single_transaction");
    let std_in = cli.flag("std_in");
    let local_only = match cli.opt_bool("local_only") {
        Some(b) => b,
        None => cli
            .config
            .bool_value(&["cli"], "sql_local_only", Some(false))?
            .unwrap_or(false),
    };
    let mut query = cli.str("query").filter(|q| !q.is_empty());
    let files = cli.list("files");
    let no_source = query.is_none() && files.is_empty() && !std_in;
    if no_source && !std::io::stdin().is_terminal() {
        let mut s = String::new();
        let _ = std::io::stdin().read_to_string(&mut s);
        let s = s.trim();
        if !s.is_empty() {
            query = Some(s.to_string());
        }
    }
    let mut prompt_format = None;
    if no_source {
        prompt_format = match cli.str("prompt_format") {
            Some(p) => Some(p),
            None => repl::require_string(cli.config.value_without_env(&["cli"], "prompt_format"))?,
        };
        repl::warn_unknown_tokens(cli, prompt_format.as_deref());
        if single_transaction {
            return Err(SnowError::argument(
                "single transaction cannot be used with REPL",
            ));
        }
    }
    apply_startup_options(cli, &cli.list("option"))?;
    let env = env_overrides(cli);
    let make_reader = || Reader {
        renderer: Renderer {
            cfg,
            data: &data,
            env_overrides: &env,
        },
        remove_comments: !retain_comments,
        local_only,
        out: Vec::new(),
    };
    if no_source {
        let no_prompt_exit = match cli.opt_bool("no_prompt_exit_repl") {
            Some(b) => b,
            None => cli
                .config
                .bool_value(&["cli"], "no_prompt_exit_repl", Some(false))?
                .unwrap_or(false),
        };
        let mut run_text = |cli: &mut Cli, text: &str, print: bool| -> Result<usize> {
            let mut reader = make_reader();
            reader.read_query(&mut cli.console, text)?;
            let parsed = std::mem::take(&mut reader.out);
            let (errors, count, compiled) = compile_statements(parsed);
            if !errors.is_empty() {
                return Err(render_failed(cli, &errors));
            }
            let mut ex = Executor::new(compiled, true);
            if print {
                print_multiple(cli, &mut ex)?;
            } else {
                ex.drain(cli)?;
            }
            Ok(count)
        };
        repl::run(cli, prompt_format, no_prompt_exit, &mut run_text)?;
        return Ok(Output::Empty);
    }

    let mut reader = make_reader();
    if std_in {
        let mut s = String::new();
        let _ = std::io::stdin().read_to_string(&mut s);
        query = Some(s);
    }
    let console = &mut cli.console;
    match &query {
        Some(q) if !q.is_empty() => reader.read_query(console, q)?,
        _ if !files.is_empty() => reader.read_files(console, &files)?,
        _ => {
            return Err(SnowError::argument(
                "Use either query, filename or input option.",
            ));
        }
    }
    let parsed = std::mem::take(&mut reader.out);
    let (errors, mut count, mut compiled) = compile_statements(parsed);
    if errors.is_empty() && count == 0 && compiled.is_empty() {
        return Err(SnowError::argument("No SQL statements found to execute."));
    }
    if !errors.is_empty() {
        return Err(render_failed(cli, &errors));
    }
    if single_transaction {
        let verbose = cli.connector_info;
        cli.connection()?
            .execute("ALTER SESSION SET autocommit=False")
            .map_err(|e| db_error_to_cli(&e, verbose))?;
        compiled.insert(0, Compiled::Sql("BEGIN;".into()));
        compiled.push(Compiled::Sql("COMMIT;".into()));
        count = compiled.len();
    }
    let mut ex = Executor::new(compiled, false);
    match count {
        0 => ex.drain(cli)?,
        1 => {
            if let Some(c) = ex.next(cli)? {
                let out = cli.query_result(&c);
                print_output(cli, &out, None);
            }
            ex.drain(cli)?;
        }
        _ => print_multiple(cli, &mut ex)?,
    }
    Ok(Output::Empty)
}
