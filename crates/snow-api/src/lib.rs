//! snow CLI core: configuration, global context, output, console and errors.
#![forbid(unsafe_code)]

pub mod ascii_table;
pub mod config;
pub mod connect;
pub mod console;
pub mod context;
pub mod errors;
pub mod identifiers;
pub mod markup;
pub mod output;
pub mod pep440;
pub mod project;
pub mod pyfmt;
pub mod rendering;
pub mod sanitize;
pub mod secure;
pub mod splitter;
pub mod sql;
pub mod stage_path;
pub mod tomlfmt;
pub mod values;

pub use context::Cli;
pub use errors::{Result, SnowError};
pub use output::{Format, Output, Pv, Row};

/// The upstream version this port reproduces.
pub const VERSION: &str = "3.35.0.dev0";
/// Short upstream commit, shown by `--version` for development versions.
pub const UPSTREAM_SHA: &str = "3ba0314";

pub fn display_version() -> String {
    format!("{VERSION} ({UPSTREAM_SHA})")
}
