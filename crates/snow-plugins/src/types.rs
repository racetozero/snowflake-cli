//! Snowflake CLI parameter types (`IdentifierType`, `LocalDirectoryType`, ...).

use snow_api::identifiers::Fqn;
use snow_api::{Cli, Result, SnowError};
use snow_click::Value;
use snow_click::spec::Param;

pub fn is_stage_path(p: &str) -> bool {
    p.starts_with('@') || p.starts_with("snow://")
}

pub fn convert(
    _cli: &mut Cli,
    _param: &'static Param,
    ty: &'static str,
    raw: &str,
) -> Result<Value> {
    match ty {
        "IdentifierType" => {
            Fqn::from_string(raw)?;
            Ok(Value::Str(raw.to_string()))
        }
        "IdentifierStageType" => {
            Fqn::from_stage(raw)?;
            Ok(Value::Str(raw.to_string()))
        }
        "LocalDirectoryType" | "LocalFileType" => {
            if is_stage_path(raw) {
                let noun = if ty == "LocalDirectoryType" {
                    "directory"
                } else {
                    "file"
                };
                return Err(SnowError::argument(format!(
                    "Stage paths are not supported. Please provide a local {noun} path."
                )));
            }
            Ok(Value::Str(crate::paths::resolve(raw)))
        }
        "_parse_tag" => {
            crate::spcs::parse_tag(raw)?;
            Ok(Value::Str(raw.to_string()))
        }
        _ => Ok(Value::Str(raw.to_string())),
    }
}
