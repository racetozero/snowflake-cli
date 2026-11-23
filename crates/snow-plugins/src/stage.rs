//! `snow stage` (SQL commands; PUT/GET in `copy` need file transfer).

use snow_api::identifiers::{Fqn, to_string_literal};
use snow_api::output::Output;
use snow_api::stage_path::StagePath;
use snow_api::{Cli, Result, SnowError};

pub fn is_stage_path(p: &str) -> bool {
    p.starts_with('@') || p.starts_with("snow://")
}

/// A temporary stage `name` in the project's database and schema, completed
/// from the connection.
pub fn temp_stage_name(cli: &mut Cli, project: &Fqn, name: String) -> Result<Fqn> {
    let stage = Fqn::from_string(&name)?
        .set_database(project.database.as_deref())
        .set_schema(project.schema.as_deref());
    cli.using_connection(stage)
}

/// Sort key for result rows: the parent directory, then the file name.
pub fn dir_then_name(s: &str) -> (&str, &str) {
    s.rsplit_once('/').unwrap_or(("", s))
}

pub fn list_files_query(path: &StagePath, pattern: Option<&str>) -> String {
    let mut q = format!("ls {}", path.path_for_sql());
    if let Some(p) = pattern {
        q.push_str(&format!(" pattern = {}", to_string_literal(p)));
    }
    q
}

pub fn list_files(cli: &mut Cli) -> Result<Output> {
    let p = StagePath::from_stage_str(&cli.str("stage_name").unwrap_or_default());
    let pattern = cli.str("pattern");
    let c = cli.execute_query(&list_files_query(&p, pattern.as_deref()))?;
    Ok(cli.query_result(&c))
}

pub fn create_query(
    fqn: &Fqn,
    temporary: bool,
    encryption: Option<&str>,
    enable_directory: bool,
    comment: Option<&str>,
) -> String {
    let t = if temporary { "temporary " } else { "" };
    let mut q = format!("create {t}stage if not exists {}", fqn.sql_identifier());
    if let Some(e) = encryption {
        q.push_str(&format!(" encryption = (type = '{e}')"));
    }
    if enable_directory {
        q.push_str(" directory = (enable = true)");
    }
    if let Some(c) = comment.filter(|c| !c.is_empty()) {
        q.push_str(&format!(" comment='{c}'"));
    }
    q
}

pub fn create(cli: &mut Cli) -> Result<Output> {
    let fqn = crate::object::fqn_arg(cli, "stage_name")?;
    let enc = cli.str("encryption");
    let q = create_query(
        &fqn,
        false,
        enc.as_deref(),
        cli.flag("enable_directory"),
        None,
    );
    let c = cli.execute_query(&q)?;
    Ok(cli.single_query_result(&c))
}

pub fn remove(cli: &mut Cli) -> Result<Output> {
    let fqn = crate::object::fqn_arg(cli, "stage_name")?;
    let file = cli.str("file_name").unwrap_or_default();
    let p = StagePath::from_stage_str(&fqn.identifier()).join(&file);
    let c = cli.execute_query(&format!("remove {}", p.path_for_sql()))?;
    Ok(cli.single_query_result(&c))
}

pub fn copy_files_query(src: &str, dest: &str) -> Result<String> {
    copy_files_query_from(StagePath::from_stage_str(src), dest)
}

/// `copy_files` with an already built source path (git repositories use `from_git_str`).
pub fn copy_files_query_from(s: StagePath, dest: &str) -> Result<String> {
    let d = StagePath::from_stage_str(dest);
    if d.is_user_stage() {
        return Err(SnowError::cli(
            "Destination path cannot be a user stage. Please provide a named stage.",
        ));
    }
    Ok(format!(
        "copy files into {} from {}",
        d.with_trailing_slash().path_for_sql(),
        s.path_for_sql()
    ))
}

pub fn copy(cli: &mut Cli) -> Result<Output> {
    let src = cli.str("source_path").unwrap_or_default();
    let dest = cli.str("destination_path").unwrap_or_default();
    let (get, is_put) = (is_stage_path(&src), is_stage_path(&dest));
    if get && is_put {
        let q = copy_files_query(&src, &dest)?;
        let c = cli.execute_query(&q)?;
        return Ok(cli.query_result(&c));
    }
    if !get && !is_put {
        return Err(SnowError::click(
            "Both source and target path are local. This operation is not supported.",
        ));
    }
    let parallel = cli.int("parallel").unwrap_or(4);
    let recursive = cli.flag("recursive");
    if get {
        let cwd = std::env::current_dir().unwrap_or_default();
        let target = resolve(&cwd.join(&dest));
        if !recursive {
            cli.console.warning(
                "Use `--recursive` flag, which copy files recursively with directory structure. This will be the default behavior in the future.",
            );
            let c = get_files(
                cli,
                &StagePath::from_stage_str(&src),
                &target,
                parallel,
                None,
            )?;
            return Ok(cli.query_result(&c));
        }
        let rows = get_recursive(cli, &src, &target, parallel, false)?;
        return Ok(Output::Collection(rows));
    }
    let overwrite = cli.flag("overwrite");
    let auto_compress = cli.flag("auto_compress");
    let local = std::path::PathBuf::from(&src);
    let out = if recursive && !local.is_file() {
        Output::Collection(put_recursive(
            cli,
            &local,
            &dest,
            parallel,
            overwrite,
            auto_compress,
        )?)
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        let abs = resolve(&cwd.join(&local));
        let c = put(
            cli,
            &abs.to_string_lossy(),
            &StagePath::from_stage_str(&dest),
            parallel,
            overwrite,
            auto_compress,
        )?;
        cli.query_result(&c)
    };
    if cli.flag("refresh") {
        let stage = StagePath::from_stage_str(&dest);
        cli.execute_query(&format!("ALTER STAGE {} REFRESH", stage.stage()))?;
    }
    Ok(out)
}

/// `Path.resolve()` (non-strict): absolute, `..` folded, symlinks followed where they exist.
pub fn resolve(p: &std::path::Path) -> std::path::PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    match (out.parent(), out.file_name()) {
        (Some(par), Some(name)) if par.exists() => resolve(par).join(name),
        _ => out,
    }
}

static UNQUOTED_URI: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"^[\w/*?\-.=&{}$#\[\]"\\!@%^+:]+$"#).expect("re")
});

/// `StageManager._to_uri`.
pub fn to_uri(local: &str) -> String {
    let uri = format!("file://{local}");
    if UNQUOTED_URI.is_match(&uri) {
        return uri;
    }
    snow_api::identifiers::to_string_literal(&uri.replace('\\', "\\\\"))
}

/// `glob.escape`.
pub fn glob_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '*' | '?' | '[' => format!("[{c}]"),
            other => other.to_string(),
        })
        .collect()
}

/// `StageManager.put`.
pub fn put(
    cli: &mut Cli,
    local: &str,
    stage: &StagePath,
    parallel: i64,
    overwrite: bool,
    auto_compress: bool,
) -> Result<snow_connector::Cursor> {
    let mut local = local.to_string();
    if !local.contains('*') && std::path::Path::new(&local).is_dir() {
        local = format!("{}/*", glob_escape(&local));
    }
    crate::logs::info(
        "snowflake.cli._plugins.stage.manager",
        &format!("Uploading {local} to {}", stage.absolute_path()),
    );
    cli.execute_query(&format!(
        "put {} {} auto_compress={} parallel={parallel} overwrite={}",
        to_uri(&local),
        stage.path_for_sql(),
        auto_compress,
        crate::spcs::py_bool(overwrite)
    ))
}

fn ensure_dir(p: &std::path::Path) -> Result<()> {
    if !p.exists() {
        snow_api::secure::mkdir_p(p)?;
    }
    if !p.is_dir() {
        return Err(
            SnowError::cli(format!("Path {} is not a directory", p.display()))
                .named("DirectoryIsNotEmptyError"),
        );
    }
    Ok(())
}

/// `StageManager.get` (one GET into `dest`, optionally filtered by `pattern`).
pub fn get_files(
    cli: &mut Cli,
    stage: &StagePath,
    dest: &std::path::Path,
    parallel: i64,
    pattern: Option<&str>,
) -> Result<snow_connector::Cursor> {
    ensure_dir(dest)?;
    let mut q = format!(
        "get {} {} parallel={parallel}",
        stage.path_for_sql(),
        to_uri(&format!("{}/", dest.display()))
    );
    if let Some(p) = pattern {
        q.push_str(&format!(
            " pattern={}",
            snow_api::identifiers::to_string_literal(p)
        ));
    }
    let before = snapshot_dir(dest);
    let c = cli.execute_query(&q);
    restrict_new(dest, &before);
    c
}

fn snapshot_dir(d: &std::path::Path) -> std::collections::HashMap<std::ffi::OsString, (u128, u64)> {
    std::fs::read_dir(d)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let m = e.metadata().ok()?;
                    let t = m
                        .modified()
                        .ok()?
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_nanos();
                    Some((e.file_name(), (t, m.len())))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn restrict_new(
    d: &std::path::Path,
    before: &std::collections::HashMap<std::ffi::OsString, (u128, u64)>,
) {
    let after = snapshot_dir(d);
    for (name, sig) in after {
        let prev = before.get(&name);
        let p = d.join(&name);
        if prev.is_some() && p.is_dir() {
            continue;
        }
        if prev != Some(&sig) {
            snow_api::secure::restrict(&p);
        }
    }
}

/// Files under a stage path (`StageManager.iter_stage`).
pub fn iter_stage(cli: &mut Cli, root: &StagePath, git: bool) -> Result<Vec<StagePath>> {
    let c = cli.execute_query(&list_files_query(root, None))?;
    let idx = c.column_index("name").unwrap_or(0);
    let mut out = Vec::new();
    for r in &c.rows {
        let name = r.get(idx).cloned().flatten().unwrap_or_default();
        let p = if root.is_user_stage() {
            StagePath::user_stage().join(&name)
        } else if git || root.is_git_repo() {
            StagePath::from_git_str(&name)
        } else if root.is_vstage() {
            root.root_path().join(&name)
        } else {
            match name.split_once('/') {
                Some((_, rel)) if !rel.is_empty() => root.root_path().join(rel),
                _ => root.root_path(),
            }
        };
        out.push(p);
    }
    Ok(out)
}

fn rel_parts(p: &StagePath, root: &StagePath) -> Vec<String> {
    p.relative_to(root).unwrap_or_default()
}

/// `StagePath.get_local_target_path`.
fn local_target(p: &StagePath, target: &std::path::Path, root: &StagePath) -> std::path::PathBuf {
    let rel = rel_parts(p, root);
    if rel.is_empty() {
        return target.to_path_buf();
    }
    let mut out = target.to_path_buf();
    for part in &rel[..rel.len() - 1] {
        out.push(part);
    }
    out
}

/// The PUT/GET `parallel`, capped by `cli.stage_upload_workers` (default 32).
fn split_budget(cli: &Cli, parallel: i64) -> Result<i64> {
    let raw = cli
        .config
        .value(&["cli"], "stage_upload_workers")
        .map(|v| v.py_str());
    let budget = match raw {
        None => 32,
        Some(r) => match r.trim().parse::<i64>() {
            Ok(v) if v >= 1 => v,
            Ok(v) => {
                return Err(SnowError::cli(format!(
                    "Invalid SNOWFLAKE_CLI_STAGE_UPLOAD_WORKERS={v}: must be a positive integer (>= 1)."
                )));
            }
            Err(_) => {
                return Err(SnowError::cli(format!(
                    "Invalid SNOWFLAKE_CLI_STAGE_UPLOAD_WORKERS={}: expected a positive integer.",
                    snow_click::py_repr(&r)
                )));
            }
        },
    };
    Ok(budget.min(parallel.max(1)))
}

struct GetUnit {
    stage: StagePath,
    local: std::path::PathBuf,
    pattern: Option<String>,
}

/// `StageManager.get_recursive` + `_plan_recursive_gets`.
pub fn get_recursive(
    cli: &mut Cli,
    src: &str,
    dest: &std::path::Path,
    parallel: i64,
    git: bool,
) -> Result<Vec<snow_api::output::Row>> {
    let root = if git {
        StagePath::from_git_str(src)
    } else {
        StagePath::from_stage_str(src)
    };
    let files = iter_stage(cli, &root, git)?;
    let resolved = resolve(dest);
    let mut singles = Vec::new();
    let mut groups: indexmap::IndexMap<Vec<String>, Vec<StagePath>> = indexmap::IndexMap::new();
    let mut rels: Vec<Vec<String>> = Vec::new();
    for f in &files {
        let local = local_target(f, dest, &root);
        let rl = resolve(&local);
        if !rl.starts_with(&resolved) {
            return Err(SnowError::cli(format!(
                "Refusing to write outside the destination directory: the server returned a path that would resolve to '{}', which is outside '{}'. Aborting download.",
                rl.display(),
                resolved.display()
            )));
        }
        let rel = rel_parts(f, &root);
        if rel.is_empty() {
            singles.push(GetUnit {
                stage: f.clone(),
                local,
                pattern: None,
            });
            continue;
        }
        groups
            .entry(rel[..rel.len() - 1].to_vec())
            .or_default()
            .push(f.clone());
        rels.push(rel);
    }
    let mut units = singles;
    for (parent, members) in &groups {
        let local = local_target(&members[0], dest, &root);
        let mut per_file = false;
        let mut pattern = None;
        let root_names: std::collections::HashSet<&String> = rels
            .iter()
            .filter(|r| r.len() == 1)
            .map(|r| &r[0])
            .collect();
        let nested_names: std::collections::HashSet<&String> = rels
            .iter()
            .filter(|r| r.len() > 1)
            .map(|r| r.last().expect("name"))
            .collect();
        let has_nested = rels
            .iter()
            .any(|r| r.len() > parent.len() + 1 && r.starts_with(parent));
        if parent.is_empty() && root_names.intersection(&nested_names).next().is_some() {
            per_file = true;
        } else if has_nested {
            if parent.is_empty() {
                let mut names: Vec<String> = members
                    .iter()
                    .filter_map(|m| rel_parts(m, &root).last().cloned())
                    .collect();
                names.sort();
                names.dedup();
                pattern = Some(format!(
                    "(?:^|.*/)(?:{})$",
                    names
                        .iter()
                        .map(|n| py_re_escape(n))
                        .collect::<Vec<_>>()
                        .join("|")
                ));
            } else {
                let p = format!("(?:^|.*/){}/[^/]+$", py_re_escape(&parent.join("/")));
                let re = regex::Regex::new(&format!("^(?:{p})$")).expect("re");
                per_file = rels
                    .iter()
                    .any(|r| r[..r.len() - 1] != parent[..] && re.is_match(&r.join("/")));
                pattern = Some(p);
            }
        }
        if per_file {
            for m in members {
                units.push(GetUnit {
                    stage: m.clone(),
                    local: local.clone(),
                    pattern: None,
                });
            }
            continue;
        }
        units.push(GetUnit {
            stage: members[0].parent().with_trailing_slash(),
            local,
            pattern,
        });
    }
    units.sort_by_key(|u| u.stage.absolute_path());
    if units.is_empty() {
        return Ok(vec![]);
    }
    let parallel = split_budget(cli, parallel)?;
    for u in &units {
        ensure_dir(&u.local)?;
    }
    let mut rows = Vec::new();
    for u in &units {
        let c = get_files(cli, &u.stage, &u.local, parallel, u.pattern.as_deref())?;
        rows.extend(cli.rows(&c));
    }
    let file = |r: &snow_api::output::Row| r.get("file").map(|v| v.py_str()).unwrap_or_default();
    rows.sort_by(|a, b| dir_then_name(&file(a)).cmp(&dir_then_name(&file(b))));
    Ok(rows)
}

/// `re.escape` (Python 3.7+: escapes only the regex special characters).
fn py_re_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "()[]{}?*+-|^$\\.&~# \t\n\r\x0b\x0c".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn walk_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().starts_with('.'))
            .unwrap_or(false);
        if name {
            continue;
        }
        if p.is_dir() {
            out.push(p.clone());
            walk_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// `StageManager.put_recursive`: one PUT per local directory, rows re-labelled.
pub fn put_recursive(
    cli: &mut Cli,
    local: &std::path::Path,
    stage: &str,
    parallel: i64,
    overwrite: bool,
    auto_compress: bool,
) -> Result<Vec<snow_api::output::Row>> {
    let tmp = tempfile::tempdir()?;
    copy_to_tmp_dir(local, tmp.path())?;
    put_tree(cli, tmp.path(), stage, parallel, overwrite, auto_compress)
}

/// `StageManager.copy_to_tmp_dir`: mirror `local` (symlinks to its files) under `tmp`.
pub fn copy_to_tmp_dir(local: &std::path::Path, tmp: &std::path::Path) -> Result<()> {
    let (root, entries) = if local.is_dir() {
        let mut v = Vec::new();
        walk_files(local, &mut v);
        (local.to_path_buf(), v)
    } else {
        let root = local
            .ancestors()
            .skip(1)
            .find(|p| p.is_dir())
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        let pattern = local.to_string_lossy().into_owned();
        (root, snow_connector::transfer::glob_paths(&pattern))
    };
    for e in &entries {
        let rel = e.strip_prefix(&root).unwrap_or(e);
        let dst = tmp.join(rel);
        if e.is_file() {
            if let Some(p) = dst.parent() {
                std::fs::create_dir_all(p)?;
            }
            link_or_copy(e, &dst)?;
        } else {
            std::fs::create_dir_all(&dst)?;
        }
    }
    Ok(())
}

/// `put_recursive(..., temp_directory=dir)`: one PUT per directory of `dir`.
pub fn put_tree(
    cli: &mut Cli,
    dir: &std::path::Path,
    stage: &str,
    parallel: i64,
    overwrite: bool,
    auto_compress: bool,
) -> Result<Vec<snow_api::output::Row>> {
    let parallel = split_budget(cli, parallel)?;
    let base = StagePath::from_stage_str(stage);
    let iso = tempfile::tempdir()?;
    let mut units = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    let mut idx = 0;
    while let Some(d) = dirs.first().cloned() {
        dirs.remove(0);
        let mut files = Vec::new();
        let mut subdirs = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&d) {
            let mut es: Vec<_> = rd.filter_map(|e| e.ok()).collect();
            es.sort_by_key(|e| e.file_name());
            for e in es {
                let p = e.path();
                if p.is_dir() {
                    subdirs.push(p);
                } else if p.is_file() {
                    files.push(e.file_name());
                }
            }
        }
        let this = idx;
        idx += 1;
        dirs.splice(0..0, subdirs);
        if files.is_empty() {
            continue;
        }
        let rel = d.strip_prefix(dir).unwrap_or(&d).to_path_buf();
        let iso_dir = iso.path().join(this.to_string());
        std::fs::create_dir_all(&iso_dir)?;
        for f in &files {
            link_or_copy(&d.join(f), &iso_dir.join(f))?;
        }
        let rel_s = rel.to_string_lossy().into_owned();
        let destination = if rel_s.is_empty() {
            base.clone()
        } else {
            base.join(&rel_s)
        };
        units.push((iso_dir, destination, rel_s));
    }
    let mut rows = Vec::new();
    for (iso_dir, destination, rel) in units {
        let c = put(
            cli,
            &iso_dir.to_string_lossy(),
            &destination,
            parallel,
            overwrite,
            auto_compress,
        )?;
        for mut r in cli.rows(&c) {
            if let Some(s) = r.get("source").map(|v| v.py_str()) {
                let v = if rel.is_empty() {
                    s
                } else {
                    format!("{rel}/{s}")
                };
                r.insert("source".into(), snow_api::output::Pv::s(v));
            }
            if let Some(t) = r.get("target").map(|v| v.py_str()) {
                r.insert(
                    "target".into(),
                    snow_api::output::Pv::s(destination.join(&t).absolute_path()),
                );
            }
            rows.push(r);
        }
    }
    Ok(rows)
}

fn link_or_copy(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    let abs = if src.is_absolute() {
        src.to_path_buf()
    } else {
        std::env::current_dir()?.join(src)
    };
    #[cfg(unix)]
    if std::os::unix::fs::symlink(&abs, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(&abs, dst)?;
    Ok(())
}

/// `fnmatch.fnmatchcase` without `[...]` classes (a `[` is literal). Like
/// `fnmatch.translate`, `*` and `?` also match a newline.
pub fn fnmatchcase(name: &str, pat: &str) -> bool {
    let mut re = String::from("(?s)^");
    for c in pat.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    regex::Regex::new(&re).is_ok_and(|r| r.is_match(name))
}
