//! Artifact bundling (`api/artifacts/bundle_map.py`, `utils.py`, `upload.py`) and
//! `sync_deploy_root_with_stage` (`api/entities/utils.py`).

use crate::stage::fnmatchcase;
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// `os.path.abspath`: absolute and normalised, symlinks not followed.
pub fn resolve_without_follow(p: &Path) -> PathBuf {
    crate::paths::normalize(&std::env::current_dir().unwrap_or_default().join(p))
}

fn is_in(child: &Path, parent: &Path) -> bool {
    child != parent && child.starts_with(parent)
}

fn not_in_deploy_root(dest: &str, deploy_root: &Path, src: Option<&str>) -> SnowError {
    let mut m = format!(
        "The specified destination path is outside of the deploy root, or\nwould entirely replace it. This can happen when a relative path\nwith \"..\" is provided, or when \".\" is used as the destination\n(use \"./\" instead to copy into the deploy root).\ndestination = {dest}\ndeploy root = {}",
        deploy_root.display()
    );
    if let Some(s) = src {
        m.push_str(&format!("\nsource = {s}"));
    }
    SnowError::click(m).named("NotInDeployRootError")
}

fn too_many(dest: &Path) -> SnowError {
    SnowError::click(format!(
        "Multiple file or directories were mapped to one output destination.\ndestination = {}",
        dest.display()
    ))
    .named("TooManyFilesError")
}

/// `pathlib.Path(root).glob(pattern)` (sorted; `**` recursive; a trailing `/` keeps directories).
pub fn path_glob(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let dirs_only = pattern.ends_with('/');
    let comps: Vec<&str> = pattern
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let mut cur = vec![root.to_path_buf()];
    for comp in &comps {
        let mut next = Vec::new();
        for base in &cur {
            if *comp == "**" {
                let mut all = vec![base.clone()];
                let mut stack = vec![base.clone()];
                while let Some(d) = stack.pop() {
                    let mut kids: Vec<PathBuf> = std::fs::read_dir(&d)
                        .map(|rd| {
                            rd.filter_map(|e| e.ok())
                                .map(|e| e.path())
                                .filter(|p| p.is_dir())
                                .collect()
                        })
                        .unwrap_or_default();
                    kids.sort();
                    for k in kids.into_iter().rev() {
                        all.push(k.clone());
                        stack.push(k);
                    }
                }
                all.sort();
                next.extend(all);
            } else if comp.contains(['*', '?', '[']) {
                let Ok(rd) = std::fs::read_dir(base) else {
                    continue;
                };
                let mut names: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                for n in names {
                    if fnmatchcase(&n, comp) {
                        next.push(base.join(n));
                    }
                }
            } else if *comp == ".." {
                next.push(base.join(".."));
            } else {
                let p = base.join(comp);
                if p.exists() {
                    next.push(p);
                }
            }
        }
        cur = next;
    }
    if comps.is_empty() {
        return vec![];
    }
    let mut out: Vec<PathBuf> = cur
        .into_iter()
        .filter(|p| !dirs_only || p.is_dir())
        .collect();
    out.dedup();
    out
}

#[derive(Default)]
pub struct BundleMap {
    pub project_root: PathBuf,
    pub deploy_root: PathBuf,
    pairs: Vec<(PathBuf, PathBuf)>,
    src_to_dest: HashMap<PathBuf, Vec<PathBuf>>,
    dest_to_src: HashMap<PathBuf, PathBuf>,
    dest_is_dir: HashMap<PathBuf, bool>,
    ignore: HashMap<PathBuf, Vec<String>>,
}

impl BundleMap {
    pub fn new(project_root: &Path, deploy_root: &Path) -> BundleMap {
        BundleMap {
            project_root: resolve_without_follow(project_root),
            deploy_root: resolve_without_follow(deploy_root),
            ..Default::default()
        }
    }

    fn absolute_src(&self, src: &Path) -> Result<PathBuf> {
        let r = resolve_without_follow(&if src.is_absolute() {
            src.to_path_buf()
        } else {
            self.project_root.join(src)
        });
        if !is_in(&r, &self.project_root) {
            return Err(SnowError::click(format!(
                "Source is not in the project root: {}, root={}",
                src.display(),
                self.project_root.display()
            ))
            .named("ArtifactError"));
        }
        Ok(r)
    }

    fn absolute_dest(&self, dest: &Path, src: Option<&Path>) -> Result<PathBuf> {
        let r = resolve_without_follow(&if dest.is_absolute() {
            dest.to_path_buf()
        } else {
            self.deploy_root.join(dest)
        });
        if r != self.deploy_root && !is_in(&r, &self.deploy_root) {
            return Err(not_in_deploy_root(
                &dest.display().to_string(),
                &self.deploy_root,
                src.map(|s| s.display().to_string()).as_deref(),
            ));
        }
        Ok(r)
    }

    fn update_dest_is_dir(&mut self, dest: &Path, is_dir: bool) -> Result<()> {
        if let Some(cur) = self.dest_is_dir.get(dest)
            && *cur != is_dir
        {
            return Err(SnowError::click(format!(
                "Conflicting type for destination path: {}",
                dest.display()
            ))
            .named("ArtifactError"));
        }
        if let Some(parent) = dest.parent()
            && parent != dest
            && !dest.as_os_str().is_empty()
        {
            self.update_dest_is_dir(parent, true)?;
        }
        self.dest_is_dir.insert(dest.to_path_buf(), is_dir);
        Ok(())
    }

    fn put(&mut self, src: PathBuf, dest: PathBuf, dest_is_dir: bool) -> Result<()> {
        let abs_src = self.project_root.join(&src);
        let src_is_dir = abs_src.is_dir();
        if dest_is_dir {
            if self.dest_is_dir.get(&dest).copied().unwrap_or(false) {
                for (child, existing) in &self.dest_to_src {
                    if *child == dest || !child.starts_with(&dest) {
                        continue;
                    }
                    let rel = child.strip_prefix(&dest).unwrap_or(child);
                    if *existing != src.join(rel) {
                        return Err(too_many(child));
                    }
                }
                return Ok(());
            }
        } else if let Some(cur) = self.dest_to_src.get(&dest) {
            if *cur != src {
                return Err(too_many(&dest));
            }
            return Ok(());
        }
        if src_is_dir {
            for entry in walk(&abs_src) {
                let rel = entry.strip_prefix(&abs_src).unwrap_or(&entry).to_path_buf();
                if entry.is_dir() {
                    self.update_dest_is_dir(&dest.join(&rel), true)?;
                } else {
                    let child_dest = dest.join(&rel);
                    let child_src = src.join(&rel);
                    self.update_dest_is_dir(&child_dest, false)?;
                    if let Some(e) = self.dest_to_src.get(&child_dest)
                        && *e != child_src
                    {
                        return Err(too_many(&child_dest));
                    }
                    self.dest_to_src.insert(child_dest, child_src);
                }
            }
        }
        self.update_dest_is_dir(&dest, dest_is_dir)?;
        let dests = self.src_to_dest.entry(src.clone()).or_default();
        if !dests.contains(&dest) {
            dests.push(dest.clone());
            self.dest_to_src.insert(dest.clone(), src.clone());
            self.pairs.push((src, dest));
        }
        Ok(())
    }

    fn add_one(&mut self, src: &Path, dest: &Path, map_as_child: bool) -> Result<()> {
        let abs_src = self.absolute_src(src)?;
        let abs_dest = self.absolute_dest(dest, Some(src))?;
        let mut dest_is_dir = abs_src.is_dir() || map_as_child;
        if abs_dest == self.deploy_root && !map_as_child {
            return Err(not_in_deploy_root(
                &dest.display().to_string(),
                &self.deploy_root,
                Some(&src.display().to_string()),
            ));
        }
        if is_in(&abs_src, &self.deploy_root) {
            return Ok(());
        }
        let canon_src = abs_src
            .strip_prefix(&self.project_root)
            .unwrap_or(&abs_src)
            .to_path_buf();
        let mut canon_dest = abs_dest
            .strip_prefix(&self.deploy_root)
            .unwrap_or(&abs_dest)
            .to_path_buf();
        if map_as_child {
            canon_dest = canon_dest.join(canon_src.file_name().unwrap_or_default());
            dest_is_dir = abs_src.is_dir();
        }
        self.put(canon_src, canon_dest, dest_is_dir)
    }

    /// `BundleMap.add(PathMapping)` (glob patterns).
    pub fn add(&mut self, src: &str, dest: Option<&str>, ignore: &[String]) -> Result<()> {
        if Path::new(src).is_absolute() {
            return Err(
                SnowError::click("Source path must be a relative path").named("ArtifactError")
            );
        }
        let mut found = false;
        for resolved in path_glob(&self.project_root.clone(), src) {
            let name = resolved
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !ignore.is_empty() && ignore.iter().any(|p| fnmatchcase(&name, p)) {
                continue;
            }
            found = true;
            if !ignore.is_empty() {
                let canon = resolved
                    .strip_prefix(&self.project_root)
                    .unwrap_or(&resolved)
                    .to_path_buf();
                self.ignore.insert(canon, ignore.to_vec());
            }
            match dest {
                Some(d) => {
                    let stem = d.trim_end_matches('/');
                    if stem.is_empty() {
                        return Err(not_in_deploy_root(
                            d,
                            &self.deploy_root,
                            Some(&resolved.display().to_string()),
                        ));
                    }
                    if Path::new(stem).is_absolute() {
                        return Err(SnowError::click("Destination path must be a relative path")
                            .named("ArtifactError"));
                    }
                    self.add_one(&resolved, Path::new(stem), d.ends_with('/'))?;
                }
                None => {
                    let rel = resolved
                        .strip_prefix(&self.project_root)
                        .unwrap_or(&resolved)
                        .to_path_buf();
                    self.add_one(&resolved, &rel, false)?;
                }
            }
        }
        if !found {
            return Err(SnowError::click(format!(
                "No match was found for the specified source in the project directory\n: {src}"
            ))
            .named("SourceNotFoundError"));
        }
        Ok(())
    }

    /// `all_mappings(absolute=True, expand_directories=True)`.
    pub fn all_mappings_expanded(&self) -> Vec<(PathBuf, PathBuf)> {
        let mut out = Vec::new();
        for (src, dest) in &self.pairs {
            let a_src = self.project_root.join(src);
            let a_dest = self.deploy_root.join(dest);
            out.push((a_src.clone(), a_dest.clone()));
            if a_src.is_dir() {
                let ignore = self.ignore.get(src).cloned().unwrap_or_default();
                for e in walk(&a_src) {
                    let rel = e.strip_prefix(&a_src).unwrap_or(&e);
                    if !ignore.is_empty()
                        && rel.components().any(|c| {
                            ignore
                                .iter()
                                .any(|p| fnmatchcase(&c.as_os_str().to_string_lossy(), p))
                        })
                    {
                        continue;
                    }
                    out.push((e.clone(), a_dest.join(rel)));
                }
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// `all_mappings(absolute=True, expand_directories=False)`.
    pub fn all_mappings_absolute(&self) -> Vec<(PathBuf, PathBuf)> {
        self.pairs
            .iter()
            .map(|(s, d)| (self.project_root.join(s), self.deploy_root.join(d)))
            .collect()
    }

    /// `all_sources(absolute=True)` in insertion order.
    pub fn all_sources_absolute(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for (s, _) in &self.pairs {
            let a = self.project_root.join(s);
            if !out.contains(&a) {
                out.push(a);
            }
        }
        out
    }

    /// `to_deploy_paths(src)` for an absolute source path (absolute results).
    pub fn to_deploy_paths(&self, src: &Path) -> Vec<PathBuf> {
        let Ok(abs) = self.absolute_src(src) else {
            return vec![];
        };
        if !abs.exists() {
            return vec![];
        }
        let canon = abs
            .strip_prefix(&self.project_root)
            .unwrap_or(&abs)
            .to_path_buf();
        let mut out: Vec<PathBuf> = self
            .src_to_dest
            .get(&canon)
            .map(|ds| ds.iter().map(|d| self.deploy_root.join(d)).collect())
            .unwrap_or_default();
        if let Some(parent) = abs.parent()
            && parent != abs
            && is_in(parent, &self.project_root)
        {
            let child = abs.strip_prefix(parent).unwrap_or(&abs).to_path_buf();
            for d in self.to_deploy_paths(parent) {
                out.push(d.join(&child));
            }
        }
        out
    }

    /// `to_project_path(dest)` for a deploy-root-relative path.
    pub fn to_project_path(&self, dest: &Path) -> Option<PathBuf> {
        if let Some(s) = self.dest_to_src.get(dest) {
            return Some(s.clone());
        }
        let parent = dest.parent()?;
        if parent == dest || dest.as_os_str().is_empty() {
            return None;
        }
        let psrc = self.to_project_path(parent)?;
        let child = dest.strip_prefix(parent).ok()?;
        let cand = psrc.join(child);
        self.project_root.join(&cand).exists().then_some(cand)
    }
}

/// `os.walk` order flattened: each directory's entries (sorted), directories recursed.
/// Every path under `dir`, depth first in sorted order (= sorted by `Path` order).
pub(crate) fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut kids: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    kids.sort();
    for k in kids {
        if k.is_dir() {
            out.push(k.clone());
            out.extend(walk(&k));
        } else {
            out.push(k);
        }
    }
    out
}

/// Write a deploy-root file. A bundled file is a symlink into the project
/// source, so unlink it first; writing through it would edit the user's file.
pub fn write_in_bundle(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    if path.is_symlink() {
        std::fs::remove_file(path)?;
    }
    std::fs::write(path, contents)
}

fn symlink_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p)?;
    }
    if src.is_file() {
        let _ = std::fs::remove_file(dst);
        #[cfg(unix)]
        if std::os::unix::fs::symlink(src, dst).is_ok() {
            return Ok(());
        }
        std::fs::copy(src, dst)?;
    } else {
        std::fs::create_dir_all(dst)?;
    }
    Ok(())
}

/// An artifact entry (`PathMapping` model) as (src, dest, ignore).
pub fn mapping_of(v: &PyV) -> (String, Option<String>, Vec<String>) {
    match v {
        PyV::Str(s) => (s.clone(), None, vec![]),
        other => (
            other.str_of("src").unwrap_or_default(),
            other.str_of("dest"),
            other
                .get("ignore")
                .and_then(|i| i.as_list())
                .map(|l| l.iter().map(PyV::py_str).collect())
                .unwrap_or_default(),
        ),
    }
}

/// `bundle_artifacts(project_paths, artifacts)`.
pub fn bundle(project_root: &Path, bundle_root: &Path, artifacts: &[PyV]) -> Result<BundleMap> {
    let mut m = BundleMap::new(project_root, bundle_root);
    for a in artifacts {
        let (src, dest, ignore) = mapping_of(a);
        m.add(&src, dest.as_deref(), &ignore)?;
    }
    for (s, d) in m.all_mappings_expanded() {
        if s.is_file() {
            symlink_or_copy(&s, &d)?;
        }
    }
    Ok(m)
}

/// `symlink_or_copy(src, dst, deploy_root, project_root)`: directories are
/// recreated (including empty ones) and their files linked one by one.
pub fn symlink_or_copy_tree(
    src: &Path,
    dst: &Path,
    deploy_root: &Path,
    project_root: &Path,
) -> Result<()> {
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p)?;
    }
    let resolved_deploy = deploy_root
        .canonicalize()
        .unwrap_or_else(|_| deploy_root.to_path_buf());
    let resolved_dst = dst
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.join(dst.file_name().unwrap_or_default()))
        .unwrap_or_else(|| dst.to_path_buf());
    if resolved_dst != resolved_deploy && !resolved_dst.starts_with(&resolved_deploy) {
        return Err(not_in_deploy_root(
            &dst.display().to_string(),
            deploy_root,
            Some(&src.display().to_string()),
        ));
    }
    let abs = resolve_without_follow(src);
    if abs.is_file() {
        return symlink_or_copy(&abs, dst);
    }
    std::fs::create_dir_all(dst)?;
    let real_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let mut kids: Vec<PathBuf> = std::fs::read_dir(&abs)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    kids.sort();
    for k in kids {
        let stays = k
            .canonicalize()
            .map(|r| r == real_root || r.starts_with(&real_root))
            .unwrap_or(false);
        if !stays {
            continue;
        }
        let target = dst.join(k.file_name().unwrap_or_default());
        if k.is_dir() {
            symlink_or_copy_tree(&k, &target, deploy_root, project_root)?;
        } else {
            symlink_or_copy(&resolve_without_follow(&k), &target)?;
        }
    }
    Ok(())
}

/// `nativeapp.artifacts.build_bundle`: recreate the deploy root and link every artifact.
pub fn build_native_bundle(
    project_root: &Path,
    deploy_root: &Path,
    artifacts: &[PyV],
) -> Result<BundleMap> {
    let resolved = deploy_root
        .canonicalize()
        .unwrap_or_else(|_| resolve_without_follow(deploy_root));
    if resolved.exists() && !resolved.is_dir() {
        return Err(SnowError::click(format!(
            "Deploy root {} exists, but is not a directory!",
            resolved.display()
        ))
        .named("DeployRootError"));
    }
    let proot = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    if !resolved.starts_with(&proot) || resolved == proot {
        return Err(SnowError::click(format!(
            "Deploy root {} is not a descendent of the project directory!",
            resolved.display()
        ))
        .named("DeployRootError"));
    }
    if resolved.exists() {
        std::fs::remove_dir_all(&resolved)?;
    }
    let mut m = BundleMap::new(project_root, deploy_root);
    for a in artifacts {
        let (src, dest, ignore) = mapping_of(a);
        m.add(&src, dest.as_deref(), &ignore)?;
    }
    for (s, d) in m.all_mappings_absolute() {
        symlink_or_copy_tree(&s, &d, deploy_root, project_root)?;
    }
    if m.is_empty() {
        return Err(SnowError::click(
            "No artifacts mapping found in project definition, nothing to do.",
        )
        .named("ArtifactError"));
    }
    Ok(m)
}

/// `sync_deploy_root_with_stage(...)` for a named (non-package) stage.
pub fn sync_deploy_root_with_stage(
    cli: &mut Cli,
    deploy_root: &Path,
    bundle: &BundleMap,
    prune: bool,
    stage_root: &str,
) -> Result<crate::stage_diff::DiffResult> {
    sync_inner(cli, deploy_root, bundle, prune, stage_root, false, false)
}

/// The same for a `snow://` versioned stage (no CREATE STAGE), optionally overwriting.
pub fn sync_vstage(
    cli: &mut Cli,
    deploy_root: &Path,
    bundle: &BundleMap,
    prune: bool,
    stage_root: &str,
    force_overwrite: bool,
) -> Result<crate::stage_diff::DiffResult> {
    sync_inner(
        cli,
        deploy_root,
        bundle,
        prune,
        stage_root,
        true,
        force_overwrite,
    )
}

fn sync_inner(
    cli: &mut Cli,
    deploy_root: &Path,
    bundle: &BundleMap,
    prune: bool,
    stage_root: &str,
    vstage: bool,
    force_overwrite: bool,
) -> Result<crate::stage_diff::DiffResult> {
    let (stage_name, directory, full_path) = crate::stage_execute::parts_for(stage_root)?;
    if !vstage {
        let stage_fqn = crate::stage_execute::stage_of(stage_root)?;
        cli.console.step(&format!(
            "Creating stage {} if not exists.",
            stage_fqn.identifier()
        ));
        cli.execute_query(&crate::stage::create_query(
            &stage_fqn, false, None, false, None,
        ))?;
    }
    let path = crate::stage_execute::parts_path(stage_root)?;
    let resolved = crate::stage::resolve(deploy_root);
    cli.console.step(&format!(
        "Performing a diff between the Snowflake stage: {path} and your local deploy_root: {}.",
        resolved.display()
    ));
    let mut diff =
        crate::stage_diff::compute(cli, deploy_root, &stage_name, &directory, &full_path)?;
    if !prune {
        let not_removed = std::mem::take(&mut diff.only_on_stage);
        if !not_removed.is_empty() {
            cli.console.warning(&format!(
                "The following files exist only on the stage:\n{}\n\nUse the --prune flag to delete them from the stage.",
                not_removed.join("\n")
            ));
        }
    }
    crate::stage_diff::print_with_map(cli, &diff, Some(bundle));
    if diff.has_changes() {
        cli.console.step(&format!(
            "Uploading files from local {} directory to stage.",
            resolved.display()
        ));
        crate::logs::info(
            "snowflake.cli._plugins.stage.diff",
            &format!(
                "Uploading diff-ed files from your local {} directory to the Snowflake stage.",
                deploy_root.display()
            ),
        );
        for p in &diff.only_on_stage {
            let target = snow_api::stage_path::StagePath::from_stage_str(&full_path).join(p);
            cli.execute_query(&format!("remove {}", target.path_for_sql()))?;
        }
        for (paths, overwrite) in [(&diff.different, true), (&diff.only_local, force_overwrite)] {
            for p in paths {
                let sub = Path::new(p)
                    .parent()
                    .map(|x| x.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let dest = if sub.is_empty() {
                    full_path.clone()
                } else {
                    format!("{full_path}/{sub}")
                };
                let local = deploy_root.join(p);
                crate::stage::put(
                    cli,
                    &local.to_string_lossy(),
                    &snow_api::stage_path::StagePath::from_stage_str(&dest),
                    4,
                    overwrite,
                    false,
                )?;
            }
        }
    }
    Ok(diff)
}
