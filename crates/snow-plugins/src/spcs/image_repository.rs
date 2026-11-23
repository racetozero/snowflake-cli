//! `snow spcs image-repository`.

use super::{already_exists, fqn};
use snow_api::identifiers::{Fqn, is_valid_object_name, to_string_literal};
use snow_api::output::{Output, Pv};
use snow_api::{Cli, Result, SnowError};

/// `_repo_name_callback`.
pub fn validate_name(raw: &str) -> Result<()> {
    let f = Fqn::from_string(raw)?;
    if !is_valid_object_name(&f.identifier(), 2, false) {
        return Err(SnowError::click(format!(
            "'{}' is not a valid image repository name. Note that image repository names must be unquoted identifiers. The same constraint also applies to database and schema names where you create an image repository.",
            f.identifier()
        )));
    }
    Ok(())
}

pub fn create_repo(
    cli: &mut Cli,
    name: &str,
    if_not_exists: bool,
    replace: bool,
) -> Result<Output> {
    let stmt = if replace {
        "create or replace image repository"
    } else if if_not_exists {
        "create image repository if not exists"
    } else {
        "create image repository"
    };
    cli.check_database_and_schema(name)?;
    let verbose = cli.connector_info;
    let c = cli
        .try_execute(&format!("{stmt} {name}"))
        .map_err(|e| already_exists(e, "image-repository", name, true, verbose))?;
    Ok(cli.single_query_result(&c))
}

pub fn create(cli: &mut Cli) -> Result<Output> {
    let n = fqn(cli, "name")?.identifier();
    let (ine, rep) = (cli.flag("if_not_exists"), cli.flag("replace"));
    create_repo(cli, &n, ine, rep)
}

pub fn list_images(cli: &mut Cli) -> Result<Output> {
    let n = fqn(cli, "name")?.identifier();
    let q = match cli.str("like_option").filter(|l| !l.is_empty()) {
        Some(l) => format!(
            "show images like {} in image repository {n}",
            to_string_literal(&l)
        ),
        None => format!("show images in image repository {n}"),
    };
    let c = cli.execute_query(&q)?;
    Ok(cli.query_result(&c))
}

fn repo_url(cli: &mut Cli, name: &str) -> Result<String> {
    let row = cli.show_specific_object("image repositories", name, "name", "", true)?;
    match row {
        Some(r) => Ok(r
            .get("repository_url")
            .map(|v| v.py_str())
            .unwrap_or_default()),
        None => {
            let s = cli.connection()?.session.clone();
            let f = Fqn::from_string(name)?.using_session(
                s.database.as_deref(),
                s.schema.as_deref(),
                None,
            );
            Err(SnowError::sql(format!(
                "Image repository '{}' does not exist or not authorized.",
                f.identifier()
            ))
            .named("ProgrammingError"))
        }
    }
}

pub fn url(cli: &mut Cli) -> Result<Output> {
    let n = fqn(cli, "name")?.identifier();
    Ok(Output::message(repo_url(cli, &n)?))
}

pub fn list_tags(cli: &mut Cli) -> Result<Output> {
    let n = fqn(cli, "name")?.identifier();
    let url = format!("https://{}", repo_url(cli, &n)?);
    let u = url::Url::parse(&url).map_err(|e| SnowError::unexpected(e.to_string()))?;
    let host = match u.port() {
        Some(p) => format!("{}:{p}", u.host_str().unwrap_or_default()),
        None => u.host_str().unwrap_or_default().to_string(),
    };
    let api = format!("{}://{host}/v2{}", u.scheme(), u.path());
    let bearer = super::image_registry::login_to_registry(cli, &api)?;
    let image = cli.str("image_name").unwrap_or_default();
    let real: String = image.split('/').skip(4).collect::<Vec<_>>().join("/");
    let agent = snow_connector::http_agent(std::time::Duration::from_secs(60));
    let mut tags: Vec<String> = Vec::new();
    let mut query = Some(format!("{api}/{real}/tags/list?n=10"));
    while let Some(q) = query {
        let (status, link, body) = match agent
            .get(&q)
            .set("Authorization", &format!("Bearer {bearer}"))
            .call()
        {
            Ok(r) => {
                let link = r.header("Link").is_some();
                (200, link, r.into_string().unwrap_or_default())
            }
            Err(ureq::Error::Status(c, r)) => {
                let link = r.header("Link").is_some();
                (c, link, r.into_string().unwrap_or_default())
            }
            Err(e) => return Err(SnowError::unexpected(e.to_string())),
        };
        if status != 200 {
            cli.console
                .warning(&format!("Call to the registry failed {body}"));
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| SnowError::unexpected(e.to_string()).named("JSONDecodeError"))?;
        if let Some(t) = v["tags"].as_array() {
            tags.extend(t.iter().map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| x.to_string())
            }));
        }
        query = link.then(|| {
            format!(
                "{api}/{real}/tags/list?n=10&last={}",
                tags.last().cloned().unwrap_or_default()
            )
        });
    }
    let rows = tags
        .iter()
        .map(|t| {
            [("tag".to_string(), Pv::s(format!("{image}:{t}")))]
                .into_iter()
                .collect()
        })
        .collect();
    Ok(Output::Collection(rows))
}
