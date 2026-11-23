//! `snow stage diff` and the stage/local diff used by deploys (`stage/diff.py`, `md5.py`).

use md5::{Digest, Md5};
use snow_api::output::{Output, Pv};
use snow_api::{Cli, Result, SnowError};
use std::path::{Path, PathBuf};

#[derive(Default, Debug)]
pub struct DiffResult {
    pub identical: Vec<String>,
    pub different: Vec<String>,
    pub only_local: Vec<String>,
    pub only_on_stage: Vec<String>,
}

impl DiffResult {
    pub fn has_changes(&self) -> bool {
        !self.different.is_empty() || !self.only_local.is_empty() || !self.only_on_stage.is_empty()
    }
    pub fn to_output(&self) -> Output {
        let sorted = |v: &Vec<String>| {
            let mut v = v.clone();
            v.sort_by(|a, b| posix_cmp(a, b));
            Pv::List(v.into_iter().map(Pv::s).collect())
        };
        Output::object(vec![
            ("modified", sorted(&self.different)),
            ("added", sorted(&self.only_local)),
            ("deleted", sorted(&self.only_on_stage)),
        ])
    }
}

/// `PurePosixPath` ordering (part by part).
pub fn posix_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let pa: Vec<&str> = a.split('/').collect();
    let pb: Vec<&str> = b.split('/').collect();
    pa.cmp(&pb)
}

pub fn enumerate_files(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.is_dir() {
        return Err(SnowError::unexpected("Path must point to a directory").named("ValueError"));
    }
    let mut out = Vec::new();
    let mut kids: Vec<PathBuf> = std::fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    kids.sort();
    for k in kids {
        if k.is_dir() {
            out.extend(enumerate_files(&k)?);
        } else {
            out.push(k);
        }
    }
    Ok(out)
}

const S3_MIN_PART_SIZE: u64 = 5 * 1024 * 1024;
const S3_MAX_PARTS: u64 = 10_000;
const S3_CHUNK_SIZE: u64 = 8_388_608;

pub fn compute_md5sum(file: &Path, chunk_size: Option<u64>) -> std::io::Result<String> {
    let data = std::fs::read(file)?;
    if data.is_empty() {
        return Ok(hex(&Md5::digest(b"")));
    }
    let Some(cs) = chunk_size else {
        return Ok(hex(&Md5::digest(&data)));
    };
    let mut digests = Vec::new();
    for chunk in data.chunks(cs as usize) {
        digests.extend_from_slice(&Md5::digest(chunk));
    }
    Ok(format!(
        "{}-{}",
        hex(&Md5::digest(&digests)),
        data.len().div_ceil(cs as usize)
    ))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn file_matches_md5sum(file: &Path, remote: Option<&str>) -> Result<bool> {
    let Some(remote) = remote.filter(|r| !r.is_empty()) else {
        return Ok(false);
    };
    if regex::Regex::new(r"^[A-Fa-f0-9]{32}$")
        .expect("re")
        .is_match(remote)
    {
        return Ok(compute_md5sum(file, None)? == remote);
    }
    if let Some(c) = regex::Regex::new(r"^([A-Fa-f0-9]{32})-(\d+)$")
        .expect("re")
        .captures(remote)
    {
        let n: u64 = c[2].parse().unwrap_or(1);
        let size = std::fs::metadata(file)?.len();
        if n == S3_MAX_PARTS {
            let cs = size.div_ceil(S3_MAX_PARTS).max(S3_MIN_PART_SIZE);
            if compute_md5sum(file, Some(cs))? == remote {
                return Ok(true);
            }
        }
        for align in [S3_CHUNK_SIZE, 1024 * 1024] {
            let mult = 1 + (size.saturating_sub(1)) / (n * align);
            if compute_md5sum(file, Some(mult * align))? == remote {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    Err(SnowError::click(format!("Unknown md5 format: {remote}")))
}

/// `compute_stage_diff(local_root, stage_path_parts)`; `stage_name`/`directory`
/// are the parts of the stage path, `full_path` what `ls` is run on.
pub fn compute(
    cli: &mut Cli,
    local_root: &Path,
    stage_name: &str,
    directory: &str,
    full_path: &str,
) -> Result<DiffResult> {
    let local = enumerate_files(local_root)?;
    let c = cli.execute_query(&crate::stage::list_files_query(
        &snow_api::stage_path::StagePath::from_stage_str(full_path),
        None,
    ))?;
    let (ni, mi) = (c.column_index("name").unwrap_or(0), c.column_index("md5"));
    let sname = snow_api::identifiers::unquote_identifier(stage_name).to_lowercase();
    let mut remote: indexmap::IndexMap<String, Option<String>> = indexmap::IndexMap::new();
    for r in &c.rows {
        let name = r.get(ni).cloned().flatten().unwrap_or_default();
        let wo = name
            .strip_prefix(&sname)
            .unwrap_or(&name)
            .trim_start_matches('/');
        let rel = wo
            .strip_prefix(directory)
            .unwrap_or(wo)
            .trim_start_matches('/');
        let rel = snow_api::stage_path::posix_str(&snow_api::stage_path::posix_parts(rel));
        remote.insert(rel, mi.and_then(|i| r.get(i).cloned().flatten()));
    }
    let mut out = DiffResult::default();
    for f in local {
        let rel = f
            .strip_prefix(local_root)
            .unwrap_or(&f)
            .to_string_lossy()
            .replace('\\', "/");
        match remote.shift_remove(&rel) {
            None => out.only_local.push(rel),
            Some(md5) => match file_matches_md5sum(&f, md5.as_deref()) {
                Ok(true) => out.identical.push(rel),
                Ok(false) => out.different.push(rel),
                Err(_) => {
                    crate::logs::log(
                        crate::logs::WARNING,
                        "snowflake.cli._plugins.stage.diff",
                        &format!(
                            "Could not compare md5 for {}, assuming file has changed",
                            f.display()
                        ),
                    );
                    out.different.push(rel)
                }
            },
        }
    }
    out.only_on_stage.extend(remote.into_keys());
    Ok(out)
}

fn diff_line(status: &str, src: Option<&str>, dest: &str) -> String {
    let pad = " ".repeat("modified".len() - status.len());
    let src = src.map(|s| format!("{s} -> ")).unwrap_or_default();
    format!("[red]{status}[/red]: {pad}{src}{dest}")
}

/// `print_diff_to_console` (no bundle map).
pub fn print(cli: &mut Cli, d: &DiffResult) {
    print_with_map(cli, d, None)
}

/// `print_diff_to_console(diff, bundle_map)`.
pub fn print_with_map(cli: &mut Cli, d: &DiffResult, map: Option<&crate::artifacts::BundleMap>) {
    if !d.has_changes() {
        cli.console
            .message("Your stage is up-to-date with your local deploy root.");
        return;
    }
    let mut blank = false;
    if !d.only_local.is_empty() || !d.different.is_empty() {
        cli.console.message("Local changes to be deployed:");
        let pair = |p: &String| -> (Option<String>, String) {
            match map {
                None => (None, p.clone()),
                Some(m) => (
                    Some(
                        m.to_project_path(Path::new(p))
                            .map(|s| s.display().to_string())
                            .unwrap_or_else(|| "?".into()),
                    ),
                    p.clone(),
                ),
            }
        };
        let mut lines: Vec<((Option<String>, String), String)> = Vec::new();
        for p in &d.different {
            let k = pair(p);
            let l = diff_line("modified", k.0.as_deref(), &k.1);
            lines.push((k, l));
        }
        for p in &d.only_local {
            let k = pair(p);
            let l = diff_line("added", k.0.as_deref(), &k.1);
            lines.push((k, l));
        }
        lines.sort_by(|a, b| a.0.cmp(&b.0));
        cli.console.indent();
        for (_, l) in lines {
            cli.console.message(&l);
        }
        cli.console.dedent();
        blank = true;
    }
    if !d.only_on_stage.is_empty() {
        if blank {
            cli.console.message("");
        }
        cli.console
            .message("Deleted paths to be removed from your stage:");
        let mut v = d.only_on_stage.clone();
        v.sort_by(|a, b| posix_cmp(a, b));
        cli.console.indent();
        for p in v {
            cli.console.message(&diff_line("deleted", None, &p));
        }
        cli.console.dedent();
    }
}

pub fn stage_diff(cli: &mut Cli) -> Result<Output> {
    let stage = cli.str("stage_name").unwrap_or_default();
    let folder = cli.str("folder_name").unwrap_or_default();
    let parts = crate::stage_execute::parts_for(&stage)?;
    let d = compute(cli, Path::new(&folder), &parts.0, &parts.1, &parts.2)?;
    if cli.format.is_json() {
        return Ok(d.to_output());
    }
    print(cli, &d);
    Ok(Output::Empty)
}
