//! `ApplicationPackageEntity` (`_plugins/nativeapp/entities/application_package.py`):
//! bundle, deploy, validate, and version create/drop.

use super::facade;
use crate::artifacts::BundleMap;
use crate::entities::EntityType;
use snow_api::identifiers::{
    Fqn, identifier_to_show_like_pattern, to_identifier, to_string_literal, unquote_identifier,
};
use snow_api::output::{Pv, Row};
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};
use snow_click::ClickError;
use std::path::{Path, PathBuf};

/// `DefaultStagePathParts` for a stage FQN without the `@` prefix.
#[derive(Clone, Debug)]
pub struct StageParts {
    pub stage: String,
    pub stage_name: String,
    pub directory: String,
    pub schema: Option<String>,
}

impl StageParts {
    pub fn from_fqn(stage_fqn: &str, subdir: &str) -> Result<StageParts> {
        let full = if subdir.is_empty() {
            stage_fqn.to_string()
        } else {
            format!("{stage_fqn}/{subdir}")
        };
        let Some(m) = snow_api::identifiers::STAGE_PATH.captures(&full) else {
            return Err(SnowError::cli("Invalid stage path"));
        };
        let directory = m
            .name("directory")
            .map(|d| d.as_str().to_string())
            .unwrap_or_default();
        let schema = m
            .name("s")
            .or_else(|| m.name("f"))
            .map(|x| x.as_str().to_string());
        let prefix = m
            .name("prefix")
            .map(|p| p.as_str())
            .unwrap_or("@")
            .to_string();
        let stage = full
            .strip_suffix(directory.as_str())
            .unwrap_or(&full)
            .trim_end_matches('/')
            .to_string();
        let mut stage_name = Fqn::from_stage(&stage)?.name;
        if let Some(s) = stage_name.strip_prefix(&prefix) {
            stage_name = s.to_string();
        }
        Ok(StageParts {
            stage,
            stage_name,
            directory,
            schema,
        })
    }
    pub fn full_path(&self) -> String {
        format!("{}/{}", self.stage.trim_end_matches('/'), self.directory)
            .trim_end_matches('/')
            .to_string()
    }
    pub fn path(&self) -> String {
        format!(
            "{}/{}",
            self.stage_name.trim_end_matches('/'),
            self.directory
        )
        .trim_end_matches('/')
        .to_string()
    }
}

/// `PolicyBase`: whether a prompt is answered yes, no, or asked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Allow,
    Deny,
    Ask,
}

impl Policy {
    pub fn from_flags(force: bool, interactive: bool) -> Policy {
        if force {
            Policy::Allow
        } else if interactive {
            Policy::Ask
        } else {
            Policy::Deny
        }
    }
    pub fn should_proceed(self, cli: &mut Cli, prompt: &str) -> Result<bool> {
        match self {
            Policy::Allow => Ok(true),
            Policy::Deny => Ok(false),
            Policy::Ask => crate::common::confirm(cli, prompt, false),
        }
    }
    /// A declined prompt prints `declined` and exits 0; a non-interactive run
    /// without `--force` prints `needs_force` and exits 1.
    pub fn proceed_or_exit(
        self,
        cli: &mut Cli,
        prompt: &str,
        interactive: bool,
        declined: &str,
        needs_force: &str,
    ) -> Result<()> {
        if self.should_proceed(cli, prompt)? {
            return Ok(());
        }
        if interactive {
            cli.console.message(declined);
            return Err(SnowError::Click(ClickError::Exit(0)));
        }
        cli.console.message(needs_force);
        Err(SnowError::Click(ClickError::Exit(1)))
    }
}

pub struct Package {
    pub id: String,
    pub model: PyV,
    pub project_root: PathBuf,
    pub project: snow_api::project::Project,
}

fn field(m: &PyV, k: &str) -> String {
    m.get(k)
        .filter(|v| !v.is_none())
        .map(PyV::py_str)
        .unwrap_or_default()
}

impl Package {
    pub fn new(cli: &Cli, e: &super::AppEntities) -> Package {
        Package {
            id: e.package_id.clone(),
            model: e.package.clone(),
            project_root: cli.project_root(),
            project: e.project.clone(),
        }
    }
    pub fn deploy_root(&self) -> PathBuf {
        self.project_root
            .join(field(&self.model, "deploy_root"))
            .join(field(&self.model, "stage_subdirectory"))
    }
    pub fn generated_root(&self) -> PathBuf {
        self.deploy_root()
            .join(field(&self.model, "generated_root"))
    }
    pub fn name(&self) -> Result<String> {
        Ok(crate::entities::fqn(&self.model, &self.id)?.name)
    }
    pub fn role(&self, cli: &mut Cli) -> Result<Option<String>> {
        super::role_of(cli, &self.model)
    }
    pub fn warehouse(&self, cli: &mut Cli) -> Result<Option<String>> {
        super::warehouse_of(cli, &self.model)
    }
    pub fn stage_path(&self) -> Result<StageParts> {
        StageParts::from_fqn(
            &format!("{}.{}", self.name()?, field(&self.model, "stage")),
            &field(&self.model, "stage_subdirectory"),
        )
    }
    pub fn scratch_stage_path(&self) -> Result<StageParts> {
        StageParts::from_fqn(
            &format!("{}.{}", self.name()?, field(&self.model, "scratch_stage")),
            "",
        )
    }
    pub fn artifacts(&self) -> Vec<PyV> {
        crate::apps::entity_artifacts(&self.model)
    }
    pub fn distribution(&self) -> String {
        let d = field(&self.model, "distribution");
        if d.is_empty() { "internal".into() } else { d }
    }

    /// `_bundle()`: the deploy root, artifact processors and children.
    pub fn bundle(&self, cli: &mut Cli) -> Result<BundleMap> {
        let artifacts = self.artifacts();
        let map = crate::artifacts::build_native_bundle(
            &self.project_root,
            &self.deploy_root(),
            &artifacts,
        )?;
        super::compiler::compile(cli, self, &artifacts)?;
        let children = self
            .model
            .get("children")
            .and_then(PyV::as_list)
            .cloned()
            .unwrap_or_default();
        if !children.is_empty() {
            let sql = self.bundle_children(cli, &children)?;
            let setup = setup_script_file(&self.deploy_root())?;
            let existing = std::fs::read_to_string(&setup)?;
            crate::artifacts::write_in_bundle(
                &setup,
                format!(
                    "{existing}\n-- AUTO GENERATED CHILDREN SECTION\n{}\n",
                    sql.join("\n")
                ),
            )?;
        }
        Ok(map)
    }

    /// `_bundle_children(action_ctx)`: bundle each child and return its setup script SQL.
    fn bundle_children(&self, cli: &mut Cli, children: &[PyV]) -> Result<Vec<String>> {
        use snow_api::identifiers::to_identifier;
        let dir_field = self
            .model
            .str_of("children_artifacts_dir")
            .unwrap_or_else(|| "_children/".into());
        let deploy_root = self.deploy_root();
        let children_dir = deploy_root.join(&dir_field);
        if children_dir.exists() {
            return Err(SnowError::unexpected(format!(
                "[Errno 17] File exists: '{}'",
                children_dir.display()
            ))
            .named("FileExistsError"));
        }
        std::fs::create_dir_all(&children_dir)?;
        let mut sql = Vec::new();
        for child in children {
            let target = child.str_of("target").unwrap_or_default();
            let safe: String = target
                .chars()
                .filter(|c| *c == ' ' || *c == '_' || c.is_alphanumeric())
                .collect();
            let child_dir = children_dir.join(&safe);
            if child_dir.exists() {
                return Err(SnowError::click(format!(
                    "Could not create sub-directory at {}. Make sure child entity names do not collide with each other.",
                    child_dir.display()
                )));
            }
            std::fs::create_dir_all(&child_dir)?;
            let Some(entity) = self.project.entities().get(&target).cloned() else {
                return Err(
                    SnowError::unexpected(format!("No such entity ID: {target}"))
                        .named("ValueError"),
                );
            };
            let ty = EntityType::of(&entity);
            if ty != Some(EntityType::Streamlit) {
                return Err(SnowError::unexpected(format!(
                    "'{}' object has no attribute 'validate_as_child'",
                    match ty {
                        Some(EntityType::Function) => "FunctionEntity",
                        Some(EntityType::Procedure) => "ProcedureEntity",
                        Some(EntityType::Application) => "ApplicationEntity",
                        Some(EntityType::ApplicationPackage) => "ApplicationPackageEntity",
                        Some(EntityType::Notebook) => "NotebookEntity",
                        Some(EntityType::ComputePool) => "ComputePoolEntity",
                        Some(EntityType::ImageRepository) => "ImageRepositoryEntity",
                        Some(EntityType::Service) => "ServiceEntity",
                        _ => "EntityBase",
                    }
                ))
                .named("AttributeError"));
            }
            let app_role = child
                .get_some("ensure_usable_by")
                .and_then(|e| e.get_some("application_roles"))
                .and_then(|r| match r {
                    PyV::List(l) => l.last().map(PyV::py_str),
                    other => Some(other.py_str()),
                })
                .map(|r| to_identifier(&r));
            let schema = child
                .get_some("identifier")
                .and_then(|i| i.str_of("schema_").or_else(|| i.str_of("schema")))
                .filter(|s| !s.is_empty())
                .map(|s| to_identifier(&s));
            let rel = child_dir
                .strip_prefix(&deploy_root)
                .unwrap_or(&child_dir)
                .to_string_lossy()
                .into_owned();
            sql.push(crate::streamlit::child_deploy_sql(
                cli,
                &target,
                &entity,
                &child_dir,
                &rel,
                schema.as_deref(),
            )?);
            if let Some(role) = app_role {
                sql.push(format!("CREATE APPLICATION ROLE IF NOT EXISTS {role};"));
                if let Some(sc) = &schema {
                    sql.push(format!(
                        "GRANT USAGE ON SCHEMA {sc} TO APPLICATION ROLE {role};"
                    ));
                }
                let name = match &schema {
                    Some(sc) => format!("{sc}.{target}"),
                    None => target.clone(),
                };
                sql.push(format!(
                    "GRANT USAGE ON STREAMLIT {name} TO APPLICATION ROLE {role};"
                ));
            }
        }
        Ok(sql)
    }

    /// `get_existing_app_pkg_info`.
    pub fn existing_info(&self, cli: &mut Cli) -> Result<Option<Row>> {
        let role = self.role(cli)?;
        let name = self.name()?;
        super::with_role(cli, role.as_deref(), |cli| {
            cli.show_specific_object("application packages", &name, "name", "", false)
        })
    }

    /// `get_app_pkg_distribution_in_snowflake`.
    pub fn distribution_in_snowflake(&self, cli: &mut Cli) -> Result<String> {
        let role = self.role(cli)?;
        let name = self.name()?;
        super::with_role(cli, role.as_deref(), |cli| {
            let c = match cli.try_query(&format!("describe application package {name}"))? {
                Ok(c) => c,
                Err(e) => return Err(generic_sql_error(cli, e)),
            };
            if c.rows.is_empty() {
                return Err(sql_execution_error(None));
            }
            for r in &c.rows {
                let k = r.first().cloned().flatten().unwrap_or_default();
                if k.to_lowercase() == "distribution" {
                    return Ok(r
                        .get(1)
                        .cloned()
                        .flatten()
                        .unwrap_or_default()
                        .to_lowercase());
                }
            }
            Err(SnowError::unexpected(format!(
                "Could not find the 'distribution' attribute for application package {name} in the output of SQL query:\n'describe application package {name}'\n"
            ))
            .named("ObjectPropertyNotFoundError"))
        })
    }

    /// `verify_project_distribution(expected_distribution)`.
    pub fn verify_distribution(&self, cli: &mut Cli, actual: &str) -> Result<bool> {
        let project = self.distribution().to_lowercase();
        if actual != project {
            let name = self.name()?;
            cli.console.warning(&format!(
                "Application package {name} in your Snowflake account has distribution property {actual},\nwhich does not match the value specified in project definition file: {project}.\n"
            ));
            return Ok(false);
        }
        Ok(true)
    }

    /// `_get_enable_release_channels_flag`.
    fn enable_release_channels(&self, cli: &mut Cli) -> Option<bool> {
        let from_yml = self
            .model
            .get("enable_release_channels")
            .and_then(|v| match v {
                PyV::Bool(b) => Some(*b),
                _ => None,
            });
        let value = match from_yml {
            Some(b) => Some(b),
            None => {
                let flag = cli
                    .config
                    .bool_value(&["cli", "features"], "enable_release_channels", None)
                    .ok()
                    .flatten();
                if flag.is_some() {
                    cli.console.warning(
                        "ENABLE_RELEASE_CHANNELS value in config.toml is deprecated. Set [enable_release_channels] for the application package in snowflake.yml instead.",
                    );
                }
                flag
            }
        };
        let in_account = crate::snowsight::ui_parameter_or(
            cli,
            "FEATURE_RELEASE_CHANNELS",
            serde_json::json!("ENABLED"),
        ) == serde_json::json!("ENABLED");
        if value == Some(true) && !in_account {
            cli.console.warning(
                "Ignoring [enable_release_channels] value because release channels are not enabled in the current account.",
            );
            return None;
        }
        value
    }

    /// `create_app_package`.
    pub fn create_app_package(&self, cli: &mut Cli) -> Result<()> {
        let name = self.name()?;
        let role = self.role(cli)?;
        if let Some(row) = self.existing_info(cli)? {
            let actual = self.distribution_in_snowflake(cli)?;
            if !self.verify_distribution(cli, &actual)? {
                cli.console.warning(&format!(
                    "Continuing to execute `snow app run` on application package {name} with distribution '{actual}'."
                ));
            }
            if actual == "internal" {
                let comment = row.get("comment").map(Pv::py_str).unwrap_or_default();
                if !facade::ALLOWED_SPECIAL_COMMENTS.contains(&comment.as_str()) {
                    return Err(SnowError::click(format!(
                        "An Application Package {name} already exists in account that may have been created without Snowflake CLI."
                    ))
                    .named("ApplicationPackageAlreadyExistsError"));
                }
            }
            let flag = self.enable_release_channels(cli);
            return facade::alter_application_package_properties(cli, &name, flag, role.as_deref());
        }
        cli.console.step(&format!(
            "Creating new application package {name} in account."
        ));
        let flag = self.enable_release_channels(cli);
        facade::create_application_package(cli, &name, &self.distribution(), flag, role.as_deref())
    }

    /// `_deploy(...)`.
    #[allow(clippy::too_many_arguments)]
    pub fn deploy(
        &self,
        cli: &mut Cli,
        bundle: Option<BundleMap>,
        prune: bool,
        recursive: bool,
        paths: &[PathBuf],
        print_diff: bool,
        validate: bool,
        stage: &StageParts,
        interactive: bool,
        force: bool,
        run_post_deploy_hooks: bool,
    ) -> Result<crate::stage_diff::DiffResult> {
        let policy = Policy::from_flags(force, interactive);
        let map = match bundle {
            Some(b) => b,
            None => self.bundle(cli)?,
        };
        if let Err(e) = self.create_app_package(cli) {
            match &e {
                SnowError::Cli {
                    name: "ApplicationPackageAlreadyExistsError",
                    ..
                } => {
                    cli.console.warning(&e.message());
                    if !policy.should_proceed(cli, "Proceed with using this package?")? {
                        return Err(SnowError::Click(ClickError::Abort));
                    }
                }
                _ => return Err(e),
            }
        }
        let role = self.role(cli)?;
        let name = self.name()?;
        let deploy_root = self.deploy_root();
        let diff = super::with_role(cli, role.as_deref(), |cli| {
            sync_package_stage(
                cli,
                &deploy_root,
                &map,
                &name,
                role.as_deref(),
                prune,
                recursive,
                stage,
                paths,
                print_diff,
            )
        })?;
        if run_post_deploy_hooks {
            self.execute_post_deploy_hooks(cli)?;
        }
        if validate {
            self.validate_setup_script(cli, false, interactive, force)?;
        }
        Ok(diff)
    }

    pub fn execute_post_deploy_hooks(&self, cli: &mut Cli) -> Result<()> {
        let hooks = self
            .model
            .get_some("meta")
            .and_then(|m| m.get("post_deploy"))
            .and_then(PyV::as_list)
            .cloned()
            .unwrap_or_default();
        if hooks.is_empty() {
            return Ok(());
        }
        let role = self.role(cli)?;
        let warehouse = self.warehouse(cli)?;
        let name = self.name()?;
        execute_post_deploy_hooks(
            cli,
            &self.project_root,
            &self.project,
            &hooks,
            EntityType::ApplicationPackage,
            role.as_deref(),
            warehouse.as_deref(),
            Some(&name),
        )
    }

    /// `validate_setup_script(use_scratch_stage, interactive, force)`.
    pub fn validate_setup_script(
        &self,
        cli: &mut Cli,
        use_scratch_stage: bool,
        interactive: bool,
        force: bool,
    ) -> Result<()> {
        cli.console
            .phase_start("Validating Snowflake Native App setup script.");
        let result = self.validation_result(cli, use_scratch_stage, interactive, force);
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                cli.console.phase_end(None);
                return Err(e);
            }
        };
        for key in ["warnings", "errors"] {
            for item in result
                .get(key)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
            {
                cli.console.warning(&validation_item_to_str(&item));
            }
        }
        cli.console.phase_end(None);
        if result.get("status").and_then(|s| s.as_str()) == Some("FAIL") {
            return Err(
                SnowError::click("Snowflake Native App setup script failed validation.")
                    .named("SetupScriptFailedValidation"),
            );
        }
        Ok(())
    }

    /// `get_validation_result(use_scratch_stage, interactive, force)`.
    pub fn validation_result(
        &self,
        cli: &mut Cli,
        use_scratch_stage: bool,
        interactive: bool,
        force: bool,
    ) -> Result<serde_json::Value> {
        let stage = if use_scratch_stage {
            self.scratch_stage_path()?
        } else {
            self.stage_path()?
        };
        if use_scratch_stage {
            self.deploy(
                cli,
                None,
                true,
                true,
                &[],
                false,
                false,
                &stage,
                interactive,
                force,
                false,
            )?;
        }
        let prefixed = crate::stage_execute::standard_prefix(&stage.full_path());
        let r = cli.try_query(&format!(
            "call system$validate_native_app_setup({})",
            to_string_literal(&prefixed)
        ));
        let out = match r {
            Err(e) => Err(e),
            Ok(Err(e)) => {
                if e.kind == snow_connector::ErrorKind::Programming
                    && e.errno == facade::errno::DOES_NOT_EXIST_OR_NOT_AUTHORIZED
                {
                    Err(SnowError::click(format!(
                        "Application Package {} does not exist in the Snowflake account or not authorized.",
                        self.name()?
                    ))
                    .named("ApplicationPackageDoesNotExistError"))
                } else {
                    Err(generic_sql_error(cli, e))
                }
            }
            Ok(Ok(c)) => {
                if c.rows.is_empty() {
                    Err(sql_execution_error(None))
                } else {
                    let cell = c.rows[0].first().cloned().flatten().unwrap_or_default();
                    serde_json::from_str(&cell)
                        .map_err(|e| SnowError::unexpected(e.to_string()).named("JSONDecodeError"))
                }
            }
        };
        if use_scratch_stage {
            let scratch = self.scratch_stage_path()?;
            cli.console
                .step(&format!("Dropping stage {}.", scratch.stage));
            let role = self.role(cli)?;
            super::with_role(cli, role.as_deref(), |cli| {
                cli.execute_query(&format!("drop stage if exists {}", scratch.stage))
                    .map(|_| ())
            })?;
        }
        out
    }

    /// `get_existing_version_info(version)`.
    pub fn existing_version_info(&self, cli: &mut Cli, version: &str) -> Result<Option<Row>> {
        let role = self.role(cli)?;
        let name = self.name()?;
        super::with_role(cli, role.as_deref(), |cli| {
            let q = format!(
                "show versions like {} in application package {name}",
                identifier_to_show_like_pattern(version)
            );
            let c = match cli.try_query(&q)? {
                Ok(c) => c,
                Err(e) => {
                    if e.raw_msg.contains("does not exist or not authorized") {
                        return Err(SnowError::click(format!(
                            "Application Package {name} does not exist in the Snowflake account or not authorized."
                        ))
                        .named("ApplicationPackageDoesNotExistError"));
                    }
                    return Err(generic_sql_error(cli, e));
                }
            };
            let want = unquote_identifier(version);
            let rows: Vec<Row> = cli
                .rows(&c)
                .into_iter()
                .filter(|r| r.get("version").map(Pv::py_str).as_deref() == Some(&want))
                .collect();
            let patch = |r: &Row| match r.get("patch") {
                Some(Pv::Int(i)) => *i,
                Some(other) => other.py_str().parse().unwrap_or(0),
                None => 0,
            };
            let mut best: Option<Row> = None;
            for r in rows {
                if best.as_ref().is_none_or(|b| patch(&r) > patch(b)) {
                    best = Some(r);
                }
            }
            Ok(best)
        })
    }
}

/// `validation_item_to_str`.
pub fn validation_item_to_str(item: &serde_json::Value) -> String {
    let msg = match &item["message"] {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let code = &item["errorCode"];
    let truthy = match code {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Number(n) => n.as_f64() != Some(0.0),
        serde_json::Value::Bool(b) => *b,
        _ => true,
    };
    if truthy {
        let c = match code {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        format!("{msg} (error code {c})")
    } else {
        msg
    }
}

/// `SnowflakeSQLExecutionError(query)` (its message keeps the docstring's indentation).
pub fn sql_execution_error(query: Option<&str>) -> SnowError {
    SnowError::cli(format!(
        "\n                \n    Could not successfully execute the Snowflake SQL statements.\n    \n                {}\n            ",
        query.unwrap_or("")
    ))
    .named("SnowflakeSQLExecutionError")
}

/// `generic_sql_error_handler(err)`.
pub fn generic_sql_error(cli: &Cli, e: snow_connector::DbError) -> SnowError {
    let msg = e.msg_with(cli.connector_info);
    if e.errno == facade::errno::DOES_NOT_EXIST_OR_CANNOT_BE_PERFORMED
        || msg.contains("does not exist or not authorized")
    {
        return SnowError::cli(format!(
            "Received the following error message while executing SQL statement:\n'{msg}'\nPlease check the name of the resource you are trying to query or the permissions of the role you are using to run the query."
        ))
        .named("DoesNotExistOrUnauthorizedError");
    }
    if e.errno == facade::errno::NO_WAREHOUSE_SELECTED_IN_SESSION {
        return SnowError::cli(format!(
            "Received the following error message while executing SQL statement:\n'{msg}'\nPlease provide a warehouse for the active session role in your project definition file, config.toml file, or via command line."
        ))
        .named("NoWarehouseSelectedInSessionError");
    }
    snow_api::connect::db_error_to_cli(&e, cli.connector_info)
}

/// `execute_post_deploy_hooks(...)`.
#[allow(clippy::too_many_arguments)]
pub fn execute_post_deploy_hooks(
    cli: &mut Cli,
    project_root: &Path,
    project: &snow_api::project::Project,
    hooks: &[PyV],
    object_type: EntityType,
    role: Option<&str>,
    warehouse: Option<&str>,
    database: Option<&str>,
) -> Result<()> {
    cli.console
        .phase_start(&format!("Executing {object_type} post-deploy actions"));
    let r = (|| -> Result<()> {
        let mut scripts = Vec::new();
        for h in hooks {
            let Some(path) = h.str_of("sql_script").filter(|s| !s.is_empty()) else {
                return Err(SnowError::unexpected(format!(
                    "Unsupported {object_type} post-deploy hook type: {}",
                    h.py_repr()
                ))
                .named("ValueError"));
            };
            let display = h
                .str_of("_display_path")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| path.clone());
            scripts.push((path, display));
        }
        let mut contents = Vec::new();
        for (path, _) in &scripts {
            contents.push(render_script_template(cli, project_root, project, path)?);
        }
        for ((_, display), content) in scripts.iter().zip(contents) {
            cli.console
                .step(&format!("Executing SQL script: {display}"));
            facade::execute_user_script(cli, &content, display, role, warehouse, database)?;
        }
        Ok(())
    })();
    cli.console.phase_end(None);
    r
}

/// `render_script_template(project_root, template_context, script, allow_absolute_paths=True)`.
pub fn render_script_template(
    cli: &mut Cli,
    project_root: &Path,
    project: &snow_api::project::Project,
    script: &str,
) -> Result<String> {
    let cand = Path::new(script);
    let full = if cand.is_absolute() {
        cand.to_path_buf()
    } else {
        let root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let resolved = crate::artifacts::resolve_without_follow(&root.join(cand));
        let resolved = resolved.canonicalize().unwrap_or(resolved);
        if !resolved.starts_with(&root) {
            return Err(SnowError::click(format!(
                "Refusing to read SQL script outside the project root: {} resolves to {}, which is outside {}.",
                snow_click::py_repr(script),
                resolved.display(),
                root.display()
            )));
        }
        resolved
    };
    let Ok(content) = std::fs::read_to_string(&full) else {
        return Err(
            SnowError::click(format!("Script \"{script}\" does not exist"))
                .named("MissingScriptError"),
        );
    };
    let overrides = project.env_overrides.clone();
    crate::rendering::render_project_sql(
        &content,
        script,
        &project.ctx,
        &overrides,
        &mut cli.console,
        true,
    )
    .map_err(|(m, line)| {
        let l = line.map(|l| format!(":{l}")).unwrap_or_default();
        SnowError::click(format!(
            "File \"{script}{l}\" does not contain a valid template: {m}"
        ))
        .named("InvalidTemplateInFileError")
    })
}

/// `sync_deploy_root_with_stage(..., package_name=..., role=...)`.
#[allow(clippy::too_many_arguments)]
pub fn sync_package_stage(
    cli: &mut Cli,
    deploy_root: &Path,
    map: &BundleMap,
    package: &str,
    role: Option<&str>,
    prune: bool,
    recursive: bool,
    stage: &StageParts,
    local_paths: &[PathBuf],
    print_diff: bool,
) -> Result<crate::stage_diff::DiffResult> {
    let stage_fqn = stage.stage.clone();
    cli.console.step(&format!(
        "Checking if stage {stage_fqn} exists, or creating a new one if none exists."
    ));
    if !facade::stage_exists(cli, &stage_fqn)? {
        facade::create_schema(
            cli,
            stage.schema.as_deref().unwrap_or_default(),
            Some(package),
        )?;
        facade::create_stage(cli, &stage_fqn)?;
    }
    let resolved_root = crate::paths::resolve(deploy_root);
    if print_diff {
        cli.console.step(&format!(
            "Performing a diff between the Snowflake stage: {} and your local deploy_root: {}.",
            stage.path(),
            resolved_root.display()
        ));
    }
    let mut diff = crate::stage_diff::compute(
        cli,
        deploy_root,
        &stage.stage_name,
        &stage.directory,
        &stage.full_path(),
    )?;
    if !local_paths.is_empty() {
        let resolved: Vec<PathBuf> = local_paths
            .iter()
            .map(|p| crate::artifacts::resolve_without_follow(p))
            .collect();
        if !recursive {
            verify_no_directories(&resolved)?;
        }
        let mut deploy_paths: Vec<PathBuf> = Vec::new();
        for r in &resolved {
            if !r.exists() {
                return Err(SnowError::click(format!(
                    "The following path does not exist: {}",
                    r.display()
                )));
            }
            let mut found = map.to_deploy_paths(r);
            if found.is_empty() && r.is_dir() && recursive {
                for src in map.all_sources_absolute() {
                    if src.starts_with(r) && src != *r {
                        found.extend(map.to_deploy_paths(&src));
                    }
                }
            }
            if found.is_empty() {
                return Err(SnowError::click(format!(
                    "No artifact found for {}",
                    r.display()
                )));
            }
            deploy_paths.extend(found);
        }
        let droot = crate::artifacts::resolve_without_follow(deploy_root);
        let mut keep: Vec<String> = Vec::new();
        for p in deploy_paths {
            if p.is_dir() {
                for f in crate::artifacts::walk(&p).iter().filter(|f| !f.is_dir()) {
                    keep.push(rel_posix(f, &droot));
                }
            } else {
                keep.push(rel_posix(&p, &droot));
            }
        }
        diff.identical.retain(|x| keep.contains(x));
        diff.different.retain(|x| keep.contains(x));
        diff.only_local.retain(|x| keep.contains(x));
        diff.only_on_stage.retain(|x| keep.contains(x));
    } else if !recursive {
        let mut kids: Vec<PathBuf> = std::fs::read_dir(&resolved_root)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        kids.sort();
        verify_no_directories(&kids)?;
    }
    if !prune {
        let not_removed = std::mem::take(&mut diff.only_on_stage);
        if !not_removed.is_empty() {
            cli.console.warning(&format!(
                "The following files exist only on the stage:\n{}\n\nUse the --prune flag to delete them from the stage.",
                not_removed.join("\n")
            ));
        }
    }
    if print_diff {
        crate::stage_diff::print_with_map(cli, &diff, Some(map));
    }
    if diff.has_changes() {
        cli.console.step(&format!(
            "Uploading files from local {} directory to stage.",
            resolved_root.display()
        ));
        crate::logs::info(
            "snowflake.cli._plugins.stage.diff",
            &format!(
                "Uploading diff-ed files from your local {} directory to the Snowflake stage.",
                deploy_root.display()
            ),
        );
        let full = stage.full_path();
        for p in &diff.only_on_stage {
            let target = snow_api::stage_path::StagePath::from_stage_str(&full).join(p);
            super::with_role(cli, role, |cli| {
                cli.execute_query(&format!("remove {}", target.path_for_sql()))
                    .map(|_| ())
            })?;
        }
        for (paths, overwrite) in [(&diff.different, true), (&diff.only_local, false)] {
            for p in paths {
                let sub = Path::new(p)
                    .parent()
                    .map(|x| x.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let dest = if sub.is_empty() {
                    full.clone()
                } else {
                    format!("{full}/{sub}")
                };
                let local = deploy_root.join(p);
                super::with_role(cli, role, |cli| {
                    crate::stage::put(
                        cli,
                        &local.to_string_lossy(),
                        &snow_api::stage_path::StagePath::from_stage_str(&dest),
                        4,
                        overwrite,
                        false,
                    )
                    .map(|_| ())
                })?;
            }
        }
    }
    Ok(diff)
}

fn rel_posix(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn verify_no_directories(paths: &[PathBuf]) -> Result<()> {
    for p in paths {
        if p.is_dir() {
            return Err(SnowError::click(format!(
                "{} is a directory. Add the -r flag to deploy directories.",
                p.display()
            )));
        }
    }
    Ok(())
}

/// `find_manifest_file` + `safe_load`.
pub fn read_manifest(deploy_root: &Path) -> Result<PyV> {
    let root = deploy_root
        .canonicalize()
        .unwrap_or_else(|_| deploy_root.to_path_buf());
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                } else {
                    files.push(p);
                }
            }
        }
        files.sort();
        for f in files {
            if f.file_name()
                .is_some_and(|n| n.to_string_lossy().to_lowercase() == "manifest.yml")
            {
                let text = std::fs::read_to_string(&f)?;
                return Ok(snow_api::project::yaml::safe_load(&text)
                    .map_err(|e| SnowError::unexpected(format!("{e:?}")).named("YAMLError"))?
                    .unwrap_or(PyV::Dict(Default::default())));
            }
        }
        dirs.sort();
        dirs.reverse();
        stack.extend(dirs);
    }
    Err(SnowError::click(
        "Required manifest.yml file not found in the deploy root of the Snowflake Native App project.",
    ))
}

/// `find_version_info_in_manifest_file`: (version, patch, label).
pub fn manifest_version_info(
    deploy_root: &Path,
) -> Result<(Option<String>, Option<i64>, Option<String>)> {
    let m = read_manifest(deploy_root)?;
    let Some(v) = m.get_some("version") else {
        return Ok((None, None, None));
    };
    if !matches!(v, PyV::Dict(_)) {
        return Err(SnowError::click(
            "Error occurred while reading manifest.yml. Received unexpected version format.",
        ));
    }
    let name = v.get_some("name").map(|n| to_identifier(&n.py_str()));
    let patch = match v.get_some("patch") {
        None => None,
        Some(PyV::Int(i)) => Some(*i),
        Some(other) => Some(other.py_str().trim().parse::<i64>().map_err(|_| {
            SnowError::unexpected(format!(
                "invalid literal for int() with base 10: {}",
                other.py_repr()
            ))
            .named("ValueError")
        })?),
    };
    let label = v.get_some("label").map(PyV::py_str);
    Ok((name, patch, label))
}

/// `find_setup_script_file(deploy_root)`.
pub fn setup_script_file(deploy_root: &Path) -> Result<PathBuf> {
    let m = read_manifest(deploy_root)?;
    let Some(rel) = m
        .get_some("artifacts")
        .and_then(|a| a.get_some("setup_script"))
        .map(PyV::py_str)
    else {
        return Err(SnowError::click(
            "Manifest.yml file must contain an artifacts section to specify the location of the setup script.",
        ));
    };
    let root = crate::artifacts::resolve_without_follow(deploy_root);
    let file = crate::artifacts::resolve_without_follow(&deploy_root.join(&rel));
    if !file.starts_with(&root) {
        return Err(SnowError::cli(
            "Setup script path must stay within the deploy root.",
        ));
    }
    if file.is_file() {
        Ok(file)
    } else {
        Err(SnowError::click(format!(
            "Could not find setup script file at {}.",
            file.display()
        )))
    }
}

/// `VersionInfo(version_name, patch_number, label)`.
#[derive(Clone, Debug)]
pub struct VersionInfo {
    pub version: String,
    pub patch: Option<i64>,
    pub label: Option<String>,
}

const VERSION_FROM_MANIFEST: &str = "Version was not provided through the Snowflake CLI. Checking version in the manifest.yml instead.\n";

impl Package {
    /// `resolve_version_info(...)`.
    #[allow(clippy::too_many_arguments)]
    fn resolve_version_info(
        &self,
        cli: &mut Cli,
        version: Option<String>,
        patch: Option<i64>,
        label: Option<String>,
        policy: Policy,
        interactive: bool,
    ) -> Result<VersionInfo> {
        let (rv, rp, rl);
        if let Some(v) = version {
            cli.console.message(
                "Ignoring version information from the application manifest since a version was explicitly specified with the command.",
            );
            rv = v;
            rp = patch;
            rl = label;
        } else {
            cli.console.message(VERSION_FROM_MANIFEST);
            let (mv, mp, ml) = manifest_version_info(&self.deploy_root())?;
            let Some(mv) = mv else {
                return Err(SnowError::click(
                    "Manifest.yml file does not contain a value for the version field.",
                ));
            };
            rv = mv;
            rp = match (patch, mp) {
                (Some(p), Some(m)) if m != p => {
                    cli.console.warning(&format!(
                        "Cannot resolve version. Found patch: {m} in manifest.yml which is different from provided patch {p}."
                    ));
                    let prompt = format!(
                        "Do you want to ignore patch in manifest.yml and proceed with provided --patch {p}?"
                    );
                    policy.proceed_or_exit(
                        cli,
                        &prompt,
                        interactive,
                        "Not creating a new patch.",
                        "Could not create a new patch non-interactively without --force.",
                    )?;
                    Some(p)
                }
                (Some(p), _) => Some(p),
                (None, m) => m,
            };
            rl = label.or(ml);
        }
        if rp.is_some_and(|p| p != 0) {
            let name = self.name()?;
            match self.existing_version_info(cli, &to_identifier(&rv)) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(SnowError::Click(ClickError::usage_class(
                        format!(
                            "Version {rv} is not defined in the application package {name}. Try again with a patch of 0 or without specifying any patch."
                        ),
                        "BadOptionUsage",
                    )));
                }
                Err(SnowError::Cli {
                    name: "ApplicationPackageDoesNotExistError",
                    ..
                }) => {
                    return Err(SnowError::Click(ClickError::usage_class(
                        format!(
                            "Application package {name} does not exist yet. Try again with a patch of 0 or without specifying any patch."
                        ),
                        "BadOptionUsage",
                    )));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(VersionInfo {
            version: rv,
            patch: rp,
            label: rl,
        })
    }

    /// `check_index_changes_in_git_repo(policy, interactive)`.
    fn check_git(&self, cli: &mut Cli, policy: Policy, interactive: bool) -> Result<()> {
        let inside = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.project_root)
            .args(["rev-parse", "--git-dir"])
            .output()
            .is_ok_and(|o| o.status.success());
        if !inside {
            return Ok(());
        }
        let dirty = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.project_root)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .is_ok_and(|o| !o.stdout.is_empty());
        if dirty {
            cli.console.warning(
                "Changes detected in the git repository. (Rerun your command with --skip-git-check flag to ignore this check)",
            );
            let prompt = "You have local changes in this repository that are not part of a previous commit. Do you still want to continue?";
            policy.proceed_or_exit(
                cli,
                prompt,
                interactive,
                "Not creating a new version.",
                "Cannot create a new version non-interactively without --force.",
            )?;
        }
        Ok(())
    }

    /// `action_version_create(...)`.
    #[allow(clippy::too_many_arguments)]
    pub fn version_create(
        &self,
        cli: &mut Cli,
        version: Option<String>,
        patch: Option<i64>,
        label: Option<String>,
        skip_git_check: bool,
        interactive: bool,
        force: bool,
        from_stage: bool,
    ) -> Result<VersionInfo> {
        let policy = Policy::from_flags(force, interactive);
        let map = self.bundle(cli)?;
        let info = self.resolve_version_info(cli, version, patch, label, policy, interactive)?;
        if !skip_git_check {
            self.check_git(cli, policy, interactive)?;
        }
        let stage = self.stage_path()?;
        if from_stage {
            if self.existing_info(cli)?.is_none() {
                return Err(SnowError::click(
                    "Cannot create version from stage because the application package does not exist yet. Try removing --from-stage flag or executing `snow app deploy` to deploy the application package first.",
                ));
            }
        } else {
            self.deploy(
                cli,
                Some(map),
                true,
                true,
                &[],
                true,
                true,
                &stage,
                interactive,
                force,
                true,
            )?;
        }
        let name = self.name()?;
        let role = self.role(cli)?;
        let warning = match facade::show_release_directives(cli, &name, None, role.as_deref()) {
            Err(SnowError::Cli { name: "InsufficientPrivilegesError", .. }) => {
                "Could not check for existing release directives due to insufficient privileges. The MANAGE RELEASES privilege is required to check for existing release directives.".to_string()
            }
            Err(e) => return Err(e),
            Ok(rows) => {
                let want = unquote_identifier(&info.version);
                let names: Vec<String> = rows
                    .iter()
                    .filter(|r| r.get("version").map(Pv::py_str).as_deref() == Some(&want))
                    .map(|r| r.get("name").map(Pv::py_str).unwrap_or_default())
                    .collect();
                if names.is_empty() {
                    String::new()
                } else {
                    format!(
                        "Version {} already defined in application package {name} and in release directive(s): {}.",
                        info.version,
                        names.join(", ")
                    )
                }
            }
        };
        if !warning.is_empty() {
            cli.console.warning(&warning);
            let prompt = format!(
                "Are you sure you want to create a new patch for version {} in application package {name}? Once added, this operation cannot be undone.",
                info.version
            );
            policy.proceed_or_exit(
                cli,
                &prompt,
                interactive,
                "Not creating a new patch.",
                "Cannot create a new patch non-interactively without --force.",
            )?;
        }
        let with_label = info
            .label
            .as_ref()
            .filter(|l| !l.is_empty())
            .map(|l| format!(" labeled {l}"))
            .unwrap_or_default();
        if self.existing_version_info(cli, &info.version)?.is_none() {
            cli.console.step(&format!(
                "Defining a new version {}{with_label} in application package {name}",
                info.version
            ));
            facade::create_version_in_package(
                cli,
                &name,
                &stage.full_path(),
                &info.version,
                info.label.as_deref(),
                role.as_deref(),
            )?;
            cli.console.message(&format!(
                "Version {}{with_label} created for application package {name}.",
                info.version
            ));
            return Ok(VersionInfo {
                patch: Some(0),
                ..info
            });
        }
        cli.console.step(&format!(
            "Adding new patch to version {}{with_label} defined in application package {name}",
            info.version
        ));
        let new_patch = facade::add_patch_to_package_version(
            cli,
            &name,
            &stage.full_path(),
            &info.version,
            info.patch,
            info.label.as_deref(),
            role.as_deref(),
        )?;
        cli.console.message(&format!(
            "Patch {new_patch}{with_label} created for version {} defined in application package {name}.",
            info.version
        ));
        Ok(VersionInfo {
            patch: Some(new_patch),
            ..info
        })
    }

    /// `action_version_drop(version, interactive, force)`: the result message.
    pub fn version_drop(
        &self,
        cli: &mut Cli,
        version: Option<String>,
        interactive: bool,
        force: bool,
    ) -> Result<String> {
        let policy = Policy::from_flags(force, interactive);
        let interactive = interactive && !force;
        let name = self.name()?;
        if self.existing_info(cli)?.is_none() {
            return Err(SnowError::click(format!(
                "Application Package {name} does not exist in the Snowflake account or not authorized."
            ))
            .named("ApplicationPackageDoesNotExistError"));
        }
        let actual = self.distribution_in_snowflake(cli)?;
        if !self.verify_distribution(cli, &actual)? {
            cli.console.warning(&format!(
                "Continuing to execute version drop on application package {name} with distribution '{actual}'."
            ));
        }
        let version = match version.filter(|v| !v.is_empty()) {
            Some(v) => v,
            None => {
                cli.console.message(
                    "Version was not provided through the Snowflake CLI. Checking version in the manifest.yml instead.\nThis step will bundle your app artifacts to determine the location of the manifest.yml file.\n",
                );
                self.bundle(cli)?;
                match manifest_version_info(&self.deploy_root())?.0 {
                    Some(v) => v,
                    None => {
                        return Err(SnowError::click(
                            "Manifest.yml file does not contain a value for the version field.",
                        ));
                    }
                }
            }
        };
        let version = to_identifier(&version);
        cli.console.step(&format!(
            "About to drop version {version} in application package {name}."
        ));
        let prompt = format!(
            "Are you sure you want to drop version {version} in application package {name}? Once dropped, this operation cannot be undone."
        );
        policy.proceed_or_exit(
            cli,
            &prompt,
            interactive,
            "Not dropping version.",
            "Cannot drop version non-interactively without --force.",
        )?;
        let sync = crate::snowsight::ui_parameter_or(
            cli,
            "ENABLE_SYNC_GLOBAL_VERSION_DROP",
            serde_json::json!(false),
        );
        let sync = match &sync {
            serde_json::Value::String(s) => s.trim().to_lowercase() == "true",
            serde_json::Value::Bool(b) => *b,
            _ => false,
        };
        let role = self.role(cli)?;
        if sync {
            cli.console
                .message("Dropping the version now, it might take a few minutes...");
            facade::drop_version_from_package(cli, &name, &version, role.as_deref())?;
            return Ok(format!(
                "Version {version} in application package {name} dropped successfully."
            ));
        }
        facade::drop_version_from_package(cli, &name, &version, role.as_deref())?;
        Ok("Version drop in progress. This may take a few minutes to complete.".into())
    }

    /// `action_drop(force_drop)`.
    pub fn drop(&self, cli: &mut Cli, force: bool) -> Result<()> {
        let name = self.name()?;
        let role = self.role(cli)?;
        let Some(row) = self.existing_info(cli)? else {
            cli.console.warning(&format!(
                "Role {} does not own any application package with the name {name}, or the application package does not exist.",
                role.clone().unwrap_or_else(|| "None".into())
            ));
            return Ok(());
        };
        let versions = facade::show_versions(cli, &name, role.as_deref())?;
        if !versions.is_empty() && !force {
            return Err(SnowError::click(
                "Application package could not be dropped as it has versions associated with it.\nDrop versions first, or use --force to override.",
            )
            .named("CouldNotDropApplicationPackageWithVersions"));
        }
        let actual = self.distribution_in_snowflake(cli)?;
        // `verify_project_distribution()` without an argument describes the package again.
        let again = self.distribution_in_snowflake(cli)?;
        if !self.verify_distribution(cli, &again)? {
            cli.console.warning(&format!(
                "Dropping application package {name} with distribution '{actual}'."
            ));
        }
        let comment = row.get("comment").map(Pv::py_str).unwrap_or_default();
        let mut needs_confirm = true;
        if actual == "internal" {
            if facade::ALLOWED_SPECIAL_COMMENTS.contains(&comment.as_str()) {
                needs_confirm = false;
            } else if !force {
                cli.console.warning(&format!(
                    "Application package {name} was not created by Snowflake CLI."
                ));
            }
        } else if !force {
            cli.console.warning(&format!(
                "Application package {name} in your Snowflake account has distribution property 'external' and could be associated with one or more of your listings on Snowflake Marketplace."
            ));
        }
        if needs_confirm && !force {
            let prompt = format!(
                "Application package details:\nName: {name}\nCreated on: {}\nDistribution: {actual}\nOwner: {}\nComment: {comment}\nAre you sure you want to drop it?\n",
                row.get("created_on").map(Pv::py_str).unwrap_or_default(),
                row.get("owner").map(Pv::py_str).unwrap_or_default(),
            );
            if !crate::common::confirm(cli, &prompt, false)? {
                cli.console
                    .message(&format!("Did not drop application package {name}."));
                return Ok(());
            }
        }
        drop_generic_object(cli, "application package", &name, role.as_deref(), false)
    }
}

/// `drop_generic_object(console, object_type, object_name, role, cascade)`.
pub fn drop_generic_object(
    cli: &mut Cli,
    object_type: &str,
    name: &str,
    role: Option<&str>,
    cascade: bool,
) -> Result<()> {
    super::with_role(cli, role, |cli| {
        cli.console
            .step(&format!("Dropping {object_type} {name} now."));
        let mut q = format!("drop {object_type} {name}");
        if cascade {
            q.push_str(" cascade");
        }
        if !matches!(cli.try_query(&q), Ok(Ok(_))) {
            return Err(sql_execution_error(Some(&q)));
        }
        cli.console
            .message(&format!("Dropped {object_type} {name} successfully."));
        Ok(())
    })
}

impl Package {
    /// `action_publish(...)`.
    #[allow(clippy::too_many_arguments)]
    pub fn publish(
        &self,
        cli: &mut Cli,
        version: Option<String>,
        patch: Option<i64>,
        channel: Option<String>,
        directive: &str,
        interactive: bool,
        force: bool,
        create_version: bool,
        from_stage: bool,
        label: Option<String>,
    ) -> Result<VersionInfo> {
        let policy = Policy::from_flags(force, interactive);
        if from_stage && !create_version {
            return Err(SnowError::usage(
                "--from-stage flag can only be used with --create-version flag.",
            ));
        }
        if label.is_some() && !create_version {
            return Err(SnowError::usage(
                "--label can only be used with --create-version flag.",
            ));
        }
        let (mut version, mut patch) = (version, patch);
        if create_version {
            let r = self.version_create(
                cli,
                version,
                patch,
                label,
                true,
                interactive,
                force,
                from_stage,
            )?;
            version = Some(r.version);
            patch = r.patch;
        }
        let Some(version) = version else {
            return Err(SnowError::usage(
                "Please provide a version using --version or use --create-version flag to create a version based on the manifest file.",
            ));
        };
        let Some(patch) = patch else {
            return Err(SnowError::usage(
                "Please provide a patch number using --patch or use --create-version flag to auto create a patch.",
            ));
        };
        let name = self.name()?;
        let role = self.role(cli)?;
        let versions = facade::show_versions(cli, &name, role.as_deref())?;
        let want = unquote_identifier(&version);
        // `patch not in available_patches` compares Python values: a text column never matches.
        let pnum = |r: &Row| match r.get("patch") {
            Some(Pv::Int(i)) => *i as i64,
            Some(Pv::Decimal(d)) => d
                .parse::<f64>()
                .ok()
                .filter(|f| f.fract() == 0.0)
                .map(|f| f as i64)
                .unwrap_or(i64::MIN),
            Some(Pv::Float(f)) if f.fract() == 0.0 => *f as i64,
            _ => i64::MIN,
        };
        let patches: Vec<i64> = versions
            .iter()
            .filter(|r| r.get("version").map(Pv::py_str).as_deref() == Some(&want))
            .map(pnum)
            .collect();
        if patches.is_empty() {
            return Err(SnowError::click(format!(
                "Version {version} does not exist in application package {name}. Use --create-version flag to create a new version."
            )));
        }
        if !patches.contains(&patch) {
            return Err(SnowError::click(format!(
                "Patch {patch} does not exist for version {version} in application package {name}. Use --create-version flag to add a new patch."
            )));
        }
        let channels = super::release_channels(cli, &name, role.as_deref())?;
        let channel = match channel.filter(|c| !c.is_empty()) {
            None => None,
            Some(c)
                if channels.is_empty()
                    && snow_api::identifiers::same_identifiers(&c, facade::DEFAULT_CHANNEL) =>
            {
                None
            }
            Some(c) => {
                super::validate_channel_in(&name, &channels, &c)?;
                Some(c)
            }
        };
        if let Some(ch) = &channel {
            let info = channels
                .iter()
                .find(|r| r.get("name").map(Pv::py_str).as_deref() == Some(&unquote_identifier(ch)))
                .cloned()
                .unwrap_or_default();
            let in_channel: Vec<String> = match info.get("versions") {
                Some(Pv::Str(s)) => serde_json::from_str::<Vec<String>>(s).unwrap_or_default(),
                Some(Pv::List(l)) => l.iter().map(Pv::py_str).collect(),
                _ => vec![],
            };
            if !in_channel.contains(&want) {
                if in_channel.len() >= 2 {
                    let directives =
                        facade::show_release_directives(cli, &name, Some(ch), role.as_deref())?;
                    let targeted: Vec<String> = directives
                        .iter()
                        .map(|d| d.get("version").map(Pv::py_str).unwrap_or_default())
                        .collect();
                    let free: Vec<&String> = in_channel
                        .iter()
                        .filter(|v| !targeted.contains(v))
                        .collect();
                    if free.is_empty() {
                        return Err(SnowError::click(format!(
                            "Maximum number of versions in release channel {ch} reached. Cannot add more versions."
                        )));
                    }
                    let mut last_updated: indexmap::IndexMap<String, String> =
                        indexmap::IndexMap::new();
                    for r in &versions {
                        let v = r.get("version").map(Pv::py_str).unwrap_or_default();
                        let c = r.get("created_on").map(Pv::py_str).unwrap_or_default();
                        if last_updated.get(&v).is_none_or(|cur| c > *cur) {
                            last_updated.insert(v, c);
                        }
                    }
                    let mut oldest: Option<(&String, String)> = None;
                    for v in free {
                        let lu = last_updated.get(v).cloned().unwrap_or_default();
                        if oldest.as_ref().is_none_or(|(_, o)| lu < *o) {
                            oldest = Some((v, lu));
                        }
                    }
                    let remove = oldest.map(|o| o.0.clone()).unwrap_or_default();
                    let prompt = format!(
                        "Maximum number of versions in release channel reached. Would you like to remove version {remove} to make space for version {version}?"
                    );
                    if !policy.should_proceed(cli, &prompt)? {
                        return Err(SnowError::click(
                            "Cannot proceed with publishing the new version. Please remove an existing version from the release channel to make space for the new version, or use --force to automatically clean up unused versions.",
                        ));
                    }
                    cli.console.warning(&format!(
                        "Maximum number of versions in release channel reached. Removing version {remove} from release_channel {ch} to make space for version {version}."
                    ));
                    super::remove_version_from_channel(cli, &name, ch, &remove, role.as_deref())?;
                }
                super::add_version_to_channel(cli, &name, ch, &version, role.as_deref())?;
            }
        }
        super::set_release_directive(
            cli,
            &name,
            directive,
            channel.as_deref(),
            None,
            &version,
            patch,
            role.as_deref(),
        )?;
        Ok(VersionInfo {
            version,
            patch: Some(patch),
            label: None,
        })
    }
}
