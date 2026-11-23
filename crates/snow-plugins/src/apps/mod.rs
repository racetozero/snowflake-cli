//! Snowflake App Runtime (`_plugins/apps`): `app.yml` (version 2) projects and
//! `snowflake-app` entities behind the shared `snow app` commands.

pub mod deploy;
pub mod events;
pub mod manager;

use crate::entities::EntityType;
use snow_api::identifiers::{Fqn, to_identifier, to_string_literal, unquote_identifier};
use snow_api::output::Output;
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};
use std::path::{Path, PathBuf};

const APP_YML: &str = "app.yml";

/// The Snowflake App Runtime flow a shared `snow app` command runs for
/// (`detect` gives `None` for Native Apps).
pub enum Flow {
    /// The validated `AppYmlDefinition`.
    AppYml(PyV),
    /// `snowflake.yml` with `snowflake-app` entities.
    Entity,
}

fn version_of(v: Option<&PyV>) -> Option<f64> {
    match v {
        Some(PyV::Bool(_)) | None => None,
        Some(PyV::Int(i)) => Some(*i as f64),
        Some(PyV::Float(f)) => Some(*f),
        Some(PyV::Str(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// `load_app_yml(project_root)`: the definition when `app.yml` (version 2) drives the project.
pub fn load_app_yml(root: &Path) -> Result<Option<PyV>> {
    let path = root.join(APP_YML);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let raw = match snow_api::project::yaml::safe_load(&text) {
        Ok(v) => v.unwrap_or(PyV::None),
        Err(snow_api::project::yaml::LoadError::Syntax(m)) => {
            return Err(SnowError::cli(format!("Could not parse {APP_YML}: {m}")));
        }
        // Upstream's plain safe_load keeps the last duplicate key; the port rejects it.
        Err(snow_api::project::yaml::LoadError::Duplicate(k)) => {
            return Err(SnowError::cli(format!(
                "Could not parse {APP_YML}: duplicate key {k}"
            )));
        }
    };
    let PyV::Dict(mut d) = raw else {
        return Err(SnowError::cli(format!(
            "{APP_YML} must be a mapping at the top level."
        )));
    };
    let version = version_of(d.get("version"));
    match version {
        None => return Ok(None),
        Some(v) if v < 2.0 => return Ok(None),
        Some(v) if v != 2.0 => {
            return Err(SnowError::cli(format!(
                "Unsupported {APP_YML} version {}: this version of Snowflake CLI supports {APP_YML} version 2 only.",
                d.get("version").map(PyV::py_repr).unwrap_or_default()
            )));
        }
        _ => {}
    }
    d.insert("version".into(), PyV::Int(2));
    let engine = snow_api::project::model::Engine::new(Default::default(), vec![]);
    match engine.model("AppYmlDefinition", &PyV::Dict(d), &mut Vec::new()) {
        Ok(v) => Ok(Some(v)),
        Err(errs) => Err(SnowError::cli(format!(
            "Invalid {APP_YML}: {}",
            snow_api::project::model::validation_error_str("AppYmlDefinition", &errs)
        ))),
    }
}

/// `with_app_flow_routing`: decide the flow; `None` entity checks are the caller's.
pub fn detect(cli: &mut Cli) -> Result<Option<Flow>> {
    if let Some(def) = load_app_yml(&cli.project_root())? {
        return Ok(Some(Flow::AppYml(def)));
    }
    let p = cli.project(false)?.expect("project");
    if !p.meets_version("2") {
        return Ok(None);
    }
    let ents = p.entities();
    let entity_id = cli.str("entity_id").unwrap_or_default();
    let explicit = !cli.str("package_entity_id").unwrap_or_default().is_empty()
        || !cli.str("app_entity_id").unwrap_or_default().is_empty();
    if explicit {
        return Ok(None);
    }
    if !entity_id.is_empty()
        && let Some(e) = ents.get(&entity_id)
    {
        let ty = e.str_of("type").unwrap_or_default().to_lowercase();
        return match EntityType::parse(&ty) {
            Some(EntityType::Application | EntityType::ApplicationPackage) => Ok(None),
            Some(EntityType::SnowflakeApp) => Ok(Some(Flow::Entity)),
            _ => Err(SnowError::click(format!(
                "Entity '{entity_id}' has type '{ty}', which is not supported by 'snow app' commands."
            ))),
        };
    }
    let types: Vec<Option<EntityType>> = ents
        .values()
        .filter_map(|e| e.str_of("type"))
        .map(|t| EntityType::parse(&t.to_lowercase()))
        .collect();
    let native = types.iter().any(|t| {
        matches!(
            t,
            Some(EntityType::Application | EntityType::ApplicationPackage)
        )
    });
    let runtime = types.contains(&Some(EntityType::SnowflakeApp));
    if native && runtime {
        return Err(SnowError::click(
            "Project contains both Native App entities (application / application package) and Snowflake App Runtime entities (snowflake-app). Specify --entity-id (for a Snowflake App Runtime entity) or --package-entity-id / --app-entity-id (for a Native App entity) to select which entity to operate on.",
        ));
    }
    Ok(runtime.then_some(Flow::Entity))
}

// ── app.yml target resolution ─────────────────────────────────────────

/// Service-config fields (`_AppYmlServiceConfig.model_fields`) in pydantic order.
fn service_fields() -> Vec<&'static str> {
    snow_api::project::model::model("AppYmlTarget")
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .collect()
}

fn field(m: &PyV, k: &str) -> PyV {
    m.as_model()
        .and_then(|m| m.fields.get(k).cloned())
        .unwrap_or(PyV::None)
}

fn opt_str(m: &PyV, k: &str) -> Option<String> {
    match field(m, k) {
        PyV::None => None,
        v => Some(v.py_str()),
    }
}

/// `_merge_baseline_and_target(app_def, target)`.
fn merge(def: &PyV, target: Option<&PyV>) -> PyV {
    // Code storage comes whole from the target when it sets either backend.
    let storage_from = match target {
        Some(t) if !field(t, "code_stage").is_none() || !field(t, "code_workspace").is_none() => t,
        _ => def,
    };
    let fields = service_fields()
        .into_iter()
        .map(|name| {
            let v = if name == "code_stage" || name == "code_workspace" {
                field(storage_from, name)
            } else {
                target
                    .map(|t| field(t, name))
                    .filter(|v| !v.is_none())
                    .unwrap_or_else(|| field(def, name))
            };
            (name.to_string(), v)
        })
        .collect();
    PyV::Model(Box::new(snow_api::project::value::Model {
        name: "AppYmlTarget".into(),
        fields,
        set: vec![],
    }))
}

/// `resolve_target(app_def, target_name)`.
fn resolve_target(def: &PyV, name: Option<&str>) -> Result<(Option<String>, PyV)> {
    let targets = match field(def, "targets") {
        PyV::Dict(t) => t,
        _ => Default::default(),
    };
    if targets.is_empty() {
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            return Err(SnowError::cli(format!(
                "Target '{n}' is not defined in {APP_YML}: no targets are declared."
            )));
        }
        return Ok((None, merge(def, None)));
    }
    let resolved = name
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| opt_str(def, "default_target"))
        .filter(|n| !n.is_empty());
    let mut keys: Vec<&String> = targets.keys().collect();
    keys.sort();
    let available = keys
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let Some(resolved) = resolved else {
        return Err(SnowError::cli(format!(
            "No target selected. Pass --target or set 'default_target' in {APP_YML}. Available targets: {available}."
        )));
    };
    let Some(t) = targets.get(&resolved) else {
        return Err(SnowError::cli(format!(
            "Target '{resolved}' is not defined in {APP_YML}. Available targets: {available}."
        )));
    };
    let merged = merge(def, Some(t));
    Ok((Some(resolved), merged))
}

/// `_CodeStorage.kind`.
#[derive(Clone, Copy, PartialEq)]
pub enum StorageKind {
    Stage,
    Workspace,
}

impl StorageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StorageKind::Stage => "stage",
            StorageKind::Workspace => "workspace",
        }
    }
}

/// The code stage encryption when none is configured.
pub const DEFAULT_ENCRYPTION: &str = "SNOWFLAKE_SSE";

/// The resolved code-storage backend (`_CodeStorage`).
pub struct CodeStorage {
    pub kind: StorageKind,
    pub name: String,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub encryption: String,
    pub temporary: bool,
}

impl CodeStorage {
    /// The `<app>_CODE` backend created for one deploy and dropped after the build.
    pub fn temporary(kind: StorageKind, app: &str) -> CodeStorage {
        CodeStorage {
            kind,
            name: format!("{app}_CODE"),
            database: None,
            schema: None,
            encryption: DEFAULT_ENCRYPTION.into(),
            temporary: true,
        }
    }

    /// The storage FQN; an unset database or schema falls back to the app's.
    pub fn fqn(&self, database: &str, schema: &str) -> Fqn {
        app_fqn(
            Some(self.database.as_deref().unwrap_or(database)),
            Some(self.schema.as_deref().unwrap_or(schema)),
            &self.name,
        )
    }
}

pub fn is_personal_database(db: Option<&str>) -> bool {
    db.is_some_and(|d| {
        unquote_identifier(d.trim())
            .to_uppercase()
            .starts_with("USER$")
    })
}

/// `_resolve_code_storage(...)`; refs are (name, database, schema, encryption).
type StorageRef = (String, Option<String>, Option<String>, Option<String>);

fn resolve_code_storage(
    cli: &mut Cli,
    workspace: Option<StorageRef>,
    stage: Option<StorageRef>,
    database: Option<&str>,
    app_name: &str,
) -> CodeStorage {
    use snow_api::sanitize::for_terminal as san;
    if let Some((name, db, sc, _)) = workspace {
        return CodeStorage {
            kind: StorageKind::Workspace,
            name,
            database: db,
            schema: sc,
            encryption: DEFAULT_ENCRYPTION.into(),
            temporary: false,
        };
    }
    if let Some((name, db, sc, enc)) = stage {
        let stage_db = db.clone().or_else(|| database.map(str::to_string));
        if is_personal_database(stage_db.as_deref()) {
            cli.console.warning(&format!(
                "code_stage '{}' resolves to the personal database '{}', which generally does not support stages. Honoring the configured stage; the deploy may fail. Consider a stage in a standard database or a workspace (code_workspace) instead.",
                san(&name),
                san(&stage_db.unwrap_or_else(|| "None".into()))
            ));
        }
        return CodeStorage {
            kind: StorageKind::Stage,
            name,
            database: db,
            schema: sc,
            encryption: enc.unwrap_or_else(|| DEFAULT_ENCRYPTION.into()),
            temporary: false,
        };
    }
    let kind = if is_personal_database(database) {
        StorageKind::Workspace
    } else {
        StorageKind::Stage
    };
    CodeStorage::temporary(kind, app_name)
}

fn yml_storage_ref(v: Option<String>) -> Result<Option<StorageRef>> {
    let Some(v) = v.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let f = Fqn::from_string(&v)?;
    Ok(Some((f.name, f.database, f.schema, None)))
}

/// `app_fqn(database, schema, name)`: each part quoted when needed.
pub fn app_fqn(database: Option<&str>, schema: Option<&str>, name: &str) -> Fqn {
    Fqn {
        database: database.filter(|d| !d.is_empty()).map(to_identifier),
        schema: schema.filter(|s| !s.is_empty()).map(to_identifier),
        name: to_identifier(name),
        signature: None,
    }
}

/// `_AppYmlDeployment`.
pub struct Deployment {
    pub target_name: Option<String>,
    pub package_name: String,
    pub database: String,
    pub schema: String,
    pub service_name: String,
    pub service_fqn: Fqn,
    pub storage: CodeStorage,
    pub storage_fqn: Fqn,
    pub target: PyV,
}

/// `get_personal_database()`: `None` when it fails.
pub(crate) fn personal_database(cli: &mut Cli) -> Option<String> {
    let Ok(c) = cli
        .try_query("SELECT 'USER$' || CURRENT_USER() AS personal_database")
        .ok()?
    else {
        crate::logs::log(
            crate::logs::WARNING,
            "snowflake.cli._plugins.apps.manager",
            "Could not resolve personal database.",
        );
        return None;
    };
    let v = c.rows.first()?.first()?.clone()?;
    (!v.is_empty() && !v.ends_with('$')).then_some(v)
}

/// `_resolve_app_yml_target(app_def, target, manager)`.
pub fn resolve_deployment(cli: &mut Cli, def: &PyV, target: Option<&str>) -> Result<Deployment> {
    let (name, tgt) = resolve_target(def, target)?;
    let mut database = opt_str(&tgt, "database");
    if database
        .as_deref()
        .is_some_and(|d| d.trim().to_uppercase() == "USER$")
    {
        match personal_database(cli) {
            Some(p) => database = Some(p),
            None => {
                return Err(SnowError::cli(
                    "Target requests the personal database (USER$) but it could not be resolved for the current user.",
                ));
            }
        }
    }
    let mut schema = opt_str(&tgt, "schema_");
    let mut service = opt_str(&tgt, "name").unwrap_or_default();
    if !service.is_empty() {
        let f = Fqn::from_string(&service)?;
        database = f.database.or(database);
        schema = f.schema.or(schema);
        service = f.name;
    }
    let wh = opt_str(&tgt, "query_warehouse").filter(|s| !s.is_empty());
    let (db, sc) = (
        database.clone().unwrap_or_default(),
        schema.clone().unwrap_or_default(),
    );
    if service.is_empty() || db.is_empty() || sc.is_empty() || wh.is_none() {
        let missing: Vec<&str> = [
            ("name", !service.is_empty()),
            ("database", !db.is_empty()),
            ("schema", !sc.is_empty()),
            ("query_warehouse", wh.is_some()),
        ]
        .into_iter()
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| n)
        .collect();
        let place = match &name {
            Some(n) => format!("target '{n}'"),
            None => "the app.yml baseline".into(),
        };
        return Err(SnowError::cli(format!(
            "Missing required field(s) in {place}: {}. Set them at the top level of app.yml or on the selected target.",
            missing.join(", ")
        )));
    }
    let package_name = opt_str(&tgt, "package_name")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| service.clone());
    warn_account_mismatch(cli, name.as_deref(), opt_str(&tgt, "account"));
    let storage = resolve_code_storage(
        cli,
        yml_storage_ref(opt_str(&tgt, "code_workspace"))?,
        yml_storage_ref(opt_str(&tgt, "code_stage"))?,
        Some(&db),
        &service,
    );
    let storage_fqn = storage.fqn(&db, &sc);
    Ok(Deployment {
        target_name: name,
        package_name,
        service_fqn: app_fqn(Some(&db), Some(&sc), &service),
        database: db,
        schema: sc,
        service_name: service,
        storage,
        storage_fqn,
        target: tgt,
    })
}

/// `_warn_on_target_account_mismatch(target_name, target_account)`.
fn warn_account_mismatch(cli: &mut Cli, target: Option<&str>, account: Option<String>) {
    let Some(account) = account.filter(|a| !a.is_empty()) else {
        return;
    };
    // `_active_connection_account()`: --account, else the named connection's `account`.
    let active = cli
        .conn
        .get_str("account")
        .filter(|a| !a.is_empty())
        .or_else(|| {
            let name = cli.conn.connection_name.clone()?;
            cli.config
                .connection_dict(&name)
                .ok()?
                .get("account")
                .map(|v| v.py_str())
        });
    let Some(active) = active else {
        return;
    };
    if active.trim().to_lowercase() != account.trim().to_lowercase() {
        let source = match target {
            Some(t) => format!("Target '{t}'"),
            None => "app.yml".into(),
        };
        cli.console.warning(&format!(
            "{source} declares account '{}', but per-target account binding is not yet supported; using the active connection (account '{}').",
            snow_api::sanitize::for_terminal(&account),
            snow_api::sanitize::for_terminal(&active)
        ));
    }
}

/// `_ensure_python_phases_declared(app_def)`.
pub fn ensure_python_phases(cli: &mut Cli, def: &PyV) -> Result<()> {
    let runtime = opt_str(def, "runtime").unwrap_or_default();
    let names: Vec<&str> = runtime.split(',').collect();
    if names.contains(&"python") {
        let run = field(def, "run");
        let has_run = !run.is_none()
            && field(&run, "command")
                .as_list()
                .is_some_and(|l| !l.is_empty());
        if !has_run {
            return Err(SnowError::cli(format!(
                "{APP_YML} has a runtime that includes `python` but does not declare `run`. A Python app gets no default commands: declare `run` in {APP_YML} (`install` and `build` are optional)."
            )));
        }
    }
    if names.contains(&"node") && names.contains(&"python") {
        let undeclared: Vec<String> = ["install", "build"]
            .into_iter()
            .filter(|n| {
                let p = field(def, n);
                p.is_none() || field(&p, "commands").as_list().is_none_or(|l| l.is_empty())
            })
            .map(|n| format!("`{n}`"))
            .collect();
        if !undeclared.is_empty() {
            cli.console.warning(&format!(
                "`runtime` includes node and python, so no npm defaults apply. Declare {} in {APP_YML}, or {}.",
                undeclared.join(" and "),
                if undeclared.len() > 1 { "none will run" } else { "it will not run" }
            ));
        }
    }
    Ok(())
}

// ── bundling ──────────────────────────────────────────────────────────

fn bundle_root(cli: &Cli) -> PathBuf {
    cli.project_root().join("output").join("bundle")
}

/// `perform_bundle(resolved_entity_id, entity)`.
pub fn perform_bundle(cli: &mut Cli, id: &str, artifacts: &[PyV]) -> Result<PathBuf> {
    let root = cli.project_root();
    let bundle = bundle_root(cli);
    if bundle.exists()
        && let Err(e) = std::fs::remove_dir_all(&bundle)
    {
        return Err(SnowError::cli(format!(
            "Could not remove the bundle directory '{}': {}. Bundling cannot continue, because files left in it would be included in the bundle. This is usually a program holding files in it (an editor, terminal, file explorer, antivirus or file-indexing scan) — close it, or delete the directory yourself, then run the command again.",
            snow_api::sanitize::for_terminal(&bundle.to_string_lossy()),
            snow_api::errors::python_os_error(&e, Some(&bundle.to_string_lossy()))
        )));
    }
    std::fs::create_dir_all(&bundle)?;
    cli.console
        .step(&format!("Bundling source files for '{id}'"));
    let resolved_bundle = crate::artifacts::resolve_without_follow(&bundle);
    let mut map = crate::artifacts::BundleMap::new(&root, &bundle);
    for a in artifacts {
        let (src, dest, ignore) = crate::artifacts::mapping_of(a);
        map.add(&src, dest.as_deref(), &ignore)?;
    }
    for (s, d) in map.all_mappings_expanded() {
        let rs = crate::artifacts::resolve_without_follow(&s);
        if rs == resolved_bundle || rs.starts_with(&resolved_bundle) {
            continue;
        }
        if s.is_file() {
            crate::artifacts::symlink_or_copy_tree(&s, &d, &bundle, &root)?;
        }
    }
    Ok(bundle)
}

/// `SnowflakeAppProjectPaths.clean_up_output()`.
fn clean_up_output(cli: &mut Cli, bundle: &Path) {
    if bundle.exists()
        && let Err(e) = std::fs::remove_dir_all(bundle)
    {
        cli.console.warning(&format!(
            "Could not remove the bundle directory '{}': {}. The command itself finished successfully. This is usually a program holding files in it (an editor, terminal, file explorer, antivirus or file-indexing scan); the directory can be deleted at any time.",
            snow_api::sanitize::for_terminal(&bundle.to_string_lossy()),
            snow_api::errors::python_os_error(&e, Some(&bundle.to_string_lossy()))
        ));
        return;
    }
    let root = cli.project_root();
    let mut dir = bundle.parent().map(Path::to_path_buf);
    while let Some(d) = dir {
        if d == root || !d.starts_with(&root) || std::fs::remove_dir(&d).is_err() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
}

/// `AppYml*.bundle_artifacts`: the whole project minus `ignore`.
fn yml_artifacts(target: &PyV) -> Vec<PyV> {
    let ignore = field(target, "ignore");
    let mut m = indexmap::IndexMap::new();
    m.insert("src".to_string(), PyV::s("./*"));
    m.insert("dest".to_string(), PyV::s("./"));
    m.insert(
        "ignore".to_string(),
        if ignore.is_none() {
            PyV::List(vec![])
        } else {
            ignore
        },
    );
    vec![PyV::Dict(m)]
}

fn app_entities(cli: &mut Cli) -> Result<indexmap::IndexMap<String, PyV>> {
    let p = cli.project(false)?.expect("project");
    Ok(p.entities()
        .into_iter()
        .filter(|(_, e)| EntityType::of(e) == Some(EntityType::SnowflakeApp))
        .collect())
}

/// `_resolve_entity_id(entity_id)` + `_get_entity(id)`.
fn entity(cli: &mut Cli) -> Result<(String, PyV)> {
    let apps = app_entities(cli)?;
    let id = match cli.str("entity_id").filter(|s| !s.is_empty()) {
        Some(id) => id,
        None => match apps.len() {
            0 => {
                return Err(SnowError::cli(
                    "No snowflake-app entities found in snowflake.yml. Add a snowflake-app entity or run 'snow app setup' first.",
                ));
            }
            1 => apps.keys().next().cloned().unwrap_or_default(),
            _ => {
                return Err(SnowError::cli(format!(
                    "Multiple snowflake-app entities found: {}. Please specify --entity-id to select one.",
                    apps.keys().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
        },
    };
    match apps.get(&id) {
        Some(e) => Ok((id, e.clone())),
        None => Err(SnowError::cli(format!(
            "Entity '{id}' not found in snowflake.yml."
        ))),
    }
}

/// The entity's `artifacts` list (empty when unset).
pub(crate) fn entity_artifacts(e: &PyV) -> Vec<PyV> {
    e.get("artifacts")
        .and_then(PyV::as_list)
        .cloned()
        .unwrap_or_default()
}

/// `_reject_native_app_options(command, **options)`.
fn reject_native_options(command: &str, options: &[(&str, bool)]) -> Result<()> {
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
        "'snow app {command}' was invoked against a Snowflake App Runtime entity (type: snowflake-app), but the following Native App options were provided: {}. Remove them and try again.",
        set.join(", ")
    )))
}

fn reject_target_without_yml(cli: &Cli) -> Result<()> {
    if cli.str("target").is_some() {
        return Err(SnowError::cli(
            "--target is only supported for Snowflake App Runtime projects that define deployment targets in app.yml (version 2).",
        ));
    }
    Ok(())
}

// ── commands ──────────────────────────────────────────────────────────

/// `snowflake_app_bundle(entity_id)`.
pub fn bundle(cli: &mut Cli, flow: Flow) -> Result<Output> {
    let root = match flow {
        Flow::AppYml(def) => {
            let base = match opt_str(&def, "name") {
                Some(n) => Some(Fqn::from_string(&n)?.name),
                None => None,
            };
            let id = opt_str(&def, "package_name")
                .filter(|s| !s.is_empty())
                .or(base)
                .unwrap_or_else(|| "app".into());
            perform_bundle(cli, &id, &yml_artifacts(&def))?
        }
        Flow::Entity => {
            let (id, e) = entity(cli)?;
            perform_bundle(cli, &id, &entity_artifacts(&e))?
        }
    };
    Ok(Output::message(format!(
        "Bundle generated at {}",
        root.display()
    )))
}

/// `snowflake_app_validate(entity_id, target)`.
pub fn validate(cli: &mut Cli, flow: Flow) -> Result<Output> {
    let target = cli.str("target");
    let (database, schema, id, artifacts) = match flow {
        Flow::AppYml(def) => {
            ensure_python_phases(cli, &def)?;
            let dep = resolve_deployment(cli, &def, target.as_deref())?;
            let arts = yml_artifacts(&dep.target);
            (Some(dep.database), Some(dep.schema), dep.package_name, arts)
        }
        Flow::Entity => {
            reject_target_without_yml(cli)?;
            let (id, e) = entity(cli)?;
            let f = crate::entities::fqn(&e, &id)?;
            let f = if f.database.is_some() {
                cli.using_connection(f)?
            } else {
                f
            };
            (
                f.database.clone(),
                f.schema.clone(),
                id.clone(),
                entity_artifacts(&e),
            )
        }
    };
    if let Some(db) = database.as_deref().filter(|d| !d.is_empty()) {
        if cli
            .execute_query(&format!("SHOW DATABASES LIKE {}", to_string_literal(db)))?
            .rows
            .is_empty()
        {
            return Err(SnowError::cli(format!(
                "Database '{db}' does not exist or is not accessible."
            )));
        }
        if let Some(sc) = schema.as_deref().filter(|s| !s.is_empty()) {
            let q = format!(
                "SHOW SCHEMAS LIKE {} IN DATABASE IDENTIFIER({})",
                to_string_literal(sc),
                to_string_literal(db)
            );
            if cli.execute_query(&q)?.rows.is_empty() {
                return Err(SnowError::cli(format!(
                    "Schema '{db}.{sc}' does not exist or is not accessible."
                )));
            }
        }
    }
    let bundle = perform_bundle(cli, &id, &artifacts);
    if let Ok(b) = &bundle {
        clean_up_output(cli, b);
    }
    bundle?;
    Ok(Output::message("Valid Snowflake App Runtime project."))
}

// ── open / teardown ───────────────────────────────────────────────────

/// `_resolve_deploy_defaults(entity, manager, app_name)`.
pub fn resolve_deploy_defaults(
    cli: &mut Cli,
    id: &str,
    e: &PyV,
) -> Result<indexmap::IndexMap<String, Option<String>>> {
    let raw = crate::entities::fqn(e, id)?;
    let (has_db, has_sc) = (raw.database.is_some(), raw.schema.is_some());
    let f = cli.using_connection(raw)?;
    let app_name = f.name.clone();
    let sub = |k: &str, inner: &str| -> Option<String> {
        e.get_some(k)
            .and_then(|r| r.str_of(inner))
            .filter(|s| !s.is_empty())
    };
    let mut yml: indexmap::IndexMap<&str, Option<String>> = indexmap::IndexMap::new();
    yml.insert("query_warehouse", e.str_of("query_warehouse"));
    yml.insert("build_compute_pool", sub("build_compute_pool", "name"));
    yml.insert("service_compute_pool", sub("service_compute_pool", "name"));
    yml.insert("build_eai", sub("build_eai", "name"));
    yml.insert("service_eai", sub("service_eai", "name"));
    yml.insert("artifact_repository", sub("artifact_repository", "name"));
    yml.insert(
        "artifact_repo_database",
        sub("artifact_repository", "database"),
    );
    yml.insert(
        "artifact_repo_schema",
        sub("artifact_repository", "schema_").or_else(|| sub("artifact_repository", "schema")),
    );
    yml.insert("database", if has_db { f.database.clone() } else { None });
    yml.insert("schema", if has_sc { f.schema.clone() } else { None });
    cli.console
        .step("Fetching Snowflake App Runtime defaults...");
    let params = manager::fetch_app_service_defaults(cli, &app_name)?;
    if !params.is_empty() {
        cli.console.step(&format!(
            "Loaded Snowflake App Runtime defaults: {}",
            params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut defaults: indexmap::IndexMap<&str, String> = indexmap::IndexMap::new();
    defaults.insert("artifact_repository", format!("{app_name}_REPO"));
    cli.console
        .step("Checking whether a personal database exists...");
    if let Some(p) = personal_database(cli) {
        defaults.insert("database", p);
        defaults.insert("schema", "PUBLIC".into());
    }
    // `get_cli_context().connection_context`: command-line connection overrides only.
    let conn = cli.conn.clone();
    let sess = |k: &str| -> Option<String> {
        match k {
            "query_warehouse" => conn.get_str("warehouse"),
            "database" => conn.get_str("database"),
            "schema" => conn.get_str("schema"),
            _ => None,
        }
    };
    let mut out = indexmap::IndexMap::new();
    for k in yml.keys() {
        let v = yml[k]
            .clone()
            .or_else(|| params.get(*k).cloned())
            .or_else(|| defaults.get(k).cloned())
            .or_else(|| sess(k));
        out.insert(k.to_string(), v);
    }
    if out
        .get("artifact_repo_database")
        .is_none_or(|v| v.as_deref().is_none_or(str::is_empty))
    {
        out.insert(
            "artifact_repo_database".into(),
            out.get("database").cloned().flatten(),
        );
    }
    if out
        .get("artifact_repo_schema")
        .is_none_or(|v| v.as_deref().is_none_or(str::is_empty))
    {
        out.insert(
            "artifact_repo_schema".into(),
            out.get("schema").cloned().flatten(),
        );
    }
    Ok(out)
}

/// `_entity_code_storage(entity, database, schema, app_name)`.
fn entity_code_storage(
    cli: &mut Cli,
    e: &PyV,
    database: Option<&str>,
    app_name: &str,
) -> CodeStorage {
    let r = |k: &str| -> Option<StorageRef> {
        let v = e.get_some(k)?;
        Some((
            v.str_of("name").unwrap_or_default(),
            v.str_of("database"),
            v.str_of("schema_").or_else(|| v.str_of("schema")),
            v.str_of("encryption_type"),
        ))
    };
    let (ws, st) = (r("code_workspace"), r("code_stage"));
    resolve_code_storage(cli, ws, st, database, app_name)
}

struct Service {
    fqn: Fqn,
    database: Option<String>,
    schema: Option<String>,
    name: String,
}

/// `_resolve_command_service(entity_id, target)`.
fn command_service(cli: &mut Cli, flow: Flow) -> Result<Service> {
    if let Flow::AppYml(def) = flow {
        let dep = resolve_deployment(cli, &def, cli.str("target").as_deref())?;
        return Ok(Service {
            fqn: dep.service_fqn,
            database: Some(dep.database),
            schema: Some(dep.schema),
            name: dep.service_name,
        });
    }
    reject_target_without_yml(cli)?;
    let (id, e) = entity(cli)?;
    let f = crate::entities::fqn(&e, &id)?;
    let f = cli.using_connection(f)?;
    let (mut db, mut sc) = (f.database.clone(), f.schema.clone());
    if db.is_none() || sc.is_none() {
        let d = manager::fetch_app_service_defaults(cli, &f.name)?;
        db = db.or_else(|| d.get("database").cloned());
        sc = sc.or_else(|| d.get("schema").cloned());
    }
    Ok(Service {
        fqn: app_fqn(db.as_deref(), sc.as_deref(), &f.name),
        database: db,
        schema: sc,
        name: f.name,
    })
}

/// `snowflake_app_open(entity_id, print_only, settings, watch, target)`.
pub fn open(cli: &mut Cli, flow: Flow) -> Result<Output> {
    let svc = command_service(cli, flow)?;
    let missing: Vec<&str> = [("database", &svc.database), ("schema", &svc.schema)]
        .into_iter()
        .filter(|(_, v)| v.as_deref().is_none_or(str::is_empty))
        .map(|(k, _)| k)
        .collect();
    if !missing.is_empty() {
        return Err(SnowError::cli(format!(
            "Cannot resolve {} for the app. Set them in app.yml, snowflake.yml, or your connection configuration.",
            missing.join(" or ")
        )));
    }
    let (db, sc) = (
        svc.database.unwrap_or_default(),
        svc.schema.unwrap_or_default(),
    );
    let settings = cli.flag("settings");
    let url = if settings {
        let id = |s: &str| snow_api::identifiers::identifier_for_url(s);
        let app_id = format!("{}.{}.{}", id(&db), id(&sc), id(&svc.name));
        crate::snowsight::make_url(cli, &format!("#/apps/app-service/{app_id}/details"))?
    } else if cli.flag("watch") {
        deploy::wait_for_service_endpoint(cli, &svc.fqn)?
    } else {
        let desc = match manager::describe_app_service(cli, &svc.fqn)? {
            Ok(d) => d,
            Err(_) => {
                return Err(SnowError::cli(format!(
                    "Could not resolve endpoint URL for service {}. This may indicate missing privileges on the target schema or application service.",
                    svc.fqn.identifier()
                )));
            }
        };
        match manager::url_from_describe(&desc) {
            Some(u) => u,
            None => {
                return Err(SnowError::cli(format!(
                    "No endpoint URL found for service {}. Is the app deployed? Run 'snow app deploy' first.",
                    svc.fqn.identifier()
                )));
            }
        }
    };
    if !settings {
        manager::warn_if_cert_missing(cli, &url);
    }
    if !cli.flag("print_only") {
        snow_connector::auth::open_browser(&url);
    }
    Ok(Output::message(url))
}

/// `snowflake_app_teardown(entity_id, force, target)`.
pub fn teardown(cli: &mut Cli, flow: Flow) -> Result<Output> {
    reject_native_options(
        "teardown",
        &[("--cascade", cli.opt_bool("cascade").is_some())],
    )?;
    let (service_fqn, storage, storage_fqn, code_name) = match flow {
        Flow::AppYml(def) => {
            let dep = resolve_deployment(cli, &def, cli.str("target").as_deref())?;
            (
                dep.service_fqn,
                dep.storage,
                dep.storage_fqn,
                dep.package_name,
            )
        }
        Flow::Entity => {
            reject_target_without_yml(cli)?;
            let (id, e) = entity(cli)?;
            let d = resolve_deploy_defaults(cli, &id, &e)?;
            let db = d
                .get("database")
                .cloned()
                .flatten()
                .filter(|s| !s.is_empty());
            let sc = d.get("schema").cloned().flatten().filter(|s| !s.is_empty());
            let (Some(db), Some(sc)) = (db.clone(), sc.clone()) else {
                let missing: Vec<&str> = [("database", db.is_none()), ("schema", sc.is_none())]
                    .into_iter()
                    .filter(|(_, m)| *m)
                    .map(|(k, _)| k)
                    .collect();
                return Err(SnowError::cli(format!(
                    "Cannot resolve {} for the app. Set them in snowflake.yml or in your connection configuration.",
                    missing.join(" or ")
                )));
            };
            let name = cli.using_connection(crate::entities::fqn(&e, &id)?)?.name;
            let storage = entity_code_storage(cli, &e, Some(&db), &name);
            let sfqn = storage.fqn(&db, &sc);
            (app_fqn(Some(&db), Some(&sc), &name), storage, sfqn, name)
        }
    };
    let ident = service_fqn.identifier();
    if !cli.flag("force") {
        let ok = crate::common::confirm(
            cli,
            &format!(
                "Are you sure you want to drop application service {ident} and its associated objects?"
            ),
            false,
        )?;
        if !ok {
            return Ok(Output::message("Teardown cancelled."));
        }
    }
    cli.console
        .step(&format!("Dropping application service {ident}"));
    cli.execute_query(&format!(
        "DROP APPLICATION SERVICE IF EXISTS {}",
        service_fqn.sql_identifier()
    ))?;
    let still = matches!(manager::describe_app_service(cli, &service_fqn)?, Ok(d) if !d.is_empty());
    if still {
        return Err(SnowError::cli(format!(
            "Failed to drop application service {ident}. Check: DESCRIBE APPLICATION SERVICE {ident}"
        )));
    }
    let sid = storage_fqn.identifier();
    if storage.kind == StorageKind::Workspace {
        if storage.temporary {
            cli.console.step(&format!("Dropping workspace {sid}"));
            cli.execute_query(&format!(
                "DROP WORKSPACE IF EXISTS {}",
                storage_fqn.sql_identifier()
            ))?;
        } else {
            cli.console.step(&format!(
                "Clearing workspace files for {code_name} in {sid}"
            ));
            cli.execute_query(&format!(
                "REMOVE snow://workspace/{sid}/versions/live/{}/",
                code_name.trim_matches('/')
            ))?;
        }
    } else {
        cli.console.step(&format!("Dropping stage {sid}"));
        cli.execute_query(&format!(
            "DROP STAGE IF EXISTS {}",
            storage_fqn.sql_identifier()
        ))?;
    }
    Ok(Output::message(format!(
        "Successfully dropped application service {ident}."
    )))
}
