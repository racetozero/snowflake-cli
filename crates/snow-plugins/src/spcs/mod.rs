//! `snow spcs`: compute pools, services, image registry and repositories.

pub mod build;
pub mod compute_pool;
pub mod deploy;
pub mod image_registry;
pub mod image_repository;
pub mod services;

use snow_api::identifiers::{Fqn, is_valid_identifier, to_string_literal, unquote_identifier};
use snow_api::{Cli, Result, SnowError};
use snow_connector::DbError;

/// `strip_empty_lines`.
pub fn strip_empty_lines(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Python `str(bool)`.
pub fn py_bool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

pub fn validate_and_set_instances(min: i64, max: Option<i64>, name: &str) -> Result<i64> {
    if min < 1 {
        return Err(SnowError::click(format!("min_{name} must be positive")));
    }
    match max {
        None => Ok(min),
        Some(m) if m < min => Err(SnowError::click(format!(
            "max_{name} must be greater or equal to min_{name}"
        ))),
        Some(m) => Ok(m),
    }
}

/// `handle_object_already_exists`.
/// `cli_name` is the object type's CLI name (`compute-pool`), which
/// `str(ObjectType)` returns.
pub fn already_exists(
    e: DbError,
    cli_name: &str,
    name: &str,
    replace_available: bool,
    verbose: bool,
) -> SnowError {
    if e.errno == 2002 {
        let mut msg = format!(
            "{} {} already exists.",
            capitalize(cli_name),
            unquote_identifier(name)
        );
        if replace_available {
            msg.push_str(" Use --replace flag to update objects.");
        }
        return SnowError::cli(msg).named("ObjectAlreadyExistsError");
    }
    snow_api::connect::db_error_to_cli(&e, verbose)
}

pub fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
        None => String::new(),
    }
}

/// `Tag.to_sql_clause`: tags arrive as `name=value` strings.
pub fn tags_clause(tags: &[String]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let items: Vec<String> = tags
        .iter()
        .filter_map(|t| t.split_once('='))
        .map(|(n, v)| format!("{n}={}", to_string_literal(v)))
        .collect();
    format!("WITH TAG ({})", items.join(","))
}

/// `_parse_tag`: validate `name=value`.
pub fn parse_tag(raw: &str) -> Result<()> {
    let err = || {
        SnowError::click(
            "tag must be in the format <name>=<value> where 'name' is a valid identifier and value is a string",
        )
    };
    // The identifier is the longest valid prefix before '=' that leaves a non-empty value.
    for (i, _) in raw.match_indices('=') {
        let (n, v) = (&raw[..i], &raw[i + 1..]);
        if is_valid_identifier(n) && !v.is_empty() {
            return Ok(());
        }
    }
    Err(err())
}

pub fn fqn(cli: &Cli, name: &str) -> Result<Fqn> {
    crate::object::fqn_arg(cli, name)
}

/// Run a statement, mapping errors like `execute_query`.
pub fn run(cli: &mut Cli, sql: &str) -> Result<snow_connector::Cursor> {
    cli.execute_query(sql)
}
