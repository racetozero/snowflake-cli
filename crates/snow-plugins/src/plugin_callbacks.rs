//! Callbacks defined by individual plugins.

use snow_api::{Cli, Result};
use snow_click::Value;
use snow_click::spec::Param;

pub fn run(cli: &mut Cli, param: &'static Param, name: &str, value: Value) -> Result<Value> {
    let _ = param;
    match name {
        "_env_callback" => crate::dbt::reject_control_chars(value.as_str(), "--env")?,
        "_default_env_callback" => {
            crate::dbt::reject_control_chars(value.as_str(), "--default-env")?
        }
        "_git_commit_callback" => crate::dbt::reject_control_chars(value.as_str(), "--git-commit")?,
        "_git_branch_callback" => crate::dbt::reject_control_chars(value.as_str(), "--git-branch")?,
        "_git_url_callback" => crate::dbt::reject_control_chars(value.as_str(), "--git-url")?,
        "_import_callback" => {
            for e in value.strings() {
                crate::dbt::render_import(&e)?;
            }
        }
        "_compute_pool_name_callback" => {
            if let Some(v) = value.as_str() {
                crate::spcs::compute_pool::validate_name(v)?;
            }
        }
        "_repo_name_callback" => {
            if let Some(v) = value.as_str() {
                crate::spcs::image_repository::validate_name(v)?;
            }
        }
        "_service_name_callback" => {
            if let Some(v) = value.as_str() {
                crate::spcs::services::validate_name(v)?;
            }
        }
        "_repo_path_argument_callback" => {
            if let Some(v) = value.as_str() {
                crate::git::validate_repo_path(v)?;
            }
        }
        "_pattern_option_callback" => {
            if let Some(v) = value.as_str()
                && v.matches('\'').count() != v.matches("\\'").count()
            {
                return Err(snow_api::SnowError::click(
                    "All \"'\" characters in PATTERN must be escaped: \"\\'\"",
                ));
            }
        }
        "_interactive_callback" => {
            // `None` means "interactive when stdin and stdout are terminals".
            if matches!(value, Value::None) {
                use std::io::IsTerminal;
                return Ok(Value::Bool(
                    std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
                ));
            }
        }
        "_path_argument_callback" => {
            if let Some(p) = value.as_str()
                && std::path::Path::new(p).exists()
            {
                return Err(snow_api::SnowError::click(format!(
                    "The directory {p} already exists. Please specify a different path for the project."
                )));
            }
        }
        "_deployment_name_callback" => {
            if value.as_str().is_some_and(|v| v.trim().is_empty()) {
                return Err(snow_api::SnowError::Click(
                    snow_click::ClickError::usage_class(
                        "Invalid value for --deployment: Deployment name or alias cannot be blank."
                            .into(),
                        "BadParameter",
                    ),
                ));
            }
        }
        "_comment_callback" => {
            if let Some(c) = value.as_str() {
                return Ok(Value::Str(snow_api::identifiers::to_string_literal(c)));
            }
        }
        _ => {}
    }
    let _ = cli;
    Ok(value)
}
