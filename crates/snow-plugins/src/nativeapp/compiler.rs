//! `NativeAppCompiler` (`_plugins/nativeapp/codegen/compiler.py`) and the
//! templates processor.

use super::package::Package;
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};
use std::path::Path;

/// The processors an artifact declares, as lower-cased names.
fn processors(artifact: &PyV) -> Vec<String> {
    artifact
        .get("processors")
        .and_then(PyV::as_list)
        .map(|l| {
            l.iter()
                .map(|p| match p {
                    PyV::Str(s) => s.clone(),
                    other => other.str_of("name").unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `ProcessorMapping` named `name` on the artifact (a bare string has no properties).
fn processor_of(artifact: &PyV, name: &str) -> PyV {
    artifact
        .get("processors")
        .and_then(PyV::as_list)
        .and_then(|l| {
            l.iter()
                .find(|p| {
                    !matches!(p, PyV::Str(_))
                        && p.str_of("name")
                            .is_some_and(|n| n.eq_ignore_ascii_case(name))
                })
                .cloned()
        })
        .unwrap_or(PyV::None)
}

fn is_enabled(cli: &Cli, name: &str) -> bool {
    match name.to_lowercase().as_str() {
        "native app setup" => {
            cli.config
                .feature("ENABLE_NATIVE_APP_PYTHON_SETUP", Some(false))
                == Some(true)
        }
        _ => true,
    }
}

/// `compile_artifacts()`.
pub fn compile(cli: &mut Cli, pkg: &Package, artifacts: &[PyV]) -> Result<()> {
    let any = artifacts
        .iter()
        .any(|a| processors(a).iter().any(|p| is_enabled(cli, p)));
    if !any {
        return Ok(());
    }
    cli.console.phase_start("Invoking artifact processors");
    let r = (|| -> Result<()> {
        let generated = pkg.generated_root();
        if generated.exists() {
            return Err(SnowError::click(format!(
                "Path {} already exists. Please choose a different name for your generated directory in the project definition file.",
                generated.display()
            )));
        }
        for a in artifacts {
            for p in processors(a) {
                if !is_enabled(cli, &p) {
                    continue;
                }
                match p.to_lowercase().as_str() {
                    "templates" => templates_process(cli, pkg, a)?,
                    "snowpark" => {
                        let proc = processor_of(a, &p);
                        super::snowpark_processor::process(cli, pkg, a, &proc)?
                    }
                    "native app setup" => {
                        return Err(SnowError::click(format!(
                            "The '{p}' artifact processor needs a Python runtime (it builds a virtual environment with the snowflake-app-python library), which this standalone snow does not support. Remove it from the artifact's processors, or use the Python Snowflake CLI for this project."
                        ))
                        .named("UnsupportedArtifactProcessorError"));
                    }
                    _ => {
                        return Err(SnowError::click(format!(
                            "Unsupported value {p} detected for an artifact processor. Please refer to documentation for a list of supported types."
                        ))
                        .named("UnsupportedArtifactProcessorError"));
                    }
                }
            }
        }
        Ok(())
    })();
    cli.console.phase_end(None);
    r
}

/// `TemplatesProcessor.process(artifact)`.
fn templates_process(cli: &mut Cli, pkg: &Package, artifact: &PyV) -> Result<()> {
    let mut map = crate::artifacts::BundleMap::new(&pkg.project_root, &pkg.deploy_root());
    let (src, dest, ignore) = crate::artifacts::mapping_of(artifact);
    map.add(&src, dest.as_deref(), &ignore)?;
    for (s, d) in map.all_mappings_expanded() {
        expand_templates_in_file(cli, pkg, &s, &d)?;
    }
    Ok(())
}

fn expand_templates_in_file(cli: &mut Cli, pkg: &Package, src: &Path, dest: &Path) -> Result<()> {
    if src.is_dir() {
        return Ok(());
    }
    let root = crate::artifacts::resolve_without_follow(&pkg.project_root);
    let rel = src
        .strip_prefix(&root)
        .unwrap_or(src)
        .to_string_lossy()
        .into_owned();
    let bytes = std::fs::read(dest)?;
    let contents = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            let b = e.as_bytes()[e.utf8_error().valid_up_to()];
            let reason = if (0x80..0xc0).contains(&b) {
                "invalid start byte"
            } else {
                "invalid continuation byte"
            };
            cli.console.warning(&format!(
                "Could not read file {rel}, error: {reason}. Skipping this file."
            ));
            return Ok(());
        }
    };
    let is_sql = dest
        .file_name()
        .is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(".sql"));
    if !contents.contains("<%") && !(is_sql && snow_api::rendering::has_sql_templates(&contents)) {
        return Ok(());
    }
    cli.console.step(&format!("Expanding templates in {rel}"));
    cli.console.indent();
    let overrides = pkg.project.env_overrides.clone();
    let r = snow_api::rendering::render_project_sql(
        &contents,
        &rel,
        &pkg.project.ctx,
        &overrides,
        &mut cli.console,
        is_sql,
    );
    cli.console.dedent();
    let expanded = r.map_err(|(m, line)| {
        let l = line.map(|l| format!(":{l}")).unwrap_or_default();
        SnowError::click(format!(
            "File \"{rel}{l}\" does not contain a valid template: {m}"
        ))
        .named("InvalidTemplateInFileError")
    })?;
    if expanded != contents {
        if dest.is_symlink() {
            std::fs::remove_file(dest)?;
        }
        std::fs::write(dest, expanded)?;
    }
    Ok(())
}
