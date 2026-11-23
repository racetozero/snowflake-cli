//! `snow spcs service build-image`: upload a build context and run the image builder job.

use super::{LogTail, py_bool, strip_empty_lines};
use snow_api::identifiers::{Fqn, to_string_literal};
use snow_api::output::{Output, Pv};
use snow_api::{Cli, Result, SnowError};
use std::path::Path;

fn arg_err(m: String) -> SnowError {
    SnowError::cli(m).named("CliArgumentError")
}

fn simple_name_ok(s: &str) -> bool {
    let t: String = s.chars().filter(|c| !"_-.".contains(*c)).collect();
    !t.is_empty() && t.chars().all(char::is_alphanumeric)
}

/// `get_account_identifier(conn)`: (org, account), lower-cased.
fn account_identifier(cli: &mut Cli) -> Result<(String, String)> {
    let c = cli.execute_query(
        "SELECT CURRENT_ORGANIZATION_NAME() AS org, CURRENT_ACCOUNT_NAME() AS acct",
    )?;
    let rows = cli.rows(&c);
    let Some(r) = rows.first() else {
        return Err(SnowError::click(
            "Could not determine account identifier: CURRENT_ORGANIZATION_NAME() / CURRENT_ACCOUNT_NAME() returned no row.",
        ));
    };
    let g = |k: &str| {
        r.get(k)
            .filter(|v| !matches!(v, Pv::None))
            .map(Pv::py_str)
            .filter(|s| !s.is_empty())
    };
    match (g("ORG"), g("ACCT")) {
        (Some(o), Some(a)) => Ok((o.to_lowercase(), a.to_lowercase())),
        (o, a) => Err(SnowError::click(format!(
            "Could not determine account identifier: CURRENT_ORGANIZATION_NAME()={}, CURRENT_ACCOUNT_NAME()={}.",
            o.map(|x| snow_click::py_repr(&x))
                .unwrap_or_else(|| "None".into()),
            a.map(|x| snow_click::py_repr(&x))
                .unwrap_or_else(|| "None".into())
        ))),
    }
}

fn describe_status(cli: &mut Cli, job: &str) -> Result<Option<String>> {
    let q = format!(
        "describe service {}",
        Fqn::from_string(job)?.sql_identifier()
    );
    let c = cli.execute_query(&q)?;
    let rows = cli.rows(&c);
    Ok(rows.first().and_then(|r| r.get("status")).map(Pv::py_str))
}

/// The stage that holds the build context under `build_contexts/<job>`; `temporary`
/// when the command created it.
struct BuildStage {
    temporary: bool,
    stage: String,
    fqn: Fqn,
    ctx_path: String,
}

impl BuildStage {
    /// A new temporary `<job>_stage`, or the `--stage` the user gave.
    fn new(cli: &mut Cli, job: &str) -> Result<BuildStage> {
        let (temporary, stage, fqn) = match cli.str("stage") {
            None => {
                let s = format!("{job}_stage");
                cli.console.step(&format!("Creating temporary stage: {s}"));
                let f = cli.using_connection(Fqn::from_string(&s)?)?;
                cli.execute_query(&crate::stage::create_query(&f, false, None, false, None))?;
                (true, s, f)
            }
            Some(s) => {
                let f = Fqn::from_string(&s)?;
                cli.console
                    .step(&format!("Using existing stage: {}", f.identifier()));
                (false, s, f)
            }
        };
        Ok(BuildStage {
            temporary,
            stage,
            fqn,
            ctx_path: format!("build_contexts/{job}"),
        })
    }

    /// The statement that cleans up by hand.
    fn manual_cleanup(&self) -> String {
        if self.temporary {
            format!("DROP STAGE {};", self.fqn.sql_identifier())
        } else {
            format!("REMOVE @{}/{};", self.fqn.identifier(), self.ctx_path)
        }
    }

    fn warn_manual_cleanup(&self, cli: &mut Cli) {
        let what = if self.temporary {
            "stage"
        } else {
            "build context"
        };
        cli.console.warning(&format!(
            "Remember to manually clean up {what}: {}",
            self.manual_cleanup()
        ));
    }

    /// Drop the temporary stage, or remove the build context from the user's stage.
    fn cleanup(&self, cli: &mut Cli) -> Result<()> {
        let (stage, ctx_path) = (&self.stage, &self.ctx_path);
        let verbose = cli.connector_info;
        if self.temporary {
            cli.console.step(&format!("Cleaning up stage: {stage}"));
            match cli.try_query(&format!(
                "drop stage if exists {}",
                self.fqn.sql_identifier()
            ))? {
                Ok(_) => cli
                    .console
                    .plain_message(&format!("✓ Dropped stage {stage}")),
                Err(e) => cli.console.warning(&format!(
                    "Failed to clean up stage: {}",
                    e.msg_with(verbose)
                )),
            }
        } else {
            cli.console.step(&format!(
                "Cleaning up build context files from stage: {stage}"
            ));
            let target = snow_api::stage_path::StagePath::from_stage_str(&self.fqn.identifier())
                .join(ctx_path);
            match cli.try_query(&format!("remove {}", target.path_for_sql()))? {
                Ok(_) => cli.console.plain_message(&format!(
                    "✓ Removed build context files from {stage}/{ctx_path}"
                )),
                Err(e) => cli.console.warning(&format!(
                    "Failed to clean up build context files: {}",
                    e.msg_with(verbose)
                )),
            }
        }
        Ok(())
    }
}

pub fn build_image(cli: &mut Cli) -> Result<Output> {
    let pool = cli.str("compute_pool").unwrap_or_default();
    let repo = cli.str("image_repository").unwrap_or_default();
    let image_name = cli.str("image_name").unwrap_or_default();
    let image_tag = cli.str("image_tag").unwrap_or_default();
    let ctx_dir = cli.str("build_context_dir").unwrap_or_default();
    let dockerfile = Path::new(&ctx_dir).join("Dockerfile");
    if !dockerfile.exists() {
        return Err(arg_err(format!(
            "Dockerfile not found in build context directory '{ctx_dir}'. Expected to find: {}",
            dockerfile.display()
        )));
    }
    if !dockerfile.is_file() {
        return Err(arg_err(format!(
            "'{}' exists but is not a file.",
            dockerfile.display()
        )));
    }
    if !simple_name_ok(&image_name) {
        return Err(arg_err(format!(
            "Invalid image name '{image_name}'. Must contain only alphanumeric characters, hyphens, underscores, and dots."
        )));
    }
    if !simple_name_ok(&image_tag) {
        return Err(arg_err(format!(
            "Invalid image tag '{image_tag}'. Must contain only alphanumeric characters, hyphens, underscores, and dots."
        )));
    }
    let uuid = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let job = match cli.str("job_name") {
        None => format!("build_image_{uuid}"),
        Some(j) => {
            if !snow_api::identifiers::is_valid_object_name(&j, 0, true) {
                return Err(arg_err(format!(
                    "Invalid job name '{j}'. Must be a valid unquoted identifier."
                )));
            }
            j
        }
    };
    let stage = BuildStage::new(cli, &job)?;
    let target = format!("@{}/{}", stage.fqn.identifier(), stage.ctx_path);
    cli.console.step(&format!(
        "Uploading build context from {ctx_dir} to {target}"
    ));
    crate::stage::put_tree(cli, Path::new(&ctx_dir), &target, 4, true, false)?;
    cli.console
        .step(&format!("Starting image build job: {job}"));
    let (org, acct) = account_identifier(cli)?;
    let repo_fqn = cli.using_connection(Fqn::from_string(&repo)?)?;
    let (Some(db), Some(schema)) = (repo_fqn.database.clone(), repo_fqn.schema.clone()) else {
        let mut missing = Vec::new();
        if repo_fqn.database.is_none() {
            missing.push("database");
        }
        if repo_fqn.schema.is_none() {
            missing.push("schema");
        }
        return Err(SnowError::unexpected(format!(
            "Image repository requires database and schema. Missing: {}. Either provide a fully qualified name 'database.schema.repository' or set the missing parts in your connection context. Provided: '{repo}'",
            missing.join(", ")
        ))
        .named("ValueError"));
    };
    let registry = format!(
        "{org}-{acct}.registry-local.snowflakecomputing.com/{}/{}/{}",
        db.to_lowercase(),
        schema.to_lowercase(),
        repo_fqn.name.to_lowercase()
    );
    let spec = serde_json::json!({"spec": {
        "containers": [{
            "name": "main",
            "image": "/snowflake/images/snowflake_images/sf-image-build:0.0.3",
            "env": {"IMAGE_REGISTRY_URL": registry, "IMAGE_NAME": image_name, "IMAGE_TAG": image_tag, "BUILD_CONTEXT": "/app"},
            "volumeMounts": [{"name": "code-volume", "mountPath": "/app"}],
        }],
        "volumes": [{"name": "code-volume", "source": "stage", "stageConfig": {"name": target}, "uid": 65532}],
    }});
    let mut q = vec![
        "EXECUTE JOB SERVICE".to_string(),
        format!("IN COMPUTE POOL {pool}"),
        "FROM SPECIFICATION $$".to_string(),
        super::services::serialize_spec(&spec),
        "$$".to_string(),
        format!("NAME = {job}"),
        "ASYNC = TRUE".to_string(),
    ];
    let eai = cli.list("external_access_integrations");
    if !eai.is_empty() {
        q.push(format!(
            "EXTERNAL_ACCESS_INTEGRATIONS = ({})",
            eai.join(",")
        ));
    }
    let verbose = cli.connector_info;
    let cursor = cli
        .try_execute(&strip_empty_lines(&q))
        .map_err(|e| super::already_exists(e, "service", &job, false, verbose))?;
    cli.console.step("Waiting for job to start...");
    cli.console
        .message("Checking job status every 5 seconds...");
    let mut status: Option<String> = None;
    let mut elapsed = 0;
    while elapsed < 300 {
        match describe_status(cli, &job) {
            Ok(s) => {
                if let Some(s) = &s {
                    cli.console
                        .plain_message(&format!("Current job status: {s}"));
                }
                status = s.or(status);
                if status.as_deref().is_some_and(|s| s != "PENDING") {
                    break;
                }
            }
            Err(_) => cli.console.message(&format!(
                "Waiting for job to be available... ({elapsed}s elapsed)"
            )),
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
        elapsed += 5;
    }

    let Some(current) = status.clone().filter(|s| s != "PENDING") else {
        cli.console.warning(&format!(
            "Job did not start within 300s (status: {})",
            status.unwrap_or_else(|| "UNKNOWN".into())
        ));
        cli.console.plain_message(&format!(
            "Use 'snow spcs service status {job}' to check status."
        ));
        stage.warn_manual_cleanup(cli);
        return Ok(cli.single_query_result(&cursor));
    };
    cli.console
        .step(&format!("Job status: {current}. Streaming logs..."));
    cli.console.message("");
    let mut tail = LogTail::default();
    let final_status = loop {
        let lq = format!(
            "call SYSTEM$GET_SERVICE_LOGS({}, '0', 'main', 1000, {}, {}, {});",
            to_string_literal(&Fqn::from_string(&job)?.identifier()),
            py_bool(false),
            to_string_literal(&tail.since),
            py_bool(true)
        );
        let c = cli.execute_query(&lq)?;
        for l in tail.take_new(&c) {
            cli.console
                .plain_message(l.split_once(' ').map_or(l.as_str(), |x| x.1));
        }
        if let Some(s) = describe_status(cli, &job)?
            && ["DONE", "FAILED", "CANCELLED"].contains(&s.as_str())
        {
            break s;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    };
    cli.console.message("");
    match final_status.as_str() {
        "DONE" => cli.console.plain_message(&format!(
            "✓ Image build job '{job}' completed successfully."
        )),
        "FAILED" => cli
            .console
            .warning(&format!("✗ Image build job '{job}' failed.")),
        "CANCELLED" => cli
            .console
            .warning(&format!("✗ Image build job '{job}' was cancelled.")),
        _ => {}
    }
    stage.cleanup(cli)?;
    Ok(cli.single_query_result(&cursor))
}

pub fn remote_build(cli: &mut Cli) -> Result<Output> {
    let ctx_dir = cli.str("build_context_dir").unwrap_or_default();
    let location = cli.str("location");
    let name = cli.str("name");
    let image_tag = cli.str("image_tag");
    let build_type = cli.str("build_type").unwrap_or_else(|| "image".into());
    if build_type != "image" && build_type != "app" {
        return Err(arg_err(format!(
            "Invalid build type '{build_type}'. Must be 'image' or 'app'."
        )));
    }
    if build_type == "app" && location.is_none() {
        return Err(arg_err(
            "--location is required for app builds. Provide the fully-qualified ARTIFACT REPOSITORY name (db.schema.repo).".into(),
        ));
    }
    if build_type == "image" {
        let d = Path::new(&ctx_dir).join("Dockerfile");
        if !d.is_file() {
            return Err(arg_err(format!(
                "Dockerfile not found in build context directory '{ctx_dir}'. Expected to find: {}",
                d.display()
            )));
        }
    }
    if let Some(n) = name
        .as_ref()
        .filter(|n| !n.is_empty() && !simple_name_ok(n))
    {
        return Err(arg_err(format!(
            "Invalid name '{n}'. Must contain only alphanumeric characters, hyphens, underscores, and dots."
        )));
    }
    if let Some(t) = image_tag
        .as_ref()
        .filter(|t| !t.is_empty() && !simple_name_ok(t))
    {
        return Err(arg_err(format!(
            "Invalid image tag '{t}'. Must contain only alphanumeric characters, hyphens, underscores, and dots."
        )));
    }
    let uuid = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let local_job = format!("remote_build_{uuid}");
    let stage = BuildStage::new(cli, &local_job)?;
    let target = format!("@{}/{}", stage.fqn.identifier(), stage.ctx_path);
    cli.console.step(&format!(
        "Uploading build context from {ctx_dir} to {target}"
    ));
    let build_source = snow_api::stage_path::StagePath::from_stage_str(&target).absolute_path();
    let submitted = (|| -> Result<String> {
        crate::stage::put_tree(cli, Path::new(&ctx_dir), &target, 4, true, false)?;
        cli.console.step("Submitting remote build via REST API...");
        let mut body = serde_json::Map::new();
        body.insert("build_type".into(), serde_json::json!(build_type));
        body.insert("build_source".into(), serde_json::json!(build_source));
        for (k, v) in [
            ("location", location.clone()),
            ("name", name.clone()),
            ("image_tag", image_tag.clone()),
            ("project_type", cli.str("project_type")),
            ("compute_pool", cli.str("compute_pool")),
            ("validation_profile", cli.str("validation_profile")),
        ] {
            if let Some(v) = v.filter(|v| !v.is_empty()) {
                body.insert(k.into(), serde_json::json!(v));
            }
        }
        let eai = cli.list("external_access_integrations");
        if !eai.is_empty() {
            body.insert(
                "config".into(),
                serde_json::json!(snow_api::pyfmt::json_dumps(
                    &serde_json::json!({"external_access_integrations": eai}),
                    None
                )),
            );
        }
        let path = "/api/v2/remote-build/execute";
        let (code, v) = cli
            .connection()?
            .rest("POST", path, Some(&serde_json::Value::Object(body)))
            .map_err(|e| SnowError::cli(e.msg()))?;
        if code >= 300 {
            let url = format!("{}{path}", cli.connection()?.base_url.clone());
            return Err(super::services::remote_build_error(
                code,
                "submit build",
                &url,
            ));
        }
        match v
            .get("job_name")
            .and_then(|j| j.as_str())
            .filter(|j| !j.is_empty())
        {
            Some(j) => Ok(j.to_string()),
            None => Err(SnowError::cli(format!(
                "Remote build API did not return a job_name. Response: {}",
                snow_api::output::Pv::from_json(&v).py_repr()
            ))),
        }
    })();
    let job = match submitted {
        Ok(j) => j,
        Err(e) => {
            stage.cleanup(cli)?;
            return Err(e);
        }
    };
    cli.console.step(&format!("Build job submitted: {job}"));
    cli.console.message("Waiting for job to complete (polling REST every 5s; streaming logs via SQL once available)...");
    cli.console.message("");
    let mut current: Option<String> = None;
    let mut elapsed = 0;
    let mut tail = LogTail::default();
    let mut terminal = false;
    while elapsed < 1800 {
        let status = super::services::fetch_remote_job(cli, &job).map(|v| {
            v.map(|v| {
                v.get("job_status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("UNKNOWN")
                    .to_string()
            })
        });
        match status {
            Ok(Some(s)) => {
                if current.as_deref() != Some(s.as_str()) {
                    cli.console
                        .plain_message(&format!("Current job status: {s}"));
                }
                current = Some(s.clone());
                if ["DONE", "FAILED", "CANCELLED"].contains(&s.as_str()) {
                    terminal = true;
                    break;
                }
            }
            Ok(None) => cli.console.message("Waiting for job to appear..."),
            Err(
                e @ SnowError::Cli {
                    name: "RemoteBuildPermanentError",
                    ..
                },
            ) => {
                cli.console.warning(&format!(
                    "\nRemote build job '{job}' was already submitted; its status could not be confirmed (see error below)."
                ));
                cli.console.plain_message(&format!(
                    "Once resolved, check its status with: snow spcs service remote-build-status --job-name {job}"
                ));
                stage.warn_manual_cleanup(cli);
                return Err(e);
            }
            Err(e) => cli.console.warning(&format!(
                "REST status check failed ({}); retrying (1/3)...",
                e.message()
            )),
        }
        let lq = format!(
            "call SYSTEM$GET_SERVICE_LOGS({}, '0', 'main', 1000, False, {}, True);",
            to_string_literal(&job),
            to_string_literal(&tail.since)
        );
        if let Ok(Ok(c)) = cli.try_query(&lq) {
            for l in tail.take_new(&c) {
                cli.console
                    .plain_message(l.split_once(' ').map_or(l.as_str(), |x| x.1));
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
        elapsed += 5;
    }
    cli.console.message("");
    match current.as_deref() {
        Some("DONE") => cli.console.plain_message(&format!(
            "✓ Remote build job '{job}' completed successfully."
        )),
        Some("FAILED") => cli
            .console
            .warning(&format!("✗ Remote build job '{job}' failed.")),
        Some("CANCELLED") => cli
            .console
            .warning(&format!("✗ Remote build job '{job}' was cancelled.")),
        _ => {}
    }
    if terminal {
        stage.cleanup(cli)?;
        if current.as_deref() == Some("DONE") {
            return Ok(Output::message(format!(
                "Remote build job '{job}' completed successfully."
            )));
        }
        return Err(SnowError::cli(format!(
            "Remote build job '{job}' finished with status: {}.",
            current.unwrap_or_default()
        )));
    }
    let hint = stage.manual_cleanup();
    Err(SnowError::cli(format!(
        "Remote build job '{job}' did not reach a terminal state within the 1800s wait budget (last known status: {}). The job may still be running.\n  • Check status: snow spcs service remote-build-status --job-name {job}\n  • Clean up stage manually once done: {hint}",
        current
            .map(|c| snow_click::py_repr(&c))
            .unwrap_or_else(|| "None".into())
    )))
}
