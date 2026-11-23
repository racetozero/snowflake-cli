//! PyYAML compatibility: `yaml.BaseLoader` loading (every scalar is a string,
//! duplicate keys rejected, `!override`) and `yaml.safe_dump(sort_keys=False,
//! width=inf)` output.

use super::value::PyV;
use indexmap::IndexMap;
use yaml_rust2::parser::{Event, Parser};

#[derive(Debug)]
pub enum LoadError {
    Duplicate(String),
    Syntax(String),
}

struct Loader<'a> {
    parser: Parser<std::str::Chars<'a>>,
    anchors: std::collections::HashMap<usize, PyV>,
    /// `yaml.SafeLoader`: resolve plain scalars to null/bool/int/float.
    safe: bool,
}

/// `SafeLoader` implicit resolution of a plain scalar.
fn resolve_plain(s: &str) -> PyV {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return PyV::None,
        "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => {
            return PyV::Bool(true);
        }
        "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => {
            return PyV::Bool(false);
        }
        _ => {}
    }
    let t = s.replace('_', "");
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b.to_string()),
        None => (false, t.strip_prefix('+').unwrap_or(&t).to_string()),
    };
    let int = if let Some(b) = body.strip_prefix("0b") {
        i64::from_str_radix(b, 2).ok()
    } else if let Some(h) = body.strip_prefix("0x") {
        i64::from_str_radix(h, 16).ok()
    } else if body.len() > 1 && body.starts_with('0') && body.chars().all(|c| c.is_ascii_digit()) {
        i64::from_str_radix(&body[1..], 8).ok()
    } else if !body.is_empty() && body.chars().all(|c| c.is_ascii_digit()) {
        body.parse().ok()
    } else {
        None
    };
    if let Some(i) = int.filter(|_| NON_STR.is_match(s)) {
        return PyV::Int(if neg { -i } else { i });
    }
    let fl = regex::Regex::new(
        r"^[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?$|^\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?$",
    )
    .expect("re");
    if fl.is_match(s)
        && let Ok(f) = t.parse::<f64>()
    {
        return PyV::Float(f);
    }
    let ts = regex::Regex::new(
        r"^([0-9]{4})-([0-9]{1,2})-([0-9]{1,2})(?:(?:[Tt]|[ \t]+)([0-9]{1,2}):([0-9]{2}):([0-9]{2})(\.[0-9]*)?(?:[ \t]*(Z|[-+][0-9]{1,2}(?::[0-9]{2})?))?)?$",
    )
    .expect("re");
    if let Some(c) = ts.captures(s) {
        let d = format!("{}-{:0>2}-{:0>2}", &c[1], &c[2], &c[3]);
        let Some(h) = c.get(4) else {
            return PyV::Timestamp(d);
        };
        let mut t = format!("{d} {:0>2}:{}:{}", h.as_str(), &c[5], &c[6]);
        if let Some(f) = c
            .get(7)
            .map(|m| m.as_str().trim_start_matches('.'))
            .filter(|f| !f.is_empty())
        {
            let us: String = format!("{f:0<6}").chars().take(6).collect();
            if us != "000000" {
                t.push('.');
                t.push_str(&us);
            }
        }
        if let Some(z) = c.get(8) {
            let z = z.as_str();
            t.push_str(&if z == "Z" {
                "+00:00".to_string()
            } else {
                let (sign, rest) = z.split_at(1);
                let (hh, mm) = rest.split_once(':').unwrap_or((rest, "00"));
                format!("{sign}{hh:0>2}:{mm}")
            });
        }
        return PyV::Timestamp(t);
    }
    match s {
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => PyV::Float(f64::INFINITY),
        "-.inf" | "-.Inf" | "-.INF" => PyV::Float(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => PyV::Float(f64::NAN),
        _ => PyV::Str(s.to_string()),
    }
}

impl Loader<'_> {
    fn next(&mut self) -> Result<Event, LoadError> {
        self.parser
            .next_token()
            .map(|(e, _)| e)
            .map_err(|e| LoadError::Syntax(e.to_string()))
    }

    fn node(&mut self, ev: Event) -> Result<PyV, LoadError> {
        let (v, anchor) = match ev {
            Event::Scalar(s, style, anchor, tag) => {
                let plain =
                    matches!(style, yaml_rust2::scanner::TScalarStyle::Plain) && tag.is_none();
                (
                    if self.safe && plain {
                        resolve_plain(&s)
                    } else {
                        PyV::Str(s)
                    },
                    anchor,
                )
            }
            Event::Alias(id) => return Ok(self.anchors.get(&id).cloned().unwrap_or(PyV::None)),
            Event::SequenceStart(anchor, tag) => {
                let mut items = Vec::new();
                loop {
                    match self.next()? {
                        Event::SequenceEnd => break,
                        e => items.push(self.node(e)?),
                    }
                }
                let v = PyV::List(items);
                (
                    if is_override(&tag) {
                        PyV::Override(Box::new(v))
                    } else {
                        v
                    },
                    anchor,
                )
            }
            Event::MappingStart(anchor, tag) => {
                let mut map = IndexMap::new();
                loop {
                    let k = match self.next()? {
                        Event::MappingEnd => break,
                        e => self.node(e)?,
                    };
                    let e = self.next()?;
                    let v = self.node(e)?;
                    let key = k.py_str();
                    if map.contains_key(&key) && !self.safe {
                        return Err(LoadError::Duplicate(key));
                    }
                    map.insert(key, v);
                }
                let v = PyV::Dict(map);
                (
                    if is_override(&tag) {
                        PyV::Override(Box::new(v))
                    } else {
                        v
                    },
                    anchor,
                )
            }
            other => return Err(LoadError::Syntax(format!("unexpected event {other:?}"))),
        };
        if anchor > 0 {
            self.anchors.insert(anchor, v.clone());
        }
        Ok(v)
    }
}

fn is_override(tag: &Option<yaml_rust2::parser::Tag>) -> bool {
    tag.as_ref()
        .is_some_and(|t| t.handle == "!" && t.suffix == "override")
}

/// `yaml.load(text, Loader=BaseLoader)`; `None` for an empty document.
pub fn load(text: &str) -> Result<Option<PyV>, LoadError> {
    load_with(text, false)
}

/// `yaml.safe_load(text)` (duplicate keys rejected like the CLI's no-duplicates loader).
pub fn safe_load(text: &str) -> Result<Option<PyV>, LoadError> {
    load_with(text, true)
}

fn load_with(text: &str, safe: bool) -> Result<Option<PyV>, LoadError> {
    let mut l = Loader {
        parser: Parser::new_from_str(text),
        anchors: Default::default(),
        safe,
    };
    loop {
        match l.next()? {
            Event::StreamStart | Event::DocumentStart => continue,
            Event::StreamEnd | Event::DocumentEnd => return Ok(None),
            e => return l.node(e).map(Some),
        }
    }
}

// ---------------------------------------------------------------- dumping

static NON_STR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    let bool_ = r"yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF";
    let null = r"~|null|Null|NULL|";
    let float = r"[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)";
    let int = r"[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+";
    let ts = r"[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?";
    regex::Regex::new(&format!(
        r"^(?:{bool_}|{null}|{float}|{int}|{ts}|<<|=|!|&|\*)$"
    ))
    .expect("re")
});

struct Analysis {
    empty: bool,
    multiline: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
}

fn is_ws(c: Option<char>) -> bool {
    matches!(
        c,
        None | Some('\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
    )
}

fn analyze(s: &str) -> Analysis {
    if s.is_empty() {
        return Analysis {
            empty: true,
            multiline: false,
            allow_block_plain: true,
            allow_single_quoted: true,
        };
    }
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let (mut block_ind, mut line_breaks, mut special) = (false, false, false);
    let (mut lead_space, mut lead_break, mut trail_space, mut trail_break) =
        (false, false, false, false);
    let (mut break_space, mut space_break) = (false, false);
    if s.starts_with("---") || s.starts_with("...") {
        block_ind = true;
    }
    let mut preceded_ws = true;
    let mut followed_ws = n == 1 || is_ws(chars.get(1).copied());
    let (mut prev_space, mut prev_break) = (false, false);
    for (i, &ch) in chars.iter().enumerate() {
        if i == 0 {
            if "#,[]{}&*!|>'\"%@`".contains(ch) {
                block_ind = true;
            }
            if "?:".contains(ch) && followed_ws {
                block_ind = true;
            }
            if ch == '-' && followed_ws {
                block_ind = true;
            }
        } else {
            if ch == ':' && followed_ws {
                block_ind = true;
            }
            if ch == '#' && preceded_ws {
                block_ind = true;
            }
        }
        let is_break = matches!(ch, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
        if is_break {
            line_breaks = true;
        }
        if !(ch == '\n' || ('\x20'..='\x7e').contains(&ch)) {
            // allow_unicode=False: every non-ASCII printable is special too.
            special = true;
        }
        if ch == ' ' {
            if i == 0 {
                lead_space = true;
            }
            if i == n - 1 {
                trail_space = true;
            }
            if prev_break {
                break_space = true;
            }
            prev_space = true;
            prev_break = false;
        } else if is_break {
            if i == 0 {
                lead_break = true;
            }
            if i == n - 1 {
                trail_break = true;
            }
            if prev_space {
                space_break = true;
            }
            prev_space = false;
            prev_break = true;
        } else {
            prev_space = false;
            prev_break = false;
        }
        preceded_ws = is_ws(Some(ch));
        followed_ws = i + 2 >= n || is_ws(chars.get(i + 2).copied());
    }
    let mut plain = true;
    let mut single = true;
    if lead_space || lead_break || trail_space || trail_break {
        plain = false;
    }
    if break_space {
        plain = false;
        single = false;
    }
    if space_break || special {
        plain = false;
        single = false;
    }
    if line_breaks || block_ind {
        plain = false;
    }
    Analysis {
        empty: false,
        multiline: line_breaks,
        allow_block_plain: plain,
        allow_single_quoted: single,
    }
}

thread_local! {
    static BEST_WIDTH: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

fn column(out: &str) -> usize {
    out.rsplit('\n')
        .next()
        .map(|l| l.chars().count())
        .unwrap_or(0)
}

fn over_width(out: &str) -> bool {
    column(out) > BEST_WIDTH.with(|w| w.get())
}

/// `write_plain`: a single space after the line passed the width becomes a break.
fn plain(text: &str, indent: usize, out: &mut String) {
    let chars: Vec<char> = text.chars().collect();
    let (mut start, mut end) = (0usize, 0usize);
    let mut spaces = false;
    while end <= chars.len() {
        let ch = chars.get(end).copied();
        if spaces {
            if ch != Some(' ') {
                if start + 1 == end && over_width(out) {
                    out.push('\n');
                    out.push_str(&" ".repeat(indent));
                } else {
                    out.extend(&chars[start..end]);
                }
                start = end;
            }
        } else if ch.is_none_or(|c| c == ' ') {
            out.extend(&chars[start..end]);
            start = end;
        }
        if let Some(c) = ch {
            spaces = c == ' ';
        }
        end += 1;
    }
}

fn single_quoted(text: &str, indent: usize, out: &mut String) {
    out.push('\'');
    let chars: Vec<char> = text.chars().collect();
    let (mut spaces, mut breaks) = (false, false);
    let (mut start, mut end) = (0usize, 0usize);
    let is_br = |c: char| matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
    while end <= chars.len() {
        let ch = chars.get(end).copied();
        if spaces {
            if ch != Some(' ') {
                if start + 1 == end && over_width(out) && start != 0 && end != chars.len() {
                    out.push('\n');
                    out.push_str(&" ".repeat(indent));
                } else {
                    out.extend(&chars[start..end]);
                }
                start = end;
            }
        } else if breaks {
            if ch.is_none_or(|c| !is_br(c)) {
                if chars[start] == '\n' {
                    out.push('\n');
                }
                for &br in &chars[start..end] {
                    out.push(if br == '\n' { '\n' } else { br });
                }
                out.push_str(&" ".repeat(indent));
                start = end;
            }
        } else if (ch.is_none_or(|c| c == ' ' || is_br(c) || c == '\'')) && start < end {
            out.extend(&chars[start..end]);
            start = end;
        }
        if ch == Some('\'') {
            out.push_str("''");
            start = end + 1;
        }
        if let Some(c) = ch {
            spaces = c == ' ';
            breaks = is_br(c);
        }
        end += 1;
    }
    out.push('\'');
}

fn double_quoted(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        let esc = match ch {
            '\0' => Some("0"),
            '\x07' => Some("a"),
            '\x08' => Some("b"),
            '\t' => Some("t"),
            '\n' => Some("n"),
            '\x0b' => Some("v"),
            '\x0c' => Some("f"),
            '\r' => Some("r"),
            '\x1b' => Some("e"),
            '"' => Some("\""),
            '\\' => Some("\\"),
            '\u{85}' => Some("N"),
            '\u{a0}' => Some("_"),
            '\u{2028}' => Some("L"),
            '\u{2029}' => Some("P"),
            _ => None,
        };
        if let Some(e) = esc {
            out.push('\\');
            out.push_str(e);
        } else if ('\x20'..='\x7e').contains(&ch) {
            out.push(ch);
        } else if (ch as u32) <= 0xff {
            out.push_str(&format!("\\x{:02X}", ch as u32));
        } else if (ch as u32) <= 0xffff {
            out.push_str(&format!("\\u{:04X}", ch as u32));
        } else {
            out.push_str(&format!("\\U{:08X}", ch as u32));
        }
    }
    out.push('"');
}

fn scalar(s: &str, simple_key: bool, indent: usize, out: &mut String) {
    let a = analyze(s);
    let implicit_plain = !NON_STR.is_match(s);
    if implicit_plain && !(simple_key && (a.empty || a.multiline)) && a.allow_block_plain {
        if simple_key {
            out.push_str(s);
        } else {
            plain(s, indent, out);
        }
        return;
    }
    if a.allow_single_quoted && !(simple_key && a.multiline) {
        single_quoted(s, indent, out);
        return;
    }
    double_quoted(s, out);
}

fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return ".nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { ".inf" } else { "-.inf" }.into();
    }
    let v = crate::pyfmt::float_repr(f).to_lowercase();
    if !v.contains('.') && v.contains('e') {
        v.replacen('e', ".0e", 1)
    } else {
        v
    }
}

fn simple_scalar(v: &PyV, indent: usize, simple_key: bool, out: &mut String) -> bool {
    match v {
        PyV::None => out.push_str("null"),
        PyV::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        PyV::Int(i) => out.push_str(&i.to_string()),
        PyV::Float(f) => out.push_str(&float_repr(*f)),
        PyV::Str(s) => scalar(s, simple_key, indent, out),
        PyV::Timestamp(t) => out.push_str(t),
        PyV::List(l) if l.is_empty() => out.push_str("[]"),
        PyV::Dict(d) if d.is_empty() => out.push_str("{}"),
        _ => return false,
    }
    true
}

fn node(v: &PyV, indent: usize, out: &mut String) {
    match v {
        PyV::Dict(d) if !d.is_empty() => {
            for (i, (k, val)) in d.iter().enumerate() {
                if i > 0 {
                    out.push_str(&" ".repeat(indent));
                }
                scalar(k, true, indent, out);
                out.push(':');
                value_after_key(val, indent, out);
            }
        }
        PyV::List(l) if !l.is_empty() => {
            for (i, item) in l.iter().enumerate() {
                if i > 0 {
                    out.push_str(&" ".repeat(indent));
                }
                out.push_str("- ");
                if !simple_scalar(item, indent + 2, false, out) {
                    node(item, indent + 2, out);
                    continue;
                }
                out.push('\n');
            }
        }
        PyV::Override(inner) => node(inner, indent, out),
        PyV::Model(m) => node(&PyV::Dict(m.fields.clone()), indent, out),
        other => {
            simple_scalar(other, indent, false, out);
            out.push('\n');
        }
    }
}

fn value_after_key(val: &PyV, indent: usize, out: &mut String) {
    match val {
        PyV::Dict(d) if !d.is_empty() => {
            out.push('\n');
            out.push_str(&" ".repeat(indent + 2));
            node(val, indent + 2, out);
        }
        PyV::List(l) if !l.is_empty() => {
            out.push('\n');
            out.push_str(&" ".repeat(indent));
            node(val, indent, out);
        }
        PyV::Override(inner) => value_after_key(inner, indent, out),
        PyV::Model(m) => value_after_key(&PyV::Dict(m.fields.clone()), indent, out),
        other => {
            out.push(' ');
            simple_scalar(other, indent + 2, false, out);
            out.push('\n');
        }
    }
}

/// `yaml.safe_dump(v, sort_keys=False)` (default width 80).
pub fn safe_dump_default_width(v: &PyV) -> String {
    BEST_WIDTH.with(|w| w.set(80));
    let out = safe_dump(v);
    BEST_WIDTH.with(|w| w.set(usize::MAX));
    out
}

/// `yaml.safe_dump(v, sort_keys=False, width=float("inf"))`.
pub fn safe_dump(v: &PyV) -> String {
    let mut out = String::new();
    match v {
        PyV::Dict(d) if !d.is_empty() => node(v, 0, &mut out),
        PyV::List(l) if !l.is_empty() => node(v, 0, &mut out),
        other => {
            simple_scalar(other, 0, false, &mut out);
            out.push_str("\n...\n");
        }
    }
    out
}
