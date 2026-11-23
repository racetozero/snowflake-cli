//! Identifier helpers (`api/identifiers.py`, `api/project/util.py`).

use crate::errors::{Result, SnowError};
use regex::Regex;
use std::sync::LazyLock;

const UNQUOTED: &str = r"([a-zA-Z_])([a-zA-Z0-9_$]{0,254})";
const QUOTED: &str = r#""((""|[^"]){0,255})""#;

static VALID_ID: LazyLock<String> = LazyLock::new(|| format!("(?:{UNQUOTED}|{QUOTED})"));

/// `VALID_IDENTIFIER_REGEX` (unanchored).
pub fn valid_identifier_regex() -> &'static str {
    &VALID_ID
}
static RE_UNQUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^(?:{UNQUOTED})$")).expect("re"));
static RE_QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^(?:{QUOTED})$")).expect("re"));
static RE_FQN: LazyLock<Regex> = LazyLock::new(|| {
    let v = &*VALID_ID;
    Regex::new(&format!(
        r"^(?:(?P<first>{v})\.)?(?:(?P<second>{v})\.)?(?P<name>{v})(?P<signature>\([A-Za-z0-9_$(),.\s]*\))?$"
    ))
    .expect("re")
});

pub fn is_valid_unquoted_identifier(s: &str) -> bool {
    RE_UNQUOTED.is_match(s)
}
pub fn is_valid_quoted_identifier(s: &str) -> bool {
    RE_QUOTED.is_match(s)
}
pub fn is_valid_identifier(s: &str) -> bool {
    is_valid_unquoted_identifier(s) || is_valid_quoted_identifier(s)
}

pub fn is_valid_object_name(name: &str, max_depth: usize, allow_quoted: bool) -> bool {
    let p = if allow_quoted {
        VALID_ID.clone()
    } else {
        format!("(?:{UNQUOTED})")
    };
    Regex::new(&format!(r"^{p}(?:\.{p}){{0,{max_depth}}}$")).is_ok_and(|r| r.is_match(name))
}

pub fn to_quoted_identifier(s: &str) -> String {
    if is_valid_quoted_identifier(s) {
        return s.to_string();
    }
    format!("\"{}\"", s.replace('"', "\"\""))
}

pub fn to_identifier(s: &str) -> String {
    if is_valid_identifier(s) {
        s.to_string()
    } else {
        to_quoted_identifier(s)
    }
}

pub fn identifier_to_str(s: &str) -> String {
    if is_valid_quoted_identifier(s) {
        s[1..s.len() - 1].replace("\"\"", "\"")
    } else {
        s.to_string()
    }
}

pub fn unquote_identifier(s: &str) -> String {
    let id = to_identifier(s);
    if is_valid_quoted_identifier(&id) {
        return id[1..id.len() - 1].replace("\"\"", "\"");
    }
    id.to_uppercase()
}

/// JavaScript `encodeURIComponent` (`quote(s, safe="!~*'()")`).
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "_.-~!*'()".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn identifier_for_url(s: &str) -> String {
    encode_uri_component(&unquote_identifier(s))
}

/// `to_string_literal`.
pub fn to_string_literal(raw: &str) -> String {
    let re = Regex::new(r"(\\*)'").expect("re");
    let mut escaped = re
        .replace_all(raw, |c: &regex::Captures| format!("{}''", c[1].repeat(2)))
        .into_owned();
    let trailing = escaped.len() - escaped.trim_end_matches('\\').len();
    if trailing % 2 == 1 {
        escaped.push('\\');
    }
    format!("'{escaped}'")
}

pub fn escape_like_pattern(p: &str) -> String {
    p.replace('%', "\\\\%").replace('_', "\\\\_")
}

pub fn identifier_to_show_like_pattern(id: &str) -> String {
    to_string_literal(&escape_like_pattern(&unquote_identifier(id)))
}

pub fn sanitize_identifier(input: &str) -> String {
    let mut v: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    if v.is_empty()
        || !v
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        v.insert(0, '_');
    }
    v.chars().take(255).collect()
}

pub fn same_identifiers(a: &str, b: &str) -> bool {
    let ca = if is_valid_unquoted_identifier(a) {
        a.to_uppercase()
    } else {
        a.to_string()
    };
    let cb = if is_valid_unquoted_identifier(b) {
        b.to_uppercase()
    } else {
        b.to_string()
    };
    to_quoted_identifier(&ca) == to_quoted_identifier(&cb)
}

pub fn concat_identifiers(ids: &[&str]) -> String {
    let quotes = ids.iter().any(|i| is_valid_quoted_identifier(i));
    let s: String = ids.iter().map(|i| identifier_to_str(i)).collect();
    if quotes {
        to_quoted_identifier(&s)
    } else {
        to_identifier(&s)
    }
}

pub fn append_to_identifier(id: &str, suffix: &str) -> String {
    if is_valid_unquoted_identifier(id) {
        to_identifier(&format!("{id}{suffix}"))
    } else {
        format!("\"{}{suffix}\"", &id[1..id.len() - 1])
    }
}

/// `sql_match(pattern=..., value=...)`.
pub fn sql_match(pattern: &str, value: &str) -> bool {
    let v = unquote_identifier(value);
    let p = regex::escape(pattern).replace('%', ".*").replace('_', ".");
    Regex::new(&format!("(?i)^{p}$")).is_ok_and(|r| r.is_match(&v))
}

/// `FQN`: a possibly qualified object name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Fqn {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    pub signature: Option<String>,
}

pub fn fqn_name_error(name: &str) -> SnowError {
    SnowError::cli(format!("Specified name '{name}' is not valid name.")).named("FQNNameError")
}

impl Fqn {
    pub fn from_string(s: &str) -> Result<Fqn> {
        let Some(c) = RE_FQN.captures(s) else {
            return Err(fqn_name_error(s));
        };
        let name = c
            .name("name")
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let first = c.name("first").map(|m| m.as_str().to_string());
        let second = c.name("second").map(|m| m.as_str().to_string());
        let (database, schema) = if second.is_some() {
            (first, second)
        } else {
            (None, first)
        };
        let signature = c.name("signature").map(|m| m.as_str().to_string());
        if let Some(sig) = &signature {
            let mut depth: i32 = 0;
            let n = sig.chars().count();
            for (i, ch) in sig.chars().enumerate() {
                if ch == '(' {
                    depth += 1;
                } else if ch == ')' {
                    depth -= 1;
                    if depth < 0 || (depth == 0 && i != n - 1) {
                        return Err(fqn_name_error(s));
                    }
                }
            }
            if depth != 0 {
                return Err(fqn_name_error(s));
            }
        }
        Ok(Fqn {
            database,
            schema,
            name,
            signature,
        })
    }

    /// `FQN.from_stage_path`: the first path component.
    pub fn from_stage_path(p: &str) -> Result<Fqn> {
        let first = crate::stage_path::posix_parts(p)
            .into_iter()
            .next()
            .unwrap_or_default();
        Fqn::from_stage(&first)
    }

    pub fn from_stage(stage: &str) -> Result<Fqn> {
        let mut name = stage;
        if let Some(n) = stage.strip_prefix('@') {
            name = n;
        }
        if let Some(n) = stage.strip_prefix("snow://") {
            name = n;
        }
        if stage.starts_with('~') {
            return Ok(Fqn {
                name: "~".into(),
                ..Fqn::default()
            });
        }
        Fqn::from_string(name)
    }

    pub fn new(name: &str) -> Fqn {
        Fqn {
            name: name.to_string(),
            ..Fqn::default()
        }
    }

    pub fn prefix(&self) -> String {
        if let Some(d) = &self.database {
            return format!("{d}.{}", self.schema.as_deref().unwrap_or("PUBLIC"));
        }
        self.schema.clone().unwrap_or_default()
    }

    pub fn identifier(&self) -> String {
        let p = self.prefix();
        if p.is_empty() {
            self.name.clone()
        } else {
            format!("{p}.{}", self.name)
        }
    }

    pub fn sql_identifier(&self) -> String {
        match &self.signature {
            Some(s) => format!("IDENTIFIER({}){s}", to_string_literal(&self.identifier())),
            None => format!("IDENTIFIER({})", to_string_literal(&self.identifier())),
        }
    }

    pub fn url_identifier(&self) -> String {
        self.identifier()
            .split('.')
            .map(identifier_for_url)
            .collect::<Vec<_>>()
            .join(".")
    }

    pub fn set_database(mut self, db: Option<&str>) -> Fqn {
        if let Some(d) = db.filter(|d| !d.is_empty()) {
            self.database = Some(d.to_string());
        }
        self
    }
    pub fn set_schema(mut self, s: Option<&str>) -> Fqn {
        if let Some(d) = s.filter(|d| !d.is_empty()) {
            self.schema = Some(d.to_string());
        }
        self
    }

    /// `using_connection`: fill database and schema from the session.
    pub fn using_session(
        mut self,
        database: Option<&str>,
        schema: Option<&str>,
        user: Option<&str>,
    ) -> Fqn {
        if let (Some(d), Some(u)) = (&self.database, user)
            && unquote_identifier(d) == "USER$"
        {
            self.database = Some(to_identifier(&format!("USER${u}")));
        }
        if self.database.is_none() {
            self = self.set_database(database);
        }
        if self.schema.is_none() {
            self = self.set_schema(schema);
        }
        self
    }
}

impl std::fmt::Display for Fqn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.identifier())
    }
}

static RE_STRING_LITERAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^'((?:''|[^'\n])*)'$").expect("re"));

pub fn is_valid_string_literal(s: &str) -> bool {
    RE_STRING_LITERAL.is_match(s)
}
