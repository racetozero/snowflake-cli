//! Static description of a command tree, produced by `scripts/gen_cli_spec.py`.
//!
//! The types mirror what Click and Typer know about a command at runtime: the
//! option strings, value type, defaults and help text of each parameter, and
//! the help, usage and documentation panels of each command. Text that Rich
//! renders as Markdown is stored as the flattened markdown-it token stream so
//! the renderer in `snow-rich` sees exactly what Rich sees.

pub use snow_rich::{MdKind, MdTok};

/// Index into the generated `MD` table; `NO_MD` means "no text".
pub type MdRef = u16;
pub const NO_MD: MdRef = u16::MAX;

/// Click parameter types used by Snowflake CLI.
#[derive(Debug, Clone, Copy)]
pub enum Ty {
    Str,
    Int,
    Float,
    Bool,
    Choice {
        choices: &'static [&'static str],
        case_sensitive: bool,
    },
    IntRange {
        min: Option<i64>,
        max: Option<i64>,
        min_open: bool,
        max_open: bool,
        clamp: bool,
    },
    Tuple(&'static [Ty]),
    Path {
        exists: bool,
        file_okay: bool,
        dir_okay: bool,
        readable: bool,
        writable: bool,
        resolve_path: bool,
        allow_dash: bool,
    },
    /// A Snowflake CLI specific type, named by its Python class name.
    Custom(&'static str),
}

#[derive(Debug)]
pub struct Param {
    pub name: &'static str,
    pub is_argument: bool,
    pub opts: &'static [&'static str],
    pub secondary: &'static [&'static str],
    pub col_long: &'static str,
    pub col_short: &'static str,
    pub col_sec_long: &'static str,
    pub col_sec_short: &'static str,
    pub hidden: bool,
    /// Feature flag whose enabled state flips `hidden`.
    pub hidden_flag: Option<&'static str>,
    pub required: bool,
    pub multiple: bool,
    pub nargs: i32,
    pub is_flag: bool,
    pub is_bool_flag: bool,
    pub count: bool,
    pub flag_value: Option<&'static str>,
    pub is_eager: bool,
    pub envvar: Option<&'static str>,
    pub ty: Ty,
    /// The metavar column of the help table (Typer rules, range included).
    pub metavar: &'static str,
    /// Click's `make_metavar()`, used in usage lines and error messages.
    pub usage_metavar: &'static str,
    pub envvar_text: Option<&'static str>,
    pub default_text: Option<&'static str>,
    /// The Python default as JSON (callables appear as `{"__callable__": name}`).
    pub default_json: &'static str,
    /// The default exactly as `snow --structure` prints it.
    pub structure_default: Option<&'static str>,
    pub help: Option<&'static str>,
    pub md: MdRef,
    pub panel: Option<&'static str>,
    pub callback: Option<&'static str>,
    /// `OverrideableOption(mutually_exclusive=...)` parameter names.
    pub mutex: &'static [&'static str],
    /// The wrapped callback (`prop:<field>` for context setters).
    pub inner_callback: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub struct Span {
    pub code: bool,
    pub text: &'static str,
}

#[derive(Debug)]
pub enum DocBlock {
    Text(&'static [Span]),
    Admonition(&'static str, &'static [Span]),
    Bullets(&'static [&'static [Span]]),
    Group(&'static [DocBlock]),
}

#[derive(Debug)]
pub struct Example {
    pub description: Option<&'static [Span]>,
    pub command: Option<&'static str>,
    pub output: Option<&'static str>,
}

#[derive(Debug)]
pub struct Related {
    pub title: &'static str,
    pub href: &'static str,
}

#[derive(Debug)]
pub struct Command {
    pub path: &'static str,
    pub name: &'static str,
    pub help: Option<&'static str>,
    pub structure_help: &'static str,
    pub help_first_md: MdRef,
    pub help_rest_md: MdRef,
    pub row_md: MdRef,
    pub epilog_md: MdRef,
    pub hidden: bool,
    pub hidden_flag: Option<&'static str>,
    pub deprecated: bool,
    pub is_group: bool,
    pub no_args_is_help: bool,
    pub invoke_without_command: bool,
    pub allow_extra_args: bool,
    pub ignore_unknown_options: bool,
    pub allow_interspersed_args: bool,
    pub markup: bool,
    pub usage_pieces: &'static str,
    pub panel: Option<&'static str>,
    pub has_help_option: bool,
    /// `snow ai` commands: everything after `--agent-args` is passed through.
    pub agent_args: bool,
    /// `snow app`: help hides the commands of the other app family.
    pub smart_app_group: bool,
    pub params: &'static [u16],
    pub children: &'static [u16],
    pub usage_notes: &'static [DocBlock],
    pub examples: &'static [Example],
    pub related: &'static [Related],
}
