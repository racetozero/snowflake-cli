//! `snow ws` (workspace: snowflake.yml entities).

use snow_api::output::Output;
use snow_api::project::{model, yaml};
use snow_api::{Cli, Result};

pub fn dump(cli: &mut Cli) -> Result<Output> {
    let p = cli.project(false)?.expect("project");
    let dumped = model::dump(&p.definition, false);
    Ok(Output::message(yaml::safe_dump(&dumped)))
}

const ACTIONS: [(&str, &[&str]); 10] = [
    (
        "application",
        &["action_deploy", "action_drop", "action_events"],
    ),
    (
        "application package",
        &[
            "action_bundle",
            "action_deploy",
            "action_diff",
            "action_drop",
            "action_publish",
            "action_release_channel_add_accounts",
            "action_release_channel_add_version",
            "action_release_channel_list",
            "action_release_channel_remove_accounts",
            "action_release_channel_remove_version",
            "action_release_channel_set_accounts",
            "action_release_directive_add_accounts",
            "action_release_directive_list",
            "action_release_directive_remove_accounts",
            "action_release_directive_set",
            "action_release_directive_unset",
            "action_validate",
            "action_version_create",
            "action_version_drop",
            "action_version_list",
        ],
    ),
    (
        "streamlit",
        &[
            "action_bundle",
            "action_deploy",
            "action_describe",
            "action_drop",
            "action_execute",
            "action_get_url",
            "action_share",
        ],
    ),
    (
        "procedure",
        &[
            "action_bundle",
            "action_deploy",
            "action_describe",
            "action_drop",
            "action_execute",
        ],
    ),
    (
        "function",
        &[
            "action_bundle",
            "action_deploy",
            "action_describe",
            "action_drop",
            "action_execute",
        ],
    ),
    ("compute-pool", &[]),
    ("image-repository", &[]),
    ("service", &[]),
    (
        "notebook",
        &[
            "action_create",
            "action_deploy",
            "action_describe",
            "action_drop",
            "action_teardown",
        ],
    ),
    ("snowflake-app", &[]),
];

/// `WorkspaceManager(...).perform_action(entity_id, action)`: entity lookup and support check.
fn entity_for(cli: &mut Cli, action: &str) -> Result<(String, snow_api::project::PyV, String)> {
    let id = cli.str("entity_id").unwrap_or_default();
    let p = cli.project(false)?.expect("project");
    if !p.meets_version("2") {
        return Err(snow_api::SnowError::cli(format!(
            "This command only supports definition version 2.x, got {}.",
            p.version()
        ))
        .named("InvalidProjectDefinitionVersionError"));
    }
    let Some(model) = p.entities().get(&id).cloned() else {
        return Err(
            snow_api::SnowError::unexpected(format!("No such entity ID: {id}")).named("ValueError"),
        );
    };
    let ty = model.str_of("type").unwrap_or_default();
    // `SnowparkEntity.__init__` refuses unless ENABLE_NATIVE_APP_CHILDREN is on.
    if (ty == "function" || ty == "procedure")
        && cli
            .config
            .feature("ENABLE_NATIVE_APP_CHILDREN", Some(false))
            != Some(true)
    {
        return Err(
            snow_api::SnowError::unexpected("Snowpark entity is not implemented yet")
                .named("NotImplementedError"),
        );
    }
    let supported = ACTIONS
        .iter()
        .find(|(t, _)| *t == ty)
        .is_some_and(|(_, a)| a.contains(&action));
    if !supported {
        return Err(snow_api::SnowError::unexpected(format!(
            "This entity type does not support \"{action}\""
        ))
        .named("ValueError"));
    }
    Ok((id, model, ty))
}

/// Unreachable: `entity_for` already refused unsupported pairs.
fn unported(_ty: &str, action: &str) -> snow_api::SnowError {
    snow_api::SnowError::unexpected(format!("This entity type does not support \"{action}\""))
        .named("ValueError")
}

pub fn bundle(cli: &mut Cli) -> Result<Output> {
    let (id, model, ty) = entity_for(cli, "action_bundle")?;
    match ty.as_str() {
        "application package" => {
            let pkg = crate::nativeapp::commands::ws_package(cli, &id, &model)?;
            crate::nativeapp::commands::ws_bundle(cli, &pkg)
        }
        "streamlit" => {
            let root = crate::streamlit::bundle_entity(cli, &id, &model)?;
            Ok(Output::message(format!(
                "Bundle generated at {}",
                root.display()
            )))
        }
        "function" | "procedure" => Err(snow_api::SnowError::unexpected(
            "SnowparkEntity.action_bundle() missing 2 required positional arguments: 'ignore_anaconda' and 'skip_version_check'",
        )
        .named("TypeError")),
        other => Err(unported(other, "ws bundle")),
    }
}

/// `SnowparkEntity.get_deploy_sql(CreateMode.create)`.
fn snowpark_deploy_sql(model: &snow_api::project::PyV, id: &str) -> Result<String> {
    use snow_api::project::PyV;
    let f = crate::entities::fqn(model, id)?;
    let ty = model.str_of("type").unwrap_or_default().to_uppercase();
    let list = |k: &str| {
        model
            .get(k)
            .and_then(PyV::as_list)
            .map(|l| l.iter().map(PyV::py_str).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let runtime = model
        .get("runtime")
        .filter(|v| !v.is_none())
        .map(PyV::py_str)
        .unwrap_or_else(|| "3.10".into());
    let mut q = vec![
        format!("CREATE {ty} {}", f.sql_identifier()),
        "COPY GRANTS".to_string(),
        format!("RETURNS {}", model.str_of("returns").unwrap_or_default()),
        "LANGUAGE PYTHON".to_string(),
        format!("RUNTIME_VERSION '{runtime}'"),
        format!("IMPORTS={}", list("imports").join(",")),
        format!("HANDLER='{}'", model.str_of("handler").unwrap_or_default()),
    ];
    let eai = list("external_access_integrations");
    if !eai.is_empty() {
        q.push(format!(
            "external_access_integrations=({})",
            eai.iter()
                .map(|x| crate::streamlit::sql_object_name(x))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(PyV::Dict(sec)) = model.get("secrets")
        && !sec.is_empty()
    {
        let items: Vec<String> = sec
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    snow_api::identifiers::to_string_literal(k),
                    crate::streamlit::sql_object_name(&v.py_str())
                )
            })
            .collect();
        q.push(format!("secrets=({})", items.join(", ")));
    }
    let repo = model
        .str_of("artifact_repository")
        .filter(|r| !r.is_empty());
    let (rp, pk) = (list("artifact_repository_packages"), list("packages"));
    if let Some(r) = repo
        && (!rp.is_empty() || !pk.is_empty())
    {
        let pkgs: Vec<String> = if !rp.is_empty() { rp } else { pk }
            .iter()
            .map(|x| format!("'{x}'"))
            .collect();
        q.push(format!("ARTIFACT_REPOSITORY= {r} "));
        q.push(format!("PACKAGES=({})", pkgs.join(",")));
    }
    if let Some(PyV::Dict(rc)) = model.get("resource_constraint")
        && !rc.is_empty()
    {
        q.push(format!(
            "RESOURCE_CONSTRAINT=({})",
            rc.iter()
                .map(|(k, v)| format!("{k}='{}'", v.py_str()))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if ty == "PROCEDURE" && matches!(model.get("execute_as_caller"), Some(PyV::Bool(true))) {
        q.push("EXECUTE AS CALLER".into());
    }
    Ok(q.join("\n"))
}

fn application_entity(
    cli: &mut Cli,
    id: &str,
    model: &snow_api::project::PyV,
) -> Result<(
    crate::nativeapp::application::App,
    crate::nativeapp::package::Package,
)> {
    let project = cli.project(false)?.expect("project").clone();
    let target = model
        .get("from_")
        .and_then(|f| f.str_of("target"))
        .unwrap_or_default();
    let Some(pkg_model) = project.entities().get(&target).cloned() else {
        return Err(
            snow_api::SnowError::unexpected(format!("No such entity ID: {target}"))
                .named("ValueError"),
        );
    };
    let root = cli.project_root();
    let app = crate::nativeapp::application::App {
        id: id.to_string(),
        model: model.clone(),
        project: project.clone(),
        project_root: root.clone(),
    };
    let pkg = crate::nativeapp::package::Package {
        id: target,
        model: pkg_model,
        project_root: root,
        project,
    };
    Ok((app, pkg))
}

pub fn deploy(cli: &mut Cli) -> Result<Output> {
    let paths = cli.list("paths");
    let (mut prune, mut recursive) = (cli.opt_bool("prune"), cli.opt_bool("recursive"));
    if prune.is_none() && recursive.is_none() && paths.is_empty() {
        prune = Some(true);
        recursive = Some(true);
    }
    let _ = recursive;
    if !paths.is_empty() && prune == Some(true) {
        return Err(snow_api::SnowError::incompatible(&["paths", "--prune"]));
    }
    let (id, model, ty) = entity_for(cli, "action_deploy")?;
    match ty.as_str() {
        "application package" => {
            let pkg = crate::nativeapp::commands::ws_package(cli, &id, &model)?;
            crate::nativeapp::commands::ws_deploy(cli, &pkg)
        }
        "function" | "procedure" => {
            let q = snowpark_deploy_sql(&model, &id)?;
            cli.execute_query(&q)?;
            Ok(Output::message("Deployed successfully."))
        }
        "application" => {
            let (app, pkg) = application_entity(cli, &id, &model)?;
            app.deploy(
                cli,
                &pkg,
                cli.flag("from_release_directive"),
                None,
                cli.flag("validate"),
                cli.flag("interactive"),
                None,
                None,
                cli.flag("force"),
            )?;
            Ok(Output::message("Deployed successfully."))
        }
        "streamlit" => Err(snow_api::SnowError::unexpected(
            "StreamlitEntity.deploy() missing 2 required positional arguments: '_open' and 'replace'",
        )
        .named("TypeError")),
        "notebook" => Err(snow_api::SnowError::unexpected(
            "NotebookEntity.action_deploy() missing 1 required positional argument: 'replace'",
        )
        .named("TypeError")),
        other => Err(unported(other, "ws deploy")),
    }
}

pub fn drop(cli: &mut Cli) -> Result<Output> {
    let (id, model, ty) = entity_for(cli, "action_drop")?;
    match ty.as_str() {
        "application package" => {
            let pkg = crate::nativeapp::commands::ws_package(cli, &id, &model)?;
            crate::nativeapp::commands::ws_drop(cli, &pkg)
        }
        "streamlit" => {
            let f = crate::entities::fqn(&model, &id)?;
            cli.execute_query(&format!("DROP STREAMLIT {};", f.sql_identifier()))?;
            Ok(Output::Empty)
        }
        "notebook" => Err(snow_api::SnowError::click(
            "action DROP not supported by NOTEBOOK entity",
        )),
        "function" | "procedure" => {
            let f = crate::entities::fqn(&model, &id)?;
            cli.execute_query(&format!(
                "DROP {} {};",
                ty.to_uppercase(),
                f.sql_identifier()
            ))?;
            Ok(Output::Empty)
        }
        "application" => {
            let (app, _) = application_entity(cli, &id, &model)?;
            app.drop(
                cli,
                cli.flag("interactive"),
                cli.flag("force"),
                cli.opt_bool("cascade"),
            )?;
            Ok(Output::Empty)
        }
        other => Err(unported(other, "ws drop")),
    }
}

pub fn simple(cli: &mut Cli, action: &str) -> Result<Output> {
    let (id, model, ty) = entity_for(cli, action)?;
    if ty == "application package" {
        use crate::nativeapp::commands as c;
        let pkg = c::ws_package(cli, &id, &model)?;
        return match action {
            "action_validate" => c::ws_validate(cli, &pkg),
            "action_version_list" => c::ws_version_list(cli, &pkg),
            "action_version_create" => c::ws_version_create(cli, &pkg),
            "action_version_drop" => c::ws_version_drop(cli, &pkg),
            _ => Err(unported(&ty, action)),
        };
    }
    Err(unported(&ty, action))
}
