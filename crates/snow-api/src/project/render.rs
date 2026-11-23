//! `<% … %>` templates in project definitions (`definition_rendering.py`).
//!
//! Upstream renders with Jinja (`<%`/`%>` variable delimiters, logic blocks
//! disabled, StrictUndefined) but first walks the template AST and accepts only
//! names, attribute chains, constants, calls and filters. This module parses
//! exactly that subset and evaluates it itself.

use super::value::PyV;
use indexmap::IndexMap;

#[derive(Debug, Clone)]
pub enum Expr {
    Const(PyV),
    /// `a.b.c`
    Var(Vec<String>),
    Call(Vec<String>, Vec<Expr>),
    Filter(Box<Expr>, String, Vec<Expr>),
}

#[derive(Debug, Clone)]
pub enum Piece {
    Text(String),
    Out(Expr),
}

#[derive(Debug)]
pub enum TemplateError {
    /// Jinja `TemplateSyntaxError` text.
    Syntax(String),
    /// "Unexpected template syntax in …"
    Unexpected,
}

struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
}

#[derive(Debug, PartialEq, Clone)]
enum Tok {
    Name(String),
    Str(String),
    Num(String),
    Dot,
    Pipe,
    LParen,
    RParen,
    Comma,
    Other,
    End,
}

impl Lexer<'_> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }
    fn next(&mut self) -> Result<Tok, TemplateError> {
        self.skip_ws();
        let Some(&c) = self.s.get(self.i) else {
            return Ok(Tok::End);
        };
        let c = c as char;
        self.i += 1;
        Ok(match c {
            '.' if self.s.get(self.i).is_some_and(|d| d.is_ascii_digit()) => Tok::Other,
            '.' => Tok::Dot,
            '|' => Tok::Pipe,
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            ',' => Tok::Comma,
            '\'' | '"' => {
                let mut out = Vec::new();
                loop {
                    let Some(&b) = self.s.get(self.i) else {
                        return Err(TemplateError::Syntax("unexpected char '\\'' at 0".into()));
                    };
                    self.i += 1;
                    if b as char == c {
                        break;
                    }
                    if b == b'\\'
                        && let Some(&n) = self.s.get(self.i)
                    {
                        self.i += 1;
                        out.push(match n {
                            b'n' => b'\n',
                            b't' => b'\t',
                            other => other,
                        });
                        continue;
                    }
                    out.push(b);
                }
                Tok::Str(String::from_utf8_lossy(&out).into_owned())
            }
            c if c.is_ascii_digit() => {
                let st = self.i - 1;
                while self
                    .s
                    .get(self.i)
                    .is_some_and(|d| d.is_ascii_digit() || *d == b'_' || *d == b'.')
                {
                    self.i += 1;
                }
                Tok::Num(String::from_utf8_lossy(&self.s[st..self.i]).into_owned())
            }
            c if c.is_alphabetic() || c == '_' => {
                let st = self.i - 1;
                while self
                    .s
                    .get(self.i)
                    .is_some_and(|d| (*d as char).is_alphanumeric() || *d == b'_' || *d >= 0x80)
                {
                    self.i += 1;
                }
                Tok::Name(String::from_utf8_lossy(&self.s[st..self.i]).into_owned())
            }
            _ => Tok::Other,
        })
    }
}

struct P<'a> {
    lx: Lexer<'a>,
    cur: Tok,
}

impl P<'_> {
    fn bump(&mut self) -> Result<Tok, TemplateError> {
        let t = std::mem::replace(&mut self.cur, Tok::End);
        self.cur = self.lx.next()?;
        Ok(t)
    }
    fn args(&mut self) -> Result<Vec<Expr>, TemplateError> {
        let mut out = Vec::new();
        if self.cur == Tok::RParen {
            self.bump()?;
            return Ok(out);
        }
        loop {
            out.push(self.expr()?);
            match self.bump()? {
                Tok::Comma => continue,
                Tok::RParen => return Ok(out),
                _ => return Err(TemplateError::Unexpected),
            }
        }
    }
    fn primary(&mut self) -> Result<Expr, TemplateError> {
        match self.bump()? {
            Tok::Str(s) => Ok(Expr::Const(PyV::Str(s))),
            Tok::Num(n) => Ok(Expr::Const(match n.replace('_', "").parse::<i64>() {
                Ok(i) => PyV::Int(i),
                Err(_) => PyV::Float(n.replace('_', "").parse().unwrap_or(0.0)),
            })),
            Tok::Name(n) => {
                match n.as_str() {
                    "true" | "True" => return Ok(Expr::Const(PyV::Bool(true))),
                    "false" | "False" => return Ok(Expr::Const(PyV::Bool(false))),
                    "none" | "None" => return Ok(Expr::Const(PyV::None)),
                    "and" | "or" | "not" | "in" | "is" | "if" | "else" => {
                        return Err(TemplateError::Unexpected);
                    }
                    _ => {}
                }
                let mut chain = vec![n];
                loop {
                    match &self.cur {
                        Tok::Dot => {
                            self.bump()?;
                            match self.bump()? {
                                Tok::Name(a) => chain.push(a),
                                _ => return Err(TemplateError::Unexpected),
                            }
                        }
                        Tok::LParen => {
                            self.bump()?;
                            let args = self.args()?;
                            if matches!(self.cur, Tok::Dot | Tok::LParen) {
                                return Err(TemplateError::Unexpected);
                            }
                            return Ok(Expr::Call(chain, args));
                        }
                        _ => return Ok(Expr::Var(chain)),
                    }
                }
            }
            _ => Err(TemplateError::Unexpected),
        }
    }
    fn expr(&mut self) -> Result<Expr, TemplateError> {
        let mut e = self.primary()?;
        while self.cur == Tok::Pipe {
            self.bump()?;
            let Tok::Name(f) = self.bump()? else {
                return Err(TemplateError::Unexpected);
            };
            let args = if self.cur == Tok::LParen {
                self.bump()?;
                self.args()?
            } else {
                vec![]
            };
            e = Expr::Filter(Box::new(e), f, args);
        }
        Ok(e)
    }
}

fn parse_expr(src: &str) -> Result<Expr, TemplateError> {
    let mut p = P {
        lx: Lexer {
            s: src.as_bytes(),
            i: 0,
        },
        cur: Tok::End,
    };
    p.cur = p.lx.next()?;
    if p.cur == Tok::End {
        return Err(TemplateError::Syntax(
            "Expected an expression, got 'end of print statement'".into(),
        ));
    }
    let e = p.expr()?;
    if p.cur != Tok::End {
        return Err(TemplateError::Unexpected);
    }
    Ok(e)
}

/// Split a template into text and `<% expr %>` pieces (`{# #}` comments dropped).
pub fn parse(t: &str) -> Result<Vec<Piece>, TemplateError> {
    let mut out = Vec::new();
    let mut rest = t;
    let mut text = String::new();
    loop {
        let v = rest.find("<%");
        let c = rest.find("{#");
        match (v, c) {
            (None, None) => {
                text.push_str(rest);
                break;
            }
            (Some(i), cc) if cc.is_none_or(|c| i < c) => {
                text.push_str(&rest[..i]);
                let mut after = &rest[i + 2..];
                if let Some(a) = after.strip_prefix('-') {
                    text.truncate(text.trim_end().len());
                    after = a;
                } else if let Some(a) = after.strip_prefix('+') {
                    after = a;
                }
                let Some(end) = after.find("%>") else {
                    return Err(TemplateError::Syntax(
                        "unexpected end of template, expected 'end of print statement'.".into(),
                    ));
                };
                let mut inner = &after[..end];
                let mut tail = &after[end + 2..];
                if let Some(x) = inner.strip_suffix('-') {
                    inner = x;
                    tail = tail.trim_start();
                }
                if !text.is_empty() {
                    out.push(Piece::Text(std::mem::take(&mut text)));
                }
                out.push(Piece::Out(parse_expr(inner)?));
                rest = tail;
            }
            (_, Some(i)) => {
                text.push_str(&rest[..i]);
                let Some(end) = rest[i + 2..].find("#}") else {
                    return Err(TemplateError::Syntax("Missing end of comment tag".into()));
                };
                rest = &rest[i + 2 + end + 2..];
            }
            _ => unreachable!(),
        }
    }
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    Ok(out)
}

fn collect(e: &Expr, out: &mut Vec<Vec<String>>) {
    match e {
        Expr::Const(_) => {}
        Expr::Var(c) => out.push(c.clone()),
        Expr::Call(c, args) => {
            out.push(c.clone());
            args.iter().for_each(|a| collect(a, out));
        }
        Expr::Filter(x, _, args) => {
            collect(x, out);
            args.iter().for_each(|a| collect(a, out));
        }
    }
}

/// The variable chains a template references (`get_referenced_vars`).
pub fn referenced_vars(t: &str) -> Result<Vec<Vec<String>>, TemplateError> {
    let mut out = Vec::new();
    for p in parse(t)? {
        if let Piece::Out(e) = p {
            collect(&e, &mut out);
        }
    }
    Ok(out)
}

/// What `fn.*` and filters need from the outside.
pub trait Host {
    fn username(&self) -> Option<String>;
    fn read_file(&self, filter: &str, name: &str) -> Result<String, String>;
}

pub type Ctx = IndexMap<String, PyV>;

fn lookup<'a>(ctx: &'a Ctx, chain: &[String]) -> Option<&'a PyV> {
    let mut cur = ctx.get(&chain[0])?;
    for k in &chain[1..] {
        cur = cur.get(k)?;
    }
    Some(cur)
}

fn verify_str(name: &str, args: &[PyV], min: usize, max: Option<usize>) -> Result<(), String> {
    if args.len() < min {
        return Err(format!("{name} requires at least {min} argument(s)"));
    }
    if let Some(m) = max
        && args.len() > m
    {
        return Err(format!("{name} supports at most {m} argument(s)"));
    }
    if args.iter().any(|a| a.as_str().is_none()) {
        return Err(format!("{name} only accepts String values"));
    }
    Ok(())
}

fn call_fn(name: &str, args: &[PyV], host: &dyn Host) -> Result<PyV, String> {
    let s = |i: usize| args[i].as_str().unwrap_or_default().to_string();
    use crate::identifiers as ids;
    Ok(PyV::Str(match name {
        "concat_ids" => {
            verify_str(name, args, 1, None)?;
            let v: Vec<String> = (0..args.len()).map(s).collect();
            let r: Vec<&str> = v.iter().map(String::as_str).collect();
            ids::concat_identifiers(&r)
        }
        "str_to_id" => {
            verify_str(name, args, 1, Some(1))?;
            ids::to_identifier(&s(0))
        }
        "id_to_str" => {
            verify_str(name, args, 1, Some(1))?;
            ids::identifier_to_str(&s(0))
        }
        "get_username" => {
            verify_str(name, args, 0, Some(1))?;
            host.username()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| if args.is_empty() { String::new() } else { s(0) })
        }
        "sanitize_id" => {
            verify_str(name, args, 1, Some(1))?;
            ids::sanitize_identifier(&s(0))
        }
        other => return Err(format!("'{other}' is undefined")),
    }))
}

fn filter(name: &str, v: PyV, args: &[PyV], host: &dyn Host) -> Result<PyV, String> {
    let sv = v.py_str();
    Ok(match name {
        "upper" => PyV::Str(sv.to_uppercase()),
        "lower" => PyV::Str(sv.to_lowercase()),
        "trim" => PyV::Str(sv.trim().to_string()),
        "string" => PyV::Str(sv),
        "capitalize" => {
            let mut c = sv.chars();
            PyV::Str(match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            })
        }
        "replace" if args.len() >= 2 => PyV::Str(sv.replace(&args[0].py_str(), &args[1].py_str())),
        "default" => {
            if v.is_none() {
                args.first().cloned().unwrap_or(PyV::Str(String::new()))
            } else {
                v
            }
        }
        "read_file_content" => PyV::Str(host.read_file("read_file_content", &sv)?),
        "procedure_from_js_file" => {
            let code = host.read_file("procedure_from_js_file", &sv)?;
            PyV::Str(format!(
                "var module = {{}};\nvar exports = {{}};\nmodule.exports = exports;\n(function() {{\n{code}\n}})()\nreturn module.exports.apply(this, arguments);"
            ))
        }
        other => return Err(format!("No filter named '{other}'.")),
    })
}

fn eval(e: &Expr, ctx: &Ctx, host: &dyn Host) -> Result<PyV, String> {
    match e {
        Expr::Const(c) => Ok(c.clone()),
        Expr::Var(chain) => lookup(ctx, chain)
            .cloned()
            .ok_or_else(|| format!("'{}' is undefined", chain.join("."))),
        Expr::Call(chain, args) => {
            let a: Vec<PyV> = args
                .iter()
                .map(|x| eval(x, ctx, host))
                .collect::<Result<_, _>>()?;
            if chain.len() == 2 && chain[0] == "fn" {
                call_fn(&chain[1], &a, host)
            } else {
                Err(format!("'{}' is undefined", chain.join(".")))
            }
        }
        Expr::Filter(x, name, args) => {
            let v = eval(x, ctx, host)?;
            let a: Vec<PyV> = args
                .iter()
                .map(|x| eval(x, ctx, host))
                .collect::<Result<_, _>>()?;
            filter(name, v, &a, host)
        }
    }
}

/// `TemplatedEnvironment.render`: a value without templates is returned as is.
pub fn render(v: &PyV, ctx: &Ctx, host: &dyn Host) -> Result<PyV, String> {
    let s = v.py_str();
    let pieces = parse(&s).map_err(|_| "template".to_string())?;
    if referenced_vars(&s).map(|r| r.is_empty()).unwrap_or(true) {
        return Ok(v.clone());
    }
    let mut out = String::new();
    for p in &pieces {
        match p {
            Piece::Text(t) => out.push_str(t),
            Piece::Out(e) => out.push_str(&eval(e, ctx, host)?.py_str()),
        }
    }
    Ok(PyV::Str(out))
}
