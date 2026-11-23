//! Segments (styled runs of text) and line shaping, ported from `rich.segment`.

use crate::style::Style;
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub style: Style,
}

pub type Line = Vec<Segment>;

impl Segment {
    pub fn new(text: impl Into<String>, style: Style) -> Segment {
        Segment {
            text: text.into(),
            style,
        }
    }
    pub fn plain(text: impl Into<String>) -> Segment {
        Segment::new(text, Style::NULL)
    }
    pub fn line() -> Segment {
        Segment::plain("\n")
    }
    pub fn cell_len(&self) -> usize {
        cell_len(&self.text)
    }
}

pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x20 || (0x7f..0xa0).contains(&cp) {
        return 0;
    }
    c.width().unwrap_or(0)
}

pub fn cell_len(s: &str) -> usize {
    if s.is_ascii() {
        return s.bytes().filter(|b| *b >= 0x20 && *b != 0x7f).count();
    }
    s.chars().map(char_width).sum()
}

/// `rich.cells.set_cell_size`: crop or pad `text` to exactly `total` cells.
pub fn set_cell_size(text: &str, total: usize) -> String {
    let size = cell_len(text);
    if size == total {
        return text.to_string();
    }
    if size < total {
        let mut s = String::with_capacity(text.len() + total - size);
        s.push_str(text);
        s.extend(std::iter::repeat_n(' ', total - size));
        return s;
    }
    let mut out = String::new();
    let mut w = 0;
    for c in text.chars() {
        let cw = char_width(c);
        if w + cw > total {
            if w < total {
                out.push(' ');
            }
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// `rich.cells.chop_cells`: split text into pieces each at most `width` cells.
pub fn chop_cells(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = vec![String::new()];
    let mut total = 0;
    for c in text.chars() {
        let cw = char_width(c);
        if total + cw > width {
            lines.push(c.to_string());
            total = cw;
        } else {
            lines.last_mut().expect("non-empty").push(c);
            total += cw;
        }
    }
    lines
}

pub fn line_len(line: &[Segment]) -> usize {
    line.iter().map(Segment::cell_len).sum()
}

/// `Segment.adjust_line_length`.
pub fn adjust_line_length(line: &[Segment], length: usize, style: Style, pad: bool) -> Line {
    let line_length = line_len(line);
    if line_length < length {
        let mut new_line = line.to_vec();
        if pad {
            new_line.push(Segment::new(" ".repeat(length - line_length), style));
        }
        new_line
    } else if line_length > length {
        let mut new_line = Vec::new();
        let mut used = 0;
        for seg in line {
            let sl = seg.cell_len();
            if used + sl < length {
                new_line.push(seg.clone());
                used += sl;
            } else {
                new_line.push(Segment::new(
                    set_cell_size(&seg.text, length - used),
                    seg.style,
                ));
                break;
            }
        }
        new_line
    } else {
        line.to_vec()
    }
}

/// `Segment.split_and_crop_lines` without trailing newline segments.
pub fn split_and_crop_lines(
    segments: Vec<Segment>,
    length: usize,
    style: Style,
    pad: bool,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut line: Line = Vec::new();
    for seg in segments {
        if seg.text.contains('\n') {
            let mut rest: &str = &seg.text;
            while !rest.is_empty() {
                let (part, newline, after) = match rest.find('\n') {
                    Some(i) => (&rest[..i], true, &rest[i + 1..]),
                    None => (rest, false, ""),
                };
                if !part.is_empty() {
                    line.push(Segment::new(part, seg.style));
                }
                if newline {
                    lines.push(adjust_line_length(&line, length, style, pad));
                    line.clear();
                }
                rest = after;
            }
        } else {
            line.push(seg);
        }
    }
    if !line.is_empty() {
        lines.push(adjust_line_length(&line, length, style, pad));
    }
    lines
}

/// `Segment.split_lines`: split into lines, keeping an unterminated last line.
pub fn split_lines(segments: Vec<Segment>) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut line: Line = Vec::new();
    for seg in segments {
        if seg.text.contains('\n') {
            let mut rest: &str = &seg.text;
            while !rest.is_empty() {
                let (part, newline, after) = match rest.find('\n') {
                    Some(i) => (&rest[..i], true, &rest[i + 1..]),
                    None => (rest, false, ""),
                };
                if !part.is_empty() {
                    line.push(Segment::new(part, seg.style));
                }
                if newline {
                    lines.push(std::mem::take(&mut line));
                }
                rest = after;
            }
        } else {
            line.push(seg);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// `Segment.set_shape`.
pub fn set_shape(lines: &[Line], width: usize, height: Option<usize>, style: Style) -> Vec<Line> {
    let h = height.unwrap_or(lines.len());
    let mut shaped: Vec<Line> = lines
        .iter()
        .map(|l| adjust_line_length(l, width, style, true))
        .collect();
    if shaped.len() < h {
        let blank = vec![Segment::new(" ".repeat(width), style)];
        shaped.resize(h, blank);
    }
    shaped
}

/// `Segment.align_top`.
pub fn align_top(lines: Vec<Line>, width: usize, height: usize, style: Style) -> Vec<Line> {
    if lines.len() >= height {
        let mut l = lines;
        if l.len() > height {
            l.truncate(height);
        }
        return l;
    }
    let mut l = lines;
    let blank = vec![Segment::new(" ".repeat(width), style)];
    l.resize(height, blank);
    l
}

/// Apply `style` underneath every segment (`Segment.apply_style(style=...)`).
pub fn apply_style(segments: Vec<Segment>, style: Style) -> Vec<Segment> {
    if style.is_null() {
        return segments;
    }
    segments
        .into_iter()
        .map(|s| Segment {
            style: style.combine(s.style),
            text: s.text,
        })
        .collect()
}
