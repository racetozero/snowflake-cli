//! `ApplicationEntity` (`_plugins/nativeapp/entities/application.py`): create or
//! upgrade (run), drop (teardown), events and the Snowsight URL.

use super::facade::{self, InstallMethod};
use super::package::{Package, Policy, StageParts, drop_generic_object, exit, generic_sql_error};
use snow_api::identifiers::{to_identifier, to_string_literal, unquote_identifier};
use snow_api::output::{Pv, Row};
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};
use snow_click::ClickError;

pub struct App {
    pub id: String,
    pub model: PyV,
    pub project: snow_api::project::Project,
    pub project_root: std::path::PathBuf,
}

/// Python truthiness of a UI parameter value.
fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64() != Some(0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

/// `find_events_definitions_in_manifest_file`: (name, type, sharing).
fn manifest_events(deploy_root: &std::path::Path) -> Result<Vec<(String, String, String)>> {
    let m = super::package::read_manifest(deploy_root)?;
    let mut out = Vec::new();
    if let Some(PyV::Dict(c)) = m.get_some("configuration")
        && let Some(PyV::List(events)) = c.get("telemetry_event_definitions")
    {
        for e in events {
            if let PyV::Dict(d) = e {
                let ty = d.get("type").map(PyV::py_str).unwrap_or_default();
                let sharing = d.get("sharing").map(PyV::py_str).unwrap_or_default();
                out.push((format!("SNOWFLAKE${ty}"), ty, sharing));
            }
        }
    }
    Ok(out)
}

/// `EventSharingHandler`.
struct EventSharing {
    enabled: bool,
    enforced: bool,
    share_mandatory: Option<bool>,
    optional: Vec<String>,
}

impl EventSharing {
    fn new(
        cli: &mut Cli,
        telemetry: Option<&PyV>,
        deploy_root: &std::path::Path,
        install: &InstallMethod,
    ) -> Result<Self> {
        let enabled = truthy(&crate::snowsight::ui_parameter_or(
            cli,
            "ENABLE_EVENT_SHARING_V2_IN_THE_SAME_ACCOUNT",
            serde_json::json!(true),
        ));
        let enforced = truthy(&crate::snowsight::ui_parameter_or(
            cli,
            "ENFORCE_MANDATORY_FILTERS_FOR_SAME_ACCOUNT_INSTALLATION",
            serde_json::json!(true),
        ));
        let mut share_mandatory = telemetry.and_then(|t| match t.get("share_mandatory_events") {
            Some(PyV::Bool(b)) => Some(*b),
            _ => None,
        });
        let optional: Vec<String> = telemetry
            .and_then(|t| t.get("optional_shared_events"))
            .and_then(PyV::as_list)
            .map(|l| l.iter().map(PyV::py_str).collect())
            .unwrap_or_default();
        let mut s = EventSharing {
            enabled,
            enforced,
            share_mandatory,
            optional,
        };
        if !enabled {
            if share_mandatory == Some(true) || !s.optional.is_empty() {
                cli.console.warning(
                    "WARNING: Same-account event sharing is not enabled in your account, therefore, application telemetry section will be ignored.",
                );
            }
            s.share_mandatory = None;
            s.optional.clear();
            return Ok(s);
        }
        if install.is_dev_mode() {
            let mandatory = manifest_events(deploy_root)?
                .iter()
                .any(|e| e.2 == "MANDATORY");
            if mandatory && enforced && share_mandatory.is_none() {
                cli.console.warning(
                    "WARNING: Mandatory events are present in the manifest file. Automatically authorizing event sharing in dev mode. To suppress this warning, please add 'share_mandatory_events: true' in the application telemetry section.",
                );
                share_mandatory = Some(true);
                s.share_mandatory = share_mandatory;
            }
        }
        Ok(s)
    }

    fn should_authorize(&self) -> Option<bool> {
        if !self.enabled {
            return None;
        }
        self.share_mandatory
    }

    /// `events_to_share(events_definitions)`.
    fn events_to_share(&self, cli: &mut Cli, defs: &[Row]) -> Result<Option<Vec<String>>> {
        let get = |r: &Row, k: &str| r.get(k).map(Pv::py_str).unwrap_or_default();
        let mut names = Vec::new();
        for t in &self.optional {
            match defs.iter().find(|r| get(r, "type") == *t) {
                Some(r) => names.push(get(r, "name")),
                None => {
                    return Err(SnowError::click(format!(
                        "Event '{t}' from application 'telemetry' section in the project definition file could not be found in the application."
                    )));
                }
            }
        }
        for r in defs {
            if get(r, "sharing") == "MANDATORY" {
                names.push(get(r, "name"));
            }
        }
        if self.share_mandatory != Some(true) {
            if defs.iter().any(|r| get(r, "sharing") == "MANDATORY") && !self.enforced {
                cli.console.warning(
                    "WARNING: Mandatory events are present in the application, but event sharing is not authorized in the application telemetry field. This will soon be required to set in order to deploy this application.",
                );
            }
            return Ok(None);
        }
        names.sort();
        names.dedup();
        Ok(Some(names))
    }
}

impl App {
    pub fn new(cli: &Cli, e: &super::AppEntities) -> Option<App> {
        Some(App {
            id: e.app_id.clone()?,
            model: e.app.clone()?,
            project: e.project.clone(),
            project_root: cli.project_root(),
        })
    }
    pub fn name(&self) -> Result<String> {
        Ok(to_identifier(
            &crate::entities::fqn(&self.model, &self.id)?.name,
        ))
    }
    pub fn role(&self, cli: &mut Cli) -> Result<Option<String>> {
        super::role_of(cli, &self.model)
    }
    pub fn warehouse(&self, cli: &mut Cli) -> Result<Option<String>> {
        if let Some(w) = self
            .model
            .get_some("meta")
            .and_then(|m| m.str_of("warehouse"))
            .filter(|w| !w.is_empty())
        {
            return Ok(Some(to_identifier(&w)));
        }
        Ok(cli
            .connection()?
            .params
            .warehouse
            .clone()
            .filter(|w| !w.is_empty())
            .map(|w| to_identifier(&w)))
    }
    fn debug(&self) -> Option<bool> {
        match self.model.get("debug") {
            Some(PyV::Bool(b)) => Some(*b),
            _ => None,
        }
    }
    pub fn package_target(&self) -> String {
        self.model
            .get("from_")
            .and_then(|f| f.str_of("target"))
            .unwrap_or_default()
    }

    /// `action_deploy(...)` (`snow app run`).
    #[allow(clippy::too_many_arguments)]
    pub fn deploy(
        &self,
        cli: &mut Cli,
        pkg: &Package,
        from_release_directive: bool,
        release_channel: Option<String>,
        validate: bool,
        interactive: bool,
        version: Option<String>,
        patch: Option<i64>,
        force: bool,
    ) -> Result<()> {
        let stage = pkg.stage_path()?;
        let policy = Policy::from_flags(force, interactive);
        if from_release_directive {
            let channel = super::sanitized_channel_for(cli, pkg, release_channel.as_deref())?;
            return self.create_or_upgrade(
                cli,
                pkg,
                &stage,
                &InstallMethod::release_directive(),
                policy,
                interactive,
                channel.as_deref(),
            );
        }
        if release_channel.is_some() {
            return Err(SnowError::usage(
                "Release channel is only supported when --from-release-directive is used.",
            ));
        }
        if let Some(v) = version {
            let pname = pkg.name()?;
            match pkg.existing_version_info(cli, &v) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(SnowError::usage(format!(
                        "Application package {pname} does not have any version {v} defined. Use 'snow app version create' to define a version in the application package first."
                    )));
                }
                Err(SnowError::Cli {
                    name: "ApplicationPackageDoesNotExistError",
                    ..
                }) => {
                    return Err(SnowError::usage(format!(
                        "Application package {pname} does not exist. Use 'snow app version create' to first create an application package and then define a version in it."
                    )));
                }
                Err(e) => return Err(e),
            }
            return self.create_or_upgrade(
                cli,
                pkg,
                &stage,
                &InstallMethod::versioned_dev(&v, patch),
                policy,
                interactive,
                None,
            );
        }
        pkg.deploy(
            cli,
            None,
            true,
            true,
            &[],
            true,
            validate,
            &stage,
            interactive,
            force,
            true,
        )?;
        self.create_or_upgrade(
            cli,
            pkg,
            &stage,
            &InstallMethod::unversioned_dev(),
            policy,
            interactive,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_or_upgrade(
        &self,
        cli: &mut Cli,
        pkg: &Package,
        stage: &StageParts,
        install: &InstallMethod,
        policy: Policy,
        interactive: bool,
        channel: Option<&str>,
    ) -> Result<()> {
        let telemetry = self.model.get_some("telemetry").cloned();
        let sharing = EventSharing::new(cli, telemetry.as_ref(), &pkg.deploy_root(), install)?;
        let name = self.name()?;
        let role = self.role(cli)?;
        let warehouse = self.warehouse(cli)?;
        let existing = facade::get_existing_app_info(cli, &name, role.as_deref())?;
        let mut result = None;
        if existing.is_some() {
            cli.console
                .step(&format!("Upgrading existing application object {name}."));
            match facade::upgrade_application(
                cli,
                &name,
                install,
                &stage.full_path(),
                role.as_deref(),
                warehouse.as_deref(),
                self.debug(),
                sharing.should_authorize(),
                channel,
            ) {
                Ok(r) => result = Some(r),
                Err(
                    e @ SnowError::Cli {
                        name: "UpgradeApplicationRestrictionError",
                        ..
                    },
                ) => {
                    cli.console.warning(&e.message());
                    self.drop_before_upgrade(cli, policy, interactive, false)?;
                }
                Err(e) => return Err(e),
            }
        }
        let result = match result {
            Some(r) => r,
            None => self.create(cli, pkg, stage, install, &sharing, channel)?,
        };
        let messages: Vec<String> = result
            .iter()
            .map(|r| {
                r.first()
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| "None".into())
            })
            .collect();
        if !messages.is_empty() {
            for m in &messages {
                cli.console.warning(m);
            }
            cli.console.message("");
        }
        let defs = facade::get_event_definitions(cli, &name, role.as_deref())?;
        if let Some(events) = sharing.events_to_share(cli, &defs)? {
            crate::logs::info(
                "snowflake.cli._plugins.nativeapp.sf_sql_facade",
                &format!("sharing events {}", py_list(&events)),
            );
            facade::share_telemetry_events(cli, &name, &events, role.as_deref())?;
        }
        let hooks = self
            .model
            .get_some("meta")
            .and_then(|m| m.get("post_deploy"))
            .and_then(PyV::as_list)
            .cloned()
            .unwrap_or_default();
        if !hooks.is_empty() {
            super::package::execute_post_deploy_hooks(
                cli,
                &self.project_root,
                &self.project,
                &hooks,
                "application",
                role.as_deref(),
                warehouse.as_deref(),
                Some(&name),
            )?;
        }
        Ok(())
    }

    fn create(
        &self,
        cli: &mut Cli,
        pkg: &Package,
        stage: &StageParts,
        install: &InstallMethod,
        sharing: &EventSharing,
        channel: Option<&str>,
    ) -> Result<facade::Rows> {
        let name = self.name()?;
        cli.console.step(&format!(
            "Creating new application object {name} in account."
        ));
        let role = self.role(cli)?;
        let prole = pkg.role(cli)?;
        let pname = pkg.name()?;
        if prole != role {
            let grantee = role.clone().unwrap_or_default();
            grant(
                cli,
                &["install", "develop"],
                "application package",
                &pname,
                &grantee,
                prole.as_deref(),
            )?;
            let schema = stage.schema.clone().unwrap_or_else(|| "None".into());
            grant(
                cli,
                &["usage"],
                "schema",
                &format!("{pname}.{schema}"),
                &grantee,
                prole.as_deref(),
            )?;
            grant(
                cli,
                &["read"],
                "stage",
                &stage.stage,
                &grantee,
                prole.as_deref(),
            )?;
        }
        let warehouse = self.warehouse(cli)?;
        let (rows, warnings) = facade::create_application(
            cli,
            &name,
            &pname,
            install,
            &stage.full_path(),
            role.as_deref(),
            warehouse.as_deref(),
            self.debug(),
            sharing.should_authorize(),
            channel,
        )?;
        for w in warnings {
            cli.console.warning(&w);
        }
        Ok(rows)
    }

    /// `get_objects_owned_by_application()`: (name, type) pairs.
    fn owned_objects(
        &self,
        cli: &mut Cli,
    ) -> Result<std::result::Result<Vec<(String, String)>, snow_connector::DbError>> {
        let name = self.name()?;
        let role = self.role(cli)?;
        super::with_role(cli, role.as_deref(), |cli| {
            Ok(cli
                .try_query(&format!("show objects owned by application {name}"))?
                .map(|c| {
                    c.rows
                        .iter()
                        .map(|r| {
                            (
                                r.get(1).cloned().flatten().unwrap_or_else(|| "None".into()),
                                r.get(2).cloned().flatten().unwrap_or_else(|| "None".into()),
                            )
                        })
                        .collect()
                }))
        })
    }

    /// `drop_application_before_upgrade(policy, interactive, cascade)`.
    fn drop_before_upgrade(
        &self,
        cli: &mut Cli,
        policy: Policy,
        interactive: bool,
        cascade: bool,
    ) -> Result<()> {
        let name = self.name()?;
        let role = self.role(cli)?;
        let again = super::with_role(cli, role.as_deref(), |cli| {
            let prompt = if cascade {
                match self.owned_objects(cli)? {
                    Ok(objs) if !objs.is_empty() => {
                        let list: Vec<String> =
                            objs.iter().map(|(n, t)| format!("({t}) {n}")).collect();
                        cli.console.message(&format!(
                            "The following objects are owned by application {name} and need to be dropped:\n{}",
                            list.join("\n")
                        ));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        if e.errno != 93079 {
                            return Err(generic_sql_error(cli, e));
                        }
                        cli.console.warning(
                            "The application owns other objects but they could not be determined.",
                        );
                    }
                }
                "Do you want the Snowflake CLI to drop these objects, then drop the existing application object and recreate it?"
            } else {
                "Do you want the Snowflake CLI to drop the existing application object and recreate it?"
            };
            if !policy.should_proceed(cli, prompt)? {
                if interactive {
                    cli.console.message("Not upgrading the application object.");
                    return Err(exit(0));
                }
                cli.console.message(
                    "Cannot upgrade the application object non-interactively without --force.",
                );
                return Err(exit(1));
            }
            let cascade_msg = if cascade { " (cascade)" } else { "" };
            cli.console
                .step(&format!("Dropping application object {name}{cascade_msg}."));
            let sql = format!(
                "drop application {name}{}",
                if cascade { " cascade" } else { "" }
            );
            match cli.try_query(&sql)? {
                Ok(_) => Ok(false),
                Err(e)
                    if e.errno == facade::errno::APPLICATION_OWNS_EXTERNAL_OBJECTS && !cascade =>
                {
                    Ok(true)
                }
                Err(e) => Err(generic_sql_error(cli, e)),
            }
        })?;
        if again {
            return self.drop_before_upgrade(cli, policy, interactive, true);
        }
        Ok(())
    }

    /// `action_drop(interactive, force_drop, cascade)`.
    pub fn drop(
        &self,
        cli: &mut Cli,
        interactive: bool,
        force: bool,
        cascade: Option<bool>,
    ) -> Result<()> {
        let name = self.name()?;
        let role = self.role(cli)?;
        let Some(row) = facade::get_existing_app_info(cli, &name, role.as_deref())? else {
            cli.console.warning(&format!(
                "Role {} does not own any application object with the name {name}, or the application object does not exist.",
                role.clone().unwrap_or_else(|| "None".into())
            ));
            return Ok(());
        };
        let g = |k: &str| row.get(k).map(Pv::py_str).unwrap_or_default();
        if !facade::ALLOWED_SPECIAL_COMMENTS.contains(&g("comment").as_str()) && !force {
            let prompt = format!(
                "Application object {name} was not created by Snowflake CLI.\nApplication object details:\nName: {name}\nCreated on: {}\nSource: {}\nOwner: {}\nComment: {}\nVersion: {}\nPatch: {}\nAre you sure you want to drop it?\n",
                g("created_on"),
                g("source"),
                g("owner"),
                g("comment"),
                g("version"),
                g("patch")
            );
            if !crate::common::confirm(cli, &prompt, false)? {
                cli.console
                    .message(&format!("Did not drop application object {name}."));
                return Err(SnowError::Click(ClickError::Abort));
            }
        }
        let mut cascade = cascade;
        let (has, objects, prefix, yes_msg, no_msg, prompt, abort_msg) = match self
            .owned_objects(cli)?
        {
            Ok(objs) if !objs.is_empty() => {
                let prefix = format!("The following objects are owned by application {name}");
                (
                    true,
                    objs,
                    prefix.clone(),
                    format!("{prefix} and will be dropped:"),
                    format!("{prefix} and will NOT be dropped:"),
                    "Would you like to drop these objects in addition to the application? [y/n/ABORT]".to_string(),
                    "Re-run teardown again with --cascade or --no-cascade to specify whether these objects should be dropped along with the application".to_string(),
                )
            }
            Ok(_) => (
                false,
                vec![],
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ),
            Err(e) if e.errno == 93079 => {
                let prefix =
                    format!("Could not determine which objects are owned by application {name}");
                (
                    true,
                    vec![],
                    prefix.clone(),
                    format!("{prefix}, an unknown number of objects will be dropped."),
                    format!("{prefix}, they will NOT be dropped."),
                    "Would you like to drop an unknown set of objects in addition to the application? [y/n/ABORT]".to_string(),
                    "Re-run teardown again with --cascade or --no-cascade to specify whether any objects should be dropped along with the application.".to_string(),
                )
            }
            Err(e) => return Err(snow_api::connect::db_error_to_cli(&e, cli.connector_info)),
        };
        let list = |cli: &mut Cli| {
            cli.console.indent();
            for (n, t) in &objects {
                cli.console.message(&format!("({t}) {n}"));
            }
            cli.console.dedent();
        };
        if has {
            match cascade {
                Some(true) => {
                    cli.console.message(&yes_msg);
                    list(cli);
                }
                Some(false) => {
                    cli.console.message(&no_msg);
                    list(cli);
                }
                None if interactive => {
                    cli.console.message(&prefix);
                    list(cli);
                    let answer = crate::common::prompt(cli, &prompt, false)?
                        .unwrap_or_else(|| "ABORT".into())
                        .to_lowercase();
                    cascade = match answer.as_str() {
                        "y" | "yes" => Some(true),
                        "n" | "no" => Some(false),
                        _ => return Err(SnowError::Click(ClickError::Abort)),
                    };
                }
                None => {
                    cli.console.message(&prefix);
                    list(cli);
                    cli.console.message(&abort_msg);
                    return Err(SnowError::Click(ClickError::Abort));
                }
            }
        }
        drop_generic_object(
            cli,
            "application",
            &name,
            role.as_deref(),
            cascade.unwrap_or(false),
        )
    }

    /// `get_snowsight_url()` (under `use_application_warehouse`).
    pub fn snowsight_url(&self, cli: &mut Cli) -> Result<String> {
        let name = snow_api::identifiers::identifier_for_url(&self.name()?);
        let Some(wh) = self.warehouse(cli)? else {
            return Err(SnowError::click(
                "Application warehouse cannot be empty.\nPlease provide a value for it in your connection information or your project definition file.\n",
            ));
        };
        let new = to_identifier(&wh);
        let c = cli.execute_query("select current_warehouse()")?;
        let prev = c
            .rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .map(|w| to_identifier(&w));
        let different = prev.as_deref() != Some(new.as_str());
        if different {
            cli.execute_query(&format!("use warehouse {new}"))?;
        }
        let url = crate::snowsight::make_url(cli, &format!("/#/apps/application/{name}"));
        if different && let Some(p) = prev {
            cli.execute_query(&format!("use warehouse {p}"))?;
        }
        url
    }

    /// `get_events(...)`: rows with TIMESTAMP and VALUE.
    #[allow(clippy::too_many_arguments)]
    pub fn events(
        &self,
        cli: &mut Cli,
        package: &str,
        since: &str,
        until: &str,
        types: &[String],
        scopes: &[String],
        org: &str,
        account: &str,
        app_hash: &str,
        first: i64,
        last: i64,
    ) -> Result<Vec<Row>> {
        if first >= 0 && last >= 0 {
            return Err(
                SnowError::unexpected("first and last cannot be used together").named("ValueError"),
            );
        }
        let table = account_event_table(cli)?;
        let Some(table) = table else {
            return Err(SnowError::click(
                "No event table was found for this Snowflake account.\n\nAsk your Snowflake administrator to set up an event table for your account by following the docs at\nhttps://docs.snowflake.com/en/developer-guide/logging-tracing/event-table-setting-up.\n\nIf your account is configured to send events to an organization event account, create a new\nconnection to this account using `snow connection add` and re-run this command using the new connection.\nMore information on event accounts is available at https://docs.snowflake.com/en/developer-guide/native-apps/setting-up-logging-and-events#configure-an-account-to-store-shared-events.",
            )
            .named("NoEventTableForAccount"));
        };
        let app_name = unquote_identifier(&self.name()?);
        let package = unquote_identifier(package);
        let app_clause = if !org.is_empty() && !account.is_empty() {
            let mut c = format!(
                "resource_attributes:\"snow.application.package.name\" = {} and resource_attributes:\"snow.application.consumer.organization\" = {} and resource_attributes:\"snow.application.consumer.name\" = {}",
                to_string_literal(&package),
                to_string_literal(&unquote_identifier(org)),
                to_string_literal(&unquote_identifier(account))
            );
            if !app_hash.is_empty() {
                c.push_str(&format!(
                    " and resource_attributes:\"snow.database.hash\" = {}",
                    to_string_literal(&app_hash.to_lowercase())
                ));
            }
            c
        } else {
            format!(
                "resource_attributes:\"snow.database.name\" = {}",
                to_string_literal(&app_name)
            )
        };
        let since_c = if since.is_empty() {
            String::new()
        } else {
            format!(
                "and timestamp >= sysdate() - interval {}",
                to_string_literal(since)
            )
        };
        let until_c = if until.is_empty() {
            String::new()
        } else {
            format!(
                "and timestamp <= sysdate() - interval {}",
                to_string_literal(until)
            )
        };
        let list = |v: &[String]| {
            v.iter()
                .map(|x| to_string_literal(x))
                .collect::<Vec<_>>()
                .join(",")
        };
        let types_c = if types.is_empty() {
            String::new()
        } else {
            format!("and record_type in ({})", list(types))
        };
        let scopes_c = if scopes.is_empty() {
            String::new()
        } else {
            format!("and scope:name in ({})", list(scopes))
        };
        let first_c = if first >= 0 {
            format!("limit {first}")
        } else {
            String::new()
        };
        let last_c = if last >= 0 {
            format!("limit {last}")
        } else {
            String::new()
        };
        let q = format!(
            "select * from (\n    select timestamp, value::varchar value\n    from {table}\n    where ({app_clause})\n    {since_c}\n    {until_c}\n    {types_c}\n    {scopes_c}\n    order by timestamp desc\n    {last_c}\n) order by timestamp asc\n{first_c}\n"
        );
        // `dedent` blanks whitespace-only lines (the empty clauses).
        let q: String = q
            .split('\n')
            .map(|l| if l.trim().is_empty() { "" } else { l })
            .collect::<Vec<_>>()
            .join("\n");
        match cli.try_query(&q)? {
            Ok(c) => Ok(cli.rows(&c)),
            Err(e)
                if e.kind == snow_connector::ErrorKind::Programming
                    && e.errno == facade::errno::DOES_NOT_EXIST_OR_NOT_AUTHORIZED =>
            {
                Err(SnowError::click(format!(
                    "Event table '{table}' does not exist or you are not authorized to perform this operation.\nPlease check your EVENT_TABLE parameter to ensure that it is set to a valid event table."
                )))
            }
            Err(e) => Err(generic_sql_error(cli, e)),
        }
    }
}

fn py_list(v: &[String]) -> String {
    format!(
        "[{}]",
        v.iter()
            .map(|s| snow_click::py_repr(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// `grant_privileges_to_role(privileges, object_type, identifier, role_to_grant, role_to_use)`.
fn grant(
    cli: &mut Cli,
    privileges: &[&str],
    object_type: &str,
    ident: &str,
    grantee: &str,
    role_to_use: Option<&str>,
) -> Result<()> {
    let privs = privileges.join(", ");
    let grantee = to_identifier(grantee);
    let what = format!("{object_type} {ident}");
    facade::use_role(cli, role_to_use, |cli| {
        match cli.try_query(&format!("grant {privs} on {what} to role {grantee}"))? {
            Ok(_) => Ok(()),
            Err(e) => Err(facade::unclassified(
                cli,
                &e,
                &format!("Failed to grant {privs} on {what} to role {grantee}."),
            )),
        }
    })
}

/// `get_account_event_table(role)`.
fn account_event_table(cli: &mut Cli) -> Result<Option<String>> {
    let rows = match cli.try_query("show parameters like 'event_table' in account")? {
        Ok(c) => cli.rows(&c),
        Err(e) => return Err(facade::unclassified(cli, &e, "Failed to get event table.")),
    };
    let t = rows
        .iter()
        .find(|r| r.get("key").map(Pv::py_str).as_deref() == Some("EVENT_TABLE"))
        .map(|r| r.get("value").map(Pv::py_str).unwrap_or_default());
    Ok(t.filter(|t| t != "NONE"))
}
