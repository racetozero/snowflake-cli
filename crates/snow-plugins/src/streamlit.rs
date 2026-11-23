//! `snow streamlit` (execute, get-url, status, share; deploy/logs elsewhere).

use snow_api::identifiers::{Fqn, to_identifier, to_string_literal, unquote_identifier};
use snow_api::output::{Output, Pv, Row};
use snow_api::{Cli, Result, SnowError};

fn name(cli: &mut Cli) -> Result<Fqn> {
    crate::object::fqn_arg(cli, "name")
}

pub fn execute(cli: &mut Cli) -> Result<Output> {
    let f = name(cli)?;
    cli.execute_query(&format!("EXECUTE STREAMLIT {}()", f.sql_identifier()))?;
    Ok(Output::message(format!(
        "Streamlit {} executed.",
        f.identifier()
    )))
}

pub fn url_for(cli: &mut Cli, f: Fqn) -> Result<String> {
    let f = cli.using_connection(f)?;
    crate::snowsight::make_url(cli, &format!("/#/streamlit-apps/{}", f.url_identifier()))
}

pub fn get_url(cli: &mut Cli) -> Result<Output> {
    let f = name(cli)?;
    let url = url_for(cli, f)?;
    if cli.flag("open_") {
        snow_connector::auth::open_browser(&url);
    }
    Ok(Output::message(url))
}

fn lower_row(c: &snow_connector::Cursor, row: &[Option<String>], cli: &mut Cli) -> Row {
    let tz = cli.timezone();
    let rows = snow_api::values::rows(
        &snow_connector::Cursor {
            rows: vec![row.to_vec()],
            ..c.clone()
        },
        &tz,
        false,
    );
    rows.into_iter()
        .next()
        .map(|r| r.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect())
        .unwrap_or_default()
}

fn split_qualified(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let (mut parts, mut buf, mut inq, mut i) = (Vec::new(), String::new(), false, 0);
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            if inq && chars.get(i + 1) == Some(&'"') {
                buf.push_str("\"\"");
                i += 2;
                continue;
            }
            inq = !inq;
            buf.push(c);
        } else if c == '.' && !inq {
            parts.push(std::mem::take(&mut buf));
        } else {
            buf.push(c);
        }
        i += 1;
    }
    parts.push(buf);
    parts
}

fn names_the_app(managing: &str, app: &Fqn) -> bool {
    if managing.is_empty() {
        return false;
    }
    let key = |s: &str| {
        if s.is_empty() {
            String::new()
        } else {
            unquote_identifier(s)
        }
    };
    let shown: Vec<String> = split_qualified(managing).iter().map(|p| key(p)).collect();
    let expected = [
        key(app.database.as_deref().unwrap_or("")),
        key(app.schema.as_deref().unwrap_or("")),
        key(&app.name),
    ];
    match shown.len() {
        3 => shown == expected,
        2 => shown == expected[1..],
        1 => shown[0] == expected[2],
        _ => false,
    }
}

fn rollup(statuses: Vec<String>) -> Option<String> {
    if statuses.is_empty() {
        return None;
    }
    let mut unique: Vec<String> = Vec::new();
    for s in statuses {
        if !unique.contains(&s) {
            unique.push(s);
        }
    }
    if unique.len() == 1 {
        return unique.pop();
    }
    if unique.iter().all(|s| s.to_uppercase() == "READY") {
        return Some("READY".into());
    }
    let non_ready: Vec<&String> = unique
        .iter()
        .filter(|s| s.to_uppercase() != "READY")
        .collect();
    for p in ["FAILED", "UNKNOWN", "PENDING", "DONE"] {
        if let Some(s) = non_ready.iter().rev().find(|s| s.to_uppercase() == p) {
            return Some((*s).clone());
        }
    }
    Some(non_ready[0].clone())
}

fn summarize(payload: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(v) => {
            let items = match v {
                serde_json::Value::Object(_) => vec![v],
                serde_json::Value::Array(a) => a,
                serde_json::Value::String(s) => return (!s.is_empty()).then_some(s),
                _ => return None,
            };
            let statuses: Vec<String> = items
                .iter()
                .filter_map(|i| {
                    i.get("status")
                        .filter(|s| !s.is_null() && s.as_str() != Some(""))
                })
                .map(|s| {
                    s.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| s.to_string())
                })
                .collect();
            rollup(statuses)
        }
        Err(_) => (!payload.is_empty()).then(|| payload.to_string()),
    }
}

fn is_container_runtime(rt: &str) -> bool {
    let n = rt.trim().to_uppercase();
    n.strip_prefix("SYSTEM$ST_CONTAINER_RUNTIME")
        .is_some_and(|r| {
            r.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        })
}

pub fn status(cli: &mut Cli) -> Result<Output> {
    let app = name(cli)?;
    let app = cli.using_connection(app)?;
    if app.database.is_none() || app.schema.is_none() {
        return Err(SnowError::cli(
            "A database and schema are required. Pass a fully qualified name such as database.schema.app, or set them on the connection.",
        ));
    }
    let c = cli.execute_query(&format!("describe streamlit {}", app.sql_identifier()))?;
    let Some(first) = c.rows.first().cloned() else {
        return Err(SnowError::cli(format!(
            "Could not describe Streamlit app {}. Verify the app exists and you have access.",
            app.identifier()
        )));
    };
    let described = lower_row(&c, &first, cli);
    let runtime = described.get("runtime_name").cloned().unwrap_or(Pv::None);
    let mut row = Row::new();
    row.insert("name".into(), Pv::s(app.identifier()));
    row.insert("runtime_name".into(), runtime.clone());
    row.insert(
        "compute_pool".into(),
        described.get("compute_pool").cloned().unwrap_or(Pv::None),
    );
    row.insert("url".into(), Pv::s(url_for(cli, app.clone())?));
    for k in [
        "service_name",
        "service_status",
        "container_status",
        "current_instances",
        "message",
    ] {
        row.insert(k.into(), Pv::None);
    }
    let rt = match &runtime {
        Pv::None => String::new(),
        other => other.py_str(),
    };
    if !is_container_runtime(&rt) {
        row.insert("service_status".into(), Pv::s("NOT_APPLICABLE"));
        row.insert("message".into(), Pv::s("This runtime has no SPCS service."));
        return Ok(Output::Collection(vec![row]));
    }
    let schema = format!(
        "{}.{}",
        to_identifier(app.database.as_deref().unwrap_or("")),
        to_identifier(app.schema.as_deref().unwrap_or(""))
    );
    let sc = cli.execute_query(&format!(
        "show services like 'STPLATSTREAMLIT%' in schema IDENTIFIER({})",
        to_string_literal(&schema)
    ))?;
    let mut services = Vec::new();
    for r in sc.rows.clone() {
        let lr = lower_row(&sc, &r, cli);
        let val = |k: &str| match lr.get(k) {
            None | Some(Pv::None) => String::new(),
            Some(v) => v.py_str().trim().to_string(),
        };
        if val("managing_object_domain").to_uppercase() == "STREAMLIT"
            && names_the_app(&val("managing_object_name"), &app)
        {
            services.push(lr);
        }
    }
    if services.is_empty() {
        row.insert("service_status".into(), Pv::s("NOT_STARTED"));
        row.insert(
            "message".into(),
            Pv::s("No SPCS service is currently attributed to this app. The service is created when the app runs and can be removed after it is idle."),
        );
        return Ok(Output::Collection(vec![row]));
    }
    let mut out = Vec::new();
    for s in services {
        let mut r = row.clone();
        let sname = s.get("name").cloned().unwrap_or(Pv::None);
        r.insert("service_name".into(), sname.clone());
        r.insert(
            "service_status".into(),
            s.get("status").cloned().unwrap_or(Pv::None),
        );
        r.insert(
            "current_instances".into(),
            s.get("current_instances").cloned().unwrap_or(Pv::None),
        );
        let sn = match &sname {
            Pv::None => String::new(),
            v => v.py_str(),
        };
        if !sn.is_empty() {
            let q = [
                app.database.as_deref().unwrap_or(""),
                app.schema.as_deref().unwrap_or(""),
                &sn,
            ]
            .iter()
            .map(|p| to_identifier(p))
            .collect::<Vec<_>>()
            .join(".");
            match cli.try_query(&format!(
                "CALL SYSTEM$GET_SERVICE_STATUS({})",
                to_string_literal(&q)
            ))? {
                Ok(c) => {
                    let payload = c.rows.first().and_then(|r| r.first().cloned().flatten());
                    if let Some(p) = payload {
                        r.insert(
                            "container_status".into(),
                            summarize(&p).map(Pv::s).unwrap_or(Pv::None),
                        );
                    }
                }
                Err(e)
                    if e.errno == 3001
                        || e.msg().to_lowercase().contains("insufficient privileges") =>
                {
                    r.insert(
                        "message".into(),
                        Pv::s("container_status requires the MONITOR privilege on the service. service_status is from SHOW SERVICES."),
                    );
                }
                Err(e) => return Err(snow_api::connect::db_error_to_cli(&e, cli.connector_info)),
            }
        }
        out.push(r);
    }
    Ok(Output::Collection(out))
}

pub fn share(cli: &mut Cli) -> Result<Output> {
    let f = name(cli)?;
    let role = cli.str("to_role");
    let users = cli.list("to_user");
    if role.is_some() && !users.is_empty() {
        return Err(SnowError::argument(
            "Share with a role or with --to-user, not both.",
        ));
    }
    if role.is_none() && users.is_empty() {
        return Err(SnowError::argument(
            "Name the role to share with, or a user via --to-user.",
        ));
    }
    let wgo = cli.flag("with_grant_option");
    let grantees: Vec<String> = match &role {
        Some(r) => vec![format!("ROLE {}", to_identifier(r.trim()))],
        None => users
            .iter()
            .map(|u| format!("USER {}", to_identifier(u.trim())))
            .collect(),
    };
    let resolved = cli.using_connection(Fqn {
        signature: None,
        ..f.clone()
    })?;
    // `_grants_target`, located before the GRANT: a project file that cannot be read
    // is a reason to warn about later, not a failure of the share.
    let has_project = cli.project_root().join("snowflake.yml").is_file();
    let target: Option<std::result::Result<(std::path::PathBuf, String), String>> =
        if users.is_empty() || !has_project {
            None
        } else {
            match cli.project(false) {
                Err(e) => Some(Err(format!(
                    "snowflake.yml could not be read: {}",
                    e.message()
                ))),
                Ok(None) => None,
                Ok(Some(p)) => {
                    let p = p.clone();
                    let mut found = None;
                    for (id, e) in p.entities() {
                        if e.str_of("type").as_deref() != Some("streamlit") {
                            continue;
                        }
                        let ef = crate::entities::fqn(&e, &id)?;
                        if cli.using_connection(ef)?.identifier().to_uppercase()
                            == resolved.identifier().to_uppercase()
                        {
                            found = Some(id.clone());
                            break;
                        }
                    }
                    found.map(|id| Ok((cli.project_root().join("snowflake.yml"), id)))
                }
            }
        };
    let opt = if wgo { " WITH GRANT OPTION" } else { "" };
    let mut cursors = Vec::new();
    for g in &grantees {
        cursors.push(cli.execute_query(&format!(
            "GRANT USAGE ON STREAMLIT {} TO {g}{opt}",
            f.sql_identifier()
        ))?);
    }
    if cli.flag("grant_location_usage") {
        let mut locations = Vec::new();
        if let Some(db) = &resolved.database {
            let d = to_identifier(db);
            locations.push(("DATABASE", format!("IDENTIFIER({})", to_string_literal(&d))));
            if let Some(s) = &resolved.schema {
                let q = format!("{d}.{}", to_identifier(s));
                locations.push(("SCHEMA", format!("IDENTIFIER({})", to_string_literal(&q))));
            }
        }
        for (ty, ident) in locations {
            for g in &grantees {
                if let Err(e) = cli.try_query(&format!("GRANT USAGE ON {ty} {ident} TO {g}"))? {
                    cli.console.warning(&format!(
                        "Could not grant USAGE on {} to {}: {}",
                        ty.to_lowercase(),
                        snow_api::sql::sanitize_for_terminal(g),
                        snow_api::sql::sanitize_for_terminal(&e.msg_with(true))
                    ));
                }
            }
        }
    } else if !users.is_empty() && resolved.database.is_some() {
        cli.console.warning(&format!(
            "A user may also need USAGE on {} to open the app. Re-run with --grant-location-usage to grant it.",
            snow_api::sql::sanitize_for_terminal(&resolved.prefix())
        ));
    }
    // `_record_user_grants(target, users)`.
    if let Some(t) = target {
        let reason = match t {
            Err(r) => Some(r),
            Ok((path, id)) => {
                let grants = users
                    .iter()
                    .map(|u| crate::streamlit_grants::Grant {
                        privilege: "USAGE".into(),
                        role: None,
                        user: Some(u.clone()),
                        with_grant_option: wgo,
                    })
                    .collect();
                crate::streamlit_grants::add_grants(&path, &id, grants)
            }
        };
        match reason {
            Some(r) => {
                let entries: Vec<String> = users
                    .iter()
                    .map(|u| {
                        format!(
                            "      - privilege: USAGE\n        user: {}",
                            snow_api::sql::sanitize_for_terminal(u)
                        )
                    })
                    .collect();
                cli.console.warning(&format!(
                    "Could not record the share in snowflake.yml, because {}. Add it under the app's grants to keep the share on the next deploy:\n{}",
                    snow_api::sql::sanitize_for_terminal(&r),
                    entries.join("\n")
                ));
            }
            None => cli.console.step("Recorded the share in snowflake.yml."),
        }
    }
    if cursors.len() == 1 {
        return Ok(cli.single_query_result(&cursors[0]));
    }
    Ok(Output::Multiple(
        cursors.iter().map(|c| cli.single_query_result(c)).collect(),
    ))
}

const WAREHOUSE_RUNTIME: &str = "SYSTEM$WAREHOUSE_RUNTIME";

fn covers(art: &snow_api::project::PyV, target: &str, root: &std::path::Path) -> bool {
    let (src, dest, _) = crate::artifacts::mapping_of(art);
    if src.is_empty() {
        return false;
    }
    let t = std::path::Path::new(target);
    if src.contains(['*', '?', '[', ']']) {
        if !crate::artifacts::path_glob(root, &src).contains(&root.join(t)) {
            return false;
        }
        return match dest {
            None => true,
            Some(d) => {
                let dp = std::path::Path::new(d.trim_end_matches('/'));
                if d.ends_with('/') {
                    dp.join(t.file_name().unwrap_or_default()) == t
                } else {
                    dp == t
                }
            }
        };
    }
    let sp = std::path::Path::new(&src);
    let Ok(rel) = t.strip_prefix(sp) else {
        return false;
    };
    let dest_root = match dest {
        None => sp.to_path_buf(),
        Some(d) => {
            let dp = std::path::PathBuf::from(d.trim_end_matches('/'));
            if d.ends_with('/') {
                dp.join(sp.file_name().unwrap_or_default())
            } else {
                dp
            }
        }
    };
    let joined = if rel.as_os_str().is_empty() {
        dest_root
    } else {
        dest_root.join(rel)
    };
    joined == t
}

/// `snow streamlit deploy` (versioned deployment of a new app; replace/legacy via the same guards).
pub fn deploy(cli: &mut Cli) -> Result<Output> {
    let entity_id = cli.str("entity_id");
    if cli.flag("experimental") {
        cli.console.warning("[Deprecation] The --experimental flag is deprecated. Versioned deployment is now the default behavior. This flag will be removed in a future version.");
    }
    let p = cli.project(false)?.expect("project").clone();
    let (id, model) = if !p.meets_version("2") {
        // `convert_project_definition_to_v2` for a v1 `streamlit:` section.
        if p.definition.get_some("streamlit").is_none() {
            return Err(SnowError::cli(format!(
                "No streamlit project definition found in {}",
                cli.project_root().display()
            ))
            .named("NoProjectDefinitionError"));
        }
        let v2 = cli.project_v2_in_memory()?;
        crate::entities::for_operation_in(cli, &v2, entity_id.as_deref(), "streamlit")?
    } else {
        crate::entities::for_operation(cli, entity_id.as_deref(), "streamlit")?
    };
    let (replace, prune, legacy) = (cli.flag("replace"), cli.flag("prune"), cli.flag("legacy"));
    let (map, bundle_root, artifacts) = bundle_inner(cli, &id, &model, None)?;
    if artifacts
        .iter()
        .any(|a| crate::artifacts::mapping_of(a).0 == "snowflake.yml")
    {
        cli.console.warning("Uploading snowflake.yml to the Streamlit stage. Anyone with USAGE on the Streamlit app or read access to its backing stage can retrieve this file. Avoid storing credentials or other secrets in snowflake.yml.");
    }
    let runtime = model.str_of("runtime_name");
    let pool = model.str_of("compute_pool");
    if legacy && let Some(r) = runtime.as_deref().filter(|r| *r != WAREHOUSE_RUNTIME) {
        return Err(SnowError::cli(format!(
            "runtime_name {r} is not compatible with the --legacy flag. A legacy deployment runs on {WAREHOUSE_RUNTIME}. Remove --legacy to use versioned deployment, or remove runtime_name and compute_pool from your snowflake.yml to use legacy deployment."
        )));
    }
    let container = runtime
        .as_deref()
        .is_some_and(|r| r.starts_with("SYSTEM$ST_CONTAINER_RUNTIME"));
    let pool_applies = pool.is_some() && runtime.as_deref() != Some(WAREHOUSE_RUNTIME);
    if pool.is_some() && (!pool_applies || (legacy && runtime.is_none())) {
        cli.console.warning(&format!(
            "compute_pool {} is ignored because runtime_name is {}, which does not run on a compute pool. Remove compute_pool, or set runtime_name to SYSTEM$ST_CONTAINER_RUNTIME_PY3_11.",
            snow_api::sql::sanitize_for_terminal(pool.as_deref().unwrap_or_default()),
            runtime.clone().unwrap_or_else(|| WAREHOUSE_RUNTIME.into())
        ));
    }
    let set_fields: Vec<&str> = [
        "execute_as",
        "app_warehouse",
        "artifact_repositories",
        "idle_auto_shutdown_time_seconds",
        "schedules",
    ]
    .into_iter()
    .filter(|k| {
        model
            .get(k)
            .is_some_and(|v| !v.is_none() && v.as_list().is_none_or(|l| !l.is_empty()))
    })
    .collect();
    let new_create_container =
        |new: bool| container || (new && pool.is_some() && runtime.is_none());
    let validate = |cli: &mut Cli, new: bool| -> Result<()> {
        let _ = cli;
        if legacy && !set_fields.is_empty() {
            return Err(SnowError::cli(format!(
                "{} cannot be set on a --legacy deployment. Remove --legacy to use versioned deployment, or remove them from snowflake.yml.",
                set_fields.join(", ")
            )));
        }
        let c = new_create_container(new);
        let rt_label = match (&runtime, new && pool.is_some()) {
            (Some(r), _) => r.clone(),
            (None, true) => "not set, and compute_pool uses the container default".into(),
            (None, false) => "not set".into(),
        };
        if c && model.str_of("execute_as").as_deref() == Some("RESTRICTED CALLER") {
            return Err(SnowError::cli(format!(
                "execute_as RESTRICTED CALLER only applies to {WAREHOUSE_RUNTIME}, but runtime_name is {rt_label}. Remove execute_as, or set runtime_name to {WAREHOUSE_RUNTIME}."
            )));
        }
        for field in ["artifact_repositories", "schedules"] {
            if model
                .get(field)
                .is_some_and(|v| !v.is_none() && v.as_list().is_none_or(|l| !l.is_empty()))
                && !c
            {
                return Err(SnowError::cli(format!(
                    "{field} only applies to container runtimes, but runtime_name is {rt_label}. Remove {field}, or set runtime_name to SYSTEM$ST_CONTAINER_RUNTIME_PY3_11."
                )));
            }
        }
        Ok(())
    };
    let unnamed_pool = pool.is_some() && runtime.is_none();
    if !(replace && unnamed_pool) {
        validate(cli, !legacy)?;
    }
    let fm = crate::entities::fqn(&model, &id)?;
    let f = cli.using_connection(fm.clone())?;
    cli.console.step("Checking if object exists");
    let describe = format!("DESCRIBE STREAMLIT {};", f.sql_identifier());
    let exists = cli.try_query(&describe)?.is_ok();
    if exists && !replace {
        return Err(SnowError::cli(format!(
            "Streamlit {} already exists. Re-run with --replace to update the existing app.",
            fm.sql_identifier()
        )));
    }
    if legacy {
        if exists {
            let row = describe_row(cli, &describe).unwrap_or_default();
            let existing_legacy = !pv_str(row.get("root_location")).is_empty()
                && pv_str(row.get("live_version_location_uri")).is_empty();
            if !existing_legacy {
                cli.console.warning("Deployment style is changing from versioned to legacy. Your existing files will remain in the versioned stage. If needed, manually copy any additional files to the legacy stage after deployment.");
            }
        }
        return deploy_legacy(cli, &id, &model, &fm, &f, &map, prune, exists);
    }
    if replace && unnamed_pool {
        validate(cli, true)?;
    }
    if model.str_of("app_warehouse").is_some() && new_create_container(true) {
        cli.console.warning(&format!(
            "app_warehouse {} is ignored because runtime_name is {}. Remove app_warehouse, or set runtime_name to {WAREHOUSE_RUNTIME}.",
            snow_api::sql::sanitize_for_terminal(&model.str_of("app_warehouse").unwrap_or_default()),
            runtime.clone().unwrap_or_else(|| "the container default".into())
        ));
    }
    if exists {
        // _is_legacy_deployment()
        let legacy_existing = match cli.try_query(&describe)? {
            Ok(c) => match c.rows.first().cloned() {
                Some(r) => {
                    let row = lower_row(&c, &r, cli);
                    !pv_str(row.get("root_location")).is_empty()
                        && pv_str(row.get("live_version_location_uri")).is_empty()
                }
                None => false,
            },
            Err(_) => false,
        };
        if legacy_existing {
            cli.console.warning("Replacing legacy ROOT_LOCATION deployment with versioned deployment. Files from the old stage location will be copied onto the new versioned stage when possible. Review the new stage if the copy fails.");
        }
        replace_existing(
            cli,
            &model,
            &fm,
            &f,
            &map,
            &bundle_root,
            prune,
            container,
            pool_applies,
        )?;
        let url = url_for(cli, f)?;
        if cli.flag("open_") {
            snow_connector::auth::open_browser(&url);
        }
        return Ok(Output::message(format!(
            "Streamlit successfully deployed and available under {url}"
        )));
    }
    let q = deploy_sql(cli, &model, &f, CreateMode::IfNotExists, None, None)?;
    settings_ddl(cli, &model, &q)?;
    let stage_root = ensure_live_version(cli, &f, &fm, true)?;
    crate::artifacts::sync_vstage(cli, &bundle_root, &map, prune, &stage_root, true)?;
    grant_privileges(cli, &model, &fm)?;
    if let Some(sch) = model.get("schedules").filter(|v| !v.is_none()) {
        warm_up_sync(cli, &f, &sch.clone(), Some(vec![]))?;
    }
    let url = url_for(cli, f)?;
    if cli.flag("open_") {
        snow_connector::auth::open_browser(&url);
    }
    Ok(Output::message(format!(
        "Streamlit successfully deployed and available under {url}"
    )))
}

/// `_sql_object_name`: quote each part of a (possibly dotted) name.
pub fn sql_object_name(name: &str) -> String {
    let re = regex::Regex::new(r#"^(?:"(?:[^"]|"")*"|[^."]+)(?:\.(?:"(?:[^"]|"")*"|[^."]+))*$"#)
        .expect("re");
    if !re.is_match(name) {
        return to_identifier(name);
    }
    regex::Regex::new(r#""(?:[^"]|"")*"|[^."]+"#)
        .expect("re")
        .find_iter(name)
        .map(|m| to_identifier(m.as_str()))
        .collect::<Vec<_>>()
        .join(".")
}

fn describe_row(cli: &mut Cli, sql: &str) -> Result<indexmap::IndexMap<String, Pv>> {
    let c = cli.execute_query(sql)?;
    Ok(match c.rows.first().cloned() {
        Some(r) => lower_row(&c, &r, cli),
        None => indexmap::IndexMap::new(),
    })
}

fn pv_str(v: Option<&Pv>) -> String {
    match v {
        None | Some(Pv::None) => String::new(),
        Some(p) => p.py_str(),
    }
}

fn upper_list(v: Option<&Pv>) -> Vec<String> {
    let mut out: Vec<String> = match v {
        None | Some(Pv::None) => vec![],
        Some(Pv::List(l)) => l.iter().map(|x| x.py_str().to_uppercase()).collect(),
        Some(p) => match serde_json::from_str::<serde_json::Value>(&p.py_str()) {
            Ok(serde_json::Value::Array(a)) => a
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| x.to_string())
                        .to_uppercase()
                })
                .collect(),
            _ => vec![],
        },
    };
    out.sort();
    out
}

fn settings_ddl(cli: &mut Cli, model: &snow_api::project::PyV, sql: &str) -> Result<()> {
    match cli.try_query(sql)? {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = e.msg_with(true).to_lowercase().replace('"', "'");
            let rejected = [
                ("app_warehouse", "warehouse"),
                ("artifact_repositories", "artifact_repositories"),
                (
                    "idle_auto_shutdown_time_seconds",
                    "idle_auto_shutdown_time_seconds",
                ),
            ]
            .into_iter()
            .find(|(k, p)| {
                model.get(k).is_some_and(|v| !v.is_none())
                    && msg.contains(&format!("invalid property '{p}'"))
            });
            match rejected {
                Some((k, p)) if e.errno == 1420 => Err(SnowError::cli(format!(
                    "This account does not accept {} on a Streamlit app, so the CLI cannot apply {k} in snowflake.yml. Remove {k}, or ask your account administrator to enable it.",
                    p.to_uppercase()
                ))),
                _ => Err(snow_api::connect::db_error_to_cli(&e, cli.connector_info)),
            }
        }
    }
}

/// `_deploy_versioned(object_exists=True)` with a live version stage.
#[allow(clippy::too_many_arguments)]
fn replace_existing(
    cli: &mut Cli,
    model: &snow_api::project::PyV,
    fm: &Fqn,
    f: &Fqn,
    map: &crate::artifacts::BundleMap,
    bundle_root: &std::path::Path,
    prune: bool,
    container: bool,
    pool_applies: bool,
) -> Result<()> {
    use snow_api::project::PyV;
    let ident = f.sql_identifier();
    let mut cur = describe_row(cli, &format!("DESCRIBE STREAMLIT {ident};"))?;
    let Some(stage_root) =
        Some(pv_str(cur.get("live_version_location_uri"))).filter(|s| !s.is_empty())
    else {
        if pv_str(cur.get("root_location")).is_empty() {
            return Err(SnowError::cli(
                "The existing Streamlit app has no live version stage. Restore its live version or deploy with a different identifier, then retry. The existing app was not recreated.",
            ));
        }
        let old_root = pv_str(cur.get("root_location"));
        let live_wh = Some(pv_str(cur.get("query_warehouse")));
        let q = deploy_sql(cli, model, f, CreateMode::Replace, None, live_wh)?;
        settings_ddl(cli, model, &q)?;
        let stage_root = ensure_live_version(cli, f, fm, false)?;
        copy_legacy_stage(cli, &old_root, &stage_root);
        crate::artifacts::sync_vstage(cli, bundle_root, map, prune, &stage_root, true)?;
        grant_privileges(cli, model, fm)?;
        if let Some(sch) = model.get("schedules").filter(|v| !v.is_none()) {
            warm_up_sync(cli, f, &sch.clone(), Some(vec![]))?;
        }
        return Ok(());
    };
    let ms = |k: &str| model.str_of(k).filter(|s| !s.is_empty());
    if model
        .get("artifact_repositories")
        .is_some_and(|v| !v.is_none())
        || model
            .get("idle_auto_shutdown_time_seconds")
            .is_some_and(|v| !v.is_none())
        || model.get("schedules").is_some_and(|v| !v.is_none())
    {
        let pat = snow_api::identifiers::identifier_to_show_like_pattern(&f.name);
        let scope = match (&f.database, &f.schema) {
            (Some(d), Some(s)) => format!(" IN SCHEMA {}.{}", to_identifier(d), to_identifier(s)),
            (Some(d), None) => format!(" IN DATABASE {}", to_identifier(d)),
            _ => String::new(),
        };
        let c = cli.execute_query(&format!("SHOW STREAMLITS LIKE {pat}{scope};"))?;
        for r in c.rows.clone() {
            let row = lower_row(&c, &r, cli);
            if pv_str(row.get("name")).to_uppercase() == unquote_identifier(&f.name).to_uppercase()
            {
                for k in [
                    "artifact_repositories",
                    "idle_auto_shutdown_time_seconds",
                    "scheduled_tasks",
                ] {
                    cur.entry(k.to_string())
                        .or_insert(row.get(k).cloned().unwrap_or(Pv::None));
                }
                break;
            }
        }
    }
    let live_container = is_container_runtime(&pv_str(cur.get("runtime_name")));
    let apply_execute_as = |cli: &mut Cli, cur: &indexmap::IndexMap<String, Pv>| -> Result<()> {
        let Some(want) = ms("execute_as") else {
            return Ok(());
        };
        let live = pv_str(cur.get("execute_as"));
        let live = if live.is_empty() {
            "OWNER".to_string()
        } else {
            live
        };
        if live
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase()
            == want
        {
            return Ok(());
        }
        cli.console.step(&format!(
            "Setting EXECUTE AS {want} on {}",
            fm.sql_identifier()
        ));
        cli.execute_query(&format!("ALTER STREAMLIT {ident} EXECUTE AS {want};"))?;
        Ok(())
    };
    if container && !live_container {
        apply_execute_as(cli, &cur)?;
    }
    let main = model.str_of("main_file");
    let bundled = |m: &str| {
        map.to_project_path(std::path::Path::new(m)).is_some() && bundle_root.join(m).is_file()
    };
    if prune && main.is_none() {
        let active = pv_str(cur.get("main_file"));
        if active.is_empty() || !bundled(&active) {
            return Err(SnowError::cli(
                "Cannot prune with main_file unset when the app's current entrypoint is missing from the bundle. Include the existing entrypoint in artifacts or set main_file before pruning.",
            ));
        }
    }
    // get_alter_sql(current=cur)
    let id = |s: &str| s.trim().to_uppercase();
    let mut clauses = Vec::new();
    let mut unset = Vec::new();
    let mut pre_unset = Vec::new();
    if main.as_deref() == Some("") {
        return Err(SnowError::cli(
            "main_file cannot be empty. Set it to the app's entrypoint file.",
        ));
    }
    let main_changed = main
        .as_ref()
        .is_some_and(|m| *m != pv_str(cur.get("main_file")));
    if main_changed {
        clauses.push(format!(
            "MAIN_FILE = {}",
            to_string_literal(main.as_deref().unwrap_or_default())
        ));
    }
    if let Some(wh) = ms("query_warehouse")
        && id(&pv_str(cur.get("query_warehouse"))) != id(&wh)
    {
        clauses.push(format!("QUERY_WAREHOUSE = {}", to_identifier(&wh)));
    }
    for (k, prop) in [("title", "TITLE"), ("comment", "COMMENT")] {
        let want = model.str_of(k).unwrap_or_default().trim().to_string();
        let have = pv_str(cur.get(k)).trim().to_string();
        if !want.is_empty() && have != want {
            clauses.push(format!("{prop} = {}", to_string_literal(&want)));
        } else if !have.is_empty() && want.is_empty() {
            unset.push(prop.to_string());
        }
    }
    let list_of = |k: &str| -> Vec<String> {
        let mut v: Vec<String> = model
            .get(k)
            .and_then(PyV::as_list)
            .map(|l| l.iter().map(|x| x.py_str().to_uppercase()).collect())
            .unwrap_or_default();
        v.sort();
        v
    };
    let (want_eai, have_eai) = (
        list_of("external_access_integrations"),
        upper_list(cur.get("external_access_integrations")),
    );
    if !want_eai.is_empty() && have_eai != want_eai {
        let names: Vec<String> = model
            .get("external_access_integrations")
            .and_then(PyV::as_list)
            .map(|l| l.iter().map(|x| sql_object_name(&x.py_str())).collect())
            .unwrap_or_default();
        clauses.push(format!(
            "external_access_integrations=({})",
            names.join(", ")
        ));
    } else if !have_eai.is_empty() && want_eai.is_empty() {
        unset.push("EXTERNAL_ACCESS_INTEGRATIONS".into());
    }
    let want_sec = model
        .get("secrets")
        .and_then(PyV::as_dict)
        .cloned()
        .unwrap_or_default();
    let have_sec: indexmap::IndexMap<String, String> = match cur.get("external_access_secrets") {
        Some(Pv::Str(s)) => {
            serde_json::from_str::<indexmap::IndexMap<String, String>>(s).unwrap_or_default()
        }
        _ => indexmap::IndexMap::new(),
    };
    let want_sec_s: indexmap::IndexMap<String, String> = want_sec
        .iter()
        .map(|(k, v)| (k.clone(), v.py_str()))
        .collect();
    if !want_sec.is_empty() && have_sec != want_sec_s {
        clauses.push(format!(
            "secrets=({})",
            want_sec
                .iter()
                .map(|(k, v)| format!("{}={}", to_string_literal(k), sql_object_name(&v.py_str())))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else if !have_sec.is_empty() && want_sec.is_empty() {
        unset.push("SECRETS".into());
    }
    let (want_imp, have_imp) = (list_of("imports"), upper_list(cur.get("import_urls")));
    if want_imp != have_imp {
        let imports: Vec<String> = model
            .get("imports")
            .and_then(PyV::as_list)
            .map(|l| l.iter().map(PyV::py_str).collect())
            .unwrap_or_default();
        if !imports.is_empty() {
            clauses.push(format!(
                "IMPORTS = ({})",
                imports
                    .iter()
                    .map(|i| to_string_literal(i))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else if !have_imp.is_empty() {
            clauses.push("IMPORTS = ()".into());
        }
    }
    let rt = model.str_of("runtime_name");
    let live_rt = pv_str(cur.get("runtime_name"));
    if let Some(r) = rt.as_ref().filter(|r| id(&live_rt) != id(r)) {
        clauses.push(format!("RUNTIME_NAME = {}", to_string_literal(r)));
        if !live_rt.is_empty() {
            cli.console.warning(&format!(
                "Moving Streamlit {} from runtime {} to {r}. This changes how the app runs.",
                fm.sql_identifier(),
                snow_api::sql::sanitize_for_terminal(&live_rt)
            ));
        }
    }
    let pool = model.str_of("compute_pool");
    if pool_applies
        && id(&pv_str(cur.get("compute_pool"))) != id(pool.as_deref().unwrap_or_default())
    {
        clauses.push(format!(
            "COMPUTE_POOL = {}",
            to_string_literal(pool.as_deref().unwrap_or_default())
        ));
    } else if !pv_str(cur.get("compute_pool")).trim().is_empty() && !pool_applies {
        cli.console.warning(&format!(
            "COMPUTE_POOL remains set on {} but is ignored for {}.",
            fm.sql_identifier(),
            rt.clone().unwrap_or_else(|| WAREHOUSE_RUNTIME.into())
        ));
    }
    if let Some(aw) = ms("app_warehouse")
        && !container
        && id(&pv_str(cur.get("code_warehouse"))) != id(&aw)
    {
        clauses.push(format!("WAREHOUSE = {}", to_identifier(&aw)));
    }
    let repos = list_of("artifact_repositories");
    if !repos.is_empty() && upper_list(cur.get("artifact_repositories")) != repos {
        let names: Vec<String> = model
            .get("artifact_repositories")
            .and_then(PyV::as_list)
            .map(|l| l.iter().map(|x| sql_object_name(&x.py_str())).collect())
            .unwrap_or_default();
        clauses.push(format!("ARTIFACT_REPOSITORIES = ({})", names.join(", ")));
    }
    if let Some(PyV::Int(idle)) = model.get("idle_auto_shutdown_time_seconds")
        && pv_str(cur.get("idle_auto_shutdown_time_seconds")) != idle.to_string()
    {
        clauses.push(format!("IDLE_AUTO_SHUTDOWN_TIME_SECONDS = {idle}"));
    }
    if model
        .get("artifact_repositories")
        .and_then(PyV::as_list)
        .is_some_and(|l| l.is_empty())
        && !upper_list(cur.get("artifact_repositories")).is_empty()
    {
        if rt.as_deref() == Some(WAREHOUSE_RUNTIME) && live_container {
            pre_unset.push("ARTIFACT_REPOSITORIES".to_string());
        } else {
            unset.push("ARTIFACT_REPOSITORIES".into());
        }
    }
    let mut stmts = Vec::new();
    if !pre_unset.is_empty() {
        stmts.push(format!(
            "ALTER STREAMLIT {ident} UNSET {};",
            pre_unset.join(", ")
        ));
    }
    if !clauses.is_empty() {
        stmts.push(format!(
            "ALTER STREAMLIT {ident} SET\n{};",
            clauses.join("\n")
        ));
    }
    if !unset.is_empty() {
        stmts.push(format!(
            "ALTER STREAMLIT {ident} UNSET {};",
            unset.join(", ")
        ));
    }
    let alter = (!stmts.is_empty()).then(|| stmts.join("\n"));
    let mut deferred = None;
    if main_changed {
        if !bundled(main.as_deref().unwrap_or_default()) {
            return Err(SnowError::cli(
                "The new main_file is not a file in the deployment bundle. Set main_file to the app's entrypoint file before deploying.",
            ));
        }
        deferred = alter;
    } else {
        if let Some(a) = &alter {
            settings_ddl(cli, model, a)?;
        }
        sync_tags(cli, model, f)?;
    }
    if !container {
        apply_execute_as(cli, &cur)?;
    }
    let mut diff = crate::artifacts::sync_vstage(
        cli,
        bundle_root,
        map,
        prune && deferred.is_none(),
        &stage_root,
        true,
    )?;
    if let Some(d) = deferred {
        if let Err(e) = cli.try_query(&d)? {
            if e.errno == 92824 {
                return Err(SnowError::cli(
                    "This account does not support changing main_file on a versioned Streamlit app. Keep the existing main_file or deploy with a different identifier. The existing app was not recreated.",
                ));
            }
            return Err(snow_api::connect::db_error_to_cli(&e, cli.connector_info));
        }
        sync_tags(cli, model, f)?;
        if prune {
            let pd = crate::artifacts::sync_vstage(cli, bundle_root, map, true, &stage_root, true)?;
            if pd.has_changes() {
                diff = pd;
            }
        }
    }
    if live_container && diff.has_changes() {
        cli.console.step(&format!(
            "Restarting Streamlit app {} so the new files take effect",
            snow_api::sql::sanitize_for_terminal(&f.identifier())
        ));
        let verbose = cli.connector_info;
        cli.connection()?
            .execute_bound("CALL SYSTEM$RESTART_STREAMLIT(?);", &[f.identifier()])
            .map_err(|e| snow_api::connect::db_error_to_cli(&e, verbose))?;
    }
    grant_privileges(cli, model, fm)?;
    if let Some(sch) = model.get("schedules").filter(|v| !v.is_none()) {
        let linked = parse_scheduled_tasks(cur.get("scheduled_tasks"));
        warm_up_sync(cli, f, &sch.clone(), linked)?;
    }
    Ok(())
}

fn sync_tags(cli: &mut Cli, model: &snow_api::project::PyV, f: &Fqn) -> Result<()> {
    use snow_api::project::PyV;
    let Some(PyV::List(tags)) = model.get("tags") else {
        return Ok(());
    };
    let db_prefix = f
        .database
        .as_ref()
        .map(|d| format!("{}.", to_identifier(d)))
        .unwrap_or_default();
    let c = cli.execute_query(&format!(
        "SELECT TAG_DATABASE, TAG_SCHEMA, TAG_NAME FROM TABLE({db_prefix}information_schema.tag_references({}, 'STREAMLIT')) WHERE LEVEL = 'STREAMLIT'",
        to_string_literal(&f.identifier())
    ))?;
    let desired: Vec<String> = tags
        .iter()
        .map(|t| t.str_of("name").unwrap_or_default().to_uppercase())
        .collect();
    let to_unset: Vec<String> = c
        .rows
        .iter()
        .filter_map(|r| {
            let g = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            (!desired.contains(&g(2).to_uppercase())).then(|| {
                format!(
                    "{}.{}.{}",
                    to_identifier(&g(0)),
                    to_identifier(&g(1)),
                    to_identifier(&g(2))
                )
            })
        })
        .collect();
    let ident = f.sql_identifier();
    if !to_unset.is_empty() {
        cli.execute_query(&format!(
            "ALTER STREAMLIT {ident} UNSET TAG {};",
            to_unset.join(",")
        ))?;
    }
    if !tags.is_empty() {
        cli.execute_query(&format!(
            "ALTER STREAMLIT {ident} SET TAG {};",
            tags.iter()
                .map(|t| format!(
                    "{}={}",
                    t.str_of("name").unwrap_or_default(),
                    to_string_literal(&t.str_of("value").unwrap_or_default())
                ))
                .collect::<Vec<_>>()
                .join(",")
        ))?;
    }
    Ok(())
}

/// `StreamlitManager.grant_privileges`: make the app's grants match snowflake.yml.
fn grant_privileges(cli: &mut Cli, model: &snow_api::project::PyV, fm: &Fqn) -> Result<()> {
    use snow_api::project::PyV;
    let Some(PyV::List(grants)) = model.get("grants") else {
        return Ok(());
    };
    let ident = fm.sql_identifier();
    let key = |p: &str, role: Option<&str>, user: Option<&str>| {
        let (kind, name) = match role {
            Some(r) if !r.is_empty() => ("ROLE", r),
            _ => ("USER", user.unwrap_or_default()),
        };
        (
            p.to_uppercase(),
            kind.to_string(),
            unquote_identifier(name).to_uppercase(),
        )
    };
    let grantee_sql = |role: Option<&str>, user: Option<&str>| match role {
        Some(r) if !r.is_empty() => format!("ROLE {}", to_identifier(r)),
        _ => format!("USER {}", to_identifier(user.unwrap_or_default())),
    };
    let desired: Vec<(String, Option<String>, Option<String>, bool)> = grants
        .iter()
        .map(|g| {
            (
                g.str_of("privilege").unwrap_or_default(),
                g.str_of("role").filter(|s| !s.is_empty()),
                g.str_of("user").filter(|s| !s.is_empty()),
                g.get("with_grant_option").is_some_and(PyV::truthy),
            )
        })
        .collect();
    let c = match cli.try_query(&format!("SHOW GRANTS ON STREAMLIT {ident}"))? {
        Ok(c) => c,
        Err(e) => {
            return Err(SnowError::cli(format!(
                "Could not list the grants on Streamlit {}, so grants removed from snowflake.yml were not revoked: {}. Check that the current role can run SHOW GRANTS on the app, or remove grants from snowflake.yml to leave the app's grants unmanaged.",
                fm.identifier(),
                snow_api::sql::sanitize_for_terminal(&e.msg_with(true))
            )));
        }
    };
    for r in c.rows.clone() {
        let row = lower_row(&c, &r, cli);
        let privilege = pv_str(row.get("privilege"));
        if privilege.to_uppercase() == "OWNERSHIP" {
            continue;
        }
        let to = pv_str(row.get("granted_to")).to_uppercase();
        let grantee = pv_str(row.get("grantee_name"));
        if privilege.is_empty() || grantee.is_empty() {
            continue;
        }
        let opt = matches!(
            pv_str(row.get("grant_option")).to_uppercase().as_str(),
            "TRUE" | "YES"
        );
        let (role, user) = match to.as_str() {
            "ROLE" => (Some(grantee.as_str()), None),
            "USER" => (None, Some(grantee.as_str())),
            _ => continue,
        };
        let k = key(&privilege, role, user);
        let wanted = desired
            .iter()
            .find(|d| key(&d.0, d.1.as_deref(), d.2.as_deref()) == k);
        let g = grantee_sql(role, user);
        match wanted {
            None => {
                cli.execute_query(&format!("REVOKE {privilege} ON STREAMLIT {ident} FROM {g}"))?;
            }
            Some(w) if opt && !w.3 => {
                cli.execute_query(&format!(
                    "REVOKE GRANT OPTION FOR {privilege} ON STREAMLIT {ident} FROM {g}"
                ))?;
            }
            _ => {}
        }
    }
    for (p, role, user, opt) in &desired {
        let g = grantee_sql(role.as_deref(), user.as_deref());
        let wgo = if *opt { " WITH GRANT OPTION" } else { "" };
        cli.execute_query(&format!("GRANT {p} ON STREAMLIT {ident} TO {g}{wgo}"))?;
    }
    Ok(())
}

/// `StreamlitEntity.bundle()`: implied artifacts, then the bundle map; returns (map, deploy root, artifacts).
fn bundle_inner(
    cli: &mut Cli,
    id: &str,
    model: &snow_api::project::PyV,
    out: Option<&std::path::Path>,
) -> Result<(
    crate::artifacts::BundleMap,
    std::path::PathBuf,
    Vec<snow_api::project::PyV>,
)> {
    use snow_api::project::PyV;
    let root = cli.project_root();
    // bundle(): main file, environment.yml, pages and (optionally) snowflake.yml are implied artifacts.
    let mut artifacts = model
        .get("artifacts")
        .and_then(PyV::as_list)
        .cloned()
        .unwrap_or_default();
    if let Some(main) = model.str_of("main_file")
        && !artifacts.iter().any(|a| covers(a, &main, &root))
    {
        artifacts.insert(0, PyV::Str(main));
    }
    if root.join("environment.yml").exists()
        && !artifacts
            .iter()
            .any(|a| covers(a, "environment.yml", &root))
    {
        artifacts.push(PyV::s("environment.yml"));
    }
    let pages = model.str_of("pages_dir").unwrap_or_else(|| "pages".into());
    if root.join(&pages).exists() && !artifacts.iter().any(|a| covers(a, &pages, &root)) {
        artifacts.push(PyV::Str(pages));
    }
    let include_pd = model
        .get("include_project_definition")
        .is_some_and(PyV::truthy);
    if include_pd
        && root.join("snowflake.yml").exists()
        && !artifacts.iter().any(|a| covers(a, "snowflake.yml", &root))
    {
        artifacts.push(PyV::s("snowflake.yml"));
    }
    let bundle_root = out
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| root.join("output/bundle/streamlit").join(id));
    if bundle_root.exists() {
        std::fs::remove_dir_all(&bundle_root)?;
    }
    std::fs::create_dir_all(&bundle_root)?;
    let map = crate::artifacts::bundle(&root, &bundle_root, &artifacts)?;
    Ok((map, bundle_root, artifacts))
}

/// `action_bundle` for `snow ws bundle`.
pub fn bundle_entity(
    cli: &mut Cli,
    id: &str,
    model: &snow_api::project::PyV,
) -> Result<std::path::PathBuf> {
    Ok(bundle_inner(cli, id, model, None)?.1)
}

/// `_deploy_legacy` for an app that does not exist yet: sync the bundle to
/// `@<stage>/<name>` and `CREATE STREAMLIT ... ROOT_LOCATION`.
#[allow(clippy::too_many_arguments)]
fn deploy_legacy(
    cli: &mut Cli,
    id: &str,
    model: &snow_api::project::PyV,
    fm: &Fqn,
    f: &Fqn,
    map: &crate::artifacts::BundleMap,
    prune: bool,
    exists: bool,
) -> Result<Output> {
    use snow_api::project::PyV;
    let stage = model.str_of("stage").unwrap_or_else(|| "streamlit".into());
    cli.console
        .step(&format!("Uploading artifacts to stage {stage}"));
    let name = match model.get("identifier") {
        Some(PyV::Model(m)) => m.fields.get("name").map(PyV::py_str).unwrap_or_default(),
        Some(PyV::Str(s)) if !s.is_empty() => s.clone(),
        _ => id.to_string(),
    };
    let stage_fqn = cli.using_connection(Fqn::from_string(&stage)?)?;
    let stage_root =
        crate::stage_execute::standard_prefix(&format!("{}/{name}", stage_fqn.identifier()));
    let deploy_root = map.deploy_root.clone();
    crate::artifacts::sync_deploy_root_with_stage(cli, &deploy_root, map, prune, &stage_root)?;
    cli.console.step(&format!(
        "Creating Streamlit object {}",
        fm.sql_identifier()
    ));
    if exists {
        let cur = describe_row(cli, &format!("DESCRIBE STREAMLIT {};", f.sql_identifier()))?;
        if let Some(alter) = legacy_alter_sql(model, f, &cur, &stage_root)? {
            cli.execute_query(&alter)?;
        }
        sync_tags(cli, model, f)?;
    } else {
        let q = deploy_sql(cli, model, f, CreateMode::Plain, Some(&stage_root), None)?;
        cli.execute_query(&q)?;
    }
    grant_privileges(cli, model, fm)?;
    let url = url_for(cli, f.clone())?;
    if cli.flag("open_") {
        snow_connector::auth::open_browser(&url);
    }
    Ok(Output::message(format!(
        "Streamlit successfully deployed and available under {url}"
    )))
}

/// `WarmUpTasks.sync(schedules, linked)`: the scheduled tasks that start the app.
fn warm_up_sync(
    cli: &mut Cli,
    app: &Fqn,
    schedules: &snow_api::project::PyV,
    linked: Option<Vec<String>>,
) -> Result<()> {
    use snow_api::identifiers::{
        append_to_identifier, escape_like_pattern, to_identifier, to_string_literal,
        unquote_identifier,
    };
    use snow_api::project::PyV;
    const SUFFIX: &str = "__STREAMLIT_LIFECYCLE";
    let schema = app
        .schema
        .clone()
        .or_else(|| app.database.as_ref().map(|_| "PUBLIC".to_string()));
    let parts = |db: &Option<String>, sc: &Option<String>, name: &str| -> Vec<String> {
        match db {
            Some(d) => vec![
                d.clone(),
                sc.clone().unwrap_or_else(|| "PUBLIC".into()),
                name.to_string(),
            ],
            None => sc
                .iter()
                .cloned()
                .chain(std::iter::once(name.to_string()))
                .collect(),
        }
    };
    let dotted = |name: &str| {
        parts(&app.database, &schema, name)
            .iter()
            .map(|p| to_identifier(p))
            .collect::<Vec<_>>()
            .join(".")
    };
    let key = |name: &str| {
        parts(&app.database, &schema, name)
            .iter()
            .map(|p| unquote_identifier(p))
            .collect::<Vec<_>>()
    };
    let task_name = |slot: usize| {
        let suffix = if slot > 0 {
            format!("{SUFFIX}_{}", slot + 1)
        } else {
            SUFFIX.to_string()
        };
        append_to_identifier(&to_identifier(&app.name), &suffix)
    };
    let task_fqn = |name: &str| -> String {
        let mut f = Fqn::from_string(name).unwrap_or_else(|_| Fqn::from_string("x").expect("fqn"));
        f = f
            .set_database(app.database.as_deref())
            .set_schema(schema.as_deref());
        f.sql_identifier()
    };
    let shown_of = |name: &str| -> String {
        Fqn::from_string(name)
            .map(|f| {
                f.set_database(app.database.as_deref())
                    .set_schema(schema.as_deref())
                    .identifier()
            })
            .unwrap_or_else(|_| name.to_string())
    };
    let timezone = schedules
        .str_of("timezone")
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "UTC".into());
    let crons: Vec<String> = schedules
        .get("cron_exprs")
        .and_then(PyV::as_list)
        .map(|l| l.iter().map(PyV::py_str).collect())
        .unwrap_or_default();
    let definition = format!(
        "SELECT SYSTEM$START_STREAMLIT({})",
        to_string_literal(&dotted(&app.name))
    );
    let scope_parts = parts(&app.database, &schema, &app.name);
    let scope = if scope_parts.len() > 1 {
        format!(
            " IN SCHEMA {}",
            scope_parts[..scope_parts.len() - 1]
                .iter()
                .map(|p| to_identifier(p))
                .collect::<Vec<_>>()
                .join(".")
        )
    } else {
        String::new()
    };
    let prefix = format!("{}{SUFFIX}", unquote_identifier(&app.name));
    let app_ident = Fqn::from_string(&app.identifier())?
        .set_database(app.database.as_deref())
        .set_schema(schema.as_deref());
    let verbose = cli.connector_info;
    let live: indexmap::IndexMap<String, indexmap::IndexMap<String, Pv>> = match cli.try_query(
        &format!(
            "SHOW TASKS LIKE {}{scope};",
            to_string_literal(&format!("{}%", escape_like_pattern(&prefix)))
        ),
    )? {
        Ok(c) => {
            let mut m = indexmap::IndexMap::new();
            for r in c.rows.clone() {
                let row = lower_row(&c, &r, cli);
                let matches = |k: &str, want: &Option<String>| {
                    want.as_ref()
                        .is_none_or(|w| pv_str(row.get(k)) == unquote_identifier(w))
                };
                if matches("database_name", &app.database) && matches("schema_name", &schema) {
                    m.insert(pv_str(row.get("name")), row);
                }
            }
            m
        }
        Err(e) => {
            return Err(SnowError::cli(format!(
                "Could not list the warm-up tasks of Streamlit {}: {}. The CLI did not change the tasks. Deploy again when the problem is fixed.",
                app_ident.sql_identifier(),
                snow_api::sql::sanitize_for_terminal(&e.msg_with(verbose))
            )));
        }
    };
    let mut created: Vec<String> = Vec::new();
    let mut wanted: Vec<String> = Vec::new();
    let fail = |cli: &mut Cli,
                created: &[String],
                step: &str,
                msg: String,
                errno: Option<i64>|
     -> SnowError {
        let mut undropped = Vec::new();
        for t in created {
            if !matches!(
                cli.try_query(&format!("DROP TASK IF EXISTS {};", task_fqn(t))),
                Ok(Ok(_))
            ) {
                undropped.push(snow_api::sql::sanitize_for_terminal(&shown_of(t)));
            }
        }
        let note = if !undropped.is_empty() {
            format!(
                " The CLI could not drop these tasks, so they may be left behind: {}.",
                undropped.join(", ")
            )
        } else if !created.is_empty() {
            " The CLI dropped the tasks that it replaced, so the app may have no working schedule. Deploy again when the problem is fixed.".to_string()
        } else {
            String::new()
        };
        let cause = if errno == Some(1420) {
            "This account does not have Streamlit lifecycle controls on."
        } else {
            "Schedules need CREATE TASK on the schema, and EXECUTE TASK and EXECUTE MANAGED TASK on the account."
        };
        SnowError::cli(format!(
            "Could not {step} for Streamlit {}: {}. {cause}{note}",
            app_ident.sql_identifier(),
            snow_api::sql::sanitize_for_terminal(&msg)
        ))
    };
    for (slot, cron) in crons.iter().enumerate() {
        let name = task_name(slot);
        wanted.push(name.clone());
        let schedule = format!("USING CRON {cron} {timezone}");
        let current = live.get(&unquote_identifier(&name));
        let same = current.is_some_and(|c| {
            pv_str(c.get("schedule")) == schedule && pv_str(c.get("definition")) == definition
        });
        if !same {
            let shown = Fqn::from_string(&name)?
                .set_database(app.database.as_deref())
                .set_schema(schema.as_deref())
                .identifier();
            cli.console.step(&format!(
                "Creating warm-up task {}",
                snow_api::sql::sanitize_for_terminal(&shown)
            ));
            let q = format!(
                "CREATE OR REPLACE TASK {}\nSCHEDULE = {}\nAS {definition};",
                task_fqn(&name),
                to_string_literal(&schedule)
            );
            if let Err(e) = cli.try_query(&q)? {
                return Err(fail(
                    cli,
                    &created,
                    "create a warm-up task",
                    e.msg_with(verbose),
                    Some(e.errno),
                ));
            }
            created.push(name.clone());
        }
        if (current.is_none()
            || !same
            || pv_str(current.and_then(|c| c.get("state"))).to_lowercase() != "started")
            && let Err(e) = cli.try_query(&format!("ALTER TASK {} RESUME;", task_fqn(&name)))?
        {
            return Err(fail(
                cli,
                &created,
                "resume a warm-up task",
                e.msg_with(verbose),
                Some(e.errno),
            ));
        }
    }
    let mut undropped = Vec::new();
    for slot in wanted.len()..5 {
        let name = task_name(slot);
        if live.contains_key(&unquote_identifier(&name))
            && !matches!(
                cli.try_query(&format!("DROP TASK IF EXISTS {};", task_fqn(&name))),
                Ok(Ok(_))
            )
        {
            let shown = Fqn::from_string(&name)?
                .set_database(app.database.as_deref())
                .set_schema(schema.as_deref())
                .identifier();
            undropped.push(shown);
        }
    }
    if !undropped.is_empty() {
        return Err(fail(
            cli,
            &created,
            "drop the warm-up tasks no longer in snowflake.yml",
            undropped.join(", "),
            None,
        ));
    }
    if let Some(l) = &linked {
        let mut a: Vec<Vec<String>> = l
            .iter()
            .map(|n| {
                Fqn::from_string(n)
                    .map(|f| {
                        parts(&f.database, &f.schema, &f.name)
                            .iter()
                            .map(|p| unquote_identifier(p))
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        let mut b: Vec<Vec<String>> = wanted.iter().map(|n| key(n)).collect();
        a.sort();
        b.sort();
        if a == b {
            return Ok(());
        }
    }
    let q = if wanted.is_empty() {
        format!(
            "ALTER STREAMLIT {} UNSET SCHEDULED_TASKS;",
            app_ident.sql_identifier()
        )
    } else {
        format!(
            "ALTER STREAMLIT {} SET SCHEDULED_TASKS = ({});",
            app_ident.sql_identifier(),
            wanted
                .iter()
                .map(|n| dotted(n))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    if let Err(e) = cli.try_query(&q)? {
        return Err(fail(
            cli,
            &created,
            "link the warm-up tasks to the app",
            e.msg_with(verbose),
            Some(e.errno),
        ));
    }
    Ok(())
}

/// `parse_scheduled_tasks(value)`: `None` when the value is unknown.
fn parse_scheduled_tasks(v: Option<&Pv>) -> Option<Vec<String>> {
    match v {
        None | Some(Pv::None) => None,
        Some(Pv::List(l)) => Some(l.iter().map(Pv::py_str).collect()),
        Some(other) => serde_json::from_str::<Vec<serde_json::Value>>(&other.py_str())
            .ok()
            .map(|l| {
                l.iter()
                    .map(|x| {
                        x.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| x.to_string())
                    })
                    .collect()
            }),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CreateMode {
    Plain,
    IfNotExists,
    Replace,
}

/// `get_deploy_sql(...)`: `root_location` is the legacy ROOT_LOCATION stage.
fn deploy_sql(
    cli: &mut Cli,
    model: &snow_api::project::PyV,
    f: &Fqn,
    mode: CreateMode,
    root_location: Option<&str>,
    live_wh: Option<String>,
) -> Result<String> {
    deploy_sql_from(cli, model, f, mode, root_location, None, live_wh)
}

fn deploy_sql_from(
    cli: &mut Cli,
    model: &snow_api::project::PyV,
    f: &Fqn,
    mode: CreateMode,
    root_location: Option<&str>,
    artifacts_dir: Option<&str>,
    live_wh: Option<String>,
) -> Result<String> {
    use snow_api::identifiers::{to_identifier, to_string_literal};
    use snow_api::project::PyV;
    let strs = |k: &str| -> Vec<String> {
        model
            .get(k)
            .and_then(PyV::as_list)
            .map(|l| l.iter().map(PyV::py_str).collect())
            .unwrap_or_default()
    };
    let mut q = match mode {
        CreateMode::Replace => "CREATE OR REPLACE STREAMLIT",
        CreateMode::IfNotExists => "CREATE STREAMLIT IF NOT EXISTS",
        CreateMode::Plain => "CREATE STREAMLIT",
    }
    .to_string();
    q.push_str(&format!(" {}", f.sql_identifier()));
    if let Some(r) = root_location {
        q.push_str(&format!("\nROOT_LOCATION = {}", to_string_literal(r)));
    } else if let Some(d) = artifacts_dir {
        q.push_str(&format!("\nFROM {}", to_string_literal(d)));
    }
    q.push_str(&format!(
        "\nMAIN_FILE = {}",
        to_string_literal(&model.str_of("main_file").unwrap_or_default())
    ));
    let imports = strs("imports");
    if !imports.is_empty() {
        q.push_str(&format!(
            "\nIMPORTS = ({})",
            imports
                .iter()
                .map(|i| to_string_literal(i))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let session_wh = cli.connection()?.session.warehouse.clone();
    let wh = model
        .str_of("query_warehouse")
        .filter(|s| !s.is_empty())
        .or(live_wh.filter(|w| !w.trim().is_empty()))
        .or(session_wh.filter(|w| !w.trim().is_empty()));
    let Some(wh) = wh else {
        return Err(SnowError::cli(
            "query_warehouse is not set in snowflake.yml and the current connection has no warehouse. Set query_warehouse on the Streamlit entity, or configure a warehouse on the connection.",
        ));
    };
    q.push_str(&format!("\nQUERY_WAREHOUSE = {}", to_identifier(&wh)));
    if let Some(t) = model.str_of("title").filter(|s| !s.is_empty()) {
        q.push_str(&format!("\nTITLE = {}", to_string_literal(&t)));
    }
    if let Some(c) = model.str_of("comment").filter(|s| !s.is_empty()) {
        q.push_str(&format!("\nCOMMENT = {}", to_string_literal(&c)));
    }
    let eais = strs("external_access_integrations");
    if !eais.is_empty() {
        q.push_str(&format!(
            "\nexternal_access_integrations=({})",
            eais.iter()
                .map(|e| sql_object_name(e))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(PyV::Dict(sec)) = model.get("secrets").filter(|s| s.truthy()) {
        q.push_str(&format!(
            "\nsecrets=({})",
            sec.iter()
                .map(|(k, v)| format!("{}={}", to_string_literal(k), sql_object_name(&v.py_str())))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let runtime = model.str_of("runtime_name");
    let pool = model.str_of("compute_pool");
    let container = runtime
        .as_deref()
        .is_some_and(|r| r.starts_with("SYSTEM$ST_CONTAINER_RUNTIME"));
    let treat_container =
        container || (mode != CreateMode::Replace && pool.is_some() && runtime.is_none());
    if root_location.is_some() {
        q.push_str(&format!(
            "\nRUNTIME_NAME = {}",
            to_string_literal(WAREHOUSE_RUNTIME)
        ));
    } else {
        if let Some(r) = &runtime {
            q.push_str(&format!("\nRUNTIME_NAME = {}", to_string_literal(r)));
        }
        if pool.is_some() && runtime.as_deref() != Some(WAREHOUSE_RUNTIME) {
            q.push_str(&format!(
                "\nCOMPUTE_POOL = {}",
                to_string_literal(pool.as_deref().unwrap_or_default())
            ));
        }
        if let Some(aw) = model.str_of("app_warehouse").filter(|_| !treat_container) {
            q.push_str(&format!("\nWAREHOUSE = {}", to_identifier(&aw)));
        }
        let repos = strs("artifact_repositories");
        if !repos.is_empty() {
            q.push_str(&format!(
                "\nARTIFACT_REPOSITORIES = ({})",
                repos
                    .iter()
                    .map(|r| sql_object_name(r))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(PyV::Int(idle)) = model.get("idle_auto_shutdown_time_seconds") {
            q.push_str(&format!("\nIDLE_AUTO_SHUTDOWN_TIME_SECONDS = {idle}"));
        }
    }
    if let Some(PyV::List(tags)) = model.get("tags").filter(|t| t.truthy()) {
        q.push_str(&format!(
            "\nWITH TAG ({})",
            tags.iter()
                .map(|t| format!(
                    "{}={}",
                    t.str_of("name").unwrap_or_default(),
                    to_string_literal(&t.str_of("value").unwrap_or_default())
                ))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if root_location.is_none()
        && !treat_container
        && model.str_of("execute_as").as_deref() == Some("RESTRICTED CALLER")
    {
        q.push_str("\nEXECUTE AS RESTRICTED CALLER");
    }
    q.push(';');
    Ok(q)
}

/// `_ensure_live_version_location_uri()`; `map_missing` turns 2003 into the "was not created" error.
fn ensure_live_version(cli: &mut Cli, f: &Fqn, fm: &Fqn, map_missing: bool) -> Result<String> {
    use snow_api::identifiers::to_identifier;
    let quals: Vec<String> = match (&f.database, &f.schema) {
        (Some(d), s) => vec![d.clone(), s.clone().unwrap_or_else(|| "PUBLIC".into())],
        (None, Some(s)) => vec![s.clone()],
        (None, None) => vec![],
    };
    let parts: Vec<String> = quals
        .iter()
        .chain(std::iter::once(&f.name))
        .map(|p| to_identifier(p))
        .collect();
    let missing = |cli: &Cli, e: &snow_connector::DbError| {
        if map_missing && e.errno == 2003 {
            SnowError::cli(format!(
                "Streamlit {} was not created, or the current role cannot access it. It may already exist and be owned by another role. Deploy with a role that owns the app, or choose a different identifier.",
                fm.sql_identifier()
            ))
        } else {
            snow_api::connect::db_error_to_cli(e, cli.connector_info)
        }
    };
    if let Err(e) = cli.try_query(&format!(
        "ALTER STREAMLIT {} ADD LIVE VERSION FROM LAST;",
        parts.join(".")
    ))? {
        let m = e.msg_with(true).to_lowercase();
        if !(m.contains("live version") && m.contains("already exists")) {
            return Err(missing(cli, &e));
        }
    }
    let stage_root = match cli.try_query(&format!("DESCRIBE STREAMLIT {};", f.sql_identifier()))? {
        Ok(c) => {
            let idx = c
                .columns
                .iter()
                .position(|col| col.name.to_lowercase() == "live_version_location_uri");
            idx.and_then(|i| c.rows.first().and_then(|r| r.get(i).cloned().flatten()))
                .filter(|s| !s.is_empty())
        }
        Err(e) => return Err(missing(cli, &e)),
    };
    stage_root.ok_or_else(|| {
        SnowError::cli(format!(
            "Snowflake did not report a versioned stage location for Streamlit {}, so the app files could not be uploaded (DESCRIBE STREAMLIT returned an empty live_version_location_uri). Please re-run with --legacy to deploy using the legacy ROOT_LOCATION layout, or check with your account administrator that versioned Streamlit apps are enabled for this account.",
            fm.identifier()
        ))
    })
}

/// `_copy_legacy_stage_if_present(old_root, new_stage_root)`: best effort.
fn copy_legacy_stage(cli: &mut Cli, old_root: &str, new_root: &str) {
    use snow_api::sql::sanitize_for_terminal as san;
    let source = snow_api::stage_path::StagePath::from_stage_str(old_root).path_for_sql();
    let dest = format!("{}/", new_root.trim_end_matches('/'));
    let q = format!(
        "COPY FILES INTO {} FROM {source}",
        snow_api::identifiers::to_string_literal(&dest)
    );
    let verbose = cli.connector_info;
    match cli.try_query(&q) {
        Ok(Ok(_)) => cli.console.warning(&format!("Copied files from legacy stage {} to {}.", san(old_root), san(new_root))),
        Ok(Err(e)) => cli.console.warning(&format!(
            "Could not copy files from legacy stage {}: {}. Files listed in artifacts will still be uploaded.",
            san(old_root),
            san(&e.msg_with(verbose))
        )),
        Err(e) => cli.console.warning(&format!(
            "Could not copy files from legacy stage {}: {}. Files listed in artifacts will still be uploaded.",
            san(old_root),
            san(&e.message())
        )),
    }
}

/// `get_alter_sql(from_stage_name=root, legacy=True, current=cur)`.
fn legacy_alter_sql(
    model: &snow_api::project::PyV,
    f: &Fqn,
    cur: &indexmap::IndexMap<String, Pv>,
    root: &str,
) -> Result<Option<String>> {
    use snow_api::identifiers::{to_identifier, to_string_literal};
    use snow_api::project::PyV;
    let id = |s: &str| s.trim().to_uppercase();
    let mut clauses = vec![format!("ROOT_LOCATION = {}", to_string_literal(root))];
    let mut unset: Vec<String> = Vec::new();
    let main = model.str_of("main_file");
    if main.as_deref() == Some("") {
        return Err(SnowError::cli(
            "main_file cannot be empty. Set it to the app's entrypoint file.",
        ));
    }
    if let Some(m) = main
        .as_ref()
        .filter(|m| **m != pv_str(cur.get("main_file")))
    {
        clauses.push(format!("MAIN_FILE = {}", to_string_literal(m)));
    }
    if let Some(wh) = model.str_of("query_warehouse").filter(|s| !s.is_empty())
        && id(&pv_str(cur.get("query_warehouse"))) != id(&wh)
    {
        clauses.push(format!("QUERY_WAREHOUSE = {}", to_identifier(&wh)));
    }
    for (k, prop) in [("title", "TITLE"), ("comment", "COMMENT")] {
        let want = model.str_of(k).unwrap_or_default().trim().to_string();
        let have = pv_str(cur.get(k)).trim().to_string();
        if !want.is_empty() && have != want {
            clauses.push(format!("{prop} = {}", to_string_literal(&want)));
        } else if !have.is_empty() && want.is_empty() {
            unset.push(prop.to_string());
        }
    }
    let raw = |k: &str| -> Vec<String> {
        model
            .get(k)
            .and_then(PyV::as_list)
            .map(|l| l.iter().map(PyV::py_str).collect())
            .unwrap_or_default()
    };
    let upper_sorted = |v: &[String]| {
        let mut v: Vec<String> = v.iter().map(|x| x.to_uppercase()).collect();
        v.sort();
        v
    };
    let eais = raw("external_access_integrations");
    let have_eai = upper_list(cur.get("external_access_integrations"));
    if !eais.is_empty() && have_eai != upper_sorted(&eais) {
        clauses.push(format!(
            "external_access_integrations=({})",
            eais.iter()
                .map(|e| sql_object_name(e))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else if !have_eai.is_empty() && eais.is_empty() {
        unset.push("EXTERNAL_ACCESS_INTEGRATIONS".into());
    }
    let imports = raw("imports");
    let have_imp = upper_list(cur.get("import_urls"));
    if upper_sorted(&imports) != have_imp {
        if !imports.is_empty() {
            clauses.push(format!(
                "IMPORTS = ({})",
                imports
                    .iter()
                    .map(|i| to_string_literal(i))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else if !have_imp.is_empty() {
            clauses.push("IMPORTS = ()".into());
        }
    }
    if id(&pv_str(cur.get("runtime_name"))) != id(WAREHOUSE_RUNTIME) {
        clauses.push(format!(
            "RUNTIME_NAME = {}",
            to_string_literal(WAREHOUSE_RUNTIME)
        ));
    }
    let ident = f.sql_identifier();
    let mut stmts = vec![format!(
        "ALTER STREAMLIT {ident} SET\n{};",
        clauses.join("\n")
    )];
    if !unset.is_empty() {
        stmts.push(format!(
            "ALTER STREAMLIT {ident} UNSET {};",
            unset.join(", ")
        ));
    }
    Ok(Some(stmts.join("\n")))
}

/// `StreamlitEntity` as an application package child: `validate_as_child`, `bundle(dir)`,
/// `get_deploy_sql(artifacts_dir=rel, schema=schema, replace=True)`.
pub fn child_deploy_sql(
    cli: &mut Cli,
    id: &str,
    model: &snow_api::project::PyV,
    dir: &std::path::Path,
    rel: &str,
    schema: Option<&str>,
) -> Result<String> {
    let set: Vec<&str> = [
        "execute_as",
        "app_warehouse",
        "artifact_repositories",
        "idle_auto_shutdown_time_seconds",
        "schedules",
    ]
    .into_iter()
    .filter(|k| {
        model
            .get(k)
            .is_some_and(|v| !v.is_none() && v.as_list().is_none_or(|l| !l.is_empty()))
    })
    .collect();
    if !set.is_empty() {
        return Err(SnowError::cli(format!(
            "{} cannot be set on a Streamlit app in an application package. Remove them from entity {id}.",
            set.join(", ")
        )));
    }
    bundle_inner(cli, id, model, Some(dir))?;
    let fm = crate::entities::fqn(model, id)?;
    let conn = cli.connection()?;
    let (cs, cd) = (conn.session.schema.clone(), conn.session.database.clone());
    let f = fm
        .clone()
        .set_schema(
            schema
                .map(str::to_string)
                .or(fm.schema.clone())
                .or(cs)
                .as_deref(),
        )
        .set_database(fm.database.clone().or(cd).as_deref());
    deploy_sql_from(cli, model, &f, CreateMode::Replace, None, Some(rel), None)
}
