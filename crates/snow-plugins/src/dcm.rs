//! `snow dcm` (DCM projects): manifest.yml targets and the project-level commands.

use snow_api::identifiers::{Fqn, same_identifiers, to_identifier};
use snow_api::output::Output;
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};

pub struct Target {
    pub name: String,
    pub project_name: String,
    pub account_identifier: String,
    pub project_owner: String,
    pub templating_config: Option<String>,
}

pub struct Manifest {
    pub targets: indexmap::IndexMap<String, Target>,
    pub default_target: Option<String>,
    pub configurations: Vec<String>,
    /// `templating.env_vars` + `templating.env_secrets` names.
    pub env_names: Vec<String>,
    pub run_post_hooks: bool,
    /// `assets:` as (name, paths).
    pub assets: Vec<(String, Vec<String>)>,
}

enum ManifestError {
    NotFound(String),
    Invalid(String),
}

fn s(v: Option<&PyV>) -> String {
    match v {
        None | Some(PyV::None) => String::new(),
        Some(x) => x.py_str(),
    }
}

fn load_manifest(dir: &std::path::Path) -> std::result::Result<Manifest, ManifestError> {
    let f = dir.join("manifest.yml");
    crate::logs::info(
        "snowflake.cli._plugins.dcm.models",
        &format!("Loading DCM manifest from {}.", f.display()),
    );
    if !f.exists() {
        return Err(ManifestError::NotFound(format!(
            "manifest.yml was not found in directory {}.",
            dir.display()
        )));
    }
    let text = std::fs::read_to_string(&f).map_err(|e| ManifestError::Invalid(e.to_string()))?;
    let data = snow_api::project::yaml::safe_load(&text)
        .ok()
        .flatten()
        .filter(PyV::truthy);
    let Some(data) = data else {
        return Err(ManifestError::Invalid(
            "Manifest file is empty or invalid.".into(),
        ));
    };
    let mut targets = indexmap::IndexMap::new();
    if let Some(PyV::Dict(t)) = data.get("targets") {
        for (name, td) in t {
            let po = s(td.get("project_owner"));
            targets.insert(
                name.to_uppercase(),
                Target {
                    name: name.to_uppercase(),
                    project_name: s(td.get("project_name")),
                    account_identifier: s(td.get("account_identifier")),
                    project_owner: if po.is_empty() {
                        po
                    } else {
                        to_identifier(&po)
                    },
                    templating_config: td
                        .get_some("templating_config")
                        .map(|c| c.py_str().to_uppercase()),
                },
            );
        }
    }
    let mut default_target = match data.get("default_target") {
        Some(PyV::Str(d)) => Some(d.to_uppercase()),
        _ => None,
    };
    if data.get("default_target").is_none() && targets.len() == 1 {
        default_target = targets.keys().next().cloned();
    }
    let version = match data.get("manifest_version") {
        None | Some(PyV::None) => {
            return Err(ManifestError::Invalid(
                "Manifest version is undefined.".into(),
            ));
        }
        Some(PyV::Int(i)) => *i,
        Some(v) => v.py_str().trim().parse::<i64>().map_err(|_| {
            ManifestError::Invalid(format!(
                "Manifest version '{}' is not valid. Expected an integer.",
                v.py_str()
            ))
        })?,
    };
    let assets = match data.get("assets") {
        None | Some(PyV::None) => Vec::new(),
        Some(PyV::Dict(d)) => {
            let mut out = Vec::new();
            for (name, spec) in d {
                out.push(asset_from_entry(name, spec)?);
            }
            out
        }
        Some(_) => {
            return Err(ManifestError::Invalid(
                "Manifest 'assets' must be a mapping of asset name to its path(s).".into(),
            ));
        }
    };
    let mut env_names = Vec::new();
    let mut per: Vec<(&str, Vec<String>)> = Vec::new();
    let name_re = regex::Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_]*$").expect("re");
    for section in ["env_vars", "env_secrets"] {
        let label = format!("manifest.yml's 'templating.{section}' section");
        let mut names = Vec::new();
        let entries = data
            .get("templating")
            .and_then(|t| t.get(section))
            .and_then(PyV::as_list)
            .cloned()
            .unwrap_or_default();
        for e in entries {
            let PyV::Dict(d) = &e else {
                return Err(ManifestError::Invalid(format!(
                    "Entry {} in {label} must be a single-key mapping (e.g. `- BUILD_NUMBER:`).",
                    e.py_repr()
                )));
            };
            if d.len() != 1 {
                return Err(ManifestError::Invalid(format!(
                    "Entry {} in {label} must declare exactly one name (e.g. `- BUILD_NUMBER:`), not {}.",
                    e.py_repr(),
                    d.len()
                )));
            }
            for name in d.keys() {
                if name == "_snow" {
                    return Err(ManifestError::Invalid(format!(
                        "'_snow' in {label} is a reserved name and cannot be declared."
                    )));
                }
                if !name_re.is_match(name) {
                    return Err(ManifestError::Invalid(format!(
                        "Variable name '{name}' in {label} is not a valid environment variable name. Must follow the POSIX portable character set: letters, digits, and underscores only, and must not start with a digit."
                    )));
                }
                names.push(name.clone());
            }
        }
        per.push((section, names));
    }
    for (section, names) in &per {
        let mut dups: Vec<&String> = names
            .iter()
            .filter(|n| names.iter().filter(|m| m == n).count() > 1)
            .collect();
        dups.sort();
        dups.dedup();
        if !dups.is_empty() {
            return Err(ManifestError::Invalid(format!(
                "Duplicate name(s) in manifest.yml's 'templating.{section}' section: [{}].",
                dups.iter()
                    .map(|d| format!("'{d}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let mut overlap: Vec<&String> = per[0].1.iter().filter(|n| per[1].1.contains(n)).collect();
    overlap.sort();
    if !overlap.is_empty() {
        return Err(ManifestError::Invalid(format!(
            "Name(s) declared in both 'templating.env_vars' and 'templating.env_secrets' sections of manifest.yml: [{}].",
            overlap
                .iter()
                .map(|d| format!("'{d}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    for (_, n) in per {
        env_names.extend(n);
    }
    let ptype = s(data.get("type")).to_lowercase();
    if ptype.is_empty() {
        return Err(ManifestError::Invalid(
            "Manifest file type is undefined. Expected dcm_project.".into(),
        ));
    }
    if ptype != "dcm_project" {
        return Err(ManifestError::Invalid(format!(
            "Manifest file is defined for type {ptype}. Expected dcm_project."
        )));
    }
    if version != 2 {
        return Err(ManifestError::Invalid(format!(
            "Manifest version {version} is not supported. Expected version 2."
        )));
    }
    let configurations = data
        .get("templating")
        .and_then(|t| t.get("configurations"))
        .and_then(PyV::as_dict)
        .map(|c| c.keys().map(|k| k.to_uppercase()).collect())
        .unwrap_or_default();
    Ok(Manifest {
        targets,
        default_target,
        configurations,
        env_names,
        assets,
        run_post_hooks: matches!(data.get("run_post_hooks"), Some(PyV::Bool(true))),
    })
}

impl Manifest {
    fn target(&self, name: &str) -> std::result::Result<&Target, String> {
        let n = name.to_uppercase();
        let Some(t) = self.targets.get(&n) else {
            return Err(format!("Target '{n}' not found in manifest."));
        };
        if let Some(c) = &t.templating_config
            && !self.configurations.contains(c)
        {
            return Err(format!(
                "Target '{}' references unknown configuration '{c}'.",
                t.name
            ));
        }
        if t.project_name.is_empty() {
            return Err(format!(
                "Target '{}' is missing required field(s): project_name.",
                t.name
            ));
        }
        Ok(t)
    }
    fn effective(&self, name: Option<&str>) -> std::result::Result<&Target, String> {
        match name.or(self.default_target.as_deref()) {
            Some(n) => self.target(n),
            None => Err("No target specified and no default_target defined in manifest.".into()),
        }
    }
}

fn warn_cannot_validate(cli: &mut Cli, thing: &str, reason: &str, guidance: &str) {
    cli.console.warning(&format!(
        "⚠️  Cannot validate target's {thing}: {reason}. {guidance}"
    ));
}

const ACCOUNT_GUIDANCE: &str =
    "The current session account is required to match the manifest target's account_identifier.";
const ROLE_GUIDANCE: &str =
    "The current session role is required to match the manifest target's project_owner.";

fn account_of(s: &str) -> (String, String) {
    let (o, a) = if s.contains('-') && !s.contains('.') {
        s.split_once('-').unwrap_or((s, ""))
    } else if s.contains('.') && !s.contains('-') {
        s.split_once('.').unwrap_or((s, ""))
    } else {
        (s, "")
    };
    (o.to_uppercase(), a.to_uppercase())
}

fn check_account(cli: &mut Cli, t: &Target) -> Result<()> {
    if t.account_identifier.is_empty() {
        warn_cannot_validate(
            cli,
            "account identifier",
            "account_identifier is not specified in the manifest target",
            ACCOUNT_GUIDANCE,
        );
        return Ok(());
    }
    let current = match cli
        .try_query("SELECT CURRENT_ORGANIZATION_NAME() AS org, CURRENT_ACCOUNT_NAME() AS acct")?
    {
        Ok(c) => {
            let (oi, ai) = (c.column_index("ORG"), c.column_index("ACCT"));
            match c.rows.first() {
                None => Err("Could not determine account identifier: CURRENT_ORGANIZATION_NAME() / CURRENT_ACCOUNT_NAME() returned no row.".to_string()),
                Some(r) => {
                    let get = |i: Option<usize>| i.and_then(|i| r.get(i).cloned().flatten());
                    let (org, acct) = (get(oi), get(ai));
                    let repr = |v: &Option<String>| v.as_ref().map(|s| PyV::s(s).py_repr()).unwrap_or_else(|| "None".into());
                    match (&org, &acct) {
                        (Some(o), Some(a)) if !o.is_empty() && !a.is_empty() => Ok((o.to_uppercase(), a.to_uppercase())),
                        _ => Err(format!(
                            "Could not determine account identifier: CURRENT_ORGANIZATION_NAME()={}, CURRENT_ACCOUNT_NAME()={}.",
                            repr(&org),
                            repr(&acct)
                        )),
                    }
                }
            }
        }
        Err(e) => Err(e.msg_with(cli.connector_info)),
    };
    match current {
        Err(e) => warn_cannot_validate(
            cli,
            "account identifier",
            &format!("current account could not be determined: {e}"),
            ACCOUNT_GUIDANCE,
        ),
        Ok(cur) => {
            if cur != account_of(&t.account_identifier) {
                cli.console.warning(&format!(
                    "⚠️  Account mismatch: manifest target specifies account_identifier '{}', but the current session account is '{}-{}'.",
                    snow_api::sql::sanitize_for_terminal(&t.account_identifier),
                    cur.0,
                    cur.1
                ));
            }
        }
    }
    Ok(())
}

fn check_owner(cli: &mut Cli, t: &Target) -> Result<()> {
    if t.project_owner.is_empty() {
        warn_cannot_validate(
            cli,
            "project owner",
            "project_owner is not specified in the manifest target",
            ROLE_GUIDANCE,
        );
        return Ok(());
    }
    match cli.try_query("select current_role()")? {
        Err(e) => warn_cannot_validate(
            cli,
            "project owner",
            &format!(
                "current role could not be determined: {}",
                e.msg_with(cli.connector_info)
            ),
            ROLE_GUIDANCE,
        ),
        Ok(c) => match c
            .rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .filter(|r| !r.is_empty())
        {
            None => warn_cannot_validate(
                cli,
                "project owner",
                "current role could not be determined",
                ROLE_GUIDANCE,
            ),
            Some(role) => {
                if !same_identifiers(&role, &t.project_owner) {
                    cli.console.warning(&format!(
                        "⚠️  Role mismatch: manifest target specifies project_owner '{}', but the current session role is '{}'.",
                        snow_api::sql::sanitize_for_terminal(&t.project_owner),
                        snow_api::sql::sanitize_for_terminal(&role)
                    ));
                }
            }
        },
    }
    Ok(())
}

/// `_resolve_context_with_optional_manifest` → the project identifier.
fn project_id(cli: &mut Cli, required_manifest: bool, validate_owner: bool) -> Result<Fqn> {
    Ok(resolve_with(cli, required_manifest, validate_owner)?.0)
}

/// The project identifier and the effective target's templating configuration.
pub fn resolve(cli: &mut Cli, required_manifest: bool) -> Result<(Fqn, Option<String>)> {
    resolve_with(cli, required_manifest, false)
}

/// `DCMAsset.from_entry(name, spec)`.
fn asset_from_entry(
    name: &str,
    spec: &PyV,
) -> std::result::Result<(String, Vec<String>), ManifestError> {
    let inv = |m: String| ManifestError::Invalid(m);
    if name.is_empty() {
        return Err(inv("Manifest asset is missing a non-empty 'name'.".into()));
    }
    if name.chars().count() > 255 {
        return Err(inv("Asset name exceeds 255 characters.".into()));
    }
    if !regex::Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_]*$")
        .expect("re")
        .is_match(name)
    {
        return Err(inv(format!(
            "Asset name '{name}' is invalid; must match ^[a-zA-Z_][a-zA-Z0-9_]*$."
        )));
    }
    let PyV::Dict(d) = spec else {
        return Err(inv(format!(
            "Asset '{name}' must be a mapping declaring 'path' or 'paths'."
        )));
    };
    let mut unknown: Vec<String> = d
        .keys()
        .filter(|k| *k != "path" && *k != "paths")
        .map(|k| PyV::s(k).py_repr())
        .collect();
    unknown.sort();
    if !unknown.is_empty() {
        return Err(inv(format!(
            "Unknown key(s) in asset '{name}': {}.",
            unknown.join(", ")
        )));
    }
    let (hp, hps) = (d.contains_key("path"), d.contains_key("paths"));
    if hp == hps {
        return Err(inv(format!(
            "Asset '{name}' must declare exactly one of 'path' or 'paths'."
        )));
    }
    let entries: Vec<PyV> = if hp {
        vec![d["path"].clone()]
    } else {
        match &d["paths"] {
            PyV::List(l) if !l.is_empty() => l.clone(),
            _ => {
                return Err(inv(format!(
                    "Asset '{name}' field 'paths' must be a non-empty list."
                )));
            }
        }
    };
    let mut paths = Vec::new();
    for e in entries {
        let entry = match &e {
            PyV::Str(s) if !s.is_empty() => s.clone(),
            _ => return Err(inv("Asset path entries must be non-empty strings.".into())),
        };
        if entry.chars().count() > 1024 {
            return Err(inv(format!("Asset path exceeds 1024 characters: {entry}")));
        }
        if entry.starts_with('/') || entry.starts_with('\\') {
            return Err(inv(format!(
                "Asset path must be relative to the project root (no leading '/'): {entry}"
            )));
        }
        if entry.starts_with('@') || entry.contains("://") {
            return Err(inv(format!(
                "Asset path must be a repo-relative path (no leading '@' or '://'): {entry}"
            )));
        }
        let segs: Vec<&str> = entry.split(['/', '\\']).collect();
        if segs.contains(&"..") {
            return Err(inv(format!(
                "Asset path must stay within the project root (no '..'): {entry}"
            )));
        }
        if segs.contains(&".") {
            return Err(inv(format!(
                "Asset path must not contain a '.' segment (use '**/*' for the whole project): {entry}"
            )));
        }
        if entry.ends_with('/') || entry.ends_with('\\') {
            return Err(inv(format!(
                "Asset path must not end with a path separator: {entry}"
            )));
        }
        for ch in ['?', '{', '}'] {
            if entry.contains(ch) {
                return Err(inv(format!(
                    "Glob metacharacter '{ch}' is not supported: {entry}"
                )));
            }
        }
        let gs: Vec<&str> = entry.split('/').collect();
        for (i, seg) in gs.iter().enumerate() {
            if !seg.contains("**") {
                continue;
            }
            if *seg != "**" {
                return Err(inv(format!(
                    "'**' must be a full path segment (write '**/*'), not '{seg}': {entry}"
                )));
            }
            if i == gs.len() - 1 {
                return Err(inv(format!(
                    "'**' must be followed by another component (write '**/*'): {entry}"
                )));
            }
        }
        paths.push(entry);
    }
    Ok((name.to_string(), paths))
}

/// The declared `assets:` of the manifest in `dir` (empty without one).
pub fn declared_assets(dir: &std::path::Path) -> Vec<(String, Vec<String>)> {
    load_manifest(dir).map(|m| m.assets).unwrap_or_default()
}

/// `context.run_post_hooks`: ENABLE_DCM_POST_HOOKS and `run_post_hooks: true` in the manifest.
pub fn run_post_hooks(cli: &Cli) -> bool {
    if cli.config.feature("ENABLE_DCM_POST_HOOKS", Some(false)) != Some(true) {
        return false;
    }
    let from = cli
        .str("from_location")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    load_manifest(&from).is_ok_and(|m| m.run_post_hooks)
}

/// The declared env var / secret names of the manifest in `--from` (empty without one).
pub fn declared_env_names(cli: &Cli) -> Vec<String> {
    let from = cli
        .str("from_location")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    load_manifest(&from)
        .map(|m| m.env_names)
        .unwrap_or_default()
}

fn resolve_with(
    cli: &mut Cli,
    required_manifest: bool,
    validate_owner: bool,
) -> Result<(Fqn, Option<String>)> {
    let identifier = match cli.str("identifier") {
        Some(_) => Some(crate::object::fqn_arg(cli, "identifier")?),
        None => None,
    };
    let target = cli.str("target");
    let from = cli
        .str("from_location")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    match load_manifest(&from) {
        Ok(m) => {
            let t = m.effective(target.as_deref()).map_err(SnowError::cli)?;
            let id = match identifier {
                Some(i) => i,
                None => Fqn::from_string(&t.project_name)?,
            };
            let t = Target {
                name: t.name.clone(),
                project_name: t.project_name.clone(),
                account_identifier: t.account_identifier.clone(),
                project_owner: t.project_owner.clone(),
                templating_config: t.templating_config.clone(),
            };
            check_account(cli, &t)?;
            if validate_owner {
                check_owner(cli, &t)?;
            }
            Ok((id, t.templating_config.clone()))
        }
        Err(ManifestError::Invalid(m)) => Err(SnowError::cli(m)),
        Err(ManifestError::NotFound(m)) => {
            if required_manifest {
                return Err(SnowError::cli(m));
            }
            let Some(id) = identifier else {
                return Err(SnowError::cli(
                    "No manifest.yml found. Please provide a project identifier or create a manifest.yml file.",
                ));
            };
            if let Some(t) = target {
                return Err(SnowError::cli(format!(
                    "Cannot use --target '{t}' without a valid manifest.yml."
                )));
            }
            Ok((id, None))
        }
    }
}

pub fn create(cli: &mut Cli) -> Result<Output> {
    let id = project_id(cli, false, true)?;
    let id = id_with_conn(cli, id)?;
    let exists = cli
        .try_query(&format!("describe DCM Project {}", id.sql_identifier()))?
        .is_ok();
    let shown = id.identifier();
    if exists {
        let m = format!("DCM Project '{shown}' already exists.");
        if cli.flag("if_not_exists") {
            return Ok(Output::message(m));
        }
        return Err(SnowError::cli(m));
    }
    cli.console
        .phase_start(&format!("Creating DCM Project '{shown}'"));
    let r = cli.execute_query(&format!("CREATE DCM PROJECT {}", id.sql_identifier()));
    cli.console.phase_end(None);
    r?;
    Ok(Output::message(format!(
        "DCM Project '{shown}' successfully created."
    )))
}

fn id_with_conn(_cli: &mut Cli, id: Fqn) -> Result<Fqn> {
    Ok(id)
}

pub fn drop(cli: &mut Cli) -> Result<Output> {
    let id = project_id(cli, false, false)?;
    let ie = if cli.flag("if_exists") {
        "if exists "
    } else {
        ""
    };
    let c = cli.execute_query(&format!("drop DCM Project {ie}{}", id.sql_identifier()))?;
    Ok(cli.query_result(&c))
}

pub fn describe(cli: &mut Cli) -> Result<Output> {
    let id = project_id(cli, false, false)?;
    let c = cli.execute_query(&format!("describe DCM Project {}", id.sql_identifier()))?;
    Ok(cli.query_result(&c))
}

pub fn list_deployments(cli: &mut Cli) -> Result<Output> {
    let id = project_id(cli, false, false)?;
    let c = cli.execute_query(&format!(
        "SHOW DEPLOYMENTS IN DCM PROJECT {}",
        id.sql_identifier()
    ))?;
    Ok(cli.query_result(&c))
}

pub fn drop_deployment(cli: &mut Cli) -> Result<Output> {
    let dep = cli.str("deployment").unwrap_or_default();
    if dep.trim().is_empty() {
        return Err(SnowError::Click(snow_click::ClickError::usage_class(
            "Invalid value for '--deployment': Deployment name or alias cannot be blank.".into(),
            "BadParameter",
        )));
    }
    let id = project_id(cli, false, false)?;
    if dep.to_uppercase() == "DEPLOYMENT" {
        cli.console.warning(&format!(
            "Deployment name '{dep}' might be truncated due to shell expansion. If you meant to use a deployment like 'DEPLOYMENT$1', try using single quotes: 'DEPLOYMENT$1'."
        ));
    }
    let mut q = format!("ALTER DCM PROJECT {} DROP DEPLOYMENT", id.sql_identifier());
    if cli.flag("if_exists") {
        q.push_str(" IF EXISTS");
    }
    q.push_str(&format!(" \"{dep}\""));
    cli.execute_query(&q)?;
    Ok(Output::message(format!(
        "Deployment '{dep}' dropped from DCM Project '{}'.",
        id.identifier()
    )))
}
