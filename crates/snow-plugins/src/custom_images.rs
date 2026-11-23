//! `snow custom-image validate`: check a local Docker image against the bundled rules.

use serde_json::Value;
use snow_api::output::Output;
use snow_api::{Cli, Result, SnowError};
use std::process::Command;

const CONFIG: &str = include_str!("../data/image_validation.yaml");

struct Check {
    name: String,
    passed: bool,
    message: String,
}

fn check(name: impl Into<String>, passed: bool, message: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        passed,
        message: message.into(),
    }
}

fn run(cmd: &[&str], missing: &str) -> Result<(i32, String, String)> {
    let out = Command::new(cmd[0])
        .args(&cmd[1..])
        .output()
        .map_err(|_| SnowError::click(missing))?;
    let s = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    Ok((
        out.status.code().unwrap_or(-1),
        s(&out.stdout),
        s(&out.stderr),
    ))
}

fn docker(args: &[&str]) -> Result<(i32, String, String)> {
    let mut cmd = vec!["docker"];
    cmd.extend_from_slice(args);
    run(&cmd, "Docker is not installed.")
}

fn py(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn grype_table(stdout: &str) -> Option<String> {
    let doc: Value = serde_json::from_str(stdout).ok()?;
    let matches = doc.as_object()?.get("matches")?.as_array()?;
    let mut rows = Vec::new();
    for m in matches {
        let a = m.get("artifact").filter(|v| v.is_object());
        let v = m.get("vulnerability").filter(|v| v.is_object());
        let mut sev: Vec<String> = Vec::new();
        let mut push = |s: Option<&Value>| {
            if let Some(s) = s.filter(|s| {
                !matches!(s, Value::Null | Value::Bool(false)) && s.as_str() != Some("")
            }) {
                sev.push(py(Some(s)).trim().to_lowercase());
            }
        };
        push(v.and_then(|v| v.get("severity")));
        for rv in m
            .get("relatedVulnerabilities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if rv.is_object() {
                push(rv.get("severity"));
            }
        }
        let critical = sev.iter().any(|s| s == "critical");
        if !critical && !sev.iter().any(|s| s == "high") {
            continue;
        }
        let field = |o: Option<&Value>, k: &str| {
            o.and_then(|o| o.get(k))
                .map(|x| py(Some(x)))
                .unwrap_or_else(|| "?".into())
        };
        rows.push(format!(
            "{} | {} | {} | {} | {}",
            field(a, "name"),
            field(a, "version"),
            field(a, "type"),
            field(v, "id"),
            if critical { "Critical" } else { "High" }
        ));
    }
    if rows.is_empty() {
        return None;
    }
    let n = rows.len();
    let mut lines = vec![
        "High/Critical vulnerabilities (--fail-on high):".to_string(),
        String::new(),
        "package | version | type | cve | severity".into(),
    ];
    lines.extend(rows);
    lines.push(format!(
        "\n{n} finding(s). Run `grype <image> -o json` for full details."
    ));
    Some(lines.join("\n"))
}

fn format_report(image: &str, results: &[Check], readiness: Option<&[&str]>) -> String {
    let eq = "=".repeat(60);
    let mut lines = vec![
        format!("\n{eq}"),
        format!("Image Validation Report: {image}"),
        format!("{eq}\n"),
    ];
    for r in results {
        lines.push(format!(
            "[{}] {}: {}",
            if r.passed { "PASS" } else { "FAIL" },
            r.name,
            r.message
        ));
    }
    let passed = results.iter().filter(|r| r.passed).count();
    lines.push(format!("\n{}", "-".repeat(60)));
    lines.push(format!(
        "Summary: {passed} passed, {} failed",
        results.len() - passed
    ));
    match readiness {
        Some(r) if !r.is_empty() => lines.push(format!("Ready for: {}", r.join(", "))),
        Some(_) => lines.push("Ready for: None".into()),
        None => {}
    }
    lines.push(
        if passed == results.len() {
            "Status: ALL CHECKS PASSED"
        } else {
            "Status: VALIDATION FAILED"
        }
        .into(),
    );
    lines.push(format!("{eq}\n"));
    lines.join("\n")
}

fn validate_image(image: &str, scan: bool) -> Result<(Vec<Check>, String)> {
    let config: Value =
        serde_yaml::from_str(CONFIG).map_err(|e| SnowError::unexpected(e.to_string()))?;
    let checks = &config["checks"];
    let mut results = Vec::new();
    let info = match docker(&["inspect", image])? {
        (0, out, _) => serde_json::from_str::<Value>(&out)
            .ok()
            .and_then(|v| v.get(0).cloned()),
        _ => None,
    };
    let Some(info) = info else {
        results.push(check(
            "image_exists",
            false,
            format!("Image '{image}' not found. Please ensure the image exists locally."),
        ));
        let report = format_report(image, &results, None);
        return Ok((results, report));
    };
    results.push(check(
        "image_exists",
        true,
        format!("Image '{image}' found"),
    ));
    let cfg = &info["Config"];
    if let Some(expected) = checks["entrypoint"].as_str() {
        let actual = match &cfg["Entrypoint"] {
            Value::Array(a) => a.first().cloned(),
            v => Some(v.clone()),
        };
        let r = if actual.as_ref().and_then(Value::as_str) != Some(expected) {
            check(
                "entrypoint",
                false,
                format!(
                    "Entrypoint mismatch. Expected: {expected}, Actual: {}",
                    py(actual.as_ref())
                ),
            )
        } else if docker(&[
            "run",
            "--rm",
            "--platform",
            "linux/amd64",
            "--entrypoint",
            "",
            image,
            "test",
            "-f",
            expected,
        ])?
        .0 != 0
        {
            check(
                "entrypoint",
                false,
                format!("Entrypoint file '{expected}' does not exist in the image"),
            )
        } else {
            check(
                "entrypoint",
                true,
                format!("Entrypoint is correctly set to '{expected}'"),
            )
        };
        let failed = !r.passed;
        results.push(r);
        if failed {
            let report = format_report(image, &results, None);
            return Ok((results, report));
        }
    }
    if let Some(vars) = checks["environment_variables"].as_array() {
        let env: indexmap::IndexMap<&str, &str> = cfg["Env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|e| e.split_once('=').unwrap_or((e, "")))
            .collect();
        for spec in vars {
            let Some(name) = spec["name"].as_str().filter(|n| !n.is_empty()) else {
                continue;
            };
            let expected = spec
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let id = format!("env_{name}");
            results.push(match env.get(name) {
                None => check(id, false, format!("Environment variable '{name}' not found. Expected: {name}={}", py(spec.get("value")))),
                Some(actual) if !expected.is_empty() && *actual != expected => check(
                    id,
                    false,
                    format!("Environment variable '{name}' has wrong value. Expected: {expected}, Actual: {actual}"),
                ),
                Some(_) => check(id, true, format!("Environment variable '{name}' is correctly set")),
            });
        }
    }
    if let Some(packages) = checks["python_packages"].as_array() {
        let norm = |s: &str| s.to_lowercase().replace('_', "-");
        let (code, out, err) = docker(&[
            "run",
            "--rm",
            "--platform",
            "linux/amd64",
            image,
            "bash",
            "-c",
            "pip list --format json",
        ])?;
        if code != 0 {
            results.push(check(
                "python_packages",
                false,
                format!(
                    "Failed to list Python packages. pip may not be installed or accessible: {err}"
                ),
            ));
        } else if let Ok(Value::Array(installed)) = serde_json::from_str::<Value>(&out) {
            let set: std::collections::HashSet<String> =
                installed.iter().map(|p| norm(&py(p.get("name")))).collect();
            for pkg in packages.iter().filter_map(Value::as_str) {
                results.push(if set.contains(&norm(pkg)) {
                    check(
                        format!("pkg_{pkg}"),
                        true,
                        format!("Package '{pkg}' is installed"),
                    )
                } else {
                    check(
                        format!("pkg_{pkg}"),
                        false,
                        format!("Package '{pkg}' is not installed"),
                    )
                });
            }
        } else {
            results.push(check(
                "python_packages",
                false,
                format!("Failed to parse pip output: {out}"),
            ));
        }
    }
    let health = &checks["dependency_health"];
    if !matches!(health, Value::Null | Value::Bool(false)) {
        let enabled = health
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if health.is_object() && !enabled {
            results.push(check(
                "dependency_health",
                true,
                "Dependency health check skipped (disabled)",
            ));
        } else {
            let ignore: Vec<String> = health
                .get("ignore_patterns")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_lowercase)
                .collect();
            let (code, out, err) = docker(&[
                "run",
                "--rm",
                "--platform",
                "linux/amd64",
                image,
                "bash",
                "-c",
                "pip check",
            ])?;
            let ok = check("dependency_health", true, "No broken dependencies found");
            if code == 0 {
                results.push(ok);
            } else {
                let mut output = if out.is_empty() { err } else { out };
                let mut clean = false;
                if !ignore.is_empty() {
                    let kept: Vec<&str> = output
                        .split('\n')
                        .filter(|l| !ignore.iter().any(|p| l.to_lowercase().contains(p)))
                        .collect();
                    clean = kept.iter().all(|l| l.trim().is_empty());
                    output = kept.join("\n");
                }
                results.push(if clean {
                    ok
                } else {
                    check(
                        "dependency_health",
                        false,
                        format!("Broken dependencies detected:\n{output}"),
                    )
                });
            }
        }
    }
    let mut notebook_ok = true;
    for script in config["notebook_checks"]["required_scripts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let code = docker(&[
            "run",
            "--rm",
            "--platform",
            "linux/amd64",
            "--entrypoint",
            "",
            image,
            "test",
            "-x",
            script,
        ])?
        .0;
        let id = format!("script_{script}");
        results.push(if code != 0 {
            notebook_ok = false;
            check(
                id,
                false,
                format!("Script '{script}' is missing or not executable"),
            )
        } else {
            check(
                id,
                true,
                format!("Script '{script}' exists and is executable"),
            )
        });
    }
    if scan {
        let (code, out, err) = run(
            &["grype", image, "-o", "json", "--fail-on", "high"],
            "Grype is required for vulnerability scanning. Please install it from https://github.com/anchore/grype",
        )?;
        results.push(if code == 0 {
            check(
                "vulnerability_scan",
                true,
                "No high/critical vulnerabilities found",
            )
        } else {
            let msg = grype_table(&out).unwrap_or_else(|| {
                let raw = if !err.is_empty() { err } else { out };
                format!(
                    "Grype scan failed.\n{}",
                    raw.chars().take(1500).collect::<String>()
                )
            });
            check("vulnerability_scan", false, msg)
        });
    }
    let common = results
        .iter()
        .filter(|r| !r.name.starts_with("script_"))
        .all(|r| r.passed);
    let mut ready = Vec::new();
    if common {
        ready.push("ML Jobs");
        if notebook_ok {
            ready.push("Notebooks");
        }
    }
    let report = format_report(image, &results, Some(&ready));
    Ok((results, report))
}

pub fn validate(cli: &mut Cli) -> Result<Output> {
    let image = cli.str("image").unwrap_or_default();
    let (results, report) = validate_image(&image, cli.flag("scan_vulnerabilities"))?;
    let failed = results.iter().filter(|r| !r.passed).count();
    if failed > 0 {
        return Err(SnowError::click(format!(
            "Image validation failed with {failed} error(s).\n{report}"
        )));
    }
    Ok(Output::message(report))
}
