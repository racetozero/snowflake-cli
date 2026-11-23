//! The `snow sql` REPL (`_plugins/sql/repl.py`, `prompt_format.py`).
//!
//! prompt_toolkit is replaced by a small raw-mode line editor on a terminal
//! (crossterm; history in `<config dir>/repl_history` in FileHistory format)
//! and plain line reads for piped input: statements end with `;`, `!` commands
//! and `exit`/`quit` submit at once, Ctrl-C drops the input, Ctrl-D asks before
//! leaving. Completion menus and highlighting are not ported (see allowlist).

use snow_api::{Cli, Result, SnowError};
use std::io::{BufRead, IsTerminal, Write};

pub const DEFAULT_PROMPT: &str = " > ";
const PLACEHOLDERS: [&str; 8] = [
    "user",
    "host",
    "account",
    "role",
    "warehouse",
    "database",
    "schema",
    "connection",
];

/// `require_string_prompt_format(value)`.
pub fn require_string(v: Option<snow_api::config::Cv>) -> Result<Option<String>> {
    use snow_api::config::Cv;
    match v {
        None => Ok(None),
        Some(Cv::Str(s)) => Ok(Some(s)),
        Some(other) => Err(SnowError::cli(format!(
            "Expected a string for cli.prompt_format, got {}. Use a quoted value, for example prompt_format = \"[user]> \".",
            match other {
                Cv::Bool(_) => "bool",
                Cv::Int(_) => "int",
                Cv::Float(_) => "float",
                Cv::List(_) => "list",
                Cv::Table(_) => "dict",
                _ => "str",
            }
        ))),
    }
}

fn token_re() -> regex::Regex {
    regex::Regex::new(r"\\\\|\\\[|\\\]|\\n|\[[^\[\]]*\]").expect("re")
}

fn colour(token: &str) -> Option<(bool, String)> {
    let c = &token[1..token.len() - 1];
    let fg = regex::Regex::new(r"(?i)^#([0-9a-f]{6})$").expect("re");
    let bg = regex::Regex::new(r"(?i)^bg:#([0-9a-f]{6})$").expect("re");
    if let Some(m) = fg.captures(c) {
        return Some((true, format!("#{}", m[1].to_lowercase())));
    }
    bg.captures(c)
        .map(|m| (false, format!("#{}", m[1].to_lowercase())))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(*c as u32, 0..=8 | 0x0b..=0x1f | 0x7f))
        .collect()
}

/// `_warn_unknown_prompt_tokens(prompt_format)`.
pub fn warn_unknown_tokens(cli: &mut Cli, template: Option<&str>) {
    let Some(t) = template else {
        return;
    };
    let mut seen: Vec<String> = Vec::new();
    for m in token_re().find_iter(t) {
        let tok = m.as_str();
        if ["\\\\", "\\[", "\\]", "\\n"].contains(&tok) || colour(tok).is_some() {
            continue;
        }
        if !PLACEHOLDERS.contains(&tok[1..tok.len() - 1].to_lowercase().as_str()) {
            let s = sanitize(tok);
            if !seen.contains(&s) {
                seen.push(s);
            }
        }
    }
    if seen.is_empty() {
        return;
    }
    let listed: Vec<String> = seen.iter().map(|t| format!("'{t}'")).collect();
    let supported: Vec<String> = PLACEHOLDERS.iter().map(|p| format!("[{p}]")).collect();
    cli.console.stderr_warning(&format!(
        "Ignoring unknown prompt placeholder(s) {}. Supported placeholders are {}. Use \\[ \\] \\\\ for literal brackets and backslash.",
        listed.join(", "),
        supported.join(", ")
    ));
}

/// `format_repl_prompt_for_terminal(template, values)`: colours as ANSI on a terminal.
fn render(template: Option<&str>, values: &dyn Fn(&str) -> Option<String>, ansi: bool) -> String {
    let Some(t) = template.filter(|t| !t.is_empty()) else {
        return DEFAULT_PROMPT.into();
    };
    let mut out = String::new();
    let mut cursor = 0;
    for m in token_re().find_iter(t) {
        out.push_str(&sanitize(&t[cursor..m.start()]));
        let tok = m.as_str();
        match tok {
            "\\\\" => out.push('\\'),
            "\\[" => out.push('['),
            "\\]" => out.push(']'),
            "\\n" => out.push('\n'),
            _ => {
                if let Some((fg, hex)) = colour(tok) {
                    if ansi {
                        let (r, g, b) = (
                            u8::from_str_radix(&hex[1..3], 16).unwrap_or(0),
                            u8::from_str_radix(&hex[3..5], 16).unwrap_or(0),
                            u8::from_str_radix(&hex[5..7], 16).unwrap_or(0),
                        );
                        out.push_str(&format!("\x1b[{};2;{r};{g};{b}m", if fg { 38 } else { 48 }));
                    }
                } else {
                    let name = tok[1..tok.len() - 1].to_lowercase();
                    if PLACEHOLDERS.contains(&name.as_str()) {
                        out.push_str(&sanitize(
                            &values(&name)
                                .filter(|v| !v.is_empty())
                                .unwrap_or_else(|| format!("(no {name})")),
                        ));
                    }
                }
            }
        }
        cursor = m.end();
    }
    out.push_str(&sanitize(&t[cursor..]));
    if out.is_empty() {
        return DEFAULT_PROMPT.into();
    }
    if ansi && out.contains('\x1b') {
        out.push_str("\x1b[0m");
    }
    out
}

fn prompt_values(cli: &mut Cli, name: &str) -> Option<String> {
    let conn_name = cli.conn.connection_name.clone();
    if name == "connection" {
        return conn_name;
    }
    let c = cli.connection().ok()?;
    match name {
        "user" => c.user.clone(),
        "host" => Some(c.host.clone()),
        "account" => Some(c.account.clone()),
        "role" => c.session.role.clone(),
        "warehouse" => c.session.warehouse.clone(),
        "database" => c.session.database.clone(),
        "schema" => c.session.schema.clone(),
        _ => None,
    }
}

enum Input {
    Line(String),
    Eof,
    /// Ctrl-C: drop the current input and prompt again.
    Interrupted,
}

/// A line of user input: plain reads for piped input, a small editor on a terminal.
fn read_line(prompt: &str, history: &mut History, cli: Option<&mut Cli>) -> Input {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return edit_line(prompt, history, cli);
    }
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    match std::io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => Input::Eof,
        Ok(_) => Input::Line(s.trim_end_matches(['\n', '\r']).to_string()),
    }
}

/// prompt_toolkit `FileHistory` (`<config dir>/repl_history`): `\n# <time>\n+line...` entries.
pub struct History {
    entries: Vec<String>,
    path: Option<std::path::PathBuf>,
}

impl History {
    fn load(path: Option<std::path::PathBuf>) -> History {
        let mut entries = Vec::new();
        if let Some(text) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            let mut cur: Vec<String> = Vec::new();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix('+') {
                    cur.push(rest.to_string());
                } else if !cur.is_empty() {
                    entries.push(cur.join("\n"));
                    cur.clear();
                }
            }
            if !cur.is_empty() {
                entries.push(cur.join("\n"));
            }
        }
        History { entries, path }
    }

    fn push(&mut self, entry: &str) {
        self.entries.push(entry.to_string());
        if let Some(p) = &self.path
            && let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
        {
            let mut text = format!(
                "\n# {}\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.6f")
            );
            for l in entry.split('\n') {
                text.push_str(&format!("+{l}\n"));
            }
            let _ = f.write_all(text.as_bytes());
        }
    }
}

/// Raw-mode line editing: arrows, Home/End, Backspace/Delete, history, Ctrl-A/E/U/C/D.
fn edit_line(prompt: &str, history: &mut History, mut cli: Option<&mut Cli>) -> Input {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, read};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    if enable_raw_mode().is_err() {
        return Input::Eof;
    }
    let mut buf: Vec<char> = Vec::new();
    let mut pos = 0usize;
    let mut hidx = history.entries.len();
    let colour = cli.is_some();
    let mut last_tab = false;
    let draw = |buf: &[char], pos: usize| {
        let raw: String = buf.iter().collect();
        let text = if colour {
            super::complete::highlight(&raw)
        } else {
            raw
        };
        let back = buf.len() - pos;
        let mut out = format!("\r\x1b[K{prompt}{text}");
        if back > 0 {
            out.push_str(&format!("\x1b[{back}D"));
        }
        print!("{out}");
        let _ = std::io::stdout().flush();
    };
    draw(&buf, pos);
    let result = loop {
        let Ok(ev) = read() else { break Input::Eof };
        let Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) = ev
        else {
            continue;
        };
        if kind == KeyEventKind::Release {
            continue;
        }
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let tab = code == KeyCode::Tab;
        if tab && let Some(c) = cli.as_deref_mut() {
            let before: String = buf[..pos].iter().collect();
            let (start, cands) = super::complete::candidates(c, &before);
            let start_chars = before[..start].chars().count();
            let common =
                cands
                    .iter()
                    .skip(1)
                    .fold(cands.first().cloned().unwrap_or_default(), |acc, c| {
                        acc.chars()
                            .zip(c.chars())
                            .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
                            .map(|(a, _)| a)
                            .collect()
                    });
            let typed = pos - start_chars;
            if cands.len() == 1 || common.chars().count() > typed {
                let rep: Vec<char> = if cands.len() == 1 {
                    cands[0].chars().collect()
                } else {
                    common.chars().collect()
                };
                buf.splice(start_chars..pos, rep.iter().copied());
                pos = start_chars + rep.len();
            } else if cands.len() > 1 && last_tab {
                let shown: Vec<&String> = cands.iter().take(60).collect();
                print!(
                    "\r\n{}\r\n",
                    shown
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("  ")
                );
            }
            last_tab = true;
            draw(&buf, pos);
            continue;
        }
        last_tab = false;
        match code {
            KeyCode::Char('c') if ctrl => break Input::Interrupted,
            KeyCode::Char('d') if ctrl => {
                if buf.is_empty() {
                    break Input::Eof;
                }
                if pos < buf.len() {
                    buf.remove(pos);
                }
            }
            KeyCode::Char('a') if ctrl => pos = 0,
            KeyCode::Char('e') if ctrl => pos = buf.len(),
            KeyCode::Char('u') if ctrl => {
                buf.drain(..pos);
                pos = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                buf.insert(pos, c);
                pos += 1;
            }
            KeyCode::Enter => break Input::Line(buf.iter().collect()),
            KeyCode::Backspace if pos > 0 => {
                pos -= 1;
                buf.remove(pos);
            }
            KeyCode::Delete if pos < buf.len() => {
                buf.remove(pos);
            }
            KeyCode::Left if pos > 0 => pos -= 1,
            KeyCode::Right if pos < buf.len() => pos += 1,
            KeyCode::Home => pos = 0,
            KeyCode::End => pos = buf.len(),
            KeyCode::Up if hidx > 0 => {
                hidx -= 1;
                buf = history.entries[hidx].replace('\n', " ").chars().collect();
                pos = buf.len();
            }
            KeyCode::Down if hidx < history.entries.len() => {
                hidx += 1;
                buf = history
                    .entries
                    .get(hidx)
                    .map(|e| e.replace('\n', " ").chars().collect())
                    .unwrap_or_default();
                pos = buf.len();
            }
            _ => {}
        }
        draw(&buf, pos);
    };
    let _ = disable_raw_mode();
    println!();
    result
}

/// `Repl.ask_yn(question)`: Enter, `y` and Ctrl-D answer yes; `n` and Ctrl-C answer no.
fn ask_yn(question: &str, history: &mut History) -> bool {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return ask_key(question);
    }
    loop {
        match read_line(&format!("{question} (y/n): "), history, None) {
            Input::Eof => return true,
            Input::Interrupted => return false,
            Input::Line(a) => match a.trim() {
                "" | "y" => return true,
                "n" => return false,
                _ => continue,
            },
        }
    }
}

/// The yes/no key bindings: Enter, Ctrl-D and `y` answer yes; `n` and Ctrl-C no.
fn ask_key(question: &str) -> bool {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, read};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    print!("\r\x1b[K{question} (y/n): ");
    let _ = std::io::stdout().flush();
    if enable_raw_mode().is_err() {
        return true;
    }
    let answer = loop {
        let Ok(Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        })) = read()
        else {
            break true;
        };
        if kind == KeyEventKind::Release {
            continue;
        }
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Enter | KeyCode::Char('y') => break true,
            KeyCode::Char('d') if ctrl => break true,
            KeyCode::Char('c') if ctrl => break false,
            KeyCode::Char('n') => break false,
            _ => {}
        }
    };
    let _ = disable_raw_mode();
    println!("{}", if answer { "y" } else { "n" });
    answer
}

/// Read one submission: a `;`-terminated statement, a `!` command or an exit keyword.
fn read_submission(prompt: &str, history: &mut History, cli: &mut Cli, complete: bool) -> Input {
    let mut buf = String::new();
    let mut p = prompt.to_string();
    loop {
        let line = match read_line(&p, history, if complete { Some(&mut *cli) } else { None }) {
            Input::Line(l) => l,
            other => return other,
        };
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(&line);
        let t = buf.trim();
        if t.is_empty() {
            buf.clear();
            continue;
        }
        if t.ends_with(';')
            || ["exit", "quit"].contains(&t.to_lowercase().as_str())
            || super::commands::compile_repl_command(t).is_ok() && t.starts_with('!')
        {
            history.push(t);
            return Input::Line(buf);
        }
        p = " ".repeat(prompt.chars().count());
    }
}

/// `Repl(...).run()`.
pub fn run(
    cli: &mut Cli,
    prompt_format: Option<String>,
    no_prompt_exit: bool,
    run_text: &mut dyn FnMut(&mut Cli, &str, bool) -> Result<usize>,
) -> Result<()> {
    cli.console
        .panel("Welcome to Snowflake-CLI REPL\nType 'exit' or 'quit' to leave");
    let leave = |cli: &mut Cli| {
        cli.console
            .styled_message("\nLeaving REPL, bye ...\n", "bold #ff5f00")
    };
    if !std::io::stdin().is_terminal() {
        eprintln!("Warning: Input is not a terminal (fd=0).");
    }
    run_text(cli, "select current_version();", false).map(|_| ())?;
    let ansi = std::io::stdout().is_terminal();
    let auto_completion = cli
        .config
        .bool_value(&["cli"], "auto_completion", Some(true))
        .ok()
        .flatten()
        .unwrap_or(true);
    let mut history = History::load(
        cli.config
            .file_path
            .parent()
            .map(|d| d.join("repl_history")),
    );
    loop {
        let prompt = match &prompt_format {
            None => DEFAULT_PROMPT.to_string(),
            Some(f) => {
                let mut vals = std::collections::HashMap::new();
                for p in PLACEHOLDERS {
                    vals.insert(p, prompt_values(cli, p));
                }
                render(Some(f), &|n| vals.get(n).cloned().flatten(), ansi)
            }
        };
        let input = match read_submission(&prompt, &mut history, cli, auto_completion) {
            Input::Line(l) => l,
            Input::Interrupted => continue,
            Input::Eof => {
                if no_prompt_exit || ask_yn("Do you want to leave?", &mut history) {
                    leave(cli);
                    return Ok(());
                }
                continue;
            }
        };
        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }
        if ["exit", "quit"].contains(&input.to_lowercase().as_str()) {
            if no_prompt_exit || ask_yn("Do you want to leave?", &mut history) {
                leave(cli);
                return Ok(());
            }
            continue;
        }
        let started = std::time::Instant::now();
        match run_text(cli, &input, true) {
            Ok(n) if n > 0 => print_elapsed(cli, started.elapsed().as_secs_f64()),
            Ok(_) => {}
            Err(e) => cli
                .console
                .warning(&format!("\nError occurred: {}", e.message())),
        }
    }
}

/// `_print_sql_elapsed(elapsed_seconds)`.
fn print_elapsed(cli: &mut Cli, secs: f64) {
    if !cli.sql.timing || cli.format.is_structured() {
        return;
    }
    let line = format!("Time Elapsed: {secs:.3}s");
    if !cli.sql.quiet {
        cli.console.message(&line);
    }
    if cli.sql.timing_in_output_file
        && let Some(f) = cli.sql.spool.as_mut()
    {
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}
