//! `snow bundle` (Snowflake Code Bundles).

use snow_api::identifiers::{Fqn, to_string_literal};
use snow_api::output::Output;
use snow_api::{Cli, Result, SnowError};

fn name(cli: &mut Cli) -> Result<Fqn> {
    let f = crate::object::fqn_arg(cli, "identifier")?;
    if f.name.is_empty() {
        return Err(SnowError::cli("Code bundle name is required."));
    }
    cli.using_connection(f)
}

fn message_of(cli: &mut Cli, sql: &str) -> Result<Output> {
    let c = cli.execute_query(sql)?;
    Ok(Output::message(Cli::first_cell(&c)))
}

pub fn create(cli: &mut Cli) -> Result<Output> {
    if cli.flag("overwrite") && cli.flag("skip_if_exists") {
        return Err(SnowError::incompatible(&[
            "--overwrite",
            "--skip-if-exists",
        ]));
    }
    let source = cli.str("source").unwrap_or_default();
    if source.is_empty() {
        return Err(SnowError::cli("Source is required."));
    }
    let src = process_source(cli, &source)?;
    let f = name(cli)?;
    let clause = if cli.flag("overwrite") {
        "CREATE OR REPLACE CODE BUNDLE"
    } else if cli.flag("skip_if_exists") {
        "CREATE CODE BUNDLE IF NOT EXISTS"
    } else {
        "CREATE CODE BUNDLE"
    };
    let mut q = format!(
        "{clause} {} FROM {}",
        f.sql_identifier(),
        to_string_literal(&src)
    );
    if let Some(c) = cli.str("comment") {
        q.push_str(&format!(" COMMENT = {}", to_string_literal(&c)));
    }
    message_of(cli, &q)
}

fn process_source(cli: &mut Cli, source: &str) -> Result<String> {
    if source.starts_with('@') || source.to_lowercase().starts_with("snow://") {
        return Ok(source.to_string());
    }
    let low = source.to_lowercase();
    if low.starts_with("file://") || !source.contains("://") {
        let local = if low.starts_with("file://") {
            &source[7..]
        } else {
            source
        };
        let stage = format!(
            "tmp_bundle_stage_{}",
            1_000_000 + uuid::Uuid::new_v4().as_u128() as u32 % 9_000_000
        );
        cli.execute_query(&format!("CREATE OR REPLACE TEMPORARY STAGE {stage}"))?;
        let root = std::path::Path::new(local);
        if !root.is_dir() {
            return Err(SnowError::cli(format!(
                "Source path '{local}' is not a directory."
            )));
        }
        let excludes = cli.list("exclude");
        let mut files = Vec::new();
        crate::stage::walk_files(root, &mut files);
        for f in files.into_iter().filter(|f| f.is_file()) {
            let skip = f.components().any(|c| {
                let part = c.as_os_str().to_string_lossy();
                excludes
                    .iter()
                    .any(|pat| crate::stage::fnmatchcase(&part, pat))
            });
            if skip {
                continue;
            }
            let rel = f.strip_prefix(root).unwrap_or(&f);
            let sub = rel
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let dest = if sub.is_empty() {
                format!("@{stage}")
            } else {
                format!("@{stage}/{sub}")
            };
            let local = crate::stage::glob_escape(&f.to_string_lossy());
            crate::stage::put(
                cli,
                &local,
                &snow_api::stage_path::StagePath::from_stage_str(&dest),
                4,
                false,
                false,
            )?;
        }
        return Ok(format!("@{stage}"));
    }
    Err(SnowError::cli(format!(
        "Invalid source: '{source}'. Source must be a Snowflake stage path (starting with '@'), a Snowflake workspace path (starting with 'snow://'), or a local file system path (starting with 'file://' or no protocol prefix)."
    )))
}

pub fn list(cli: &mut Cli) -> Result<Output> {
    let scope = cli.list("scope");
    let in_account = cli.flag("in_account");
    let scope_set = scope.first().is_some_and(|s| !s.is_empty());
    if in_account && scope_set {
        return Err(SnowError::incompatible(&["--in-account", "--in"]));
    }
    if scope_set {
        if !["database", "schema"].contains(&scope[0].to_lowercase().as_str()) {
            return Err(SnowError::cli("Scope must be 'database' or 'schema'."));
        }
        if scope.get(1).is_none_or(|s| s.is_empty()) {
            return Err(SnowError::cli("Scope name cannot be empty."));
        }
    }
    let mut q = "SHOW CODE BUNDLES".to_string();
    if let Some(l) = cli.str("like") {
        q.push_str(&format!(" LIKE {}", to_string_literal(&l)));
    }
    if in_account {
        q.push_str(" IN ACCOUNT");
    } else if scope_set {
        q.push_str(&format!(
            " IN {} {}",
            scope[0].to_uppercase(),
            Fqn::from_string(&scope[1])?.sql_identifier()
        ));
    }
    let c = cli.execute_query(&q)?;
    Ok(cli.query_result(&c))
}

pub fn delete(cli: &mut Cli) -> Result<Output> {
    let f = name(cli)?;
    let ie = if cli.flag("if_exists") {
        "IF EXISTS "
    } else {
        ""
    };
    message_of(cli, &format!("DROP CODE BUNDLE {ie}{}", f.sql_identifier()))
}

pub fn alter(cli: &mut Cli) -> Result<Output> {
    let (rename, add) = (cli.str("rename_to"), cli.str("add_version"));
    if rename.is_some() && add.is_some() {
        return Err(SnowError::incompatible(&["--rename-to", "--add-version"]));
    }
    if rename.is_none() && add.is_none() {
        return Err(SnowError::cli(
            "Exactly one of '--rename-to' or '--add-version' must be provided.",
        ));
    }
    let f = name(cli)?;
    let mut q = format!("ALTER CODE BUNDLE {}", f.sql_identifier());
    if let Some(r) = rename {
        let n = cli.using_connection(Fqn::from_string(&r)?)?;
        q.push_str(&format!(" RENAME TO {}", n.sql_identifier()));
    } else if let Some(a) = add {
        q.push_str(&format!(" ADD VERSION FROM {}", to_string_literal(&a)));
    }
    message_of(cli, &q)
}

pub fn execute(cli: &mut Cli) -> Result<Output> {
    let spec = match cli.str("overrides") {
        Some(p) => Some(
            snow_api::secure::read_text(std::path::Path::new(&p))
                .map_err(|e| SnowError::cli(format!("Could not read overrides file '{p}': {e}")))?,
        ),
        None => None,
    };
    let f = name(cli)?;
    let entrypoint = cli.str("entrypoint").unwrap_or_default();
    if entrypoint.is_empty() {
        return Err(SnowError::cli("Entrypoint is required."));
    }
    let mut q = format!(
        "EXECUTE CODE BUNDLE {} ENTRYPOINT={}",
        f.sql_identifier(),
        to_string_literal(&entrypoint)
    );
    if let Some(n) = cli.str("execution_name").filter(|s| !s.is_empty()) {
        q.push_str(&format!(" EXECUTION_NAME={}", to_string_literal(&n)));
    }
    let args = cli.list("arguments");
    if !args.is_empty() {
        q.push_str(&format!(
            " ARGUMENTS=({})",
            args.iter()
                .map(|a| to_string_literal(a))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(s) = spec {
        if s.contains("$$") {
            return Err(SnowError::cli("Overrides file must not contain '$$'."));
        }
        q.push_str(&format!(" WITH SPECIFICATION $$\n{}\n$$", s.trim_end()));
    }
    if cli.flag("is_async") {
        let verbose = cli.connector_info;
        let c = cli
            .connection()?
            .execute_async(&snow_rich::render::dedent(&q))
            .map_err(|e| snow_api::connect::db_error_to_cli(&e, verbose))?;
        return Ok(Output::message(format!(
            "Request submitted. Query ID: {}",
            c.sfqid.unwrap_or_else(|| "None".into())
        )));
    }
    message_of(cli, &q)
}

pub fn status(cli: &mut Cli) -> Result<Output> {
    let qid = cli.str("query_id").unwrap_or_default();
    if qid.is_empty() {
        return Err(SnowError::cli("Query ID is required."));
    }
    if uuid::Uuid::parse_str(&qid).is_err() {
        return Err(SnowError::cli(format!("Invalid query ID: {qid}")));
    }
    let path = format!(
        "/monitoring/queries/{}",
        snow_api::identifiers::encode_uri_component(&qid)
    );
    let v = cli
        .connection()?
        .rest_get(&path)
        .map_err(|e| SnowError::cli(e.msg()))?;
    let status = match v["data"]["queries"].as_array() {
        None => "FAILED_WITH_ERROR".to_string(),
        Some(q) => q
            .first()
            .and_then(|x| x["status"].as_str())
            .unwrap_or("NO_DATA")
            .to_string(),
    };
    Ok(Output::message(format!("Query {qid}: {status}")))
}

pub fn cancel(cli: &mut Cli) -> Result<Output> {
    let qid = cli.str("query_id").unwrap_or_default();
    if qid.is_empty() {
        return Err(SnowError::cli("Query ID is required."));
    }
    message_of(
        cli,
        &format!("SELECT SYSTEM$CANCEL_QUERY({})", to_string_literal(&qid)),
    )
}

pub fn history(cli: &mut Cli) -> Result<Output> {
    let mut args = Vec::new();
    let lit = |k: &str, v: &str| format!("{k} => {}", to_string_literal(v));
    for (opt, key) in [
        ("bundle_name", "BUNDLE_NAME"),
        ("bundle_database", "DATABASE"),
        ("bundle_schema", "SCHEMA"),
        ("entrypoint", "ENTRYPOINT"),
    ] {
        if let Some(v) = cli.str(opt).filter(|s| !s.is_empty()) {
            args.push(lit(key, &v));
        }
    }
    for (opt, key) in [
        ("start_time_range_start", "START_TIME_RANGE_START"),
        ("start_time_range_end", "START_TIME_RANGE_END"),
    ] {
        if let Some(v) = cli.str(opt).filter(|s| !s.is_empty()) {
            args.push(format!(
                "{key} => TO_TIMESTAMP_LTZ({})",
                to_string_literal(&v)
            ));
        }
    }
    for (opt, key) in [
        ("bundle_types", "BUNDLE_TYPES"),
        ("compute_types", "COMPUTE_TYPES"),
        ("language_types", "LANGUAGE_TYPES"),
    ] {
        let l = cli.list(opt);
        if !l.is_empty() {
            args.push(lit(key, &l.join(",")));
        }
    }
    for (opt, key) in [("status", "STATUS"), ("execution_name", "EXECUTION_NAME")] {
        if let Some(v) = cli.str(opt).filter(|s| !s.is_empty()) {
            args.push(lit(key, &v));
        }
    }
    args.push(format!(
        "RESULT_LIMIT => {}",
        cli.int("result_limit").unwrap_or(100)
    ));
    let q = format!(
        "SELECT * FROM TABLE(SNOWFLAKE.INFORMATION_SCHEMA.CODE_BUNDLE_HISTORY({}))",
        args.join(", ")
    );
    let c = cli.execute_query(&q)?;
    Ok(cli.query_result(&c))
}
