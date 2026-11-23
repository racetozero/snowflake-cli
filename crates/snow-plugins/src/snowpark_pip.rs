//! Dependencies that are not in the Snowflake Anaconda channel: `python -m pip wheel`,
//! wheel metadata and extraction (`_plugins/snowpark/package_utils.py`, `models.py`).

use crate::snowpark_package::{Anaconda, Requirement, standardize_name};
use snow_api::{Cli, Result, SnowError};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The entries of a ZIP archive: (name, contents), from its central directory.
pub fn read_zip(path: &Path) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let b = std::fs::read(path)?;
    let bad = || std::io::Error::other("bad zip");
    let eocd = (0..b.len().saturating_sub(21))
        .rev()
        .find(|&i| b[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
        .ok_or_else(bad)?;
    let u16at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as usize;
    let u32at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
    let count = u16at(eocd + 10);
    let mut p = u32at(eocd + 16);
    let mut out = Vec::new();
    for _ in 0..count {
        if b.get(p..p + 4) != Some(&[0x50, 0x4b, 0x01, 0x02]) {
            return Err(bad());
        }
        let method = u16at(p + 10);
        let csize = u32at(p + 20);
        let (nlen, xlen, clen) = (u16at(p + 28), u16at(p + 30), u16at(p + 32));
        let local = u32at(p + 42);
        let name = String::from_utf8_lossy(&b[p + 46..p + 46 + nlen]).into_owned();
        p += 46 + nlen + xlen + clen;
        let data_at = local + 30 + u16at(local + 26) + u16at(local + 28);
        let raw = b.get(data_at..data_at + csize).ok_or_else(bad)?;
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut v = Vec::new();
                flate2::read::DeflateDecoder::new(raw).read_to_end(&mut v)?;
                v
            }
            _ => return Err(std::io::Error::other("unsupported compression")),
        };
        out.push((name, data));
    }
    Ok(out)
}

/// `WheelMetadata.from_wheel`: (name, dependencies) or `None` without exactly one METADATA.
fn wheel_metadata(path: &Path) -> Option<(String, Vec<String>)> {
    let entries = read_zip(path).ok()?;
    let metas: Vec<&(String, Vec<u8>)> = entries
        .iter()
        .filter(|(n, _)| n.ends_with(".dist-info/METADATA"))
        .collect();
    if metas.len() != 1 {
        return None;
    }
    let text = String::from_utf8_lossy(&metas[0].1).into_owned();
    let deps = text
        .lines()
        .filter_map(|l| l.strip_prefix("Requires-Dist:"))
        .map(|d| d.trim().to_string())
        .collect();
    let name = path
        .file_name()?
        .to_string_lossy()
        .split('-')
        .next()?
        .to_lowercase();
    Some((name, deps))
}

/// `pip_wheel(...)`: run `python -m pip wheel -w <dir> ...` and return its exit code.
pub fn pip_wheel(
    cli: &mut Cli,
    requirements_file: Option<&Path>,
    package: Option<&str>,
    dir: &Path,
    index_url: Option<&str>,
    deps: bool,
    raise: bool,
) -> Result<i32> {
    let resolved = index_url.map(str::to_string).or_else(|| {
        std::env::var("PIP_INDEX_URL")
            .ok()
            .filter(|s| !s.is_empty())
    });
    if let Some(r) = resolved.as_deref().filter(|r| *r != "--no-index")
        && r.to_lowercase().starts_with("http://")
    {
        return Err(SnowError::cli(
            "Plaintext http:// package indexes are not allowed. Use https://.",
        ));
    }
    let air = cli.config.bool_value(&["cli"], "air_gapped", Some(false))? == Some(true);
    if air && resolved.is_none() {
        return Err(SnowError::cli(
            "No package index is configured. Pass --index-url or set PIP_INDEX_URL to an in-region https:// index.",
        ));
    }
    let mut args: Vec<String> = vec![
        "-m".into(),
        "pip".into(),
        "wheel".into(),
        "-w".into(),
        dir.to_string_lossy().into_owned(),
    ];
    if let Some(p) = package {
        args.push(p.to_string());
    }
    if let Some(r) = requirements_file {
        args.push("-r".into());
        args.push(r.to_string_lossy().into_owned());
    }
    if air {
        args.push("--isolated".into());
        match resolved.as_deref() {
            Some(u) if u != "--no-index" => {
                args.push("-i".into());
                args.push(u.to_string());
            }
            Some(_) => args.push("--no-index".into()),
            None if std::env::var("PIP_NO_INDEX").is_ok_and(|v| !v.is_empty()) => {
                args.push("--no-index".into())
            }
            None => {}
        }
    } else if let Some(u) = resolved.as_deref().filter(|u| *u != "--no-index") {
        args.push("-i".into());
        args.push(u.to_string());
    }
    if !deps {
        args.push("--no-deps".into());
    }
    crate::logs::info(
        "snowflake.cli._plugins.snowpark.package_utils",
        &format!("Running pip wheel with command: {}", args.join(" ")),
    );
    let out = std::process::Command::new("python")
        .args(&args)
        .output()
        .map_err(|_| {
            SnowError::unexpected("[Errno 2] No such file or directory: 'python'")
                .named("FileNotFoundError")
        })?;
    let code = out.status.code().unwrap_or(1);
    if code != 0 {
        crate::logs::info(
            "snowflake.cli._plugins.snowpark.package_utils",
            &format!(
                "pip wheel finished with error code {code}. Details: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        );
        if raise {
            return Err(SnowError::cli(format!(
                "pip wheel finished with error code {code}. Please re-run with --verbose or --debug for more details."
            )));
        }
    } else {
        crate::logs::info(
            "snowflake.cli._plugins.snowpark.package_utils",
            "pip wheel command executed successfully",
        );
    }
    Ok(code)
}

/// The outcome of `download_unavailable_packages`.
pub struct Download {
    /// Requirements served by the Anaconda channel (`line`, name, specs).
    pub anaconda: Vec<Requirement>,
    /// Downloaded packages and the files inside each wheel.
    pub downloaded: Vec<(String, Vec<String>)>,
}

fn line_of(r: &Requirement) -> String {
    r.line.clone()
}

/// `download_unavailable_packages(requirements, target_dir, anaconda, skip_version_check, index_url)`.
pub fn download_unavailable(
    cli: &mut Cli,
    requirements: Vec<Requirement>,
    target: &Path,
    anaconda: &Anaconda,
    skip_version_check: bool,
    index_url: Option<&str>,
) -> Result<Download> {
    let available = |r: &Requirement| {
        if skip_version_check {
            !r.name.is_empty() && r.extras.is_empty() && anaconda.packages.contains_key(&r.name)
        } else {
            anaconda.is_available(r)
        }
    };
    let mut in_sf = Vec::new();
    let mut missing = Vec::new();
    for r in requirements {
        if available(&r) {
            in_sf.push(r);
        } else {
            crate::logs::info(
                "snowflake.cli._plugins.snowpark.package.anaconda_packages",
                &format!(
                    "'{}' not found in Snowflake Anaconda channel (or ignored)...",
                    r.name
                ),
            );
            missing.push(r);
        }
    }
    if missing.is_empty() {
        return Ok(Download {
            anaconda: in_sf,
            downloaded: vec![],
        });
    }
    let downloads = tempfile::tempdir()?;
    let req_file = downloads.path().join("requirements.txt");
    crate::logs::info(
        "snowflake.cli._plugins.snowpark.package_utils",
        &format!("Writing {} file", req_file.display()),
    );
    std::fs::write(
        &req_file,
        missing
            .iter()
            .map(|r| format!("{}\n", line_of(r)))
            .collect::<String>(),
    )?;
    pip_wheel(
        cli,
        Some(&req_file),
        None,
        downloads.path(),
        index_url,
        true,
        true,
    )?;
    // `split_downloaded_dependencies`.
    let mut wheels: Vec<PathBuf> = std::fs::read_dir(downloads.path())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "whl"))
        .collect();
    wheels.sort();
    let mut metas: indexmap::IndexMap<String, (PathBuf, Vec<String>)> = indexmap::IndexMap::new();
    for w in wheels {
        if let Some((n, deps)) = wheel_metadata(&w) {
            metas.entry(n).or_insert((w, deps));
        }
    }
    let mut sf_deps: indexmap::IndexMap<String, Requirement> = indexmap::IndexMap::new();
    let mut unavailable: indexmap::IndexMap<String, Option<PathBuf>> = indexmap::IndexMap::new();
    fn walk(
        r: Requirement,
        available: &dyn Fn(&Requirement) -> bool,
        metas: &indexmap::IndexMap<String, (PathBuf, Vec<String>)>,
        sf: &mut indexmap::IndexMap<String, Requirement>,
        un: &mut indexmap::IndexMap<String, Option<PathBuf>>,
    ) {
        if sf.contains_key(&r.name) || un.contains_key(&r.name) {
            return;
        }
        if available(&r) {
            sf.insert(r.name.clone(), r);
            return;
        }
        let key = regex::Regex::new(r"[^\w\d.]+")
            .expect("re")
            .replace_all(&r.name, "_")
            .into_owned();
        let meta = metas.get(&key);
        un.insert(r.name.clone(), meta.map(|m| m.0.clone()));
        for d in meta.map(|m| m.1.clone()).unwrap_or_default() {
            if let Ok(req) = Requirement::parse(&d) {
                walk(req, available, metas, sf, un);
            }
        }
    }
    let text = std::fs::read_to_string(&req_file)?;
    for l in text.lines() {
        if let Ok(r) = Requirement::parse(l) {
            walk(r, &available, &metas, &mut sf_deps, &mut unavailable);
        }
    }
    let names: Vec<&String> = unavailable.keys().collect();
    crate::logs::info(
        "snowflake.cli._plugins.snowpark.package_utils",
        &format!(
            "Downloaded packages: {}",
            names
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    std::fs::create_dir_all(target)?;
    let mut downloaded = Vec::new();
    for (name, wheel) in &unavailable {
        let mut files = Vec::new();
        if let Some(w) = wheel {
            for (n, data) in read_zip(w)? {
                files.push(n.clone());
                let dest = target.join(&n);
                if n.ends_with('/') {
                    std::fs::create_dir_all(&dest)?;
                } else {
                    if let Some(p) = dest.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    std::fs::write(&dest, data)?;
                }
            }
        }
        downloaded.push((name.clone(), files));
    }
    in_sf.extend(sf_deps.into_values());
    Ok(Download {
        anaconda: in_sf,
        downloaded,
    })
}

/// `detect_and_log_shared_libraries(details)`.
pub fn shared_libraries(downloaded: &[(String, Vec<String>)]) -> bool {
    let libs: Vec<&String> = downloaded
        .iter()
        .filter(|(_, f)| f.iter().any(|x| x.ends_with(".so") || x.ends_with(".dll")))
        .map(|(n, _)| n)
        .collect();
    let src = "snowflake.cli._plugins.snowpark.package_utils";
    if libs.is_empty() {
        crate::logs::info(
            src,
            "Unsupported native libraries not found in packages (Good news!)...",
        );
        return false;
    }
    crate::logs::log(
        crate::logs::ERROR,
        src,
        "Following dependencies utilise shared libraries, not supported by Conda:",
    );
    crate::logs::log(
        crate::logs::ERROR,
        src,
        &libs
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    crate::logs::log(
        crate::logs::ERROR,
        src,
        "You may still try to create your package with --allow-shared-libraries, but the might not work.",
    );
    crate::logs::log(
        crate::logs::ERROR,
        src,
        "You may also request adding the package to Snowflake Conda channel",
    );
    crate::logs::log(crate::logs::ERROR, src, "at https://support.anaconda.com/");
    true
}

/// `get_package_name_from_pip_wheel(package, index_url)`.
pub fn package_name_from_wheel(
    cli: &mut Cli,
    package: &str,
    index_url: Option<&str>,
) -> Result<String> {
    let tmp = tempfile::tempdir()?;
    let code = pip_wheel(
        cli,
        None,
        Some(package),
        tmp.path(),
        index_url,
        false,
        false,
    )?;
    let files: Vec<PathBuf> = std::fs::read_dir(tmp.path())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".whl"))
        .collect();
    if code != 0 || files.len() != 1 {
        if package.contains('/') || package.contains('\\') {
            return Ok(Requirement::parse(package)
                .map(|r| r.name)
                .unwrap_or_else(|_| package.to_string()));
        }
        return Ok(package.to_string());
    }
    Ok(wheel_metadata(&files[0])
        .map(|m| m.0)
        .unwrap_or_else(|| standardize_name(package)))
}

pub fn create(cli: &mut Cli) -> Result<snow_api::output::Output> {
    let name = cli.str("name").unwrap_or_default();
    let (ignore, index_url, skip_version, allow_shared) = (
        cli.flag("ignore_anaconda"),
        cli.str("index_url"),
        cli.flag("skip_version_check"),
        cli.flag("allow_shared_libraries"),
    );
    let tmp = tempfile::tempdir()?;
    let package = Requirement::parse(&name)?;
    let anaconda = if ignore {
        Anaconda {
            packages: Default::default(),
        }
    } else {
        Anaconda::load(cli)?
    };
    let line = package.line.clone();
    let d = download_unavailable(
        cli,
        vec![package],
        tmp.path(),
        &anaconda,
        skip_version,
        index_url.as_deref(),
    )?;
    if d.anaconda.iter().any(|p| p.line == line) {
        return Ok(snow_api::output::Output::message(format!(
            "Package {name} is already available in Snowflake Anaconda Channel."
        )));
    }
    crate::logs::info(
        "snowflake.cli._plugins.snowpark.package.commands",
        "Checking to see if packages have shared (.so/.dll) libraries...",
    );
    if shared_libraries(&d.downloaded) && !allow_shared {
        return Err(SnowError::click(
            "Some packages contain shared (.so/.dll) libraries. Try again with --allow-shared-libraries.",
        ));
    }
    let zip = format!(
        "{}.zip",
        package_name_from_wheel(cli, &name, index_url.as_deref())?
    );
    crate::zipper::zip_dir(tmp.path(), Path::new(&zip))?;
    let mut msg = format!(
        "\nPackage {zip} created. You can now upload it to a stage using\nsnow snowpark package upload -f {zip} -s <stage-name>`\nand reference it in your procedure or function.\nRemember to add it to imports in the procedure or function definition.\n"
    );
    if !d.anaconda.is_empty() {
        msg.push_str(&format!(
            "\nThe package {name} is successfully created, but depends on the following\nAnaconda libraries. They need to be included in project requirements,\nas they are not included in the .zip.\n"
        ));
        msg.push_str(
            &d.anaconda
                .iter()
                .map(|r| r.line.clone())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    Ok(snow_api::output::Output::message(msg))
}
