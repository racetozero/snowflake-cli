//! Rich-compatible terminal layout for the snow CLI.
//!
//! Snowflake CLI prints its help, errors and some results through the Python
//! library Rich. This crate ports the parts of Rich's layout that those
//! outputs use (word wrapping, padding, panels with rounded borders, box-less
//! tables with Rich's column width algorithm, Columns, Markdown paragraphs and
//! lists) so the Rust port prints byte-identical text.
#![forbid(unsafe_code)]

pub mod markdown;
pub mod render;
pub mod segment;
pub mod style;
pub mod text;

pub use markdown::{MdKind, MdTok};
pub use render::{Column, M, Opts, R, Table, measure, render, render_lines, segments_to_string};
pub use segment::{Segment, cell_len};
pub use style::{Style, theme};
pub use text::{Justify, Overflow, Text};

/// Render `r` at `width` columns and return the printed text.
pub fn to_string(r: &R, width: usize, color: bool) -> String {
    segments_to_string(&render(r, Opts::new(width)), color)
}
