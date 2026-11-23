//! Click 8.1 command-line parsing and parameter processing.
//!
//! This follows `click.parser.OptionParser` (option matching, short option
//! clusters, `--opt=value`, `--`), `click.core.Command.parse_args`
//! (eager-first processing order, value sources, type conversion, required
//! checks, callbacks, extra-argument errors) and `MultiCommand` subcommand
//! resolution, so that every accepted command line and every error message
//! matches Snowflake CLI.

use crate::difflib::get_close_matches;
use crate::error::ClickError;
use crate::spec::{Command, MdKind, MdTok, NO_MD, Param, Ty};
use crate::value::Value;
use std::collections::HashMap;

/// Everything the parser needs to know about the command tree.
pub struct Spec {
    pub commands: &'static [Command],
    pub params: &'static [Param],
    pub md: &'static [&'static [MdTok]],
    pub shared_connection: &'static [u16],
    pub shared_global: &'static [u16],
}

/// Pseudo index of Click's built-in `--help` option (`--help`, `-h`).
pub const HELP: u16 = u16::MAX;
/// `--help` alone, when a parameter of the command already uses `-h`.
pub const HELP_LONG: u16 = u16::MAX - 1;
/// `-h` alone, when a parameter already uses `--help`.
pub const HELP_SHORT: u16 = u16::MAX - 2;

pub fn is_help(idx: u16) -> bool {
    idx >= HELP_SHORT
}

static HELP_MD: &[MdTok] = &[
    MdTok {
        kind: MdKind::ParagraphOpen,
        tag: "p",
        nesting: 1,
        content: "",
        extra: "",
    },
    MdTok {
        kind: MdKind::Text,
        tag: "",
        nesting: 0,
        content: "Show this message and exit.",
        extra: "",
    },
    MdTok {
        kind: MdKind::ParagraphClose,
        tag: "p",
        nesting: -1,
        content: "",
        extra: "",
    },
];

pub static HELP_PARAM: Param = Param {
    name: "help",
    is_argument: false,
    opts: &["--help", "-h"],
    secondary: &[],
    col_long: "--help",
    col_short: "-h",
    col_sec_long: "",
    col_sec_short: "",
    hidden: false,
    hidden_flag: None,
    required: false,
    multiple: false,
    nargs: 0,
    is_flag: true,
    is_bool_flag: true,
    count: false,
    flag_value: None,
    is_eager: true,
    envvar: None,
    ty: Ty::Bool,
    metavar: "",
    usage_metavar: "",
    envvar_text: None,
    default_text: None,
    default_json: "false",
    structure_default: None,
    help: Some("Show this message and exit."),
    md: NO_MD,
    panel: None,
    callback: None,
    mutex: &[],
    inner_callback: None,
};

impl std::fmt::Debug for Spec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Spec({} commands)", self.commands.len())
    }
}

pub static HELP_LONG_PARAM: Param = Param {
    opts: &["--help"],
    col_short: "",
    ..HELP_PARAM
};

pub static HELP_SHORT_PARAM: Param = Param {
    opts: &["-h"],
    col_long: "",
    ..HELP_PARAM
};

impl Spec {
    pub fn param(&self, idx: u16) -> &'static Param {
        match idx {
            HELP => &HELP_PARAM,
            HELP_LONG => &HELP_LONG_PARAM,
            HELP_SHORT => &HELP_SHORT_PARAM,
            _ => &self.params[idx as usize],
        }
    }
    pub fn md(&self, r: u16) -> &'static [MdTok] {
        if r == NO_MD { &[] } else { self.md[r as usize] }
    }
    pub fn param_md(&self, idx: u16) -> &'static [MdTok] {
        if is_help(idx) {
            HELP_MD
        } else {
            self.md(self.params[idx as usize].md)
        }
    }
    pub fn cmd(&self, idx: u16) -> &'static Command {
        &self.commands[idx as usize]
    }
    /// The parameters Click processes for a command: its own plus `--help`.
    pub fn cmd_params(&self, idx: u16) -> Vec<u16> {
        let c = self.cmd(idx);
        let mut v: Vec<u16> = c.params.to_vec();
        if c.has_help_option {
            let used = |o: &str| {
                c.params.iter().any(|p| {
                    let p = &self.params[*p as usize];
                    p.opts.contains(&o) || p.secondary.contains(&o)
                })
            };
            match (used("--help"), used("-h")) {
                (false, false) => v.push(HELP),
                (false, true) => v.push(HELP_LONG),
                (true, false) => v.push(HELP_SHORT),
                (true, true) => {}
            }
        }
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    CommandLine,
    Environment,
    Default,
    Prompt,
}

/// One parsed command level (`snow`, `snow object`, `snow object list`).
#[derive(Debug, Clone)]
pub struct Level {
    pub cmd: u16,
    pub info_name: String,
    pub values: Vec<(u16, Value, Source)>,
    /// `ctx.args`: arguments left after parsing (extra args, passthrough).
    pub args: Vec<String>,
    /// `ctx.obj["show_all_help"]`.
    pub show_all_help: bool,
}

impl Level {
    pub fn get(&self, spec: &Spec, name: &str) -> Option<&Value> {
        self.values
            .iter()
            .find(|(i, _, _)| spec.param(*i).name == name)
            .map(|(_, v, _)| v)
    }
    pub fn source(&self, spec: &Spec, name: &str) -> Option<Source> {
        self.values
            .iter()
            .find(|(i, _, _)| spec.param(*i).name == name)
            .map(|(_, _, s)| *s)
    }
}

/// The result of parsing a full command line.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub levels: Vec<Level>,
    /// The original arguments (without the program name).
    pub argv: Vec<String>,
    /// The command tree, for `command_path_to`.
    pub spec: Option<&'static Spec>,
}

impl Invocation {
    /// The plain path of names (`snow dbt execute run`), used for dispatch.
    pub fn command_path(&self) -> String {
        self.levels
            .iter()
            .map(|l| l.info_name.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
    /// Click's `ctx.command_path`: parent levels contribute their name and
    /// the usage metavars of their arguments (`snow dbt execute NAME run`).
    pub fn command_path_to(&self, depth: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (i, l) in self.levels[..=depth].iter().enumerate() {
            parts.push(l.info_name.clone());
            if i < depth
                && let Some(spec) = self.spec
            {
                for p in spec.cmd(l.cmd).params {
                    let par = spec.param(*p);
                    if par.is_argument {
                        parts.push(par.usage_metavar.to_string());
                    }
                }
            }
        }
        parts.join(" ")
    }
    pub fn leaf(&self) -> &Level {
        self.levels.last().expect("at least the root level")
    }
}

/// Application callbacks invoked while parameters are processed.
pub trait Hooks {
    fn feature_enabled(&self, _flag: &str) -> bool {
        false
    }
    /// Convert a value of a Snowflake CLI specific type (`Ty::Custom`).
    fn convert_custom(
        &mut self,
        _spec: &Spec,
        _inv: &Invocation,
        _param: &'static Param,
        _ty: &'static str,
        raw: &str,
    ) -> Result<Value, ClickError> {
        Ok(Value::Str(raw.to_string()))
    }
    /// Run the parameter callback; may replace the value.
    fn callback(
        &mut self,
        _spec: &Spec,
        _inv: &mut Invocation,
        _param: &'static Param,
        value: Value,
    ) -> Result<Value, ClickError> {
        Ok(value)
    }
    /// Prompt for an option given without its optional value (`--mfa-passcode`).
    fn prompt(&mut self, _param: &'static Param) -> Result<Value, ClickError> {
        Ok(Value::None)
    }
    /// Override the visible subcommands of a group (`snow app`).
    fn list_commands(&self, _spec: &Spec, _cmd: u16) -> Option<Vec<u16>> {
        None
    }
    /// Called when a level asks for its help (`--help`, `--help-all`, no
    /// arguments with `no_args_is_help`). Returns the exit code.
    fn print_help(&mut self, spec: &Spec, inv: &Invocation, depth: usize)
    -> Result<(), ClickError>;
}

fn param_hidden(spec: &Spec, hooks: &dyn Hooks, idx: u16) -> bool {
    let p = spec.param(idx);
    match p.hidden_flag {
        Some(f) if hooks.feature_enabled(f) => !p.hidden,
        _ => p.hidden,
    }
}

pub fn command_hidden(spec: &Spec, hooks: &dyn Hooks, idx: u16) -> bool {
    let c = spec.cmd(idx);
    match c.hidden_flag {
        Some(f) if hooks.feature_enabled(f) => !c.hidden,
        _ => c.hidden,
    }
}

pub fn param_is_hidden(spec: &Spec, hooks: &dyn Hooks, idx: u16) -> bool {
    param_hidden(spec, hooks, idx)
}

/// Python `repr()` of a string.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[derive(Clone)]
enum Raw {
    One(String),
    Many(Vec<String>),
    Flag(bool),
    Const(&'static str),
    NeedsValue,
}

/// Parser state, the raw values of the positional arguments, and leftovers.
type Parsed = (ParseState, Vec<(u16, Option<Raw>)>, Vec<String>);

struct OptSlot {
    param: u16,
    takes_value: bool,
    nargs: usize,
    /// For flags: the constant stored.
    konst: Option<bool>,
}

struct ParseState {
    opts: HashMap<u16, Vec<Raw>>,
    order: Vec<u16>,
    largs: Vec<String>,
    rargs: Vec<String>,
}

fn split_opt(opt: &str) -> (&str, &str) {
    let first = &opt[..opt.chars().next().map(char::len_utf8).unwrap_or(0)];
    if first.chars().all(char::is_alphanumeric) {
        return ("", opt);
    }
    if opt[first.len()..].starts_with(first) {
        (&opt[..first.len() * 2], &opt[first.len() * 2..])
    } else {
        (first, &opt[first.len()..])
    }
}

struct OptParser<'a> {
    spec: &'a Spec,
    long: Vec<(&'static str, OptSlot)>,
    short: Vec<(&'static str, OptSlot)>,
    args: Vec<u16>,
    allow_interspersed: bool,
    ignore_unknown: bool,
}

impl<'a> OptParser<'a> {
    fn new(spec: &'a Spec, cmd: u16) -> OptParser<'a> {
        let c = spec.cmd(cmd);
        let mut p = OptParser {
            spec,
            long: Vec::new(),
            short: Vec::new(),
            args: Vec::new(),
            allow_interspersed: c.allow_interspersed_args,
            ignore_unknown: c.ignore_unknown_options,
        };
        for idx in spec.cmd_params(cmd) {
            let par = spec.param(idx);
            if par.is_argument {
                p.args.push(idx);
                continue;
            }
            let add = |p: &mut OptParser, opts: &'static [&'static str], konst: Option<bool>| {
                for o in opts {
                    let (prefix, value) = split_opt(o);
                    let slot = OptSlot {
                        param: idx,
                        takes_value: !par.is_flag && !par.count,
                        nargs: par.nargs.max(0) as usize,
                        konst,
                    };
                    if prefix.len() == 1 && value.chars().count() == 1 {
                        p.short.push((o, slot));
                    } else {
                        p.long.push((o, slot));
                    }
                }
            };
            if par.is_flag && par.is_bool_flag && !par.secondary.is_empty() {
                add(&mut p, par.opts, Some(true));
                add(&mut p, par.secondary, Some(false));
            } else if par.is_flag {
                add(&mut p, par.opts, Some(true));
            } else {
                add(&mut p, par.opts, None);
            }
        }
        p
    }

    fn find_long(&self, o: &str) -> Option<&OptSlot> {
        self.long.iter().find(|(n, _)| *n == o).map(|(_, s)| s)
    }
    fn find_short(&self, o: &str) -> Option<&OptSlot> {
        self.short.iter().find(|(n, _)| *n == o).map(|(_, s)| s)
    }

    fn store(&self, st: &mut ParseState, slot: &OptSlot, raw: Raw) {
        let par = self.spec.param(slot.param);
        let raw = match (raw, slot.konst) {
            (Raw::Flag(_), Some(k)) => {
                if par.is_bool_flag {
                    Raw::Flag(k)
                } else {
                    Raw::Const(par.flag_value.unwrap_or(""))
                }
            }
            (r, _) => r,
        };
        let e = st.opts.entry(slot.param).or_default();
        if par.multiple || par.count {
            e.push(raw);
        } else {
            *e = vec![raw];
        }
        st.order.push(slot.param);
    }

    fn get_value(&self, opt: &str, slot: &OptSlot, st: &mut ParseState) -> Result<Raw, ClickError> {
        let nargs = slot.nargs;
        let needs_value = self.spec.param(slot.param).name == "mfa_passcode";
        if st.rargs.len() < nargs {
            if needs_value {
                return Ok(Raw::NeedsValue);
            }
            return Err(ClickError::bad_option_usage(if nargs == 1 {
                format!("Option {} requires an argument.", py_repr(opt))
            } else {
                format!("Option {} requires {} arguments.", py_repr(opt), nargs)
            }));
        }
        if nargs == 1 {
            let next = &st.rargs[0];
            if needs_value && next.starts_with('-') && next.len() > 1 {
                return Ok(Raw::NeedsValue);
            }
            Ok(Raw::One(st.rargs.remove(0)))
        } else {
            Ok(Raw::Many(st.rargs.drain(..nargs).collect()))
        }
    }

    fn match_long(
        &self,
        opt: &str,
        explicit: Option<String>,
        st: &mut ParseState,
    ) -> Result<(), ClickError> {
        let Some(slot) = self.find_long(opt) else {
            let possibilities = get_close_matches(opt, self.long.iter().map(|(n, _)| *n));
            return Err(ClickError::no_such_option(opt, possibilities));
        };
        let raw = if slot.takes_value {
            if let Some(v) = explicit {
                st.rargs.insert(0, v);
            }
            self.get_value(opt, slot, st)?
        } else if explicit.is_some() {
            return Err(ClickError::bad_option_usage(format!(
                "Option {} does not take a value.",
                py_repr(opt)
            )));
        } else {
            Raw::Flag(true)
        };
        self.store(st, slot, raw);
        Ok(())
    }

    fn match_short(&self, arg: &str, st: &mut ParseState) -> Result<(), ClickError> {
        let mut chars = arg.chars();
        let prefix = chars.next().unwrap_or('-');
        let rest: Vec<char> = chars.collect();
        let mut unknown = String::new();
        let mut i = 1;
        for (ci, ch) in rest.iter().enumerate() {
            let opt = format!("{prefix}{ch}");
            i += 1;
            let Some(slot) = self.find_short(&opt) else {
                if self.ignore_unknown {
                    unknown.push(*ch);
                    continue;
                }
                return Err(ClickError::no_such_option(&opt, Vec::new()));
            };
            if slot.takes_value {
                let mut stop = false;
                if i <= rest.len() {
                    let tail: String = rest[ci + 1..].iter().collect();
                    st.rargs.insert(0, tail);
                    stop = true;
                }
                let raw = self.get_value(&opt, slot, st)?;
                self.store(st, slot, raw);
                if stop {
                    break;
                }
            } else {
                self.store(st, slot, Raw::Flag(true));
            }
        }
        if self.ignore_unknown && !unknown.is_empty() {
            st.largs.push(format!("{prefix}{unknown}"));
        }
        Ok(())
    }

    fn process_opts(&self, arg: &str, st: &mut ParseState) -> Result<(), ClickError> {
        let (long_opt, explicit) = match arg.split_once('=') {
            Some((a, b)) => (a.to_string(), Some(b.to_string())),
            None => (arg.to_string(), None),
        };
        match self.match_long(&long_opt, explicit, st) {
            Ok(()) => Ok(()),
            Err(e) if e.is_no_such_option() => {
                let two: String = arg.chars().take(2).collect();
                if two != "-" && two != "--" {
                    return self.match_short(arg, st);
                }
                if !self.ignore_unknown {
                    return Err(e);
                }
                st.largs.push(arg.to_string());
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn parse(&self, args: Vec<String>) -> Result<Parsed, ClickError> {
        let mut st = ParseState {
            opts: HashMap::new(),
            order: Vec::new(),
            largs: Vec::new(),
            rargs: args,
        };
        while !st.rargs.is_empty() {
            let arg = st.rargs.remove(0);
            if arg == "--" {
                break;
            }
            if arg.starts_with('-') && arg.chars().count() > 1 {
                self.process_opts(&arg, &mut st)?;
            } else if self.allow_interspersed {
                st.largs.push(arg);
            } else {
                st.rargs.insert(0, arg);
                break;
            }
        }
        // _process_args_for_args
        let mut all: Vec<String> = std::mem::take(&mut st.largs);
        all.append(&mut st.rargs);
        let nargs: Vec<i32> = self
            .args
            .iter()
            .map(|a| self.spec.param(*a).nargs)
            .collect();
        let (values, rest) = unpack_args(all, &nargs);
        let mut argvals = Vec::new();
        for (a, v) in self.args.iter().zip(values) {
            st.order.push(*a);
            argvals.push((*a, v));
        }
        Ok((st, argvals, rest))
    }
}

/// `click.parser._unpack_args`.
fn unpack_args(args: Vec<String>, nargs_spec: &[i32]) -> (Vec<Option<Raw>>, Vec<String>) {
    let mut args: std::collections::VecDeque<String> = args.into();
    let mut spec: std::collections::VecDeque<i32> = nargs_spec.iter().copied().collect();
    let mut rv: Vec<Option<Raw>> = Vec::new();
    let mut spos: Option<usize> = None;
    let mut from_back = false;
    while let Some(nargs) = if from_back {
        spec.pop_back()
    } else {
        spec.pop_front()
    } {
        let take = |args: &mut std::collections::VecDeque<String>| {
            if from_back {
                args.pop_back()
            } else {
                args.pop_front()
            }
        };
        if nargs == 1 {
            rv.push(take(&mut args).map(Raw::One));
        } else if nargs > 1 {
            let mut x = Vec::new();
            for _ in 0..nargs {
                if let Some(a) = take(&mut args) {
                    x.push(a);
                }
            }
            if from_back {
                x.reverse();
            }
            rv.push(Some(Raw::Many(x)));
        } else if nargs < 0 {
            if spos.is_some() {
                break;
            }
            spos = Some(rv.len());
            rv.push(None);
            from_back = true;
        }
    }
    if let Some(sp) = spos {
        // Restore order: values after the variadic were collected from the back.
        let mut tail: Vec<Option<Raw>> = rv.split_off(sp + 1);
        tail.reverse();
        let rest: Vec<String> = args.drain(..).collect();
        rv[sp] = Some(Raw::Many(rest));
        rv.extend(tail);
        return (rv, Vec::new());
    }
    (rv, args.into_iter().collect())
}

fn is_true_str(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Click's Python-`int()` parsing (underscores and surrounding spaces allowed).
fn py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let t2 = t.replace('_', "");
    if t2.is_empty() || t.starts_with('_') || t.ends_with('_') || t.contains("__") {
        return None;
    }
    t2.parse().ok()
}

fn describe_range(min: Option<i64>, max: Option<i64>, min_open: bool, max_open: bool) -> String {
    match (min, max) {
        (None, Some(mx)) => format!("x{}{}", if max_open { "<" } else { "<=" }, mx),
        (Some(mn), None) => format!("x{}{}", if min_open { ">" } else { ">=" }, mn),
        (Some(mn), Some(mx)) => format!(
            "{}{}x{}{}",
            mn,
            if min_open { "<" } else { "<=" },
            if max_open { "<" } else { "<=" },
            mx
        ),
        (None, None) => String::new(),
    }
}

pub struct Processor<'a, H: Hooks + ?Sized> {
    pub spec: &'static Spec,
    pub hooks: &'a mut H,
}

impl<'a, H: Hooks + ?Sized> Processor<'a, H> {
    fn convert_one(
        &mut self,
        inv: &Invocation,
        param: &'static Param,
        ty: &Ty,
        raw: &str,
    ) -> Result<Value, ClickError> {
        let bad = |msg: String| Err(ClickError::bad_parameter(param, msg));
        match ty {
            Ty::Str => Ok(Value::Str(raw.to_string())),
            Ty::Int => match py_int(raw) {
                Some(i) => Ok(Value::Int(i)),
                None => bad(format!("{} is not a valid integer.", py_repr(raw))),
            },
            Ty::Float => match raw.trim().parse::<f64>() {
                Ok(f) => Ok(Value::Float(f)),
                Err(_) => bad(format!("{} is not a valid float.", py_repr(raw))),
            },
            Ty::Bool => match is_true_str(raw) {
                Some(b) => Ok(Value::Bool(b)),
                None => bad(format!("{} is not a valid boolean.", py_repr(raw))),
            },
            Ty::Choice {
                choices,
                case_sensitive,
            } => {
                let found = choices.iter().find(|c| {
                    if *case_sensitive {
                        **c == raw
                    } else {
                        c.to_lowercase() == raw.to_lowercase()
                    }
                });
                match found {
                    Some(c) => Ok(Value::Str((*c).to_string())),
                    None => {
                        let list = choices
                            .iter()
                            .map(|c| py_repr(c))
                            .collect::<Vec<_>>()
                            .join(", ");
                        if choices.len() == 1 {
                            bad(format!("{} is not {}.", py_repr(raw), list))
                        } else {
                            bad(format!("{} is not one of {}.", py_repr(raw), list))
                        }
                    }
                }
            }
            Ty::IntRange {
                min,
                max,
                min_open,
                max_open,
                clamp,
            } => {
                let Some(v) = py_int(raw) else {
                    return bad(format!("{} is not a valid integer.", py_repr(raw)));
                };
                let lt_min = min.is_some_and(|m| if *min_open { v <= m } else { v < m });
                let gt_max = max.is_some_and(|m| if *max_open { v >= m } else { v > m });
                if *clamp {
                    if lt_min {
                        return Ok(Value::Int(min.unwrap_or(v)));
                    }
                    if gt_max {
                        return Ok(Value::Int(max.unwrap_or(v)));
                    }
                }
                if lt_min || gt_max {
                    return bad(format!(
                        "{} is not in the range {}.",
                        v,
                        describe_range(*min, *max, *min_open, *max_open)
                    ));
                }
                Ok(Value::Int(v))
            }
            Ty::Path {
                exists,
                file_okay,
                dir_okay,
                ..
            } => {
                if raw == "-" {
                    return Ok(Value::Str(raw.to_string()));
                }
                let name = if *file_okay && !*dir_okay {
                    "file"
                } else if *dir_okay && !*file_okay {
                    "directory"
                } else {
                    "path"
                };
                let title = {
                    let mut c = name.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                };
                match std::fs::metadata(raw) {
                    Err(_) => {
                        if *exists {
                            return bad(format!("{} {} does not exist.", title, py_repr(raw)));
                        }
                    }
                    Ok(m) => {
                        if !*file_okay && m.is_file() {
                            return bad(format!("{} {} is a file.", title, py_repr(raw)));
                        }
                        if !*dir_okay && m.is_dir() {
                            return bad(format!("{} {} is a directory.", title, py_repr(raw)));
                        }
                    }
                }
                Ok(Value::Str(raw.to_string()))
            }
            // A tuple converts element-wise in convert_value; a lone string
            // only reaches here for a 1-tuple.
            Ty::Tuple(_) => Ok(Value::Str(raw.to_string())),
            Ty::Custom(name) => self.hooks.convert_custom(self.spec, inv, param, name, raw),
        }
    }

    fn convert_value(
        &mut self,
        inv: &Invocation,
        param: &'static Param,
        v: Value,
    ) -> Result<Value, ClickError> {
        // type_cast_value
        if v.is_none() {
            return Ok(if param.multiple || param.nargs == -1 {
                Value::List(Vec::new())
            } else {
                Value::None
            });
        }
        let conv_single = |this: &mut Self, v: Value| -> Result<Value, ClickError> {
            match (&param.ty, v) {
                (Ty::Tuple(tys), Value::List(items)) => {
                    if items.len() != tys.len() {
                        return Err(ClickError::bad_parameter(
                            param,
                            format!(
                                "{} values are required, but {} were given.",
                                tys.len(),
                                items.len()
                            ),
                        ));
                    }
                    let mut out = Vec::new();
                    for (t, it) in tys.iter().zip(items) {
                        out.push(match it {
                            Value::Str(s) => this.convert_one(inv, param, t, &s)?,
                            other => other,
                        });
                    }
                    Ok(Value::List(out))
                }
                (ty, Value::Str(s)) => this.convert_one(inv, param, ty, &s),
                (Ty::Bool, Value::Bool(b)) => Ok(Value::Bool(b)),
                (Ty::Int | Ty::IntRange { .. }, Value::Int(i)) => {
                    this.convert_one(inv, param, &param.ty, &i.to_string())
                }
                (_, other) => Ok(other),
            }
        };
        let per_item = |this: &mut Self, v: Value| -> Result<Value, ClickError> {
            if param.nargs == 1 || matches!(param.ty, Ty::Tuple(_)) {
                conv_single(this, v)
            } else if param.nargs == -1 {
                let items = match v {
                    Value::List(l) => l,
                    other => vec![other],
                };
                let mut out = Vec::new();
                for it in items {
                    out.push(conv_single(this, it)?);
                }
                Ok(Value::List(out))
            } else {
                let items = match v {
                    Value::List(l) => l,
                    other => vec![other],
                };
                if items.len() != param.nargs as usize {
                    return Err(ClickError::bad_parameter(
                        param,
                        format!(
                            "Takes {} values but {} were given.",
                            param.nargs,
                            items.len()
                        ),
                    ));
                }
                let mut out = Vec::new();
                for it in items {
                    out.push(conv_single(this, it)?);
                }
                Ok(Value::List(out))
            }
        };
        if param.multiple {
            let items = match v {
                Value::List(l) => l,
                other => vec![other],
            };
            let mut out = Vec::new();
            for it in items {
                out.push(per_item(self, it)?);
            }
            Ok(Value::List(out))
        } else {
            per_item(self, v)
        }
    }

    fn raw_to_value(raw: &Raw) -> Value {
        match raw {
            Raw::One(s) => Value::Str(s.clone()),
            Raw::Many(v) => Value::List(v.iter().cloned().map(Value::Str).collect()),
            Raw::Flag(b) => Value::Bool(*b),
            Raw::Const(s) => Value::Str((*s).to_string()),
            Raw::NeedsValue => Value::None,
        }
    }

    fn default_of(&self, inv: &Invocation, param: &'static Param) -> Value {
        if param.is_flag && !param.is_bool_flag {
            // A flag with a non-boolean flag value: the default is the flag
            // value when some sibling option with the same name defaults to on.
            let _ = inv;
        }
        let v: serde_json::Value =
            serde_json::from_str(param.default_json).unwrap_or(serde_json::Value::Null);
        Value::from_json(&v)
    }

    fn envvar_value(param: &'static Param) -> Option<Value> {
        let name = param.envvar?;
        let rv = std::env::var(name).ok().filter(|v| !v.is_empty())?;
        let depth = usize::from(param.nargs != 1) + usize::from(param.multiple);
        if depth > 0 {
            let parts: Vec<Value> = rv
                .split_whitespace()
                .map(|s| Value::Str(s.to_string()))
                .collect();
            return Some(Value::List(parts));
        }
        Some(Value::Str(rv))
    }

    /// Parse and process one level. Returns the remaining arguments.
    fn process_level(
        &mut self,
        inv: &mut Invocation,
        depth: usize,
        args: Vec<String>,
    ) -> Result<Vec<String>, ClickError> {
        let cmd = self.spec.cmd(inv.levels[depth].cmd);
        if !cmd.agent_args {
            return self.process_level_inner(inv, depth, args);
        }
        // `AgentArgsCommand._agent_args_hint`: usage errors point at --agent-args.
        let name = inv.levels[depth].info_name.clone();
        self.process_level_inner(inv, depth, args).map_err(|e| match e {
            ClickError::Usage { message, depth, no_such_option, attach, class } => {
                // Re-formatting a BadParameter prefixes "Invalid value for X: " again.
                let message = match class {
                    "BadParameter" => match message.find(": ") {
                        Some(i) if message.starts_with("Invalid value") => format!("{}{message}", &message[..i + 2]),
                        _ => message,
                    },
                    _ => message,
                };
                let path = inv.command_path_to(depth.unwrap_or(inv.levels.len().saturating_sub(1)));
                ClickError::Usage {
                    message: format!(
                        "{message}\n\nIf that argument was meant for {name}, put it after --agent-args, which forwards everything following it verbatim:\n    {path} --agent-args ..."
                    ),
                    depth,
                    no_such_option,
                    attach,
                    class,
                }
            }
            other => other,
        })
    }

    fn process_level_inner(
        &mut self,
        inv: &mut Invocation,
        depth: usize,
        args: Vec<String>,
    ) -> Result<Vec<String>, ClickError> {
        let cmd_idx = inv.levels[depth].cmd;
        let cmd = self.spec.cmd(cmd_idx);
        if args.is_empty() && cmd.no_args_is_help {
            self.hooks.print_help(self.spec, inv, depth)?;
            return Err(ClickError::Exit(0));
        }
        let (args, tail) = if cmd.agent_args {
            match args.iter().position(|a| a == "--agent-args") {
                Some(i) => (args[..i].to_vec(), Some(args[i + 1..].to_vec())),
                None => (args, Some(Vec::new())),
            }
        } else {
            (args, None)
        };
        let parser = OptParser::new(self.spec, cmd_idx);
        let (st, argvals, rest) = parser.parse(args).map_err(|e| e.at(depth))?;
        let mut raw: HashMap<u16, Value> = HashMap::new();
        let mut needs_value: Vec<u16> = Vec::new();
        for (p, vals) in &st.opts {
            let par = self.spec.param(*p);
            if vals.iter().any(|v| matches!(v, Raw::NeedsValue)) {
                needs_value.push(*p);
                continue;
            }
            let v = if par.multiple || par.count {
                if par.count {
                    Value::Int(vals.len() as i64)
                } else {
                    Value::List(vals.iter().map(Self::raw_to_value).collect())
                }
            } else {
                Self::raw_to_value(&vals[vals.len() - 1])
            };
            raw.insert(*p, v);
        }
        for (a, v) in argvals {
            if let Some(v) = v {
                let val = Self::raw_to_value(&v);
                // An empty variadic argument counts as not given.
                if !matches!(&val, Value::List(l) if l.is_empty()) {
                    raw.insert(a, val);
                }
            }
        }
        // iter_params_for_processing
        let params = self.spec.cmd_params(cmd_idx);
        let mut ordered: Vec<(bool, usize, usize, u16)> = params
            .iter()
            .enumerate()
            .map(|(decl, p)| {
                let inv_idx = st.order.iter().position(|o| o == p).unwrap_or(usize::MAX);
                (!self.spec.param(*p).is_eager, inv_idx, decl, *p)
            })
            .collect();
        ordered.sort();
        inv.levels[depth].args = rest.clone();
        for (_, _, _, p) in ordered {
            let param = self.spec.param(p);
            let (mut value, mut source) = match raw.remove(&p) {
                Some(v) => (v, Source::CommandLine),
                None => match Self::envvar_value(param) {
                    Some(v) => (v, Source::Environment),
                    None => (self.default_of(inv, param), Source::Default),
                },
            };
            if needs_value.contains(&p) {
                value = self.hooks.prompt(param).map_err(|e| e.at(depth))?;
                source = Source::Prompt;
            }
            if is_help(p) {
                if value.as_bool() {
                    self.hooks.print_help(self.spec, inv, depth)?;
                    return Err(ClickError::Exit(0));
                }
                continue;
            }
            let value = self
                .convert_value(inv, param, value)
                .map_err(|e| e.at(depth))?;
            let missing = match &value {
                Value::None => true,
                Value::List(l) => (param.nargs != 1 || param.multiple) && l.is_empty(),
                _ => false,
            };
            if param.required && missing {
                return Err(ClickError::missing(param).at(depth));
            }
            inv.levels[depth].values.push((p, value.clone(), source));
            if param.name == "show_all_help" && param.callback.is_some() {
                if value.as_bool() {
                    if !self.hooks.feature_enabled("ENABLE_CONDENSED_COMMAND_HELP") {
                        return Err(ClickError::no_such_option("--help-all", Vec::new()).at(depth));
                    }
                    inv.levels[depth].show_all_help = true;
                    self.hooks.print_help(self.spec, inv, depth)?;
                    return Err(ClickError::Exit(0));
                }
                continue;
            }
            if param.callback.is_some() {
                let new = self
                    .hooks
                    .callback(self.spec, inv, param, value)
                    .map_err(|e| e.at(depth))?;
                if let Some(slot) = inv.levels[depth]
                    .values
                    .iter_mut()
                    .find(|(i, _, _)| *i == p)
                {
                    slot.1 = new;
                }
            }
        }
        if let Some(tail) = tail {
            if !rest.is_empty() && !cmd.allow_extra_args {
                return Err(extra_args_error(&rest).at(depth));
            }
            inv.levels[depth].args = rest.into_iter().chain(tail).collect();
            return Ok(Vec::new());
        }
        if cmd.is_group {
            return Ok(rest);
        }
        if !rest.is_empty() && !cmd.allow_extra_args {
            return Err(extra_args_error(&rest).at(depth));
        }
        Ok(Vec::new())
    }

    fn resolve(&self, group: u16, name: &str) -> Option<u16> {
        self.spec
            .cmd(group)
            .children
            .iter()
            .copied()
            .find(|c| self.spec.cmd(*c).name == name)
    }

    /// Parse `argv` (without the program name) starting at the root command.
    pub fn run(
        &mut self,
        root: u16,
        argv: Vec<String>,
    ) -> Result<Invocation, (ClickError, Invocation)> {
        let mut inv = Invocation {
            levels: vec![Level {
                cmd: root,
                info_name: "snow".to_string(),
                values: Vec::new(),
                args: Vec::new(),
                show_all_help: false,
            }],
            argv: argv.clone(),
            spec: Some(self.spec),
        };
        let mut args = argv;
        let mut depth = 0;
        loop {
            let rest = match self.process_level(&mut inv, depth, args) {
                Ok(r) => r,
                Err(e) => return Err((e, inv)),
            };
            let cmd = self.spec.cmd(inv.levels[depth].cmd);
            if !cmd.is_group {
                return Ok(inv);
            }
            if rest.is_empty() {
                if cmd.invoke_without_command {
                    return Ok(inv);
                }
                return Err((ClickError::usage("Missing command.".into()).at(depth), inv));
            }
            let name = rest[0].clone();
            let Some(child) = self.resolve(inv.levels[depth].cmd, &name) else {
                if !split_opt(&name).0.is_empty() {
                    // Click re-parses the group with the arguments after the
                    // name so option errors (and --help) are reported.
                    inv.levels[depth].values.clear();
                    if let Err(e) = self.process_level(&mut inv, depth, rest[1..].to_vec()) {
                        return Err((e, inv));
                    }
                }
                return Err((
                    ClickError::usage(format!("No such command {}.", py_repr(&name))).at(depth),
                    inv,
                ));
            };
            inv.levels[depth].args = Vec::new();
            inv.levels.push(Level {
                cmd: child,
                info_name: name,
                values: Vec::new(),
                args: Vec::new(),
                show_all_help: false,
            });
            depth += 1;
            args = rest[1..].to_vec();
        }
    }
}

fn extra_args_error(rest: &[String]) -> ClickError {
    let joined = rest.join(" ");
    if rest.len() == 1 {
        ClickError::usage(format!("Got unexpected extra argument ({joined})"))
    } else {
        ClickError::usage(format!("Got unexpected extra arguments ({joined})"))
    }
}

pub fn describe(min: Option<i64>, max: Option<i64>, min_open: bool, max_open: bool) -> String {
    describe_range(min, max, min_open, max_open)
}
