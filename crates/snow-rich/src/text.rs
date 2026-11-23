//! Styled text with Rich's wrapping rules (`rich.text.Text`, `rich._wrap`).

use crate::segment::{Segment, cell_len, chop_cells, set_cell_size};
use crate::style::Style;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Justify {
    Default,
    Left,
    Center,
    Right,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    Fold,
    Crop,
    Ellipsis,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub style: Style,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub plain: String,
    pub spans: Vec<Span>,
    pub style: Style,
    pub justify: Option<Justify>,
    pub overflow: Option<Overflow>,
    pub no_wrap: Option<bool>,
    pub end: &'static str,
}

impl Default for Text {
    fn default() -> Self {
        Text {
            plain: String::new(),
            spans: Vec::new(),
            style: Style::NULL,
            justify: None,
            overflow: None,
            no_wrap: None,
            end: "\n",
        }
    }
}

fn char_len(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().count()
    }
}

fn byte_at(s: &str, char_idx: usize) -> usize {
    if s.is_ascii() {
        return char_idx.min(s.len());
    }
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

impl Text {
    pub fn new(s: impl Into<String>) -> Text {
        Text {
            plain: s.into(),
            ..Text::default()
        }
    }
    pub fn styled(s: impl Into<String>, style: Style) -> Text {
        Text {
            plain: s.into(),
            style,
            ..Text::default()
        }
    }
    pub fn with_no_wrap(mut self, v: bool) -> Text {
        self.no_wrap = Some(v);
        self
    }
    pub fn with_justify(mut self, j: Justify) -> Text {
        self.justify = Some(j);
        self
    }
    pub fn with_overflow(mut self, o: Overflow) -> Text {
        self.overflow = Some(o);
        self
    }

    pub fn len(&self) -> usize {
        char_len(&self.plain)
    }
    pub fn is_empty(&self) -> bool {
        self.plain.is_empty()
    }
    pub fn cell_len(&self) -> usize {
        cell_len(&self.plain)
    }

    pub fn append(&mut self, s: &str, style: Style) {
        if s.is_empty() {
            return;
        }
        let start = self.len();
        self.plain.push_str(s);
        if !style.is_null() {
            self.spans.push(Span {
                start,
                end: start + char_len(s),
                style,
            });
        }
    }

    pub fn append_text(&mut self, other: &Text) {
        let offset = self.len();
        if !other.style.is_null() && !other.plain.is_empty() {
            self.spans.push(Span {
                start: offset,
                end: offset + other.len(),
                style: other.style,
            });
        }
        self.plain.push_str(&other.plain);
        for sp in &other.spans {
            self.spans.push(Span {
                start: sp.start + offset,
                end: sp.end + offset,
                style: sp.style,
            });
        }
    }

    /// Add a style over a range of characters (used by highlighters).
    pub fn stylize(&mut self, start: usize, end: usize, style: Style) {
        if start < end && !style.is_null() {
            self.spans.push(Span { start, end, style });
        }
    }

    fn copy_meta(&self, plain: String) -> Text {
        Text {
            plain,
            spans: Vec::new(),
            style: self.style,
            justify: self.justify,
            overflow: self.overflow,
            no_wrap: self.no_wrap,
            end: self.end,
        }
    }

    /// `Text.divide`.
    pub fn divide(&self, offsets: &[usize]) -> Vec<Text> {
        if offsets.is_empty() {
            return vec![self.clone()];
        }
        let total = self.len();
        let mut bounds = Vec::with_capacity(offsets.len() + 2);
        bounds.push(0);
        bounds.extend_from_slice(offsets);
        bounds.push(total);
        let mut lines: Vec<Text> = Vec::with_capacity(bounds.len() - 1);
        let ascii = self.plain.is_ascii();
        let byte = |c: usize| {
            if ascii {
                c.min(self.plain.len())
            } else {
                byte_at(&self.plain, c)
            }
        };
        for w in bounds.windows(2) {
            let (a, b) = (w[0], w[1]);
            let piece = if a <= b {
                &self.plain[byte(a)..byte(b)]
            } else {
                ""
            };
            lines.push(self.copy_meta(piece.to_string()));
        }
        for sp in &self.spans {
            for (i, w) in bounds.windows(2).enumerate() {
                let (a, b) = (w[0], w[1]);
                if sp.end <= a || sp.start >= b {
                    continue;
                }
                let s = sp.start.max(a) - a;
                let e = sp.end.min(b) - a;
                if s < e {
                    lines[i].spans.push(Span {
                        start: s,
                        end: e,
                        style: sp.style,
                    });
                }
            }
        }
        lines
    }

    /// `Text.split("\n", allow_blank=...)`.
    pub fn split_lines(&self, allow_blank: bool) -> Vec<Text> {
        if !self.plain.contains('\n') {
            return vec![self.clone()];
        }
        let mut offsets = Vec::new();
        for (ci, c) in self.plain.chars().enumerate() {
            if c == '\n' {
                offsets.push(ci);
                offsets.push(ci + 1);
            }
        }
        let mut lines: Vec<Text> = self
            .divide(&offsets)
            .into_iter()
            .filter(|l| l.plain != "\n")
            .collect();
        if !allow_blank && self.plain.ends_with('\n') {
            lines.pop();
        }
        lines
    }

    fn trim_spans(&mut self) {
        let max = self.len();
        self.spans.retain(|s| s.start < max);
        for s in &mut self.spans {
            if s.end > max {
                s.end = max;
            }
        }
    }

    pub fn right_crop(&mut self, amount: usize) {
        if amount == 0 {
            return;
        }
        let len = self.len();
        let keep = len.saturating_sub(amount);
        let b = byte_at(&self.plain, keep);
        self.plain.truncate(b);
        self.trim_spans();
    }

    pub fn rstrip(&mut self) {
        let t = self.plain.trim_end().len();
        self.plain.truncate(t);
        self.trim_spans();
    }

    /// `Text.rstrip_end`.
    pub fn rstrip_end(&mut self, size: usize) {
        let len = self.len();
        if len > size {
            let excess = len - size;
            let trimmed = self.plain.trim_end_matches(|c: char| c.is_whitespace());
            let ws = char_len(&self.plain[trimmed.len()..]);
            if ws > 0 {
                self.right_crop(ws.min(excess));
            }
        }
    }

    pub fn pad_right(&mut self, count: usize) {
        self.plain.extend(std::iter::repeat_n(' ', count));
    }

    pub fn pad_left(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.plain.insert_str(0, &" ".repeat(count));
        for s in &mut self.spans {
            s.start += count;
            s.end += count;
        }
    }

    /// `Text.truncate`.
    pub fn truncate(&mut self, max_width: usize, overflow: Overflow, pad: bool) {
        if overflow != Overflow::Ignore {
            let length = cell_len(&self.plain);
            if length > max_width {
                if overflow == Overflow::Ellipsis {
                    self.plain = set_cell_size(&self.plain, max_width.saturating_sub(1)) + "…";
                } else {
                    self.plain = set_cell_size(&self.plain, max_width);
                }
                self.trim_spans();
            }
            if pad && length < max_width {
                self.pad_right(max_width - length);
            }
        }
    }

    pub fn expand_tabs(&mut self, tab_size: usize) {
        if !self.plain.contains('\t') {
            return;
        }
        // Spans are dropped across tab expansion; tabs only occur in unstyled text.
        let mut out = String::new();
        for line in self.plain.split_inclusive('\n') {
            let mut col = 0;
            for c in line.chars() {
                if c == '\t' {
                    let n = tab_size - (col % tab_size);
                    out.extend(std::iter::repeat_n(' ', n));
                    col += n;
                } else if c == '\n' {
                    out.push(c);
                    col = 0;
                } else {
                    out.push(c);
                    col += 1;
                }
            }
        }
        self.plain = out;
        self.spans.clear();
    }

    /// `Text.wrap`.
    pub fn wrap(
        &self,
        width: usize,
        justify: Justify,
        overflow: Overflow,
        no_wrap: bool,
    ) -> Vec<Text> {
        let no_wrap = no_wrap || overflow == Overflow::Ignore;
        let mut lines = Vec::new();
        for mut line in self.split_lines(true) {
            if line.plain.contains('\t') {
                line.expand_tabs(8);
            }
            let mut new_lines = if no_wrap {
                vec![line]
            } else {
                let offsets = divide_line(&line.plain, width, overflow == Overflow::Fold);
                line.divide(&offsets)
            };
            for l in &mut new_lines {
                l.rstrip_end(width);
            }
            justify_lines(&mut new_lines, width, justify, overflow);
            for l in &mut new_lines {
                l.truncate(width, overflow, false);
            }
            lines.extend(new_lines);
        }
        lines
    }

    /// Render to segments, applying span styles over the base style.
    pub fn render(&self, end: &str) -> Vec<Segment> {
        let mut out = Vec::new();
        if self.spans.is_empty() {
            if !self.plain.is_empty() {
                out.push(Segment::new(self.plain.clone(), self.style));
            }
        } else {
            let len = self.len();
            let mut cuts: Vec<usize> = vec![0, len];
            for s in &self.spans {
                cuts.push(s.start.min(len));
                cuts.push(s.end.min(len));
            }
            cuts.sort_unstable();
            cuts.dedup();
            let chars: Vec<(usize, char)> = self.plain.char_indices().collect();
            let byte = |c: usize| chars.get(c).map(|x| x.0).unwrap_or(self.plain.len());
            for w in cuts.windows(2) {
                let (a, b) = (w[0], w[1]);
                if a >= b {
                    continue;
                }
                let mut st = self.style;
                for s in &self.spans {
                    if s.start <= a && s.end >= b {
                        st = st.combine(s.style);
                    }
                }
                out.push(Segment::new(&self.plain[byte(a)..byte(b)], st));
            }
        }
        if !end.is_empty() {
            out.push(Segment::plain(end));
        }
        out
    }

    /// `Text.__rich_measure__`: (longest word, longest line).
    pub fn measure(&self) -> (usize, usize) {
        let max = self.plain.lines().map(cell_len).max().unwrap_or(0);
        let min = self
            .plain
            .split_whitespace()
            .map(cell_len)
            .max()
            .unwrap_or(max);
        (min, max)
    }

    /// `Text("\n").join(lines)`.
    pub fn join_lines(lines: Vec<Text>) -> Text {
        let mut out = Text::default();
        let n = lines.len();
        for (i, l) in lines.into_iter().enumerate() {
            out.append_text(&l);
            if i + 1 < n {
                out.plain.push('\n');
            }
        }
        out
    }
}

fn justify_lines(lines: &mut [Text], width: usize, justify: Justify, overflow: Overflow) {
    match justify {
        Justify::Left => {
            for l in lines.iter_mut() {
                l.truncate(width, overflow, true);
            }
        }
        Justify::Center => {
            for l in lines.iter_mut() {
                l.rstrip();
                l.truncate(width, overflow, false);
                let left = (width.saturating_sub(cell_len(&l.plain))) / 2;
                l.pad_left(left);
                let right = width.saturating_sub(cell_len(&l.plain));
                l.pad_right(right);
            }
        }
        Justify::Right => {
            for l in lines.iter_mut() {
                l.rstrip();
                l.truncate(width, overflow, false);
                let left = width.saturating_sub(cell_len(&l.plain));
                l.pad_left(left);
            }
        }
        Justify::Full => {
            let n = lines.len();
            for (i, l) in lines.iter_mut().enumerate() {
                if i + 1 == n {
                    break;
                }
                let words: Vec<&str> = l.plain.split(' ').collect();
                let words_size: usize = words.iter().map(|w| cell_len(w)).sum();
                let mut num_spaces = words.len().saturating_sub(1);
                let mut spaces = vec![1usize; num_spaces];
                let mut index = 0;
                if !spaces.is_empty() {
                    while words_size + num_spaces < width {
                        let k = spaces.len() - index - 1;
                        spaces[k] += 1;
                        num_spaces += 1;
                        index = (index + 1) % spaces.len();
                    }
                }
                let mut s = String::new();
                for (wi, w) in words.iter().enumerate() {
                    s.push_str(w);
                    if wi < spaces.len() {
                        s.extend(std::iter::repeat_n(' ', spaces[wi]));
                    }
                }
                l.plain = s;
                l.spans.clear();
            }
        }
        Justify::Default => {}
    }
}

/// Yield (start, end, word) like `rich._wrap.words` (regex `\s*\S+\s*`).
fn words(text: &str) -> Vec<(usize, usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut pos = 0;
    let n = chars.len();
    loop {
        let start = pos;
        let mut i = pos;
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        while i < n && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        out.push((start, i, chars[start..i].iter().collect()));
        pos = i;
    }
    out
}

/// `rich._wrap.divide_line`: character offsets at which to break `text`.
pub fn divide_line(text: &str, width: usize, fold: bool) -> Vec<usize> {
    let mut breaks = Vec::new();
    let mut cell_offset = 0usize;
    for (start, _end, word) in words(text) {
        let mut start = start;
        let word_length = cell_len(word.trim_end());
        let remaining = width as isize - cell_offset as isize;
        if remaining >= word_length as isize {
            cell_offset += cell_len(&word);
        } else if word_length > width {
            if fold {
                let folded = chop_cells(&word, width);
                let n = folded.len();
                for (i, line) in folded.iter().enumerate() {
                    if start != 0 {
                        breaks.push(start);
                    }
                    if i + 1 == n {
                        cell_offset = cell_len(line);
                    } else {
                        start += line.chars().count();
                    }
                }
            } else {
                if start != 0 {
                    breaks.push(start);
                }
                cell_offset = cell_len(&word);
            }
        } else if cell_offset != 0 && start != 0 {
            breaks.push(start);
            cell_offset = cell_len(&word);
        }
    }
    breaks
}

#[cfg(test)]
mod tests {
    use super::divide_line;

    #[test]
    fn divide_line_matches_rich() {
        assert_eq!(divide_line("hello world foo", 11, true), vec![12]);
        assert_eq!(divide_line("abcdefghij", 4, true), vec![4, 8]);
        assert_eq!(divide_line("a bb ccc dddd", 5, true), vec![5, 9]);
        assert!(divide_line("short", 80, true).is_empty());
    }
}
