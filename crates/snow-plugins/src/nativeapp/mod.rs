//! `snow app`: Native Apps and Snowflake App Runtime.

pub mod application;
pub mod commands;
pub mod compiler;
pub mod facade;
pub mod package;
pub mod snowpark_processor;

use snow_click::Spec;

/// `SmartAppGroup.list_commands`: hide the other app family's commands.
pub fn visible_app_commands(spec: &Spec, cmd: u16) -> Option<Vec<u16>> {
    let _ = (spec, cmd);
    None
}

use snow_api::identifiers::{to_identifier, unquote_identifier};
use snow_api::output::{Output, Pv, Row};
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};

/// The package (and app) entities chosen by `@force_project_definition_v2`.
pub struct AppEntities {
    pub package_id: String,
    pub package: PyV,
    pub app_id: Option<String>,
    pub app: Option<PyV>,
    /// The (possibly converted in-memory) v2 project; `ctx` is the template context.
    pub project: snow_api::project::Project,
}

fn find(
    ents: &indexmap::IndexMap<String, PyV>,
    ty: &str,
    id: &str,
    option: &str,
    required: bool,
) -> Result<Option<(String, PyV)>> {
    let of_type: Vec<(&String, &PyV)> = ents
        .iter()
        .filter(|(_, e)| e.str_of("type").as_deref() == Some(ty))
        .collect();
    let found = if !id.is_empty() {
        of_type
            .iter()
            .find(|(k, _)| *k == id)
            .map(|(k, v)| ((*k).clone(), (*v).clone()))
    } else if of_type.len() == 1 {
        Some((of_type[0].0.clone(), of_type[0].1.clone()))
    } else if of_type.len() > 1 && required {
        return Err(SnowError::click(format!(
            "More than one {ty} entity exists in the project definition file, specify {option} to choose which one to operate on."
        )));
    } else {
        None
    };
    if found.is_none() && required {
        let with_id = if id.is_empty() {
            String::new()
        } else {
            format!("with ID \"{id}\" ")
        };
        return Err(SnowError::click(format!(
            "Could not find an {ty} entity {with_id}in the project definition file."
        )));
    }
    Ok(found)
}

/// `force_project_definition_v2(single_app_and_package=True)` for v2 projects.
pub fn entities(cli: &mut Cli, app_required: bool) -> Result<AppEntities> {
    let pkg_opt = cli.str("package_entity_id").unwrap_or_default();
    let app_opt = cli.str("app_entity_id").unwrap_or_default();
    let is_v1 = !cli.project(false)?.expect("project").meets_version("2");
    let project = cli.project_v2_in_memory()?;
    let ents = project.entities();
    if is_v1 {
        // The converted entities are chosen directly (every non-package entity
        // lands in app_entity_id, like the kwargs loop upstream).
        let mut package_id = String::new();
        let mut app_id = None;
        for (id, e) in &ents {
            if e.str_of("type").as_deref() == Some("application package") {
                package_id = id.clone();
            } else {
                app_id = Some(id.clone());
            }
        }
        let package = ents.get(&package_id).cloned().unwrap_or(PyV::None);
        let app = app_id.as_ref().and_then(|a| ents.get(a).cloned());
        return Ok(AppEntities {
            package_id,
            package,
            app_id,
            app,
            project,
        });
    }
    let app = find(
        &ents,
        "application",
        &app_opt,
        "--app-entity-id",
        app_required,
    )?;
    let mut pkg_id = pkg_opt.clone();
    if let Some((aid, a)) = &app {
        let target = a
            .get("from_")
            .and_then(|f| f.str_of("target"))
            .unwrap_or_default();
        if !pkg_opt.is_empty() {
            if target != pkg_opt && app_required {
                return Err(SnowError::click(format!(
                    "The application entity {aid} does not target the application package entity {pkg_opt}. Eitheruse --package-entity-id {target} to target the correct package entity, or omit the --package-entity-id flag to automatically use the package entity that the application entity targets."
                )));
            }
        } else if ents
            .get(&target)
            .is_some_and(|e| e.str_of("type").as_deref() == Some("application package"))
        {
            pkg_id = target;
        }
    }
    let (package_id, package) = find(
        &ents,
        "application package",
        &pkg_id,
        "--package-entity-id",
        true,
    )?
    .expect("required");
    Ok(AppEntities {
        package_id,
        package,
        app_id: app.as_ref().map(|a| a.0.clone()),
        app: app.map(|a| a.1),
        project,
    })
}

/// `ApplicationPackageEntity.role`: `meta.role`, else the connection's role.
pub fn role_of(cli: &mut Cli, entity: &PyV) -> Result<Option<String>> {
    if let Some(r) = entity
        .get_some("meta")
        .and_then(|m| m.str_of("role"))
        .filter(|r| !r.is_empty())
    {
        return Ok(Some(r));
    }
    if let Some(r) = cli.connection()?.session.role.clone() {
        return Ok(Some(r));
    }
    cli.current_role()
}

/// `SqlExecutor.use_role`: switch for the duration of `f` when needed.
pub fn with_role<T>(
    cli: &mut Cli,
    role: Option<&str>,
    f: impl FnOnce(&mut Cli) -> Result<T>,
) -> Result<T> {
    let Some(role) = role else { return f(cli) };
    let new = to_identifier(role);
    let prev = cli.current_role()?;
    let different = prev
        .as_ref()
        .is_none_or(|p| p.to_lowercase() != new.to_lowercase());
    if different {
        cli.execute_query(&format!("use role {new}"))?;
    }
    let r = f(cli);
    if different && let Some(p) = prev {
        cli.execute_query(&format!("use role {p}"))?;
    }
    r
}

fn package_name(e: &AppEntities) -> Result<String> {
    Ok(crate::entities::fqn(&e.package, &e.package_id)?.name)
}

fn facade_error(cli: &Cli, e: &snow_connector::DbError, context: &str, package: &str) -> SnowError {
    if e.kind == snow_connector::ErrorKind::Programming && e.errno == 2003 {
        return facade::user_input(format!(
            "Application package {package} does not exist or you are not authorized to access it."
        ));
    }
    facade::unclassified(cli, e, context)
}

fn query_rows(cli: &mut Cli, sql: &str) -> std::result::Result<Vec<Row>, snow_connector::DbError> {
    let c = match cli.try_query(sql) {
        Ok(r) => r?,
        Err(e) => {
            return Err(snow_connector::DbError::new(
                snow_connector::ErrorKind::Database,
                -1,
                None,
                e.message(),
            ));
        }
    };
    Ok(cli.rows(&c))
}

/// `snow app version list`.
pub fn version_list(cli: &mut Cli) -> Result<Output> {
    let e = entities(cli, false)?;
    let name = package_name(&e)?;
    let role = role_of(cli, &e.package)?;
    let ident = to_identifier(&name);
    let rows = facade::use_role(cli, role.as_deref(), |cli| {
        query_rows(
            cli,
            &format!("show versions in application package {ident}"),
        )
        .map_err(|err| {
            facade_error(
                cli,
                &err,
                &format!("Failed to show versions for application package {ident}."),
                &ident,
            )
        })
    })?;
    Ok(Output::Collection(rows))
}

/// `snow app release-directive list`.
pub fn release_directive_list(cli: &mut Cli) -> Result<Output> {
    let e = entities(cli, false)?;
    let name = package_name(&e)?;
    let role = role_of(cli, &e.package)?;
    let like = cli.str("like").unwrap_or_else(|| "%%".into());
    let channel = cli.str("channel");
    let channel = match channel {
        Some(c) => sanitized_channel(cli, &name, role.as_deref(), &c)?,
        None => None,
    };
    let mut q = format!(
        "show release directives in application package {}",
        to_identifier(&name)
    );
    if let Some(c) = &channel {
        q.push_str(&format!(" for release channel {}", to_identifier(c)));
    }
    let rows = facade::use_role(cli, role.as_deref(), |cli| match query_rows(cli, &q) {
        Ok(r) => Ok(r),
        Err(err) if err.errno == 3001 => Err(SnowError::click(format!(
            "Insufficient privileges to show release directives for application package {name}"
        ))
        .named("InsufficientPrivilegesError")),
        Err(err) => Err(facade_error(
            cli,
            &err,
            &format!("Failed to show release directives for application package {name}."),
            &name,
        )),
    })?;
    let rows = rows
        .into_iter()
        .filter(|r| {
            snow_api::identifiers::sql_match(
                &like,
                &r.get("name").map(Pv::py_str).unwrap_or_default(),
            )
        })
        .collect();
    Ok(Output::Collection(rows))
}

/// `show_release_channels` (empty when the account has release channels off).
pub fn release_channels(cli: &mut Cli, name: &str, role: Option<&str>) -> Result<Vec<Row>> {
    if let Ok(Some(v)) = crate::snowsight::ui_parameter(cli, "FEATURE_RELEASE_CHANNELS")
        && (v == serde_json::json!(false)
            || v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("false")))
    {
        return Ok(vec![]);
    }
    let ident = to_identifier(name);
    facade::use_role(cli, role, |cli| {
        match query_rows(
            cli,
            &format!("show release channels in application package {ident}"),
        ) {
            Ok(r) => Ok(r),
            Err(err) if err.errno == 1003 => Ok(vec![]),
            Err(err) => Err(facade_error(
                cli,
                &err,
                &format!("Failed to show release channels for application package {name}."),
                name,
            )),
        }
    })
}

fn sanitized_channel(
    cli: &mut Cli,
    name: &str,
    role: Option<&str>,
    channel: &str,
) -> Result<Option<String>> {
    let available = release_channels(cli, name, role)?;
    if available.is_empty() && snow_api::identifiers::same_identifiers(channel, "DEFAULT") {
        return Ok(None);
    }
    let names: Vec<String> = available
        .iter()
        .map(|r| r.get("name").map(Pv::py_str).unwrap_or_default())
        .collect();
    if available.is_empty() {
        return Err(SnowError::usage(format!(
            "Release channels are not enabled for application package {name}."
        )));
    }
    if !names
        .iter()
        .any(|n| snow_api::identifiers::same_identifiers(n, channel))
    {
        return Err(SnowError::usage(format!(
            "Release channel {channel} is not available in application package {name}. Available release channels are: ({}).",
            names.join(", ")
        )));
    }
    Ok(Some(channel.to_string()))
}

/// `snow app release-channel list`.
pub fn release_channel_list(cli: &mut Cli) -> Result<Output> {
    let e = entities(cli, false)?;
    let name = package_name(&e)?;
    let role = role_of(cli, &e.package)?;
    let filter = cli.str("channel");
    let rows: Vec<Row> = release_channels(cli, &name, role.as_deref())?
        .into_iter()
        .filter(|r| {
            filter.as_ref().is_none_or(|c| {
                unquote_identifier(c) == r.get("name").map(Pv::py_str).unwrap_or_default()
            })
        })
        .collect();
    let mut out = Vec::new();
    if rows.is_empty() {
        cli.console.message("No release channels found.");
    }
    for r in rows {
        let g = |k: &str| r.get(k).cloned().unwrap_or(Pv::None);
        let targets: serde_json::Value = match g("targets") {
            Pv::None => serde_json::json!({}),
            v => serde_json::from_str(&v.py_str()).unwrap_or(serde_json::json!({})),
        };
        let versions: Vec<String> = match g("versions") {
            Pv::None => vec![],
            v => serde_json::from_str::<Vec<String>>(&v.py_str()).unwrap_or_default(),
        };
        let accounts = targets["accounts"].as_array().map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        });
        let ts = |v: Pv| match v {
            Pv::None => String::new(),
            other => other.py_str(),
        };
        cli.console
            .message(&format!("[bold]{}[/bold]", g("name").py_str()));
        cli.console.indent();
        cli.console
            .message(&format!("Description: {}", g("description").py_str()));
        cli.console
            .message(&format!("Versions: ({})", versions.join(", ")));
        cli.console
            .message(&format!("Created on: {}", ts(g("created_on"))));
        cli.console
            .message(&format!("Updated on: {}", ts(g("updated_on"))));
        cli.console.message(&format!(
            "Target accounts: {}",
            accounts
                .map(|a| format!("({})", a.join(", ")))
                .unwrap_or_else(|| "ALL ACCOUNTS".into())
        ));
        cli.console.dedent();
        let mut row = Row::new();
        row.insert("name".into(), g("name"));
        row.insert("description".into(), g("description"));
        row.insert("created_on".into(), g("created_on"));
        row.insert("updated_on".into(), g("updated_on"));
        row.insert("targets".into(), Pv::from_json(&targets));
        row.insert(
            "versions".into(),
            Pv::List(versions.into_iter().map(Pv::s).collect()),
        );
        out.push(row);
    }
    if cli.format.is_json() {
        return Ok(Output::Collection(out));
    }
    Ok(Output::Empty)
}

fn validate_accounts(accounts: &[String]) -> Result<()> {
    let v = snow_api::identifiers::valid_identifier_regex();
    let re = regex::Regex::new(&format!("^(?:{v})\\.(?:{v})$")).expect("re");
    for a in accounts {
        if !re.is_match(a) {
            return Err(SnowError::click(format!(
                "Target account {a} is not in a valid format. Make sure you provide the target account in the format 'org.account'."
            )));
        }
    }
    Ok(())
}

fn validate_channel(cli: &mut Cli, name: &str, role: Option<&str>, channel: &str) -> Result<()> {
    let available = release_channels(cli, name, role)?;
    if available.is_empty() {
        return Err(SnowError::usage(format!(
            "Release channels are not enabled for application package {name}."
        )));
    }
    let names: Vec<String> = available
        .iter()
        .map(|r| r.get("name").map(Pv::py_str).unwrap_or_default())
        .collect();
    if names.contains(&unquote_identifier(channel)) {
        return Ok(());
    }
    Err(SnowError::usage(format!(
        "Release channel {channel} is not available in application package {name}. Available release channels are: ({}).",
        names.join(", ")
    )))
}

fn user_input(m: String) -> SnowError {
    SnowError::click(m).named("UserInputError")
}

/// Run a facade statement under the package role, mapping errnos to `UserInputError`s.
fn facade_exec(
    cli: &mut Cli,
    role: Option<&str>,
    sql: &str,
    mapping: &[(i64, String)],
    context: &str,
) -> Result<()> {
    facade::use_role(cli, role, |cli| match cli.try_query(sql)? {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = e.msg_with(cli.connector_info);
            for (errno, m) in mapping {
                if e.errno == *errno {
                    return Err(user_input(m.replace("{msg}", &msg)));
                }
            }
            Err(facade::unclassified(cli, &e, context))
        }
    })
}

fn accounts_arg(cli: &Cli) -> Vec<String> {
    cli.str("target_accounts")
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_default()
}

const ACCOUNT_ERRS: [i64; 2] = [1999, 906];

/// `snow app release-channel add-accounts|remove-accounts|set-accounts`.
pub fn release_channel_accounts(cli: &mut Cli, verb: &str) -> Result<Output> {
    let e = entities(cli, false)?;
    let name = package_name(&e)?;
    let role = role_of(cli, &e.package)?;
    let channel = cli.str("channel").unwrap_or_default();
    let accounts = accounts_arg(cli);
    if accounts.is_empty()
        || accounts.iter().all(String::is_empty) && cli.str("target_accounts").is_none()
    {
        return Err(SnowError::click("No target accounts provided."));
    }
    validate_channel(cli, &name, role.as_deref(), &channel)?;
    validate_accounts(&accounts)?;
    let (pkg, ch) = (to_identifier(&name), to_identifier(&channel));
    let (kw, ctx, done) = match verb {
        "add" => (
            "add",
            format!("Failed to add accounts to release channel {ch} in application package {pkg}."),
            "Successfully added accounts to the release channel.",
        ),
        "remove" => (
            "remove",
            format!(
                "Failed to remove accounts from release channel {ch} in application package {pkg}."
            ),
            "Successfully removed accounts from the release channel.",
        ),
        _ => (
            "set",
            format!(
                "Failed to set accounts for release channel {ch} in application package {pkg}."
            ),
            "Successfully set accounts for the release channel.",
        ),
    };
    let mut mapping: Vec<(i64, String)> = ACCOUNT_ERRS
        .iter()
        .map(|e| (*e, "Invalid account passed in.\n{msg}".to_string()))
        .collect();
    mapping.push((
        512017,
        format!("Cannot modify accounts for release channel {ch} in application package {pkg}."),
    ));
    facade_exec(
        cli,
        role.as_deref(),
        &format!(
            "alter application package {pkg} modify release channel {ch} {kw} accounts = ({})",
            accounts.join(",")
        ),
        &mapping,
        &ctx,
    )?;
    Ok(Output::message(done))
}

/// `snow app release-channel add-version|remove-version`.
pub fn release_channel_version(cli: &mut Cli, add: bool) -> Result<Output> {
    let e = entities(cli, false)?;
    let name = package_name(&e)?;
    let role = role_of(cli, &e.package)?;
    let channel = cli.str("channel").unwrap_or_default();
    let version = cli.str("version").unwrap_or_default();
    validate_channel(cli, &name, role.as_deref(), &channel)?;
    let (pkg, ch, v) = (
        to_identifier(&name),
        to_identifier(&channel),
        to_identifier(&version),
    );
    if add {
        add_version_to_channel(cli, &pkg, &ch, &v, role.as_deref())?;
        return Ok(Output::message(format!(
            "Successfully added version {version} to the release channel."
        )));
    }
    remove_version_from_channel(cli, &pkg, &ch, &v, role.as_deref())?;
    Ok(Output::message(format!(
        "Successfully removed version {version} from the release channel."
    )))
}

fn channel_clause(cli: &mut Cli, name: &str, role: Option<&str>) -> Result<Option<String>> {
    match cli.str("channel") {
        Some(c) => sanitized_channel(cli, name, role, &c),
        None => Ok(None),
    }
}

/// `snow app release-directive set`.
pub fn release_directive_set(cli: &mut Cli) -> Result<Output> {
    let e = entities(cli, false)?;
    let name = package_name(&e)?;
    let role = role_of(cli, &e.package)?;
    let directive = cli.str("directive").unwrap_or_default();
    let accounts = cli
        .str("target_accounts")
        .map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>());
    if let Some(a) = accounts.as_ref().filter(|a| !a.is_empty()) {
        validate_accounts(a)?;
        if snow_api::identifiers::same_identifiers(&directive, "DEFAULT") {
            return Err(SnowError::Click(snow_click::ClickError::Usage {
                message: "Target accounts can only be specified for non-default named release directives.".into(),
                depth: None,
                no_such_option: false,
                attach: true,
                class: "BadOptionUsage",
            }));
        }
    }
    let channel = channel_clause(cli, &name, role.as_deref())?;
    let version = cli.str("version").unwrap_or_default();
    let patch = cli.int("patch").unwrap_or_default();
    set_release_directive(
        cli,
        &name,
        &directive,
        channel.as_deref(),
        accounts.as_deref(),
        &version,
        patch,
        role.as_deref(),
    )?;
    Ok(Output::message("Successfully set release directive."))
}

/// `snow app release-directive unset`.
pub fn release_directive_unset(cli: &mut Cli) -> Result<Output> {
    let e = entities(cli, false)?;
    let name = package_name(&e)?;
    let role = role_of(cli, &e.package)?;
    let directive = cli.str("directive").unwrap_or_default();
    if snow_api::identifiers::same_identifiers(&directive, "DEFAULT") {
        return Err(SnowError::click(
            "Cannot unset default release directive. Please specify a non-default release directive.",
        ));
    }
    let channel = channel_clause(cli, &name, role.as_deref())?;
    let (pkg, rd) = (to_identifier(&name), to_identifier(&directive));
    let ch = channel
        .map(|c| format!(" modify release channel {}", to_identifier(&c)))
        .unwrap_or_default();
    facade_exec(
        cli,
        role.as_deref(),
        &format!("alter application package {pkg}{ch} unset release directive {rd}"),
        &[(
            93090,
            format!("Release directive {rd} does not exist in application package {pkg}."),
        )],
        &format!("Failed to unset release directive {rd} for application package {pkg}."),
    )?;
    Ok(Output::message(format!(
        "Successfully unset release directive {directive}."
    )))
}

/// `snow app release-directive add-accounts|remove-accounts`.
pub fn release_directive_accounts(cli: &mut Cli, add: bool) -> Result<Output> {
    let e = entities(cli, false)?;
    let name = package_name(&e)?;
    let role = role_of(cli, &e.package)?;
    let directive = cli.str("directive").unwrap_or_default();
    let accounts = accounts_arg(cli);
    if accounts.is_empty() {
        return Err(SnowError::click("No target accounts provided."));
    }
    validate_accounts(&accounts)?;
    let channel = channel_clause(cli, &name, role.as_deref())?;
    let (pkg, rd) = (to_identifier(&name), to_identifier(&directive));
    let (kw, word) = if add {
        ("add", "adding")
    } else {
        ("remove", "removing")
    };
    if snow_api::identifiers::same_identifiers(&rd, "DEFAULT") {
        return Err(user_input(format!(
            "Default release directive does not support {word} accounts. Please specify a non-default release directive."
        )));
    }
    let mut lines = vec![format!("alter application package {pkg}")];
    if let Some(c) = &channel {
        lines.push(format!("    modify release channel {}", to_identifier(c)));
    }
    lines.push(format!("    modify release directive {rd}"));
    lines.push(format!("    {kw} accounts = ({})", accounts.join(",")));
    let sql = format!("{}\n", lines.join("\n"));
    let mut mapping: Vec<(i64, String)> = ACCOUNT_ERRS
        .iter()
        .map(|e| (*e, "Invalid account passed in.\n{msg}".to_string()))
        .collect();
    mapping.push((
        93090,
        format!("Release directive {rd} does not exist in application package {pkg}."),
    ));
    if add {
        mapping.push((93091, format!("Some target accounts are already referenced by other release directives in application package {pkg}.\n{{msg}}")));
    }
    let ctx = if add {
        format!("Failed to add accounts to release directive {rd} for application package {pkg}.")
    } else {
        format!(
            "Failed to remove accounts from release directive {rd} for application package {pkg}."
        )
    };
    facade_exec(cli, role.as_deref(), &sql, &mapping, &ctx)?;
    Ok(Output::message(if add {
        "Successfully added accounts to the release directive."
    } else {
        "Successfully removed accounts from the release directive."
    }))
}

/// `get_sanitized_release_channel(release_channel)` for a package.
pub fn sanitized_channel_for(
    cli: &mut Cli,
    pkg: &package::Package,
    channel: Option<&str>,
) -> Result<Option<String>> {
    let Some(c) = channel.filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    let name = pkg.name()?;
    let role = pkg.role(cli)?;
    sanitized_channel(cli, &name, role.as_deref(), c)
}

/// `add_version_to_release_channel(package, channel, version, role)`.
pub fn add_version_to_channel(
    cli: &mut Cli,
    package: &str,
    channel: &str,
    version: &str,
    role: Option<&str>,
) -> Result<()> {
    let (pkg, ch, v) = (
        to_identifier(package),
        to_identifier(channel),
        to_identifier(version),
    );
    facade_exec(
        cli,
        role,
        &format!("alter application package {pkg} modify release channel {ch} add version {v}"),
        &[
            (
                93031,
                format!("Version {v} does not exist in application package {pkg}."),
            ),
            (
                512005,
                format!("Version {v} is already added to release channel {ch}."),
            ),
            (
                512004,
                format!(
                    "Maximum number of versions allowed in release channel {ch} has been reached."
                ),
            ),
        ],
        &format!("Failed to add version {v} to release channel {ch} in application package {pkg}."),
    )
}

/// `remove_version_from_release_channel(package, channel, version, role)`.
pub fn remove_version_from_channel(
    cli: &mut Cli,
    package: &str,
    channel: &str,
    version: &str,
    role: Option<&str>,
) -> Result<()> {
    let (pkg, ch, v) = (
        to_identifier(package),
        to_identifier(channel),
        to_identifier(version),
    );
    facade_exec(
        cli,
        role,
        &format!("alter application package {pkg} modify release channel {ch} drop version {v}"),
        &[
            (
                512010,
                format!("Version {v} is not found in release channel {ch}."),
            ),
            (
                93088,
                format!(
                    "Cannot remove version {v} from release channel {ch} as it is referenced by a release directive."
                ),
            ),
        ],
        &format!(
            "Failed to remove version {v} from release channel {ch} in application package {pkg}."
        ),
    )
}

/// `set_release_directive(package, directive, channel, target_accounts, version, patch, role)`.
#[allow(clippy::too_many_arguments)]
pub fn set_release_directive(
    cli: &mut Cli,
    name: &str,
    directive: &str,
    channel: Option<&str>,
    accounts: Option<&[String]>,
    version: &str,
    patch: i64,
    role: Option<&str>,
) -> Result<()> {
    let (pkg, rd) = (to_identifier(name), to_identifier(directive));
    let version = to_identifier(version);
    let rd_stmt = if snow_api::identifiers::same_identifiers(&rd, "DEFAULT") {
        "set default release directive".to_string()
    } else if accounts.is_some_and(|a| !a.is_empty()) {
        format!("set release directive {rd}")
    } else {
        format!("modify release directive {rd}")
    };
    let mut lines = vec![format!("alter application package {pkg}")];
    if let Some(c) = &channel {
        lines.push(format!("    modify release channel {}", to_identifier(c)));
    }
    lines.push(format!("    {rd_stmt}"));
    if let Some(a) = accounts.filter(|a| !a.is_empty()) {
        lines.push(format!("    accounts = ({})", a.join(",")));
    }
    lines.push(format!("    version = {version} patch = {patch}"));
    let sql = format!("{}\n", lines.join("\n"));
    let ch = channel
        .as_ref()
        .map(|c| to_identifier(c))
        .unwrap_or_else(|| "None".into());
    let mut mapping: Vec<(i64, String)> = ACCOUNT_ERRS
        .iter()
        .map(|e| (*e, "Invalid account passed in.\n{msg}".to_string()))
        .collect();
    mapping.extend([
        (93090, format!("Release directive {rd} does not exist in application package {pkg}. Please create it first by specifying --target-accounts with the `snow app release-directive set` command.")),
        (93091, format!("Some target accounts are already referenced by other release directives in application package {pkg}.\n{{msg}}")),
        (512008, format!("Version {version} is not added to release channel {ch}. Please add it to the release channel first.")),
        (93036, format!("Patch {patch} for version {version} not found in application package {pkg}.")),
        (93074, format!("Version {version}, patch {patch} has not yet been approved to release to accounts outside of this organization.")),
        (93031, format!("Version {version} does not exist in application package {pkg}.")),
    ]);
    facade_exec(
        cli,
        role,
        &sql,
        &mapping,
        &format!("Failed to set release directive {rd} for application package {pkg}."),
    )
}

/// `validate_release_channel(release_channel, available_release_channels)`.
pub fn validate_channel_in(name: &str, channels: &[Row], channel: &str) -> Result<()> {
    if channels.is_empty() {
        return Err(SnowError::usage(format!(
            "Release channels are not enabled for application package {name}."
        )));
    }
    let names: Vec<String> = channels
        .iter()
        .map(|r| r.get("name").map(Pv::py_str).unwrap_or_default())
        .collect();
    if names.contains(&unquote_identifier(channel)) {
        return Ok(());
    }
    Err(SnowError::usage(format!(
        "Release channel {channel} is not available in application package {name}. Available release channels are: ({}).",
        names.join(", ")
    )))
}
