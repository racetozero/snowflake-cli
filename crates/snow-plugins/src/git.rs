//! `snow git`.

use snow_api::identifiers::to_string_literal;
use snow_api::output::Output;
use snow_api::stage_path::StagePath;
use snow_api::{Cli, Result, SnowError};

/// `_repo_path_argument_callback`.
pub fn validate_repo_path(path: &str) -> Result<()> {
    if !crate::stage::is_stage_path(path) || path.matches('/').count() < 3 {
        return Err(SnowError::click(
            "REPOSITORY_PATH should be a path to git repository stage with scope provided. Path to the repository root must end with '/'. For example: @my_repo/branches/main/",
        ));
    }
    Ok(())
}

fn repo(cli: &Cli) -> Result<String> {
    Ok(crate::object::fqn_arg(cli, "repository_name")?.identifier())
}

pub fn list_branches(cli: &mut Cli) -> Result<Output> {
    let r = repo(cli)?;
    let like = cli.str("like").unwrap_or_else(|| "%%".into());
    let c = cli.execute_query(&format!(
        "show git branches like {} in {r}",
        to_string_literal(&like)
    ))?;
    Ok(cli.query_result(&c))
}

pub fn list_tags(cli: &mut Cli) -> Result<Output> {
    let r = repo(cli)?;
    let like = cli.str("like").unwrap_or_else(|| "%%".into());
    let c = cli.execute_query(&format!(
        "show git tags like {} in {r}",
        to_string_literal(&like)
    ))?;
    Ok(cli.query_result(&c))
}

pub fn fetch(cli: &mut Cli) -> Result<Output> {
    let r = repo(cli)?;
    let c = cli.execute_query(&format!("alter git repository {r} fetch"))?;
    Ok(cli.query_result(&c))
}

pub fn list_files(cli: &mut Cli) -> Result<Output> {
    let p = StagePath::from_git_str(&cli.str("repository_path").unwrap_or_default());
    let pattern = cli.str("pattern");
    let c = cli.execute_query(&crate::stage::list_files_query(&p, pattern.as_deref()))?;
    Ok(cli.query_result(&c))
}

/// `snow git copy`: copy to a stage, or download keeping the repository tree.
pub fn copy(cli: &mut Cli) -> snow_api::Result<snow_api::output::Output> {
    let src = cli.str("repository_path").unwrap_or_default();
    let dest = cli.str("destination_path").unwrap_or_default();
    if crate::stage::is_stage_path(&dest) {
        let q = crate::stage::copy_files_query_from(
            snow_api::stage_path::StagePath::from_git_str(&src),
            &dest,
        )?;
        let c = cli.execute_query(&q)?;
        return Ok(cli.query_result(&c));
    }
    let parallel = cli.int("parallel").unwrap_or(4);
    let cwd = std::env::current_dir().unwrap_or_default();
    let target = crate::paths::resolve(cwd.join(&dest));
    let rows = crate::stage::get_recursive(cli, &src, &target, parallel, true)?;
    Ok(snow_api::output::Output::Collection(rows))
}

fn unique_name(cli: &mut Cli, plural: &str, proposed: &str) -> Result<String> {
    let c = cli.execute_query(&format!(
        "show {plural} like {}",
        to_string_literal(&format!("{proposed}%"))
    ))?;
    let idx = match c.column_index("name") {
        Some(i) => i,
        None if c.rows.is_empty() => 0,
        None => return Err(SnowError::unexpected("'name'").named("KeyError")),
    };
    let existing: std::collections::HashSet<String> = c
        .rows
        .iter()
        .filter_map(|r| r.get(idx).cloned().flatten())
        .map(|n| n.to_uppercase())
        .collect();
    let mut result = proposed.to_string();
    let mut i = 1;
    while existing.contains(&result.to_uppercase()) {
        result = format!("{proposed}{i}");
        i += 1;
    }
    Ok(result)
}

/// `snow git setup` (interactive).
pub fn setup(cli: &mut Cli) -> Result<Output> {
    use snow_api::identifiers::Fqn;
    let repo = crate::object::fqn_arg(cli, "repository_name")?;
    if crate::object::object_exists(cli, "git repository", &repo)? {
        return Err(SnowError::click(format!(
            "Repository '{}' already exists",
            repo.identifier()
        )));
    }
    let url = crate::common::prompt_default(cli, "Origin url", "")?;
    if !url.starts_with("https://") {
        return Err(SnowError::click("Url address should start with 'https'"));
    }
    if url.contains('\'') {
        return Err(SnowError::click(
            "Url address must not contain single-quote characters",
        ));
    }
    let mut secret: Option<Fqn> = None;
    let mut create_secret: Option<(String, String)> = None;
    if crate::common::confirm(cli, "Use secret for authentication?", false)? {
        let default = Fqn::from_string(&format!("{}_secret", repo.name))?
            .set_schema(repo.schema.as_deref())
            .set_database(repo.database.as_deref());
        let name = unique_name(cli, "secrets", &default.name)?;
        let answer = crate::common::prompt_default(
            cli,
            "Secret identifier (will be created if not exists)",
            &name,
        )?;
        let mut s = Fqn::from_string(&answer)?;
        if s.database.is_none() {
            s = s.set_database(repo.database.as_deref());
        }
        if s.schema.is_none() {
            s = s.set_schema(repo.schema.as_deref());
        }
        if crate::object::object_exists(cli, "secret", &s)? {
            cli.console
                .step(&format!("Using existing secret '{}'", s.identifier()));
        } else {
            cli.console
                .step(&format!("Secret '{}' will be created", s.identifier()));
            let user = crate::common::prompt_default(cli, "username", "")?;
            let pwd = crate::common::prompt(cli, "password/token", true)?.unwrap_or_default();
            create_secret = Some((user, pwd));
        }
        secret = Some(s);
    }
    let api_default = unique_name(
        cli,
        "integrations",
        &format!("{}_api_integration", repo.name),
    )?;
    let api_name = crate::common::prompt_default(
        cli,
        "API integration identifier (will be created if not exists)",
        &api_default,
    )?;
    let api = Fqn::from_string(&format!("{}_api_integration", repo.name))?;
    let api = Fqn {
        name: api_name,
        ..api
    };
    if let (Some(s), Some((u, p))) = (&secret, &create_secret) {
        cli.execute_query(&format!(
            "\ncreate secret {}\ntype = password\nusername = {}\npassword = {}\n",
            s.sql_identifier(),
            to_string_literal(u),
            to_string_literal(p)
        ))?;
        cli.console.step(&format!(
            "Secret '{}' successfully created.",
            s.identifier()
        ));
    }
    if !crate::object::object_exists(cli, "integration", &api)? {
        cli.execute_query(&format!(
            "\ncreate api integration {}\napi_provider = git_https_api\napi_allowed_prefixes = ({})\nallowed_authentication_secrets = ({})\nenabled = true\n",
            api.sql_identifier(),
            to_string_literal(&url),
            secret.as_ref().map(|s| s.identifier()).unwrap_or_default()
        ))?;
        cli.console.step(&format!(
            "API integration '{}' successfully created.",
            api.identifier()
        ));
    } else {
        cli.console.step(&format!(
            "Using existing API integration '{}'.",
            api.identifier()
        ));
    }
    let mut q = format!(
        "\ncreate git repository {}\napi_integration = {}\norigin = {}\n",
        repo.sql_identifier(),
        snow_api::identifiers::to_identifier(&api.identifier()),
        to_string_literal(&url)
    );
    if let Some(s) = &secret {
        q.push_str(&format!("git_credentials = {}\n", s.identifier()));
    }
    let c = cli.execute_query(&q)?;
    Ok(cli.query_result(&c))
}
