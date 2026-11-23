//! `snow feature`: upstream delegates every command to the optional
//! `snowflake-ml-python[feature_store]` library (`decl_api`). The Rust binary cannot
//! load it, so every command takes upstream's missing-library path.

use snow_api::output::Output;
use snow_api::{Cli, Result, SnowError};

/// `feature_store_preflight`: `_require_snowflake_ml()` with no library available.
pub fn preflight(_cli: &mut Cli) -> Result<Output> {
    Err(SnowError::cli(
        "Error: 'snow feature' requires the snowflake-ml-python[feature_store] library",
    ))
}
