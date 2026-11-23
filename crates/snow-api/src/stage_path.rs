//! `snowflake.cli.api.stage_path.StagePath`.

use crate::identifiers::{Fqn, to_string_literal};
use regex::Regex;
use std::sync::LazyLock;

const SNOW_PREFIX: &str = "snow://";
static UNQUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_$]+$").expect("re"));
static SQL_SAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^@([\w./$])+$").expect("re"));

/// A `PurePosixPath` as a list of parts; empty means `.`.
pub fn posix_parts(p: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if p.starts_with('/') {
        out.push("/".into());
    }
    for part in p.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        out.push(part.to_string());
    }
    out
}

pub fn posix_str(parts: &[String]) -> String {
    if parts.is_empty() {
        return ".".into();
    }
    if parts[0] == "/" {
        return format!("/{}", parts[1..].join("/"));
    }
    parts.join("/")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagePath {
    stage_name: String,
    path: Vec<String>,
    git_ref: Option<String>,
    trailing_slash: bool,
    snow_prefixed: bool,
}

fn strip_prefixes(t: &str) -> String {
    let t = t.strip_prefix(SNOW_PREFIX).unwrap_or(t);
    t.strip_prefix('@').unwrap_or(t).to_string()
}

impl StagePath {
    pub fn new(
        stage_name: &str,
        path: Option<&str>,
        git_ref: Option<&str>,
        trailing_slash: bool,
        snow: Option<bool>,
    ) -> StagePath {
        StagePath {
            snow_prefixed: snow.unwrap_or_else(|| stage_name.starts_with(SNOW_PREFIX)),
            stage_name: strip_prefixes(stage_name),
            path: path.map(posix_parts).unwrap_or_default(),
            git_ref: git_ref.filter(|g| !g.is_empty()).map(str::to_string),
            trailing_slash,
        }
    }

    pub fn user_stage() -> StagePath {
        StagePath::from_stage_str("~")
    }

    pub fn from_stage_str(s: &str) -> StagePath {
        let at = s.starts_with('@');
        let snow = s.starts_with(SNOW_PREFIX);
        let mut rest = strip_prefixes(s);
        let mut resource = String::new();
        if snow {
            resource = rest.split('/').next().unwrap_or("").to_string();
            rest = rest
                .strip_prefix(&format!("{resource}/"))
                .unwrap_or(&rest)
                .to_string();
        }
        let parts: Vec<&str> = rest.splitn(2, '/').filter(|p| !p.is_empty()).collect();
        let (mut stage, path) = if parts.len() == 2 {
            (parts[0].to_string(), Some(parts[1]))
        } else {
            (parts.first().copied().unwrap_or("").to_string(), None)
        };
        if at && !stage.starts_with('@') {
            stage = format!("@{stage}");
        }
        if snow {
            stage = format!("{SNOW_PREFIX}{resource}/{stage}");
        }
        StagePath::new(&stage, path, None, rest.ends_with('/'), None)
    }

    pub fn from_git_str(s: &str) -> StagePath {
        let at = s.starts_with('@');
        let snow = s.starts_with(SNOW_PREFIX);
        let (mut repo, gref, path) = split_repo_path(&strip_prefixes(s));
        if at {
            repo = format!("@{}", repo.trim_start_matches('@'));
        }
        if snow {
            repo = format!("{SNOW_PREFIX}{repo}");
        }
        StagePath::new(&repo, Some(&path), Some(&gref), s.ends_with('/'), None)
    }

    pub fn stage(&self) -> &str {
        &self.stage_name
    }
    pub fn stage_fqn(&self) -> crate::Result<Fqn> {
        Fqn::from_stage(&self.stage_name)
    }
    pub fn path_str(&self) -> String {
        posix_str(&self.path)
    }
    pub fn parts(&self) -> &[String] {
        &self.path
    }
    pub fn name(&self) -> String {
        self.path.last().cloned().unwrap_or_default()
    }
    pub fn is_user_stage(&self) -> bool {
        self.stage_name.starts_with('~')
    }
    pub fn is_git_repo(&self) -> bool {
        self.git_ref.is_some()
    }
    pub fn is_vstage(&self) -> bool {
        self.snow_prefixed
    }
    pub fn is_root(&self) -> bool {
        self.path.is_empty()
    }
    pub fn git_ref(&self) -> Option<&str> {
        self.git_ref.as_deref()
    }
    pub fn trailing_slash(&self) -> bool {
        self.trailing_slash
    }

    pub fn absolute_path_opts(&self, no_fqn: bool, at_prefix: bool) -> String {
        let mut stage = self.stage_name.clone();
        if !self.is_user_stage()
            && no_fqn
            && let Ok(f) = Fqn::from_string(&stage)
        {
            stage = f.name;
        }
        let mut parts = posix_parts(&stage);
        if let Some(g) = &self.git_ref {
            parts.extend(posix_parts(g));
        }
        if !self.is_root() {
            parts.extend(self.path.iter().cloned());
        }
        let mut s = posix_str(&parts);
        if self.snow_prefixed {
            if !s.starts_with(SNOW_PREFIX) {
                s = format!("{SNOW_PREFIX}{s}");
            }
        } else if at_prefix && !s.starts_with('@') {
            s = format!("@{s}");
        }
        if self.trailing_slash {
            return format!("{}/", s.trim_end_matches('/'));
        }
        s
    }

    pub fn absolute_path(&self) -> String {
        self.absolute_path_opts(false, true)
    }

    pub fn path_for_sql(&self) -> String {
        let p = self.absolute_path();
        if SQL_SAFE.is_match(&p) {
            p
        } else {
            to_string_literal(&p)
        }
    }

    pub fn quoted_absolute_path(&self) -> String {
        let p = self.absolute_path();
        if crate::identifiers::is_valid_string_literal(&p) {
            p
        } else {
            to_string_literal(&p)
        }
    }

    pub fn join(&self, p: &str) -> StagePath {
        let mut path = self.path.clone();
        path.extend(posix_parts(p.trim_start_matches('/')));
        StagePath {
            stage_name: self.stage_name.clone(),
            path,
            git_ref: self.git_ref.clone(),
            trailing_slash: false,
            snow_prefixed: self.snow_prefixed,
        }
    }

    pub fn with_trailing_slash(&self) -> StagePath {
        StagePath {
            trailing_slash: true,
            ..self.clone()
        }
    }

    pub fn parent(&self) -> StagePath {
        let mut path = self.path.clone();
        path.pop();
        StagePath {
            path,
            trailing_slash: false,
            ..self.clone()
        }
    }

    pub fn root_path(&self) -> StagePath {
        StagePath {
            path: Vec::new(),
            trailing_slash: false,
            ..self.clone()
        }
    }

    pub fn is_dir(&self) -> bool {
        let p = self.path_str();
        let fs = std::path::Path::new(&p);
        if fs.exists() {
            return fs.is_dir();
        }
        !self.name().contains('.')
    }
    pub fn is_file(&self) -> bool {
        let p = self.path_str();
        let fs = std::path::Path::new(&p);
        if fs.exists() {
            return fs.is_file();
        }
        !self.is_dir()
    }

    /// `relative_to`: remaining parts, `None` when not a subpath.
    pub fn relative_to(&self, other: &StagePath) -> Option<Vec<String>> {
        if self.path.len() < other.path.len() {
            return None;
        }
        for (s, o) in self.path.iter().zip(&other.path) {
            let same = if UNQUOTED.is_match(s) && UNQUOTED.is_match(o) {
                s.to_lowercase() == o.to_lowercase()
            } else {
                s == o
            };
            if !same {
                return None;
            }
        }
        Some(self.path[other.path.len()..].to_vec())
    }
}

impl std::fmt::Display for StagePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.absolute_path())
    }
}

fn split_repo_path(s: &str) -> (String, String, String) {
    let chars: Vec<char> = s.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut slash = 0usize;
    let mut skipping = false;
    for i in 0..chars.len().saturating_sub(1) {
        let (c, next) = (chars[i], chars[i + 1]);
        if !skipping {
            if c != '/' {
                continue;
            }
            parts.push(chars[slash..i].iter().collect());
            slash = i + 1;
        }
        if next == '"' {
            skipping = !skipping;
        }
    }
    parts.push(chars[slash..].iter().collect());
    let repo = parts[0].clone();
    let gref = format!(
        "{}/{}",
        parts.get(1).cloned().unwrap_or_default(),
        parts.get(2).cloned().unwrap_or_default()
    );
    let path = if parts.len() > 2 {
        parts[3..].join("/")
    } else {
        String::new()
    };
    (repo, gref, path)
}
