//! `snow spcs compute-pool`.

use super::{
    already_exists, fqn, py_bool, strip_empty_lines, tags_clause, validate_and_set_instances,
};
use snow_api::identifiers::{Fqn, is_valid_object_name, to_string_literal};
use snow_api::output::Output;
use snow_api::{Cli, Result, SnowError};

/// `_compute_pool_name_callback`.
pub fn validate_name(raw: &str) -> Result<()> {
    let f = Fqn::from_string(raw)?;
    if !is_valid_object_name(&f.identifier(), 0, false) {
        return Err(SnowError::click(format!(
            "'{}' is not a valid compute pool name. Note that compute pool names must be unquoted identifiers.",
            f.identifier()
        )));
    }
    Ok(())
}

fn pool(cli: &Cli) -> Result<String> {
    Ok(fqn(cli, "name")?.identifier())
}

#[allow(clippy::too_many_arguments)]
pub fn create_sql(
    pool_name: &str,
    min_nodes: i64,
    max_nodes: i64,
    family: &str,
    auto_resume: bool,
    initially_suspended: bool,
    auto_suspend_secs: i64,
    tags: &[String],
    comment: Option<&str>,
    if_not_exists: bool,
) -> String {
    let stmt = if if_not_exists {
        "CREATE COMPUTE POOL IF NOT EXISTS"
    } else {
        "CREATE COMPUTE POOL"
    };
    let mut q: Vec<String> = vec![
        format!("{stmt} {pool_name}"),
        format!("MIN_NODES = {min_nodes}"),
        format!("MAX_NODES = {max_nodes}"),
        format!("INSTANCE_FAMILY = {}", to_string_literal(family)),
        format!("AUTO_RESUME = {}", py_bool(auto_resume)),
        format!("INITIALLY_SUSPENDED = {}", py_bool(initially_suspended)),
        format!("AUTO_SUSPEND_SECS = {auto_suspend_secs}"),
    ];
    if let Some(c) = comment.filter(|c| !c.is_empty()) {
        q.push(format!("COMMENT = {c}"));
    }
    if !tags.is_empty() {
        q.push(tags_clause(tags));
    }
    strip_empty_lines(&q)
}

pub fn create(cli: &mut Cli) -> Result<Output> {
    let name = pool(cli)?;
    let resume = if cli.flag("auto_resume") {
        true
    } else {
        !cli.flag("no_auto_resume")
    };
    let min = cli.int("min_nodes").unwrap_or(1);
    let max = validate_and_set_instances(min, cli.int("max_nodes"), "nodes")?;
    let sql = create_sql(
        &name,
        min,
        max,
        &cli.str("instance_family").unwrap_or_default(),
        resume,
        cli.flag("initially_suspended"),
        cli.int("auto_suspend_secs").unwrap_or(3600),
        &cli.list("tags"),
        cli.str("comment").as_deref(),
        cli.flag("if_not_exists"),
    );
    let verbose = cli.connector_info;
    let c = cli
        .try_execute(&sql)
        .map_err(|e| already_exists(e, "compute-pool", &name, false, verbose))?;
    Ok(cli.single_query_result(&c))
}

fn simple(cli: &mut Cli, sql: String) -> Result<Output> {
    let c = cli.execute_query(&sql)?;
    Ok(cli.single_query_result(&c))
}

pub fn stop_all(cli: &mut Cli) -> Result<Output> {
    let n = pool(cli)?;
    simple(cli, format!("alter compute pool {n} stop all"))
}
pub fn suspend(cli: &mut Cli) -> Result<Output> {
    let n = pool(cli)?;
    simple(cli, format!("alter compute pool {n} suspend"))
}
pub fn resume(cli: &mut Cli) -> Result<Output> {
    let n = pool(cli)?;
    simple(cli, format!("alter compute pool {n} resume"))
}

pub fn set_sql(
    name: &str,
    min: Option<i64>,
    max: Option<i64>,
    auto_resume: Option<bool>,
    secs: Option<i64>,
    comment: Option<String>,
) -> Result<String> {
    let pairs: Vec<(&str, Option<String>)> = vec![
        ("min_nodes", min.map(|v| v.to_string())),
        ("max_nodes", max.map(|v| v.to_string())),
        ("auto_resume", auto_resume.map(|b| py_bool(b).to_string())),
        ("auto_suspend_secs", secs.map(|v| v.to_string())),
        ("comment", comment),
    ];
    if pairs.iter().all(|(_, v)| v.is_none()) {
        return Err(SnowError::click(format!(
            "No properties specified for compute pool '{name}'. Please provide at least one property to set."
        )).named("NoPropertiesProvidedError"));
    }
    let mut q = vec![format!("alter compute pool {name} set")];
    for (k, v) in pairs {
        if let Some(v) = v {
            q.push(format!("{k} = {v}"));
        }
    }
    Ok(strip_empty_lines(&q))
}

pub fn set(cli: &mut Cli) -> Result<Output> {
    let n = pool(cli)?;
    let resume = if cli.flag("auto_resume") {
        Some(true)
    } else if cli.flag("no_auto_resume") {
        Some(false)
    } else {
        None
    };
    let sql = set_sql(
        &n,
        cli.int("min_nodes"),
        cli.int("max_nodes"),
        resume,
        cli.int("auto_suspend_secs"),
        cli.str("comment"),
    )?;
    simple(cli, sql)
}

pub fn unset(cli: &mut Cli) -> Result<Output> {
    let n = pool(cli)?;
    let pairs = [
        ("auto_resume", cli.flag("auto_resume")),
        ("auto_suspend_secs", cli.flag("auto_suspend_secs")),
        ("comment", cli.flag("comment")),
    ];
    if !pairs.iter().any(|(_, v)| *v) {
        return Err(SnowError::click(format!(
            "No properties specified for compute pool '{n}'. Please provide at least one property to reset to its default value."
        )).named("NoPropertiesProvidedError"));
    }
    let list: Vec<&str> = pairs.iter().filter(|(_, v)| *v).map(|(k, _)| *k).collect();
    simple(
        cli,
        format!("alter compute pool {n} unset {}", list.join(",")),
    )
}

pub fn status(cli: &mut Cli) -> Result<Output> {
    let n = fqn(cli, "pool_name")?.identifier();
    simple(cli, format!("call system$get_compute_pool_status('{n}')"))
}
