//! Authenticators and the login request.

use crate::{
    CLIENT_APP_ID, CLIENT_APP_VERSION, Connection, DbError, ErrorKind, response_error, uuid4,
};
use base64::Engine;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::time::{SystemTime, UNIX_EPOCH};

fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build the key-pair JWT (`AuthByKeyPair`).
pub fn keypair_jwt(account: &str, user: &str, der: &[u8]) -> Result<String, DbError> {
    use pkcs8::DecodePrivateKey;
    use rsa::pkcs8::EncodePublicKey;
    use rsa::signature::{SignatureEncoding, Signer};
    let key = rsa::RsaPrivateKey::from_pkcs8_der(der).map_err(|e| {
        DbError::new(
            ErrorKind::Programming,
            251_008,
            None,
            format!("Failed to load private key: {e}"),
        )
    })?;
    let public = rsa::RsaPublicKey::from(&key)
        .to_public_key_der()
        .map_err(|e| DbError::new(ErrorKind::Programming, 251_008, None, e.to_string()))?;
    let fp = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(public.as_bytes()));
    let account = account.split('.').next().unwrap_or(account).to_uppercase();
    let user = user.to_uppercase();
    let qualified = format!("{account}.{user}");
    let iat = now();
    let header = json!({"alg": "RS256", "typ": "JWT"});
    let payload = json!({
        "iss": format!("{qualified}.SHA256:{fp}"),
        "sub": qualified,
        "iat": iat,
        "exp": iat + 120,
    });
    let signing_input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(payload.to_string().as_bytes())
    );
    let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(key);
    let sig = signer.sign(signing_input.as_bytes()).to_bytes();
    Ok(format!("{signing_input}.{}", b64url(&sig)))
}

/// Convert a PEM private key (optionally encrypted) into PKCS#8 DER.
pub fn pem_to_der(pem: &str, passphrase: Option<&str>) -> Result<Vec<u8>, String> {
    use pkcs8::der::Encode;
    if pem.starts_with("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
        let (_, doc) = pkcs8::SecretDocument::from_pem(pem).map_err(|e| e.to_string())?;
        let enc =
            pkcs8::EncryptedPrivateKeyInfo::try_from(doc.as_bytes()).map_err(|e| e.to_string())?;
        let dec = enc
            .decrypt(passphrase.unwrap_or("").as_bytes())
            .map_err(|_| "Bad decrypt. Incorrect password?".to_string())?;
        Ok(dec.as_bytes().to_vec())
    } else {
        let (_, doc) = pkcs8::SecretDocument::from_pem(pem).map_err(|e| e.to_string())?;
        let info = pkcs8::PrivateKeyInfo::try_from(doc.as_bytes()).map_err(|e| e.to_string())?;
        info.to_der().map_err(|e| e.to_string())
    }
}

fn client_environment(conn: &Connection) -> Value {
    json!({
        "APPLICATION": conn.params.application.clone().unwrap_or_else(|| CLIENT_APP_ID.to_string()),
        "OS": std::env::consts::OS,
        "OS_VERSION": std::env::consts::OS,
        "PYTHON_VERSION": "3.12.0",
        "PYTHON_RUNTIME": "CPython",
        "PYTHON_COMPILER": "Clang",
        "OCSP_MODE": "FAIL_OPEN",
        "TRACING": 30,
        "LOGIN_TIMEOUT": conn.params.login_timeout,
        "NETWORK_TIMEOUT": Value::Null,
        "SOCKET_TIMEOUT": Value::Null,
    })
}

fn require(v: &Option<String>, errno: i64, msg: &str) -> Result<String, DbError> {
    match v {
        Some(s) if !s.is_empty() => Ok(s.clone()),
        _ => Err(DbError::new(ErrorKind::Programming, errno, None, msg)),
    }
}

/// Authenticate and store the session tokens on `conn`.
pub fn login(conn: &mut Connection) -> Result<(), DbError> {
    if conn.params.account.as_deref().unwrap_or("").is_empty() {
        return Err(DbError::new(
            ErrorKind::Programming,
            251_001,
            None,
            "Account must be specified",
        ));
    }
    let authenticator = conn
        .params
        .authenticator
        .clone()
        .unwrap_or_else(|| "SNOWFLAKE".into())
        .to_uppercase();
    let mut data = Map::new();
    data.insert(
        "CLIENT_APP_ID".into(),
        json!(
            conn.params
                .internal_application_name
                .clone()
                .unwrap_or_else(|| CLIENT_APP_ID.into())
        ),
    );
    data.insert(
        "CLIENT_APP_VERSION".into(),
        json!(
            conn.params
                .internal_application_version
                .clone()
                .unwrap_or_else(|| CLIENT_APP_VERSION.into())
        ),
    );
    data.insert("SVN_REVISION".into(), Value::Null);
    data.insert("ACCOUNT_NAME".into(), json!(conn.account));
    data.insert("LOGIN_NAME".into(), json!(conn.params.user));
    data.insert("CLIENT_ENVIRONMENT".into(), client_environment(conn));
    let mut session_parameters = conn.params.session_parameters.clone();
    if let Some(k) = conn.params.client_session_keep_alive {
        session_parameters.insert("CLIENT_SESSION_KEEP_ALIVE".into(), json!(k));
    }
    if let Some(a) = conn.params.autocommit {
        session_parameters.insert("AUTOCOMMIT".into(), json!(a));
    }
    if let Some(s) = conn.params.client_store_temporary_credential {
        session_parameters.insert("CLIENT_STORE_TEMPORARY_CREDENTIAL".into(), json!(s));
    }
    if conn.params.client_request_mfa_token {
        session_parameters.insert("CLIENT_REQUEST_MFA_TOKEN".into(), json!(true));
    }
    if let Some(r) = &conn.params.secondary_roles {
        data.insert("SECONDARY_ROLES".into(), json!(r));
    }
    if conn.params.server_session_keep_alive == Some(true) {
        session_parameters.insert("SERVER_SESSION_KEEP_ALIVE".into(), json!(true));
    }
    if !session_parameters.is_empty() {
        data.insert(
            "SESSION_PARAMETERS".into(),
            Value::Object(session_parameters),
        );
    }
    let token_from_file = || -> Result<Option<String>, DbError> {
        match &conn.params.token_file_path {
            Some(p) => std::fs::read_to_string(p)
                .map(Some)
                .map_err(|e| DbError::new(ErrorKind::Programming, 251_005, None, e.to_string())),
            None => Ok(None),
        }
    };
    match authenticator.as_str() {
        "SNOWFLAKE" => {
            require(&conn.params.user, 251_006, "User is empty")?;
            let pw = require(&conn.params.password, 251_006, "Password is empty")?;
            data.insert("PASSWORD".into(), json!(pw));
        }
        "USERNAME_PASSWORD_MFA" => {
            require(&conn.params.user, 251_006, "User is empty")?;
            let pw = require(&conn.params.password, 251_006, "Password is empty")?;
            data.insert("PASSWORD".into(), json!(pw));
            data.insert("AUTHENTICATOR".into(), json!("USERNAME_PASSWORD_MFA"));
            if let Some(code) = &conn.params.passcode {
                data.insert("EXT_AUTHN_DUO_METHOD".into(), json!("passcode"));
                data.insert("PASSCODE".into(), json!(code));
            } else {
                data.insert("EXT_AUTHN_DUO_METHOD".into(), json!("push"));
            }
        }
        "SNOWFLAKE_JWT" => {
            let user = require(&conn.params.user, 251_006, "User is empty")?;
            let der = conn.params.private_key.clone().ok_or_else(|| {
                DbError::new(
                    ErrorKind::Programming,
                    251_008,
                    None,
                    "Private key is empty",
                )
            })?;
            data.insert("AUTHENTICATOR".into(), json!("SNOWFLAKE_JWT"));
            data.insert(
                "TOKEN".into(),
                json!(keypair_jwt(&conn.account, &user, &der)?),
            );
        }
        "OAUTH" => {
            // The connector sends whatever it has (the raw file content, or null).
            let token = token_from_file()?.or_else(|| conn.params.token.clone());
            data.insert("AUTHENTICATOR".into(), json!("OAUTH"));
            data.insert("TOKEN".into(), json!(token));
        }
        "PROGRAMMATIC_ACCESS_TOKEN" => {
            // The connector sends whatever it has (the raw file content, or null).
            let token = token_from_file()?.or_else(|| conn.params.token.clone());
            data.insert("AUTHENTICATOR".into(), json!("PROGRAMMATIC_ACCESS_TOKEN"));
            data.insert("TOKEN".into(), json!(token));
        }
        "WORKLOAD_IDENTITY" => {
            let provider = conn
                .params
                .workload_identity_provider
                .clone()
                .unwrap_or_default()
                .to_uppercase();
            let p = &conn.params;
            crate::wif::validate(
                &provider,
                &crate::wif::Options {
                    entra_resource: p.workload_identity_entra_resource.clone(),
                    impersonation_path: p.workload_identity_impersonation_path.clone(),
                    aws_use_outbound_token: p.workload_identity_aws_use_outbound_token,
                    host: p.workload_identity_host.clone(),
                },
            )?;
            data.insert("AUTHENTICATOR".into(), json!("WORKLOAD_IDENTITY"));
            data.insert("PROVIDER".into(), json!(provider));
            let token = token_from_file()?.or_else(|| conn.params.token.clone());
            match provider.as_str() {
                "OIDC" => {
                    let t = require(
                        &token,
                        251_005,
                        "token must be provided if workload_identity_provider=OIDC",
                    )?;
                    data.insert("TOKEN".into(), json!(t));
                }
                _ => {
                    let t = crate::auth::wif_token(&provider, conn)?;
                    data.insert("TOKEN".into(), json!(t));
                }
            }
        }
        "EXTERNALBROWSER" => {
            let (token, proof) = external_browser(conn)?;
            data.insert("AUTHENTICATOR".into(), json!("EXTERNALBROWSER"));
            data.insert("TOKEN".into(), json!(token));
            data.insert("PROOF_KEY".into(), json!(proof));
        }
        "OAUTH_CLIENT_CREDENTIALS" | "OAUTH_AUTHORIZATION_CODE" => {
            let token = oauth_token(conn, &authenticator)?;
            data.insert("AUTHENTICATOR".into(), json!("OAUTH"));
            data.insert("TOKEN".into(), json!(token));
            data.insert(
                "OAUTH_TYPE".into(),
                json!(if authenticator == "OAUTH_CLIENT_CREDENTIALS" {
                    "client_credentials"
                } else {
                    "authorization_code"
                }),
            );
        }
        other => {
            return Err(DbError::new(
                ErrorKind::Programming,
                251_000 + 4,
                None,
                format!("Unknown authenticator: {other}"),
            ));
        }
    }
    let mut q = vec![("request_id".to_string(), uuid4())];
    let s = &conn.session;
    for (k, v) in [
        ("databaseName", &s.database),
        ("schemaName", &s.schema),
        ("warehouse", &s.warehouse),
        ("roleName", &s.role),
    ] {
        if let Some(v) = v {
            q.push((k.to_string(), v.clone()));
        }
    }
    q.push(("request_guid".into(), uuid4()));
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(q)
        .finish();
    let body = json!({ "data": Value::Object(data) });
    let v = conn.post(&format!("/session/v1/login-request?{query}"), &body, None)?;
    if v["success"].as_bool() != Some(true) {
        let mut e = response_error(&v, ErrorKind::Database);
        let code = v["code"].as_str().unwrap_or("").to_string();
        const REJECTION: &[&str] = &[
            "390100", "390144", "394300", "394301", "394302", "394303", "394304", "394305",
            "394306",
        ];
        e.sqlstate = Some(
            if REJECTION.contains(&code.as_str()) {
                "28000"
            } else {
                "08001"
            }
            .into(),
        );
        let port = conn
            .base_url
            .rsplit(':')
            .next()
            .unwrap_or("443")
            .to_string();
        e.raw_msg = format!(
            "Failed to connect to DB: {}:{}. {}",
            conn.host, port, e.raw_msg
        );
        e.sfqid = None;
        return Err(e);
    }
    let d = v["data"].clone();
    conn.set_session(&d);
    Ok(())
}

/// Workload identity attestation for cloud providers (AWS, AZURE, GCP).
pub fn wif_token(provider: &str, conn: &Connection) -> Result<String, DbError> {
    let p = &conn.params;
    let opts = crate::wif::Options {
        entra_resource: p.workload_identity_entra_resource.clone(),
        impersonation_path: p.workload_identity_impersonation_path.clone(),
        aws_use_outbound_token: p.workload_identity_aws_use_outbound_token,
        host: p.workload_identity_host.clone(),
    };
    match provider {
        "AWS" => crate::wif::aws_attestation(&opts),
        "AZURE" => crate::wif::azure_attestation(&opts),
        "GCP" => crate::wif::gcp_attestation(&opts),
        _ => Err(DbError::new(
            ErrorKind::Programming,
            251_018,
            None,
            format!("Unknown workload_identity_provider: '{provider}'."),
        )),
    }
}

/// `AuthByWebBrowser`: open the SSO URL and wait for the redirect carrying the token.
fn external_browser(conn: &Connection) -> Result<(String, String), DbError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| DbError::new(ErrorKind::Operational, 251_000, None, e.to_string()))?;
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let body = json!({"data": {
        "ACCOUNT_NAME": conn.account,
        "LOGIN_NAME": conn.params.user,
        "PORT": port.to_string(),
        "PROTOCOL": "http",
        "AUTHENTICATOR": "EXTERNALBROWSER",
        "CLIENT_APP_ID": CLIENT_APP_ID,
        "CLIENT_APP_VERSION": CLIENT_APP_VERSION,
        "BROWSER_MODE_REDIRECT_PORT": port.to_string(),
    }});
    let v = conn.post(
        &format!("/session/authenticator-request?request_guid={}", uuid4()),
        &body,
        None,
    )?;
    if v["success"].as_bool() != Some(true) {
        return Err(response_error(&v, ErrorKind::Database));
    }
    let sso = v["data"]["ssoUrl"].as_str().unwrap_or_default().to_string();
    let proof = v["data"]["proofKey"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    eprintln!(
        "Initiating login request with your identity provider. A browser window should have opened for you to complete the login. If you can't see it, check existing browser windows, or your OS settings. Press CTRL+C to abort and try again..."
    );
    eprintln!("Going to open: {sso} to authenticate...");
    open_browser(&sso);
    let (mut stream, _) = listener
        .accept()
        .map_err(|e| DbError::new(ErrorKind::Operational, 251_000, None, e.to_string()))?;
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| DbError::new(ErrorKind::Operational, 251_000, None, e.to_string()))?,
    );
    let mut first = String::new();
    let _ = reader.read_line(&mut first);
    let target = first.split_whitespace().nth(1).unwrap_or("/").to_string();
    let parsed = url::Url::parse(&format!("http://localhost{target}"))
        .map_err(|e| DbError::new(ErrorKind::Operational, 251_000, None, e.to_string()))?;
    let token = parsed
        .query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    let html = "<!DOCTYPE html><html><head><meta charset=\"UTF-8\"/><title>SAML Response for Snowflake</title></head><body>Your identity was confirmed and propagated to Snowflake PythonConnector.\nYou can close this window now and go back where you started from.</body></html>";
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
        html.len(),
        html
    );
    Ok((token, proof))
}

pub fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        ("open", vec![url.to_string()])
    } else if cfg!(windows) {
        (
            "cmd",
            vec![
                "/C".to_string(),
                "start".to_string(),
                String::new(),
                url.to_string(),
            ],
        )
    } else {
        ("xdg-open", vec![url.to_string()])
    };
    let _ = std::process::Command::new(cmd.0)
        .args(cmd.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// OAuth 2.0 token from the configured identity provider.
fn oauth_token(conn: &Connection, authenticator: &str) -> Result<String, DbError> {
    let p = &conn.params;
    let fail = |m: String| DbError::new(ErrorKind::Programming, 251_005, None, m);
    let token_url = p
        .oauth_token_request_url
        .clone()
        .unwrap_or_else(|| format!("{}/oauth/token-request", conn.base_url));
    let client_id = p.oauth_client_id.clone().unwrap_or_default();
    let secret = p.oauth_client_secret.clone().unwrap_or_default();
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{secret}"));
    let scope = p.oauth_scope.clone().unwrap_or_else(|| {
        conn.params
            .role
            .as_ref()
            .map(|r| format!("session:role:{r}"))
            .unwrap_or_default()
    });
    let form: Vec<(String, String)> = if authenticator == "OAUTH_CLIENT_CREDENTIALS" {
        vec![
            ("grant_type".into(), "client_credentials".into()),
            ("scope".into(), scope),
        ]
    } else {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| fail(e.to_string()))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let redirect = p
            .oauth_redirect_uri
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{port}"));
        let verifier = b64url(&rand::random::<[u8; 32]>());
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        let state = b64url(&rand::random::<[u8; 16]>());
        let auth_url = p
            .oauth_authorization_url
            .clone()
            .unwrap_or_else(|| format!("{}/oauth/authorize", conn.base_url));
        let mut u = url::Url::parse(&auth_url).map_err(|e| fail(e.to_string()))?;
        {
            let mut q = u.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("client_id", &client_id)
                .append_pair("redirect_uri", &redirect)
                .append_pair("state", &state)
                .append_pair("scope", &scope);
            if !p.oauth_disable_pkce {
                q.append_pair("code_challenge_method", "S256")
                    .append_pair("code_challenge", &challenge);
            }
        }
        open_browser(u.as_str());
        let (mut stream, _) = listener.accept().map_err(|e| fail(e.to_string()))?;
        let mut reader = BufReader::new(stream.try_clone().map_err(|e| fail(e.to_string()))?);
        let mut first = String::new();
        let _ = reader.read_line(&mut first);
        let target = first.split_whitespace().nth(1).unwrap_or("/").to_string();
        let parsed = url::Url::parse(&format!("http://localhost{target}"))
            .map_err(|e| fail(e.to_string()))?;
        let code = parsed
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
        let mut f = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".into(), code),
            ("redirect_uri".into(), redirect),
        ];
        if !p.oauth_disable_pkce {
            f.push(("code_verifier".into(), verifier));
        }
        f
    };
    let body: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let r = conn
        .agent()
        .post(&token_url)
        .set("Authorization", &format!("Basic {basic}"))
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .map_err(|e| {
            fail(format!(
                "Invalid HTTP request from web browser. Idp authentication could have failed. {e}"
            ))
        })?;
    let v: Value = serde_json::from_reader(r.into_reader()).map_err(|e| fail(e.to_string()))?;
    v["access_token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| fail("Invalid HTTP response from identity provider".into()))
}
