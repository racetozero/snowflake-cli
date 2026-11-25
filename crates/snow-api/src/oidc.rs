//! OIDC tokens from the environment (`_app/auth/oidc_providers.py`).

use crate::{Cli, Result, SnowError};

fn github_available() -> bool {
    std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
}

fn github_token(cli: &Cli) -> Result<String> {
    let audience = match std::env::var("SNOWFLAKE_AUDIENCE") {
        Ok(a) if !a.is_empty() => a,
        _ => {
            if cli.config.bool_value(&["cli"], "air_gapped", Some(false))? == Some(true) {
                let m = "Failed to detect OIDC credentials: SNOWFLAKE_AUDIENCE is not set. Set it to the audience for this deployment.";
                crate::logs::log(
                    crate::logs::ERROR,
                    "snowflake.cli._app.auth.oidc_providers",
                    m,
                );
                return Err(SnowError::cli(m));
            }
            "snowflakecomputing.com".into()
        }
    };
    let fail = |m: &str| {
        let m = format!("Failed to detect OIDC credentials: {m}");
        crate::logs::log(
            crate::logs::ERROR,
            "snowflake.cli._app.auth.oidc_providers",
            &m,
        );
        SnowError::cli(m)
    };
    let token = std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").unwrap_or_default();
    if token.is_empty() {
        return Err(fail(
            "GitHub: missing or insufficient OIDC token permissions, the ACTIONS_ID_TOKEN_REQUEST_TOKEN environment variable was unset",
        ));
    }
    let url = std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").unwrap_or_default();
    if url.is_empty() {
        return Err(fail(
            "GitHub: missing or insufficient OIDC token permissions, the ACTIONS_ID_TOKEN_REQUEST_URL environment variable was unset",
        ));
    }
    let mut u = url::Url::parse(&url).map_err(|e| fail(&e.to_string()))?;
    u.query_pairs_mut().append_pair("audience", &audience);
    let agent = snow_connector::http_agent(std::time::Duration::from_secs(30));
    let res = agent
        .get(u.as_str())
        .set("Authorization", &format!("bearer {token}"))
        .call();
    let body = match res {
        Ok(r) => r.into_string().unwrap_or_default(),
        Err(ureq::Error::Status(code, r)) => {
            let b = r.into_string().unwrap_or_default();
            return Err(fail(&format!(
                "GitHub: OIDC token request failed (code={code}, body={})",
                snow_click::py_repr(&b)
            )));
        }
        Err(e) => return Err(fail(&e.to_string())),
    };
    let v: Option<String> = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["value"].as_str().map(str::to_string));
    match v {
        Some(t) if !t.is_empty() => Ok(t),
        Some(_) => {
            crate::logs::log(
                crate::logs::ERROR,
                "snowflake.cli._app.auth.oidc_providers",
                "No OIDC credentials detected",
            );
            Err(fail(
                "No OIDC credentials detected. This command should be run in a GitHub Actions environment.",
            ))
        }
        None => Err(fail("GitHub: malformed or incomplete JSON")),
    }
}

/// `OidcManager.read_token(type)`; `ty` is `auto` or a provider name.
pub fn read_token(cli: &Cli, ty: &str) -> Result<String> {
    crate::logs::info(
        "snowflake.cli._plugins.auth.oidc.manager",
        &format!(
            "Reading OIDC token with provider type: OidcProviderTypeWithAuto.{}",
            ty.to_uppercase()
        ),
    );
    if github_available() {
        if ty == "auto" {
            crate::logs::info(
                "snowflake.cli._app.auth.oidc_providers",
                "Found 1 available provider: github",
            );
        }
        github_token(cli)
    } else if ty == "github" {
        Err(SnowError::cli(
            "Provider 'github' is not available in the current environment.",
        ))
    } else {
        Err(SnowError::cli(
            "No OIDC provider detected in current environment. Available providers: github. Use --type <provider> to specify a provider explicitly.",
        ))
    }
}
