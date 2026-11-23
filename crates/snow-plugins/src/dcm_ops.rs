//! `snow dcm deploy|plan|preview|purge|raw-analyze|refresh|test`: the upload step,
//! the step checklist (`MultiStepProgress` as printed when stdout is not a terminal),
//! server polling and the result reporters (`_plugins/dcm/{progress,multistep_progress,reporters}`).

use snow_api::identifiers::Fqn;
use snow_api::output::{Output, Pv};
use snow_api::{Cli, Result, SnowError};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Running,
    Done,
    Failed,
}

/// A step of a DCM command. The server steps match the `phase` of
/// `SYSTEM$GET_DCM_PROJECT_PROGRESS`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Upload,
    Render,
    Compile,
    Plan,
    Deploy,
    PostHook,
    Test,
    Expectations,
    Refresh,
    Preview,
    Analyze,
}

impl Step {
    fn key(self) -> &'static str {
        match self {
            Step::Upload => "UPLOAD",
            Step::Render => "RENDER",
            Step::Compile => "COMPILE",
            Step::Plan => "PLAN",
            Step::Deploy => "DEPLOY",
            Step::PostHook => "POST_HOOK",
            Step::Test => "TEST",
            Step::Expectations => "EXPECTATIONS",
            Step::Refresh => "REFRESH",
            Step::Preview => "PREVIEW",
            Step::Analyze => "ANALYZE",
        }
    }
}

/// `MultiStepProgress` in its non-TTY form: one line per state change.
struct Progress {
    steps: Vec<(Step, &'static str, State, Option<Instant>)>,
}

impl Progress {
    fn new(steps: &[(Step, &'static str)]) -> Progress {
        Progress {
            steps: steps
                .iter()
                .map(|(k, l)| (*k, *l, State::Pending, None))
                .collect(),
        }
    }
    fn idx(&self, key: Step) -> usize {
        self.steps
            .iter()
            .position(|s| s.0 == key)
            .expect("known step")
    }
    fn line(&self, cli: &mut Cli, i: usize, verb: &str) {
        let n = self.steps.len();
        cli.console.step(&format!(
            "❯ Step {}/{n} - {} - {verb}",
            i + 1,
            self.steps[i].1
        ));
    }
    /// Rich's `stop_task` starts a never-started task, so a finished step always shows its time.
    fn elapsed(&self, i: usize) -> String {
        match self.steps[i].3.or(Some(Instant::now())) {
            Some(t) => {
                let secs = t.elapsed().as_secs();
                let (h, rest) = (secs / 3600, secs % 3600);
                let (m, s) = (rest / 60, rest % 60);
                let mut parts = Vec::new();
                if h > 0 {
                    parts.push(format!("{h}h"));
                }
                if h > 0 || m > 0 {
                    parts.push(format!("{m}m"));
                }
                parts.push(format!("{s}s"));
                format!(" ({})", parts.join(" "))
            }
            None => String::new(),
        }
    }
    fn start(&mut self, cli: &mut Cli, key: Step) {
        let i = self.idx(key);
        let already = self.steps[i].2 == State::Running;
        if self.steps[i].3.is_none() {
            self.steps[i].3 = Some(Instant::now());
        }
        self.steps[i].2 = State::Running;
        if !already {
            self.line(cli, i, "⠋ Running...");
        }
    }
    fn complete(&mut self, cli: &mut Cli, key: Step) {
        let i = self.idx(key);
        self.steps[i].2 = State::Done;
        let e = self.elapsed(i);
        self.line(cli, i, &format!("✓ Completed{e}"));
    }
    fn fail(&mut self, cli: &mut Cli, key: Step) {
        let i = self.idx(key);
        self.steps[i].2 = State::Failed;
        let e = self.elapsed(i);
        self.line(cli, i, &format!("✗ Failed{e}"));
    }
    fn terminal(&self, key: Step) -> bool {
        matches!(self.steps[self.idx(key)].2, State::Done | State::Failed)
    }
    fn fail_running(&mut self, cli: &mut Cli) {
        if let Some(i) = self.steps.iter().position(|s| s.2 == State::Running) {
            let key = self.steps[i].0;
            self.fail(cli, key);
        }
    }
    /// `run_step(key, fn)`.
    fn run<T>(
        &mut self,
        cli: &mut Cli,
        key: Step,
        f: impl FnOnce(&mut Cli, &mut Progress) -> Result<T>,
    ) -> Result<T> {
        self.start(cli, key);
        match f(cli, self) {
            Ok(v) => {
                self.complete(cli, key);
                Ok(v)
            }
            Err(e) => {
                self.fail(cli, key);
                Err(e)
            }
        }
    }
}

/// `progress_session(progress)`: a blank line once the session ends.
fn session<T>(
    cli: &mut Cli,
    p: &mut Progress,
    f: impl FnOnce(&mut Cli, &mut Progress) -> Result<T>,
) -> Result<T> {
    let r = f(cli, p);
    if r.is_err() {
        p.fail_running(cli);
    }
    if !cli.console.silent {
        cli.console.message("");
    }
    r
}

/// A node of the `Upload files` tree.
struct Node {
    label: String,
    kids: Vec<Node>,
}

fn printable(name: &str) -> String {
    let s = snow_api::sanitize::for_terminal(name);
    if s.chars().all(|c| !c.is_control()) {
        s
    } else {
        format!("{s:?}").trim_matches('"').to_string()
    }
}

fn folder_label(name: &str, count: usize) -> String {
    match count {
        0 => name.to_string(),
        1 => format!("{name} (1 file)"),
        n => format!("{name} ({n} files)"),
    }
}

/// Rich's tree measure: the widest `label + 4 * depth`.
fn measure(n: &Node, depth: usize) -> usize {
    let own = n.label.chars().count() + 4 * depth;
    n.kids
        .iter()
        .map(|k| measure(k, depth + 1))
        .fold(own, usize::max)
}

fn tree_lines(n: &Node, guides: &str, last: bool, depth: usize, out: &mut Vec<String>) {
    if depth == 0 {
        out.push(n.label.clone());
    } else {
        out.push(format!(
            "{guides}{}{}",
            if last { "└─ " } else { "├─ " },
            n.label
        ));
    }
    let child_guides = if depth == 0 {
        String::new()
    } else {
        format!("{guides}{}", if last { "   " } else { "│  " })
    };
    let count = n.kids.len();
    for (i, k) in n.kids.iter().enumerate() {
        tree_lines(k, &child_guides, i + 1 == count, depth + 1, out);
    }
}

/// `upload_details(stage_fqn, relative_paths)` printed beneath the UPLOAD step.
fn print_upload_details(cli: &mut Cli, stage: &Fqn, rel: &[String]) {
    let prefix = stage.prefix();
    let scope = if prefix.is_empty() {
        String::new()
    } else {
        format!(" inside {prefix}")
    };
    cli.console
        .plain_message(&format!("  • Create temporary stage{scope}"));
    let mut root_files: Vec<String> = rel.iter().filter(|r| !r.contains('/')).cloned().collect();
    root_files.sort();
    let mut direct: std::collections::BTreeMap<String, usize> = Default::default();
    let mut sub: std::collections::BTreeMap<String, std::collections::BTreeMap<String, usize>> =
        Default::default();
    for r in rel {
        let parts: Vec<&str> = r.split('/').collect();
        let folders = &parts[..parts.len() - 1];
        match folders.len() {
            0 => {}
            1 => *direct.entry(folders[0].to_string()).or_default() += 1,
            _ => {
                *sub.entry(folders[0].to_string())
                    .or_default()
                    .entry(folders[1].to_string())
                    .or_default() += 1
            }
        }
    }
    let mut names: Vec<String> = direct.keys().chain(sub.keys()).cloned().collect();
    names.sort();
    names.dedup();
    if root_files.is_empty() && names.is_empty() {
        return;
    }
    let mut root = Node {
        label: "Upload files".into(),
        kids: vec![],
    };
    for f in root_files {
        root.kids.push(Node {
            label: printable(&f),
            kids: vec![],
        });
    }
    for n in names {
        let kids = sub
            .get(&n)
            .map(|m| {
                m.iter()
                    .map(|(s, c)| Node {
                        label: folder_label(&printable(s), *c),
                        kids: vec![],
                    })
                    .collect()
            })
            .unwrap_or_default();
        root.kids.push(Node {
            label: folder_label(&printable(&n), direct.get(&n).copied().unwrap_or(0)),
            kids,
        });
    }
    let width = measure(&root, 0);
    let mut lines = Vec::new();
    tree_lines(&root, "", true, 0, &mut lines);
    for (i, l) in lines.iter().enumerate() {
        let pad = width.saturating_sub(l.chars().count());
        let bullet = if i == 0 { "• " } else { "  " };
        cli.console
            .plain_message(&format!("  {bullet}{l}{}", " ".repeat(pad)));
    }
}

fn is_hidden(rel: &str) -> bool {
    rel.split('/').any(|p| p.starts_with('.'))
}

/// `DCM_<project>_<unix seconds>_<suffix>` next to the project, completed from the connection.
fn temp_stage(cli: &mut Cli, project: &Fqn, suffix: &str) -> Result<Fqn> {
    let safe = snow_api::identifiers::sanitize_identifier(
        &snow_api::identifiers::unquote_identifier(&project.name),
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    crate::stage::temp_stage_name(cli, project, format!("DCM_{safe}_{now}_{suffix}"))
}

/// `DCMProjectManager.sync_local_files(...)`: the temporary stage name.
fn sync_local_files(cli: &mut Cli, project: &Fqn, source: &Path) -> Result<String> {
    let source = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    let stage = temp_stage(cli, project, "TMP_STAGE")?;
    let stage_id = stage.identifier();
    // `_build_upload_plan`: manifest.yml, sources/ (hidden files uploaded one by one).
    let mut rel = vec!["manifest.yml".to_string()];
    let mut artifacts = vec![snow_api::project::PyV::s("manifest.yml")];
    let mut individual: Vec<(PathBuf, String)> = Vec::new();
    let sources = source.join("sources");
    if sources.is_dir() {
        let mut m = indexmap::IndexMap::new();
        m.insert("src".to_string(), snow_api::project::PyV::s("sources"));
        m.insert(
            "ignore".to_string(),
            snow_api::project::PyV::List(vec![snow_api::project::PyV::s(".*")]),
        );
        artifacts.push(snow_api::project::PyV::Dict(m));
        for f in crate::artifacts::walk(&sources)
            .into_iter()
            .filter(|f| !f.is_dir())
        {
            let r = f
                .strip_prefix(&sources)
                .unwrap_or(&f)
                .to_string_lossy()
                .replace('\\', "/");
            rel.push(format!("sources/{r}"));
            if is_hidden(&r) {
                let parent = Path::new(&r)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let dest = if parent.is_empty() {
                    format!("{stage_id}/sources")
                } else {
                    format!("{stage_id}/sources/{parent}")
                };
                individual.push((f, dest));
            }
        }
    }
    // `_add_assets`: resolved asset files not already shipped.
    let mut asset_files: Vec<String> = Vec::new();
    for r in resolve_asset_paths(cli, &source, &crate::dcm::declared_assets(&source)) {
        if !rel.contains(&r) {
            rel.push(r.clone());
            asset_files.push(r);
        }
    }
    let tmp = tempfile::tempdir()?;
    for r in &asset_files {
        crate::artifacts::symlink_or_copy_tree(
            &source.join(r),
            &tmp.path().join(r),
            tmp.path(),
            &source,
        )?;
    }
    let mut map = crate::artifacts::BundleMap::new(&source, tmp.path());
    for a in &artifacts {
        let (src, dest, ignore) = crate::artifacts::mapping_of(a);
        map.add(&src, dest.as_deref(), &ignore)?;
    }
    for (s, d) in map.all_mappings_expanded() {
        if s.is_file() {
            crate::artifacts::symlink_or_copy_tree(&s, &d, tmp.path(), &source)?;
        }
    }
    print_upload_details(cli, &stage, &rel);
    cli.execute_query(&crate::stage::create_query(
        &Fqn::from_stage(&stage_id)?,
        true,
        None,
        false,
        None,
    ))?;
    crate::stage::put_tree(cli, tmp.path(), &stage_id, 1, false, false)?;
    for (f, dest) in individual {
        crate::stage::put(
            cli,
            &f.to_string_lossy(),
            &snow_api::stage_path::StagePath::from_stage_str(&dest),
            4,
            false,
            false,
        )?;
    }
    Ok(stage_id)
}

/// The resolved project and `--from` directory for one command.
struct Context {
    project: Fqn,
    configuration: Option<String>,
    from: PathBuf,
}

fn context(cli: &mut Cli, required_manifest: bool) -> Result<Context> {
    let (project, configuration) = crate::dcm::resolve(cli, required_manifest)?;
    Ok(Context {
        project,
        configuration,
        from: crate::dcm::from_dir(cli),
    })
}

/// `_get_configuration_and_variables_query(configuration, variables)`.
fn using_clause(cli: &Cli, configuration: Option<&str>) -> Result<String> {
    let vars = cli.list("variables");
    let mut q = String::new();
    if configuration.is_some() || !vars.is_empty() {
        q.push_str(" USING");
    }
    if let Some(c) = configuration {
        q.push_str(&format!(" CONFIGURATION {c}"));
    }
    if !vars.is_empty() {
        let parsed = crate::common::parse_key_value_variables(&vars)?;
        let items: Vec<String> = parsed
            .iter()
            .map(|v| format!("{}=>{}", v.key, v.value))
            .collect();
        q.push_str(&format!(" ({})", items.join(", ")));
    }
    Ok(q)
}

fn from_stage(stage: &str) -> String {
    format!(
        " FROM {}",
        snow_api::stage_path::StagePath::from_stage_str(stage).absolute_path()
    )
}

/// `Reporter.process(cursor)` for the single-cell JSON result reporters.
fn first_json(
    cli: &mut Cli,
    c: &snow_connector::Cursor,
) -> Result<Option<(serde_json::Value, String)>> {
    let Some(row) = c.rows.first() else {
        cli.console.styled_message("No data.\n", "");
        return Ok(None);
    };
    let cell = row.first().cloned().flatten().unwrap_or_default();
    match serde_json::from_str::<serde_json::Value>(&cell) {
        Ok(v) => Ok(Some((v, cell))),
        Err(_) => Err(SnowError::cli("Could not process response.")),
    }
}

/// The tail of a single-cell JSON command: save and report the response, then the
/// format-aware result.
fn finish_json(
    cli: &mut Cli,
    c: &snow_connector::Cursor,
    command: &str,
    report: impl FnOnce(&mut Cli, &serde_json::Value) -> Result<()>,
) -> Result<Output> {
    let Some((v, raw)) = first_json(cli, c)? else {
        return Ok(Output::Empty);
    };
    save_response(cli, command, &v)?;
    report(cli, &v)?;
    Ok(format_aware(cli, c, raw))
}

/// `format_aware_result`: nothing more for TABLE, the raw cell for JSON/CSV.
fn format_aware(cli: &Cli, c: &snow_connector::Cursor, raw: String) -> Output {
    if cli.format == snow_api::output::Format::Table {
        return Output::Empty;
    }
    let col = c.columns.first();
    let name = col.map(|c| c.name.clone()).unwrap_or_default();
    // `RespectingColumnTypesRowMapper`: VARIANT/OBJECT cells are decoded, text stays text.
    let structured = col.is_some_and(|c| {
        ["variant", "object", "array"].contains(&c.type_name.to_lowercase().as_str())
    });
    let v = if structured {
        serde_json::from_str::<serde_json::Value>(&raw)
            .map(|j| Pv::from_json(&j))
            .unwrap_or(Pv::s(raw))
    } else {
        Pv::s(raw)
    };
    let mut row = snow_api::output::Row::new();
    row.insert(name, v);
    Output::Collection(vec![row])
}

fn print_summary(cli: &mut Cli, parts: &[(String, &str)]) {
    cli.console.styled_message("\n", "");
    for (t, s) in parts {
        cli.console.styled_message(t, s);
    }
    cli.console.styled_message("\n", "");
}

fn finish(cli: &mut Cli, ok: bool, parts: Vec<(String, &str)>) -> Result<()> {
    if ok {
        print_summary(cli, &parts);
        Ok(())
    } else {
        Err(SnowError::cli(
            parts.iter().map(|p| p.0.clone()).collect::<String>(),
        ))
    }
}

fn format_number(n: i64) -> String {
    let a = n.unsigned_abs() as f64;
    let units = [
        (1e18, "E"),
        (1e15, "P"),
        (1e12, "T"),
        (1e9, "B"),
        (1e6, "M"),
        (1e3, "k"),
    ];
    for (t, s) in units {
        if a >= t {
            let v = a / t;
            return if (v * 10.0).round() / 10.0 >= 1000.0 {
                format!("{}{s}", v as i64)
            } else {
                format!("{v:.1}{s}").replace(".0", "")
            };
        }
    }
    n.to_string()
}

/// `RefreshReporter`.
fn report_refresh(cli: &mut Cli, v: &serde_json::Value) -> Result<()> {
    if !v.is_object() {
        return Err(SnowError::cli("Could not process response."));
    }
    let tables = v["dts_refresh_result"]["refreshed_tables"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let (mut up, mut refreshed, mut unknown) = (0, 0, 0);
    for t in &tables {
        let name = snow_api::sanitize::for_terminal(t["table_name"].as_str().unwrap_or("UNKNOWN"));
        let stats = &t["statistics"];
        let int = |x: &serde_json::Value| {
            x.as_i64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
                .unwrap_or(0)
        };
        let (status, ins, del) = if stats.is_object() {
            let (i, d) = (int(&stats["inserted_rows"]), int(&stats["deleted_rows"]));
            if i == 0 && d == 0 {
                up += 1;
                ("UP-TO-DATE", i, d)
            } else {
                refreshed += 1;
                ("REFRESHED", i, d)
            }
        } else {
            unknown += 1;
            ("UNKNOWN", 0, 0)
        };
        let fmt = |n: i64, sign: &str| -> String {
            if status == "UNKNOWN" {
                return String::new();
            }
            let f = format_number(n);
            if f != "0" { format!("{sign}{f}") } else { f }
        };
        cli.console
            .styled_message(&format!("{status:<11} "), "#a0a8fe");
        cli.console
            .styled_message(&format!("{:>7} ", fmt(ins, "+")), "green italic");
        cli.console
            .styled_message(&format!("{:>7} ", fmt(del, "-")), "red italic");
        cli.console.styled_message(&name, "cyan");
        cli.console.styled_message("\n", "");
    }
    let total = up + refreshed + unknown;
    let summary = if total == 0 {
        "No dynamic tables found in the project.".to_string()
    } else {
        let mut parts = Vec::new();
        if refreshed > 0 {
            parts.push(format!("{refreshed} refreshed"));
        }
        if up > 0 {
            parts.push(format!("{up} up-to-date"));
        }
        if unknown > 0 {
            parts.push(format!("{unknown} unknown"));
        }
        format!("{}.", parts.join(", "))
    };
    finish(cli, unknown == 0, vec![(summary, "")])
}

/// `TestReporter`.
fn report_test(cli: &mut Cli, v: &serde_json::Value) -> Result<()> {
    if !v.is_object() {
        return Err(SnowError::cli("Could not process response."));
    }
    let list = match v.get("expectations") {
        None => vec![],
        Some(serde_json::Value::Array(a)) => a.clone(),
        Some(_) => return Err(SnowError::cli("Could not process response.")),
    };
    let (mut pass, mut fail, mut unknown) = (0, 0, 0);
    for e in &list {
        if !e.is_object() {
            continue;
        }
        let g = |k: &str| {
            snow_api::sanitize::for_terminal(&match e.get(k) {
                None => "UNKNOWN".to_string(),
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Null) => "None".to_string(),
                Some(serde_json::Value::Bool(b)) => crate::spcs::py_bool(*b).to_string(),
                Some(other) => other.to_string(),
            })
        };
        let (text, style) = match e.get("expectation_violated") {
            Some(serde_json::Value::Bool(true)) => {
                fail += 1;
                ("✗ FAIL", "red")
            }
            Some(serde_json::Value::Bool(false)) => {
                pass += 1;
                ("✓ PASS", "green")
            }
            _ => {
                unknown += 1;
                ("? UNKNOWN", "#a0a8fe")
            }
        };
        cli.console.styled_message(&format!("{:<11} ", text), style);
        cli.console.styled_message(&g("table_name"), "cyan");
        cli.console
            .styled_message(&format!(" ({})", g("expectation_name")), "");
        cli.console.styled_message("\n", "");
        if text == "✗ FAIL" {
            cli.console.styled_message(
                &format!(
                    "  └─ Expected: {}, Got: {} (Metric: {})\n",
                    g("expectation_expression"),
                    g("value"),
                    g("metric_name")
                ),
                "",
            );
        }
    }
    let total = pass + fail + unknown;
    let parts = if total == 0 {
        vec![("No expectations found in the project.".to_string(), "")]
    } else {
        let mut p = vec![
            (format!("{pass} passed"), "green"),
            (", ".to_string(), ""),
            (format!("{fail} failed"), "red"),
        ];
        if unknown > 0 {
            p.push((", ".into(), ""));
            p.push((format!("{unknown} unknown"), "red"));
        }
        p.push((" out of ".into(), ""));
        p.push((format!("{total}"), "bold"));
        p.push((" total.".into(), ""));
        p
    };
    finish(cli, fail + unknown == 0, parts)
}

pub fn refresh(cli: &mut Cli) -> Result<Output> {
    with_artifacts(cli, refresh_inner)
}

fn refresh_inner(cli: &mut Cli) -> Result<Output> {
    let ctx = context(cli, false)?;
    let mut p = Progress::new(&[(Step::Refresh, "REFRESH")]);
    let c = session(cli, &mut p, |cli, p| {
        p.run(cli, Step::Refresh, |cli, _| {
            cli.execute_query(&format!(
                "EXECUTE DCM PROJECT {} REFRESH ALL",
                ctx.project.sql_identifier()
            ))
        })
    })?;
    finish_json(cli, &c, "refresh", report_refresh)
}

pub fn test(cli: &mut Cli) -> Result<Output> {
    with_artifacts(cli, test_inner)
}

fn test_inner(cli: &mut Cli) -> Result<Output> {
    let scripts = cli.list("scripts");
    let all = cli.flag("all_scripts");
    if !scripts.is_empty() && all {
        return Err(SnowError::cli(
            "--script and --all-scripts cannot be used together.",
        ));
    }
    let use_scripts = !scripts.is_empty() || all;
    let use_expectations = cli.flag("expectations") || !use_scripts;
    let ctx = if use_scripts {
        context(cli, true).map_err(|e| {
            SnowError::cli(format!(
                "--script and --all-scripts deploy the project to a fresh data branch before testing it, so they require a manifest.yml. {}",
                e.message()
            ))
        })?
    } else {
        context(cli, false)?
    };
    let flag_on = cli
        .config
        .feature(snow_api::config::FeatureFlag::EnableDcmUnitTestFeatures);
    let label = if use_scripts || flag_on {
        "EXPECTATIONS"
    } else {
        "TEST"
    };
    let mut steps: Vec<(Step, &str)> = Vec::new();
    if use_scripts {
        steps.extend([(Step::Upload, "UPLOAD"), (Step::Test, "TEST")]);
    }
    if use_expectations {
        steps.push((Step::Expectations, label));
    }
    let mut p = Progress::new(&steps);
    let combined = use_scripts && use_expectations;
    let mut outcomes: Vec<(&str, snow_connector::Cursor)> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    session(cli, &mut p, |cli, p| {
        if use_scripts {
            let env = resolve_env(cli)?;
            let r = (|| -> Result<snow_connector::Cursor> {
                let stage = p.run(cli, Step::Upload, |cli, _| {
                    sync_local_files(cli, &ctx.project, &ctx.from)
                })?;
                let out = output_stage(cli, &ctx.project)?;
                let r = p.run(cli, Step::Test, |cli, _| {
                    let mut q =
                        format!("EXECUTE DCM PROJECT {} TEST", ctx.project.sql_identifier());
                    q.push_str(&using_env(cli, ctx.configuration.as_deref(), &env)?);
                    q.push_str(&from_stage(&stage));
                    if !scripts.is_empty() {
                        let names: Vec<String> = scripts
                            .iter()
                            .map(|n| format!("\"{}\"", n.replace('"', "\"\"")))
                            .collect();
                        q.push_str(&format!(" SCRIPTS ({})", names.join(", ")));
                    }
                    if let Some(o) = &out {
                        q.push_str(&format!(" OUTPUT_PATH {o}/test"));
                    }
                    exec_env(cli, &q, &env)
                });
                download_outputs(cli, &out);
                r
            })();
            match r {
                Ok(c) => outcomes.push(("unit_test", c)),
                Err(e) if combined => failures.push(e.message()),
                Err(e) => return Err(e),
            }
        }
        if use_expectations {
            if flag_on {
                cli.console.warning(
                    "⚠️  This checks expectations against already-deployed objects. The newer --script/--all-scripts option runs test scripts against a fresh, isolated data branch instead, and will eventually replace this check.",
                );
            }
            let r = p.run(cli, Step::Expectations, |cli, _| {
                cli.execute_query(&format!(
                    "EXECUTE DCM PROJECT {} TEST ALL",
                    ctx.project.sql_identifier()
                ))
            });
            match r {
                Ok(c) => outcomes.push(("test", c)),
                Err(e) if combined => failures.push(e.message()),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })?;
    let mut result = Output::Empty;
    let mut raws: Vec<(&str, snow_connector::Cursor, String)> = Vec::new();
    for (name, c) in &outcomes {
        let r = (|| -> Result<Option<String>> {
            let Some((v, raw)) = first_json(cli, c)? else {
                return Ok(None);
            };
            save_response(cli, name, &v)?;
            if *name == "test" {
                report_test(cli, &v)?;
            } else {
                report_unit_test(cli, &v)?;
            }
            Ok(Some(raw))
        })();
        match r {
            Ok(Some(raw)) => {
                result = format_aware(cli, c, raw.clone());
                raws.push((name, c.clone(), raw));
            }
            Ok(None) => result = Output::Empty,
            Err(e) => failures.push(e.message()),
        }
    }
    if !failures.is_empty() {
        return Err(SnowError::cli(failures.join("\n")));
    }
    if outcomes.len() == 1 || cli.format == snow_api::output::Format::Table {
        return Ok(result);
    }
    let mut row = snow_api::output::Row::new();
    for (name, c, raw) in raws {
        if let Output::Collection(rows) = format_aware(cli, &c, raw)
            && let Some(v) = rows.into_iter().next().and_then(|r| r.into_iter().next())
        {
            row.insert(name.to_string(), v.1);
        }
    }
    Ok(Output::Collection(vec![row]))
}

/// `UnitTestReporter`.
fn report_unit_test(cli: &mut Cli, v: &serde_json::Value) -> Result<()> {
    if !v.is_object() {
        return Err(SnowError::cli("Could not process response."));
    }
    let list = match v.get("scripts") {
        None => vec![],
        Some(serde_json::Value::Array(a)) => a.clone(),
        Some(_) => return Err(SnowError::cli("Could not process response.")),
    };
    let (mut pass, mut fail) = (0, 0);
    for e in &list {
        if !e.is_object() {
            continue;
        }
        let name = snow_api::sanitize::for_terminal(&match e.get("script_name") {
            None => "UNKNOWN".to_string(),
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Null) => "None".to_string(),
            Some(other) => other.to_string(),
        });
        let passed = e.get("passed") == Some(&serde_json::Value::Bool(true));
        if passed {
            pass += 1;
            cli.console
                .styled_message(&format!("{:<11} ", "✓ PASS"), "green");
        } else {
            fail += 1;
            cli.console
                .styled_message(&format!("{:<11} ", "✗ FAIL"), "red");
        }
        cli.console.styled_message(&name, "cyan");
        cli.console.styled_message("\n", "");
        let msg = match e.get("error_message") {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Bool(b)) => {
                if *b {
                    "True".into()
                } else {
                    String::new()
                }
            }
            Some(other) => other.to_string(),
        };
        if !passed && !msg.is_empty() {
            cli.console.styled_message(
                &format!("  └─ {}\n", snow_api::sanitize::for_terminal(&msg)),
                "",
            );
        }
    }
    let total = pass + fail;
    let parts = if total == 0 {
        vec![("No test scripts were run.".to_string(), "")]
    } else {
        vec![
            (format!("{pass} passed"), "green"),
            (", ".to_string(), ""),
            (format!("{fail} failed"), "red"),
            (" out of ".into(), ""),
            (format!("{total}"), "bold"),
            (" total.".into(), ""),
        ]
    };
    finish(cli, fail == 0, parts)
}

pub fn preview(cli: &mut Cli) -> Result<Output> {
    let ctx = context(cli, true)?;
    let env = resolve_env(cli)?.filter(|_| cli.str("deployment").is_none());
    let object = crate::object::fqn_arg(cli, "object_identifier")?;
    let limit = cli.int("limit");
    let mut p = Progress::new(&[(Step::Upload, "UPLOAD"), (Step::Preview, "PREVIEW")]);
    let c = session(cli, &mut p, |cli, p| {
        let stage = p.run(cli, Step::Upload, |cli, _| {
            sync_local_files(cli, &ctx.project, &ctx.from)
        })?;
        p.run(cli, Step::Preview, |cli, _| {
            let mut q = format!(
                "EXECUTE DCM PROJECT {} PREVIEW {}",
                ctx.project.sql_identifier(),
                object.sql_identifier()
            );
            q.push_str(&using_env(cli, ctx.configuration.as_deref(), &env)?);
            q.push_str(&from_stage(&stage));
            if let Some(l) = limit {
                q.push_str(&format!(" LIMIT {l}"));
            }
            exec_env(cli, &q, &env)
        })
    })?;
    Ok(cli.query_result(&c))
}

/// `ServerPoll.run()`: wait for an async DCM operation, reflecting it on the steps.
fn server_poll(
    cli: &mut Cli,
    p: &mut Progress,
    steps: &[Step],
    sfqid: &str,
) -> Result<snow_connector::Cursor> {
    if let Some(first) = steps.first() {
        p.start(cli, *first);
    }
    let running = [
        "RUNNING",
        "RESUMING_WAREHOUSE",
        "QUEUED",
        "QUEUED_REPARING_WAREHOUSE",
        "NO_DATA",
    ];
    let failed = [
        "ABORTING",
        "FAILED_WITH_ERROR",
        "ABORTED",
        "FAILED_WITH_INCIDENT",
        "DISCONNECTED",
        "BLOCKED",
    ];
    let verbose = cli.connector_info;
    let start = Instant::now();
    let mut no_data = 0;
    let mut status = cli
        .connection()?
        .query_status(sfqid)
        .map_err(|e| snow_api::connect::db_error_to_cli(&e, verbose))?;
    while running.contains(&status.as_str()) {
        if status == "NO_DATA" {
            no_data += 1;
            if no_data > 24 {
                return Err(SnowError::cli(format!(
                    "Snowflake reported no status for query {sfqid} in 24 consecutive checks, so its progress cannot be tracked. The operation may still be running - check the query in Snowsight."
                ))
                .named("QueryStatusUnavailableCliError"));
            }
        } else {
            no_data = 0;
        }
        if let Ok(Ok(c)) = cli.try_query(&format!(
            "SELECT SYSTEM$GET_DCM_PROJECT_PROGRESS({})",
            snow_api::identifiers::to_string_literal(sfqid)
        )) && let Some(cell) = Cli::first_cell_opt(&c)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&cell)
        {
            let phase = v["phase"].as_str().unwrap_or("");
            if let Some(at) = steps.iter().position(|s| s.key() == phase) {
                for &s in &steps[..at] {
                    if !p.terminal(s) {
                        p.complete(cli, s);
                    }
                }
                p.start(cli, steps[at]);
            }
        }
        let wait = if start.elapsed().as_secs() < 60 {
            1
        } else {
            10
        };
        std::thread::sleep(std::time::Duration::from_secs(wait));
        status = cli
            .connection()?
            .query_status(sfqid)
            .map_err(|e| snow_api::connect::db_error_to_cli(&e, verbose))?;
    }
    if failed.contains(&status.as_str()) {
        for &s in steps {
            if !p.terminal(s) {
                p.fail(cli, s);
            }
        }
        return Err(SnowError::cli(format!(
            "Query {sfqid} failed, but Snowflake reported no error for it."
        )));
    }
    for &s in steps {
        if !p.terminal(s) {
            p.complete(cli, s);
        }
    }
    cli.connection()?
        .query_result(sfqid)
        .map_err(|e| snow_api::connect::db_error_to_cli(&e, verbose))
}

fn submit_async(cli: &mut Cli, q: &str, env: &Option<String>) -> Result<String> {
    let verbose = cli.connector_info;
    let conn = cli.connection()?;
    let c = match env {
        Some(e) => conn.execute_async_bound(q, std::slice::from_ref(e)),
        None => conn.execute_async(q),
    }
    .map_err(|e| snow_api::connect::db_error_to_cli(&e, verbose))?;
    Ok(c.sfqid.unwrap_or_default())
}

struct ChangeNode {
    label: String,
    sort: (i32, i32),
    expand: bool,
    kids: Vec<ChangeNode>,
}

fn json_py_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(if *b { "true" } else { "false" }.into()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) => {
            let s = snow_api::sanitize::for_terminal(s);
            Some(if s.is_empty() { "''".into() } else { s })
        }
        other => Some(snow_api::sanitize::for_terminal(
            &snow_api::pyfmt::json_dumps(other, None),
        )),
    }
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn window(txt: &str, start: usize) -> String {
    let chars: Vec<char> = txt.chars().collect();
    let end = start + 50;
    let seg: String = chars.iter().skip(start).take(50).collect();
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < chars.len() { "…" } else { "" };
    let mut seg = seg;
    if !prefix.is_empty() {
        seg = seg.trim_start().to_string();
    }
    if !suffix.is_empty() {
        seg = seg.trim_end().to_string();
    }
    format!("{prefix}{seg}{suffix}")
}

fn truncate_inline(s: &str) -> String {
    window(&collapse(s), 0)
}

fn truncate_pair(prev: &str, new: &str) -> (String, String) {
    let (p, n) = (collapse(prev), collapse(new));
    if p.chars().count() <= 50 && n.chars().count() <= 50 {
        return (p, n);
    }
    let diff = p.chars().zip(n.chars()).take_while(|(a, b)| a == b).count();
    if diff <= 42 {
        return (window(&p, 0), window(&n, 0));
    }
    (window(&p, diff - 12), window(&n, diff - 12))
}

fn property_value(value: &serde_json::Value, prev: &serde_json::Value) -> String {
    match (json_py_str(value), json_py_str(prev)) {
        (Some(n), Some(p)) => {
            let (ps, ns) = truncate_pair(&p, &n);
            format!(": {ps} → {ns}")
        }
        (Some(n), None) => format!(": {}", truncate_inline(&n)),
        _ => String::new(),
    }
}

fn item_desc(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::Object(o) => o
            .get("desc")
            .and_then(|d| d.as_str())
            .map(snow_api::sanitize::for_terminal)
            .unwrap_or_default(),
        serde_json::Value::String(s) => snow_api::sanitize::for_terminal(s),
        _ => String::new(),
    }
}

fn build_node(c: &serde_json::Value) -> Option<ChangeNode> {
    let kind = snow_api::sanitize::for_terminal(c["kind"].as_str().unwrap_or(""));
    let kids = build_nodes(c["changes"].as_array().map(Vec::as_slice).unwrap_or(&[]));
    let attr = c["attribute_name"].as_str().map(str::to_string);
    let attr_up = snow_api::sanitize::for_terminal(attr.as_deref().unwrap_or("")).to_uppercase();
    let prop = |kw: &str, sort: (i32, i32), value_part: String| -> ChangeNode {
        let mut l = kw.to_string();
        if !attr_up.is_empty() {
            l.push(' ');
            l.push_str(&attr_up);
            l.push_str(&value_part);
        }
        ChangeNode {
            label: l,
            sort,
            expand: true,
            kids: vec![],
        }
    };
    let mut node = match kind.as_str() {
        "collection" | "nested" => {
            let label = if kind == "collection" {
                c["collection_name"].as_str()
            } else {
                c["attribute_name"].as_str()
            };
            let label = label.filter(|l| !l.is_empty())?;
            ChangeNode {
                label: snow_api::sanitize::for_terminal(label),
                sort: (3, 0),
                expand: true,
                kids: vec![],
            }
        }
        "set" => prop(
            "set",
            (0, 1),
            json_py_str(&c["value"])
                .map(|v| format!(": {}", truncate_inline(&v)))
                .unwrap_or_default(),
        ),
        "changed" => prop(
            "changed",
            (1, 1),
            property_value(&c["value"], &c["prev_value"]),
        ),
        "unset" => prop("unset", (2, 1), String::new()),
        "added" | "modified" | "removed" => {
            let d = item_desc(&c["item_id"]);
            let sort = match kind.as_str() {
                "added" => (0, 0),
                "modified" => (1, 0),
                _ => (2, 0),
            };
            ChangeNode {
                label: if d.is_empty() {
                    kind.clone()
                } else {
                    format!("{kind} {d}")
                },
                sort,
                expand: kind != "removed",
                kids: vec![],
            }
        }
        _ => {
            let d = item_desc(&c["item_id"]);
            if kind.is_empty() && d.is_empty() && attr.as_deref().is_none_or(str::is_empty) {
                return None;
            }
            let mut l = kind.clone();
            if !d.is_empty() {
                l.push(' ');
                l.push_str(&d);
            } else if let Some(a) = attr.filter(|a| !a.is_empty()) {
                l.push(' ');
                l.push_str(&snow_api::sanitize::for_terminal(&a).to_uppercase());
                l.push_str(&property_value(&c["value"], &c["prev_value"]));
            }
            ChangeNode {
                label: l,
                sort: (100, 0),
                expand: true,
                kids: vec![],
            }
        }
    };
    node.kids = kids;
    Some(node)
}

fn build_nodes(changes: &[serde_json::Value]) -> Vec<ChangeNode> {
    changes.iter().filter_map(build_node).collect()
}

/// `_relationship_changes`: what a CREATE shows (grants, tags, DMFs, policies).
fn relationship_changes(changes: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut kept = Vec::new();
    for c in changes {
        let kind = c["kind"].as_str().unwrap_or("");
        let container = kind == "collection" || kind == "nested";
        let policy = !container
            && c["attribute_name"]
                .as_str()
                .unwrap_or("")
                .ends_with("_policy")
            && (c["value"].is_object() || c["prev_value"].is_object());
        if (kind == "collection"
            && ["grants", "tags", "data_metric_functions"]
                .contains(&c["collection_name"].as_str().unwrap_or("")))
            || policy
        {
            kept.push(c.clone());
        } else {
            let kids =
                relationship_changes(c["changes"].as_array().map(Vec::as_slice).unwrap_or(&[]));
            if !kids.is_empty() {
                let mut c2 = c.clone();
                c2["changes"] = serde_json::Value::Array(kids);
                kept.push(c2);
            }
        }
    }
    kept
}

/// Rich wraps each tree label to the width left of its guides; continuation lines
/// carry the guide of that level.
fn wrapped(label: &str, width: usize) -> Vec<String> {
    snow_rich::Text::new(label)
        .wrap(
            width.max(1),
            snow_rich::Justify::Default,
            snow_rich::Overflow::Fold,
            false,
        )
        .into_iter()
        .map(|t| t.plain)
        .collect()
}

fn render_tree(n: &ChangeNode, guides: &str, last: bool, depth: usize, out: &mut Vec<String>) {
    let width = snow_click::term::rich_width();
    if depth == 0 {
        out.extend(wrapped(&n.label, width));
    } else {
        let first = format!("{guides}{}", if last { "└─ " } else { "├─ " });
        let cont = format!("{guides}{}", if last { "   " } else { "│  " });
        let avail = width.saturating_sub(first.chars().count());
        for (i, l) in wrapped(&n.label, avail).into_iter().enumerate() {
            out.push(format!("{}{l}", if i == 0 { &first } else { &cont }));
        }
    }
    if !n.expand {
        return;
    }
    let child_guides = if depth == 0 {
        String::new()
    } else {
        format!("{guides}{}", if last { "   " } else { "│  " })
    };
    let mut kids: Vec<&ChangeNode> = n.kids.iter().collect();
    kids.sort_by_key(|k| k.sort);
    let count = kids.len();
    for (i, k) in kids.into_iter().enumerate() {
        render_tree(k, &child_guides, i + 1 == count, depth + 1, out);
    }
}

/// `PlanReporter` for plan/deploy/purge.
fn report_plan(cli: &mut Cli, v: &serde_json::Value, command: &str) -> Result<()> {
    if !v.is_object() || !v["version"].is_i64() {
        return Err(SnowError::cli("Could not process response."));
    }
    let changeset = v["changeset"].as_array().cloned().unwrap_or_default();
    let (mut created, mut altered, mut dropped) = (0, 0, 0);
    let mut rows: Vec<(usize, String, String, String, Vec<ChangeNode>)> = Vec::new();
    for e in &changeset {
        let op = snow_api::sanitize::for_terminal(
            &e["type"].as_str().unwrap_or("UNKNOWN").to_uppercase(),
        );
        let domain = snow_api::sanitize::for_terminal(
            &e["object_id"]["domain"]
                .as_str()
                .unwrap_or("")
                .to_uppercase(),
        );
        let fqn_s = snow_api::sanitize::for_terminal(e["object_id"]["fqn"].as_str().unwrap_or(""));
        let display = match Fqn::from_string(&fqn_s) {
            Ok(f) => {
                let mut parts = Vec::new();
                if let Some(d) = &f.database {
                    parts.push(snow_api::identifiers::unquote_identifier(d));
                }
                if let Some(s) = &f.schema {
                    parts.push(snow_api::identifiers::unquote_identifier(s));
                }
                parts.push(snow_api::identifiers::unquote_identifier(&f.name));
                let mut r = parts.join(".");
                if let Some(sig) = &f.signature {
                    r.push_str(sig);
                }
                r
            }
            Err(_) => "UNKNOWN".to_string(),
        };
        let changes = e["changes"].as_array().cloned().unwrap_or_default();
        let details = match op.as_str() {
            "ALTER" => build_nodes(&changes),
            "CREATE" => build_nodes(&relationship_changes(&changes)),
            _ => vec![],
        };
        match op.as_str() {
            "CREATE" => created += 1,
            "ALTER" => altered += 1,
            "DROP" => dropped += 1,
            _ => {}
        }
        let order = match op.as_str() {
            "CREATE" => 0,
            "ALTER" => 1,
            "DROP" => 2,
            _ => 3,
        };
        rows.push((order, domain, op, display, details));
    }
    rows.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let last = rows.len().saturating_sub(1);
    for (i, (_, domain, op, display, details)) in rows.into_iter().enumerate() {
        let root = ChangeNode {
            label: format!("{op:<8} {domain:<20} {display}"),
            sort: (0, 0),
            expand: true,
            kids: details,
        };
        let mut lines = Vec::new();
        render_tree(&root, "", true, 0, &mut lines);
        for l in lines {
            cli.console.plain_message(&l);
        }
        if !root.kids.is_empty() && i != last {
            cli.console.styled_message("\n", "");
        }
    }
    let total = created + altered + dropped;
    let parts: Vec<(String, &str)> = if total == 0 {
        vec![("No changes detected.".into(), "")]
    } else {
        let (c, a, d, h) = match command {
            "plan" => ("to create", "to alter", "to drop", "Planned"),
            "deploy" => ("created", "altered", "dropped", "Deployed"),
            _ => ("created", "altered", "dropped", "Purged"),
        };
        vec![
            (
                format!(
                    "{h} {total} {} (",
                    if total == 1 { "entity" } else { "entities" }
                ),
                "",
            ),
            (format!("{created} {c}"), "green"),
            (", ".into(), ""),
            (format!("{altered} {a}"), "yellow"),
            (", ".into(), ""),
            (format!("{dropped} {d}"), "red"),
            (").".into(), ""),
        ]
    };
    finish(cli, true, parts)
}

/// `command_artifacts(save_output)`: recreate out/, run, then announce artifacts.
fn with_artifacts(cli: &mut Cli, f: impl FnOnce(&mut Cli) -> Result<Output>) -> Result<Output> {
    let save = cli.flag("save_output");
    let out = Path::new("out");
    if save {
        if out.exists() {
            std::fs::remove_dir_all(out)?;
        }
        std::fs::create_dir_all(out)?;
    }
    let r = f(cli);
    if save && crate::artifacts::walk(out).iter().any(|p| p.is_file()) {
        let abs = std::env::current_dir().unwrap_or_default().join(out);
        cli.console
            .step(&format!("Artifacts saved to: {}", abs.display()));
    }
    r
}

/// `save_command_response`: out/<command>_result.json, written once.
fn save_response(cli: &Cli, command: &str, v: &serde_json::Value) -> Result<()> {
    if !cli.flag("save_output") {
        return Ok(());
    }
    let f = Path::new("out").join(format!("{command}_result.json"));
    if f.exists() {
        return Ok(());
    }
    std::fs::write(&f, snow_api::pyfmt::json_dumps(v, None))?;
    Ok(())
}

/// `_collect_output`: a temporary output stage whose `/outputs` is the OUTPUT_PATH.
fn output_stage(cli: &mut Cli, project: &Fqn) -> Result<Option<String>> {
    if !cli.flag("save_output") {
        return Ok(None);
    }
    let stage = temp_stage(cli, project, "OUTPUT_TMP_STAGE")?;
    cli.execute_query(&crate::stage::create_query(&stage, true, None, false, None))?;
    Ok(Some(
        snow_api::stage_path::StagePath::from_stage_str(&stage.identifier())
            .join("/outputs")
            .absolute_path(),
    ))
}

fn download_outputs(cli: &mut Cli, path: &Option<String>) {
    if let Some(p) = path {
        let _ = crate::stage::get_recursive(cli, p, Path::new("out"), 1, false);
    }
}

fn upload_and(cli: &mut Cli, p: &mut Progress, ctx: &Context) -> Result<Option<String>> {
    if cli.str("deployment").is_some() {
        return Ok(None);
    }
    Ok(Some(p.run(cli, Step::Upload, |cli, _| {
        sync_local_files(cli, &ctx.project, &ctx.from)
    })?))
}

fn from_clause(stage: Option<&str>, deployment: Option<&str>) -> String {
    match deployment {
        Some(d) => format!(
            " FROM DEPLOYMENT {}",
            snow_api::identifiers::to_quoted_identifier(d)
        ),
        None => from_stage(stage.unwrap_or_default()),
    }
}

pub fn plan(cli: &mut Cli) -> Result<Output> {
    with_artifacts(cli, plan_inner)
}

fn plan_inner(cli: &mut Cli) -> Result<Output> {
    let ctx = context(cli, true)?;
    let env = resolve_env(cli)?.filter(|_| cli.str("deployment").is_none());
    let deployment = cli.str("deployment");
    let delta = cli.flag("delta");
    let mut steps: Vec<(Step, &str)> = vec![];
    if deployment.is_none() {
        steps.push((Step::Upload, "UPLOAD"));
    }
    steps.extend([
        (Step::Render, "RENDER"),
        (Step::Compile, "COMPILE"),
        (Step::Plan, "PLAN"),
    ]);
    let mut p = Progress::new(&steps);
    let c = session(cli, &mut p, |cli, p| {
        let stage = upload_and(cli, p, &ctx)?;
        let mut q = format!("EXECUTE DCM PROJECT {} PLAN", ctx.project.sql_identifier());
        if delta {
            q.push_str(" DELTA");
        }
        if deployment.is_none() {
            q.push_str(&using_env(cli, ctx.configuration.as_deref(), &env)?);
        }
        q.push_str(&from_clause(stage.as_deref(), deployment.as_deref()));
        let out = output_stage(cli, &ctx.project)?;
        if let Some(o) = &out {
            q.push_str(&format!(" OUTPUT_PATH {o}"));
        }
        let r = submit_async(cli, &q, &env)
            .and_then(|qid| server_poll(cli, p, &[Step::Render, Step::Compile, Step::Plan], &qid));
        download_outputs(cli, &out);
        r
    })?;
    finish_json(cli, &c, "plan", |cli, v| report_plan(cli, v, "plan"))
}

pub fn deploy(cli: &mut Cli) -> Result<Output> {
    with_artifacts(cli, deploy_inner)
}

fn deploy_inner(cli: &mut Cli) -> Result<Output> {
    let ctx = context(cli, true)?;
    let env = resolve_env(cli)?.filter(|_| cli.str("deployment").is_none());
    let deployment = cli.str("deployment");
    let alias = cli.str("alias");
    let skip_plan = cli.flag("skip_plan");
    let mut server = vec![Step::Render, Step::Compile, Step::Plan, Step::Deploy];
    if crate::dcm::run_post_hooks(cli) {
        server.push(Step::PostHook);
    }
    if skip_plan {
        server.retain(|s| *s != Step::Plan);
    }
    let mut steps: Vec<(Step, &str)> = vec![];
    if deployment.is_none() {
        steps.push((Step::Upload, "UPLOAD"));
    }
    steps.extend(server.iter().map(|&s| {
        (
            s,
            if s == Step::PostHook {
                "POST HOOK"
            } else {
                s.key()
            },
        )
    }));
    let mut p = Progress::new(&steps);
    let c = session(cli, &mut p, |cli, p| {
        if skip_plan {
            cli.console.warning("Skipping planning step");
        }
        let stage = upload_and(cli, p, &ctx)?;
        let mut q = format!(
            "EXECUTE DCM PROJECT {} DEPLOY",
            ctx.project.sql_identifier()
        );
        if let Some(a) = &alias {
            q.push_str(&format!(" AS \"{a}\""));
        }
        if deployment.is_none() {
            q.push_str(&using_env(cli, ctx.configuration.as_deref(), &env)?);
        }
        q.push_str(&from_clause(stage.as_deref(), deployment.as_deref()));
        if skip_plan {
            q.push_str(" SKIP PLAN");
        }
        let qid = submit_async(cli, &q, &env)?;
        server_poll(cli, p, &server, &qid)
    })?;
    finish_json(cli, &c, "deploy", |cli, v| report_plan(cli, v, "deploy"))
}

pub fn purge(cli: &mut Cli) -> Result<Output> {
    with_artifacts(cli, purge_inner)
}

fn purge_inner(cli: &mut Cli) -> Result<Output> {
    let ctx = context(cli, false)?;
    let (force, interactive) = (cli.flag("force"), cli.flag("interactive"));
    if !force && !interactive {
        return Err(SnowError::cli(
            "Cannot purge the DCM project non-interactively without --force.",
        ));
    }
    if !force {
        cli.console.warning(&format!(
            "⚠️  DANGER: This operation will DROP ALL objects managed by DCM Project {}  ⚠️",
            ctx.project.identifier()
        ));
        let expected = ctx.project.identifier();
        loop {
            let input = crate::common::prompt(
                cli,
                &format!("Type 'purge {expected}' to confirm or 'cancel' to abort"),
                false,
            )?
            .unwrap_or_default();
            let parts: Vec<&str> = input.trim().splitn(2, char::is_whitespace).collect();
            if parts.is_empty() || parts[0].is_empty() {
                continue;
            }
            let cmd = parts[0].to_uppercase();
            if cmd == "CANCEL" {
                return Err(SnowError::Click(snow_click::ClickError::Abort));
            }
            if cmd == "PURGE" && parts.len() == 2 {
                if parts[1].trim().to_uppercase() == expected.to_uppercase() {
                    break;
                }
                cli.console.message(&format!(
                    "  Project identifier mismatch. Expected: {expected}, provided: {}",
                    parts[1].trim()
                ));
            }
        }
    }
    let alias = cli.str("alias");
    let skip_plan = cli.flag("skip_plan");
    let server = if skip_plan {
        vec![Step::Deploy]
    } else {
        vec![Step::Plan, Step::Deploy]
    };
    let steps: Vec<(Step, &str)> = server
        .iter()
        .map(|&s| (s, if s == Step::Deploy { "PURGE" } else { "PLAN" }))
        .collect();
    let mut p = Progress::new(&steps);
    let c = session(cli, &mut p, |cli, p| {
        if skip_plan {
            cli.console.warning("Skipping planning step");
        }
        let mut q = format!("EXECUTE DCM PROJECT {} PURGE", ctx.project.sql_identifier());
        if let Some(a) = &alias {
            q.push_str(&format!(" AS \"{a}\""));
        }
        if skip_plan {
            q.push_str(" SKIP PLAN");
        }
        let qid = submit_async(cli, &q, &None)?;
        server_poll(cli, p, &server, &qid)
    })?;
    finish_json(cli, &c, "purge", |cli, v| report_plan(cli, v, "purge"))
}

pub fn raw_analyze(cli: &mut Cli) -> Result<Output> {
    with_artifacts(cli, raw_analyze_inner)
}

fn raw_analyze_inner(cli: &mut Cli) -> Result<Output> {
    let ctx = context(cli, true)?;
    let env = resolve_env(cli)?.filter(|_| cli.str("deployment").is_none());
    let mut p = Progress::new(&[(Step::Upload, "UPLOAD"), (Step::Analyze, "ANALYZE")]);
    let c = session(cli, &mut p, |cli, p| {
        let stage = p.run(cli, Step::Upload, |cli, _| {
            sync_local_files(cli, &ctx.project, &ctx.from)
        })?;
        p.run(cli, Step::Analyze, |cli, _| {
            let mut q = format!(
                "EXECUTE DCM PROJECT {} ANALYZE",
                ctx.project.sql_identifier()
            );
            q.push_str(&using_env(cli, ctx.configuration.as_deref(), &env)?);
            q.push_str(&from_stage(&stage));
            let out = output_stage(cli, &ctx.project)?;
            if let Some(o) = &out {
                q.push_str(&format!(" OUTPUT_PATH {o}"));
            }
            let r = exec_env(cli, &q, &env);
            download_outputs(cli, &out);
            r
        })
    })?;
    let Some((v, raw)) = first_json(cli, &c)? else {
        return Ok(Output::Empty);
    };
    let files = match v.get("files") {
        None => vec![],
        Some(serde_json::Value::Array(a)) => a.clone(),
        Some(_) => return Err(SnowError::cli("Could not process response.")),
    };
    let mut count = v
        .get("issues")
        .and_then(|i| i.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    for f in &files {
        count += f
            .get("issues")
            .and_then(|i| i.as_array())
            .map(Vec::len)
            .unwrap_or(0);
        for d in f
            .get("definitions")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default()
        {
            count += d
                .get("issues")
                .and_then(|i| i.as_array())
                .map(Vec::len)
                .unwrap_or(0);
        }
    }
    save_response(cli, "compile", &v)?;
    cli.console.styled_message(&raw, "");
    cli.console.styled_message("\n", "");
    let msg = if count == 0 {
        "Analysis completed successfully.".to_string()
    } else {
        format!("Analysis found {count} error(s).")
    };
    finish(cli, count == 0, vec![(msg, "")])?;
    Ok(format_aware(cli, &c, raw))
}

/// `_get_configuration_and_variables_query` plus ` ENVIRONMENT (?)` when env vars resolved.
fn using_env(cli: &Cli, configuration: Option<&str>, env: &Option<String>) -> Result<String> {
    let mut q = using_clause(cli, configuration)?;
    if env.is_some() {
        q.push_str(" ENVIRONMENT (?)");
    }
    Ok(q)
}

fn exec_env(cli: &mut Cli, q: &str, env: &Option<String>) -> Result<snow_connector::Cursor> {
    match env {
        None => cli.execute_query(q),
        Some(e) => {
            let verbose = cli.connector_info;
            cli.connection()?
                .execute_bound(q, std::slice::from_ref(e))
                .map_err(|err| snow_api::connect::db_error_to_cli(&err, verbose))
        }
    }
}

/// A python-dotenv style `KEY=VALUE` file (`dotenv_values(interpolate=False)`).
fn parse_env_file(text: &str) -> indexmap::IndexMap<String, String> {
    let mut out = indexmap::IndexMap::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("export ").unwrap_or(l);
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let quoted = v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')));
        let v = if quoted {
            v[1..v.len() - 1].to_string()
        } else {
            v.split(" #").next().unwrap_or(v).trim().to_string()
        };
        out.insert(k.trim().to_string(), v);
    }
    out
}

/// `resolve_declared_env_vars(declared, env_file)` as the JSON bound to ENVIRONMENT (?).
fn resolve_env(cli: &mut Cli) -> Result<Option<String>> {
    let names = crate::dcm::declared_env_names(cli);
    let env_file = cli.str("env_file");
    let file_values = match &env_file {
        None => indexmap::IndexMap::new(),
        Some(f) => {
            let p = Path::new(f);
            if !p.exists() {
                return Err(SnowError::cli(format!("Env file {f} was not found.")));
            }
            if !p.is_file() {
                return Err(SnowError::cli(format!("Env file {f} is not a file.")));
            }
            parse_env_file(&std::fs::read_to_string(p)?)
        }
    };
    let mut result = serde_json::Map::new();
    let mut missing = Vec::new();
    for n in &names {
        match std::env::var(n)
            .ok()
            .or_else(|| file_values.get(n).cloned())
        {
            Some(v) => {
                result.insert(n.clone(), serde_json::Value::String(v));
            }
            None => missing.push(snow_api::sanitize::for_terminal(n)),
        }
    }
    if !missing.is_empty() {
        missing.sort();
        let src = match &env_file {
            None => "the shell environment".to_string(),
            Some(f) => format!("the shell environment or env file '{f}'"),
        };
        cli.console.warning(&format!(
            "Declared in manifest.yml but could not be resolved from {src}: {}.",
            missing.join(", ")
        ));
    }
    if result.is_empty() {
        return Ok(None);
    }
    Ok(Some(snow_api::pyfmt::json_dumps(
        &serde_json::Value::Object(result),
        None,
    )))
}

fn sorted_children(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// `Path.glob(pattern)` for `*` / `**` segments.
fn glob_segments(dir: &Path, segs: &[&str], out: &mut Vec<PathBuf>) {
    let Some((first, rest)) = segs.split_first() else {
        out.push(dir.to_path_buf());
        return;
    };
    if *first == "**" {
        glob_segments(dir, rest, out);
        for c in sorted_children(dir) {
            if c.is_dir() {
                glob_segments(&c, segs, out);
            }
        }
    } else if first.contains('*') {
        for c in sorted_children(dir) {
            let name = c
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if crate::stage::fnmatchcase(&name, first) && (rest.is_empty() || c.is_dir()) {
                glob_segments(&c, rest, out);
            }
        }
    } else {
        let c = dir.join(first);
        if c.exists() {
            glob_segments(&c, rest, out);
        }
    }
}

/// `resolve_asset_paths(project_root, assets)`.
fn resolve_asset_paths(
    cli: &mut Cli,
    root: &Path,
    assets: &[(String, Vec<String>)],
) -> Vec<String> {
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let inside = |p: &Path| p.canonicalize().is_ok_and(|r| r.starts_with(&real_root));
    let rel_of = |p: &Path| {
        p.strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let mut resolved = std::collections::BTreeSet::new();
    for (name, patterns) in assets {
        for pattern in patterns {
            let mut matches = Vec::new();
            let segs: Vec<&str> = pattern.split('/').collect();
            glob_segments(root, &segs, &mut matches);
            matches.sort();
            matches.dedup();
            let is_glob = pattern.contains('*');
            let mut files = Vec::new();
            for m in matches {
                let r = rel_of(&m);
                if is_hidden(&r) || !inside(&m) {
                    continue;
                }
                if m.is_file() {
                    files.push(r);
                } else if m.is_dir() && !is_glob {
                    for f in crate::artifacts::walk(&m) {
                        let fr = rel_of(&f);
                        if f.is_file() && !is_hidden(&fr) && inside(&f) {
                            files.push(fr);
                        }
                    }
                }
            }
            if files.is_empty() {
                cli.console.warning(&format!(
                    "Asset '{name}' path '{pattern}' matched no files; skipping."
                ));
                continue;
            }
            resolved.extend(files);
        }
    }
    resolved.into_iter().collect()
}
