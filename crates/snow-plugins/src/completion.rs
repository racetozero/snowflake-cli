//! Typer's `--install-completion` / `--show-completion`.

use snow_api::{Cli, Result};

pub fn run(cli: &Cli, install: bool) -> Result<()> {
    let _ = (cli, install);
    Ok(())
}
