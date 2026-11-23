//! Click-compatible parsing and Typer/Rich-compatible help for the snow CLI.
#![forbid(unsafe_code)]

pub mod difflib;
pub mod error;
pub mod help;
pub mod parse;
pub mod spec;
pub mod term;
pub mod textwrap;
pub mod value;

pub use error::ClickError;
pub use parse::{Hooks, Invocation, Processor, Source, Spec, py_repr};
pub use value::Value;
