//! The streamed ASCII table writer for large results (`_app/ascii_table.py`).

use crate::output::Sink;
use snow_rich::segment::{cell_len, chop_cells};

const PIPE_MAX_COLUMN_WIDTH: usize = 256;
const COLUMN_FLOOR: usize = 8;
pub(crate) const LARGE_RESULT_HINT: &str =
    "Large result: table output is streamed. Use --format csv for faster export.";

fn content_width(line: &str) -> usize {
    if line.is_empty() {
        0
    } else if line.is_ascii() {
        line.len()
    } else {
        cell_len(line)
    }
}

fn preferred_widths(columns: &[String], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths = vec![1; columns.len()];
    for (i, c) in columns.iter().enumerate() {
        for l in c.split('\n') {
            widths[i] = widths[i].max(content_width(l));
        }
    }
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            if i < widths.len() {
                for l in c.split('\n') {
                    widths[i] = widths[i].max(content_width(l));
                }
            }
        }
    }
    widths
}

fn allocate_tty_widths(preferred: &[usize], console_width: usize, plain: bool) -> Vec<usize> {
    if preferred.is_empty() {
        return Vec::new();
    }
    let floors: Vec<usize> = preferred.iter().map(|w| (*w).min(COLUMN_FLOOR)).collect();
    let n = preferred.len();
    let chrome = if plain { 2 * (n - 1) } else { 3 * n + 1 };
    let available = console_width as i64 - chrome as i64;
    let mut widths = preferred.to_vec();
    if widths.iter().sum::<usize>() as i64 <= available {
        return widths;
    }
    loop {
        let excess = widths.iter().sum::<usize>() as i64 - available;
        if excess <= 0 {
            return widths;
        }
        let shrinkable: Vec<usize> = (0..n).filter(|i| widths[*i] > floors[*i]).collect();
        if shrinkable.is_empty() {
            return widths;
        }
        let widest = shrinkable.iter().map(|i| widths[*i]).max().unwrap_or(0);
        let group: Vec<usize> = shrinkable
            .iter()
            .copied()
            .filter(|i| widths[*i] == widest)
            .collect();
        let next_lower = shrinkable
            .iter()
            .map(|i| widths[*i])
            .filter(|w| *w < widest)
            .max()
            .unwrap_or(0);
        let step = group
            .iter()
            .map(|i| widths[*i] - floors[*i].max(next_lower))
            .min()
            .unwrap_or(0);
        if step == 0 {
            return widths;
        }
        if (step * group.len()) as i64 <= excess {
            for i in &group {
                widths[*i] -= step;
            }
            continue;
        }
        let excess = excess as usize;
        let base = excess / group.len();
        let rem = excess % group.len();
        for (off, i) in group.iter().enumerate() {
            let dec = (base + usize::from(off < rem)).min(widths[*i] - floors[*i]);
            widths[*i] -= dec;
        }
        return widths.into_iter().map(|w| w.max(1)).collect();
    }
}

fn split_cell(value: &str, width: usize, wrap: bool) -> Vec<String> {
    let parts: Vec<&str> = value.split('\n').collect();
    if !wrap {
        return parts.into_iter().map(str::to_string).collect();
    }
    let mut lines = Vec::new();
    for p in parts {
        let w = width.max(1);
        if cell_len(p) <= w {
            lines.push(p.to_string());
        } else {
            let pieces: Vec<String> = chop_cells(p, w)
                .into_iter()
                .filter(|x| !x.is_empty())
                .collect();
            if pieces.is_empty() {
                lines.push(p.to_string());
            } else {
                lines.extend(pieces);
            }
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn pad_cell(piece: &str, width: usize) -> String {
    let size = cell_len(piece);
    if size >= width {
        piece.to_string()
    } else {
        format!("{piece}{}", " ".repeat(width - size))
    }
}

fn format_line(pieces: &[String], widths: &[usize], bold: bool, plain: bool) -> String {
    let rendered: Vec<String> = pieces
        .iter()
        .zip(widths)
        .map(|(p, w)| {
            let t = pad_cell(p, *w);
            if bold {
                format!("\x1b[1m{t}\x1b[0m")
            } else {
                t
            }
        })
        .collect();
    if plain {
        rendered.join("  ")
    } else {
        format!("| {} |", rendered.join(" | "))
    }
}

pub fn stream(columns: &[String], rows: &[Vec<String>], plain: bool, sink: &mut Sink) {
    let preferred = preferred_widths(columns, rows);
    let (widths, wrap) = if sink.is_tty {
        (allocate_tty_widths(&preferred, sink.width, plain), true)
    } else {
        (
            preferred
                .iter()
                .map(|w| (*w).min(PIPE_MAX_COLUMN_WIDTH))
                .collect::<Vec<_>>(),
            false,
        )
    };
    if sink.is_tty {
        eprintln!("{LARGE_RESULT_HINT}");
    }
    let bold = sink.is_tty && sink.color;
    let mut out = String::new();
    let rule = || {
        let inner =
            widths.iter().sum::<usize>() + 2 * widths.len() + widths.len().saturating_sub(1);
        format!("+{}+\n", "-".repeat(inner))
    };
    if !plain {
        out.push_str(&rule());
    }
    let record = |vals: &[String], bold: bool, out: &mut String| {
        let folded: Vec<Vec<String>> = vals
            .iter()
            .zip(&widths)
            .map(|(v, w)| split_cell(v, *w, wrap))
            .collect();
        let h = folded.iter().map(|c| c.len()).max().unwrap_or(1);
        for li in 0..h {
            let pieces: Vec<String> = folded
                .iter()
                .map(|c| c.get(li).cloned().unwrap_or_default())
                .collect();
            out.push_str(&format_line(&pieces, &widths, bold, plain));
            out.push('\n');
        }
    };
    record(columns, bold, &mut out);
    if !plain {
        if widths.is_empty() {
            out.push_str("|\n");
        } else {
            let body: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
            out.push_str(&format!("|{}|\n", body.join("+")));
        }
    }
    for r in rows {
        record(r, false, &mut out);
    }
    if !plain {
        out.push_str(rule().trim_end_matches('\n'));
    }
    sink.write_str(&out);
    if plain {
        return;
    }
    if sink.is_tty {
        sink.write_str("\n");
    }
    sink.write_str("\n");
}
