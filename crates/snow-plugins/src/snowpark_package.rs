//! `snow snowpark package lookup|upload`.

use snow_api::identifiers::Fqn;
use snow_api::output::{Output, Pv};
use snow_api::pep440::Version;
use snow_api::{Cli, Result, SnowError};
use std::collections::BTreeSet;

/// `Requirement.standardize_name`: lower-case, runs of non-word characters to `_`.
pub fn standardize_name(name: &str) -> String {
    regex::Regex::new(r"[^\w\d.]+")
        .expect("re")
        .replace_all(&name.to_lowercase(), "_")
        .into_owned()
}

/// A parsed requirement specifier (`name[extras] op ver, ...`).
pub struct Requirement {
    /// The requirement text (without an environment marker).
    pub line: String,
    pub name: String,
    pub extras: Vec<String>,
    pub specs: Vec<(String, String)>,
}

impl Requirement {
    pub fn parse(line: &str) -> Result<Requirement> {
        let line = line.split(';').next().unwrap_or(line);
        let line = regex::Regex::new("#.*")
            .expect("re")
            .replace(line, "")
            .into_owned();
        let re = regex::Regex::new(
            r"^\s*([A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?)\s*(?:\[([^\]]*)\])?\s*(.*?)\s*$",
        )
        .expect("re");
        let Some(c) = re.captures(&line) else {
            return Err(SnowError::unexpected(format!(
                "Expected package name at the start of dependency specifier\n    {line}\n    ^"
            ))
            .named("InvalidRequirement"));
        };
        let extras = c
            .get(2)
            .map(|e| {
                e.as_str()
                    .split(',')
                    .map(|x| x.trim().to_lowercase())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let mut specs = Vec::new();
        let rest = c.get(3).map(|m| m.as_str()).unwrap_or("");
        let rest = rest.trim_start_matches('(').trim_end_matches(')');
        let spec_re =
            regex::Regex::new(r"^\s*(===|~=|==|!=|<=|>=|<|>)\s*([^\s,]+)\s*$").expect("re");
        if !rest.trim().is_empty() {
            for part in rest.split(',') {
                let Some(m) = spec_re.captures(part) else {
                    return Err(
                        SnowError::unexpected(format!("Invalid requirement: {line:?}"))
                            .named("InvalidRequirement"),
                    );
                };
                specs.push((m[1].to_string(), m[2].to_string()));
            }
        }
        Ok(Requirement {
            line: line.trim().to_string(),
            name: standardize_name(&c[1]),
            extras,
            specs,
        })
    }
}

/// `AnacondaPackages`: standardized name → (snowflake name, versions).
pub struct Anaconda {
    pub packages: indexmap::IndexMap<String, (String, BTreeSet<String>)>,
}

impl Anaconda {
    /// `AnacondaPackagesManager.find_packages_available_in_snowflake_anaconda()`.
    pub fn load(cli: &mut Cli) -> Result<Anaconda> {
        let c = cli.execute_query(
            "select package_name, version from snowflake.information_schema.packages where language = 'python'",
        )?;
        if c.rows.is_empty() {
            return Err(crate::nativeapp::package::sql_execution_error(None));
        }
        let rows = cli.rows(&c);
        let mut packages: indexmap::IndexMap<String, (String, BTreeSet<String>)> =
            indexmap::IndexMap::new();
        for r in rows {
            let name = r
                .get("PACKAGE_NAME")
                .map(Pv::py_str)
                .filter(|s| !s.is_empty() && s != "None");
            let version = r
                .get("VERSION")
                .map(Pv::py_str)
                .filter(|s| !s.is_empty() && s != "None");
            let (Some(name), Some(version)) = (name, version) else {
                continue;
            };
            packages
                .entry(standardize_name(&name))
                .or_insert_with(|| (name.clone(), BTreeSet::new()))
                .1
                .insert(version);
        }
        Ok(Anaconda { packages })
    }

    fn versions(&self, name: &str) -> Option<Vec<Version>> {
        let (_, vs) = self.packages.get(name)?;
        vs.iter().map(|v| Version::parse(v)).collect()
    }

    pub fn is_available(&self, r: &Requirement) -> bool {
        if r.name.is_empty() || !r.extras.is_empty() || !self.packages.contains_key(&r.name) {
            return false;
        }
        if r.specs.is_empty() {
            return true;
        }
        if r.specs.iter().any(|(op, _)| op == "!=") {
            return false;
        }
        let Some(versions) = self.versions(&r.name) else {
            return false;
        };
        let allow_pre = r.specs.iter().any(|(_, v)| {
            Version::parse(v.trim_end_matches(".*")).is_some_and(|v| v.is_prerelease())
        });
        versions.iter().any(|v| {
            (allow_pre || !v.is_prerelease()) && r.specs.iter().all(|(op, s)| v.matches(op, s))
        })
    }

    pub fn latest_version(&self, r: &Requirement) -> Option<String> {
        self.versions(&r.name)?
            .into_iter()
            .max()
            .map(|v| v.to_string())
    }

    pub fn package_versions(&self, r: &Requirement) -> Vec<String> {
        let Some((_, raw)) = self.packages.get(&r.name) else {
            return vec![];
        };
        match self.versions(&r.name) {
            Some(mut v) => {
                v.sort();
                v.into_iter().rev().map(|x| x.to_string()).collect()
            }
            None => raw.iter().rev().cloned().collect(),
        }
    }
}

pub fn lookup(cli: &mut Cli) -> Result<Output> {
    let name = cli.str("package_name").unwrap_or_default();
    let anaconda = Anaconda::load(cli)?;
    let r = Requirement::parse(&name)?;
    if anaconda.is_available(&r) {
        let mut msg = format!("Package `{name}` is available in Anaconda");
        if let Some(v) = anaconda.latest_version(&r) {
            msg.push_str(&format!(". Latest available version: {v}."));
        } else {
            let vs = anaconda.package_versions(&r);
            if !vs.is_empty() {
                msg.push_str(&format!(" in versions: {}.", vs.join(", ")));
            }
        }
        return Ok(Output::message(msg));
    }
    Ok(Output::message(format!(
        "\nPackage `{name}` is not available in Anaconda. To prepare Snowpark compatible package run:\nsnow snowpark package create {name}\n"
    )))
}

pub fn upload(cli: &mut Cli) -> Result<Output> {
    let file = cli.str("file").unwrap_or_default();
    let stage = cli.str("stage").unwrap_or_default();
    let overwrite = cli.flag("overwrite");
    crate::logs::info(
        "snowflake.cli._plugins.snowpark.package.manager",
        &format!("Uploading {file} to Snowflake @{stage}/{file}..."),
    );
    let tmp = tempfile::tempdir()?;
    let src = std::path::Path::new(&file);
    let name = src
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    let dst = tmp.path().join(&name);
    if let Err(e) = std::fs::copy(src, &dst) {
        let abs = std::env::current_dir().unwrap_or_default().join(src);
        let abs = snow_api::project::model::pure_path(&abs.to_string_lossy());
        let _ = e;
        return Err(SnowError::unexpected(format!(
            "[Errno 2] No such file or directory: PosixPath({})",
            snow_click::py_repr(&abs)
        ))
        .named("FileNotFoundError"));
    }
    let stage_name = snow_api::stage_path::posix_parts(&stage)
        .first()
        .cloned()
        .unwrap_or_default();
    let fqn = Fqn::from_string(&stage_name)?;
    cli.execute_query(&crate::stage::create_query(&fqn, false, None, false, None))?;
    let c = crate::stage::put(
        cli,
        &dst.to_string_lossy(),
        &snow_api::stage_path::StagePath::from_stage_str(&stage),
        4,
        overwrite,
        false,
    )?;
    let status = c
        .rows
        .first()
        .and_then(|r| r.get(6).cloned().flatten())
        .unwrap_or_else(|| "None".into());
    if status == "SKIPPED" {
        return Ok(Output::message(
            "Package already exists on stage. Consider using --overwrite to overwrite the file.",
        ));
    }
    Ok(Output::message(format!(
        "Package {file} {status} to Snowflake @{stage}/{file}."
    )))
}
