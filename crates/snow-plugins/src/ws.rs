//! `snow ws` (workspace: snowflake.yml entities).

use crate::entities::EntityType;
use snow_api::output::Output;
use snow_api::project::{model, yaml};
use snow_api::{Cli, Result};

pub fn dump(cli: &mut Cli) -> Result<Output> {
    let p = cli.project(false)?.expect("project");
    let dumped = model::dump(&p.definition, false);
    Ok(Output::message(yaml::safe_dump(&dumped)))
}

const ACTIONS: [(EntityType, &[&str]); 10] = [
    (
        EntityType::Application,
        &["action_deploy", "action_drop", "action_events"],
    ),
    (
        EntityType::ApplicationPackage,
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
        EntityType::Streamlit,
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
        EntityType::Procedure,
        &[
            "action_bundle",
            "action_deploy",
            "action_describe",
            "action_drop",
            "action_execute",
        ],
    ),
    (
        EntityType::Function,
        &[
            "action_bundle",
            "action_deploy",
            "action_describe",
            "action_drop",
            "action_execute",
        ],
    ),
    (EntityType::ComputePool, &[]),
    (EntityType::ImageRepository, &[]),
    (EntityType::Service, &[]),
    (
        EntityType::Notebook,
        &[
            "action_create",
            "action_deploy",
            "action_describe",
            "action_drop",
            "action_teardown",
        ],
    ),
    (EntityType::SnowflakeApp, &[]),
];

/// `WorkspaceManager(...).perform_action(entity_id, action)`: entity lookup and support check.
fn entity_for(cli: &mut Cli, action: &str) -> Result<(String, snow_api::project::PyV, EntityType)> {
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
    let ty = EntityType::of(&model);
    // `SnowparkEntity.__init__` refuses unless ENABLE_NATIVE_APP_CHILDREN is on.
    if matches!(ty, Some(EntityType::Function | EntityType::Procedure))
        && !cli
            .config
            .feature(snow_api::config::FeatureFlag::EnableNativeAppChildren)
    {
        return Err(
            snow_api::SnowError::unexpected("Snowpark entity is not implemented yet")
                .named("NotImplementedError"),
        );
    }
    let Some(ty) = ty.filter(|ty| ACTIONS.iter().any(|(t, a)| t == ty && a.contains(&action)))
    else {
        return Err(snow_api::SnowError::unexpected(format!(
            "This entity type does not support \"{action}\""
        ))
        .named("ValueError"));
    };
    Ok((id, model, ty))
}

/// Unreachable: `entity_for` already refused unsupported pairs.
fn unported(action: &str) -> snow_api::SnowError {
    snow_api::SnowError::unexpected(format!("This entity type does not support \"{action}\""))
        .named("ValueError")
}

pub fn bundle(cli: &mut Cli) -> Result<Output> {
    let (id, model, ty) = entity_for(cli, "action_bundle")?;
    match ty {
        EntityType::ApplicationPackage => {
            let pkg = crate::nativeapp::commands::ws_package(cli, &id, &model)?;
            crate::nativeapp::commands::ws_bundle(cli, &pkg)
        }
        EntityType::Streamlit => {
            let root = crate::streamlit::bundle_entity(cli, &id, &model)?;
            Ok(Output::message(format!(
                "Bundle generated at {}",
                root.display()
            )))
        }
        EntityType::Function | EntityType::Procedure => Err(snow_api::SnowError::unexpected(
            "SnowparkEntity.action_bundle() missing 2 required positional arguments: 'ignore_anaconda' and 'skip_version_check'",
        )
        .named("TypeError")),
        _ => Err(unported("ws bundle")),
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
                .map(|x| snow_api::identifiers::sql_object_name(x))
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
                    snow_api::identifiers::sql_object_name(&v.py_str())
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
    let pkg = crate::nativeapp::commands::ws_package(cli, &target, &pkg_model)?;
    let app = crate::nativeapp::application::App {
        id: id.to_string(),
        model: model.clone(),
        project,
        project_root: cli.project_root(),
    };
    Ok((app, pkg))
}

pub fn deploy(cli: &mut Cli) -> Result<Output> {
    crate::nativeapp::commands::deploy_flags(cli)?;
    let (id, model, ty) = entity_for(cli, "action_deploy")?;
    match ty {
        EntityType::ApplicationPackage => {
            let pkg = crate::nativeapp::commands::ws_package(cli, &id, &model)?;
            crate::nativeapp::commands::ws_deploy(cli, &pkg)
        }
        EntityType::Function | EntityType::Procedure => {
            let q = snowpark_deploy_sql(&model, &id)?;
            cli.execute_query(&q)?;
            Ok(Output::message("Deployed successfully."))
        }
        EntityType::Application => {
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
        EntityType::Streamlit => Err(snow_api::SnowError::unexpected(
            "StreamlitEntity.deploy() missing 2 required positional arguments: '_open' and 'replace'",
        )
        .named("TypeError")),
        EntityType::Notebook => Err(snow_api::SnowError::unexpected(
            "NotebookEntity.action_deploy() missing 1 required positional argument: 'replace'",
        )
        .named("TypeError")),
        _ => Err(unported("ws deploy")),
    }
}

pub fn drop(cli: &mut Cli) -> Result<Output> {
    let (id, model, ty) = entity_for(cli, "action_drop")?;
    match ty {
        EntityType::ApplicationPackage => {
            let pkg = crate::nativeapp::commands::ws_package(cli, &id, &model)?;
            crate::nativeapp::commands::ws_drop(cli, &pkg)
        }
        EntityType::Streamlit => {
            let f = crate::entities::fqn(&model, &id)?;
            cli.execute_query(&format!("DROP STREAMLIT {};", f.sql_identifier()))?;
            Ok(Output::Empty)
        }
        EntityType::Notebook => Err(snow_api::SnowError::click(
            "action DROP not supported by NOTEBOOK entity",
        )),
        EntityType::Function | EntityType::Procedure => {
            let f = crate::entities::fqn(&model, &id)?;
            cli.execute_query(&format!(
                "DROP {} {};",
                ty.as_str().to_uppercase(),
                f.sql_identifier()
            ))?;
            Ok(Output::Empty)
        }
        EntityType::Application => {
            let (app, _) = application_entity(cli, &id, &model)?;
            app.drop(
                cli,
                cli.flag("interactive"),
                cli.flag("force"),
                cli.opt_bool("cascade"),
            )?;
            Ok(Output::Empty)
        }
        _ => Err(unported("ws drop")),
    }
}

pub fn simple(cli: &mut Cli, action: &str) -> Result<Output> {
    let (id, model, ty) = entity_for(cli, action)?;
    if ty == EntityType::ApplicationPackage {
        use crate::nativeapp::commands as c;
        let pkg = c::ws_package(cli, &id, &model)?;
        return match action {
            "action_validate" => c::ws_validate(cli, &pkg),
            "action_version_list" => c::ws_version_list(cli, &pkg),
            "action_version_create" => c::ws_version_create(cli, &pkg),
            "action_version_drop" => c::ws_version_drop(cli, &pkg),
            _ => Err(unported(action)),
        };
    }
    Err(unported(action))
}
