//! `snow upgrade`: self-upgrade is only offered by upstream's snowflake-managed direct
//! install. The Rust binary is never that channel, so it always gets the refusal.

use snow_api::output::{Output, Pv};
use snow_api::{Cli, Result, SnowError};

const INSTALL_SH: &str =
    "curl -LsS https://sfc-repo.snowflakecomputing.com/snowflake-cli/install.sh | sh";
const INSTALL_PS1: &str =
    "irm https://sfc-repo.snowflakecomputing.com/snowflake-cli/install.ps1 | iex";

pub fn upgrade(cli: &mut Cli) -> Result<Output> {
    if cli.config.bool_value(&["cli"], "air_gapped", Some(false))? == Some(true) {
        return Err(SnowError::cli(
            "Upgrades are disabled because cli.air_gapped is set. Unset cli.air_gapped or SNOWFLAKE_CLI_AIR_GAPPED to upgrade from a networked host.",
        ));
    }
    let channel = crate::root::INSTALLATION_SOURCE;
    let command = if cfg!(windows) {
        INSTALL_PS1
    } else {
        INSTALL_SH
    };
    if cli.format.is_json() {
        return Ok(Output::object(vec![
            ("channel", Pv::s(channel)),
            ("status", Pv::s("refused")),
            ("install_command", Pv::s(command)),
        ]));
    }
    Ok(Output::message(format!(
        "snow upgrade can only be used with a direct install of Snowflake CLI.\nThis install is {channel}.\n\nInstall using a direct install:\n  {command}"
    )))
}
