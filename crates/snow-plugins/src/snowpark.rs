//! `snow snowpark build` and `snow snowpark deploy` (functions and procedures from
//! snowflake.yml; `_plugins/snowpark/commands.py`, `common.py`, `snowpark_project_paths.py`).

use crate::snowpark_package::{Anaconda, Requirement};
use snow_api::identifiers::{Fqn, to_string_literal};
use snow_api::output::{Output, Pv, Row};
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};
use std::path::{Path, PathBuf};

fn has_magic(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// A destination that names a file (`dest_is_file`): a 2-4 character extension.
static FILE_DEST: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\.[a-zA-Z0-9]{2,4}$").expect("re"));

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `ArtifactOldBuild` / `Artifact` (glob support) for one artifact mapping.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Artifact {
    /// Project-relative (glob build) or absolute (old build) source.
    path: PathBuf,
    dest: Option<String>,
    glob_build: bool,
    project_root: PathBuf,
    bundle_root: PathBuf,
}

impl Artifact {
    fn dest_is_file(&self) -> bool {
        self.dest.as_deref().is_some_and(|d| FILE_DEST.is_match(d))
    }

    fn name(&self) -> String {
        let stem = |p: &Path| {
            p.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        if !self.glob_build {
            return if self.path.is_dir() {
                format!("{}.zip", stem(&self.path))
            } else {
                file_name(&self.path)
            };
        }
        let s = self.path.to_string_lossy();
        if has_magic(&s) {
            let mut last = None;
            for part in self.path.components() {
                let p = part.as_os_str().to_string_lossy().into_owned();
                if has_magic(&p) {
                    break;
                }
                last = Some(p);
            }
            return format!("{}.zip", last.unwrap_or_else(|| s.into_owned()));
        }
        let full = self.project_root.join(&self.path);
        if full.is_dir() {
            return format!("{}.zip", stem(&self.path));
        }
        if full.is_file() && self.dest_is_file() {
            return file_name(Path::new(self.dest.as_deref().unwrap_or_default()));
        }
        file_name(&self.path)
    }

    fn until_asterisk(&self) -> PathBuf {
        let mut parts: Vec<String> = Vec::new();
        for c in self.path.components() {
            let p = c.as_os_str().to_string_lossy().into_owned();
            if has_magic(&p) {
                break;
            }
            parts.push(p);
        }
        parts.pop();
        parts.iter().collect()
    }

    /// The source directory a glob build keeps: up to the first wildcard, else the parent.
    fn base(&self) -> PathBuf {
        if has_magic(&self.path.to_string_lossy()) {
            self.until_asterisk()
        } else {
            self.path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
        }
    }

    fn post_build_path(&self) -> PathBuf {
        if !self.glob_build {
            return self
                .path
                .parent()
                .unwrap_or(Path::new(""))
                .join(self.name());
        }
        if self.dest_is_file() {
            return self
                .bundle_root
                .join(self.dest.as_deref().unwrap_or_default());
        }
        match &self.dest {
            Some(d) => self.bundle_root.join(d).join(self.name()),
            None => self.bundle_root.join(self.base()).join(self.name()),
        }
    }

    fn upload_path(&self, stage: &str) -> String {
        let mut p = format!("@{stage}");
        let join = |p: &mut String, x: &str| {
            let x = x.trim_matches('/');
            if !x.is_empty() && x != "." {
                p.push('/');
                p.push_str(x);
            }
        };
        if !self.glob_build {
            if let Some(d) = &self.dest {
                join(&mut p, d);
            }
            return format!("{p}/");
        }
        match &self.dest {
            Some(d) if self.dest_is_file() => join(
                &mut p,
                &Path::new(d)
                    .parent()
                    .map(|x| x.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            Some(d) => join(&mut p, d),
            None => join(&mut p, &self.base().to_string_lossy()),
        }
        format!("{p}/")
    }

    fn import_path(&self, stage: &str) -> String {
        format!("{}{}", self.upload_path(stage), self.name())
    }
}

struct Paths {
    root: PathBuf,
    glob: bool,
}

impl Paths {
    fn bundle_root(&self) -> PathBuf {
        self.root.join("output").join("bundle").join("snowpark")
    }
    fn rel(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() {
            return p.to_path_buf();
        }
        let j = self.root.join(p);
        j.canonicalize()
            .unwrap_or_else(|_| crate::artifacts::resolve_without_follow(&j))
    }
    fn artifact(&self, src: &str, dest: Option<String>) -> Artifact {
        let mut dest = dest.filter(|d| !d.is_empty());
        let path = if self.glob {
            PathBuf::from(src)
        } else {
            self.rel(src)
        };
        let a = Artifact {
            path,
            dest: None,
            glob_build: self.glob,
            project_root: self.root.clone(),
            bundle_root: self.bundle_root(),
        };
        if self.glob
            && let Some(d) = &mut dest
            && !FILE_DEST.is_match(d)
            && !d.ends_with('/')
        {
            d.push('/');
        }
        Artifact { dest, ..a }
    }
    fn dependencies(&self) -> Artifact {
        self.artifact("dependencies.zip", None)
    }
}

fn entities(cli: &mut Cli) -> Result<Vec<(String, PyV)>> {
    let p = cli.project_v2_in_memory()?;
    let mut procs: Vec<(String, PyV)> = Vec::new();
    let mut funcs: Vec<(String, PyV)> = Vec::new();
    for (k, e) in p.entities() {
        match e.str_of("type").as_deref() {
            Some("procedure") => procs.push((k, e)),
            Some("function") => funcs.push((k, e)),
            _ => {}
        }
    }
    let mut all = procs;
    for (k, e) in funcs {
        if let Some(i) = all.iter().position(|(x, _)| *x == k) {
            all[i] = (k, e);
        } else {
            all.push((k, e));
        }
    }
    Ok(all)
}

fn artifact_specs(e: &PyV) -> Vec<(String, Option<String>)> {
    e.get("artifacts")
        .and_then(PyV::as_list)
        .map(|l| {
            l.iter()
                .map(|a| match a {
                    PyV::Str(s) => (s.clone(), None),
                    other => (
                        other.str_of("src").unwrap_or_default(),
                        other.str_of("dest"),
                    ),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `resolve_requirements_source(requirements.txt, pyproject.toml)`: (file name, lines).
fn requirements_source(cli: &mut Cli, root: &Path) -> Result<Option<(String, Vec<String>)>> {
    let req = root.join("requirements.txt");
    let pyproject = root.join("pyproject.toml");
    let py_deps = || -> Result<(Vec<String>, bool)> {
        let Ok(text) = std::fs::read_to_string(&pyproject) else {
            return Ok((vec![], false));
        };
        let doc: toml_edit::DocumentMut = text.parse().map_err(|e| {
            SnowError::cli(format!("Cannot parse {}: {e}", pyproject.display()))
                .named("CliArgumentError")
        })?;
        let Some(project) = doc.get("project") else {
            return Ok((vec![], false));
        };
        let deps = project
            .get("dependencies")
            .and_then(|d| d.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let dynamic = project
            .get("dynamic")
            .and_then(|d| d.as_array())
            .is_some_and(|a| a.iter().any(|x| x.as_str() == Some("dependencies")));
        Ok((deps, dynamic))
    };
    if req.exists() {
        if !py_deps()?.0.is_empty() {
            cli.console.warning(
                "requirements.txt exists, so dependencies are taken from requirements.txt and [project] dependencies in pyproject.toml are ignored. pyproject.toml is only used when requirements.txt is absent.",
            );
        }
        let text = std::fs::read_to_string(&req)?;
        let text = regex::Regex::new(r"\\\r?\n")
            .expect("re")
            .replace_all(&text, " ")
            .into_owned();
        let mut lines = Vec::new();
        let comment = regex::Regex::new(r"\s*#.*").expect("re");
        let hash = regex::Regex::new(r"(?:^|\s)--hash=\S+").expect("re");
        for l in text.lines() {
            let l = comment.replace(l, "").into_owned();
            let l = hash.replace_all(&l, "").trim().to_string();
            if !l.is_empty() {
                lines.push(l);
            }
        }
        return Ok(Some(("requirements.txt".into(), lines)));
    }
    let (deps, dynamic) = py_deps()?;
    if !deps.is_empty() {
        return Ok(Some(("pyproject.toml".into(), deps)));
    }
    if dynamic {
        cli.console.warning(
            "pyproject.toml declares dependencies as dynamic metadata, which cannot be resolved by the CLI. List them under [project] dependencies, or in a requirements.txt file.",
        );
    }
    Ok(None)
}

fn build_artifact(a: &Artifact, cli: &mut Cli) -> Result<()> {
    if a.glob_build {
        let mut map = crate::artifacts::BundleMap::new(&a.project_root, &a.bundle_root);
        map.add(&a.path.to_string_lossy(), a.dest.as_deref(), &[])?;
        if a.project_root.join(&a.path).is_file() {
            for (s, d) in map.all_mappings_absolute() {
                crate::artifacts::symlink_or_copy_tree(&s, &d, &a.bundle_root, &a.project_root)?;
            }
        } else {
            let dest = a.post_build_path();
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            cli.console.step(&format!("Creating: {}", dest.display()));
            let mut z = crate::zipper::ZipWriter::default();
            for (s, _) in map.all_mappings_expanded() {
                if s.is_file() {
                    let rel = s
                        .strip_prefix(&map.project_root)
                        .unwrap_or(&s)
                        .to_path_buf();
                    let comps: Vec<_> = rel.components().collect();
                    let arc: PathBuf = if comps.len() > 1 {
                        comps[1..].iter().collect()
                    } else {
                        rel.clone()
                    };
                    z.add_path(&s, &arc.to_string_lossy())?;
                }
            }
            std::fs::write(&dest, z.finish())?;
        }
        return Ok(());
    }
    if !a.path.is_dir() {
        return Ok(());
    }
    let dest = a.post_build_path();
    cli.console.step(&format!("Creating: {}", file_name(&dest)));
    crate::zipper::zip_dir(&a.path, &dest)?;
    Ok(())
}

pub fn build(cli: &mut Cli) -> Result<Output> {
    let skip = cli.flag("skip_dependencies");
    let (ignore_anaconda, allow_shared, index_url, skip_version) = (
        cli.flag("ignore_anaconda"),
        cli.flag("allow_shared_libraries"),
        cli.str("index_url"),
        cli.flag("skip_version_check"),
    );
    if skip {
        let mut used = vec!["--skip-dependencies"];
        for (n, u) in [
            ("--ignore-anaconda", ignore_anaconda),
            ("--allow-shared-libraries", allow_shared),
            ("--index-url", index_url.is_some()),
            ("--skip-version-check", skip_version),
        ] {
            if u {
                used.push(n);
            }
        }
        if used.len() > 1 {
            return Err(SnowError::incompatible(&used));
        }
    }
    let ents = entities(cli)?;
    let root = cli.project_root();
    let glob = cli
        .config
        .feature(snow_api::config::FeatureFlag::EnableSnowparkGlobSupport);
    let paths = Paths {
        root: root.clone(),
        glob,
    };
    let bundle = paths.bundle_root();
    if bundle.exists() {
        std::fs::remove_dir_all(&bundle)?;
    }
    if skip {
        let no_pkgs: Vec<String> = ents
            .iter()
            .filter(|(_, e)| {
                !(e.str_of("artifact_repository")
                    .is_some_and(|r| !r.is_empty())
                    && (e
                        .get("artifact_repository_packages")
                        .and_then(PyV::as_list)
                        .is_some_and(|l| !l.is_empty())
                        || e.get("packages")
                            .and_then(PyV::as_list)
                            .is_some_and(|l| !l.is_empty())))
            })
            .map(|(k, _)| k.clone())
            .collect();
        if !no_pkgs.is_empty() {
            cli.console.warning(&format!(
                "No packages are declared for: {}. --skip-dependencies reads neither requirements.txt nor pyproject.toml, so these are deployed with no dependencies and imports of them fail at runtime. Declare artifact_repository and packages in the project definition to install them from an artifact repository.",
                no_pkgs.join(", ")
            ));
        }
        for p in [
            paths.rel("requirements.snowflake.txt"),
            paths.dependencies().post_build_path(),
        ] {
            if p.exists() {
                cli.console.step(&format!(
                    "Removing {} left by a previous build",
                    file_name(&p)
                ));
                std::fs::remove_file(&p)?;
            }
        }
    } else if let Some((file, lines)) = requirements_source(cli, &root)? {
        cli.console
            .phase_start(&format!("Resolving dependencies from {file}"));
        let r = (|| -> Result<()> {
            let anaconda = if ignore_anaconda {
                Anaconda::default()
            } else {
                Anaconda::load(cli)?
            };
            let reqs = lines
                .iter()
                .map(|l| Requirement::parse(l))
                .collect::<Result<Vec<_>>>()?;
            let deps_dir = tempfile::tempdir()?;
            let d = crate::snowpark_pip::download_unavailable(
                cli,
                reqs,
                deps_dir.path(),
                &anaconda,
                skip_version,
                index_url.as_deref(),
            )?;
            crate::snowpark_pip::check_shared_libraries(
                &d,
                allow_shared,
                "snowflake.cli._plugins.snowpark.commands",
            )?;
            if !d.anaconda.is_empty() {
                let mut seen = Vec::new();
                let mut out = Vec::new();
                for r in &d.anaconda {
                    if !r.name.is_empty() && anaconda.packages.contains_key(&r.name) {
                        if seen.contains(&r.name) {
                            continue;
                        }
                        seen.push(r.name.clone());
                        let sf = &anaconda.packages[&r.name].0;
                        let mut specs: Vec<String> =
                            r.specs.iter().map(|(o, v)| format!("{o}{v}")).collect();
                        specs.sort();
                        out.push(format!("{sf}{}", specs.join(",")));
                    }
                }
                if !out.is_empty() {
                    std::fs::write(paths.rel("requirements.snowflake.txt"), out.join("\n"))?;
                }
            }
            if std::fs::read_dir(deps_dir.path())?.next().is_some() {
                let dep = paths.dependencies();
                let dest = dep.post_build_path();
                cli.console
                    .step(&format!("Creating {}", file_name(&dep.path)));
                crate::zipper::zip_dir(deps_dir.path(), &dest)?;
                return Ok(());
            }
            cli.console.step("No external dependencies.");
            Ok(())
        })();
        cli.console.phase_end(None);
        r?;
    }
    cli.console
        .phase_start("Preparing artifacts for source code");
    let mut arts: Vec<Artifact> = Vec::new();
    for (_, e) in &ents {
        for (src, dest) in artifact_specs(e) {
            let a = paths.artifact(&src, dest);
            if !arts.contains(&a) {
                arts.push(a);
            }
        }
    }
    let r = (|| -> Result<()> {
        for a in &arts {
            build_artifact(a, cli)?;
        }
        Ok(())
    })();
    cli.console.phase_end(None);
    r?;
    Ok(Output::message("Build done."))
}

/// `UdfSprocIdentifier` pieces: (names, types, defaults).
fn signature(e: &PyV) -> (Vec<String>, Vec<String>, Vec<Option<String>>) {
    let mut n = Vec::new();
    let mut t = Vec::new();
    let mut d = Vec::new();
    if let Some(PyV::List(args)) = e.get("signature") {
        for a in args {
            n.push(a.str_of("name").unwrap_or_default());
            t.push(
                a.str_of("arg_type")
                    .or_else(|| a.str_of("type"))
                    .unwrap_or_default(),
            );
            d.push(a.get("default").filter(|v| !v.is_none()).map(PyV::py_str));
        }
    }
    (n, t, d)
}

fn full_signature(e: &PyV) -> Vec<String> {
    let (n, t, d) = signature(e);
    n.iter()
        .zip(t.iter())
        .zip(d.iter())
        .map(|((n, t), d)| {
            let mut s = format!("{n} {t}");
            if let Some(d) = d {
                let d = if ["string", "varchar"].contains(&t.to_lowercase().as_str())
                    && d.to_lowercase() != "null"
                {
                    format!("'{d}'")
                } else {
                    d.clone()
                };
                s.push_str(&format!(" default {d}"));
            }
            s
        })
        .collect()
}

fn fqn_in_context(cli: &mut Cli, e: &PyV, id: &str) -> Result<Fqn> {
    let f = crate::entities::fqn(e, id)?;
    cli.using_connection(f)
}

pub fn deploy(cli: &mut Cli) -> Result<Output> {
    // `require_warehouse=True`.
    if !cli.session_has_warehouse()? {
        return Err(SnowError::click(
            "The command requires warehouse. No warehouse found in current connection.",
        ));
    }
    let (replace, force_replace, prune) = (
        cli.flag("replace"),
        cli.flag("force_replace"),
        cli.flag("prune"),
    );
    let ents = entities(cli)?;
    let root = cli.project_root();
    let glob = cli
        .config
        .feature(snow_api::config::FeatureFlag::EnableSnowparkGlobSupport);
    let paths = Paths {
        root: root.clone(),
        glob,
    };
    cli.console.phase_start("Performing initial validation");
    let r = (|| -> Result<()> {
        if ents.is_empty() {
            return Err(SnowError::click(
                "No procedures or functions were specified in the project definition.",
            ));
        }
        for (k, e) in &ents {
            for (src, dest) in artifact_specs(e) {
                let p = paths.artifact(&src, dest).post_build_path();
                if !p.exists() {
                    return Err(SnowError::usage(format!(
                        "Artifact {} required for {} {k} does not exist.",
                        p.display(),
                        e.str_of("type").unwrap_or_default()
                    )));
                }
            }
        }
        Ok(())
    })();
    cli.console.phase_end(None);
    r?;
    cli.console.phase_start("Checking remote state");
    let existing = (|| -> Result<indexmap::IndexMap<String, Vec<Vec<Option<String>>>>> {
        let c = cli.execute_query("show integrations")?;
        let rows = cli.rows(&c);
        let have: Vec<String> = rows
            .iter()
            .filter(|r| r.get("type").map(Pv::py_str).as_deref() == Some("EXTERNAL_ACCESS"))
            .map(|r| {
                r.get("name")
                    .map(Pv::py_str)
                    .unwrap_or_default()
                    .to_lowercase()
            })
            .collect();
        let mut declared: Vec<String> = Vec::new();
        for (k, e) in &ents {
            let eai: Vec<String> = e
                .str_list("external_access_integrations")
                .iter()
                .map(|s| s.to_lowercase())
                .collect();
            let secrets = e
                .get("secrets")
                .and_then(PyV::as_dict)
                .is_some_and(|d| !d.is_empty());
            if eai.is_empty() && secrets {
                let f = crate::entities::fqn(e, k)?;
                return Err(SnowError::click(format!(
                    "Can not provide secrets without external access integration for {}",
                    f.identifier()
                ))
                .named("SecretsWithoutExternalAccessIntegrationError"));
            }
            for x in eai {
                if !declared.contains(&x) {
                    declared.push(x);
                }
            }
        }
        let missing: Vec<String> = declared.into_iter().filter(|d| !have.contains(d)).collect();
        if !missing.is_empty() {
            return Err(SnowError::click(format!(
                "Following external access integration does not exists in Snowflake: {}",
                missing.join(", ")
            )));
        }
        let mut existing = indexmap::IndexMap::new();
        if !force_replace {
            for (k, e) in &ents {
                let f = fqn_in_context(cli, e, k)?;
                let (_, types, _) = signature(e);
                let ident = format!("{}({})", f.identifier(), types.join(", "));
                let ty = e.str_of("type").unwrap_or_default();
                let q = format!(
                    "describe {ty} {}",
                    Fqn::from_string(&ident)?.sql_identifier()
                );
                if let Ok(c) = cli.try_query(&q)? {
                    existing.insert(k.clone(), c.rows);
                }
            }
            if !existing.is_empty() && !replace {
                let mut msg =
                    "Following objects already exists. Consider using --replace.\n".to_string();
                let lines: Vec<String> = existing
                    .keys()
                    .map(|k| {
                        format!(
                            "{}: {k}",
                            ents.iter()
                                .find(|(x, _)| x == k)
                                .and_then(|(_, e)| e.str_of("type"))
                                .unwrap_or_default()
                        )
                    })
                    .collect();
                msg.push_str(&lines.join("\n"));
                return Err(SnowError::click(msg));
            }
        }
        Ok(existing)
    })();
    cli.console.phase_end(None);
    let existing = existing?;
    cli.console
        .phase_start("Preparing required stages and artifacts");
    let imports = (|| -> Result<indexmap::IndexMap<String, Vec<String>>> {
        let mut stages: indexmap::IndexMap<String, Vec<Artifact>> = indexmap::IndexMap::new();
        let mut imports: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();
        for (k, e) in &ents {
            let stage = e.str_of("stage").unwrap_or_default();
            let stage_ctx = cli.using_connection(Fqn::from_stage(&stage)?)?.identifier();
            let mut req = Vec::new();
            for (src, dest) in artifact_specs(e) {
                let a = paths.artifact(&src, dest);
                imports
                    .entry(k.clone())
                    .or_default()
                    .push(a.import_path(&stage_ctx));
                req.push(a);
            }
            let s = stages.entry(stage.clone()).or_default();
            for a in req {
                if !s.contains(&a) {
                    s.push(a);
                }
            }
            let deps = paths.dependencies();
            if deps.post_build_path().exists() {
                if !s.contains(&deps) {
                    s.push(deps.clone());
                }
                imports
                    .entry(k.clone())
                    .or_default()
                    .push(deps.import_path(&stage_ctx));
            }
        }
        if prune {
            for stage in stages.keys() {
                cli.console
                    .step(&format!("Removing contents of stage {stage}"));
                let target = snow_api::stage_path::StagePath::from_stage_str(stage);
                cli.execute_query(&format!("remove {}", target.path_for_sql()))?;
            }
        }
        for (stage, arts) in &stages {
            cli.console
                .step(&format!("Creating (if not exists) stage: {stage}"));
            let f = cli.using_connection(Fqn::from_stage(stage)?)?;
            cli.execute_query(&crate::stage::create_query(
                &f,
                false,
                None,
                false,
                Some("deployments managed by Snowflake CLI"),
            ))?;
            let sname = f.identifier();
            for a in arts {
                let pb = a.post_build_path();
                let up = a.upload_path(&sname);
                cli.console
                    .step(&format!("Uploading {} to {up}", file_name(&pb)));
                crate::stage::put(
                    cli,
                    &pb.to_string_lossy(),
                    &snow_api::stage_path::StagePath::from_stage_str(&up),
                    4,
                    true,
                    false,
                )?;
            }
        }
        Ok(imports)
    })();
    cli.console.phase_end(None);
    let imports = imports?;
    cli.console.phase_start("Creating Snowpark entities");
    let rows = (|| -> Result<Vec<Row>> {
        let deps: Vec<String> = std::fs::read_to_string(paths.rel("requirements.snowflake.txt"))
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        for (k, e) in &ents {
            let ty = e.str_of("type").unwrap_or_default();
            let f = fqn_in_context(cli, e, k)?;
            let base = crate::entities::fqn(e, k)?;
            cli.console
                .step(&format!("Creating {ty} {}", base.identifier()));
            let arts = imports.get(k).cloned().unwrap_or_default();
            let exists = existing.contains_key(k);
            let replace_needed = match existing.get(k) {
                Some(rows) => replace_required(e, rows, &deps, &arts),
                None => false,
            };
            let object = format!("{}({})", f.identifier(), full_signature(e).join(", "));
            let mut row = Row::new();
            row.insert("object".into(), Pv::s(object));
            row.insert("type".into(), Pv::s(ty.clone()));
            if exists && !replace_needed {
                row.insert("status".into(), Pv::s("packages updated"));
                out.push(row);
                continue;
            }
            create_or_replace(cli, k, e, &f, &arts, &deps)?;
            row.insert(
                "status".into(),
                Pv::s(if exists {
                    "definition updated"
                } else {
                    "created"
                }),
            );
            out.push(row);
        }
        Ok(out)
    })();
    cli.console.phase_end(None);
    Ok(Output::Collection(rows?))
}

fn create_or_replace(
    cli: &mut Cli,
    id: &str,
    e: &PyV,
    f: &Fqn,
    artifacts: &[String],
    deps: &[String],
) -> Result<()> {
    let mut imports: Vec<String> = e.str_list("imports");
    let mut arts = artifacts.to_vec();
    arts.sort();
    arts.dedup();
    imports.extend(arts);
    let imports: Vec<String> = imports.iter().map(|x| format!("'{x}'")).collect();
    let ty = e.str_of("type").unwrap_or_default();
    let sig = full_signature(e).join(", ");
    let runtime = e
        .get("runtime")
        .filter(|v| !v.is_none())
        .map(PyV::py_str)
        .unwrap_or_else(|| "3.10".into());
    let mut q = vec![
        format!("create or replace {ty} {}({sig})", f.sql_identifier()),
        "copy grants".to_string(),
        format!("returns {}", e.str_of("returns").unwrap_or_default()),
        "language python".to_string(),
        format!("runtime_version={runtime}"),
        format!("imports=({})", imports.join(", ")),
        format!("handler='{}'", e.str_of("handler").unwrap_or_default()),
    ];
    let eai = e.str_list("external_access_integrations");
    if !eai.is_empty() {
        q.push(format!(
            "external_access_integrations=({})",
            eai.iter()
                .map(|x| snow_api::identifiers::sql_object_name(x))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(PyV::Dict(s)) = e.get("secrets")
        && !s.is_empty()
    {
        let items: Vec<String> = s
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    to_string_literal(k),
                    snow_api::identifiers::sql_object_name(&v.py_str())
                )
            })
            .collect();
        q.push(format!("secrets=({})", items.join(", ")));
    }
    let repo_pkgs = e.str_list("artifact_repository_packages");
    let pkgs = e.str_list("packages");
    if !repo_pkgs.is_empty() && !pkgs.is_empty() {
        return Err(SnowError::usage(
            "You cannot specify both artifact_repository_packages and packages.",
        ));
    }
    let declared = if !repo_pkgs.is_empty() {
        repo_pkgs
    } else {
        pkgs
    };
    let repo = e.str_of("artifact_repository").filter(|r| !r.is_empty());
    let list = if let (Some(r), false) = (&repo, declared.is_empty()) {
        if !deps.is_empty() {
            cli.console.warning(&format!(
                "Packages resolved from the Anaconda channel are not sent for {}: ARTIFACT_REPOSITORY applies to the whole PACKAGES clause, so everything the entity needs, snowflake-snowpark-python included, has to come from artifact repository {r}. Declare them in the artifact_repository_packages or packages of the entity. If no entity in the project needs the Anaconda channel, drop requirements.txt, and delete the requirements.snowflake.txt and dependencies.zip left by an earlier build, which deploy still reads.",
                snow_api::sanitize::for_terminal(id)
            ));
        }
        q.push(format!("ARTIFACT_REPOSITORY= {r}"));
        declared
    } else {
        deps.to_vec()
    };
    q.push(format!(
        "packages=({})",
        list.iter()
            .map(|p| to_string_literal(p))
            .collect::<Vec<_>>()
            .join(",")
    ));
    if let Some(PyV::Dict(rc)) = e.get("resource_constraint")
        && !rc.is_empty()
    {
        q.push(format!(
            "RESOURCE_CONSTRAINT=({})",
            rc.iter()
                .map(|(k, v)| format!("{k}='{}'", v.py_str()))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if ty == "procedure" && matches!(e.get("execute_as_caller"), Some(PyV::Bool(true))) {
        q.push("execute as caller".into());
    }
    cli.execute_query(&q.join("\n"))?;
    Ok(())
}

/// `_check_if_replace_is_required(...)` against `describe` rows.
fn replace_required(
    e: &PyV,
    rows: &[Vec<Option<String>>],
    deps: &[String],
    arts: &[String],
) -> bool {
    let mut props: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
    for r in rows {
        let k = r.first().cloned().flatten().unwrap_or_default();
        let v = r.get(1).cloned().flatten().unwrap_or_else(|| "None".into());
        props.insert(k, v);
    }
    let json_list = |s: &str| -> Vec<String> {
        serde_json::from_str::<Vec<String>>(&s.replace('\'', "\"")).unwrap_or_default()
    };
    let std = |v: &[String]| -> std::collections::BTreeSet<String> {
        v.iter()
            .map(|p| match Requirement::parse(p) {
                Ok(r) => {
                    let mut s: Vec<String> =
                        r.specs.iter().map(|(o, v)| format!("{o}{v}")).collect();
                    s.sort();
                    format!("{}{}", r.name, s.join(","))
                }
                Err(_) => p.clone(),
            })
            .collect()
    };
    let old = json_list(props.get("packages").map(String::as_str).unwrap_or("[]"));
    if std(&old) != std(deps) {
        return true;
    }
    let eai: std::collections::BTreeSet<String> = e
        .str_list("external_access_integrations")
        .into_iter()
        .collect();
    let remote_eai: std::collections::BTreeSet<String> = props
        .get("external_access_integrations")
        .map(|s| json_list(s).into_iter().collect())
        .unwrap_or_default();
    if eai != remote_eai {
        return true;
    }
    let handler = e.str_of("handler").unwrap_or_default();
    let returns = e.str_of("returns").unwrap_or_default();
    if props.get("handler").map(|h| h.to_lowercase()) != Some(handler.to_lowercase())
        || !same_type(
            props.get("returns").map(String::as_str).unwrap_or(""),
            &returns,
        )
    {
        return true;
    }
    let pat = regex::Regex::new(r"(?:\[@?\w+_\w+\.)?(\w+(?:/\w+)+\.\w+)(?:\])?").expect("re");
    let mut project: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for s in e.str_list("imports").iter().chain(arts.iter()) {
        for c in pat.captures_iter(&s.to_lowercase()) {
            project.insert(c[1].to_string());
        }
    }
    let remote: std::collections::BTreeSet<String> = props
        .get("imports")
        .map(|s| {
            pat.captures_iter(&s.to_lowercase())
                .map(|c| c[1].to_string())
                .collect()
        })
        .unwrap_or_default();
    if project != remote {
        return true;
    }
    if let Some(rt) = e.get("runtime").filter(|v| !v.is_none()).map(PyV::py_str)
        && Some(&rt) != props.get("runtime_version")
    {
        return true;
    }
    let rc_none = e.get("resource_constraint").is_none_or(|v| v.is_none());
    if !rc_none || props.contains_key("resource_constraint") {
        return true;
    }
    let repo = e.str_of("artifact_repository").filter(|r| !r.is_empty());
    if repo.as_ref() != props.get("artifact_repository") {
        return true;
    }
    let repo_pkgs = e
        .get("artifact_repository_packages")
        .is_some_and(|v| !v.is_none());
    if repo_pkgs || props.contains_key("artifact_repository_packages") {
        return true;
    }
    if e.get("packages").is_some_and(|v| !v.is_none()) {
        return true;
    }
    if e.str_of("type").as_deref() == Some("procedure") {
        let want = if matches!(e.get("execute_as_caller"), Some(PyV::Bool(true))) {
            "CALLER"
        } else {
            "OWNER"
        };
        if props
            .get("execute as")
            .map(String::as_str)
            .unwrap_or("OWNER")
            != want
        {
            return true;
        }
    }
    false
}

fn same_type(sf: &str, local: &str) -> bool {
    let (sf, local) = (sf.to_uppercase(), local.to_uppercase());
    if sf == local {
        return true;
    }
    let local = user_to_sql_type(&local).to_uppercase();
    sf == local || sf.starts_with(&local) || local.starts_with(&sf)
}

fn user_to_sql_type(t: &str) -> String {
    let mapping: [(&str, &str, &[&str]); 8] = [
        ("VARCHAR", "(16777216)", &["CHAR", "TEXT", "STRING"]),
        ("BINARY", "(8388608)", &["BINARY", "VARBINARY"]),
        (
            "NUMBER",
            "(38,0)",
            &[
                "NUMBER", "DECIMAL", "INT", "INTEGER", "BIGINT", "SMALLINT", "TINYINT", "BYTEINT",
            ],
        ),
        (
            "FLOAT",
            "",
            &[
                "FLOAT",
                "DOUBLE",
                "DOUBLE PRECISION",
                "REAL",
                "FLOAT",
                "FLOAT4",
                "FLOAT8",
            ],
        ),
        ("DECFLOAT", "", &["DECFLOAT"]),
        (
            "TIMESTAMP_NTZ",
            "",
            &["TIMESTAMP_NTZ", "TIMESTAMPNTZ", "DATETIME"],
        ),
        ("TIMESTAMP_LTZ", "", &["TIMESTAMP_LTZ", "TIMESTAMPLTZ"]),
        ("TIMESTAMP_TZ", "", &["TIMESTAMP_TZ", "TIMESTAMPTZ"]),
    ];
    let t = t.to_uppercase();
    for (cast, default, types) in mapping {
        for ty in types {
            if t == *ty {
                return format!("{cast}{default}");
            }
            if t.starts_with(ty) {
                return t.replacen(ty, &format!("{cast}{default}"), usize::MAX);
            }
        }
    }
    t
}
