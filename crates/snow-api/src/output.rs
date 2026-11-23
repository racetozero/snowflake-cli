//! Command results and their printers (`snowflake.cli._app.printing`).
//!
//! Formats: TABLE (Rich ASCII table, or the streamed ASCII writer for big
//! results), PLAIN (border-less table), JSON (Python `json.dump(indent=4)`,
//! `ensure_ascii=True`), JSON_EXT (JSON with VARIANT/OBJECT/ARRAY parsed) and
//! CSV (`csv.QUOTE_MINIMAL` with formula-prefix sanitising).

use crate::pyfmt::{float_repr, json_str};
use crate::sanitize;
use indexmap::IndexMap;
use snow_rich::{Column, Overflow, R, Table, Text};
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Table,
    Json,
    JsonExt,
    Csv,
    Plain,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s.to_uppercase().as_str() {
            "TABLE" => Some(Format::Table),
            "JSON" => Some(Format::Json),
            "JSON_EXT" => Some(Format::JsonExt),
            "CSV" => Some(Format::Csv),
            "PLAIN" => Some(Format::Plain),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Format::Table => "TABLE",
            Format::Json => "JSON",
            Format::JsonExt => "JSON_EXT",
            Format::Csv => "CSV",
            Format::Plain => "PLAIN",
        }
    }
    pub fn is_json(self) -> bool {
        matches!(self, Format::Json | Format::JsonExt)
    }
    pub fn is_structured(self) -> bool {
        self.is_json() || self == Format::Csv
    }
}

/// A Python value as it appears in command results.
#[derive(Debug, Clone, PartialEq)]
pub enum Pv {
    None,
    Bool(bool),
    Int(i128),
    Float(f64),
    /// `decimal.Decimal`, kept as its `str()`.
    Decimal(String),
    Str(String),
    /// `datetime.date` / `datetime.time` isoformat.
    Date(String),
    Time(String),
    /// `datetime.datetime`: date, time (with fraction) and offset (`+HH:MM` or empty).
    DateTime {
        date: String,
        time: String,
        tz: String,
    },
    /// Python `bytes`.
    Bytes(Vec<u8>),
    /// Python `bytearray` (printed as hex).
    ByteArray(Vec<u8>),
    /// `pathlib.Path` (printed with `as_posix()` in JSON).
    Path(String),
    List(Vec<Pv>),
    Tuple(Vec<Pv>),
    Dict(IndexMap<String, Pv>),
}

impl Pv {
    pub fn s(v: impl Into<String>) -> Pv {
        Pv::Str(v.into())
    }
    pub fn opt(v: Option<impl Into<String>>) -> Pv {
        v.map(|s| Pv::Str(s.into())).unwrap_or(Pv::None)
    }

    /// Python `str(value)`.
    pub fn py_str(&self) -> String {
        match self {
            Pv::None => "None".into(),
            Pv::Bool(b) => if *b { "True" } else { "False" }.into(),
            Pv::Int(i) => i.to_string(),
            Pv::Float(f) => float_repr(*f),
            Pv::Decimal(d) => d.clone(),
            Pv::Str(s) | Pv::Path(s) => s.clone(),
            Pv::Date(s) | Pv::Time(s) => s.clone(),
            Pv::DateTime { date, time, tz } => format!("{date} {time}{tz}"),
            Pv::Bytes(b) => bytes_repr(b),
            Pv::ByteArray(b) => format!("bytearray({})", bytes_repr(b)),
            Pv::List(_) | Pv::Tuple(_) | Pv::Dict(_) => self.py_repr(),
        }
    }

    /// Python `repr(value)`.
    pub fn py_repr(&self) -> String {
        match self {
            Pv::Str(s) => snow_click::py_repr(s),
            Pv::Path(s) => format!("PosixPath({})", snow_click::py_repr(s)),
            Pv::Decimal(d) => format!("Decimal('{d}')"),
            Pv::List(l) => format!(
                "[{}]",
                l.iter().map(Pv::py_repr).collect::<Vec<_>>().join(", ")
            ),
            Pv::Tuple(l) => {
                if l.len() == 1 {
                    format!("({},)", l[0].py_repr())
                } else {
                    format!(
                        "({})",
                        l.iter().map(Pv::py_repr).collect::<Vec<_>>().join(", ")
                    )
                }
            }
            Pv::Dict(d) => format!(
                "{{{}}}",
                d.iter()
                    .map(|(k, v)| format!("{}: {}", snow_click::py_repr(k), v.py_repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Pv::Date(s) => {
                let p: Vec<&str> = s.split('-').collect();
                format!(
                    "datetime.date({})",
                    p.iter()
                        .map(|x| x.trim_start_matches('0').to_string())
                        .map(|x| if x.is_empty() { "0".into() } else { x })
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            other => other.py_str(),
        }
    }

    /// The table-cell text (`printing.__to_str`).
    pub fn cell(&self) -> String {
        match self {
            Pv::ByteArray(b) => hex(b),
            other => other.py_str(),
        }
    }

    pub fn from_json(v: &serde_json::Value) -> Pv {
        use serde_json::Value as J;
        match v {
            J::Null => Pv::None,
            J::Bool(b) => Pv::Bool(*b),
            J::Number(n) => {
                let t = n.to_string();
                if t.contains(['.', 'e', 'E']) {
                    Pv::Float(t.parse().unwrap_or(0.0))
                } else {
                    Pv::Int(t.parse().unwrap_or(0))
                }
            }
            J::String(s) => Pv::Str(s.clone()),
            J::Array(a) => Pv::List(a.iter().map(Pv::from_json).collect()),
            J::Object(m) => Pv::Dict(
                m.iter()
                    .map(|(k, v)| (k.clone(), Pv::from_json(v)))
                    .collect(),
            ),
        }
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    let mut s = String::from("b");
    s.push(quote);
    for &c in b {
        match c {
            b'\\' => s.push_str("\\\\"),
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            b'\t' => s.push_str("\\t"),
            c if c as char == quote => {
                s.push('\\');
                s.push(c as char);
            }
            0x20..=0x7e => s.push(c as char),
            c => s.push_str(&format!("\\x{c:02x}")),
        }
    }
    s.push(quote);
    s
}

pub type Row = IndexMap<String, Pv>;

/// A result of a command (`snowflake.cli.api.output.types`).
#[derive(Debug, Clone)]
pub enum Output {
    /// `EmptyResult`, or a command that returned `None`: nothing is printed.
    Empty,
    Message(String),
    /// `ObjectResult`; `None` element prints "No data".
    Object(Option<Row>),
    Collection(Vec<Row>),
    Multiple(Vec<Output>),
    Stream(Vec<Output>),
}

impl Output {
    pub fn message(s: impl Into<String>) -> Output {
        Output::Message(s.into())
    }
    pub fn object(pairs: Vec<(&str, Pv)>) -> Output {
        Output::Object(Some(
            pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        ))
    }
    /// The JSON value the agent envelope carries.
    pub fn to_json_value(&self) -> serde_json::Value {
        match self {
            Output::Empty => serde_json::Value::Null,
            Output::Message(m) => serde_json::json!({ "message": m }),
            Output::Object(None) => serde_json::Value::Null,
            Output::Object(Some(r)) => row_json(r),
            Output::Collection(rows) => {
                serde_json::Value::Array(rows.iter().map(row_json).collect())
            }
            Output::Multiple(v) | Output::Stream(v) => {
                serde_json::Value::Array(v.iter().map(Output::to_json_value).collect())
            }
        }
    }
}

fn row_json(r: &Row) -> serde_json::Value {
    serde_json::Value::Object(r.iter().map(|(k, v)| (k.clone(), pv_json(v))).collect())
}

pub fn pv_json(v: &Pv) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Pv::None => J::Null,
        Pv::Bool(b) => J::Bool(*b),
        Pv::Int(i) => serde_json::from_str(&i.to_string()).unwrap_or(J::Null),
        Pv::Float(f) => serde_json::Number::from_f64(*f)
            .map(J::Number)
            .unwrap_or(J::Null),
        Pv::Decimal(d) => J::String(d.clone()),
        Pv::Str(s) | Pv::Path(s) | Pv::Date(s) | Pv::Time(s) => J::String(s.clone()),
        Pv::DateTime { date, time, tz } => J::String(format!("{date}T{time}{tz}")),
        Pv::Bytes(b) | Pv::ByteArray(b) => J::String(hex(b)),
        Pv::List(l) | Pv::Tuple(l) => J::Array(l.iter().map(pv_json).collect()),
        Pv::Dict(d) => J::Object(d.iter().map(|(k, v)| (k.clone(), pv_json(v))).collect()),
    }
}

// ----------------------------------------------------------------------------
// JSON in Python's iterencode chunks
// ----------------------------------------------------------------------------

/// An error while encoding (`TypeError: Object of type bytes is not JSON serializable`).
#[derive(Debug)]
pub struct JsonTypeError(pub String);

fn leaf(v: &Pv) -> Result<Option<String>, JsonTypeError> {
    Ok(Some(match v {
        Pv::None => "null".into(),
        Pv::Bool(b) => if *b { "true" } else { "false" }.into(),
        Pv::Int(i) => i.to_string(),
        Pv::Float(f) => {
            if f.is_nan() {
                "NaN".into()
            } else if f.is_infinite() {
                if *f > 0.0 {
                    "Infinity".into()
                } else {
                    "-Infinity".into()
                }
            } else {
                float_repr(*f)
            }
        }
        Pv::Str(s) => {
            let mut o = String::new();
            json_str(&sanitize::for_terminal(s), &mut o);
            o
        }
        Pv::Decimal(_)
        | Pv::Date(_)
        | Pv::Time(_)
        | Pv::DateTime { .. }
        | Pv::Path(_)
        | Pv::ByteArray(_) => {
            // Converted by the encoder's default() into a string.
            let j = pv_json(v);
            let mut o = String::new();
            json_str(
                &sanitize::for_terminal(j.as_str().unwrap_or_default()),
                &mut o,
            );
            o
        }
        Pv::Bytes(_) => {
            return Err(JsonTypeError(
                "Object of type bytes is not JSON serializable".into(),
            ));
        }
        _ => return Ok(None),
    }))
}

/// `json.encoder._make_iterencode` with `indent` and default separators.
pub fn json_chunks(
    v: &Pv,
    indent: Option<usize>,
    level: usize,
    out: &mut Vec<String>,
) -> Result<(), JsonTypeError> {
    let item_sep = if indent.is_some() { "," } else { ", " };
    match v {
        Pv::List(items) | Pv::Tuple(items) => {
            if items.is_empty() {
                out.push("[]".into());
                return Ok(());
            }
            let mut buf = String::from("[");
            let lvl = level + 1;
            let newline_indent = indent.map(|n| format!("\n{}", " ".repeat(n * lvl)));
            let separator = format!("{item_sep}{}", newline_indent.clone().unwrap_or_default());
            if let Some(ni) = &newline_indent {
                buf.push_str(ni);
            }
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    buf = separator.clone();
                }
                match leaf(item)? {
                    Some(s) => out.push(format!("{buf}{s}")),
                    None => {
                        out.push(buf.clone());
                        json_chunks(item, indent, lvl, out)?;
                    }
                }
            }
            if let Some(n) = indent {
                out.push(format!("\n{}", " ".repeat(n * level)));
            }
            out.push("]".into());
        }
        Pv::Dict(map) => {
            if map.is_empty() {
                out.push("{}".into());
                return Ok(());
            }
            out.push("{".into());
            let lvl = level + 1;
            let newline_indent = indent.map(|n| format!("\n{}", " ".repeat(n * lvl)));
            let item_separator =
                format!("{item_sep}{}", newline_indent.clone().unwrap_or_default());
            if let Some(ni) = &newline_indent {
                out.push(ni.clone());
            }
            let keys: Vec<String> =
                uniquify(map.keys().map(|k| sanitize::for_terminal(k)).collect());
            for (i, ((_, item), key)) in map.iter().zip(keys).enumerate() {
                if i > 0 {
                    out.push(item_separator.clone());
                }
                let mut k = String::new();
                json_str(&key, &mut k);
                out.push(k);
                out.push(": ".into());
                match leaf(item)? {
                    Some(s) => out.push(s),
                    None => json_chunks(item, indent, lvl, out)?,
                }
            }
            if let Some(n) = indent {
                out.push(format!("\n{}", " ".repeat(n * level)));
            }
            out.push("}".into());
        }
        other => {
            if let Some(s) = leaf(other)? {
                out.push(s);
            }
        }
    }
    Ok(())
}

fn uniquify(keys: Vec<String>) -> Vec<String> {
    let mut used: std::collections::HashSet<String> = keys.iter().cloned().collect();
    let mut counters: std::collections::HashMap<String, usize> = Default::default();
    let mut out = Vec::with_capacity(keys.len());
    for name in keys {
        match counters.get(&name).copied() {
            None => {
                counters.insert(name.clone(), 1);
                out.push(name);
            }
            Some(c) => {
                let mut idx = c + 1;
                let mut cand = format!("{name}_{idx}");
                while used.contains(&cand) {
                    idx += 1;
                    cand = format!("{name}_{idx}");
                }
                used.insert(cand.clone());
                counters.insert(name, idx);
                out.push(cand);
            }
        }
    }
    out
}

pub fn json_string(v: &Pv, indent: Option<usize>) -> Result<String, JsonTypeError> {
    let mut chunks = Vec::new();
    json_chunks(v, indent, 0, &mut chunks)?;
    Ok(chunks.concat())
}

/// `textwrap.indent(text, prefix)`: prefix every line that is not blank.
fn py_indent(text: &str, prefix: &str) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if line.trim().is_empty() {
            out.push_str(line);
        } else {
            out.push_str(prefix);
            out.push_str(line);
        }
    }
    out
}

/// QueryResult column name de-duplication.
pub fn uniquify_columns(names: Vec<String>) -> Vec<String> {
    uniquify(names)
}

// ----------------------------------------------------------------------------
// Printing
// ----------------------------------------------------------------------------

/// Where printed results go (stdout, a tee with a spool file, or nowhere).
pub struct Sink<'a> {
    pub out: &'a mut dyn Write,
    pub is_tty: bool,
    pub width: usize,
    pub color: bool,
}

impl Sink<'_> {
    fn w(&mut self, s: &str) {
        let _ = self.out.write_all(s.as_bytes());
    }
}

fn result_value(o: &Output) -> Pv {
    match o {
        Output::Message(m) => Pv::Dict(
            [("message".to_string(), Pv::Str(m.clone()))]
                .into_iter()
                .collect(),
        ),
        Output::Object(None) | Output::Empty => Pv::None,
        Output::Object(Some(r)) => Pv::Dict(r.clone()),
        Output::Collection(rows) => Pv::List(rows.iter().map(|r| Pv::Dict(r.clone())).collect()),
        Output::Multiple(v) | Output::Stream(v) => Pv::List(v.iter().map(result_value).collect()),
    }
}

pub type PrintResult = Result<(), JsonTypeError>;

/// One element of a streamed `MultipleResults` JSON array (`_stream_json`).
pub fn json_multiple_item(o: &Output, sink: &mut Sink) -> PrintResult {
    let mut chunks = Vec::new();
    json_chunks(&result_value(o), Some(2), 0, &mut chunks)?;
    for c in chunks {
        sink.w(&py_indent(&c, "  "));
    }
    Ok(())
}

/// `_print_csv_result_streaming`.
pub fn csv_result(o: &Output, sink: &mut Sink) {
    print_csv(o, sink);
}

impl Sink<'_> {
    pub fn write_str(&mut self, s: &str) {
        self.w(s);
    }
}

/// `print_result`.
pub fn print_result(o: &Output, format: Format, sink: &mut Sink) -> PrintResult {
    match o {
        Output::Empty => Ok(()),
        _ if format.is_structured() => print_structured(o, format, sink),
        Output::Multiple(v) | Output::Stream(v) => {
            for r in v {
                print_result(r, format, sink)?;
            }
            Ok(())
        }
        _ => {
            print_unstructured(o, format, sink);
            Ok(())
        }
    }
}

fn print_structured(o: &Output, format: Format, sink: &mut Sink) -> PrintResult {
    let mut printed_end_line = false;
    match o {
        Output::Multiple(v) => {
            if format == Format::Csv {
                for r in v {
                    print_csv(r, sink);
                    sink.w("\n");
                }
                printed_end_line = true;
            } else {
                // _stream_json: each result dumped with indent=2 through a
                // writer that indents every chunk.
                sink.w("[\n");
                let n = v.len();
                for (i, r) in v.iter().enumerate() {
                    if i > 0 && i < n {
                        // printed after the previous result
                    }
                    let mut chunks = Vec::new();
                    json_chunks(&result_value(r), Some(2), 0, &mut chunks)?;
                    for c in chunks {
                        sink.w(&py_indent(&c, "  "));
                    }
                    if i + 1 < n {
                        sink.w(",\n");
                    }
                }
                sink.w("\n]\n");
            }
        }
        Output::Stream(v) => {
            for r in v {
                if format == Format::Csv {
                    print_csv(r, sink);
                } else {
                    if matches!(r, Output::Collection(_)) {
                        return Err(JsonTypeError(
                            "CollectionResult should be handled by streaming functions, not encoder".into(),
                        ));
                    }
                    sink.w(&json_string(&result_value(r), None)?);
                }
                sink.w("\n");
                printed_end_line = true;
            }
        }
        _ => {
            if format == Format::Csv {
                print_csv(o, sink);
                printed_end_line = true;
            } else {
                match o {
                    Output::Collection(rows) if rows.is_empty() => sink.w("[]"),
                    _ => {
                        let s = json_string(&result_value(o), Some(4))?;
                        sink.w(&s);
                    }
                }
            }
        }
    }
    if !printed_end_line {
        sink.w("\n");
    }
    Ok(())
}

fn csv_field(v: &str) -> String {
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

fn csv_value(v: &Pv) -> String {
    match v {
        Pv::Str(s) => sanitize::for_csv(s),
        Pv::Date(_) | Pv::Time(_) | Pv::DateTime { .. } => {
            pv_json(v).as_str().unwrap_or_default().to_string()
        }
        Pv::Path(p) => sanitize::for_csv(p),
        Pv::Decimal(_) | Pv::Int(_) | Pv::Float(_) => v.py_str(),
        Pv::ByteArray(b) => hex(b),
        Pv::None => String::new(),
        other => sanitize::for_csv(&other.py_str()),
    }
}

fn write_csv_row(fields: &[String], values: &[String], sink: &mut Sink) {
    let _ = fields;
    let line: Vec<String> = values.iter().map(|v| csv_field(v)).collect();
    sink.w(&(line.join(",") + "\n"));
}

fn print_csv(o: &Output, sink: &mut Sink) {
    match o {
        Output::Collection(rows) => {
            let Some(first) = rows.first() else { return };
            let fields: Vec<String> = first.keys().cloned().collect();
            let header: Vec<String> = fields.iter().map(|f| sanitize::for_csv(f)).collect();
            write_csv_row(&fields, &header, sink);
            for r in rows {
                let vals: Vec<String> = fields
                    .iter()
                    .map(|f| r.get(f).map(csv_value).unwrap_or_default())
                    .collect();
                write_csv_row(&fields, &vals, sink);
            }
        }
        Output::Object(Some(r)) => {
            let fields: Vec<String> = r.keys().cloned().collect();
            let header: Vec<String> = fields.iter().map(|f| sanitize::for_csv(f)).collect();
            write_csv_row(&fields, &header, sink);
            let vals: Vec<String> = r.values().map(csv_value).collect();
            write_csv_row(&fields, &vals, sink);
        }
        Output::Message(m) => {
            write_csv_row(&["message".into()], &["message".into()], sink);
            write_csv_row(&["message".into()], &[sanitize::for_csv(m)], sink);
        }
        _ => {}
    }
}

fn render(sink: &mut Sink, r: &R, width: usize) {
    let s = snow_rich::to_string(r, width, sink.color);
    sink.w(&s);
}

const NON_TERMINAL_WIDTH: usize = 1_000_000;

fn print_unstructured(o: &Output, format: Format, sink: &mut Sink) {
    match o {
        Output::Object(None) => sink.w("No data\n"),
        Output::Object(Some(r)) if r.is_empty() => sink.w("No data\n"),
        Output::Message(m) => {
            sink.w(&sanitize::for_terminal(m));
            sink.w("\n");
        }
        Output::Object(Some(r)) => {
            let mut t = if format == Format::Plain {
                Table::plain_with_header()
            } else {
                Table::ascii_with_header()
            };
            add_col(&mut t, "key");
            add_col(&mut t, "value");
            for (k, v) in r {
                t.add_row(vec![
                    Some(cell_text(&sanitize::for_terminal(k))),
                    Some(cell_text(&sanitize::for_terminal(&v.cell()))),
                ]);
            }
            let w = if sink.is_tty {
                sink.width
            } else {
                NON_TERMINAL_WIDTH
            };
            render(sink, &R::Table(t), w);
        }
        Output::Collection(rows) => print_table(rows, format, sink),
        _ => {}
    }
}

fn add_col(t: &mut Table, name: &str) {
    t.add_column(Column {
        header: Some(Box::new(R::Text(Text::new(name.to_string())))),
        overflow: Overflow::Fold,
        ..Column::default()
    });
}

fn cell_text(s: &str) -> R {
    R::Text(Text::new(s.to_string()))
}

/// `ascii_table.normalize_cell`.
pub fn normalize_cell(v: &Pv) -> String {
    let text = v.cell();
    if text.is_ascii() && text.chars().all(|c| !c.is_control()) {
        return text;
    }
    let s = sanitize::for_terminal(&text);
    let stripped: String = s
        .chars()
        .filter(|c| !matches!(*c as u32, 7 | 8 | 11 | 12 | 13))
        .collect();
    let mut t = Text::new(stripped);
    t.expand_tabs(8);
    t.plain
}

const PROBE_MAX_CELLS: usize = 20_000;
const PROBE_MAX_CHARS: usize = 2 * 1024 * 1024;

fn print_table(rows: &[Row], format: Format, sink: &mut Sink) {
    let Some(first) = rows.first() else {
        sink.w("No data\n\n");
        return;
    };
    let columns: Vec<String> = first.keys().cloned().collect();
    let normalized: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.values().map(normalize_cell).collect())
        .collect();
    if probe_fits(&normalized) {
        let mut t = if format == Format::Plain {
            Table::plain_with_header()
        } else {
            Table::ascii_with_header()
        };
        for c in &columns {
            add_col(&mut t, c);
        }
        for r in &normalized {
            t.add_row(r.iter().map(|c| Some(cell_text(c))).collect());
        }
        let w = if sink.is_tty {
            sink.width
        } else {
            NON_TERMINAL_WIDTH
        };
        let s = snow_rich::to_string(&R::Table(t), w, sink.color);
        // Live leaves the cursor after the last line; the trailing print() ends it.
        sink.w(s.strip_suffix('\n').unwrap_or(&s));
        sink.w("\n");
        return;
    }
    crate::ascii_table::stream(&columns, &normalized, format == Format::Plain, sink);
}

/// The probe stops at 20,000 cells or 2 MiB of text; within that it uses Rich.
fn probe_fits(rows: &[Vec<String>]) -> bool {
    let mut cells = 0;
    let mut chars = 0;
    for (i, r) in rows.iter().enumerate() {
        let rc = r.len();
        let rch: usize = r.iter().map(|c| c.chars().count()).sum();
        if i > 0 && (cells + rc > PROBE_MAX_CELLS || chars + rch > PROBE_MAX_CHARS) {
            return false;
        }
        cells += rc;
        chars += rch;
        if cells > PROBE_MAX_CELLS || chars > PROBE_MAX_CHARS {
            return false;
        }
        if cells >= PROBE_MAX_CELLS || chars >= PROBE_MAX_CHARS {
            return i + 1 == rows.len();
        }
    }
    true
}
