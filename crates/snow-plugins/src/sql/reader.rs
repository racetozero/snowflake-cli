//! `snowflake.cli._plugins.sql.statement_reader`: read statements from a
//! query or files, render templates, expand `!source` / `!load`, and compile
//! the result into statements and `!` commands.

use super::commands::{ReplCommand, compile_repl_command};
use crate::rendering::{RenderError, SyntaxConfig, sql_render, transpile_snowsql_templates};
use indexmap::IndexMap;
use regex::Regex;
use snow_api::Result;
use snow_api::console::Console;
use snow_api::splitter::{Statement, split_raw};
use snow_api::stage_path::pure_path;
use std::sync::LazyLock;

const MAX_DEPTH: usize = 32;
const MAX_INCLUDES: usize = 32;
const SIZE_LIMIT_MB: u64 = 128;
const ASYNC_SUFFIX: &str = ";>";

static COMMAND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)^!(\w+)\s*["']?(.*?)["']?\s*(?:;|$)"#).expect("re"));
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(\w+?):/(/.*)").expect("re"));
static SQL_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)'[^']*(?:''[^']*)*'|\$\$.*?\$\$|(/\*)|(--[^\n]*)").expect("re")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Query,
    Unknown,
    Url,
    ReplCommand,
}

#[derive(Debug)]
pub struct Parsed {
    pub statement: String,
    pub kind: Kind,
    pub source_path: Option<String>,
    pub error: Option<String>,
}

impl Parsed {
    fn new(
        statement: &str,
        kind: Kind,
        source_path: Option<&str>,
        error: Option<String>,
    ) -> Parsed {
        Parsed {
            statement: statement.to_string(),
            kind,
            source_path: source_path.map(str::to_string),
            error,
        }
    }
}

/// Template settings shared by every statement.
pub struct Renderer<'a> {
    pub cfg: SyntaxConfig,
    pub data: &'a IndexMap<String, String>,
    pub env_overrides: &'a IndexMap<String, String>,
}

impl Renderer<'_> {
    fn vars(&self) -> crate::rendering::TemplateData<'_> {
        crate::rendering::TemplateData {
            data: self.data,
            env_overrides: self.env_overrides,
        }
    }

    /// The per-statement operators (when Jinja is off).
    fn operators(
        &self,
        console: &mut Console,
        text: &str,
    ) -> std::result::Result<String, RenderError> {
        if self.cfg.jinja {
            return Ok(text.to_string());
        }
        let mut s = text.to_string();
        if self.cfg.legacy {
            s = transpile_snowsql_templates(&s);
        }
        let cfg = SyntaxConfig {
            jinja: false,
            ..self.cfg
        };
        sql_render(&s, cfg, &self.vars(), console)
    }

    /// `_jinja_pre_render` (when Jinja is on).
    fn pre_render(&self, console: &mut Console, text: &str) -> Result<String> {
        if !self.cfg.jinja {
            return Ok(text.to_string());
        }
        let (mut content, saved) = protect_sql_comments(text);
        if self.cfg.legacy {
            content = transpile_snowsql_templates(&content);
        }
        let rendered = sql_render(&content, self.cfg, &self.vars(), console)
            .map_err(RenderError::into_snow)?;
        Ok(saved.restore(&rendered))
    }
}

struct SavedComments {
    comments: Vec<String>,
    nonce: String,
}

impl SavedComments {
    fn save(&mut self, text: &str) -> String {
        let i = self.comments.len();
        self.comments.push(text.to_string());
        format!("__SQLC_{}_{i}__", self.nonce)
    }
    fn restore(&self, sql: &str) -> String {
        let mut s = sql.to_string();
        for (i, c) in self.comments.iter().enumerate() {
            s = s.replace(&format!("__SQLC_{}_{i}__", self.nonce), c);
        }
        s
    }
}

/// `_protect_sql_comments`: swap comments for inert placeholders.
fn protect_sql_comments(sql: &str) -> (String, SavedComments) {
    let mut saved = SavedComments {
        comments: Vec::new(),
        nonce: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
    };
    let mut out = String::new();
    let mut pos = 0;
    while pos < sql.len() {
        let Some(c) = SQL_TOKEN.captures_at(sql, pos) else {
            break;
        };
        let m = c.get(0).expect("match");
        if c.get(2).is_some() {
            out.push_str(&sql[pos..m.start()]);
            out.push_str(&saved.save(m.as_str()));
            pos = m.end();
        } else if c.get(1).is_some() {
            let b = sql.as_bytes();
            let mut depth = 1;
            let mut j = m.end();
            while j < b.len() && depth > 0 {
                if b[j..].starts_with(b"/*") {
                    depth += 1;
                    j += 2;
                } else if b[j..].starts_with(b"*/") {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            while !sql.is_char_boundary(j) {
                j += 1;
            }
            out.push_str(&sql[pos..m.start()]);
            out.push_str(&saved.save(&sql[m.start()..j]));
            pos = j;
        } else {
            out.push_str(&sql[pos..m.end()]);
            pos = m.end();
        }
    }
    out.push_str(&sql[pos.min(sql.len())..]);
    (out, saved)
}

fn drop_comments(path_part: &str) -> String {
    split_raw(path_part, true)
        .into_iter()
        .next()
        .map(|s| s.text)
        .unwrap_or_default()
}

fn read_limited(path: &str) -> std::result::Result<String, ()> {
    let meta = std::fs::metadata(path).map_err(|_| ())?;
    if meta.len() > SIZE_LIMIT_MB * 1024 * 1024 {
        return Err(());
    }
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .map_err(|_| ())
}

fn from_file(path_part: &str, raw: &str) -> Parsed {
    let stripped = drop_comments(path_part);
    let p = std::path::Path::new(&stripped);
    if p.is_file() {
        return match read_limited(&stripped) {
            Ok(payload) => Parsed::new(&payload, Kind::File, Some(&pure_path(&stripped)), None),
            Err(()) => Parsed::new(
                path_part,
                Kind::File,
                Some(raw),
                Some(format!(
                    "Could not read: {path_part} (exceeds {SIZE_LIMIT_MB} MB limit)"
                )),
            ),
        };
    }
    Parsed::new(
        path_part,
        Kind::File,
        Some(raw),
        Some(format!("Could not read: {path_part}")),
    )
}

fn plaintext_error(url: &str) -> String {
    format!("Plaintext http:// sources are not allowed: {url}. Use https:// or a local file.")
}

/// `ParsedStatement.from_url`: https only, redirects must stay on https.
fn from_url(path_part: &str, raw: &str) -> Parsed {
    let url0 = drop_comments(path_part);
    let agent = snow_connector::tls::agent_builder()
        .redirects(0)
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let mut url = url0.clone();
    for _ in 0..10 {
        match agent.get(&url).call() {
            Ok(r) if (300..400).contains(&r.status()) => {
                let Some(loc) = r.header("location").map(str::to_string) else {
                    break;
                };
                let next = if loc.contains("://") {
                    loc
                } else {
                    url::Url::parse(&url)
                        .ok()
                        .and_then(|u| u.join(&loc).ok())
                        .map(|u| u.to_string())
                        .unwrap_or(loc)
                };
                if !next.to_lowercase().starts_with("https://") {
                    return Parsed::new(
                        path_part,
                        Kind::Unknown,
                        Some(raw),
                        Some(plaintext_error(&next)),
                    );
                }
                url = next;
            }
            Ok(r) => {
                use std::io::Read;
                let limit = SIZE_LIMIT_MB * 1024 * 1024;
                let mut buf = Vec::new();
                let _ = r.into_reader().take(limit + 1).read_to_end(&mut buf);
                if buf.len() as u64 > limit {
                    return Parsed::new(
                        path_part,
                        Kind::Url,
                        Some(raw),
                        Some(format!(
                            "Could not fetch {path_part}: payload exceeds {SIZE_LIMIT_MB} MB limit"
                        )),
                    );
                }
                return Parsed::new(&String::from_utf8_lossy(&buf), Kind::Url, Some(&url0), None);
            }
            Err(ureq::Error::Status(code, r)) => {
                let reason = r.status_text().to_string();
                return Parsed::new(
                    path_part,
                    Kind::Url,
                    Some(raw),
                    Some(format!(
                        "Could not fetch {path_part}: HTTP Error {code}: {reason}"
                    )),
                );
            }
            Err(e) => {
                return Parsed::new(
                    path_part,
                    Kind::Url,
                    Some(raw),
                    Some(format!("Could not fetch {path_part}: <urlopen error {e}>")),
                );
            }
        }
    }
    Parsed::new(
        path_part,
        Kind::Url,
        Some(raw),
        Some(format!("Could not fetch {path_part}: too many redirects")),
    )
}

const REGISTERED: &[&str] = &[
    "queries", "result", "abort", "set", "spool", "edit", "rehash",
];

/// `parse_statement(..., load_sources=False)`.
fn parse_statement(
    source: &str,
    r: &Renderer,
    console: &mut Console,
    local_only: bool,
) -> Result<Parsed> {
    let statement = match r.operators(console, source) {
        Ok(s) => s,
        Err(RenderError::Undefined(m)) => {
            return Ok(Parsed::new(
                source,
                Kind::Unknown,
                Some(source),
                Some(format!("SQL template rendering error: {m}")),
            ));
        }
        Err(RenderError::Fatal(e)) => return Err(e),
    };
    let Some(c) = COMMAND.captures(&statement) else {
        return Ok(Parsed::new(&statement, Kind::Query, None, None));
    };
    let command = c[1].trim().to_string();
    let args = c[2].trim().to_string();
    let lower_args = args.to_lowercase();
    let scheme = URL.captures(&lower_args).map(|m| m[1].to_string());
    let cmd = command.to_lowercase();
    if cmd == "source" || cmd == "load" {
        return Ok(match scheme.as_deref() {
            Some("http") => Parsed::new(
                &statement,
                Kind::Unknown,
                Some(&args),
                Some(plaintext_error(&args)),
            ),
            Some("https") if local_only => Parsed::new(
                &args,
                Kind::Url,
                Some(&statement),
                Some(format!(
                    "Loading SQL from URLs is disabled (--local-only): {}",
                    drop_comments(&args)
                )),
            ),
            Some("https") => Parsed::new("", Kind::Url, Some(&drop_comments(&args)), None),
            None => Parsed::new(
                "",
                Kind::File,
                Some(&pure_path(&drop_comments(&args))),
                None,
            ),
            Some(_) => Parsed::new(
                &statement,
                Kind::Unknown,
                Some(&args),
                Some(format!("Unknown source: {args}")),
            ),
        });
    }
    if scheme.is_none() && cmd == "edit" {
        return Ok(Parsed::new(
            &statement,
            Kind::ReplCommand,
            Some(&args),
            None,
        ));
    }
    if scheme.is_none() && REGISTERED.contains(&cmd.as_str()) {
        return Ok(Parsed::new(&statement, Kind::ReplCommand, None, None));
    }
    Ok(Parsed::new(
        &statement,
        Kind::Unknown,
        None,
        Some(format!("Unknown command: {command}")),
    ))
}

pub struct Reader<'a> {
    pub renderer: Renderer<'a>,
    pub remove_comments: bool,
    pub local_only: bool,
    pub out: Vec<Parsed>,
}

impl Reader<'_> {
    fn recurse(
        &mut self,
        console: &mut Console,
        stmts: Vec<Statement>,
        seen: &mut Vec<String>,
        depth: usize,
        includes: &mut usize,
    ) -> Result<()> {
        for st in stmts {
            if st.text.is_empty() {
                continue;
            }
            let mut parsed = parse_statement(&st.text, &self.renderer, console, self.local_only)?;
            if !matches!(parsed.kind, Kind::File | Kind::Url) || parsed.error.is_some() {
                self.out.push(parsed);
                continue;
            }
            let src = parsed.source_path.clone().unwrap_or_default();
            let identity = if parsed.kind == Kind::File {
                crate::paths::resolve(&src).to_string_lossy().into_owned()
            } else {
                src.clone()
            };
            if seen.contains(&identity) {
                parsed.error = Some(format!("Recursion detected: {}", seen.join(" -> ")));
                self.out.push(parsed);
                continue;
            }
            if depth >= MAX_DEPTH {
                parsed.error = Some(format!(
                    "Maximum !source nesting depth ({MAX_DEPTH}) exceeded"
                ));
                self.out.push(parsed);
                continue;
            }
            if *includes >= MAX_INCLUDES {
                parsed.error = Some(format!(
                    "Maximum number of !source/!load inclusions ({MAX_INCLUDES}) exceeded"
                ));
                self.out.push(parsed);
                continue;
            }
            *includes += 1;
            let loaded = if parsed.kind == Kind::Url {
                from_url(&src, &st.text)
            } else {
                from_file(&src, &st.text)
            };
            if loaded.error.is_some() {
                self.out.push(loaded);
                continue;
            }
            seen.push(identity);
            let content = self.renderer.pre_render(console, &loaded.statement)?;
            let inner = split_raw(&content, self.remove_comments);
            self.recurse(console, inner, seen, depth + 1, includes)?;
            seen.pop();
        }
        Ok(())
    }

    /// `query_reader`.
    pub fn read_query(&mut self, console: &mut Console, query: &str) -> Result<()> {
        let content = self.renderer.pre_render(console, query)?;
        let stmts = split_raw(&content, self.remove_comments);
        self.recurse(console, stmts, &mut Vec::new(), 0, &mut 0)
    }

    /// `files_reader`.
    pub fn read_files(&mut self, console: &mut Console, files: &[String]) -> Result<()> {
        for f in files {
            let Ok(content) = read_limited(f) else {
                self.out.push(Parsed::new(
                    "",
                    Kind::File,
                    Some(&pure_path(f)),
                    Some(format!(
                        "Could not read: {} (exceeds {SIZE_LIMIT_MB} MB limit)",
                        pure_path(f)
                    )),
                ));
                continue;
            };
            let content = self.renderer.pre_render(console, &content)?;
            let stmts = split_raw(&content, self.remove_comments);
            let mut seen = vec![crate::paths::resolve(f).to_string_lossy().into_owned()];
            self.recurse(console, stmts, &mut seen, 0, &mut 0)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum Compiled {
    Sql(String),
    /// A statement that ended with `;>`.
    Async(String),
    Command(ReplCommand),
}

fn is_empty_statement(s: &str) -> bool {
    s.lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with("--"))
}

/// `compile_statements`: (errors, expected result count, statements).
pub fn compile_statements(parsed: Vec<Parsed>) -> (Vec<String>, usize, Vec<Compiled>) {
    let mut errors = Vec::new();
    let mut count = 0;
    let mut compiled = Vec::new();
    for p in parsed {
        if p.kind == Kind::Query && p.error.is_none() && !is_empty_statement(&p.statement) {
            match p.statement.strip_suffix(ASYNC_SUFFIX) {
                Some(s) => compiled.push(Compiled::Async(s.to_string())),
                None => {
                    compiled.push(Compiled::Sql(p.statement.clone()));
                    count += 1;
                }
            }
        }
        if p.kind == Kind::ReplCommand && p.error.is_none() {
            let t = p
                .statement
                .strip_suffix(ASYNC_SUFFIX)
                .unwrap_or(&p.statement);
            let t = t.strip_suffix(';').unwrap_or(t).trim();
            match compile_repl_command(t) {
                Ok(cmd) => compiled.push(Compiled::Command(cmd)),
                Err(e) => errors.push(e),
            }
        }
        if let Some(e) = p.error {
            errors.push(e);
        }
    }
    (errors, count, compiled)
}
