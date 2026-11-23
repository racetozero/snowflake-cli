//! Renderables and the layout engine (`rich.console`, `rich.padding`,
//! `rich.panel`, `rich.table`, `rich.columns`, `rich.align`).

use crate::markdown::{self, MdTok};
use crate::segment::{
    Line, Segment, align_top, apply_style, cell_len, line_len, set_shape, split_and_crop_lines,
    split_lines,
};
use crate::style::Style;
use crate::text::{Justify, Overflow, Text};

/// `ConsoleOptions` subset.
#[derive(Debug, Clone, Copy)]
pub struct Opts {
    pub max_width: usize,
    pub justify: Option<Justify>,
    pub overflow: Option<Overflow>,
    pub no_wrap: Option<bool>,
}

impl Opts {
    pub fn new(width: usize) -> Opts {
        Opts {
            max_width: width,
            justify: None,
            overflow: None,
            no_wrap: None,
        }
    }
    pub fn width(self, w: usize) -> Opts {
        Opts {
            max_width: w,
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct M {
    pub min: usize,
    pub max: usize,
}

impl M {
    fn normalize(self) -> M {
        let min = self.min.min(self.max);
        M {
            min,
            max: self.max.max(min),
        }
    }
    fn with_maximum(self, w: usize) -> M {
        M {
            min: self.min.min(w),
            max: self.max.min(w),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Column {
    /// Header renderable (shown when the table has `show_header`).
    pub header: Option<Box<R>>,
    pub width: Option<usize>,
    pub ratio: Option<usize>,
    pub no_wrap: bool,
    pub justify: Justify,
    pub overflow: Overflow,
    pub style: Style,
    pub cells: Vec<R>,
}

impl Default for Column {
    fn default() -> Self {
        Column {
            header: None,
            width: None,
            ratio: None,
            no_wrap: false,
            justify: Justify::Left,
            overflow: Overflow::Ellipsis,
            style: Style::NULL,
            cells: Vec::new(),
        }
    }
}

/// Rich `Table`: box-less (`box=None`) unless `ascii_box`, with an optional header row.
#[derive(Debug, Clone)]
pub struct Table {
    pub columns: Vec<Column>,
    /// (top, right, bottom, left)
    pub padding: (usize, usize, usize, usize),
    pub pad_edge: bool,
    pub collapse_padding: bool,
    pub expand: bool,
    /// `box.ASCII` borders when true; box-less otherwise.
    pub ascii_box: bool,
    pub show_header: bool,
    pub show_edge: bool,
    pub header_style: Style,
    rows: usize,
}

/// One `rich.box.Box` (only the ASCII box is used for results).
struct BoxChars {
    top: [&'static str; 4],
    head: [&'static str; 3],
    head_row: [&'static str; 4],
    mid: [&'static str; 3],
    bottom: [&'static str; 4],
}

const ASCII: BoxChars = BoxChars {
    top: ["+", "-", "-", "+"],
    head: ["|", "|", "|"],
    head_row: ["|", "-", "+", "|"],
    mid: ["|", "|", "|"],
    bottom: ["+", "-", "-", "+"],
};

fn box_line(parts: [&str; 4], widths: &[usize], edge: bool) -> String {
    let mut s = String::new();
    if edge {
        s.push_str(parts[0]);
    }
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            s.push_str(parts[2]);
        }
        s.push_str(&parts[1].repeat(*w));
    }
    if edge {
        s.push_str(parts[3]);
    }
    s
}

impl Table {
    pub fn new(padding: (usize, usize, usize, usize), pad_edge: bool, expand: bool) -> Table {
        Table {
            columns: Vec::new(),
            padding,
            pad_edge,
            collapse_padding: false,
            expand,
            ascii_box: false,
            show_header: false,
            show_edge: true,
            header_style: Style::NULL,
            rows: 0,
        }
    }

    /// `Table(show_header=True, box=box.ASCII)`.
    pub fn ascii_with_header() -> Table {
        let mut t = Table::new((0, 1, 0, 1), true, false);
        t.ascii_box = true;
        t.show_header = true;
        t.header_style = Style::parse("bold");
        t
    }

    /// `Table(show_header=True, box=None, show_edge=False, pad_edge=False)`.
    pub fn plain_with_header() -> Table {
        let mut t = Table::new((0, 1, 0, 1), false, false);
        t.show_header = true;
        t.show_edge = false;
        t.header_style = Style::parse("bold");
        t
    }

    fn extra_width(&self) -> usize {
        let mut w = 0;
        if self.ascii_box && self.show_edge {
            w += 2;
        }
        if self.ascii_box {
            w += self.columns.len().saturating_sub(1);
        }
        w
    }
    /// `Table.grid(padding=...)`: no edge padding, collapsed padding.
    pub fn grid(padding: (usize, usize, usize, usize)) -> Table {
        let mut t = Table::new(padding, false, false);
        t.collapse_padding = true;
        t
    }
    pub fn add_column(&mut self, c: Column) {
        self.columns.push(c);
    }
    pub fn add_row(&mut self, cells: Vec<Option<R>>) {
        let mut cells = cells;
        while cells.len() < self.columns.len() {
            cells.push(None);
        }
        for (i, cell) in cells.into_iter().enumerate() {
            if i == self.columns.len() {
                let mut c = Column::default();
                for _ in 0..self.rows {
                    c.cells.push(R::Text(Text::new("")));
                }
                self.columns.push(c);
            }
            self.columns[i]
                .cells
                .push(cell.unwrap_or_else(|| R::Text(Text::new(""))));
        }
        self.rows += 1;
    }
}

#[derive(Debug, Clone)]
pub enum R {
    Text(Text),
    Markdown {
        toks: &'static [MdTok],
        style: Style,
    },
    Group(Vec<R>),
    Padding {
        inner: Box<R>,
        pad: (usize, usize, usize, usize),
        expand: bool,
        style: Style,
    },
    Panel {
        inner: Box<R>,
        title: Option<String>,
        border_style: Style,
    },
    Table(Table),
    Columns(Vec<R>),
    /// `Align(renderable, "left", pad=False)`.
    Align(Box<R>),
    Constrain(Box<R>, usize),
    MdList {
        items: Vec<Vec<R>>,
        ordered: bool,
        start: usize,
    },
    /// A Markdown code block (`rich.syntax.Syntax(..., word_wrap=True, padding=1)`).
    Syntax(String),
    /// A title followed by a URL folded after `/` (Snowflake CLI's `_RelatedTopic`).
    RelatedTopic {
        title: String,
        url: String,
    },
}

impl R {
    pub fn text(s: impl Into<String>) -> R {
        R::Text(Text::new(s))
    }
}

/// `Measurement.get`.
pub fn measure(r: &R, o: Opts) -> M {
    let w = o.max_width;
    if w < 1 {
        return M { min: 0, max: 0 };
    }
    match rich_measure(r, o) {
        Some(m) => {
            let m = m.normalize().with_maximum(w);
            if m.max < 1 {
                M { min: 0, max: 0 }
            } else {
                m.normalize()
            }
        }
        None => M { min: 0, max: w },
    }
}

fn measure_renderables(rs: &[R], o: Opts) -> M {
    if rs.is_empty() {
        return M { min: 0, max: 0 };
    }
    let ms: Vec<M> = rs.iter().map(|r| measure(r, o)).collect();
    M {
        min: ms.iter().map(|m| m.min).max().unwrap_or(0),
        max: ms.iter().map(|m| m.max).max().unwrap_or(0),
    }
}

/// The renderable's own `__rich_measure__`, or `None` if it has none.
fn rich_measure(r: &R, o: Opts) -> Option<M> {
    match r {
        R::Text(t) => {
            let (min, max) = t.measure();
            Some(M { min, max })
        }
        R::Group(rs) => Some(measure_renderables(rs, o)),
        R::Padding { inner, pad, .. } => {
            let extra = pad.1 + pad.3;
            if o.max_width < extra + 1 {
                return Some(M {
                    min: o.max_width,
                    max: o.max_width,
                });
            }
            let m = measure(inner, o);
            Some(
                M {
                    min: m.min + extra,
                    max: m.max + extra,
                }
                .with_maximum(o.max_width),
            )
        }
        R::Panel { inner, title, .. } => {
            let padding = 2;
            let inner_o = o.width(o.max_width.saturating_sub(padding + 2));
            let mut m = measure(inner, inner_o).max;
            if let Some(t) = title {
                m = m.max(cell_len(t) + 2);
            }
            let width = m + padding + 2;
            Some(M {
                min: width,
                max: width,
            })
        }
        R::Table(t) => Some(table_measure(t, o)),
        R::Align(inner) => Some(measure(inner, o)),
        R::Constrain(inner, width) => Some(measure(inner, o.width((*width).min(o.max_width)))),
        R::Syntax(code) => {
            let max = code.lines().map(cell_len).max().unwrap_or(0) + 2;
            Some(M { min: 0, max })
        }
        R::Markdown { .. } | R::Columns(_) | R::MdList { .. } | R::RelatedTopic { .. } => None,
    }
}

/// `Console.render`.
pub fn render(r: &R, o: Opts) -> Vec<Segment> {
    match r {
        R::Text(t) => render_text(t, o),
        R::Markdown { toks, style } => markdown::render(toks, *style, o),
        R::Group(rs) => rs.iter().flat_map(|r| render(r, o)).collect(),
        R::Padding {
            inner,
            pad,
            expand,
            style,
        } => render_padding(inner, *pad, *expand, *style, o),
        R::Panel {
            inner,
            title,
            border_style,
        } => render_panel(inner, title.as_deref(), *border_style, o),
        R::Table(t) => render_table(t, o),
        R::Columns(items) => render_columns(items, o),
        R::Align(inner) => render_align(inner, o),
        R::Constrain(inner, width) => render(inner, o.width((*width).min(o.max_width))),
        R::MdList {
            items,
            ordered,
            start,
        } => markdown::render_list(items, *ordered, *start, o),
        R::Syntax(code) => render_syntax(code, o),
        R::RelatedTopic { title, url } => {
            let mut out = Vec::new();
            if !title.is_empty() {
                out.extend(render_text(&Text::new(title.clone()), o));
            }
            for line in fold_at_slash(url, o.max_width) {
                out.extend(render_text(&Text::new(line), o));
            }
            out
        }
    }
}

/// Snowflake CLI's `_fold_at_slash`.
pub fn fold_at_slash(url: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![url.to_string()];
    }
    let segments: Vec<&str> = url.split('/').collect();
    let mut chunks: Vec<String> = segments[..segments.len() - 1]
        .iter()
        .map(|s| format!("{s}/"))
        .collect();
    chunks.push(segments[segments.len() - 1].to_string());
    let mut lines = Vec::new();
    let mut current = String::new();
    for chunk in chunks {
        if !current.is_empty() && current.chars().count() + chunk.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        current.push_str(&chunk);
        while current.chars().count() > width {
            let head: String = current.chars().take(width).collect();
            let tail: String = current.chars().skip(width).collect();
            lines.push(head);
            current = tail;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(url.to_string());
    }
    lines
}

pub fn render_text(t: &Text, o: Opts) -> Vec<Segment> {
    let justify = t.justify.or(o.justify).unwrap_or(Justify::Default);
    let overflow = t.overflow.or(o.overflow).unwrap_or(Overflow::Fold);
    let no_wrap = t.no_wrap.or(o.no_wrap).unwrap_or(false);
    let lines = t.wrap(o.max_width, justify, overflow, no_wrap);
    Text::join_lines(lines).render("\n")
}

/// `Console.render_lines`.
pub fn render_lines(r: &R, o: Opts, style: Style, pad: bool) -> Vec<Line> {
    let segs = apply_style(render(r, o), style);
    split_and_crop_lines(segs, o.max_width, style, pad)
}

fn render_padding(
    inner: &R,
    pad: (usize, usize, usize, usize),
    expand: bool,
    style: Style,
    o: Opts,
) -> Vec<Segment> {
    let (top, right, bottom, left) = pad;
    let width = if expand {
        o.max_width
    } else {
        (measure(inner, o).max + left + right).min(o.max_width)
    };
    let inner_o = o.width(width.saturating_sub(left + right));
    let lines = render_lines(inner, inner_o, style, true);
    let mut out = Vec::new();
    let blank = || Segment::new(format!("{}\n", " ".repeat(width)), style);
    for _ in 0..top {
        out.push(blank());
    }
    for line in lines {
        if left > 0 {
            out.push(Segment::new(" ".repeat(left), style));
        }
        out.extend(line);
        if right > 0 {
            out.push(Segment::new(" ".repeat(right), style));
        }
        out.push(Segment::line());
    }
    for _ in 0..bottom {
        out.push(blank());
    }
    out
}

fn render_panel(inner: &R, title: Option<&str>, border: Style, o: Opts) -> Vec<Segment> {
    // Panel default: padding (0, 1), expand=True, ROUNDED box.
    let padded = R::Padding {
        inner: Box::new(inner.clone()),
        pad: (0, 1, 0, 1),
        expand: true,
        style: Style::NULL,
    };
    let width = o.max_width;
    let title_text = title.map(|t| format!(" {} ", t.replace('\n', " ")));
    let mut child_width = width.saturating_sub(2);
    if let Some(t) = &title_text {
        child_width = (o.max_width.saturating_sub(2)).min(child_width.max(cell_len(t) + 2));
    }
    let width = child_width + 2;
    let lines = render_lines(&padded, o.width(child_width), Style::NULL, true);
    let mut out = Vec::new();
    match &title_text {
        Some(t) if width > 4 => {
            let avail = width - 4;
            let mut tt = Text::new(t.clone());
            tt.truncate(avail, Overflow::Fold, false);
            let excess = avail.saturating_sub(tt.cell_len());
            out.push(Segment::new("╭─", border));
            out.push(Segment::new(tt.plain, border));
            if excess > 0 {
                out.push(Segment::new("─".repeat(excess), border));
            }
            out.push(Segment::new("─╮", border));
        }
        _ => {
            out.push(Segment::new(
                format!("╭{}╮", "─".repeat(width.saturating_sub(2))),
                border,
            ));
        }
    }
    out.push(Segment::line());
    for line in lines {
        out.push(Segment::new("│", border));
        out.extend(line);
        out.push(Segment::new("│", border));
        out.push(Segment::line());
    }
    out.push(Segment::new(
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        border,
    ));
    out.push(Segment::line());
    out
}

fn render_align(inner: &R, o: Opts) -> Vec<Segment> {
    let width = measure(inner, o).max;
    let rendered = render(&R::Constrain(Box::new(inner.clone()), width), o);
    let lines = split_lines(rendered);
    let shape_w = lines.iter().map(|l| line_len(l)).max().unwrap_or(0);
    let lines = set_shape(&lines, shape_w, None, Style::NULL);
    let mut out = Vec::new();
    for l in lines {
        out.extend(l);
        out.push(Segment::line());
    }
    out
}

fn render_syntax(code: &str, o: Opts) -> Vec<Segment> {
    // rich.syntax.Syntax(code, word_wrap=True, padding=1): every line is
    // wrapped at the full width, then the result sits inside Padding(1),
    // which crops each line to width - 2.
    let style = crate::style::theme("markdown.code_block");
    let width = o.max_width;
    let code = dedent(&format!("{code}\n"));
    let mut lines: Vec<Line> = Vec::new();
    let mut parts: Vec<&str> = code.split('\n').collect();
    if code.ends_with('\n') {
        parts.pop();
    }
    for line in parts {
        let mut t = Text::new(crate::text::expand_tabs(line, 8));
        t.style = style;
        let r = R::Text(t);
        lines.extend(render_lines(
            &r,
            Opts {
                justify: Some(Justify::Left),
                ..o.width(width)
            },
            style,
            true,
        ));
    }
    let inner = width.saturating_sub(2);
    let mut out = Vec::new();
    out.push(Segment::new(format!("{}\n", " ".repeat(width)), style));
    for l in lines {
        out.push(Segment::new(" ", style));
        out.extend(crate::segment::adjust_line_length(&l, inner, style, true));
        out.push(Segment::new(" ", style));
        out.push(Segment::line());
    }
    out.push(Segment::new(format!("{}\n", " ".repeat(width)), style));
    out
}

/// Python `textwrap.dedent`.
pub fn dedent(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut margin: Option<String> = None;
    for l in &lines {
        if l.trim().is_empty() {
            continue;
        }
        let indent: String = l.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        margin = Some(match margin {
            None => indent,
            Some(m) => {
                let common: String = m
                    .chars()
                    .zip(indent.chars())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a)
                    .collect();
                common
            }
        });
    }
    let m = margin.unwrap_or_default();
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                l.strip_prefix(m.as_str()).unwrap_or(l).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn padding_width(t: &Table, index: usize) -> usize {
    let (_, right, _, mut left) = t.padding;
    if t.collapse_padding && index > 0 {
        left = left.saturating_sub(right);
    }
    left + right
}

fn cell_padding(
    t: &Table,
    col: usize,
    first_row: bool,
    last_row: bool,
) -> (usize, usize, usize, usize) {
    let (mut top, mut right, mut bottom, mut left) = t.padding;
    let first_col = col == 0;
    let last_col = col + 1 == t.columns.len();
    if t.collapse_padding {
        if !first_col {
            left = left.saturating_sub(right);
        }
        if !last_row {
            bottom = top.saturating_sub(bottom);
        }
    }
    if !t.pad_edge {
        if first_col {
            left = 0;
        }
        if last_col {
            right = 0;
        }
        if first_row {
            top = 0;
        }
        if last_row {
            bottom = 0;
        }
    }
    (top, right, bottom, left)
}

fn cells(t: &Table, ci: usize) -> Vec<R> {
    let col = &t.columns[ci];
    let any_padding = t.padding != (0, 0, 0, 0);
    let mut raw: Vec<R> = Vec::with_capacity(col.cells.len() + 1);
    if t.show_header {
        raw.push(
            col.header
                .as_deref()
                .cloned()
                .unwrap_or_else(|| R::text("")),
        );
    }
    raw.extend(col.cells.iter().cloned());
    let n = raw.len();
    raw.iter()
        .enumerate()
        .map(|(ri, c)| {
            if any_padding {
                R::Padding {
                    inner: Box::new(c.clone()),
                    pad: cell_padding(t, ci, ri == 0, ri + 1 == n),
                    expand: true,
                    style: Style::NULL,
                }
            } else {
                c.clone()
            }
        })
        .collect()
}

fn measure_column(t: &Table, o: Opts, ci: usize) -> M {
    let max_width = o.max_width;
    if max_width < 1 {
        return M { min: 0, max: 0 };
    }
    let pw = padding_width(t, ci);
    let col = &t.columns[ci];
    if let Some(w) = col.width {
        return M {
            min: w + pw,
            max: w + pw,
        }
        .with_maximum(max_width);
    }
    let cs = cells(t, ci);
    let mut mins = Vec::with_capacity(cs.len());
    let mut maxs = Vec::with_capacity(cs.len());
    for c in &cs {
        let m = measure(c, o);
        mins.push(m.min);
        maxs.push(m.max);
    }
    M {
        min: mins.iter().copied().max().unwrap_or(1),
        max: maxs.iter().copied().max().unwrap_or(max_width),
    }
    .with_maximum(max_width)
}

fn py_round(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `rich._ratio.ratio_reduce`.
pub fn ratio_reduce(
    total: usize,
    ratios: &[usize],
    maximums: &[usize],
    values: &[usize],
) -> Vec<usize> {
    let ratios: Vec<usize> = ratios
        .iter()
        .zip(maximums)
        .map(|(r, m)| if *m != 0 { *r } else { 0 })
        .collect();
    let mut total_ratio: usize = ratios.iter().sum();
    if total_ratio == 0 {
        return values.to_vec();
    }
    let mut total_remaining = total as i64;
    let mut result = Vec::with_capacity(values.len());
    for ((ratio, maximum), value) in ratios.iter().zip(maximums).zip(values) {
        if *ratio != 0 && total_ratio > 0 {
            let d = (*maximum as i64).min(py_round(
                *ratio as f64 * total_remaining as f64 / total_ratio as f64,
            ));
            result.push((*value as i64 - d).max(0) as usize);
            total_remaining -= d;
            total_ratio -= ratio;
        } else {
            result.push(*value);
        }
    }
    result
}

/// `rich._ratio.ratio_distribute`.
pub fn ratio_distribute(total: usize, ratios: &[usize], minimums: Option<&[usize]>) -> Vec<usize> {
    let ratios: Vec<usize> = match minimums {
        Some(m) => ratios
            .iter()
            .zip(m)
            .map(|(r, mn)| if *mn != 0 { *r } else { 0 })
            .collect(),
        None => ratios.to_vec(),
    };
    let mut total_ratio: usize = ratios.iter().sum();
    let mut total_remaining = total as i64;
    let mut out = Vec::with_capacity(ratios.len());
    for (i, ratio) in ratios.iter().enumerate() {
        let minimum = minimums.map(|m| m[i]).unwrap_or(0) as i64;
        let d = if total_ratio > 0 {
            minimum.max((*ratio as f64 * total_remaining as f64 / total_ratio as f64).ceil() as i64)
        } else {
            total_remaining
        };
        out.push(d.max(0) as usize);
        total_ratio -= ratio;
        total_remaining -= d;
    }
    out
}

fn collapse_widths(widths: &[usize], wrapable: &[bool], max_width: usize) -> Vec<usize> {
    let mut widths = widths.to_vec();
    let mut total: usize = widths.iter().sum();
    let mut excess = total as i64 - max_width as i64;
    if wrapable.iter().any(|w| *w) {
        while total > 0 && excess > 0 {
            let max_column = widths
                .iter()
                .zip(wrapable)
                .filter(|(_, w)| **w)
                .map(|(x, _)| *x)
                .max()
                .unwrap_or(0);
            let second = widths
                .iter()
                .zip(wrapable)
                .map(|(x, w)| if *w && *x != max_column { *x } else { 0 })
                .max()
                .unwrap_or(0);
            let diff = max_column - second;
            let ratios: Vec<usize> = widths
                .iter()
                .zip(wrapable)
                .map(|(x, w)| usize::from(*x == max_column && *w))
                .collect();
            if ratios.iter().all(|r| *r == 0) || diff == 0 {
                break;
            }
            let max_reduce = vec![(excess as usize).min(diff); widths.len()];
            widths = ratio_reduce(excess as usize, &ratios, &max_reduce, &widths);
            total = widths.iter().sum();
            excess = total as i64 - max_width as i64;
        }
    }
    widths
}

fn calculate_column_widths(t: &Table, o: Opts) -> Vec<usize> {
    let max_width = o.max_width;
    let ranges: Vec<M> = (0..t.columns.len())
        .map(|i| measure_column(t, o, i))
        .collect();
    let mut widths: Vec<usize> = ranges
        .iter()
        .map(|r| if r.max == 0 { 1 } else { r.max })
        .collect();
    if t.expand {
        let ratios: Vec<usize> = t.columns.iter().filter_map(|c| c.ratio).collect();
        if ratios.iter().any(|r| *r != 0) {
            let fixed: Vec<usize> = ranges
                .iter()
                .zip(&t.columns)
                .map(|(r, c)| if c.ratio.is_some() { 0 } else { r.max })
                .collect();
            let flex_min: Vec<usize> = t
                .columns
                .iter()
                .enumerate()
                .filter(|(_, c)| c.ratio.is_some())
                .map(|(i, c)| c.width.unwrap_or(1) + padding_width(t, i))
                .collect();
            let flexible_width = max_width.saturating_sub(fixed.iter().sum());
            let flex = ratio_distribute(flexible_width, &ratios, Some(&flex_min));
            let mut it = flex.into_iter();
            for (i, c) in t.columns.iter().enumerate() {
                if c.ratio.is_some() {
                    widths[i] = fixed[i] + it.next().unwrap_or(0);
                }
            }
        }
    }
    let mut table_width: usize = widths.iter().sum();
    if table_width > max_width {
        let wrapable: Vec<bool> = t
            .columns
            .iter()
            .map(|c| c.width.is_none() && !c.no_wrap)
            .collect();
        widths = collapse_widths(&widths, &wrapable, max_width);
        table_width = widths.iter().sum();
        if table_width > max_width {
            let excess = table_width - max_width;
            let ones = vec![1; widths.len()];
            widths = ratio_reduce(excess, &ones, &widths.clone(), &widths);
            table_width = widths.iter().sum();
        }
        let ranges: Vec<M> = widths
            .iter()
            .enumerate()
            .map(|(i, w)| measure_column(t, o.width(*w), i))
            .collect();
        // Rich keeps the pre-measurement total here.
        widths = ranges.iter().map(|r| r.max).collect();
    }
    if table_width < max_width && t.expand {
        let pads = ratio_distribute(max_width - table_width, &widths, None);
        widths = widths.iter().zip(pads).map(|(w, p)| w + p).collect();
    }
    widths
}

fn table_measure(t: &Table, o: Opts) -> M {
    let extra = t.extra_width();
    let max_width = o.max_width;
    let total: usize = calculate_column_widths(t, o.width(max_width.saturating_sub(extra)))
        .iter()
        .sum();
    let ms: Vec<M> = (0..t.columns.len())
        .map(|i| measure_column(t, o.width(total), i))
        .collect();
    M {
        min: ms.iter().map(|m| m.min).sum::<usize>() + extra,
        max: ms.iter().map(|m| m.max).sum::<usize>() + extra,
    }
}

fn render_table(t: &Table, o: Opts) -> Vec<Segment> {
    if t.columns.is_empty() {
        return vec![Segment::line()];
    }
    let extra = t.extra_width();
    let widths = calculate_column_widths(t, o.width(o.max_width.saturating_sub(extra)));
    let col_cells: Vec<Vec<R>> = (0..t.columns.len()).map(|i| cells(t, i)).collect();
    let nrows = col_cells.first().map(|c| c.len()).unwrap_or(0);
    let mut out = Vec::new();
    if t.ascii_box && t.show_edge {
        out.push(Segment::plain(box_line(ASCII.top, &widths, true)));
        out.push(Segment::line());
    }
    #[allow(clippy::needless_range_loop)]
    for row in 0..nrows {
        let header_row = t.show_header && row == 0;
        let mut rendered: Vec<Vec<Line>> = Vec::with_capacity(widths.len());
        let mut max_height = 1;
        for (ci, w) in widths.iter().enumerate() {
            let col = &t.columns[ci];
            let ro = Opts {
                max_width: *w,
                justify: Some(col.justify),
                no_wrap: Some(col.no_wrap),
                overflow: Some(col.overflow),
            };
            let style = if header_row {
                t.header_style
            } else {
                col.style
            };
            let lines = render_lines(&col_cells[ci][row], ro, style, true);
            max_height = max_height.max(lines.len());
            rendered.push(lines);
        }
        let row_height = rendered.iter().map(|c| c.len()).max().unwrap_or(0);
        let shaped: Vec<Vec<Line>> = rendered
            .into_iter()
            .zip(&widths)
            .enumerate()
            .map(|(ci, (cell, w))| {
                let st = if header_row {
                    t.header_style
                } else {
                    t.columns[ci].style
                };
                if header_row {
                    // Header cells align to the bottom.
                    let mut c = cell;
                    let missing = row_height.saturating_sub(c.len());
                    let mut v: Vec<Line> = (0..missing)
                        .map(|_| vec![Segment::new(" ".repeat(*w), st)])
                        .collect();
                    v.append(&mut c);
                    set_shape(&v, *w, Some(max_height), st)
                } else {
                    set_shape(
                        &align_top(cell, *w, row_height, st),
                        *w,
                        Some(max_height),
                        st,
                    )
                }
            })
            .collect();
        let (left, div, right) = if header_row {
            (ASCII.head[0], ASCII.head[1], ASCII.head[2])
        } else {
            (ASCII.mid[0], ASCII.mid[1], ASCII.mid[2])
        };
        for line_no in 0..max_height {
            if t.ascii_box && t.show_edge {
                out.push(Segment::plain(left));
            }
            for (ci, cell) in shaped.iter().enumerate() {
                out.extend(cell[line_no].iter().cloned());
                if t.ascii_box && ci + 1 < shaped.len() {
                    out.push(Segment::plain(div));
                }
            }
            if t.ascii_box && t.show_edge {
                out.push(Segment::plain(right));
            }
            out.push(Segment::line());
        }
        if t.ascii_box && header_row {
            out.push(Segment::plain(box_line(
                ASCII.head_row,
                &widths,
                t.show_edge,
            )));
            out.push(Segment::line());
        }
    }
    if t.ascii_box && t.show_edge {
        out.push(Segment::plain(box_line(ASCII.bottom, &widths, true)));
        out.push(Segment::line());
    }
    out
}

fn render_columns(items: &[R], o: Opts) -> Vec<Segment> {
    if items.is_empty() {
        return Vec::new();
    }
    let width_padding = 1; // max(left, right) of padding (0, 1)
    let max_width = o.max_width;
    let widths_of: Vec<usize> = items.iter().map(|r| measure(r, o).max).collect();
    let mut column_count = items.len();
    let iter = |count: usize| -> Vec<(usize, Option<usize>)> {
        let mut v: Vec<(usize, Option<usize>)> = widths_of
            .iter()
            .copied()
            .zip((0..items.len()).map(Some))
            .collect();
        if !items.len().is_multiple_of(count) {
            for _ in 0..(count - items.len() % count) {
                v.push((0, None));
            }
        }
        v
    };
    while column_count > 1 {
        let mut widths: std::collections::BTreeMap<usize, usize> = Default::default();
        let mut column_no = 0;
        let mut broke = false;
        for (rw, _) in iter(column_count) {
            let e = widths.entry(column_no).or_insert(0);
            *e = (*e).max(rw);
            let total: usize = widths.values().sum::<usize>() + width_padding * (widths.len() - 1);
            if total > max_width {
                column_count = widths.len() - 1;
                broke = true;
                break;
            } else {
                column_no = (column_no + 1) % column_count;
            }
        }
        if !broke {
            break;
        }
    }
    let column_count = column_count.max(1);
    let mut table = Table::grid((0, 1, 0, 1));
    let order = iter(column_count);
    for chunk in order.chunks(column_count) {
        table.add_row(
            chunk
                .iter()
                .map(|(_, idx)| idx.map(|i| items[i].clone()))
                .collect(),
        );
    }
    render_table(&table, o)
}

/// Write segments as text, with ANSI styles when `color` is true.
pub fn segments_to_string(segs: &[Segment], color: bool) -> String {
    let mut s = String::new();
    for seg in segs {
        if color && !seg.style.is_null() && seg.text != "\n" {
            let sgr = seg.style.sgr();
            if sgr.is_empty() {
                s.push_str(&seg.text);
            } else {
                // Style each line separately so newlines are never inside an SGR run.
                let mut first = true;
                for part in seg.text.split('\n') {
                    if !first {
                        s.push('\n');
                    }
                    first = false;
                    if !part.is_empty() {
                        s.push_str("\x1b[");
                        s.push_str(&sgr);
                        s.push('m');
                        s.push_str(part);
                        s.push_str("\x1b[0m");
                    }
                }
            }
        } else {
            s.push_str(&seg.text);
        }
    }
    s
}
