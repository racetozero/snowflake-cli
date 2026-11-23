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

/// A stage path (`@db.schema.stage/dir/`) split into `prefix`, `f`, `s`,
/// `name` and `directory`.
pub static STAGE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    let v = &*VALID_ID;
    Regex::new(&format!(
        r"^(?P<prefix>(@|snow://))?(?:(?P<f>{v})\.)?(?:(?P<s>{v})\.)?(?P<name>{v})/?(?P<directory>([^/]*/?)*)?$"
    ))
    .expect("re")
});

/// The byte length of the `UNQUOTED` (or, when `allow_quoted`, `QUOTED`)
/// identifier that starts `s`, for checks anchored at both ends. Scanning
/// replaces the regexes because their counted repetitions take about 1 ms each
/// to compile, and every `snow` process paid that again.
fn identifier_len(s: &str, allow_quoted: bool) -> Option<usize> {
    let b = s.as_bytes();
    if allow_quoted && b.first() == Some(&b'"') {
        let mut i = 1;
        let mut chars = 0;
        loop {
            match b.get(i)? {
                b'"' if b.get(i + 1) == Some(&b'"') => i += 2,
                b'"' => return (chars <= 255).then_some(i + 1),
                _ => i += s[i..].chars().next()?.len_utf8(),
            }
            chars += 1;
        }
    }
    if !b
        .first()
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
    {
        return None;
    }
    let n = 1 + b[1..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_' || **c == b'$')
        .count();
    (n <= 255).then_some(n)
}

pub fn is_valid_unquoted_identifier(s: &str) -> bool {
    identifier_len(s, false) == Some(s.len())
}
pub fn is_valid_quoted_identifier(s: &str) -> bool {
    s.starts_with('"') && identifier_len(s, true) == Some(s.len())
}
pub fn is_valid_identifier(s: &str) -> bool {
    identifier_len(s, true) == Some(s.len())
}

pub fn is_valid_object_name(name: &str, max_depth: usize, allow_quoted: bool) -> bool {
    let mut rest = name;
    for _ in 0..=max_depth {
        let Some(n) = identifier_len(rest, allow_quoted) else {
            return false;
        };
        match rest[n..].strip_prefix('.') {
            Some(next) => rest = next,
            None => return n == rest.len(),
        }
    }
    false
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

pub(crate) fn identifier_to_str(s: &str) -> String {
    if is_valid_quoted_identifier(s) {
        s[1..s.len() - 1].replace("\"\"", "\"")
    } else {
        s.to_string()
    }
}

pub fn unquote_identifier(s: &str) -> String {
    let id = to_identifier(s);
    if is_valid_quoted_identifier(&id) {
        return identifier_to_str(&id);
    }
    id.to_uppercase()
}

static DOTTED_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^(?:"(?:[^"]|"")*"|[^."]+)(?:\.(?:"(?:[^"]|"")*"|[^."]+))*$"#).expect("re")
});
static NAME_PART: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(?:[^"]|"")*"|[^."]+"#).expect("re"));

/// `_sql_object_name`: quote each part of a (possibly dotted) name.
pub fn sql_object_name(name: &str) -> String {
    if !DOTTED_NAME.is_match(name) {
        return to_identifier(name);
    }
    NAME_PART
        .find_iter(name)
        .map(|m| to_identifier(m.as_str()))
        .collect::<Vec<_>>()
        .join(".")
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
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\\*)'").expect("re"));
    let mut escaped = RE
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
    let canonical = |s: &str| {
        to_quoted_identifier(&if is_valid_unquoted_identifier(s) {
            s.to_uppercase()
        } else {
            s.to_string()
        })
    };
    canonical(a) == canonical(b)
}

pub(crate) fn concat_identifiers(ids: &[&str]) -> String {
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

pub(crate) fn fqn_name_error(name: &str) -> SnowError {
    SnowError::cli(format!("Specified name '{name}' is not valid name.")).named("FQNNameError")
}

impl Fqn {
    pub fn from_string(s: &str) -> Result<Fqn> {
        let mut parts: Vec<&str> = Vec::with_capacity(3);
        let mut rest = s;
        loop {
            let n = identifier_len(rest, true).ok_or_else(|| fqn_name_error(s))?;
            parts.push(&rest[..n]);
            rest = &rest[n..];
            match rest.strip_prefix('.') {
                Some(next) if parts.len() < 3 => rest = next,
                _ => break,
            }
        }
        let signature_char =
            |c: char| c.is_ascii_alphanumeric() || "_$(),.".contains(c) || c.is_whitespace();
        let signature = match rest {
            "" => None,
            sig if sig.len() >= 2
                && sig.starts_with('(')
                && sig.ends_with(')')
                && sig[1..sig.len() - 1].chars().all(signature_char) =>
            {
                Some(sig.to_string())
            }
            _ => return Err(fqn_name_error(s)),
        };
        let name = parts.pop().unwrap_or_default().to_string();
        let mut qualifiers = parts.into_iter().map(str::to_string);
        let (first, second) = (qualifiers.next(), qualifiers.next());
        let (database, schema) = if second.is_some() {
            (first, second)
        } else {
            (None, first)
        };
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
        let name = stage
            .strip_prefix('@')
            .or_else(|| stage.strip_prefix("snow://"))
            .unwrap_or(stage);
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
        format!(
            "IDENTIFIER({}){}",
            to_string_literal(&self.identifier()),
            self.signature.as_deref().unwrap_or("")
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The scanner gives the same answers as the upstream regexes it replaced.
    #[test]
    fn scanner_matches_the_upstream_regexes() {
        let v = &*VALID_ID;
        let full = |p: &str| Regex::new(&format!("^(?:{p})$")).expect("re");
        let (unquoted, quoted) = (full(UNQUOTED), full(QUOTED));
        let fqn = Regex::new(&format!(
            r"^(?:(?P<first>{v})\.)?(?:(?P<second>{v})\.)?(?P<name>{v})(?P<signature>\([A-Za-z0-9_$(),.\s]*\))?$"
        ))
        .expect("re");
        let object_name: Vec<(usize, bool, Regex)> = (0..3)
            .flat_map(|d| [(d, true), (d, false)])
            .map(|(d, q)| {
                let p = if q {
                    v.clone()
                } else {
                    format!("(?:{UNQUOTED})")
                };
                (d, q, full(&format!(r"{p}(?:\.{p}){{0,{d}}}")))
            })
            .collect();

        let alphabet = ['a', 'Z', '_', '$', '1', '"', '.', '(', ')', ' ', 'é', ','];
        let mut cases = vec![String::new()];
        let mut level = vec![String::new()];
        for _ in 0..4 {
            level = level
                .iter()
                .flat_map(|s| alphabet.iter().map(move |c| format!("{s}{c}")))
                .collect();
            cases.extend(level.iter().cloned());
        }
        for n in [254, 255, 256] {
            cases.push(format!("a{}", "b".repeat(n - 1)));
            cases.push(format!("\"{}\"", "é".repeat(n)));
            cases.push(format!("\"{}\"", "\"\"".repeat(n)));
            cases.push(format!("db.\"{}\".t(x)", "é".repeat(n)));
        }

        for s in &cases {
            assert_eq!(is_valid_unquoted_identifier(s), unquoted.is_match(s), "{s}");
            assert_eq!(is_valid_quoted_identifier(s), quoted.is_match(s), "{s}");
            for (d, q, re) in &object_name {
                assert_eq!(
                    is_valid_object_name(s, *d, *q),
                    re.is_match(s),
                    "{s} {d} {q}"
                );
            }
            let expected = fqn.captures(s).map(|c| {
                let g = |n: &str| c.name(n).map(|m| m.as_str().to_string());
                let (database, schema) = match g("second") {
                    Some(second) => (g("first"), Some(second)),
                    None => (None, g("first")),
                };
                (
                    database,
                    schema,
                    g("name").unwrap_or_default(),
                    g("signature"),
                )
            });
            let actual = Fqn::from_string(s).ok();
            match (&expected, &actual) {
                (Some(e), Some(a)) => {
                    assert_eq!(
                        e,
                        &(
                            a.database.clone(),
                            a.schema.clone(),
                            a.name.clone(),
                            a.signature.clone()
                        ),
                        "{s}"
                    )
                }
                // The signature's bracket check rejects names whose brackets do
                // not nest to one group that closes at the end.
                (Some(e), None) => {
                    let sig = e.3.as_deref().unwrap_or_else(|| panic!("{s}"));
                    let depths: Vec<i32> = sig
                        .chars()
                        .scan(0, |d, c| {
                            *d += i32::from(c == '(') - i32::from(c == ')');
                            Some(*d)
                        })
                        .collect();
                    let (last, inner) = depths.split_last().expect("non-empty signature");
                    assert!(*last != 0 || inner.iter().any(|d| *d <= 0), "{s}");
                }
                (None, a) => assert!(a.is_none(), "{s}"),
            }
        }
    }
}
