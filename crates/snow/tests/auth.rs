//! Authentication at the binary's boundary: what `connection add` stores and
//! what each authenticator sends to the server.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<(String, String)>>>;

/// A server that records `(path, body)` and answers with `reply(path, earlier logins)`.
fn serve(reply: fn(&str, usize) -> Value) -> (u16, Requests) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Requests::default();
    let log = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let target = line.split_whitespace().nth(1).unwrap_or("/");
            let path = target.split('?').next().unwrap().to_string();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some(v) = header.to_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let mut log = log.lock().unwrap();
            let logins = log.iter().filter(|(p, _)| p.contains("login")).count();
            log.push((path.clone(), String::from_utf8_lossy(&body).into_owned()));
            let text = reply(&path, logins).to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            )
            .unwrap();
        }
    });
    (port, requests)
}

fn accept(path: &str, _: usize) -> Value {
    match path {
        "/token" => json!({"access_token": "AT"}),
        "/oidc" => json!({"value": "oidc.from.github"}),
        "/session/v1/login-request" => {
            json!({"success": true, "data": {"token": "t", "masterToken": "m", "sessionInfo": {}}})
        }
        _ => json!({"success": true, "data": {"rowtype": [], "rowset": [], "queryId": "q"}}),
    }
}

fn logins(requests: &Requests) -> Vec<Value> {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "/session/v1/login-request")
        .map(|(_, b)| serde_json::from_str(b).unwrap())
        .collect()
}

fn snow(home: &Path, config: &str, args: &[&str], env: &[(&str, &str)], stdin: &str) -> Output {
    let file = home.join("config.toml");
    std::fs::write(
        &file,
        format!("[cli]\nignore_new_version_warning = true\n{config}"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_snow"))
        .args(args)
        .current_dir(home)
        .env("SNOWFLAKE_HOME", home)
        .env("COLUMNS", "400")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The command can fail before it reads its input.
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    child.wait_with_output().unwrap()
}

/// `snow sql` on a connection with `fields` (TOML lines) and the test server.
fn sql(home: &Path, port: u16, fields: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let config = format!(
        "[connections.c]\nhost = \"127.0.0.1\"\nport = {port}\nprotocol = \"http\"\n{fields}\n"
    );
    let args = [&["sql", "-c", "c", "-q", "select 1"], args].concat();
    snow(home, &config, &args, env, "")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn connection_add_stores_the_secret_where_the_authenticator_reads_it() {
    let prompts = |password: &str, authenticator: &str| {
        // Name, account, user, password, role, warehouse, database, schema, host,
        // port, protocol, region, authenticator, then four more prompts.
        format!("c\nacc\nme\n{password}\n\n\n\n\n\n\n\n\n{authenticator}\n\n\n\n\n")
    };
    let pat = "PROGRAMMATIC_ACCESS_TOKEN";
    let typed = prompts("SECRET", "programmatic_access_token");
    let flagged = prompts("SECRET", "").replacen("\n\n\n\n\n\n", "\n\n\n\n\n", 1);
    let flag = |authenticator| vec!["-A", authenticator, "-p", "SECRET", "--no-interactive"];
    for (args, stdin, stored, unused) in [
        (vec![], typed.as_str(), "token", "password"),
        (vec!["-A", pat], flagged.as_str(), "token", "password"),
        (flag(pat), "", "token", "password"),
        (flag("oauth"), "", "token", "password"),
        (flag("SNOWFLAKE"), "", "password", "token"),
        (flag("username_password_mfa"), "", "password", "token"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let mut argv = vec!["connection", "add"];
        if stdin.is_empty() {
            argv.extend(["-n", "c", "-a", "acc"]);
        }
        argv.extend(&args);
        let out = snow(home.path(), "", &argv, &[], stdin);
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        let config = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(
            config.contains(&format!("{stored} = \"SECRET\"")),
            "{args:?}: {config}"
        );
        assert!(
            !config.contains(&format!("{unused} = ")),
            "{args:?}: {config}"
        );
    }
}

#[test]
fn login_requests() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("tok"), "PAT\n").unwrap();
    let (port, requests) = serve(accept);
    let login = |fields: &str, args: &[&str], env: &[(&str, &str)]| {
        requests.lock().unwrap().clear();
        let out = sql(home.path(), port, fields, args, env);
        assert!(out.status.success(), "{fields}: {}", text(&out));
        logins(&requests).remove(0)["data"].clone()
    };
    let mfa = "account = \"a\"\nuser = \"u\"\npassword = \"p\"\nauthenticator = \"USERNAME_PASSWORD_MFA\"";

    let d = login(mfa, &[], &[]);
    assert_eq!(d["PASSWORD"], "p");
    assert_eq!(d["AUTHENTICATOR"], Value::Null);
    assert_eq!(d["EXT_AUTHN_DUO_METHOD"], Value::Null);
    // The passcode applies to the default authenticator too.
    for fields in [mfa, "account = \"a\"\nuser = \"u\"\npassword = \"p\""] {
        let d = login(fields, &["--mfa-passcode", "123456"], &[]);
        assert_eq!(d["PASSCODE"], "123456");
        assert_eq!(d["EXT_AUTHN_DUO_METHOD"], "passcode");
    }

    let d = login(
        "account = \"org-ext.global\"\nauthenticator = \"PROGRAMMATIC_ACCESS_TOKEN\"\ntoken_file_path = \"tok\"\nsecondary_roles = \"none\"",
        &[],
        &[],
    );
    assert_eq!(d["TOKEN"], "PAT", "the final newline of a file is not sent");
    assert_eq!(d["ACCOUNT_NAME"], "org");
    assert_eq!(d["SECONDARY_ROLES"], "NONE");

    let d = login(
        &format!(
            "account = \"a\"\nauthenticator = \"OAUTH_CLIENT_CREDENTIALS\"\noauth_client_id = \"i\"\noauth_client_secret = \"s\"\noauth_credentials_in_body = true\noauth_token_request_url = \"http://{{host}}:{port}/token\""
        ),
        &[],
        &[],
    );
    assert_eq!(d["TOKEN"], "AT");
    assert_eq!(
        d["CLIENT_ENVIRONMENT"]["OAUTH_TYPE"],
        "oauth_client_credentials"
    );
    assert!(
        requests.lock().unwrap()[0]
            .1
            .ends_with("client_id=i&client_secret=s")
    );
}

#[test]
fn key_pair_login_accepts_ec_keys() {
    // Made with `openssl ecparam -genkey`; the fingerprint is from `openssl dgst`.
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("key.p8"), EC_KEY).unwrap();
    let (port, requests) = serve(accept);
    let out = sql(
        home.path(),
        port,
        "account = \"a\"\nuser = \"u\"\nauthenticator = \"SNOWFLAKE_JWT\"\nprivate_key_file = \"key.p8\"",
        &[],
        &[],
    );
    assert!(out.status.success(), "{}", text(&out));
    let token = logins(&requests)[0]["data"]["TOKEN"]
        .as_str()
        .unwrap()
        .to_string();
    let part = |i: usize| -> Value {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token.split('.').nth(i).unwrap())
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    };
    assert_eq!(part(0)["alg"], "ES256");
    assert_eq!(part(1)["iss"], format!("A.U.SHA256:{EC_FINGERPRINT}"));
    assert_eq!(
        part(1)["exp"].as_i64().unwrap() - part(1)["iat"].as_i64().unwrap(),
        60
    );
}

const EC_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgXpaP6KirG2SxoIog\neTqh/nLIEj82ye2MgOXsxbyfE5KhRANCAAQ+5Z2IeP5fD0UHiyryLAjVjabmsNDa\nmzrCuaQqI7VaC5pR9dWAHSGyO/Y1oUXebBVXzx9lgkreFv699mgrLM6m\n-----END PRIVATE KEY-----\n";
const EC_FINGERPRINT: &str = "oNqfKovo7K17rzQp3MG/x1rngAPIrXXN1H6QH4vZO/w=";

#[test]
fn duo_push_continues_the_login() {
    let (port, requests) = serve(|path, logins| match (path, logins) {
        ("/session/v1/login-request", 0) => {
            json!({"success": false, "data": {"nextAction": "EXT_AUTHN_DUO_ALL", "inFlightCtx": "c1"}})
        }
        ("/session/v1/login-request", 1) => {
            json!({"success": true, "data": {"nextAction": "EXT_AUTHN_SUCCESS", "inFlightCtx": "c2"}})
        }
        _ => accept(path, logins),
    });
    let home = tempfile::tempdir().unwrap();
    let out = sql(
        home.path(),
        port,
        "account = \"a\"\nuser = \"u\"\npassword = \"p\"",
        &[],
        &[],
    );
    assert!(out.status.success(), "{}", text(&out));
    let sent = logins(&requests);
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1]["inFlightCtx"], "c1");
    assert_eq!(sent[1]["data"]["EXT_AUTHN_DUO_METHOD"], "push");
    assert_eq!(sent[2]["inFlightCtx"], "c2");
    assert_eq!(sent[2]["data"]["PASSWORD"], Value::Null);
}

#[test]
fn workload_identity_reads_the_github_token_and_checks_the_host() {
    let (port, requests) = serve(accept);
    let home = tempfile::tempdir().unwrap();
    let fields = "account = \"a\"\nauthenticator = \"WORKLOAD_IDENTITY\"\nworkload_identity_provider = \"OIDC\"";
    let url = format!("http://127.0.0.1:{port}/oidc");
    let github = [
        ("GITHUB_ACTIONS", "true"),
        ("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "t"),
        ("ACTIONS_ID_TOKEN_REQUEST_URL", url.as_str()),
    ];
    let out = sql(home.path(), port, fields, &[], &github);
    assert!(
        text(&out).contains("requires a recognized Snowflake host"),
        "{}",
        text(&out)
    );

    let trusted = [
        &github[..],
        &[("SNOWFLAKE_WIF_ALLOWED_HOST_SUFFIXES", "127.0.0.1")],
    ]
    .concat();
    // The stub's value is not a JWT, so reaching its check proves it was used.
    let out = sql(home.path(), port, fields, &[], &trusted);
    assert!(text(&out).contains("Invalid JWT token"), "{}", text(&out));
    assert!(logins(&requests).is_empty());
}

#[test]
fn failed_login_explains_a_token_stored_as_the_password() {
    let (port, _) = serve(
        |_, _| json!({"success": false, "code": "390100", "message": "rejected", "data": null}),
    );
    let home = tempfile::tempdir().unwrap();
    let out = sql(
        home.path(),
        port,
        "account = \"a\"\nauthenticator = \"PROGRAMMATIC_ACCESS_TOKEN\"\npassword = \"PAT\"",
        &[],
        &[],
    );
    let text = text(&out);
    assert!(
        text.contains("rejected") && text.contains("rename 'password' to 'token'"),
        "{text}"
    );
}

#[test]
fn invalid_credentials_are_refused_before_login() {
    let (port, requests) = serve(|path, n| match path {
        "/session/heartbeat" => json!({"success": false, "code": "390104", "data": null}),
        _ => accept(path, n),
    });
    let home = tempfile::tempdir().unwrap();
    for (fields, message) in [
        (
            "account = \"a/b\"\nuser = \"u\"\npassword = \"p\"",
            "Invalid account identifier",
        ),
        (
            "account = \"a\"\nuser = \"u\"\npassword = \"p\"\nworkload_identity_provider = \"OIDC\"",
            "authenticator was not set to WORKLOAD_IDENTITY",
        ),
        (
            "account = \"a\"\nauthenticator = \"OAUTH_CLIENT_CREDENTIALS\"",
            "'client_id' is empty",
        ),
    ] {
        let out = sql(home.path(), port, fields, &[], &[]);
        assert!(!out.status.success());
        assert!(text(&out).contains(message), "{}", text(&out));
    }
    let port = port.to_string();
    let args = [
        "sql",
        "-x",
        "-q",
        "select 1",
        "--account",
        "a",
        "--host",
        "127.0.0.1",
        "--port",
        &port,
        "--protocol",
        "http",
        "--session-token",
        "s",
        "--master-token",
        "m",
    ];
    let out = snow(home.path(), "", &args, &[], "");
    assert!(
        text(&out).contains("Session and master tokens invalid"),
        "{}",
        text(&out)
    );
    assert!(logins(&requests).is_empty());
}

/// The "browser" is a script that asks for an icon, then follows the redirect.
#[cfg(unix)]
#[test]
fn oauth_code_flow_checks_the_state() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    for opener in ["open", "xdg-open"] {
        let file = bin.join(opener);
        std::fs::write(
            &file,
            "#!/usr/bin/env python3\nimport os, sys, subprocess\nfrom urllib.parse import urlparse, parse_qs\nq = parse_qs(urlparse(sys.argv[1]).query)\nuri = q['redirect_uri'][0]\nstate = os.environ.get('STATE') or q['state'][0]\nsubprocess.Popen(['curl', '-s', uri + '/favicon.ico', '--next', '-s', uri + '?code=CODE&state=' + state])\n",
        )
        .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let (port, requests) = serve(accept);
    let fields = format!(
        "account = \"a\"\nauthenticator = \"OAUTH_AUTHORIZATION_CODE\"\noauth_client_id = \"i\"\noauth_client_secret = \"s\"\noauth_authorization_url = \"https://idp.example/authorize\"\noauth_token_request_url = \"http://127.0.0.1:{port}/token\"\noauth_redirect_uri = \"http://localhost/callback\""
    );
    let out = sql(
        home.path(),
        port,
        &fields,
        &[],
        &[("PATH", &path), ("STATE", "forged")],
    );
    assert!(
        text(&out).contains("State changed during OAuth process"),
        "{}",
        text(&out)
    );
    assert!(requests.lock().unwrap().is_empty());

    let out = sql(home.path(), port, &fields, &[], &[("PATH", &path)]);
    assert!(out.status.success(), "{}", text(&out));
    let token_request = requests.lock().unwrap()[0].1.clone();
    assert!(token_request.contains("code=CODE"), "{token_request}");
    // The port is the listener's; the rest is the configured URI.
    assert!(
        token_request.contains("redirect_uri=http%3A%2F%2Flocalhost%3A")
            && token_request.contains("%2Fcallback"),
        "{token_request}"
    );
    assert_eq!(logins(&requests)[0]["data"]["TOKEN"], "AT");
}
