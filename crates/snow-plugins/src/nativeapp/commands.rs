//! `snow app bundle|deploy|validate|version create|version drop` and the `ws`
//! actions on application package entities.

use super::package::Package;
use snow_api::identifiers::to_identifier;
use snow_api::output::{Output, Pv};
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};
use std::path::PathBuf;

/// `_reject_snowflake_app_options(command, **options)`.
fn reject_runtime_options(command: &str, options: &[(&str, bool)]) -> Result<()> {
    let mut set: Vec<&str> = options
        .iter()
        .filter(|(_, v)| *v)
        .map(|(n, _)| *n)
        .collect();
    if set.is_empty() {
        return Ok(());
    }
    set.sort();
    Err(SnowError::click(format!(
        "'snow app {command}' was invoked against a Native App entity (application / application package), but the following Snowflake App Runtime options were provided: {}. Remove them and try again.",
        set.join(", ")
    )))
}

fn package(cli: &mut Cli) -> Result<Package> {
    let e = super::entities(cli, false)?;
    Ok(Package::new(cli, &e))
}

/// Deploy flags: `prune`/`recursive` defaults and the `paths`/`--prune` conflict.
fn deploy_flags(cli: &Cli) -> Result<(bool, bool, Vec<PathBuf>)> {
    let paths: Vec<PathBuf> = cli.list("paths").into_iter().map(PathBuf::from).collect();
    let (prune, recursive) = match (cli.opt_bool("prune"), cli.opt_bool("recursive")) {
        (None, None) if paths.is_empty() => (true, true),
        (p, r) => (p.unwrap_or(false), r.unwrap_or(false)),
    };
    if !paths.is_empty() && prune {
        return Err(SnowError::incompatible(&["paths", "--prune"]));
    }
    Ok((prune, recursive, paths))
}

pub fn bundle(cli: &mut Cli) -> Result<Output> {
    match crate::apps::detect(cli)? {
        crate::apps::Flow::Native => {}
        flow => return crate::apps::bundle(cli, flow),
    }
    let pkg = package(cli)?;
    pkg.bundle(cli)?;
    let root = pkg
        .project_root
        .join(pkg.model.str_of("deploy_root").unwrap_or_default());
    Ok(Output::message(format!(
        "Bundle generated at {}",
        snow_api::project::model::pure_path(&root.to_string_lossy())
    )))
}

pub fn deploy(cli: &mut Cli) -> Result<Output> {
    match crate::apps::detect(cli)? {
        crate::apps::Flow::Native => {}
        flow => return crate::apps::deploy::deploy(cli, flow),
    }
    reject_runtime_options(
        "deploy",
        &[
            ("--upload-only", cli.flag("upload_only")),
            ("--build-only", cli.flag("build_only")),
            (
                "--promote-only",
                cli.flag("promote_only") || cli.flag("deploy_only"),
            ),
            ("--skip-certs-check", cli.flag("skip_certs_check")),
            ("--target", cli.str("target").is_some()),
        ],
    )?;
    let (prune, recursive, paths) = deploy_flags(cli)?;
    let pkg = package(cli)?;
    let stage = pkg.stage_path()?;
    let (validate, interactive, force) = (
        cli.flag("validate"),
        cli.flag("interactive"),
        cli.flag("force"),
    );
    pkg.deploy(
        cli,
        None,
        prune,
        recursive,
        &paths,
        true,
        validate,
        &stage,
        interactive,
        force,
        true,
    )?;
    Ok(Output::message(
        "Deployed successfully. Application package and stage are up-to-date.",
    ))
}

pub fn validate(cli: &mut Cli) -> Result<Output> {
    match crate::apps::detect(cli)? {
        crate::apps::Flow::Native => {}
        flow => return crate::apps::validate(cli, flow),
    }
    reject_runtime_options("validate", &[("--target", cli.str("target").is_some())])?;
    let pkg = package(cli)?;
    if cli.format.is_json() {
        let r = pkg.validation_result(cli, true, false, true)?;
        return Ok(json_object(&r));
    }
    pkg.validate_setup_script(cli, true, false, true)?;
    cli.console.message("Setup script is valid");
    Ok(Output::message(
        "Snowflake Native App validation succeeded.",
    ))
}

fn json_object(v: &serde_json::Value) -> Output {
    match v {
        serde_json::Value::Object(m) => Output::object(
            m.iter()
                .map(|(k, v)| (k.as_str(), Pv::from_json(v)))
                .collect(),
        ),
        other => Output::object(vec![("result", Pv::from_json(other))]),
    }
}

pub fn version_create(cli: &mut Cli) -> Result<Output> {
    let pkg = package(cli)?;
    let info = pkg.version_create(
        cli,
        cli.str("version"),
        cli.int("patch"),
        cli.str("label"),
        cli.flag("skip_git_check"),
        cli.flag("interactive"),
        cli.flag("force"),
        cli.flag("from_stage"),
    )?;
    let message = "Version create is now complete.";
    if cli.format.is_json() {
        return Ok(Output::object(vec![
            ("message", Pv::s(message)),
            ("version", Pv::s(to_identifier(&info.version))),
            (
                "patch",
                info.patch.map(|p| Pv::Int(p as i128)).unwrap_or(Pv::None),
            ),
            ("label", Pv::opt(info.label.clone())),
        ]));
    }
    Ok(Output::message(message))
}

pub fn version_drop(cli: &mut Cli) -> Result<Output> {
    let pkg = package(cli)?;
    let m = pkg.version_drop(
        cli,
        cli.str("version"),
        cli.flag("interactive"),
        cli.flag("force"),
    )?;
    Ok(Output::message(m))
}

/// The application package entity named by `ws --entity-id`.
pub fn ws_package(cli: &mut Cli, id: &str, model: &PyV) -> Result<Package> {
    let project = cli.project(false)?.expect("project").clone();
    Ok(Package {
        id: id.to_string(),
        model: model.clone(),
        project_root: cli.project_root(),
        project,
    })
}

pub fn ws_bundle(cli: &mut Cli, pkg: &Package) -> Result<Output> {
    let map = pkg.bundle(cli)?;
    Ok(Output::message(format!(
        "Bundle generated at {}",
        map.deploy_root.display()
    )))
}

pub fn ws_deploy(cli: &mut Cli, pkg: &Package) -> Result<Output> {
    let (prune, recursive, paths) = deploy_flags(cli)?;
    let stage = pkg.stage_path()?;
    let (validate, interactive, force) = (
        cli.flag("validate"),
        cli.flag("interactive"),
        cli.flag("force"),
    );
    pkg.deploy(
        cli,
        None,
        prune,
        recursive,
        &paths,
        true,
        validate,
        &stage,
        interactive,
        force,
        true,
    )?;
    Ok(Output::message("Deployed successfully."))
}

pub fn ws_validate(cli: &mut Cli, pkg: &Package) -> Result<Output> {
    let (interactive, force) = (cli.flag("interactive"), cli.flag("force"));
    pkg.validate_setup_script(cli, true, interactive, force)?;
    cli.console.message("Setup script is valid");
    Ok(Output::Empty)
}

pub fn ws_version_list(cli: &mut Cli, pkg: &Package) -> Result<Output> {
    let name = pkg.name()?;
    let role = pkg.role(cli)?;
    let rows = super::facade::show_versions(cli, &name, role.as_deref())?;
    Ok(Output::Collection(rows))
}

pub fn ws_version_create(cli: &mut Cli, pkg: &Package) -> Result<Output> {
    pkg.version_create(
        cli,
        cli.str("version"),
        cli.int("patch"),
        cli.str("label"),
        cli.flag("skip_git_check"),
        cli.flag("interactive"),
        cli.flag("force"),
        false,
    )?;
    Ok(Output::Empty)
}

pub fn ws_version_drop(cli: &mut Cli, pkg: &Package) -> Result<Output> {
    pkg.version_drop(
        cli,
        cli.str("version"),
        cli.flag("interactive"),
        cli.flag("force"),
    )?;
    Ok(Output::Empty)
}

pub fn ws_drop(cli: &mut Cli, pkg: &Package) -> Result<Output> {
    pkg.drop(cli, cli.flag("force"))?;
    Ok(Output::Empty)
}

/// `native_app_only(command)`: refuse projects with only snowflake-app entities.
fn native_only(cli: &mut Cli, command: &str) -> Result<()> {
    let p = cli.project(false)?.expect("project");
    let types: Vec<String> = p
        .entities()
        .values()
        .filter_map(|e| e.str_of("type"))
        .map(|t| t.to_lowercase())
        .collect();
    if !types.is_empty() && types.iter().all(|t| t == "snowflake-app") {
        return Err(SnowError::click(format!(
            "'snow app {command}' is only available for Native App projects (entity types: application, application package). Your project contains snowflake-app entities."
        )));
    }
    Ok(())
}

fn app_and_package(cli: &mut Cli) -> Result<(super::application::App, Package, String)> {
    let e = super::entities(cli, true)?;
    let pkg = Package::new(cli, &e);
    let app = super::application::App::new(cli, &e)
        .ok_or_else(|| SnowError::unexpected("app entity required"))?;
    let raw_name = crate::entities::fqn(&app.model, &app.id)?.name;
    Ok((app, pkg, raw_name))
}

pub fn run(cli: &mut Cli) -> Result<Output> {
    native_only(cli, "run")?;
    let (app, pkg, raw_name) = app_and_package(cli)?;
    app.deploy(
        cli,
        &pkg,
        cli.flag("from_release_directive"),
        cli.str("channel"),
        cli.flag("validate"),
        cli.flag("interactive"),
        cli.str("version"),
        cli.int("patch"),
        cli.flag("force"),
    )?;
    let url = app.snowsight_url(cli)?;
    Ok(Output::message(format!(
        "Your application object ({raw_name}) is now available:\n{url}"
    )))
}

pub fn open(cli: &mut Cli) -> Result<Output> {
    match crate::apps::detect(cli)? {
        crate::apps::Flow::Native => {}
        flow => return crate::apps::open(cli, flow),
    }
    reject_runtime_options(
        "open",
        &[
            ("--print-only", cli.flag("print_only")),
            ("--settings", cli.flag("settings")),
            ("--watch", cli.flag("watch")),
            ("--target", cli.str("target").is_some()),
        ],
    )?;
    let (app, _, _) = app_and_package(cli)?;
    let name = app.name()?;
    let role = app.role(cli)?;
    if super::facade::get_existing_app_info(cli, &name, role.as_deref())?.is_some() {
        let url = app.snowsight_url(cli)?;
        crate::common::launch(&url);
        return Ok(Output::message("Snowflake Native App opened in browser."));
    }
    Ok(Output::message(
        "Snowflake Native App not yet deployed! Please run \"snow app run\" first.",
    ))
}

pub fn teardown(cli: &mut Cli) -> Result<Output> {
    match crate::apps::detect(cli)? {
        crate::apps::Flow::Native => {}
        flow => return crate::apps::teardown(cli, flow),
    }
    reject_runtime_options("teardown", &[("--target", cli.str("target").is_some())])?;
    let is_v1 = !cli.project(false)?.expect("project").meets_version("2");
    let project = cli.project_v2_in_memory()?;
    let ents = project.entities();
    let mut pkg_opt = cli.str("package_entity_id").unwrap_or_default();
    if is_v1 {
        pkg_opt = ents
            .iter()
            .filter(|(_, e)| e.str_of("type").as_deref() == Some("application package"))
            .map(|(k, _)| k.clone())
            .next_back()
            .unwrap_or_default();
    }
    let packages: Vec<(&String, &PyV)> = ents
        .iter()
        .filter(|(_, e)| e.str_of("type").as_deref() == Some("application package"))
        .collect();
    let (pkg_id, pkg_model) = if !pkg_opt.is_empty() {
        match packages.iter().find(|(k, _)| **k == pkg_opt) {
            Some((k, v)) => ((*k).clone(), (*v).clone()),
            None => {
                return Err(SnowError::click(format!(
                    "Could not find an application package entity with ID \"{pkg_opt}\" in the project definition file."
                )));
            }
        }
    } else if packages.len() == 1 {
        (packages[0].0.clone(), packages[0].1.clone())
    } else if packages.len() > 1 {
        return Err(SnowError::click(
            "More than one application package entity exists in the project definition file, specify --package-entity-id to choose which one to operate on.",
        ));
    } else {
        return Err(SnowError::click(
            "Could not find an application package entity in the project definition file.",
        ));
    };
    let pname = crate::entities::fqn(&pkg_model, &pkg_id)?.name;
    let same: Vec<String> = packages
        .iter()
        .filter(|(k, v)| {
            crate::entities::fqn(v, k)
                .is_ok_and(|f| snow_api::identifiers::same_identifiers(&f.name, &pname))
        })
        .map(|(k, _)| (*k).clone())
        .collect();
    let (force, interactive, cascade) = (
        cli.flag("force"),
        cli.flag("interactive"),
        cli.opt_bool("cascade"),
    );
    let root = cli.project_root();
    for (id, e) in &ents {
        if e.str_of("type").as_deref() != Some("application") {
            continue;
        }
        let target = e
            .get("from_")
            .and_then(|f| f.str_of("target"))
            .unwrap_or_default();
        if same.contains(&target) {
            let app = super::application::App {
                id: id.clone(),
                model: e.clone(),
                project: project.clone(),
                project_root: root.clone(),
            };
            app.drop(cli, interactive, force, cascade)?;
        }
    }
    let pkg = Package {
        id: pkg_id,
        model: pkg_model,
        project_root: root,
        project,
    };
    pkg.drop(cli, force)?;
    Ok(Output::message("Teardown is now complete."))
}

pub fn events(cli: &mut Cli) -> Result<Output> {
    match crate::apps::detect(cli)? {
        crate::apps::Flow::Native => {}
        flow => return crate::apps::events::events(cli, flow),
    }
    reject_runtime_options(
        "events",
        &[
            ("--metric", cli.str("metric").is_some_and(|m| !m.is_empty())),
            ("--raw", cli.flag("raw")),
            ("--target", cli.str("target").is_some()),
            ("--instance", cli.int("instance").is_some()),
        ],
    )?;
    let first = cli.int("first").unwrap_or(-1);
    let mut last = cli.int("last").unwrap_or(-1);
    let (org, account) = (
        cli.str("consumer_org").unwrap_or_default(),
        cli.str("consumer_account").unwrap_or_default(),
    );
    if first >= 0 && last >= 0 {
        return Err(SnowError::incompatible(&["--first", "--last"]));
    }
    if org.is_empty() != account.is_empty() {
        return Err(SnowError::unmet(&["--consumer-org", "--consumer-account"]));
    }
    let follow = cli.flag("follow");
    let since = cli.str("since").unwrap_or_default();
    let until = cli.str("until").unwrap_or_default();
    if follow {
        if !until.is_empty() {
            return Err(SnowError::incompatible(&["--follow", "--until"]));
        }
        if first >= 0 {
            return Err(SnowError::incompatible(&["--follow", "--first"]));
        }
    }
    let (app, pkg, _) = app_and_package(cli)?;
    let mut types = Vec::new();
    for t in cli.list("record_types") {
        let t = t.to_lowercase();
        match t.as_str() {
            "log" | "span" | "span_event" => types.push(t.to_uppercase()),
            other => {
                return Err(SnowError::click(format!(
                    "'--type {other}' is not supported for Native App projects. Valid values are: log, span, span_event."
                )));
            }
        }
    }
    if follow && last == -1 && since.is_empty() {
        last = 20;
    }
    let scopes = cli.list("scopes");
    let pname = pkg.name()?;
    let app_hash = cli.str("consumer_app_hash").unwrap_or_default();
    let rows = app.events(
        cli, &pname, &since, &until, &types, &scopes, &org, &account, &app_hash, first, last,
    )?;
    if follow {
        // Polling is not reproducible in tests; one batch is fetched (see [deviations]).
        crate::logs::info(
            "snowflake.cli._plugins.nativeapp",
            "--follow polls for new events",
        );
    }
    let json = cli.format.is_json();
    let mut items: Vec<Output> = rows
        .into_iter()
        .map(|r| {
            if json {
                Output::Object(Some(r))
            } else {
                let g = |k: &str| r.get(k).map(Pv::py_str).unwrap_or_else(|| "None".into());
                Output::message(format!("{} {}", g("TIMESTAMP"), g("VALUE")))
            }
        })
        .collect();
    if follow {
        items.push(Output::message(""));
    }
    Ok(Output::Stream(items))
}

pub fn publish(cli: &mut Cli) -> Result<Output> {
    native_only(cli, "publish")?;
    let pkg = package(cli)?;
    let channel = cli.str("channel");
    let directive = cli.str("directive").unwrap_or_else(|| "DEFAULT".into());
    let info = pkg.publish(
        cli,
        cli.str("version"),
        cli.int("patch"),
        channel.clone(),
        &directive,
        cli.flag("interactive"),
        cli.flag("force"),
        cli.flag("create_version"),
        cli.flag("from_stage"),
        cli.str("label"),
    )?;
    Ok(Output::message(format!(
        "Version {} and patch {} published to release directive {directive} of release channel {}.",
        info.version,
        info.patch
            .map(|p| p.to_string())
            .unwrap_or_else(|| "None".into()),
        channel.unwrap_or_else(|| "None".into())
    )))
}

pub fn diff(cli: &mut Cli) -> Result<Output> {
    native_only(cli, "diff")?;
    let pkg = package(cli)?;
    let map = pkg.bundle(cli)?;
    let stage = pkg.stage_path()?;
    let diff = crate::stage_diff::compute(
        cli,
        &pkg.deploy_root(),
        &stage.stage_name,
        &stage.directory,
        &stage.full_path(),
    )?;
    if cli.format.is_json() {
        return Ok(diff.to_output());
    }
    crate::stage_diff::print_with_map(cli, &diff, Some(&map));
    Ok(Output::Empty)
}

/// `snow app setup` (`_plugins/apps/commands.py::snowflake_app_setup`): write app.yml.
pub fn setup(cli: &mut Cli) -> Result<Output> {
    if cli.flag("capabilities") {
        return setup_capabilities(cli);
    }
    let dry_run = cli.flag("dry_run");
    let r = setup_inner(cli, dry_run);
    // A dry run neutralises the exit code of ClickExceptions (rendered as usual).
    match r {
        Err(SnowError::Cli {
            message,
            class,
            after,
            name,
        }) if dry_run => {
            let class = if class == snow_api::errors::ErrorClass::Click {
                snow_api::errors::ErrorClass::ExitZeroClick
            } else {
                snow_api::errors::ErrorClass::ExitZero
            };
            Err(SnowError::Cli {
                message,
                class,
                after,
                name,
            })
        }
        Err(SnowError::Click(snow_click::ClickError::Exception { message, .. })) if dry_run => {
            Err(SnowError::Click(snow_click::ClickError::Exception {
                message,
                code: 0,
            }))
        }
        other => other,
    }
}

fn setup_inner(cli: &mut Cli, dry_run: bool) -> Result<Output> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let name = match cli.str("app_name") {
        Some(n) => n,
        None => {
            let d = cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            regex::Regex::new(r"[^a-zA-Z0-9_]")
                .expect("re")
                .replace_all(&d.replace([' ', '-'], "_"), "")
                .into_owned()
        }
    };
    if name.is_empty() {
        return Err(SnowError::click(
            "Could not derive app name from the current directory. Please provide --app-name.",
        ));
    }
    if !regex::Regex::new(r"^[a-zA-Z0-9_]+$")
        .expect("re")
        .is_match(&name)
    {
        return Err(SnowError::click(format!(
            "Invalid app name '{name}'. Only letters, digits, and underscores are allowed."
        )));
    }
    let project_file = cwd.join("app.yml");
    if !dry_run {
        for existing in ["app.yml", "snowflake.yml"] {
            if cwd.join(existing).exists() {
                return Ok(Output::message(format!(
                    "{existing} already exists. Skipping initialization."
                )));
            }
        }
    }
    let conn_name = cli
        .str("connection")
        .unwrap_or_else(|| cli.config.default_connection_name());
    let conf = cli.config.connection_dict(&conn_name)?;
    let params = app_service_defaults(cli, &name)?;
    let flag = |cli: &Cli, k: &str| cli.str(k).filter(|v| !v.is_empty());
    let (cli_wh, cli_db, cli_schema) = (
        flag(cli, "warehouse"),
        flag(cli, "database"),
        flag(cli, "schema"),
    );
    if cli_db.is_some() && cli_schema.is_none() {
        return Err(SnowError::cli(
            "--schema is required when --database is specified. Provide --schema to select the schema within the requested database.",
        ));
    }
    let conf_get = |k: &str| conf.get(k).map(|v| v.py_str()).filter(|v| !v.is_empty());
    let personal_db =
        match cli.try_query("SELECT 'USER$' || CURRENT_USER() AS personal_database")? {
            Ok(c) => c
                .rows
                .first()
                .and_then(|r| r.first().cloned().flatten())
                .filter(|v| !v.is_empty() && !v.ends_with('$')),
            Err(_) => {
                crate::logs::log(
                    crate::logs::WARNING,
                    "snowflake.cli._plugins.apps.manager",
                    "Could not resolve personal database.",
                );
                None
            }
        };
    let personal_schema = personal_db.as_ref().map(|_| "PUBLIC".to_string());
    let pick = |u: Option<String>,
                a: Option<String>,
                d: Option<String>,
                s: Option<String>|
     -> (Option<String>, &'static str) {
        if u.is_some() {
            (u, "user input")
        } else if a.is_some() {
            (a, "account parameter")
        } else if d.is_some() {
            (d, "default")
        } else if s.is_some() {
            (s, "current session")
        } else {
            (None, "missing")
        }
    };
    let resolved: Vec<(&str, (Option<String>, &str))> = vec![
        (
            "database",
            pick(
                cli_db,
                params.get("database").cloned(),
                personal_db.clone(),
                conf_get("database"),
            ),
        ),
        (
            "schema",
            pick(
                cli_schema,
                params.get("schema").cloned(),
                personal_schema,
                conf_get("schema"),
            ),
        ),
        (
            "warehouse",
            pick(
                cli_wh,
                params.get("query_warehouse").cloned(),
                None,
                conf_get("warehouse"),
            ),
        ),
        (
            "build_eai",
            pick(
                cli.str("build_eai"),
                params.get("build_eai").cloned(),
                None,
                None,
            ),
        ),
    ];
    let val = |k: &str| {
        resolved
            .iter()
            .find(|(n, _)| *n == k)
            .and_then(|(_, (v, _))| v.clone())
    };
    for (k, param) in [
        ("database", "DEFAULT_SNOWFLAKE_APPS_DESTINATION_DATABASE"),
        ("schema", "DEFAULT_SNOWFLAKE_APPS_DESTINATION_SCHEMA"),
        ("warehouse", "DEFAULT_SNOWFLAKE_APPS_QUERY_WAREHOUSE"),
    ] {
        if val(k).is_none() {
            return Err(SnowError::click(format!(
                "Missing {k}. Provide --{k}, set the {param} account parameter, or check your connection."
            )));
        }
    }
    let (db, schema, wh) = (
        val("database").unwrap_or_default(),
        val("schema").unwrap_or_default(),
        val("warehouse").unwrap_or_default(),
    );
    let unq = db.trim().trim_matches('"').to_string();
    let use_workspace = unq.to_uppercase().starts_with("USER$");
    let code_storage = if use_workspace {
        "temporary workspace"
    } else {
        "temporary stage"
    };
    let fqn_name = to_identifier(&name.to_uppercase());
    let (fdb, fschema) = (to_identifier(&db), to_identifier(&schema));
    let app_exists = match cli.try_query(&format!(
        "SHOW APPLICATION SERVICES LIKE {} IN SCHEMA {fdb}.{fschema}",
        snow_api::identifiers::identifier_to_show_like_pattern(&fqn_name)
    ))? {
        Ok(c) => {
            let want = snow_api::identifiers::unquote_identifier(&fqn_name).to_uppercase();
            cli.rows(&c).iter().any(|r| {
                r.get("name")
                    .map(Pv::py_str)
                    .unwrap_or_default()
                    .to_uppercase()
                    == want
            })
        }
        Err(_) => false,
    };
    let python = ["pyproject.toml", "requirements.txt"]
        .iter()
        .any(|f| cwd.join(f).is_file());
    let node = cwd.join("package.json").is_file();
    let streamlit = python && has_streamlit_dependency(&cwd);
    if !dry_run {
        let yaml_str = |v: &str| {
            if v.contains('"') {
                format!("'{}'", v.replace('\'', "''"))
            } else {
                v.to_string()
            }
        };
        let mut lines = vec![
            "version: 2".to_string(),
            String::new(),
            format!("name: {}", name.to_uppercase()),
            format!("database: {}", yaml_str(&db)),
            format!("schema: {}", yaml_str(&schema)),
            format!("query_warehouse: {}", yaml_str(&wh)),
        ];
        if let Some(e) = val("build_eai").filter(|e| !e.is_empty()) {
            lines.push(format!("build_eai: {}", yaml_str(&e)));
        }
        if python && !node {
            lines.push(String::new());
            lines.push("runtime: python".into());
            if streamlit {
                lines.push("streamlit: true".into());
            }
            lines.push("compute_resource: SERVERLESS".into());
        }
        lines.extend(
            [
                "",
                "ignore:",
                "  - node_modules",
                "  - .env*",
                "  - __pycache__",
                "  - \"*.pyc\"",
                "  - .next",
                "  - .git",
                "  - snowflake.log",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        std::fs::write(&project_file, lines.join("\n") + "\n")?;
    }
    if cli.format.is_json() {
        let mut pairs: Vec<(&str, Pv)> = vec![
            ("success", Pv::Bool(!dry_run)),
            ("code_storage", Pv::s(code_storage)),
            ("app_exists", Pv::Bool(app_exists)),
            (
                "runtime",
                Pv::s(if python && !node { "python" } else { "node" }),
            ),
            ("streamlit", Pv::Bool(python && !node && streamlit)),
        ];
        for (k, (v, _)) in &resolved {
            pairs.push((k, Pv::opt(v.clone())));
        }
        return Ok(Output::object(pairs));
    }
    if dry_run {
        cli.console.step("Dry run — resolved configuration:");
    } else {
        cli.console
            .step("Initialized Snowflake App Runtime project in app.yml.");
    }
    for (k, (v, source)) in &resolved {
        if v.is_none() && *source == "missing" {
            continue;
        }
        cli.console.step(&format!(
            "  {k}: {}  ({source})",
            v.clone().unwrap_or_else(|| "None".into())
        ));
    }
    cli.console.step(&format!("  code storage: {code_storage}"));
    if python && node {
        let mut settings = vec![
            "`runtime: [node, python]`",
            "`compute_resource: SERVERLESS`",
        ];
        if streamlit {
            settings.push("`streamlit: true`");
        }
        let (last, rest) = settings.split_last().expect("non-empty");
        cli.console.warning(&format!(
            "Found package.json, so the app deploys as Node. If it also runs Python, set {} and {last}, and declare `install`, `build` and `run`.",
            rest.join(", ")
        ));
    } else if python {
        let kind = if streamlit { "Streamlit " } else { "" };
        cli.console.step(&format!(
            "  runtime: python  (detected {kind}Python project)"
        ));
        cli.console.warning(
            "Python apps have no default commands. Declare `run` in app.yml before deploying.",
        );
    }
    if app_exists {
        let hint = if dry_run {
            "pass a different --app-name"
        } else {
            "change name in app.yml"
        };
        cli.console.warning(&format!(
            "Application service {fdb}.{fschema}.{fqn_name} already exists. Deploying this project will replace that app's code and settings. To create a separate app, {hint}."
        ));
    }
    Ok(Output::Empty)
}

fn has_streamlit_dependency(dir: &std::path::Path) -> bool {
    let mut reqs: Vec<String> = Vec::new();
    if let Ok(t) = std::fs::read_to_string(dir.join("requirements.txt")) {
        reqs.extend(t.trim_start_matches('\u{feff}').lines().map(str::to_string));
    }
    if let Ok(t) = std::fs::read_to_string(dir.join("pyproject.toml"))
        && let Ok(doc) = t.parse::<toml_edit::DocumentMut>()
        && let Some(deps) = doc
            .get("project")
            .and_then(|p| p.get("dependencies"))
            .and_then(|d| d.as_array())
    {
        reqs.extend(deps.iter().filter_map(|d| d.as_str()).map(str::to_string));
    }
    let name_re = regex::Regex::new(r"^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?").expect("re");
    let sep = regex::Regex::new(r"[-_.]+").expect("re");
    reqs.iter().any(|r| {
        name_re
            .find(r.trim())
            .is_some_and(|m| sep.replace_all(&m.as_str().to_lowercase(), "-") == "streamlit")
    })
}

/// `SnowflakeAppManager.fetch_app_service_defaults(app_name)`.
fn app_service_defaults(cli: &mut Cli, app: &str) -> Result<indexmap::IndexMap<String, String>> {
    let mut out = indexmap::IndexMap::new();
    if let Ok(c) = cli.try_query("SELECT * FROM TABLE(SNOWFLAKE.APPS.GET_DEFAULT_APP_SPACE())")? {
        let rows = cli.rows(&c);
        if let Some(r) = rows.first() {
            for (col, key) in [
                ("DATABASE_NAME", "database"),
                ("QUERY_WAREHOUSE", "query_warehouse"),
                ("BUILD_EXTERNAL_ACCESS_INTEGRATION", "build_eai"),
            ] {
                let v = r
                    .get(col)
                    .or_else(|| r.get(&col.to_lowercase()))
                    .filter(|v| !matches!(v, Pv::None))
                    .map(Pv::py_str);
                if let Some(v) = v.filter(|v| !v.is_empty()) {
                    out.insert(key.to_string(), to_identifier(&v));
                }
            }
            if !out.is_empty() {
                out.insert("schema".into(), to_identifier(app));
                return Ok(out);
            }
        }
    }
    match cli.try_query("SELECT SYSTEM$GET_APPLICATION_SERVICE_DEFAULTS()")? {
        Ok(c) => {
            let cell = c.rows.first().and_then(|r| r.first().cloned().flatten());
            if let Some(v) = cell.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
                for k in ["database", "schema", "query_warehouse", "build_eai"] {
                    if let Some(s) = v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                        out.insert(k.into(), s.to_string());
                    }
                }
            }
        }
        Err(_) => {
            crate::logs::log(
                crate::logs::WARNING,
                "snowflake.cli._plugins.apps.manager",
                "Could not fetch Snowflake App Runtime defaults – skipping.",
            );
        }
    }
    Ok(out)
}

/// `_report_account_capabilities`: which App Runtime capabilities the account has.
fn setup_capabilities(cli: &mut Cli) -> Result<Output> {
    let mut used = vec!["--capabilities"];
    for (n, set) in [
        ("--app-name", cli.str("app_name").is_some()),
        ("--dry-run", cli.flag("dry_run")),
        ("--compute-pool", cli.str("compute_pool").is_some()),
        ("--build-eai", cli.str("build_eai").is_some()),
    ] {
        if set {
            used.push(n);
        }
    }
    if used.len() > 1 {
        return Err(SnowError::incompatible(&used));
    }
    let values: indexmap::IndexMap<String, serde_json::Value> = cli
        .try_query("call system$bootstrap_data_request('CLIENT_PARAMS_INFO')")
        .ok()
        .and_then(|r| r.ok())
        .and_then(|c| c.rows.first().and_then(|r| r.first().cloned().flatten()))
        .and_then(|cell| serde_json::from_str::<serde_json::Value>(&cell).ok())
        .and_then(|v| v["clientParamsInfo"].as_array().cloned())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| Some((r["name"].as_str()?.to_string(), r["value"].clone())))
                .collect()
        })
        .unwrap_or_default();
    let on = |v: Option<&serde_json::Value>| {
        let text = match v {
            None | Some(serde_json::Value::Null) => return false,
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        };
        ["true", "enabled", "enabled_public_preview"].contains(&text.trim().to_lowercase().as_str())
    };
    let caps = [
        (
            "serverless",
            "FEATURE_APPLICATION_SERVICE_COMPUTE_RESOURCE_SERVERLESS",
        ),
        (
            "python",
            "ENABLE_APP_SERVICE_PYTHON_RUNTIME_MANAGED_COMPUTE_POOL",
        ),
        (
            "python-serverless",
            "ENABLE_APP_SERVICE_PYTHON_RUNTIME_SERVERLESS",
        ),
        (
            "streamlit-serverless",
            "ENABLE_APP_SERVICE_STREAMLIT_SERVERLESS",
        ),
    ];
    Ok(Output::object(
        caps.iter()
            .map(|(n, p)| (*n, Pv::Bool(on(values.get(*p)))))
            .collect(),
    ))
}
