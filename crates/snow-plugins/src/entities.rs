//! Shared helpers for snowflake.yml entities (`api/entities`, `api/commands/utils.py`).

use snow_api::identifiers::Fqn;
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};

/// `EntityModelBase.fqn` (identifier string, `Identifier` model, or the entity id).
pub fn fqn(model: &PyV, entity_id: &str) -> Result<Fqn> {
    match model.get("identifier") {
        Some(PyV::Str(s)) => Fqn::from_string(s),
        Some(PyV::Model(m)) => {
            let name = m.fields.get("name").map(PyV::py_str).unwrap_or_default();
            let db = m
                .fields
                .get("database")
                .filter(|v| !v.is_none())
                .map(PyV::py_str);
            let sc = m
                .fields
                .get("schema_")
                .filter(|v| !v.is_none())
                .map(PyV::py_str);
            let f = Fqn::from_string(&name)?;
            if f.database.is_some() && db.is_some() {
                return Err(SnowError::cli(format!(
                    "Database provided but name '{name}' is fully qualified name."
                )));
            }
            if f.schema.is_some() && sc.is_some() {
                return Err(SnowError::cli(format!(
                    "Schema provided but name '{name}' is fully qualified name."
                )));
            }
            Ok(f.set_database(db.as_deref()).set_schema(sc.as_deref()))
        }
        _ => Fqn::from_string(entity_id),
    }
}

/// `get_entity_for_operation(cli_context, entity_id, project_definition, entity_type)`.
pub fn for_operation(
    cli: &mut Cli,
    entity_id: Option<&str>,
    entity_type: &str,
) -> Result<(String, PyV)> {
    let p = cli.project(false)?.expect("project").clone();
    for_operation_in(cli, &p, entity_id, entity_type)
}

/// `get_entity_for_operation` against a given (e.g. converted) project.
pub fn for_operation_in(
    cli: &mut Cli,
    p: &snow_api::project::Project,
    entity_id: Option<&str>,
    entity_type: &str,
) -> Result<(String, PyV)> {
    let root = cli.project_root();
    let ents: Vec<(String, PyV)> = p
        .entities()
        .into_iter()
        .filter(|(_, e)| e.str_of("type").as_deref() == Some(entity_type))
        .collect();
    if ents.is_empty() {
        return Err(SnowError::cli(format!(
            "No {entity_type} project definition found in {}",
            root.display()
        ))
        .named("NoProjectDefinitionError"));
    }
    if let Some(id) = entity_id
        && !ents.iter().any(|(k, _)| k == id)
    {
        return Err(SnowError::usage(format!(
            "No '{id}' entity in project definition file."
        )));
    }
    if ents.len() == 1 {
        return Ok(ents.into_iter().next().expect("one"));
    }
    match entity_id {
        Some(id) => Ok(ents.into_iter().find(|(k, _)| k == id).expect("checked")),
        None => Err(SnowError::usage(format!(
            "Multiple entities of type {entity_type} found. Please provide entity id for the operation."
        ))),
    }
}

/// `ProjectPaths.remove_up_bundle_root` + bundle + sync + `clean_up_output`
/// (`sync_artifacts_with_stage`).
pub fn sync_artifacts_with_stage(
    cli: &mut Cli,
    bundle_root: &std::path::Path,
    stage_root: &str,
    prune: bool,
    artifacts: &[PyV],
) -> Result<()> {
    let root = cli.project_root();
    if bundle_root.exists() {
        std::fs::remove_dir_all(bundle_root)?;
    }
    std::fs::create_dir_all(bundle_root)?;
    let map = crate::artifacts::bundle(&root, bundle_root, artifacts)?;
    crate::artifacts::sync_deploy_root_with_stage(cli, bundle_root, &map, prune, stage_root)?;
    let out = root.join("output");
    if out.exists() {
        std::fs::remove_dir_all(out)?;
    }
    Ok(())
}
