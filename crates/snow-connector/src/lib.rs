//! A Snowflake REST protocol client.
//!
//! This speaks the same endpoints as `snowflake-connector-python`:
//! `POST /session/v1/login-request` to authenticate,
//! `POST /queries/v1/query-request` to run a statement (polling the result
//! URL while the query is in progress), result chunk downloads, session
//! renewal and `POST /session?delete=true` on close. Results are requested
//! in JSON format, so every cell arrives as a string (or null) together
//! with the column metadata (`rowtype`); conversion to typed values happens
//! in `snow-api`.
#![forbid(unsafe_code)]

pub mod auth;
mod cloud;
mod sigv4;
mod storage;
pub mod transfer;
pub mod wif;
pub mod ws;

use serde_json::{Map, Value, json};
use std::io::Read;
use std::time::Duration;

pub const CLIENT_APP_ID: &str = "PythonConnector";
pub const CLIENT_APP_VERSION: &str = "4.8.1";

/// Response codes of a query that is still running.
const IN_PROGRESS: [&str; 2] = ["333333", "333334"];
/// Response code of an expired session token.
const SESSION_EXPIRED: &str = "390112";

/// An error as the Python connector reports it.
#[derive(Debug, Clone)]
pub struct DbError {
    pub errno: i64,
    pub sqlstate: Option<String>,
    pub sfqid: Option<String>,
    pub raw_msg: String,
    /// `ProgrammingError` (SQL) vs `DatabaseError`/`OperationalError` (login, network).
    pub kind: ErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Programming,
    Database,
    Operational,
    Forbidden,
}

impl DbError {
    pub fn new(
        kind: ErrorKind,
        errno: i64,
        sqlstate: Option<String>,
        msg: impl Into<String>,
    ) -> DbError {
        DbError {
            errno,
            sqlstate,
            sfqid: None,
            raw_msg: msg.into(),
            kind,
        }
    }
    /// `str(error)` at the default log level.
    pub fn msg(&self) -> String {
        self.msg_with(false)
    }
    /// With `verbose` (log level INFO/DEBUG) the query id is included.
    pub fn msg_with(&self, verbose: bool) -> String {
        if self.errno == -1 {
            return self.raw_msg.clone();
        }
        match &self.sqlstate {
            Some(s) if s != "n/a" => {
                if verbose {
                    format!(
                        "{:06} ({}): {}: {}",
                        self.errno,
                        s,
                        self.sfqid.as_deref().unwrap_or("None"),
                        self.raw_msg
                    )
                } else {
                    format!("{:06} ({}): {}", self.errno, s, self.raw_msg)
                }
            }
            _ => {
                if verbose {
                    format!("{:06}: {}: {}", self.errno, self.errno, self.raw_msg)
                } else {
                    format!("{:06}: {}", self.errno, self.raw_msg)
                }
            }
        }
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg())
    }
}

/// Connection parameters (the keys `snowflake.connector.connect` accepts).
#[derive(Debug, Clone, Default)]
pub struct ConnectParams {
    pub account: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub protocol: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub warehouse: Option<String>,
    pub role: Option<String>,
    pub authenticator: Option<String>,
    pub token: Option<String>,
    pub token_file_path: Option<String>,
    /// DER-encoded PKCS#8 private key for `SNOWFLAKE_JWT`.
    pub private_key: Option<Vec<u8>>,
    pub passcode: Option<String>,
    pub passcode_in_password: bool,
    pub client_request_mfa_token: bool,
    pub application: Option<String>,
    pub application_name: Option<String>,
    pub internal_application_name: Option<String>,
    pub internal_application_version: Option<String>,
    pub session_token: Option<String>,
    pub master_token: Option<String>,
    pub workload_identity_provider: Option<String>,
    pub workload_identity_entra_resource: Option<String>,
    pub workload_identity_impersonation_path: Vec<String>,
    pub workload_identity_aws_use_outbound_token: bool,
    pub workload_identity_host: Option<String>,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub oauth_authorization_url: Option<String>,
    pub oauth_token_request_url: Option<String>,
    pub oauth_redirect_uri: Option<String>,
    pub oauth_scope: Option<String>,
    pub oauth_disable_pkce: bool,
    pub client_store_temporary_credential: Option<bool>,
    pub client_session_keep_alive: Option<bool>,
    pub server_session_keep_alive: Option<bool>,
    pub secondary_roles: Option<String>,
    pub autocommit: Option<bool>,
    pub login_timeout: Option<u64>,
    pub session_parameters: Map<String, Value>,
}

/// One column of a result (`ResultMetadata`).
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    pub scale: Option<i64>,
}

impl Column {
    /// The connector's `type_code` for the column type.
    pub fn type_code(&self) -> i64 {
        match self.type_name.to_lowercase().as_str() {
            "fixed" => 0,
            "real" => 1,
            "text" => 2,
            "date" => 3,
            "timestamp" => 4,
            "variant" => 5,
            "timestamp_ltz" => 6,
            "timestamp_tz" => 7,
            "timestamp_ntz" => 8,
            "object" => 9,
            "array" => 10,
            "binary" => 11,
            "time" => 12,
            "boolean" => 13,
            "geography" => 14,
            "geometry" => 15,
            "vector" => 16,
            "map" => 17,
            "file" => 18,
            "decfloat" => 19,
            _ => -1,
        }
    }
}

/// The outcome of one statement.
#[derive(Debug, Clone, Default)]
pub struct Cursor {
    pub query: String,
    pub sfqid: Option<String>,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Option<String>>>,
    pub rowcount: Option<i64>,
    /// For PUT/GET the rows are produced by the client.
    pub raw: Value,
}

impl Cursor {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

/// Session state reported by the server.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub warehouse: Option<String>,
    pub role: Option<String>,
}

/// Who owns the server session; only an owned session is deleted on `close()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionState {
    /// `auth::login` has not succeeded yet.
    LoggingIn,
    /// Created by this connection's login.
    Owned,
    /// A `worker()` handle, or reused from `session_token` + `master_token`.
    Borrowed,
    Closed,
}

pub struct Connection {
    agent: ureq::Agent,
    pub base_url: String,
    pub host: String,
    pub account: String,
    pub user: Option<String>,
    token: String,
    master_token: Option<String>,
    pub session_id: Option<i64>,
    pub session: SessionInfo,
    pub parameters: Map<String, Value>,
    sequence: u64,
    state: SessionState,
    pub params: ConnectParams,
}

/// Lower-case hex digits of `b`.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn uuid4() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn user_agent() -> String {
    format!(
        "{CLIENT_APP_ID}/{CLIENT_APP_VERSION} ({}) CPython/3.12.0",
        std::env::consts::OS
    )
}

pub type HttpAgent = ureq::Agent;

/// A plain HTTPS client with a timeout (version checks, downloads).
pub fn http_agent(timeout: Duration) -> HttpAgent {
    ureq::AgentBuilder::new().timeout(timeout).build()
}

pub(crate) fn agent() -> ureq::Agent {
    let mut b = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(60))
        .timeout_read(Duration::from_secs(300))
        .user_agent(&user_agent());
    if let Ok(p) = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("https_proxy"))
        && let Ok(proxy) = ureq::Proxy::new(p)
    {
        b = b.proxy(proxy);
    }
    b.build()
}

fn read_body(resp: ureq::Response) -> Result<Vec<u8>, std::io::Error> {
    let mut buf = Vec::new();
    resp.into_reader().take(1 << 31).read_to_end(&mut buf)?;
    if buf.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::MultiGzDecoder::new(&buf[..]).read_to_end(&mut out)?;
        return Ok(out);
    }
    Ok(buf)
}

fn net_err(e: impl std::fmt::Display) -> DbError {
    DbError::new(ErrorKind::Operational, 251_000 + 1, None, format!("{e}"))
}

impl Connection {
    /// `snowflake.connector.connect(**params)`.
    pub fn connect(params: ConnectParams) -> Result<Connection, DbError> {
        let account_full = params.account.clone().unwrap_or_default();
        let host = params
            .host
            .clone()
            .unwrap_or_else(|| format!("{account_full}.snowflakecomputing.com"));
        let protocol = params.protocol.clone().unwrap_or_else(|| "https".into());
        let port = params.port.unwrap_or(443);
        let base_url = format!("{protocol}://{host}:{port}");
        let account = account_full.split('.').next().unwrap_or("").to_string();
        let mut conn = Connection {
            agent: agent(),
            base_url,
            host,
            account,
            user: params.user.clone(),
            token: String::new(),
            master_token: None,
            session_id: None,
            session: SessionInfo {
                database: params.database.clone(),
                schema: params.schema.clone(),
                warehouse: params.warehouse.clone(),
                role: params.role.clone(),
            },
            parameters: Map::new(),
            sequence: 0,
            state: SessionState::LoggingIn,
            params,
        };
        if let (Some(st), Some(mt)) = (
            conn.params.session_token.clone(),
            conn.params.master_token.clone(),
        ) {
            conn.token = st;
            conn.master_token = Some(mt);
            conn.state = SessionState::Borrowed;
            return Ok(conn);
        }
        auth::login(&mut conn)?;
        conn.state = SessionState::Owned;
        Ok(conn)
    }

    pub(crate) fn set_session(&mut self, data: &Value) {
        self.token = data["token"].as_str().unwrap_or_default().to_string();
        self.master_token = data["masterToken"].as_str().map(str::to_string);
        self.session_id = data["sessionId"].as_i64();
        self.merge_parameters(data);
        let si = &data["sessionInfo"];
        let s = |k: &str| si[k].as_str().map(str::to_string);
        self.session = SessionInfo {
            database: s("databaseName"),
            schema: s("schemaName"),
            warehouse: s("warehouseName"),
            role: s("roleName"),
        };
    }

    pub(crate) fn post(
        &self,
        path_and_query: &str,
        body: &Value,
        auth: Option<&str>,
    ) -> Result<Value, DbError> {
        let url = format!("{}{}", self.base_url, path_and_query);
        let mut req = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/snowflake");
        if let Some(t) = auth {
            req = req.set("Authorization", &format!("Snowflake Token=\"{t}\""));
        }
        let resp = match req.send_string(&body.to_string()) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => return Err(http_error(code, &url)),
            Err(e) => return Err(net_err(e)),
        };
        let bytes = read_body(resp).map_err(net_err)?;
        serde_json::from_slice(&bytes).map_err(net_err)
    }

    /// `connection.rest.request(url, method="get", client="rest")`: also serves
    /// non-`/api` endpoints such as `/monitoring/queries`.
    pub fn rest_get(&self, path_or_url: &str) -> Result<Value, DbError> {
        let url = if path_or_url.starts_with("http") {
            path_or_url.to_string()
        } else {
            format!("{}{}", self.base_url, path_or_url)
        };
        let resp = match self
            .agent
            .get(&url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/snowflake")
            .set(
                "Authorization",
                &format!("Snowflake Token=\"{}\"", self.token),
            )
            .call()
        {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => return Err(http_error(code, &url)),
            Err(e) => return Err(net_err(e)),
        };
        let bytes = read_body(resp).map_err(net_err)?;
        serde_json::from_slice(&bytes).map_err(net_err)
    }

    fn merge_parameters(&mut self, data: &Value) {
        for e in data["parameters"].as_array().into_iter().flatten() {
            if let Some(n) = e["name"].as_str() {
                self.parameters.insert(n.to_string(), e["value"].clone());
            }
        }
    }

    /// `_token_request(request_type)`: `POST /session/token-request` signed with `master`.
    fn token_request(&self, master: &str, request_type: &str) -> Result<Value, DbError> {
        let body = json!({ "oldSessionToken": self.token, "requestType": request_type });
        let path = format!(
            "/session/token-request?requestId={}&request_guid={}",
            uuid4(),
            uuid4()
        );
        let v = self.post(&path, &body, Some(master))?;
        if v["success"].as_bool() == Some(true) {
            Ok(v)
        } else {
            Err(response_error(&v, ErrorKind::Database))
        }
    }

    fn renew(&mut self) -> Result<(), DbError> {
        let Some(master) = self.master_token.clone() else {
            return Err(DbError::new(
                ErrorKind::Database,
                390112,
                None,
                "Session expired",
            ));
        };
        let v = self.token_request(&master, "RENEW")?;
        self.token = v["data"]["sessionToken"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if let Some(m) = v["data"]["masterToken"].as_str() {
            self.master_token = Some(m.to_string());
        }
        Ok(())
    }

    pub fn execute(&mut self, sql: &str) -> Result<Cursor, DbError> {
        self.execute_with(sql, false, None)
    }

    /// Run a statement with qmark (`?`) bindings, all bound as TEXT.
    pub fn execute_bound(&mut self, sql: &str, binds: &[String]) -> Result<Cursor, DbError> {
        self.execute_with(sql, false, Some(binds))
    }

    /// `execute_async` with qmark bindings.
    pub fn execute_async_bound(&mut self, sql: &str, binds: &[String]) -> Result<Cursor, DbError> {
        self.execute_with(sql, true, Some(binds))
    }

    /// Submit a statement without waiting for its result (`_no_results=True`).
    pub fn execute_async(&mut self, sql: &str) -> Result<Cursor, DbError> {
        self.execute_with(sql, true, None)
    }

    fn execute_with(
        &mut self,
        sql: &str,
        async_exec: bool,
        binds: Option<&[String]>,
    ) -> Result<Cursor, DbError> {
        if transfer::is_file_transfer(sql) {
            return transfer::execute(self, sql);
        }
        let v = self.query_request(sql, async_exec, binds)?;
        self.cursor_from(sql, v)
    }

    /// `binds` become server-side `bindings`, all bound as TEXT.
    pub(crate) fn query_request(
        &mut self,
        sql: &str,
        async_exec: bool,
        binds: Option<&[String]>,
    ) -> Result<Value, DbError> {
        self.sequence += 1;
        let mut parameters = Map::new();
        parameters.insert("PYTHON_CONNECTOR_QUERY_RESULT_FORMAT".into(), json!("JSON"));
        let mut body = json!({
            "sqlText": sql,
            "asyncExec": async_exec,
            "sequenceId": self.sequence,
            "querySubmissionTime": chrono::Utc::now().timestamp_millis(),
            "isInternal": false,
            "describeOnly": false,
            "parameters": parameters,
        });
        if let Some(binds) = binds {
            body["bindings"] = binds
                .iter()
                .enumerate()
                .map(|(i, b)| ((i + 1).to_string(), json!({"type": "TEXT", "value": b})))
                .collect::<Map<String, Value>>()
                .into();
        }
        let request_id = uuid4();
        let path = format!(
            "/queries/v1/query-request?requestId={request_id}&request_guid={}",
            uuid4()
        );
        let mut v = self.post(&path, &body, Some(&self.token.clone()))?;
        if v["code"].as_str() == Some(SESSION_EXPIRED) {
            self.renew()?;
            v = self.post(&path, &body, Some(&self.token.clone()))?;
        }
        let mut delay = 50u64;
        while v["code"].as_str().is_some_and(|c| IN_PROGRESS.contains(&c)) && !async_exec {
            let url = v["data"]["getResultUrl"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            std::thread::sleep(Duration::from_millis(delay));
            delay = (delay * 2).min(1000);
            v = self.rest_get(&url)?;
            if v["code"].as_str() == Some(SESSION_EXPIRED) {
                self.renew()?;
                v = self.rest_get(&url)?;
            }
        }
        Ok(v)
    }

    pub fn cursor_from(&mut self, sql: &str, v: Value) -> Result<Cursor, DbError> {
        if v["success"].as_bool() != Some(true)
            && !v["code"].as_str().is_some_and(|c| IN_PROGRESS.contains(&c))
        {
            let mut e = response_error(&v, ErrorKind::Programming);
            e.sfqid = v["data"]["queryId"].as_str().map(str::to_string);
            return Err(e);
        }
        let data = &v["data"];
        self.update_session(data);
        let columns: Vec<Column> = data["rowtype"]
            .as_array()
            .map(|a| a.iter().map(column_from).collect())
            .unwrap_or_default();
        let mut rows = parse_rowset(&data["rowset"]);
        if let Some(chunks) = data["chunks"].as_array() {
            for c in chunks {
                let url = c["url"].as_str().unwrap_or_default();
                rows.extend(self.download_chunk(url, data)?);
            }
        }
        let rowcount = data["total"].as_i64().or(Some(rows.len() as i64));
        Ok(Cursor {
            query: sql.to_string(),
            sfqid: data["queryId"].as_str().map(str::to_string),
            columns,
            rows,
            rowcount,
            raw: v,
        })
    }

    fn update_session(&mut self, data: &Value) {
        let s = &mut self.session;
        for (key, slot) in [
            ("finalDatabaseName", &mut s.database),
            ("finalSchemaName", &mut s.schema),
            ("finalWarehouseName", &mut s.warehouse),
        ] {
            match data.get(key) {
                Some(Value::String(v)) => *slot = Some(v.clone()),
                Some(Value::Null) => *slot = None,
                _ => {}
            }
        }
        // A null role is never cleared.
        if let Some(r) = data["finalRoleName"].as_str() {
            s.role = Some(r.to_string());
        }
        self.merge_parameters(data);
    }

    fn download_chunk(&self, url: &str, data: &Value) -> Result<Vec<Vec<Option<String>>>, DbError> {
        let mut req = self.agent.get(url);
        if let Some(h) = data["chunkHeaders"].as_object() {
            for (k, v) in h {
                req = req.set(k, v.as_str().unwrap_or_default());
            }
        } else if let Some(qrmk) = data["qrmk"].as_str() {
            req = req
                .set("x-amz-server-side-encryption-customer-algorithm", "AES256")
                .set("x-amz-server-side-encryption-customer-key", qrmk);
        }
        let resp = req.call().map_err(net_err)?;
        let bytes = read_body(resp).map_err(net_err)?;
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        text.insert(0, '[');
        text.push(']');
        let v: Value = serde_json::from_str(&text).map_err(net_err)?;
        Ok(parse_rowset(&v))
    }

    /// The raw `/queries/<id>/result` response once the query has finished.
    pub fn result_response(&mut self, sfqid: &str) -> Result<Value, DbError> {
        let mut v = self.rest_get(&format!("/queries/{sfqid}/result"))?;
        let mut delay = 100;
        while v["code"].as_str().is_some_and(|c| IN_PROGRESS.contains(&c)) {
            std::thread::sleep(Duration::from_millis(delay));
            delay = (delay * 2).min(1000);
            v = self.rest_get(&format!("/queries/{sfqid}/result"))?;
        }
        Ok(v)
    }

    /// `get_query_status(sfqid)`: the monitoring status name (`NO_DATA` when absent).
    pub fn query_status(&self, sfqid: &str) -> Result<String, DbError> {
        let v = self.rest_get(&format!("/monitoring/queries/{sfqid}"))?;
        Ok(v["data"]["queries"]
            .as_array()
            .and_then(|q| q.first())
            .and_then(|q| q["status"].as_str())
            .unwrap_or("NO_DATA")
            .to_string())
    }

    /// `cursor.query_result(sfqid)`: the result of an earlier (async) query.
    pub fn query_result(&mut self, sfqid: &str) -> Result<Cursor, DbError> {
        let v = self.rest_get(&format!("/queries/{sfqid}/result"))?;
        self.cursor_from("", v)
    }

    /// `connection.rest.request(url, method, body)` for `/api/v2` calls.
    pub fn rest(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), DbError> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self
            .agent
            .request(method, &url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .set(
                "Authorization",
                &format!("Snowflake Token=\"{}\"", self.token),
            );
        req = req.set("X-Snowflake-Authorization-Token-Type", "");
        let res = match body {
            Some(b) => req.send_string(&b.to_string()),
            None => req.call(),
        };
        match res {
            Ok(r) => {
                let code = r.status();
                let bytes = read_body(r).map_err(net_err)?;
                Ok((code, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
            }
            Err(ureq::Error::Status(code, r)) => {
                let bytes = read_body(r).unwrap_or_default();
                Ok((code, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
            }
            Err(e) => Err(net_err(e)),
        }
    }

    /// `_token_request("ISSUE")`: a fresh session token for registry logins.
    pub fn issue_token(&mut self) -> Result<Value, DbError> {
        let Some(master) = self.master_token.clone() else {
            return Err(DbError::new(
                ErrorKind::Database,
                -1,
                None,
                "Failed to connect to Snowflake to retrieve token.",
            ));
        };
        self.token_request(&master, "ISSUE")
    }

    pub fn rest_text(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, String), DbError> {
        let url = format!("{}{}", self.base_url, path);
        let req = self
            .agent
            .request(method, &url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set(
                "Authorization",
                &format!("Snowflake Token=\"{}\"", self.token),
            );
        let res = match body {
            Some(b) => req.send_string(&b.to_string()),
            None => req.call(),
        };
        match res {
            Ok(r) => {
                let code = r.status();
                Ok((
                    code,
                    String::from_utf8_lossy(&read_body(r).map_err(net_err)?).into_owned(),
                ))
            }
            Err(ureq::Error::Status(code, r)) => Ok((code, r.into_string().unwrap_or_default())),
            Err(e) => Err(net_err(e)),
        }
    }

    pub fn parameter(&self, name: &str) -> Option<&Value> {
        self.parameters.get(name)
    }

    /// A second handle on this session for parallel work (the connector's
    /// thread-pool uploads share one connection); dropping it keeps the session.
    pub fn worker(&self) -> Connection {
        Connection {
            agent: self.agent.clone(),
            base_url: self.base_url.clone(),
            host: self.host.clone(),
            account: self.account.clone(),
            user: self.user.clone(),
            token: self.token.clone(),
            master_token: self.master_token.clone(),
            session_id: self.session_id,
            session: self.session.clone(),
            parameters: self.parameters.clone(),
            sequence: self.sequence + 1_000_000,
            state: SessionState::Borrowed,
            params: self.params.clone(),
        }
    }

    /// `POST /session?delete=true` when this connection owns the session.
    pub fn close(&mut self) {
        if std::mem::replace(&mut self.state, SessionState::Closed) != SessionState::Owned {
            return;
        }
        let path = format!(
            "/session?delete=true&request_id={}&request_guid={}",
            uuid4(),
            uuid4()
        );
        let _ = self.post(&path, &json!({}), Some(&self.token.clone()));
    }

    pub fn send_telemetry_checked(&self, logs: Value) -> bool {
        matches!(self.state, SessionState::Owned | SessionState::Borrowed)
            && self
                .post(
                    "/telemetry/send",
                    &json!({ "logs": logs }),
                    Some(&self.token),
                )
                .is_ok()
    }

    pub fn send_telemetry(&self, logs: Value) {
        let _ = self.send_telemetry_checked(logs);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

fn http_error(code: u16, url: &str) -> DbError {
    let kind = match code {
        403 => ErrorKind::Forbidden,
        _ => ErrorKind::Operational,
    };
    let path = url::Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_default();
    DbError::new(
        kind,
        250_001,
        Some("08001".into()),
        format!("{code}: {code} {}: for url: {path}", status_text(code)),
    )
}

/// `http.HTTPStatus(code).phrase`.
pub fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        423 => "Locked",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

pub(crate) fn response_error(v: &Value, kind: ErrorKind) -> DbError {
    let code = v["code"]
        .as_str()
        .and_then(|c| c.parse::<i64>().ok())
        .or_else(|| v["code"].as_i64())
        .unwrap_or(-1);
    let msg = v["message"].as_str().unwrap_or("Unknown error").to_string();
    let sqlstate = v["data"]["sqlState"].as_str().map(str::to_string);
    DbError {
        errno: code,
        sqlstate,
        sfqid: v["data"]["queryId"].as_str().map(str::to_string),
        raw_msg: msg,
        kind,
    }
}

fn column_from(c: &Value) -> Column {
    Column {
        name: c["name"].as_str().unwrap_or_default().to_string(),
        type_name: c["type"].as_str().unwrap_or("text").to_string(),
        scale: c["scale"].as_i64(),
    }
}

fn parse_rowset(v: &Value) -> Vec<Vec<Option<String>>> {
    v.as_array()
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    r.as_array()
                        .map(|cells| {
                            cells
                                .iter()
                                .map(|c| match c {
                                    Value::Null => None,
                                    Value::String(s) => Some(s.clone()),
                                    other => Some(other.to_string()),
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}
