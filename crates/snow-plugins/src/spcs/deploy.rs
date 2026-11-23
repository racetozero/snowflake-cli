//! `snow spcs compute-pool|image-repository|service deploy` from snowflake.yml.

use super::{already_exists, py_bool, strip_empty_lines, tags_clause, validate_and_set_instances};
use snow_api::identifiers::{Fqn, to_identifier, to_string_literal};
use snow_api::output::Output;
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};

/// `get_entity_from_project_definition(entity_type, entity_id)`.
fn entity(cli: &mut Cli, cli_name: &str, sf_name: &str, plural: &str) -> Result<(String, PyV)> {
    let id = cli.str("entity_id").filter(|s| !s.is_empty());
    let root = cli.project_root();
    let p = cli.project(false)?.expect("project");
    let ents: Vec<(String, PyV)> = p
        .entities()
        .into_iter()
        .filter(|(_, e)| e.str_of("type").as_deref() == Some(cli_name))
        .collect();
    if ents.is_empty() {
        return Err(SnowError::cli(format!(
            "No {sf_name} project definition found in {}",
            root.display()
        ))
        .named("NoProjectDefinitionError"));
    }
    let mut id = id;
    if let Some(i) = &id
        && !ents.iter().any(|(k, _)| k == i)
    {
        return Err(SnowError::usage(format!(
            "No '{i}' entity in project definition file."
        )));
    }
    if ents.len() == 1 {
        id = Some(ents[0].0.clone());
    }
    let Some(id) = id else {
        return Err(SnowError::usage(format!(
            "Multiple {plural} found. Please provide entity id for the operation."
        )));
    };
    Ok(ents.into_iter().find(|(k, _)| *k == id).expect("checked"))
}

fn int(m: &PyV, k: &str) -> Option<i64> {
    match m.get(k) {
        Some(PyV::Int(i)) => Some(*i),
        _ => None,
    }
}

fn boolean(m: &PyV, k: &str) -> Option<bool> {
    match m.get(k) {
        Some(PyV::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// `Tag` models as `name=value` strings for `tags_clause`.
fn tags(m: &PyV) -> Vec<String> {
    m.get("tags")
        .and_then(PyV::as_list)
        .map(|l| {
            l.iter()
                .map(|t| {
                    format!(
                        "{}={}",
                        t.str_of("name").unwrap_or_default(),
                        t.str_of("value").unwrap_or_default()
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn ident(m: &PyV, id: &str) -> Result<String> {
    Ok(crate::entities::fqn(m, id)?.identifier())
}

pub fn compute_pool(cli: &mut Cli) -> Result<Output> {
    let (id, m) = entity(cli, "compute-pool", "compute pool", "compute pools")?;
    let min = int(&m, "min_nodes").unwrap_or(1);
    let max = validate_and_set_instances(min, int(&m, "max_nodes"), "nodes")?;
    let name = ident(&m, &id)?;
    let auto_resume = boolean(&m, "auto_resume");
    let secs = int(&m, "auto_suspend_seconds");
    let comment = m.str_of("comment");
    let sql = if cli.flag("upgrade") {
        super::compute_pool::set_sql(&name, Some(min), Some(max), auto_resume, secs, comment)?
    } else {
        let family = m.str_of("instance_family").unwrap_or_else(|| "None".into());
        super::compute_pool::create_sql(
            &name,
            min,
            max,
            &family,
            auto_resume.unwrap_or(true),
            boolean(&m, "initially_suspended").unwrap_or(false),
            secs.unwrap_or(3600),
            &tags(&m),
            comment.as_deref(),
            false,
        )
    };
    let verbose = cli.connector_info;
    let c = cli
        .try_execute(&sql)
        .map_err(|e| already_exists(e, "compute-pool", &name, false, verbose))?;
    Ok(cli.single_query_result(&c))
}

pub fn image_repository(cli: &mut Cli) -> Result<Output> {
    let (id, m) = entity(
        cli,
        "image-repository",
        "image repository",
        "image repositories",
    )?;
    let name = ident(&m, &id)?;
    let replace = cli.flag("replace");
    super::image_repository::create_repo(cli, &name, false, replace)
}

pub fn service(cli: &mut Cli) -> Result<Output> {
    let (id, m) = entity(cli, "service", "service", "services")?;
    let min = int(&m, "min_instances").unwrap_or(1);
    let max = validate_and_set_instances(min, int(&m, "max_instances"), "instances")?;
    let name = ident(&m, &id)?;
    let stage = m.str_of("stage").unwrap_or_else(|| "None".into());
    let stage_fqn = Fqn::from_stage(&stage)?;
    cli.execute_query(&crate::stage::create_query(
        &stage_fqn, false, None, false, None,
    ))?;
    let stage = crate::stage_execute::standard_prefix(&stage);
    let artifacts = m
        .get("artifacts")
        .and_then(PyV::as_list)
        .cloned()
        .unwrap_or_default();
    if artifacts.is_empty() {
        return Err(
            SnowError::unexpected("Service needs to have artifacts to deploy").named("ValueError"),
        );
    }
    let root = cli.project_root();
    let bundle_root = root.join("output").join("bundle").join("service");
    if bundle_root.exists() {
        std::fs::remove_dir_all(&bundle_root)?;
    }
    std::fs::create_dir_all(&bundle_root)?;
    let map = crate::artifacts::bundle(&root, &bundle_root, &artifacts)?;
    let resolved_bundle = crate::artifacts::resolve_without_follow(&bundle_root);
    for (_, dest) in map.all_mappings_expanded() {
        if dest.is_dir() {
            continue;
        }
        let parent = dest
            .strip_prefix(&resolved_bundle)
            .ok()
            .and_then(|r| r.parent())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let sp = snow_api::stage_path::StagePath::from_stage_str(&stage);
        let sp = if parent.is_empty() {
            sp
        } else {
            sp.join(&parent)
        };
        crate::stage::put(cli, &dest.to_string_lossy(), &sp, 4, true, false)?;
    }
    let out = root.join("output");
    if out.exists() {
        std::fs::remove_dir_all(out)?;
    }
    let stage_sql = snow_api::stage_path::StagePath::from_stage_str(&stage).path_for_sql();
    let spec = to_string_literal(&m.str_of("spec_file").unwrap_or_else(|| "None".into()));
    let eai: Option<Vec<String>> = m
        .get("external_access_integrations")
        .and_then(PyV::as_list)
        .map(|l| l.iter().map(PyV::py_str).collect());
    let wh = m.str_of("query_warehouse");
    let auto_resume = boolean(&m, "auto_resume").unwrap_or(true);
    let secs = int(&m, "auto_suspend_secs");
    let comment = m.str_of("comment");
    if cli.flag("upgrade") {
        let set = super::services::set_sql(
            &name,
            Some(min),
            Some(max),
            wh,
            Some(auto_resume),
            secs,
            eai,
            comment,
        )?;
        cli.execute_query(&set)?;
        let q = strip_empty_lines(&[
            format!("ALTER SERVICE {name}"),
            format!("FROM {stage_sql}"),
            format!("SPECIFICATION_FILE = {spec}"),
        ]);
        let c = cli.execute_query(&q)?;
        return Ok(cli.single_query_result(&c));
    }
    let pool = to_identifier(&m.str_of("compute_pool").unwrap_or_else(|| "None".into()));
    let mut q = vec![
        format!("CREATE SERVICE {name}"),
        format!("IN COMPUTE POOL {pool}"),
        format!("FROM {stage_sql}"),
        format!("SPECIFICATION_FILE = {spec}"),
        format!("AUTO_RESUME = {}", py_bool(auto_resume)),
    ];
    if min != 0 {
        q.push(format!("MIN_INSTANCES = {min}"));
    }
    if max != 0 {
        q.push(format!("MAX_INSTANCES = {max}"));
    }
    if let Some(s) = secs {
        q.push(format!("AUTO_SUSPEND_SECS = {s}"));
    }
    if let Some(w) = wh.filter(|w| !w.is_empty()) {
        q.push(format!("QUERY_WAREHOUSE = {}", to_identifier(&w)));
    }
    if let Some(e) = eai.filter(|e| !e.is_empty()) {
        q.push(format!(
            "EXTERNAL_ACCESS_INTEGRATIONS = ({})",
            super::services::eai_list(&e)
        ));
    }
    if let Some(c) = comment.filter(|c| !c.is_empty()) {
        q.push(format!("COMMENT = {c}"));
    }
    let t = tags(&m);
    if !t.is_empty() {
        q.push(tags_clause(&t));
    }
    let sql = strip_empty_lines(&q);
    let verbose = cli.connector_info;
    let c = cli
        .try_execute(&sql)
        .map_err(|e| already_exists(e, "service", &name, false, verbose))?;
    Ok(cli.single_query_result(&c))
}
