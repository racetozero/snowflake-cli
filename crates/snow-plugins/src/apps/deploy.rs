//! `snow app deploy` for Snowflake App Runtime projects: upload → build → deploy
//! (`snowflake_app_deploy`, `_deploy_from_app_yml`, `_upload_and_build_app`).

use super::manager::{self, PER_ACCOUNT_CERT_ISSUE_FUNCTION};
use super::{CodeStorage, Flow, app_fqn, is_personal_database};
use indexmap::IndexMap;
use snow_api::identifiers::{
    Fqn, identifier_to_show_like_pattern, to_identifier, to_string_literal, unquote_identifier,
};
use snow_api::output::{Output, Pv};
use snow_api::project::PyV;
use snow_api::sql::sanitize_for_terminal as san;
use snow_api::{Cli, Result, SnowError};
use snow_connector::DbError;
use std::path::{Path, PathBuf};

fn ts() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn db_err(cli: &Cli, e: &DbError) -> SnowError {
    snow_api::connect::db_error_to_cli(e, cli.connector_info)
}

/// `str(ProgrammingError)`.
fn err_str(e: &DbError) -> String {
    e.msg_with(true)
}

fn run(cli: &mut Cli, sql: &str) -> Result<std::result::Result<snow_connector::Cursor, DbError>> {
    cli.try_query(sql)
}

// ── upload errors (`upload_errors.py`) ────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    PrepareStage,
    PrepareWorkspace,
    PushStage,
    PushWorkspace,
}

impl Phase {
    fn is_push(self) -> bool {
        matches!(self, Phase::PushStage | Phase::PushWorkspace)
    }
    fn kind(self) -> &'static str {
        if matches!(self, Phase::PrepareWorkspace | Phase::PushWorkspace) {
            "workspace"
        } else {
            "stage"
        }
    }
    fn value(self) -> &'static str {
        match self {
            Phase::PrepareStage => "prepare_stage",
            Phase::PrepareWorkspace => "prepare_workspace",
            Phase::PushStage => "push_stage_files",
            Phase::PushWorkspace => "push_workspace_files",
        }
    }
}

#[derive(Default)]
struct UploadCtx<'a> {
    action: Option<&'a str>,
    privilege: Option<&'a str>,
    role: Option<String>,
    database: Option<String>,
    schema: Option<String>,
    encryption: Option<String>,
    source_file: Option<String>,
    files_uploaded: usize,
}

/// `classify_upload_error(exc, phase=..., target=..., ...)`.
fn classify(e: &DbError, phase: Phase, target: &str, c: &UploadCtx) -> SnowError {
    let code = (e.errno >= 0).then_some(e.errno);
    let detail = san(&err_str(e)).trim().trim_end_matches('.').to_string();
    let safe_target = san(target);
    let prefix = if !phase.is_push() {
        let step = c
            .action
            .map(str::to_string)
            .unwrap_or_else(|| format!("prepare the {}", phase.kind()));
        format!("Failed to {step} '{safe_target}'")
    } else {
        let mut p = match &c.source_file {
            Some(f) => format!("Failed to upload '{}' to {safe_target}", san(f)),
            None => format!("Failed to upload files to {safe_target}"),
        };
        if c.files_uploaded > 0 {
            let n = if c.files_uploaded == 1 {
                "1 file".to_string()
            } else {
                format!("{} files", c.files_uploaded)
            };
            p = format!("{p} after {n} had already uploaded");
        }
        p
    };
    let kind = phase.kind();
    let role = c
        .role
        .as_ref()
        .map(|r| format!("role '{}'", san(r)))
        .unwrap_or_else(|| "your role".into());
    let remediation = match code {
        Some(3001) => {
            let needed = c.privilege.map(str::to_string).unwrap_or_else(|| {
                if phase.is_push() { format!("WRITE on the {kind}") } else { "the privileges it requires".into() }
            });
            format!("Verify that {role} has {needed}, plus USAGE on the database and schema.")
        }
        Some(2003 | 2043) => {
            if phase.is_push() {
                format!("The {kind} existed when the upload started and is gone now. This usually means another deploy of the same app recreated it at the same time. Check for concurrent deploys, then re-run this one.")
            } else if let (Some(d), Some(s)) = (&c.database, &c.schema) {
                format!(
                    "Database '{}' or schema '{}' does not exist, or {role} cannot see it. Create the schema, or correct the database and schema in your project configuration.",
                    san(d),
                    san(s)
                )
            } else {
                format!("The {kind}, or the schema holding it, does not exist or {role} cannot see it. Create it, or correct the database and schema in your project configuration.")
            }
        }
        Some(3041) => format!("'{safe_target}' already exists and {role} cannot use it. Drop it, or point code storage at a name your role owns."),
        Some(60119) => match &c.encryption {
            Some(enc) => format!(
                "This Snowflake deployment does not support a stage with ENCRYPTION = (TYPE = '{}'). Set a different encryption_type on the code stage, or use a workspace for code storage.",
                san(enc)
            ),
            None => "This Snowflake deployment does not support the requested feature. Contact your account administrator to find out whether it can be enabled.".into(),
        },
        Some(1003) => format!("Snowflake could not compile the statement. Check that the {kind} name in your project configuration is a valid identifier."),
        Some(253003) => "The transfer to cloud storage failed. This is usually a transient network problem, so re-running the deploy often succeeds.".into(),
        Some(99108) => "Snowflake rejected the write to the workspace. Re-run the deploy; if it keeps failing, set code_stage to use a stage for code storage instead.".into(),
        _ if !phase.is_push() && c.privilege.is_some() => format!(
            "Check that {role} has {}, then re-run with --debug for the full traceback.",
            c.privilege.unwrap_or_default()
        ),
        _ => format!("The {} step of the upload failed. Re-run with --debug for the full traceback.", phase.value().replace('_', " ")),
    };
    let msg = if detail.is_empty() {
        format!("{prefix}. {remediation}")
    } else {
        format!("{prefix}: {detail}. {remediation}")
    };
    SnowError::cli(msg).named("UploadError")
}

// ── uploads ───────────────────────────────────────────────────────────

fn workspace_uri(f: &Fqn) -> String {
    format!("snow://workspace/{}/versions/live", f.identifier())
}

fn workspace_sub_uri(f: &Fqn, dir: &str) -> String {
    format!("{}/{}", workspace_uri(f), dir.trim_matches('/'))
}

/// `_local_path_to_file_uri(local_path)`.
fn file_uri(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut esc = String::new();
    for ch in s.chars() {
        match ch {
            '*' | '?' | '[' => {
                esc.push('[');
                esc.push(ch);
                esc.push(']');
            }
            c => esc.push(c),
        }
    }
    let uri = format!("file://{esc}");
    let re = regex::Regex::new(r#"^[\w/*?\-.=&{}$#\[\]"\\!@%^+:]+$"#).expect("re");
    if re.is_match(&uri) {
        uri
    } else {
        to_string_literal(&uri.replace('\\', "\\\\"))
    }
}

fn bundle_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(d) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    walk(root, &mut out);
    out.sort();
    out
}

/// `upload_to_stage` / `upload_to_workspace` + `_run_uploads` + `_stream_uploads`:
/// up to five PUTs at once on the same session, each `Uploaded` line printed as it
/// completes (so the order varies, as upstream).
fn push_files(
    cli: &mut Cli,
    root: &Path,
    dest: &(dyn Fn(&str) -> (String, String) + Sync),
    phase: Phase,
    target: &str,
    reraise_sql: bool,
) -> Result<std::result::Result<(), DbError>> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut jobs: Vec<(String, String)> = Vec::new();
    for path in bundle_files(&root) {
        let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
        let rel_dir = rel
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let (put_target, shown_dir) = dest(&rel_dir);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let sql = format!(
            "PUT {} {put_target} auto_compress=false overwrite=true",
            file_uri(&path.canonicalize().unwrap_or(path.clone()))
        );
        jobs.push((
            sql,
            format!("  Uploaded {} -> {shown_dir}{name}", rel.display()),
        ));
    }
    if jobs.is_empty() {
        return Ok(Ok(()));
    }
    let base = cli.connection()?.worker();
    let queue = std::sync::Mutex::new(
        jobs.into_iter()
            .enumerate()
            .collect::<std::collections::VecDeque<_>>(),
    );
    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<String, DbError>>();
    let workers = 5.min(queue.lock().map(|q| q.len()).unwrap_or(1));
    let stop = std::sync::atomic::AtomicBool::new(false);
    let mut uploaded = 0usize;
    let mut failure: Option<DbError> = None;
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let mut conn = base.worker();
            let (queue, stop) = (&queue, &stop);
            scope.spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let Some((_, (sql, line))) = queue.lock().ok().and_then(|mut q| q.pop_front())
                    else {
                        break;
                    };
                    let mut attempt = 0;
                    let r = loop {
                        attempt += 1;
                        match conn.execute(&sql) {
                            Ok(_) => break Ok(line.clone()),
                            Err(e) if e.errno == 253003 && attempt < 3 => {
                                std::thread::sleep(std::time::Duration::from_millis(500 * attempt))
                            }
                            Err(e) => break Err(e),
                        }
                    };
                    let failed = r.is_err();
                    let _ = tx.send(r);
                    if failed {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            });
        }
        drop(tx);
        for r in rx {
            match r {
                Ok(line) if failure.is_none() => {
                    uploaded += 1;
                    cli.console.step(&line);
                }
                Ok(_) => {}
                Err(e) => {
                    if failure.is_none() {
                        failure = Some(e);
                    }
                }
            }
        }
    });
    let Some(e) = failure else { return Ok(Ok(())) };
    if reraise_sql && e.sqlstate.is_some() {
        return Ok(Err(e));
    }
    let role = manager::current_role(cli);
    Err(classify(
        &e,
        phase,
        target,
        &UploadCtx {
            role,
            files_uploaded: uploaded,
            ..Default::default()
        },
    ))
}

fn stage_exists(cli: &mut Cli, f: &Fqn) -> Result<std::result::Result<bool, DbError>> {
    let q = format!(
        "SHOW STAGES LIKE {} IN SCHEMA {}.{}",
        identifier_to_show_like_pattern(&f.name),
        to_identifier(f.database.as_deref().unwrap_or_default()),
        to_identifier(f.schema.as_deref().unwrap_or_default())
    );
    Ok(match run(cli, &q)? {
        Ok(c) => Ok(any_name(&c, &f.name)?),
        Err(e) => Err(e),
    })
}

/// `any(row["name"].upper() == unquote_identifier(name).upper() for row in cursor)`.
fn any_name(c: &snow_connector::Cursor, name: &str) -> Result<bool> {
    if c.rows.is_empty() {
        return Ok(false);
    }
    let Some(i) = c.columns.iter().position(|col| col.name == "name") else {
        return Err(SnowError::unexpected("'name'").named("KeyError"));
    };
    let want = unquote_identifier(name).to_uppercase();
    Ok(c.rows.iter().any(|r| {
        r.get(i)
            .cloned()
            .flatten()
            .is_some_and(|n| n.to_uppercase() == want)
    }))
}

fn create_stage_sql(f: &Fqn, enc: &str) -> String {
    format!(
        "CREATE STAGE IF NOT EXISTS {} ENCRYPTION = (TYPE = {})",
        f.sql_identifier(),
        to_string_literal(enc)
    )
}

/// `_empty_stage_for_upload(manager, stage_fqn, encryption)`: `true` when it (re)created the stage.
fn empty_stage(cli: &mut Cli, f: &Fqn, enc: &str) -> Result<bool> {
    let ident = f.identifier();
    let fail = |cli: &mut Cli, e: &DbError, action: &str, privilege: &str| -> SnowError {
        let role = manager::current_role(cli);
        classify(
            e,
            Phase::PrepareStage,
            &ident,
            &UploadCtx {
                action: Some(action),
                privilege: Some(privilege),
                role,
                database: f.database.clone(),
                schema: f.schema.clone(),
                encryption: Some(enc.to_string()),
                ..Default::default()
            },
        )
    };
    let existed = match stage_exists(cli, f)? {
        Ok(b) => b,
        Err(e) => return Err(fail(cli, &e, "look up stage", "USAGE on the schema")),
    };
    cli.console.step(&if existed {
        format!("Recreating stage @{ident}")
    } else {
        format!("Creating stage @{ident}")
    });
    let cannot_create = match run(cli, &create_stage_sql(f, enc))? {
        Ok(_) => None,
        Err(e) if e.errno == 3001 => Some(e),
        Err(e) => return Err(fail(cli, &e, "create stage", "CREATE STAGE on the schema")),
    };
    if !existed {
        if let Some(e) = cannot_create {
            return Err(fail(cli, &e, "create stage", "CREATE STAGE on the schema"));
        }
        return Ok(true);
    }
    if cannot_create.is_none() {
        match run(cli, &format!("DROP STAGE IF EXISTS {}", f.sql_identifier()))? {
            Ok(_) => {
                if let Err(e) = run(cli, &create_stage_sql(f, enc))? {
                    return Err(fail(cli, &e, "create stage", "CREATE STAGE on the schema"));
                }
                return Ok(true);
            }
            Err(e) if e.errno == 3001 => {}
            Err(e) => return Err(fail(cli, &e, "drop stage", "OWNERSHIP on the stage")),
        }
    }
    let reason = if cannot_create.is_some() {
        "the deploying role cannot create it again"
    } else {
        "the deploying role cannot drop it"
    };
    cli.console.warning(&format!(
        "Clearing stage @{} instead of recreating it, because {reason}. Files deleted from the project since the last deploy may survive on the stage. Grant the deploying role OWNERSHIP on the stage and CREATE STAGE on the schema to have it recreated cleanly.",
        san(&ident)
    ));
    let root =
        snow_api::stage_path::StagePath::from_stage_str(&format!("@{ident}/")).path_for_sql();
    if let Err(e) = run(cli, &format!("REMOVE {root}"))? {
        return Err(fail(cli, &e, "clear stage", "WRITE on the stage"));
    }
    Ok(false)
}

/// `_upload_via_stage(...)`.
fn upload_via_stage(cli: &mut Cli, f: &Fqn, enc: &str, bundle: &Path) -> Result<bool> {
    let created = empty_stage(cli, f, enc)?;
    let ident = f.identifier();
    cli.console
        .step(&format!("Uploading bundled files to @{ident}"));
    let base = snow_api::stage_path::StagePath::from_stage_str(&format!("@{ident}"));
    let dest = |rel_dir: &str| {
        let p = if rel_dir.is_empty() {
            base.clone()
        } else {
            base.join(rel_dir)
        };
        (p.path_for_sql(), format!("{}/", p.absolute_path()))
    };
    if let Err(e) = push_files(
        cli,
        bundle,
        &dest,
        Phase::PushStage,
        &format!("@{ident}"),
        false,
    )? {
        return Err(db_err(cli, &e));
    }
    Ok(created)
}

/// `_upload_via_workspace(...)`; `Ok(Err(e))` is a SQL error a regular database falls back from.
fn upload_via_workspace(
    cli: &mut Cli,
    f: &Fqn,
    app: &str,
    database: &str,
    bundle: &Path,
) -> Result<std::result::Result<(), DbError>> {
    let sub = workspace_sub_uri(f, app);
    let personal = is_personal_database(Some(database));
    let fail = |cli: &mut Cli,
                e: DbError,
                action: &str,
                privilege: &str|
     -> Result<std::result::Result<(), DbError>> {
        if !personal {
            return Ok(Err(e));
        }
        let role = manager::current_role(cli);
        Err(classify(
            &e,
            Phase::PrepareWorkspace,
            &f.identifier(),
            &UploadCtx {
                action: Some(action),
                privilege: Some(privilege),
                role,
                database: f.database.clone(),
                schema: f.schema.clone(),
                ..Default::default()
            },
        ))
    };
    cli.console
        .step(&format!("Creating workspace {}", f.identifier()));
    if let Err(e) = run(
        cli,
        &format!("CREATE WORKSPACE IF NOT EXISTS {}", f.sql_identifier()),
    )? {
        return fail(cli, e, "create workspace", "CREATE WORKSPACE on the schema");
    }
    if let Err(e) = run(
        cli,
        &format!(
            "ALTER WORKSPACE {} ADD LIVE VERSION FROM LAST",
            f.sql_identifier()
        ),
    )? {
        let text = err_str(&e);
        let exists = matches!(e.errno, 99106 | 3041)
            || (text.contains("42710") && (text.contains("099106") || text.contains("003041")));
        if !exists {
            return fail(cli, e, "create workspace", "CREATE WORKSPACE on the schema");
        }
    }
    cli.console
        .step(&format!("Clearing existing workspace files in {sub}/"));
    if let Err(e) = run(cli, &format!("REMOVE {sub}/"))? {
        return fail(cli, e, "clear workspace files", "WRITE on the workspace");
    }
    cli.console
        .step(&format!("Uploading bundled files to {sub}"));
    let dest = |rel_dir: &str| {
        let d = if rel_dir.is_empty() {
            format!("{sub}/")
        } else {
            format!("{sub}/{rel_dir}/")
        };
        (to_string_literal(&d), d)
    };
    push_files(cli, bundle, &dest, Phase::PushWorkspace, &sub, !personal)
}

/// The resolved inputs of `_upload_and_build_app`.
struct Pipeline {
    storage: CodeStorage,
    storage_fqn: Fqn,
    app_id: String,
    database: String,
    schema: String,
    repo_fqn: String,
    repo_db: Option<String>,
    repo_schema: Option<String>,
    repo_name: String,
    build_eai: Option<String>,
    build_job_location: Option<String>,
    bundle_id: String,
    artifacts: Vec<PyV>,
    compute_pool: Option<String>,
    runtime_image: String,
    project_type: String,
}

/// `_upload_and_build_app(...)`: `Some` short-circuits for --upload-only / --build-only.
fn upload_and_build(
    cli: &mut Cli,
    p: &mut Pipeline,
    upload: bool,
    build: bool,
    upload_only: bool,
    build_only: bool,
) -> Result<Option<Output>> {
    let mut use_ws = p.storage.ty == "workspace";
    let mut stage_created = false;
    if upload {
        let bundle = super::perform_bundle(cli, &p.bundle_id, &p.artifacts)?;
        let r = (|| -> Result<()> {
            if !use_ws {
                stage_created =
                    upload_via_stage(cli, &p.storage_fqn, &p.storage.encryption, &bundle)?;
            } else if let Err(e) =
                upload_via_workspace(cli, &p.storage_fqn, &p.app_id, &p.database, &bundle)?
            {
                cli.console.warning(&format!(
                    "Could not use a workspace for code storage in '{}': {}. Falling back to a stage.",
                    san(&p.storage_fqn.identifier()),
                    err_str(&e)
                ));
                use_ws = false;
                p.storage_fqn = app_fqn(
                    Some(&p.database),
                    Some(&p.schema),
                    &format!("{}_CODE", p.app_id),
                );
                stage_created = upload_via_stage(cli, &p.storage_fqn, "SNOWFLAKE_SSE", &bundle)?;
            }
            Ok(())
        })();
        super::clean_up_output_pub(cli, &bundle);
        r?;
    }
    if upload_only {
        return Ok(Some(Output::message(if use_ws {
            format!(
                "Artifacts uploaded to {}",
                workspace_sub_uri(&p.storage_fqn, &p.app_id)
            )
        } else {
            format!("Artifacts uploaded to @{}", p.storage_fqn.identifier())
        })));
    }
    if build {
        let (rdb, rsc) = (
            p.repo_db.clone().unwrap_or_default(),
            p.repo_schema.clone().unwrap_or_default(),
        );
        let q = format!(
            "SHOW ARTIFACT REPOSITORIES LIKE {} IN SCHEMA {}.{}",
            identifier_to_show_like_pattern(&p.repo_name),
            to_identifier(&rdb),
            to_identifier(&rsc)
        );
        let c = cli.execute_query(&q)?;
        if !any_name(&c, &p.repo_name)? {
            cli.console
                .step(&format!("Creating artifact repository: {}", p.repo_fqn));
            let f = app_fqn(Some(&rdb), Some(&rsc), &p.repo_name);
            cli.execute_query(&format!(
                "CREATE ARTIFACT REPOSITORY IF NOT EXISTS {} TYPE=APPLICATION",
                f.sql_identifier()
            ))?;
        }
        cli.console
            .step("Building app using artifact repository...");
        let source = if use_ws {
            workspace_sub_uri(&p.storage_fqn, &p.app_id)
        } else {
            format!("@{}", p.storage_fqn.identifier())
        };
        let result = build_artifact_repo(cli, p, &source)?;
        cli.console.step(&format!(
            "SPCS_TEST_BUILD_APP_ARTIFACT_REPO output:\n{result}"
        ));
        let re = regex::Regex::new(r"Build job submitted:\s*(\S+)").expect("re");
        let Some(m) = re.captures(&result) else {
            return Err(SnowError::cli(format!(
                "Could not parse build job name from output: {result}"
            )));
        };
        let job = Fqn::from_string(&m[1])?;
        cli.console.step(&format!(
            "[{}] Waiting for artifact repo build to complete: {job}...",
            ts()
        ));
        let timeout = format!(
            "Artifact repo build timed out. Check build logs:\n  SELECT * FROM TABLE({}!SPCS_GET_LOGS())",
            job.identifier()
        );
        poll_states(cli, &job, &timeout)?;
        if p.storage.temporary {
            if use_ws {
                drop_after_build(cli, "workspace", &p.storage_fqn);
            } else {
                drop_after_build(cli, "stage", &p.storage_fqn);
            }
        } else if stage_created {
            drop_after_build(cli, "stage", &p.storage_fqn);
        }
    }
    if build_only {
        return Ok(Some(Output::message("Build completed successfully.")));
    }
    Ok(None)
}

fn drop_after_build(cli: &mut Cli, kind: &str, f: &Fqn) {
    let (step, sql) = if kind == "stage" {
        (
            format!("Dropping stage @{f} now that the build is complete"),
            format!("DROP STAGE IF EXISTS {}", f.sql_identifier()),
        )
    } else {
        (
            format!("Dropping workspace {f} now that the build is complete"),
            format!("DROP WORKSPACE IF EXISTS {}", f.sql_identifier()),
        )
    };
    cli.console.step(&step);
    if let Ok(Err(e)) = cli.try_query(&sql) {
        cli.console.warning(&format!(
            "Could not drop {kind} '{}' after the build completed: {}. The build succeeded; you can remove the {kind} manually if desired.",
            san(&f.identifier()),
            err_str(&e)
        ));
    }
}

/// `build_app_artifact_repo(...)` inside `_use_database_and_schema`.
fn build_artifact_repo(cli: &mut Cli, p: &Pipeline, source: &str) -> Result<String> {
    let first = |cli: &mut Cli, q: &str| -> Result<Option<String>> {
        let c = cli.execute_query(q)?;
        Ok(c.rows.first().and_then(|r| r.first().cloned().flatten()))
    };
    let prev_db = first(cli, "SELECT CURRENT_DATABASE()")?;
    let prev_schema = first(cli, "SELECT CURRENT_SCHEMA()")?;
    cli.execute_query(&format!("USE DATABASE {}", to_identifier(&p.database)))?;
    cli.execute_query(&format!("USE SCHEMA {}", to_identifier(&p.schema)))?;
    let mut cfg = serde_json::Map::new();
    if let Some(e) = p.build_eai.as_ref().filter(|e| !e.is_empty()) {
        cfg.insert(
            "external_access_integrations".into(),
            serde_json::json!([e]),
        );
    }
    if let Some(l) = p.build_job_location.as_ref().filter(|e| !e.is_empty()) {
        cfg.insert(
            "build_job_location".into(),
            serde_json::Value::String(l.clone()),
        );
    }
    let config = snow_api::pyfmt::json_dumps(&serde_json::Value::Object(cfg), None);
    let q = format!(
        "SELECT SYSTEM$SPCS_TEST_BUILD_APP_ARTIFACT_REPO({}, {}, {}, {}, {}, {}, {})",
        to_string_literal(source),
        to_string_literal(&p.repo_fqn),
        to_string_literal(&p.app_id),
        to_string_literal(p.compute_pool.as_deref().unwrap_or("")),
        to_string_literal(&p.runtime_image),
        to_string_literal(&p.project_type),
        to_string_literal(&config)
    );
    let r = first(cli, &q);
    if let Some(d) = prev_db.filter(|d| !d.is_empty()) {
        cli.execute_query(&format!("USE DATABASE {}", to_identifier(&d)))?;
        if let Some(s) = prev_schema.filter(|s| !s.is_empty()) {
            cli.execute_query(&format!("USE SCHEMA {}", to_identifier(&s)))?;
        }
    }
    Ok(r?.unwrap_or_default())
}

fn sleep_poll() {
    std::thread::sleep(std::time::Duration::from_secs(5));
}

/// `_poll_until(get_build_status, done={DONE}, error={FAILED, IDLE}, pending={PENDING, RUNNING})`.
fn poll_states(cli: &mut Cli, job: &Fqn, timeout: &str) -> Result<()> {
    let failed = regex::Regex::new(r"(?i)\btimed out\b")
        .expect("re")
        .replace(timeout, "failed")
        .into_owned();
    let mut container: Option<(String, String)> = None;
    let mut seen = 0usize;
    for _ in 0..240 {
        // `_make_build_log_streamer`: new build log lines go to the INFO log.
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let lines = build_job_logs(cli, job, &mut container);
            for l in lines.iter().skip(seen) {
                crate::logs::info("snowflake.cli._plugins.apps.commands", l);
            }
            seen = lines.len();
        }
        let status =
            match manager::first_row(cli, &format!("DESCRIBE SERVICE {}", job.identifier()))? {
                Ok(row) if !row.is_empty() => match row.get("status") {
                    Some(Pv::None) | None => "IDLE".to_string(),
                    Some(v) => v.py_str(),
                },
                _ => "IDLE".to_string(),
            };
        cli.console.step(&format!("[{}] Status: {status}", ts()));
        match status.as_str() {
            "DONE" => return Ok(()),
            "FAILED" | "IDLE" => return Err(SnowError::cli(format!("{failed} (status={status})"))),
            "PENDING" | "RUNNING" => {}
            _ => {
                return Err(SnowError::cli(format!(
                    "{timeout} (unexpected status={status})"
                )));
            }
        }
    }
    Err(SnowError::cli(format!(
        "{timeout} (timed out after 20 minutes)"
    )))
}

/// `get_build_job_logs(build_job_fqn)` with `_resolve_build_job_container`; failures give `[]`.
fn build_job_logs(cli: &mut Cli, job: &Fqn, cache: &mut Option<(String, String)>) -> Vec<String> {
    let ident = job.identifier();
    if cache.is_none() {
        let Ok(Ok(c)) = cli.try_query(&format!("SHOW SERVICE CONTAINERS IN SERVICE {ident}"))
        else {
            return vec![];
        };
        let idx = |n: &str| {
            c.columns
                .iter()
                .position(|col| col.name.eq_ignore_ascii_case(n))
        };
        let (ci, ii) = (idx("container_name"), idx("instance_id"));
        let containers: Vec<(String, String)> = c
            .rows
            .iter()
            .filter_map(|r| {
                let name = ci.and_then(|i| r.get(i).cloned().flatten())?;
                let inst = ii.and_then(|i| r.get(i).cloned().flatten())?;
                Some((inst, name))
            })
            .collect();
        if containers.is_empty() {
            return vec![];
        }
        if containers.len() > 1 {
            let listed: Vec<String> = containers.iter().map(|(_, n)| san(n)).collect();
            cli.console.warning(&format!(
                "Build job {} has multiple containers: {}. Using 'builder' if present, otherwise the first.",
                san(&ident),
                listed.join(", ")
            ));
        }
        *cache = Some(
            containers
                .iter()
                .find(|(_, n)| n == "builder")
                .cloned()
                .unwrap_or_else(|| containers[0].clone()),
        );
    }
    let Some((inst, name)) = cache.clone() else {
        return vec![];
    };
    let q = format!(
        "CALL SYSTEM$GET_SERVICE_LOGS({}, {}, {}, 500)",
        to_string_literal(&ident),
        to_string_literal(&inst),
        to_string_literal(&name)
    );
    match cli.try_query(&q) {
        Ok(Ok(c)) => c
            .rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .map(|t| {
                t.lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

type Desc = IndexMap<String, Pv>;

/// `_poll_until(describe_app_service, is_done=url ready, is_error=FAILED)`.
fn poll_endpoint(
    cli: &mut Cli,
    f: &Fqn,
    timeout: &str,
    fmt: &dyn Fn(&Desc) -> String,
) -> Result<Desc> {
    let failed = regex::Regex::new(r"(?i)\btimed out\b")
        .expect("re")
        .replace(timeout, "failed")
        .into_owned();
    for _ in 0..240 {
        sleep_poll();
        let d = match manager::describe_app_service(cli, f)? {
            Ok(d) => d,
            Err(e) => return Err(db_err(cli, &e)),
        };
        cli.console.step(&format!("[{}] Status: {}", ts(), fmt(&d)));
        if manager::url_from_describe(&d).is_some() {
            return Ok(d);
        }
        if desc_str(&d, "status").to_uppercase() == "FAILED" {
            return Err(SnowError::cli(format!("{failed} (status={})", fmt(&d))));
        }
    }
    Err(SnowError::cli(format!(
        "{timeout} (timed out after 20 minutes)"
    )))
}

fn desc_str(d: &Desc, k: &str) -> String {
    match d.get(k) {
        None | Some(Pv::None) => String::new(),
        Some(v) => v.py_str(),
    }
}

/// `_wait_for_app_yml_endpoint` / the snowflake.yml endpoint wait, with the FAILED follow-up.
fn wait_endpoint(cli: &mut Cli, f: &Fqn, upgrade: bool) -> Result<String> {
    let ident = f.identifier();
    let r = if upgrade {
        cli.console
            .step(&format!("[{}] Waiting for upgrade to complete...", ts()));
        let t = format!(
            "Upgrade timed out. Check application service state and logs:\n  DESCRIBE APPLICATION SERVICE {ident}\n  CALL SYSTEM$GET_APPLICATION_SERVICE_LOGS('{ident}')"
        );
        poll_endpoint(cli, f, &t, &|d| {
            if ["true", "1", "yes"].contains(&desc_str(d, "is_upgrading").to_lowercase().as_str()) {
                "upgrading".into()
            } else {
                "ready".into()
            }
        })
    } else {
        cli.console.step(&format!(
            "[{}] Waiting for application service endpoint...",
            ts()
        ));
        let t = format!(
            "Application service deployment timed out. Check application service state and logs:\n  DESCRIBE APPLICATION SERVICE {ident}\n  CALL SYSTEM$GET_APPLICATION_SERVICE_LOGS('{ident}')"
        );
        poll_endpoint(cli, f, &t, &|d| {
            let u = desc_str(d, "url");
            if u.is_empty() {
                "url not yet available".into()
            } else {
                u
            }
        })
    };
    let d = match r {
        Ok(d) => d,
        Err(e) => {
            if let Ok(Ok(d)) = manager::describe_app_service(cli, f)
                && desc_str(&d, "status").to_uppercase() == "FAILED"
            {
                log_service_logs(cli, f);
            }
            return Err(e);
        }
    };
    manager::url_from_describe(&d).ok_or_else(|| {
        SnowError::cli(format!("Application service URL is not available after deploy. Check: DESCRIBE APPLICATION SERVICE {ident}"))
    })
}

/// `_log_service_logs`: the logs go to the INFO log only.
fn log_service_logs(cli: &mut Cli, f: &Fqn) {
    let _ = cli.connection().map(|c| {
        c.execute_bound(
            "CALL SYSTEM$GET_APPLICATION_SERVICE_LOGS(?)",
            &[f.identifier()],
        )
    });
}

/// `_ensure_app_derived_schema(manager, database, schema, app_name)`.
fn ensure_app_schema(cli: &mut Cli, db: &str, schema: &str, app: &str) -> Result<()> {
    if db.is_empty() || schema.is_empty() || unquote_identifier(schema) != unquote_identifier(app) {
        return Ok(());
    }
    cli.console.step(&format!(
        "Creating app schema {}.{} if it does not exist",
        san(db),
        san(schema)
    ));
    let f = Fqn {
        database: None,
        schema: Some(db.to_string()),
        name: schema.to_string(),
        signature: None,
    };
    if let Err(e) = run(
        cli,
        &format!("CREATE SCHEMA IF NOT EXISTS {}", f.sql_identifier()),
    )? {
        let q = format!(
            "SHOW SCHEMAS LIKE {} IN DATABASE IDENTIFIER({})",
            to_string_literal(schema),
            to_string_literal(db)
        );
        if let Ok(Ok(c)) = cli.try_query(&q)
            && !c.rows.is_empty()
        {
            return Ok(());
        }
        return Err(SnowError::cli(format!(
            "Could not create app-derived schema '{}.{}': {}. Grant CREATE SCHEMA on database '{}' to the active role, or create the schema manually.",
            san(db),
            san(schema),
            err_str(&e),
            san(db)
        )));
    }
    Ok(())
}

fn ui_enabled(cli: &mut Cli, name: &str, values: &[&str]) -> bool {
    match crate::snowsight::ui_parameter(cli, name) {
        Ok(Some(v)) => {
            let s = match &v {
                serde_json::Value::String(x) => x.clone(),
                serde_json::Value::Bool(b) => if *b { "True" } else { "False" }.into(),
                other => other.to_string(),
            };
            values.contains(&s.trim().to_lowercase().as_str())
        }
        _ => false,
    }
}

/// `_ensure_cng_url_cert_ready(manager, skip, required)`.
fn cert_precheck(cli: &mut Cli, skip: bool, required: bool) -> Result<()> {
    if skip {
        return Ok(());
    }
    let Some(host) = manager::per_account_cert_probe_host(cli) else {
        return Ok(());
    };
    cli.console.step("Checking per-account URL certificate...");
    match manager::cert_status_for_host(cli, &host) {
        Some(true) => Ok(()),
        None => {
            cli.console.warning(&format!(
                "Could not verify the account's per-account URL certificate (the ingress was unreachable or untrusted — e.g. PrivateLink, a proxy, or a custom CA). Continuing the deploy. If the app URL shows a browser TLS warning, provision the certificate by running:\n  SELECT {PER_ACCOUNT_CERT_ISSUE_FUNCTION}();"
            ));
            Ok(())
        }
        Some(false) => {
            cli.console
                .step("Starting per-account URL certificate provisioning...");
            cli.execute_query(&format!("SELECT {PER_ACCOUNT_CERT_ISSUE_FUNCTION}()"))?;
            let msg = format!(
                "This account does not yet have a per-account URL certificate, which serverless apps require. Provisioning has been started for you via {PER_ACCOUNT_CERT_ISSUE_FUNCTION}(). This can take up to 3 hours. Re-run 'snow app deploy' once provisioning completes."
            );
            if required {
                return Err(SnowError::cli(msg));
            }
            cli.console.warning(&msg);
            Ok(())
        }
    }
}

fn field(m: &PyV, k: &str) -> PyV {
    m.as_model()
        .and_then(|m| m.fields.get(k).cloned())
        .unwrap_or(PyV::None)
}

fn opt(m: &PyV, k: &str) -> Option<String> {
    match field(m, k) {
        PyV::None => None,
        v => Some(v.py_str()).filter(|s| !s.is_empty()),
    }
}

/// `build_service_specification(target, database, schema, include_*)`.
fn service_spec(t: &PyV, db: &str, sc: &str, cng: bool) -> Result<String> {
    let mut spec = IndexMap::new();
    let s = |k: &str| opt(t, k).map(PyV::Str);
    for (k, include) in [
        ("query_warehouse", true),
        ("url_prefix", cng),
        ("health_check", cng),
        ("label", true),
        ("description", true),
        ("icon", true),
        ("execute_as_role", true),
    ] {
        if include && let Some(v) = s(k) {
            spec.insert(k.to_string(), v);
        }
    }
    for k in [
        "auto_resume",
        "auto_suspend_secs",
        "min_instances",
        "max_instances",
    ] {
        let v = field(t, k);
        if !v.is_none() {
            spec.insert(k.to_string(), v);
        }
    }
    if let PyV::List(l) = field(t, "external_access_integrations")
        && !l.is_empty()
    {
        spec.insert("external_access_integrations".into(), PyV::List(l));
    }
    if let PyV::List(l) = field(t, "secrets")
        && !l.is_empty()
    {
        let mut out = Vec::new();
        for sec in &l {
            let secret = opt(sec, "secret").unwrap_or_default();
            let f = Fqn::from_string(&secret)?;
            let qualified = match (
                f.database.clone().or_else(|| Some(db.to_string())),
                f.schema.clone().or_else(|| Some(sc.to_string())),
            ) {
                (Some(d), Some(s)) if !d.is_empty() && !s.is_empty() => {
                    format!("{d}.{s}.{}", f.name)
                }
                _ => secret.clone(),
            };
            let mut m = IndexMap::new();
            m.insert(
                "name".to_string(),
                PyV::Str(opt(sec, "name").unwrap_or_default()),
            );
            m.insert("secret".to_string(), PyV::Str(qualified));
            out.push(PyV::Dict(m));
        }
        spec.insert("secrets".into(), PyV::List(out));
    }
    if let PyV::List(l) = field(t, "environment_variables")
        && !l.is_empty()
    {
        let out = l
            .iter()
            .map(|e| {
                let mut m = IndexMap::new();
                m.insert(
                    "name".to_string(),
                    PyV::Str(opt(e, "name").unwrap_or_default()),
                );
                m.insert("value".to_string(), PyV::Str(field(e, "value").py_str()));
                PyV::Dict(m)
            })
            .collect();
        spec.insert("environment_variables".into(), PyV::List(out));
    }
    if cng && let PyV::List(l) = field(t, "postgres_instances") {
        let out = l
            .iter()
            .map(|b| {
                let mut m = IndexMap::new();
                for k in ["instance", "name", "authentication_mode"] {
                    if let Some(v) = opt(b, k) {
                        m.insert(k.to_string(), PyV::Str(v));
                    }
                }
                PyV::Dict(m)
            })
            .collect();
        spec.insert("postgres_instances".into(), PyV::List(out));
    }
    let doc = if spec.is_empty() {
        "{}\n".to_string()
    } else {
        snow_api::project::yaml::safe_dump_default_width(&PyV::Dict(spec))
    };
    Ok(doc)
}

/// `snowflake_app_deploy(...)`.
pub fn deploy(cli: &mut Cli, flow: Flow) -> Result<Output> {
    let mut native: Vec<&str> = Vec::new();
    if cli.opt_bool("prune").is_some() {
        native.push("--prune");
    }
    if cli.opt_bool("recursive").is_some() {
        native.push("--recursive");
    }
    if !cli.list("paths").is_empty() {
        native.push("paths");
    }
    if !native.is_empty() {
        native.sort();
        return Err(SnowError::click(format!(
            "'snow app deploy' was invoked against a Snowflake App Runtime entity (type: snowflake-app), but the following Native App options were provided: {}. Remove them and try again.",
            native.join(", ")
        )));
    }
    let upload_only = cli.flag("upload_only");
    let build_only = cli.flag("build_only");
    let promote_only = cli.flag("promote_only") || cli.flag("deploy_only");
    if [upload_only, build_only, promote_only]
        .iter()
        .filter(|b| **b)
        .count()
        > 1
    {
        return Err(SnowError::click(
            "Only one of --upload-only, --build-only, or --promote-only may be specified.",
        ));
    }
    let skip_certs = cli.flag("skip_certs_check");
    match flow {
        Flow::AppYml(def) => {
            deploy_yml(cli, &def, upload_only, build_only, promote_only, skip_certs)
        }
        _ => {
            super::reject_target_pub(cli)?;
            deploy_entity(cli, upload_only, build_only, promote_only)
        }
    }
}

fn deploy_yml(
    cli: &mut Cli,
    def: &PyV,
    upload_only: bool,
    build_only: bool,
    promote_only: bool,
    skip_certs: bool,
) -> Result<Output> {
    super::ensure_python_phases(cli, def)?;
    let dep = super::resolve_deployment(cli, def, cli.str("target").as_deref())?;
    let label = san(&dep.service_name);
    match &dep.target_name {
        Some(t) => cli.console.step(&format!(
            "Deploying target '{}' (application service {label}).",
            san(t)
        )),
        None => cli
            .console
            .step(&format!("Deploying application service {label}.")),
    }
    let compute = opt(&dep.target, "compute_resource");
    let python = opt(def, "runtime")
        .unwrap_or_default()
        .split(',')
        .any(|r| r == "python");
    if python {
        let effective = compute
            .clone()
            .unwrap_or_else(|| "MANAGED_COMPUTE_POOL".into());
        let param = if effective == "SERVERLESS" {
            "ENABLE_APP_SERVICE_PYTHON_RUNTIME_SERVERLESS"
        } else {
            "ENABLE_APP_SERVICE_PYTHON_RUNTIME_MANAGED_COMPUTE_POOL"
        };
        if !ui_enabled(cli, param, &["true"]) {
            return Err(SnowError::cli(format!(
                "The Python runtime is not enabled for compute_resource {effective} in this account. This app's runtime in app.yml includes `python`. Deploy to a compute_resource that allows the Python runtime, or contact Snowflake support to enable it."
            )));
        }
    }
    ensure_app_schema(cli, &dep.database, &dep.schema, &dep.service_name)?;
    if promote_only {
        cli.console
            .step("Promoting the latest built package (skipping upload and build).");
    }
    let cng = compute
        .as_deref()
        .is_some_and(|c| c.eq_ignore_ascii_case("SERVERLESS"));
    if cng
        && !ui_enabled(
            cli,
            "FEATURE_APPLICATION_SERVICE_COMPUTE_RESOURCE_SERVERLESS",
            &["true", "enabled", "enabled_public_preview"],
        )
    {
        return Err(SnowError::cli(
            "compute_resource SERVERLESS is not enabled for this account.",
        ));
    }
    if cng {
        cert_precheck(cli, skip_certs, !upload_only && !build_only)?;
    }
    let (rdb, rsc, rname) = match opt(&dep.target, "artifact_repo") {
        Some(v) => {
            let f = Fqn::from_string(&v)?;
            (
                f.database.or(Some(dep.database.clone())),
                f.schema.or(Some(dep.schema.clone())),
                f.name,
            )
        }
        None => (
            Some(dep.database.clone()),
            Some(dep.schema.clone()),
            format!("{}_REPO", dep.service_name),
        ),
    };
    let repo_fqn = app_fqn(rdb.as_deref(), rsc.as_deref(), &rname).identifier();
    let mut p = Pipeline {
        storage: dep.storage,
        storage_fqn: dep.storage_fqn,
        app_id: dep.package_name.clone(),
        database: dep.database.clone(),
        schema: dep.schema.clone(),
        repo_fqn: repo_fqn.clone(),
        repo_db: rdb,
        repo_schema: rsc,
        repo_name: rname,
        build_eai: opt(&dep.target, "build_eai"),
        build_job_location: opt(&dep.target, "build_job_location"),
        bundle_id: dep.package_name.clone(),
        artifacts: super::yml_artifacts_pub(&dep.target),
        compute_pool: None,
        runtime_image: String::new(),
        project_type: String::new(),
    };
    if let Some(r) = upload_and_build(
        cli,
        &mut p,
        !build_only && !promote_only,
        !upload_only && !promote_only,
        upload_only,
        build_only,
    )? {
        return Ok(r);
    }
    if matches!(field(&dep.target, "postgres_instances"), PyV::List(ref l) if !l.is_empty()) && !cng
    {
        return Err(SnowError::cli(
            "postgres_instances is only valid when compute_resource is SERVERLESS.",
        ));
    }
    let spec = service_spec(&dep.target, &dep.database, &dep.schema, cng)?;
    let ident = dep.service_fqn.identifier();
    cli.console
        .step(&format!("Applying application service {ident}..."));
    if spec.contains("$$") {
        return Err(SnowError::cli(
            "Application service specification must not contain '$$'.",
        ));
    }
    let mut parts = vec![
        format!("CREATE OR ALTER APPLICATION SERVICE {ident}"),
        format!(
            "FROM ARTIFACT REPOSITORY {repo_fqn} PACKAGE {}",
            to_identifier(&dep.package_name)
        ),
        "VERSION LATEST".to_string(),
    ];
    if let Some(c) = &compute {
        parts.push(format!("COMPUTE_RESOURCE = {c}"));
    }
    parts.push(format!("SPECIFICATION = $$\n{spec}$$"));
    if let Err(e) = run(cli, &parts.join("\n"))? {
        log_service_logs(cli, &dep.service_fqn);
        return Err(SnowError::cli(format!(
            "Deployment failed while applying application service '{ident}': {}. Verify privileges for CREATE OR ALTER APPLICATION SERVICE plus USAGE on the warehouse, secrets, and external access integrations referenced by the target.",
            err_str(&e)
        )));
    }
    let url = wait_endpoint(cli, &dep.service_fqn, false)?;
    Ok(Output::message(format!("App ready at {url}")))
}

fn deploy_entity(
    cli: &mut Cli,
    upload_only: bool,
    build_only: bool,
    promote_only: bool,
) -> Result<Output> {
    let (id, e) = super::entity_pub(cli)?;
    let raw = crate::entities::fqn(&e, &id)?;
    let app = raw.name.clone();
    let meta = e.get_some("meta");
    let meta_s = |k: &str| meta.and_then(|m| m.str_of(k)).filter(|s| !s.is_empty());
    let (title, description, icon) = (meta_s("title"), meta_s("description"), meta_s("icon"));
    let d = super::resolve_deploy_defaults(cli, &id, &e)?;
    let g = |k: &str| d.get(k).cloned().flatten();
    let database = g("database").unwrap_or_default();
    let schema = g("schema").unwrap_or_default();
    ensure_app_schema(cli, &database, &schema, &app)?;
    let build_eai = g("build_eai");
    let service_eai = g("service_eai")
        .filter(|s| !s.is_empty())
        .or_else(|| build_eai.clone());
    let storage = super::entity_code_storage_pub(cli, &e, Some(&database), &app);
    let repo_name = g("artifact_repository").unwrap_or_default();
    let (rdb, rsc) = (g("artifact_repo_database"), g("artifact_repo_schema"));
    let repo_fqn = app_fqn(rdb.as_deref(), rsc.as_deref(), &repo_name).identifier();
    let storage_fqn = app_fqn(
        Some(storage.database.as_deref().unwrap_or(&database)),
        Some(storage.schema.as_deref().unwrap_or(&schema)),
        &storage.name,
    );
    let service_fqn = app_fqn(Some(&database), Some(&schema), &app);
    let mut p = Pipeline {
        storage,
        storage_fqn,
        app_id: app.clone(),
        database: database.clone(),
        schema: schema.clone(),
        repo_fqn: repo_fqn.clone(),
        repo_db: rdb,
        repo_schema: rsc,
        repo_name,
        build_eai,
        build_job_location: None,
        bundle_id: id.clone(),
        artifacts: e
            .get("artifacts")
            .and_then(PyV::as_list)
            .cloned()
            .unwrap_or_default(),
        compute_pool: g("build_compute_pool"),
        runtime_image: e.str_of("runtime_image").unwrap_or_default(),
        project_type: e.str_of("spcs_test_project_type").unwrap_or_default(),
    };
    if let Some(r) = upload_and_build(
        cli,
        &mut p,
        !build_only && !promote_only,
        !upload_only && !promote_only,
        upload_only,
        build_only,
    )? {
        return Ok(r);
    }
    let mut comment = serde_json::Map::new();
    comment.insert("appId".into(), serde_json::Value::String(app.clone()));
    for (k, v) in [
        ("appName", &title),
        ("appDescription", &description),
        ("appIcon", &icon),
    ] {
        if let Some(v) = v {
            comment.insert(k.into(), serde_json::Value::String(v.clone()));
        }
    }
    let comment = snow_api::pyfmt::json_dumps(&serde_json::Value::Object(comment), None);
    let ident = service_fqn.identifier();
    cli.console.step("Creating application service...");
    let mut parts = vec![
        format!("CREATE APPLICATION SERVICE {ident}"),
        format!("FROM ARTIFACT REPOSITORY {repo_fqn} PACKAGE {app}"),
        "VERSION LATEST".into(),
    ];
    if let Some(cp) = g("service_compute_pool").filter(|s| !s.is_empty()) {
        parts.push(format!("IN COMPUTE POOL {cp}"));
    }
    if let Some(eai) = &service_eai {
        parts.push(format!("EXTERNAL_ACCESS_INTEGRATIONS = ({eai})"));
    }
    if let Some(wh) = g("query_warehouse").filter(|s| !s.is_empty()) {
        parts.push(format!("QUERY_WAREHOUSE = {wh}"));
    }
    parts.push(format!("COMMENT = '{}'", comment.replace('\'', "''")));
    let mut upgraded = false;
    if let Err(err) = run(cli, &parts.join("\n"))? {
        if err.errno == 2002 && err_str(&err).to_lowercase().contains("already exists") {
            cli.console.step(&format!(
                "Application service {app} already exists. Upgrading..."
            ));
            if let Err(ue) = run(
                cli,
                &format!("ALTER APPLICATION SERVICE {ident} UPGRADE\nTO VERSION LATEST"),
            )? {
                log_service_logs(cli, &service_fqn);
                return Err(SnowError::cli(format!(
                    "Deployment failed while upgrading application service '{ident}': {}. Verify privileges for ALTER APPLICATION SERVICE and access to referenced objects.",
                    err_str(&ue)
                )));
            }
            upgraded = true;
        } else {
            log_service_logs(cli, &service_fqn);
            return Err(SnowError::cli(format!(
                "Deployment failed while creating application service '{ident}': {}. Verify privileges for CREATE APPLICATION SERVICE plus USAGE on configured compute pools, warehouse, and external access integrations.",
                err_str(&err)
            )));
        }
    }
    let url = wait_endpoint(cli, &service_fqn, upgraded)?;
    Ok(Output::message(format!("App ready at {url}")))
}

/// `_wait_for_service_endpoint(manager, service_fqn)` for `snow app open --watch`.
pub fn wait_for_service_endpoint(cli: &mut Cli, f: &Fqn) -> Result<String> {
    let describe = |cli: &mut Cli| -> Result<Desc> {
        Ok(manager::describe_app_service(cli, f)?.unwrap_or_default())
    };
    let initial = describe(cli)?;
    if let Some(u) = manager::url_from_describe(&initial) {
        return Ok(u);
    }
    let ident = f.identifier();
    cli.console.step(&format!(
        "[{}] Waiting for application service '{ident}' to be ready...",
        ts()
    ));
    let timeout = format!(
        "Timed out waiting for application service '{ident}' to become ready. Check application service state and logs:\n  DESCRIBE APPLICATION SERVICE {ident}\n  CALL SYSTEM$GET_APPLICATION_SERVICE_LOGS('{ident}')"
    );
    let failed = regex::Regex::new(r"(?i)\btimed out\b")
        .expect("re")
        .replace(&timeout, "failed")
        .into_owned();
    let fmt = |d: &Desc| -> String {
        if d.is_empty() {
            return "waiting for service to be created...".into();
        }
        let u = desc_str(d, "url");
        if !u.is_empty() {
            return san(&u);
        }
        let st = desc_str(d, "status").trim().to_string();
        if st.is_empty() {
            "url not yet available".into()
        } else {
            san(&st)
        }
    };
    for _ in 0..240 {
        sleep_poll();
        let d = describe(cli)?;
        cli.console.step(&format!("[{}] Status: {}", ts(), fmt(&d)));
        if let Some(u) = manager::url_from_describe(&d) {
            return Ok(u);
        }
        if desc_str(&d, "status").to_uppercase() == "FAILED" {
            return Err(SnowError::cli(format!("{failed} (status={})", fmt(&d))));
        }
    }
    Err(SnowError::cli(format!(
        "{timeout} (timed out after 20 minutes)"
    )))
}
