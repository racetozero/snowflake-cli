//! Authenticators and the login request.

use crate::{
    CLIENT_APP_ID, CLIENT_APP_VERSION, Connection, DbError, ErrorKind, response_error, uuid4,
};
use base64::Engine;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Build the key-pair JWT (`AuthByKeyPair`). `JWT_LIFETIME_IN_SECONDS` overrides
/// `lifetime` (seconds).
pub fn keypair_jwt(
    account: &str,
    user: &str,
    der: &[u8],
    lifetime: i64,
) -> Result<String, DbError> {
    use pkcs8::DecodePrivateKey;
    use rsa::pkcs8::EncodePublicKey;
    use rsa::signature::{SignatureEncoding, Signer};
    let bad_key = |m: String| DbError::new(ErrorKind::Programming, 251_008, None, m);
    let lifetime = match std::env::var("JWT_LIFETIME_IN_SECONDS") {
        Ok(v) => v.trim().parse().map_err(|_| {
            bad_key(format!(
                "JWT_LIFETIME_IN_SECONDS is '{v}', but it must be a whole number of seconds. Set it to a number such as 60, or unset it."
            ))
        })?,
        Err(_) => lifetime,
    };
    enum Key {
        Rsa(Box<rsa::RsaPrivateKey>),
        Ec(Box<ring::signature::EcdsaKeyPair>),
    }
    // SubjectPublicKeyInfo headers for an uncompressed point on the curve.
    const P256_SPKI: &[u8] = b"\x30\x59\x30\x13\x06\x07\x2a\x86\x48\xce\x3d\x02\x01\x06\x08\x2a\x86\x48\xce\x3d\x03\x01\x07\x03\x42\x00";
    const P384_SPKI: &[u8] = b"\x30\x76\x30\x10\x06\x07\x2a\x86\x48\xce\x3d\x02\x01\x06\x05\x2b\x81\x04\x00\x22\x03\x62\x00";
    let rng = ring::rand::SystemRandom::new();
    let (key, spki, alg) = match rsa::RsaPrivateKey::from_pkcs8_der(der) {
        Ok(k) => {
            let spki = rsa::RsaPublicKey::from(&k)
                .to_public_key_der()
                .map_err(|e| bad_key(e.to_string()))?
                .as_bytes()
                .to_vec();
            (Key::Rsa(Box::new(k)), spki, "RS256")
        }
        Err(rsa_error) => {
            use ring::signature::{
                ECDSA_P256_SHA256_FIXED_SIGNING, ECDSA_P384_SHA384_FIXED_SIGNING, EcdsaKeyPair,
                KeyPair,
            };
            [
                (&ECDSA_P256_SHA256_FIXED_SIGNING, P256_SPKI, "ES256"),
                (&ECDSA_P384_SHA384_FIXED_SIGNING, P384_SPKI, "ES384"),
            ]
            .into_iter()
            .find_map(|(curve, header, alg)| {
                let k = EcdsaKeyPair::from_pkcs8(curve, der, &rng).ok()?;
                let spki = [header, k.public_key().as_ref()].concat();
                Some((Key::Ec(Box::new(k)), spki, alg))
            })
            .ok_or_else(|| {
                bad_key(format!(
                    "Failed to load private key: {rsa_error}. Use an RSA key or an EC key on curve P-256 or P-384, in PKCS#8 format."
                ))
            })?
        }
    };
    let fp = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&spki));
    let account = if account.contains(".global") {
        account.split('-').next()
    } else {
        account.split('.').next()
    }
    .unwrap_or(account)
    .to_uppercase();
    let user = user.to_uppercase();
    let qualified = format!("{account}.{user}");
    let iat = chrono::Utc::now().timestamp();
    let header = json!({"alg": alg, "typ": "JWT"});
    let payload = json!({
        "iss": format!("{qualified}.SHA256:{fp}"),
        "sub": qualified,
        "iat": iat,
        "exp": iat + lifetime,
    });
    let signing_input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(payload.to_string().as_bytes())
    );
    let sig = match key {
        Key::Rsa(k) => rsa::pkcs1v15::SigningKey::<Sha256>::new(*k)
            .sign(signing_input.as_bytes())
            .to_vec(),
        Key::Ec(k) => k
            .sign(&rng, signing_input.as_bytes())
            .map_err(|e| bad_key(e.to_string()))?
            .as_ref()
            .to_vec(),
    };
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
    let invalid = |errno: i64, m: String| DbError::new(ErrorKind::Programming, errno, None, m);
    let given = conn
        .params
        .authenticator
        .clone()
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "SNOWFLAKE".into());
    let authenticator = given.to_uppercase();
    let needs_user = match authenticator.as_str() {
        "SNOWFLAKE" | "USERNAME_PASSWORD_MFA" | "SNOWFLAKE_JWT" | "EXTERNALBROWSER" => true,
        "OAUTH"
        | "OAUTH_AUTHORIZATION_CODE"
        | "OAUTH_CLIENT_CREDENTIALS"
        | "PROGRAMMATIC_ACCESS_TOKEN"
        | "WORKLOAD_IDENTITY" => false,
        a if a.starts_with("HTTPS://") || a == "PAT_WITH_EXTERNAL_SESSION" => {
            return Err(invalid(
                251_007,
                format!(
                    "The authenticator is '{given}', which this snow does not support. Use SNOWFLAKE, SNOWFLAKE_JWT, PROGRAMMATIC_ACCESS_TOKEN, OAUTH, OAUTH_AUTHORIZATION_CODE, OAUTH_CLIENT_CREDENTIALS, USERNAME_PASSWORD_MFA, WORKLOAD_IDENTITY or externalbrowser. To sign in through Okta, set authenticator = \"externalbrowser\"."
                ),
            ));
        }
        _ => {
            return Err(invalid(251_007, format!("Unknown authenticator: {given}")));
        }
    };
    if needs_user && conn.params.user.as_deref().unwrap_or("").is_empty() {
        return Err(invalid(
            251_005,
            "User is empty, but it must be provided unless authenticator is one of OAUTH, NO_AUTH, WORKLOAD_IDENTITY, PROGRAMMATIC_ACCESS_TOKEN, PAT_WITH_EXTERNAL_SESSION, OAUTH_AUTHORIZATION_CODE, OAUTH_CLIENT_CREDENTIALS.".into(),
        ));
    }
    if authenticator != "WORKLOAD_IDENTITY" {
        let p = &conn.params;
        let set = |v: &Option<String>| v.as_ref().is_some_and(|v| !v.is_empty());
        for (option, given) in [
            (
                "workload_identity_provider",
                set(&p.workload_identity_provider),
            ),
            (
                "workload_identity_entra_resource",
                set(&p.workload_identity_entra_resource),
            ),
            (
                "workload_identity_impersonation_path",
                !p.workload_identity_impersonation_path.is_empty(),
            ),
            (
                "workload_identity_aws_use_outbound_token",
                p.workload_identity_aws_use_outbound_token,
            ),
            ("workload_identity_host", set(&p.workload_identity_host)),
        ] {
            if given {
                return Err(invalid(
                    251_017,
                    format!("{option} was set but authenticator was not set to WORKLOAD_IDENTITY"),
                ));
            }
        }
    }
    if matches!(
        authenticator.as_str(),
        "SNOWFLAKE" | "USERNAME_PASSWORD_MFA"
    ) {
        require(&conn.params.password, 251_006, "Password is empty")?;
    }
    let account = require(&conn.params.account, 251_001, "Account must be specified")?;
    let label = |l: &str| {
        !l.is_empty()
            && l.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    if !account.split('.').all(label) {
        return Err(invalid(
            251_001,
            "Invalid account identifier: only letters, digits, '_', '-' and '.' (as a label separator) allowed; no slashes or backslashes".into(),
        ));
    }
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
    data.insert(
        "LOGIN_NAME".into(),
        json!(conn.params.user.as_deref().unwrap_or_default()),
    );
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
        data.insert("SECONDARY_ROLES".into(), json!(r.to_uppercase()));
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
    // Sent in every login request, also the one that finishes a Duo push.
    let base = data.clone();
    let token = || -> Result<Option<String>, DbError> {
        match &conn.params.token_file_path {
            // A token has no whitespace; editors and `echo` add a final newline.
            Some(p) => std::fs::read_to_string(p)
                .map(|t| Some(t.trim().to_string()))
                .map_err(|e| {
                    invalid(
                        251_005,
                        format!(
                            "Cannot read the token file '{p}': {e}. Set token_file_path (--token-file-path) to a readable text file that contains only the token."
                        ),
                    )
                }),
            None => Ok(conn.params.token.clone()),
        }
    };
    match authenticator.as_str() {
        // The MFA login also uses the default authenticator; the server asks for
        // the second factor in `nextAction`.
        "SNOWFLAKE" | "USERNAME_PASSWORD_MFA" => {
            data.insert("PASSWORD".into(), json!(conn.params.password));
        }
        "SNOWFLAKE_JWT" => {
            let der = conn
                .params
                .private_key
                .as_deref()
                .ok_or_else(|| invalid(251_008, "Private key is empty".into()))?;
            let user = conn.params.user.as_deref().unwrap_or_default();
            data.insert("AUTHENTICATOR".into(), json!("SNOWFLAKE_JWT"));
            data.insert(
                "TOKEN".into(),
                json!(keypair_jwt(&conn.account, user, der, 60)?),
            );
        }
        // The connector sends whatever it has, or null.
        "OAUTH" | "PROGRAMMATIC_ACCESS_TOKEN" => {
            data.insert("AUTHENTICATOR".into(), json!(authenticator));
            data.insert("TOKEN".into(), json!(token()?));
        }
        "WORKLOAD_IDENTITY" => {
            let p = &conn.params;
            let given = p.workload_identity_provider.clone().unwrap_or_default();
            let provider = given.to_uppercase();
            if !["", "AWS", "AZURE", "GCP", "OIDC"].contains(&provider.as_str()) {
                return Err(invalid(
                    251_017,
                    format!(
                        "Unknown workload_identity_provider: '{given}'. Expected one of: AWS, AZURE, GCP, OIDC"
                    ),
                ));
            }
            crate::wif::validate(
                &provider,
                &crate::wif::Options {
                    entra_resource: p.workload_identity_entra_resource.clone(),
                    impersonation_path: p.workload_identity_impersonation_path.clone(),
                    aws_use_outbound_token: p.workload_identity_aws_use_outbound_token,
                    host: p.workload_identity_host.clone(),
                },
            )?;
            // Before any cloud credential is fetched.
            crate::wif::verify_host(&conn.host)?;
            let t = if provider == "OIDC" {
                let t = require(
                    &token()?,
                    251_018,
                    "token must be provided if workload_identity_provider=OIDC",
                )?;
                crate::wif::validate_oidc_token(&t)?;
                t
            } else {
                wif_token(&provider, conn)?
            };
            data.insert("AUTHENTICATOR".into(), json!("WORKLOAD_IDENTITY"));
            data.insert("PROVIDER".into(), json!(provider));
            data.insert("TOKEN".into(), json!(t));
            data["CLIENT_ENVIRONMENT"]["WORKLOAD_IDENTITY_IMPERSONATION_PATH_LENGTH"] =
                json!(conn.params.workload_identity_impersonation_path.len());
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
            data["CLIENT_ENVIRONMENT"]["OAUTH_TYPE"] = json!(authenticator.to_lowercase());
        }
        _ => unreachable!("the authenticator was checked above"),
    }
    if conn.params.passcode_in_password {
        data.insert("EXT_AUTHN_DUO_METHOD".into(), json!("passcode"));
    } else if let Some(code) = &conn.params.passcode {
        data.insert("EXT_AUTHN_DUO_METHOD".into(), json!("passcode"));
        data.insert("PASSCODE".into(), json!(code));
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
    let path = format!("/session/v1/login-request?{query}");
    let mut v = conn.post(&path, &json!({ "data": data }), None)?;
    if matches!(
        v["data"]["nextAction"].as_str(),
        Some("EXT_AUTHN_DUO_ALL" | "EXT_AUTHN_DUO_PUSH_N_PASSCODE")
    ) {
        // Waits until the user approves the push.
        data.insert("EXT_AUTHN_DUO_METHOD".into(), json!("push"));
        let body = json!({ "data": data, "inFlightCtx": v["data"]["inFlightCtx"] });
        v = conn.post(&path, &body, None)?;
        if v["data"]["nextAction"] == "EXT_AUTHN_SUCCESS" {
            let body = json!({ "data": base, "inFlightCtx": v["data"]["inFlightCtx"] });
            v = conn.post(&path, &body, None)?;
        } else if v["data"]["token"].as_str().unwrap_or("").is_empty() {
            return Err(connect_failure(
                250_001,
                &format!(
                    "Failed to connect to DB. MFA authentication failed: {}:{}. {}",
                    conn.host,
                    conn.port(),
                    v["message"].as_str().unwrap_or("None")
                ),
            ));
        }
    }
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
        e.raw_msg = format!(
            "Failed to connect to DB: {}:{}. {}",
            conn.host,
            conn.port(),
            e.raw_msg
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

fn connect_failure(errno: i64, msg: &str) -> DbError {
    DbError::new(ErrorKind::Database, errno, Some("08001".into()), msg)
}

/// `AuthByPlugin._handle_failure`.
fn handle_failure(conn: &Connection, errno: i64, msg: &str) -> DbError {
    let msg = format!(
        "Failed to connect to DB: {}:{}, {msg}",
        conn.host,
        conn.port()
    );
    connect_failure(errno, &msg)
}

/// The URL of the first local callback that `wanted` accepts. Browsers also open
/// idle connections and ask for icons; those are not the callback.
fn receive_callback(
    listener: &TcpListener,
    timeout: Option<Duration>,
    title: &str,
    wanted: impl Fn(&url::Url, Option<&str>) -> bool,
) -> std::io::Result<Option<url::Url>> {
    let deadline = timeout.map(|t| Instant::now() + t);
    listener.set_nonblocking(true)?;
    loop {
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(e),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut lines = BufReader::new(stream.try_clone()?).lines();
        let Some(Ok(request)) = lines.next() else {
            continue;
        };
        let mut origin = None;
        for line in lines {
            let Ok(line) = line else { break };
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("origin")
            {
                origin = Some(value.trim().to_string());
            }
        }
        let url = request
            .strip_prefix("GET ")
            .and_then(|r| r.split_whitespace().next())
            .and_then(|target| url::Url::parse(&format!("http://localhost{target}")).ok())
            .filter(|u| wanted(u, origin.as_deref()));
        if url.is_none() {
            let _ = write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            continue;
        }
        let html = format!(
            "<!DOCTYPE html><html><head><meta charset=\"UTF-8\"/><link rel=\"icon\" href=\"data:,\"><title>{title}</title></head><body>Your identity was confirmed and propagated to Snowflake PythonConnector.\nYou can close this window now and go back where you started from.</body></html>"
        );
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        return Ok(url);
    }
}

fn query_value(url: &url::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, v)| k == key && !v.is_empty())
        .map(|(_, v)| v.into_owned())
}

/// The URL that the user pastes when no browser can open.
fn pasted_callback(prompt: &str) -> Option<url::Url> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    url::Url::parse(line.trim()).ok()
}

/// `AuthByWebBrowser`: open the SSO URL and wait for the redirect carrying the token.
fn external_browser(conn: &Connection) -> Result<(String, String), DbError> {
    let io = |e: std::io::Error| DbError::new(ErrorKind::Operational, 250_009, None, e.to_string());
    let host = std::env::var("SF_AUTH_SOCKET_ADDR").unwrap_or_else(|_| "localhost".into());
    let port = match std::env::var("SF_AUTH_SOCKET_PORT") {
        Ok(p) => p.trim().parse::<u16>().map_err(|_| {
            DbError::new(
                ErrorKind::Programming,
                251_007,
                None,
                format!("SF_AUTH_SOCKET_PORT is '{p}', but it must be a port number from 0 to 65535. Set it to a free port, or unset it."),
            )
        })?,
        Err(_) => 0,
    };
    // `localhost` can also mean ::1, but the connector listens on IPv4.
    let host = host.replace("localhost", "127.0.0.1");
    let listener = TcpListener::bind((host.as_str(), port)).map_err(io)?;
    let port = listener.local_addr().map_err(io)?.port().to_string();
    let body = json!({"data": {
        "ACCOUNT_NAME": conn.account,
        "LOGIN_NAME": conn.params.user,
        "PORT": port,
        "PROTOCOL": "http",
        "AUTHENTICATOR": "EXTERNALBROWSER",
        "CLIENT_APP_ID": CLIENT_APP_ID,
        "CLIENT_APP_VERSION": CLIENT_APP_VERSION,
        "BROWSER_MODE_REDIRECT_PORT": port,
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
    if !url::Url::parse(&sso).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
    {
        return Err(handle_failure(
            conn,
            251_007,
            &format!("The SSO URL provided {sso} is invalid"),
        ));
    }
    eprintln!(
        "Initiating login request with your identity provider. Press CTRL+C to abort and try again..."
    );
    eprintln!("Going to open: {sso} to authenticate...");
    let opened = open_browser(&sso);
    let force =
        std::env::var("SNOWFLAKE_AUTH_FORCE_SERVER").is_ok_and(|v| v.eq_ignore_ascii_case("true"));
    let callback = if opened || force {
        if opened {
            eprintln!(
                "A browser window should have opened for you to complete the login. If you can't see it, check existing browser windows, or your OS settings."
            );
        }
        let server = url::Url::parse(&conn.base_url).ok().map(|u| u.origin());
        receive_callback(
            &listener,
            None,
            "SAML Response for Snowflake",
            |u, origin| {
                let trusted = match origin {
                    None | Some("null") => true,
                    Some(o) => url::Url::parse(o).ok().map(|o| o.origin()) == server,
                };
                trusted && query_value(u, "token").is_some()
            },
        )
        .map_err(io)?
    } else {
        pasted_callback(
            "We were unable to open a browser window for you, please open the url above manually then paste the URL you are redirected to into the terminal.\nEnter the URL the SSO URL redirected you to: ",
        )
    };
    match callback.and_then(|u| query_value(&u, "token")) {
        Some(token) => Ok((token, proof)),
        None => Err(handle_failure(
            conn,
            250_008,
            "Unable to open a browser in this environment and SSO URL contained no token",
        )),
    }
}

/// Open `url` in the browser; false when the opener is missing or fails.
pub fn open_browser(url: &str) -> bool {
    let cmd = if cfg!(target_os = "macos") {
        ("open", vec![url.to_string()])
    } else if cfg!(windows) {
        // `start` reads `&` as a command separator, so a URL with a query needs this.
        (
            "rundll32",
            vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
        )
    } else {
        ("xdg-open", vec![url.to_string()])
    };
    let Ok(mut child) = std::process::Command::new(cmd.0)
        .args(cmd.1)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    // Some openers stay alive as the browser; those that fail exit at once.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return false,
        }
    }
    true
}

/// `AuthByOauthCode` and `AuthByOauthCredentials`, without the token cache.
fn oauth_token(conn: &Connection, authenticator: &str) -> Result<String, DbError> {
    let p = &conn.params;
    let invalid = |errno: i64, m: &str| DbError::new(ErrorKind::Programming, errno, None, m);
    let endpoint = |given: &Option<String>, path: &str| {
        given
            .clone()
            .unwrap_or_else(|| format!("https://{{host}}:{{port}}/{path}"))
            .replace("{host}", &conn.host)
            .replace("{port}", &conn.port().to_string())
    };
    let token_url = endpoint(&p.oauth_token_request_url, "oauth/token-request");
    let code_flow = authenticator == "OAUTH_AUTHORIZATION_CODE";
    let auth_url = endpoint(&p.oauth_authorization_url, "oauth/authorize");
    if code_flow && !auth_url.starts_with("https://") {
        return Err(invalid(
            251_007,
            "251007: OAuth supports only authorization urls that use 'https' scheme",
        ));
    }
    let mut client_id = p.oauth_client_id.clone().unwrap_or_default();
    let mut secret = p.oauth_client_secret.clone().unwrap_or_default();
    let host = conn.host.to_lowercase();
    let snowflake_idp = [&auth_url, &token_url].into_iter().all(|u| {
        u.is_empty() || url::Url::parse(u).is_ok_and(|u| u.host_str() == Some(host.as_str()))
    });
    if code_flow && secret.is_empty() && snowflake_idp && p.oauth_client_secret.is_none() {
        // Snowflake OAuth for local applications.
        client_id = "LOCAL_APPLICATION".into();
        secret = client_id.clone();
    }
    // The code flow raises before the connection exists, which repeats the code.
    for (value, name, errno) in [
        (&client_id, "client_id", 251_013),
        (&secret, "client_secret", 251_020),
    ] {
        if value.is_empty() {
            let msg = format!("Oauth code flow requirement '{name}' is empty");
            return Err(invalid(
                errno,
                &if code_flow {
                    format!("{errno}: {msg}")
                } else {
                    msg
                },
            ));
        }
    }
    let scope = p
        .oauth_scope
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| p.role.as_ref().map(|r| format!("session:role:{r}")))
        .unwrap_or_default();
    let mut form: Vec<(&str, String)> = Vec::new();
    if code_flow {
        let (code, redirect, verifier) = authorization_code(conn, &auth_url, &client_id, &scope)?;
        form.push(("grant_type", "authorization_code".into()));
        form.push(("code", code));
        form.push(("redirect_uri", redirect));
        if !p.oauth_disable_pkce {
            form.push(("code_verifier", verifier));
        }
    } else {
        form.push(("grant_type", "client_credentials".into()));
        form.push(("scope", scope));
        if p.oauth_credentials_in_body {
            form.push(("client_id", client_id.clone()));
            form.push(("client_secret", secret.clone()));
        }
    }
    let body: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{secret}"));
    let response = conn
        .agent
        .post(&token_url)
        .set("Authorization", &format!("Basic {basic}"))
        .set("Accept", "application/json")
        .set(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .send_string(&body);
    let response = match response {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => {
            return Err(handle_failure(
                conn,
                250_003,
                &format!("Failed to request OAuth access token from IdP: {e}"),
            ));
        }
    };
    serde_json::from_reader::<_, Value>(response.into_reader())
        .ok()
        .and_then(|v| v["access_token"].as_str().map(str::to_string))
        .ok_or_else(|| {
            handle_failure(
                conn,
                250_006,
                "Invalid HTTP request from web browser. Idp authentication could have failed.",
            )
        })
}

/// The browser step of the code flow: `(code, redirect_uri, PKCE verifier)`.
fn authorization_code(
    conn: &Connection,
    auth_url: &str,
    client_id: &str,
    scope: &str,
) -> Result<(String, String, String), DbError> {
    let p = &conn.params;
    let invalid = |m: String| DbError::new(ErrorKind::Programming, 251_007, None, m);
    let uri = |name: &str, given: &Option<String>| {
        let text = given
            .clone()
            .unwrap_or_else(|| "http://127.0.0.1".to_string());
        match url::Url::parse(&text) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.has_host() => Ok((text, u)),
            _ => Err(invalid(format!(
                "{name} is '{text}', but it must be an http or https URL. Set it to a URL such as http://127.0.0.1:8080."
            ))),
        }
    };
    let (mut redirect, redirect_url) = uri("oauth_redirect_uri", &p.oauth_redirect_uri)?;
    let local = matches!(redirect_url.host_str(), Some("localhost" | "127.0.0.1"));
    let listen = if p.oauth_socket_uri.is_some() {
        uri("oauth_socket_uri", &p.oauth_socket_uri)?.1
    } else if local {
        redirect_url.clone()
    } else {
        uri("oauth_socket_uri", &None)?.1
    };
    let wanted_port = if local { &redirect_url } else { &listen }
        .port()
        .unwrap_or(0);
    let address = match listen.host_str() {
        Some("localhost") | None => ("127.0.0.1", wanted_port),
        Some(host) => (host, wanted_port),
    };
    let io = |e: std::io::Error| {
        DbError::new(
            ErrorKind::Operational,
            250_009,
            None,
            format!(
                "Cannot listen for the OAuth callback on {}:{}: {e}. Set oauth_redirect_uri to http://127.0.0.1 or to a free local port.",
                address.0, address.1
            ),
        )
    };
    let listener = TcpListener::bind(address).map_err(io)?;
    let port = listener.local_addr().map_err(io)?.port();
    if local && redirect_url.port() != Some(port) {
        // Only the port changes; the rest of the URI must match what is registered.
        let start = redirect.find("://").map_or(0, |i| i + 3);
        let end = redirect[start..]
            .find(['/', '?', '#'])
            .map_or(redirect.len(), |i| start + i);
        let authority = format!("{}:{port}", redirect_url.host_str().unwrap_or("127.0.0.1"));
        redirect.replace_range(start..end, &authority);
    }
    let verifier = b64url(&rand::random::<[u8; 32]>());
    let state = b64url(&rand::random::<[u8; 32]>());
    let mut url = url::Url::parse(auth_url).map_err(|e| {
        invalid(format!(
            "oauth_authorization_url is '{auth_url}', but it must be an https URL ({e}). Correct it in the connection."
        ))
    })?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("state", &state);
        if !scope.is_empty() {
            q.append_pair("scope", scope);
        }
        if !p.oauth_disable_pkce {
            q.append_pair(
                "code_challenge",
                &b64url(&Sha256::digest(verifier.as_bytes())),
            )
            .append_pair("code_challenge_method", "S256");
        }
    }
    eprintln!(
        "Initiating login request with your identity provider. A browser window should have opened for you to complete the login. If you can't see it, check existing browser windows, or your OS settings. Press CTRL+C to abort and try again..."
    );
    let callback = if open_browser(url.as_str()) {
        let timeout = Duration::from_secs(120);
        receive_callback(
            &listener,
            Some(timeout),
            "OAuth Response for Snowflake",
            |u, _| query_value(u, "code").is_some() || query_value(u, "error").is_some(),
        )
        .map_err(io)?
        .ok_or_else(|| {
            handle_failure(
                conn,
                251_016,
                "Unable to receive the OAuth message within a given timeout. Please check the redirect URI and try again.",
            )
        })?
    } else {
        pasted_callback(&format!(
            "We were unable to open a browser window for you, please open the URL manually then paste the URL you are redirected to into the terminal:\n{url}\nEnter the URL the OAuth flow redirected you to: "
        ))
        .ok_or_else(|| {
            handle_failure(
                conn,
                250_008,
                "Unable to open a browser in this environment and OAuth URL contained no code",
            )
        })?
    };
    if let Some(error) = query_value(&callback, "error") {
        let detail = query_value(&callback, "error_description")
            .map_or_else(|| ".".into(), |d| format!(": {d}"));
        return Err(handle_failure(
            conn,
            251_015,
            &format!("Oauth callback returned an {error} error{detail}"),
        ));
    }
    let Some(code) = query_value(&callback, "code") else {
        return Err(handle_failure(
            conn,
            250_008,
            "Unable to open a browser in this environment and OAuth URL contained no code",
        ));
    };
    if query_value(&callback, "state").as_deref() != Some(state.as_str()) {
        return Err(handle_failure(
            conn,
            251_014,
            "State changed during OAuth process.",
        ));
    }
    Ok((code, redirect, verifier))
}
