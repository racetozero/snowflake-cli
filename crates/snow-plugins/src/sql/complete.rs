//! REPL completion and highlighting (`_plugins/sql/completion`, `lexer`): SQL
//! keywords and functions, types after `::`, and catalog names after FROM / JOIN /
//! INTO / UPDATE / TABLE / USE, looked up with the same scoped SHOW queries as
//! upstream (`build_show_sql`) and cached until `!rehash`.

use snow_api::Cli;
use snow_api::identifiers::{
    escape_like_pattern, is_valid_unquoted_identifier, to_quoted_identifier, to_string_literal,
    unquote_identifier,
};
use std::cell::RefCell;
use std::collections::HashMap;

const WORDS: &str = include_str!("../../data/sql_words.txt");

fn section(name: &str) -> Vec<&'static str> {
    let mut on = false;
    let mut out = Vec::new();
    for l in WORDS.lines() {
        if l.starts_with('#') {
            continue;
        }
        if let Some(n) = l.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            on = n == name;
            continue;
        }
        if on && !l.is_empty() {
            out.push(l);
        }
    }
    out
}

thread_local! {
    static KEYWORDS: Vec<&'static str> = section("keywords");
    static FUNCTIONS: Vec<&'static str> = section("functions");
    static TYPES: Vec<&'static str> = section("types");
    /// SHOW statement -> names, until `!rehash`.
    static CACHE: RefCell<HashMap<String, Vec<String>>> = RefCell::new(HashMap::new());
}

/// `Repl.refresh_completion_catalog()`: drop the object snapshots.
pub fn clear() {
    CACHE.with(|c| c.borrow_mut().clear());
}

pub fn is_keyword(word: &str) -> bool {
    let up = word.to_uppercase();
    KEYWORDS.with(|k| k.binary_search(&up.as_str()).is_ok() || k.contains(&up.as_str()))
}

/// Match `prefix` case-insensitively, keeping the user's case style.
fn matching(words: &[&str], prefix: &str) -> Vec<String> {
    let up = prefix.to_uppercase();
    let lower = !prefix.is_empty() && prefix.chars().all(|c| !c.is_uppercase());
    words
        .iter()
        .filter(|w| w.starts_with(&up))
        .map(|w| {
            if lower {
                w.to_lowercase()
            } else {
                w.to_string()
            }
        })
        .collect()
}

fn show(cli: &mut Cli, sql: &str) -> Vec<String> {
    if let Some(v) = CACHE.with(|c| c.borrow().get(sql).cloned()) {
        return v;
    }
    let names: Vec<String> = match cli.try_query(sql) {
        Ok(Ok(c)) => {
            let i = c
                .columns
                .iter()
                .position(|col| col.name.eq_ignore_ascii_case("name"));
            c.rows
                .iter()
                .filter_map(|r| i.and_then(|i| r.get(i).cloned().flatten()))
                .collect()
        }
        _ => vec![],
    };
    CACHE.with(|c| c.borrow_mut().insert(sql.to_string(), names.clone()));
    names
}

fn shown(name: &str) -> String {
    if is_valid_unquoted_identifier(name) && name == name.to_uppercase() {
        name.to_string()
    } else {
        to_quoted_identifier(name)
    }
}

fn like(prefix: &str) -> String {
    to_string_literal(&format!(
        "{}%",
        escape_like_pattern(&unquote_identifier(prefix))
    ))
}

/// Candidates for the word that ends at the end of `before` (the text left of the
/// cursor): `(start of that word, replacements)`.
pub fn candidates(cli: &mut Cli, before: &str) -> (usize, Vec<String>) {
    let start = before
        .rfind(|c: char| c.is_whitespace() || "(),;=<>+*/".contains(c))
        .map(|i| i + 1)
        .unwrap_or(0);
    let word = &before[start..];
    if word.starts_with('!') {
        let cmds = [
            "!abort", "!edit", "!load", "!queries", "!rehash", "!result", "!set", "!source",
            "!spool",
        ];
        return (
            start,
            cmds.iter()
                .filter(|c| c.starts_with(word))
                .map(|c| c.to_string())
                .collect(),
        );
    }
    if let Some(i) = word.find("::") {
        let t = &word[i + 2..];
        return (start + i + 2, TYPES.with(|ts| matching(ts, t)));
    }
    let prev: Vec<String> = before[..start]
        .split_whitespace()
        .rev()
        .take(2)
        .map(|w| w.to_uppercase())
        .collect();
    let p0 = prev.first().map(String::as_str).unwrap_or("");
    let p1 = prev.get(1).map(String::as_str).unwrap_or("");
    let ident_slot = matches!(
        p0,
        "FROM"
            | "JOIN"
            | "INTO"
            | "UPDATE"
            | "TABLE"
            | "VIEW"
            | "DESCRIBE"
            | "DESC"
            | "USE"
            | "DATABASE"
            | "SCHEMA"
    );
    if ident_slot && !word.is_empty() {
        let parts: Vec<&str> = word.split('.').collect();
        let (session_db, session_schema) = match cli.connection() {
            Ok(c) => (c.session.database.clone(), c.session.schema.clone()),
            Err(_) => (None, None),
        };
        let last = parts.last().copied().unwrap_or("");
        let head = &word[..word.len() - last.len()];
        let names = match parts.len() {
            1 if p0 == "DATABASE" || (p0 == "USE" && p1 != "SCHEMA") => {
                show(cli, &format!("SHOW DATABASES LIKE {}", like(last)))
            }
            1 if p0 == "SCHEMA" => match &session_db {
                Some(db) => show(
                    cli,
                    &format!(
                        "SHOW SCHEMAS LIKE {} IN DATABASE {}",
                        like(last),
                        to_quoted_identifier(db)
                    ),
                ),
                None => vec![],
            },
            1 => match (&session_db, &session_schema) {
                (Some(db), Some(sc)) => show(
                    cli,
                    &format!(
                        "SHOW OBJECTS LIKE {} IN SCHEMA {}.{}",
                        like(last),
                        to_quoted_identifier(db),
                        to_quoted_identifier(sc)
                    ),
                ),
                _ => vec![],
            },
            2 => show(
                cli,
                &format!(
                    "SHOW SCHEMAS LIKE {} IN DATABASE {}",
                    like(last),
                    to_quoted_identifier(&unquote_identifier(parts[0]))
                ),
            ),
            3 => show(
                cli,
                &format!(
                    "SHOW OBJECTS LIKE {} IN SCHEMA {}.{}",
                    like(last),
                    to_quoted_identifier(&unquote_identifier(parts[0])),
                    to_quoted_identifier(&unquote_identifier(parts[1]))
                ),
            ),
            _ => vec![],
        };
        let mut out: Vec<String> = names
            .iter()
            .map(|n| format!("{head}{}", shown(n)))
            .collect();
        out.sort();
        out.dedup();
        if !out.is_empty() {
            return (start, out);
        }
    }
    if word.is_empty() {
        return (start, vec![]);
    }
    let mut out = KEYWORDS.with(|k| matching(k, word));
    for f in FUNCTIONS.with(|f| matching(f, word)) {
        if !out.contains(&f) {
            out.push(f);
        }
    }
    (start, out)
}

/// ANSI colouring of a REPL buffer: keywords, strings, numbers and comments.
pub fn highlight(text: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            let rest: String = chars[i..].iter().collect();
            out.push_str(&format!("\x1b[90m{rest}\x1b[0m"));
            break;
        }
        if c == '\'' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '\'' {
                j += 1;
            }
            let end = (j + 1).min(chars.len());
            out.push_str(&format!(
                "\x1b[32m{}\x1b[0m",
                chars[i..end].iter().collect::<String>()
            ));
            i = end;
            continue;
        }
        if c.is_ascii_digit()
            && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_'))
        {
            let mut j = i;
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == '.') {
                j += 1;
            }
            out.push_str(&format!(
                "\x1b[36m{}\x1b[0m",
                chars[i..j].iter().collect::<String>()
            ));
            i = j;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '$')
            {
                j += 1;
            }
            let w: String = chars[i..j].iter().collect();
            if is_keyword(&w) {
                out.push_str(&format!("\x1b[1;34m{w}\x1b[0m"));
            } else {
                out.push_str(&w);
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_load_and_highlight() {
        assert!(is_keyword("select"));
        assert!(!is_keyword("my_table"));
        assert_eq!(
            highlight("select 1 -- x"),
            "\x1b[1;34mselect\x1b[0m \x1b[36m1\x1b[0m \x1b[90m-- x\x1b[0m"
        );
        assert_eq!(matching(&["SELECT", "SET"], "se"), vec!["select", "set"]);
    }
}
