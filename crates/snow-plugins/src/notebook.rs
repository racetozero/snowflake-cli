//! `snow notebook`.

use snow_api::identifiers::Fqn;
use snow_api::output::Output;
use snow_api::stage_path::StagePath;
use snow_api::{Cli, Result, SnowError};

fn name(cli: &mut Cli) -> Result<Fqn> {
    crate::object::fqn_arg(cli, "identifier")
}

fn using_conn(cli: &mut Cli, f: Fqn) -> Result<Fqn> {
    let s = cli.connection()?.session.clone();
    let u = cli.connection()?.user.clone();
    Ok(f.using_session(s.database.as_deref(), s.schema.as_deref(), u.as_deref()))
}

pub fn execute(cli: &mut Cli) -> Result<Output> {
    let f = name(cli)?;
    cli.execute_query(&format!("EXECUTE NOTEBOOK {}()", f.sql_identifier()))?;
    Ok(Output::message(format!(
        "Notebook {} executed.",
        f.identifier()
    )))
}

pub fn get_url(cli: &mut Cli) -> Result<Output> {
    let f = name(cli)?;
    let f = using_conn(cli, f)?;
    let url = crate::snowsight::make_url(cli, &format!("/#/notebooks/{}", f.url_identifier()))?;
    Ok(Output::message(url))
}

pub fn open(cli: &mut Cli) -> Result<Output> {
    let f = name(cli)?;
    let f = using_conn(cli, f)?;
    let url = crate::snowsight::make_url(cli, &format!("/#/notebooks/{}", f.url_identifier()))?;
    snow_connector::auth::open_browser(&url);
    Ok(Output::message(url))
}

pub fn create(cli: &mut Cli) -> Result<Output> {
    let f = name(cli)?;
    let f = using_conn(cli, f)?;
    let file = cli.str("notebook_file").unwrap_or_default();
    if !file.ends_with(".ipynb") {
        return Err(SnowError::click(format!(
            "Cannot extract notebook file name from path={}",
            snow_click::py_repr(&file)
        ))
        .named("NotebookFilePathError"));
    }
    let sp = StagePath::from_stage_str(&file);
    if sp.parts().is_empty() {
        return Err(SnowError::click(format!(
            "Cannot extract notebook file name from path={}",
            snow_click::py_repr(&file)
        ))
        .named("NotebookFilePathError"));
    }
    let wh = cli
        .connection()?
        .session
        .warehouse
        .clone()
        .unwrap_or_else(|| "None".into());
    let q = format!(
        "\nCREATE OR REPLACE NOTEBOOK {}\nFROM '{}'\nQUERY_WAREHOUSE = '{wh}'\nMAIN_FILE = '{}';\n// Cannot use IDENTIFIER(...)\nALTER NOTEBOOK {} ADD LIVE VERSION FROM LAST;\n",
        f.sql_identifier(),
        sp.parent(),
        sp.name(),
        f.identifier()
    );
    cli.execute_queries(&q)?;
    let url = crate::snowsight::make_url(cli, &format!("/#/notebooks/{}", f.url_identifier()))?;
    Ok(Output::message(url))
}

/// `snow notebook deploy` (`NotebookEntity.action_deploy`).
pub fn deploy(cli: &mut Cli) -> Result<Output> {
    let entity_id = cli.str("entity_id");
    let version_ok = cli.project(false)?.expect("project").meets_version("2");
    if !version_ok {
        return Err(SnowError::usage(
            "This command requires project definition of version at least 2.",
        ));
    }
    let (id, model) = crate::entities::for_operation(cli, entity_id.as_deref(), "notebook")?;
    let f = crate::entities::fqn(&model, &id)?;
    let stage_root = model
        .str_of("stage_path")
        .unwrap_or_else(|| format!("@notebooks/{}", f.name));
    let stage = StagePath::from_stage_str(&stage_root);
    let (replace, prune) = (cli.flag("replace"), cli.flag("prune"));
    if cli
        .try_query(&format!("DESCRIBE NOTEBOOK {};", f.sql_identifier()))?
        .is_ok()
        && !replace
    {
        return Err(SnowError::click(format!(
            "Notebook {} already exists. Consider using --replace.",
            f.name
        )));
    }
    let root = cli.project_root();
    cli.console
        .phase_start(&format!("Uploading artifacts to {}", stage.absolute_path()));
    let artifacts = model
        .get("artifacts")
        .and_then(|a| a.as_list())
        .cloned()
        .unwrap_or_default();
    let r = crate::entities::sync_artifacts_with_stage(
        cli,
        &root.join("output/bundle/notebook"),
        &stage_root,
        prune,
        &artifacts,
    );
    cli.console.phase_end(None);
    r?;
    cli.console
        .phase_start(&format!("Creating notebook {}", f.identifier()));
    let nb = model.str_of("notebook_file").unwrap_or_default();
    let abs = crate::artifacts::resolve_without_follow(std::path::Path::new(&nb));
    let rel = abs
        .strip_prefix(&root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(nb);
    let main = stage.join(&rel);
    let mut q = format!(
        "{}NOTEBOOK {}\nFROM '@{}'\nQUERY_WAREHOUSE = '{}'\nMAIN_FILE = '{}'",
        if replace {
            "CREATE OR REPLACE "
        } else {
            "CREATE "
        },
        f.sql_identifier(),
        main.stage(),
        model
            .str_of("query_warehouse")
            .unwrap_or_else(|| "None".into()),
        main.path_str()
    );
    if let Some(cp) = model.str_of("compute_pool").filter(|s| !s.is_empty()) {
        q.push_str(&format!("\nCOMPUTE_POOL = '{cp}'"));
        if let Some(rn) = model.str_of("runtime_name").filter(|s| !s.is_empty()) {
            q.push_str(&format!("\nRUNTIME_NAME = '{rn}'"));
        }
    } else {
        if let Some(rn) = model.str_of("runtime_name").filter(|s| !s.is_empty()) {
            q.push_str(&format!("\nRUNTIME_NAME = '{rn}'"));
        }
        if let Some(rev) = model
            .str_of("runtime_environment_version")
            .filter(|s| !s.is_empty())
        {
            q.push_str(&format!("\nRUNTIME_ENVIRONMENT_VERSION = '{rev}'"));
        }
    }
    q.push_str(&format!(
        ";\n// Cannot use IDENTIFIER(...)\nALTER NOTEBOOK {} ADD LIVE VERSION FROM LAST;",
        f.identifier()
    ));
    let r = cli.execute_query(&q);
    let url = r.and_then(|_| {
        let ff = using_conn(cli, f.clone())?;
        crate::snowsight::make_url(cli, &format!("/#/notebooks/{}", ff.url_identifier()))
    });
    cli.console.phase_end(None);
    Ok(Output::message(format!(
        "Notebook successfully deployed and available under {}",
        url?
    )))
}
