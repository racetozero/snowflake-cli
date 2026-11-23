//! Cloud stage access for PUT/GET (`s3_storage_client.py`, `azure_storage_client.py`,
//! `gcs_storage_client.py`, `encryption_util.py`): uploads and downloads with
//! Snowflake client-side encryption (AES-CBC data key wrapped with AES-ECB
//! under the query stage master key), S3 multipart / Azure block uploads and
//! ranged downloads past 64 MiB, transient-status retries, credential renewal
//! by re-running the command, and S3 region redirects.
//!
//! These paths cannot be exercised by the parity harness (it serves LOCAL_FS
//! stages only); the request layout follows the Python connector line by line.

use crate::{Connection, DbError, ErrorKind, hex};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone)]
struct Material {
    master_key: Vec<u8>,
    query_id: String,
    smk_id: String,
}

type Headers = Vec<(String, String)>;

/// Re-runs the PUT/GET command and returns the response JSON (credential renewal).
pub(crate) type Renew<'a> = &'a mut dyn FnMut(&str) -> Result<Value, DbError>;

enum Kind {
    S3 {
        path: String,
        region: String,
        endpoint: String,
        key_id: String,
        secret: String,
        token: String,
    },
    Azure {
        account: String,
        endpoint: String,
        container: String,
        path: String,
        sas: String,
    },
    Gcs {
        url_base: String,
        path: String,
        token: Option<String>,
        presigned: Vec<String>,
    },
}

pub(crate) struct Remote {
    kind: Kind,
    /// The PUT/GET statement, re-run to renew expired credentials.
    command: String,
    /// `multipart_threshold` and the default chunk size (tests use small ones).
    threshold: u64,
    chunk: u64,
    agent: ureq::Agent,
    upload_material: Option<Material>,
    download_materials: Vec<Option<Material>>,
}

/// `multipart_threshold` (historical 64 MiB).
const MULTIPART_THRESHOLD: u64 = 67_108_864;
const HEXDIGITS: &[u8] = b"0123456789abcdefABCDEF";

/// `_chunk_size_calculator(file_size)`: at most 10000 parts.
fn chunk_size(file_size: u64, default: u64) -> u64 {
    if file_size.div_ceil(default) > 10_000 {
        file_size.div_ceil(10_000)
    } else {
        default
    }
}

/// GCS headers plus the current bearer token (renewal replaces it).
fn gcs_auth(r: &Remote, base: &[(String, String)]) -> Headers {
    let mut h = base.to_vec();
    if let Kind::Gcs { token: Some(t), .. } = &r.kind {
        h.push(("Authorization".into(), format!("Bearer {t}")));
    }
    h
}

pub(crate) fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(
        xml[start..end]
            .replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn err(m: impl Into<String>) -> DbError {
    DbError::new(ErrorKind::Operational, 253003, None, m)
}

fn material(v: &Value) -> Option<Material> {
    let key = v["queryStageMasterKey"].as_str()?;
    Some(Material {
        master_key: B64.decode(key).ok()?,
        query_id: v["queryId"].as_str().unwrap_or_default().to_string(),
        smk_id: match &v["smkId"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        },
    })
}

fn split_location(loc: &str) -> (String, String) {
    match loc.split_once('/') {
        Some((b, p)) => {
            let p = if !p.is_empty() && !p.ends_with('/') {
                format!("{p}/")
            } else {
                p.to_string()
            };
            (b.to_string(), p)
        }
        None => (loc.to_string(), String::new()),
    }
}

fn quote(s: &str) -> String {
    // urllib.parse.quote (safe="/")
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn pkcs7(data: &[u8]) -> Vec<u8> {
    let n = 16 - data.len() % 16;
    let mut v = data.to_vec();
    v.extend(std::iter::repeat_n(n as u8, n));
    v
}

fn unpad(data: &[u8]) -> Vec<u8> {
    let n = data.last().copied().unwrap_or(0) as usize;
    if n == 0 || n > 16 || n > data.len() {
        return data.to_vec();
    }
    data[..data.len() - n].to_vec()
}

fn aes_ecb(key: &[u8], data: &[u8], encrypt: bool) -> Result<Vec<u8>, DbError> {
    use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
    let mut out = data.to_vec();
    macro_rules! run {
        ($c:ty) => {{
            let c = <$c>::new_from_slice(key).map_err(|e| err(e.to_string()))?;
            for chunk in out.chunks_mut(16) {
                let b = GenericArray::from_mut_slice(chunk);
                if encrypt {
                    c.encrypt_block(b)
                } else {
                    c.decrypt_block(b)
                }
            }
        }};
    }
    match key.len() {
        16 => run!(aes::Aes128),
        24 => run!(aes::Aes192),
        _ => run!(aes::Aes256),
    }
    Ok(out)
}

fn aes_cbc(key: &[u8], iv: &[u8], data: &[u8], encrypt: bool) -> Result<Vec<u8>, DbError> {
    use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
    let mut out = data.to_vec();
    let mut prev: [u8; 16] = iv.try_into().map_err(|_| err("bad iv"))?;
    macro_rules! run {
        ($c:ty) => {{
            let c = <$c>::new_from_slice(key).map_err(|e| err(e.to_string()))?;
            for chunk in out.chunks_mut(16) {
                if encrypt {
                    for (x, p) in chunk.iter_mut().zip(prev.iter()) {
                        *x ^= p;
                    }
                    c.encrypt_block(GenericArray::from_mut_slice(chunk));
                    prev.copy_from_slice(chunk);
                } else {
                    let cur: [u8; 16] = (&*chunk).try_into().map_err(|_| err("bad block"))?;
                    c.decrypt_block(GenericArray::from_mut_slice(chunk));
                    for (x, p) in chunk.iter_mut().zip(prev.iter()) {
                        *x ^= p;
                    }
                    prev = cur;
                }
            }
        }};
    }
    match key.len() {
        16 => run!(aes::Aes128),
        24 => run!(aes::Aes192),
        _ => run!(aes::Aes256),
    }
    Ok(out)
}

/// `SnowflakeEncryptionUtil.encrypt_stream`: (ciphertext, wrapped key b64, iv b64, matdesc).
fn encrypt(m: &Material, data: &[u8]) -> Result<(Vec<u8>, String, String, String), DbError> {
    let key_size = m.master_key.len();
    let iv: [u8; 16] = rand::random();
    let file_key: Vec<u8> = (0..key_size).map(|_| rand::random::<u8>()).collect();
    let body = aes_cbc(&file_key, &iv, &pkcs7(data), true)?;
    let wrapped = aes_ecb(&m.master_key, &pkcs7(&file_key), true)?;
    let matdesc = format!(
        "{{\"queryId\":{},\"smkId\":{},\"keySize\":{}}}",
        Value::String(m.query_id.clone()),
        Value::String(m.smk_id.clone()),
        Value::String((key_size * 8).to_string())
    );
    Ok((body, B64.encode(wrapped), B64.encode(iv), matdesc))
}

fn decrypt(m: &Material, key_b64: &str, iv_b64: &str, data: &[u8]) -> Result<Vec<u8>, DbError> {
    let wrapped = B64.decode(key_b64).map_err(|e| err(e.to_string()))?;
    let iv = B64.decode(iv_b64).map_err(|e| err(e.to_string()))?;
    let file_key = unpad(&aes_ecb(&m.master_key, &wrapped, false)?);
    Ok(unpad(&aes_cbc(&file_key, &iv, data, false)?))
}

fn encryption_data(key: &str, iv: &str, algo: &str) -> String {
    serde_json::to_string(&json!({
        "EncryptionMode": "FullBlob",
        "WrappedContentKey": {"KeyId": "symmKey1", "EncryptedKey": key, "Algorithm": "AES_CBC_256"},
        "EncryptionAgent": {"Protocol": "1.0", "EncryptionAlgorithm": algo},
        "ContentEncryptionIV": iv,
        "KeyWrappingMetadata": {"EncryptionLibrary": "Java 5.3.0"},
    }))
    .unwrap_or_default()
    .replace(",\"", ", \"")
    .replace("\":", "\": ")
}

/// AWS SigV4 for an S3 request with `UNSIGNED-PAYLOAD`: the x-amz headers to send and
/// the Authorization value (`_send_single_request_with_authentication`).
#[allow(clippy::too_many_arguments)]
fn sigv4(
    verb: &str,
    url: &str,
    query: &[(&str, &str)],
    payload_hash: &str,
    headers: &[(String, String)],
    region: &str,
    key_id: &str,
    secret: &str,
    token: &str,
    amzdate: &str,
) -> (Vec<(String, String)>, String) {
    let parsed = url::Url::parse(url).ok();
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let mut amz: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| k.starts_with("x-amz"))
        .cloned()
        .collect();
    amz.push(("x-amz-security-token".into(), token.to_string()));
    amz.push(("host".into(), host));
    amz.push(("x-amz-content-sha256".into(), payload_hash.to_string()));
    amz.push(("x-amz-date".into(), amzdate.to_string()));
    let mut lower: Vec<(String, String)> = amz
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .collect();
    lower.sort_by(|a, b| a.0.cmp(&b.0));
    let ws = regex::Regex::new(" +").expect("re");
    let canon_headers: String = lower
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k.trim(), ws.replace_all(v.trim(), " ")))
        .collect();
    let signed: String = lower
        .iter()
        .map(|(k, _)| k.clone())
        .collect::<Vec<_>>()
        .join(";");
    let path = parsed
        .map(|u| u.path().to_string())
        .unwrap_or_else(|| "/".into());
    let mut q: Vec<&(&str, &str)> = query.iter().collect();
    q.sort_by(|a, b| a.0.cmp(b.0));
    let canon_query = q
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical =
        format!("{verb}\n{path}\n{canon_query}\n{canon_headers}\n{signed}\n{payload_hash}");
    let auth =
        crate::sigv4::authorization(&canonical, &signed, amzdate, region, "s3", key_id, secret);
    (amz, auth)
}

impl Remote {
    pub(crate) fn new(
        conn: &Connection,
        kind: &str,
        stage: &Value,
        data: &Value,
        command: &str,
    ) -> Result<Remote, DbError> {
        let creds = &stage["creds"];
        let loc = stage["location"].as_str().unwrap_or_default();
        let (bucket, path) = split_location(loc);
        let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let kind = match kind {
            "S3" => {
                let region = s(&stage["region"]);
                let endpoint =
                    if let Some(ep) = stage["endPoint"].as_str().filter(|e| !e.is_empty()) {
                        format!("https://{bucket}.{ep}")
                    } else if region.starts_with("cn-") {
                        format!("https://{bucket}.s3.{region}.amazonaws.com.cn")
                    } else if stage["useS3RegionalUrl"].as_bool() == Some(true)
                        || stage["useRegionalUrl"].as_bool() == Some(true)
                    {
                        format!("https://{bucket}.s3.{region}.amazonaws.com")
                    } else {
                        format!("https://{bucket}.s3.amazonaws.com")
                    };
                Kind::S3 {
                    path,
                    region,
                    endpoint,
                    key_id: s(&creds["AWS_KEY_ID"]),
                    secret: s(&creds["AWS_SECRET_KEY"]),
                    token: s(&creds["AWS_TOKEN"]),
                }
            }
            "AZURE" => {
                let mut ep = s(&stage["endPoint"]);
                if let Some(e) = ep.strip_prefix("blob.") {
                    ep = e.to_string();
                }
                let sas = s(&creds["AZURE_SAS_TOKEN"]);
                Kind::Azure {
                    account: s(&stage["storageAccount"]),
                    endpoint: ep,
                    container: bucket,
                    path,
                    sas: sas.strip_prefix('?').map(str::to_string).unwrap_or(sas),
                }
            }
            _ => {
                let region = s(&stage["region"]).to_lowercase();
                let url_base = if let Some(ep) =
                    stage["endPoint"].as_str().filter(|e| !e.is_empty())
                {
                    let ep = ep.trim_end_matches('/');
                    let ep = if ep.starts_with("https://") {
                        ep.to_string()
                    } else {
                        format!("https://{ep}")
                    };
                    format!("{ep}/{bucket}")
                } else if stage["useVirtualUrl"].as_bool() == Some(true) {
                    format!("https://{bucket}.storage.googleapis.com")
                } else if region == "me-central2" || stage["useRegionalUrl"].as_bool() == Some(true)
                {
                    format!("https://storage.{region}.rep.googleapis.com/{bucket}")
                } else {
                    format!("https://storage.googleapis.com/{bucket}")
                };
                let presigned = data["presignedUrls"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .or_else(|| stage["presignedUrl"].as_str().map(|u| vec![u.to_string()]))
                    .unwrap_or_default();
                Kind::Gcs {
                    url_base,
                    path,
                    token: creds["GCS_ACCESS_TOKEN"].as_str().map(str::to_string),
                    presigned,
                }
            }
        };
        let em = &data["encryptionMaterial"];
        let (upload_material, download_materials) = match em {
            Value::Array(a) => (None, a.iter().map(material).collect()),
            Value::Null => (None, vec![]),
            other => (material(other), vec![]),
        };
        let _ = conn;
        Ok(Remote {
            kind,
            command: command.to_string(),
            threshold: MULTIPART_THRESHOLD,
            chunk: 8 * 1024 * 1024,
            // `allow_redirects=False`: S3 region redirects are handled (and re-signed) by `s3_send`.
            agent: ureq::AgentBuilder::new().redirects(0).build(),
            upload_material,
            download_materials,
        })
    }

    /// `SnowflakeFileEncryptionMaterial`-free credential renewal: re-run the command.
    fn renew(&mut self, conn: Renew<'_>) -> Result<(), DbError> {
        let v = conn(&self.command)?;
        let creds = &v["data"]["stageInfo"]["creds"];
        let s = |k: &str| creds[k].as_str().unwrap_or_default().to_string();
        match &mut self.kind {
            Kind::S3 {
                key_id,
                secret,
                token,
                ..
            } => {
                *key_id = s("AWS_KEY_ID");
                *secret = s("AWS_SECRET_KEY");
                *token = s("AWS_TOKEN");
            }
            Kind::Azure { sas, .. } => {
                let t = s("AZURE_SAS_TOKEN");
                *sas = t.strip_prefix('?').map(str::to_string).unwrap_or(t);
            }
            Kind::Gcs { token, .. } => {
                *token = creds["GCS_ACCESS_TOKEN"].as_str().map(str::to_string);
            }
        }
        Ok(())
    }

    /// `_send_request_with_retry`: transient statuses back off, an expired token is renewed.
    fn send(
        &mut self,
        conn: Renew<'_>,
        verb: &str,
        build: &dyn Fn(&Remote) -> (String, Headers),
        body: &[u8],
    ) -> Result<ureq::Response, DbError> {
        let mut url = String::new();
        for attempt in 0..5u32 {
            let (u, headers) = build(self);
            url = u;
            let mut req = self.agent.request(verb, &url);
            for (k, v) in &headers {
                req = req.set(k, v);
            }
            let r = if body.is_empty() && verb != "PUT" {
                req.call()
            } else {
                req.send_bytes(body)
            };
            let resp = match r {
                Ok(r) => r,
                Err(ureq::Error::Status(_, r)) => r,
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_secs_f64(f64::min(
                        2f64.powi(attempt as i32),
                        16.0,
                    )));
                    continue;
                }
            };
            let code = resp.status();
            if [408, 429, 500, 502, 503, 504].contains(&code) {
                std::thread::sleep(std::time::Duration::from_secs_f64(f64::min(
                    2f64.powi(attempt as i32),
                    16.0,
                )));
                continue;
            }
            let expired = match &self.kind {
                Kind::S3 { .. } if code == 400 => {
                    let text = resp.into_string().unwrap_or_default();
                    if text.contains("<Code>ExpiredToken</Code>") {
                        true
                    } else {
                        return Err(err(format!("{verb} {url}: HTTP 400 {text}")));
                    }
                }
                Kind::Azure { .. } if code == 403 => {
                    let reason = resp.status_text().to_string();
                    if [
                        "Signature not valid in the specified time frame",
                        "Server failed to authenticate the request.",
                    ]
                    .iter()
                    .any(|m| reason.contains(m))
                    {
                        true
                    } else {
                        return Ok(resp);
                    }
                }
                Kind::Gcs { token: Some(_), .. } if code == 401 => true,
                _ => return Ok(resp),
            };
            if expired {
                self.renew(conn)?;
            }
        }
        Err(DbError::new(
            ErrorKind::Operational,
            253003,
            None,
            format!("{verb} with url {url} failed for exceeding maximum retries."),
        ))
    }

    /// `_send_request_with_authentication_and_retry` (S3): SigV4 plus region redirects.
    #[allow(clippy::too_many_arguments)]
    fn s3_send(
        &mut self,
        conn: Renew<'_>,
        verb: &str,
        key: &str,
        query: &[(&str, &str)],
        headers: &[(String, String)],
        body: &[u8],
        unsigned: bool,
    ) -> Result<ureq::Response, DbError> {
        let Kind::S3 { endpoint, .. } = &self.kind else {
            unreachable!()
        };
        let qs: String = query
            .iter()
            .map(|(k, v)| {
                if v.is_empty() {
                    k.to_string()
                } else {
                    format!("{k}={v}")
                }
            })
            .collect::<Vec<_>>()
            .join("&");
        let mut url = format!("{endpoint}/{}", quote(key));
        if !qs.is_empty() {
            url = format!("{url}?{qs}");
        }
        let payload_hash = if unsigned {
            "UNSIGNED-PAYLOAD".to_string()
        } else {
            hex(&Sha256::digest(body))
        };
        for _ in 0..5 {
            let u = url.clone();
            let build = |r: &Remote| -> (String, Headers) {
                let Kind::S3 {
                    region,
                    key_id,
                    secret,
                    token,
                    ..
                } = &r.kind
                else {
                    unreachable!()
                };
                let amzdate = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
                let (amz, auth) = sigv4(
                    verb,
                    &u,
                    query,
                    &payload_hash,
                    headers,
                    region,
                    key_id,
                    secret,
                    token,
                    &amzdate,
                );
                let mut h: Vec<(String, String)> = headers
                    .iter()
                    .filter(|(k, _)| !k.starts_with("x-amz"))
                    .cloned()
                    .collect();
                h.extend(amz.into_iter().filter(|(k, _)| k != "host"));
                h.push(("Authorization".into(), auth));
                (u.clone(), h)
            };
            let resp = self.send(conn, verb, &build, body)?;
            let code = resp.status();
            if ![301, 302, 307, 308].contains(&code) {
                return Ok(resp);
            }
            if [301, 302].contains(&code) && !["GET", "HEAD"].contains(&verb) {
                return Err(DbError::new(
                    ErrorKind::Operational,
                    253003,
                    None,
                    format!(
                        "S3 returned {code} for {verb} request. Expected 307/308 for method-preserving redirect."
                    ),
                ));
            }
            let Some(loc) = resp.header("Location").map(str::to_string) else {
                return Err(DbError::new(
                    ErrorKind::Operational,
                    253003,
                    None,
                    format!("S3 returned {code} without Location header"),
                ));
            };
            let Some(bucket_region) = resp.header("x-amz-bucket-region").map(str::to_string) else {
                return Err(DbError::new(
                    ErrorKind::Operational,
                    253003,
                    None,
                    format!("S3 returned {code} without x-amz-bucket-region header"),
                ));
            };
            if let Kind::S3 { region, .. } = &mut self.kind {
                *region = bucket_region;
            }
            url = loc;
        }
        Err(DbError::new(
            ErrorKind::Operational,
            253003,
            None,
            "Too many S3 redirects (max 5)".to_string(),
        ))
    }

    fn azure_url(&self, name: &str, query: &str) -> String {
        let Kind::Azure {
            account,
            endpoint,
            container,
            path,
            sas,
        } = &self.kind
        else {
            unreachable!()
        };
        // A plain-http endpoint is only used by the unit tests' stub server.
        let host = if endpoint.starts_with("http://") {
            endpoint.clone()
        } else {
            format!("https://{account}.blob.{endpoint}")
        };
        let base = format!(
            "{host}/{}/{}",
            quote(container),
            quote(&format!("{path}{name}"))
        );
        if query.is_empty() {
            format!("{base}?{sas}")
        } else {
            format!("{base}?{query}&{sas}")
        }
    }

    fn azure_send(
        &mut self,
        conn: Renew<'_>,
        verb: &str,
        name: &str,
        query: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<ureq::Response, DbError> {
        let (name, query) = (name.to_string(), query.to_string());
        let build = |r: &Remote| {
            let mut h = headers.to_vec();
            // The connector formats the literal text "YYYY-MM-DD".
            h.push(("Date".into(), "YYYY-MM-DD".into()));
            (r.azure_url(&name, &query), h)
        };
        self.send(conn, verb, &build, body)
    }

    /// `upload` one file; `false` when it exists and `overwrite` is off (SKIPPED).
    pub(crate) fn upload(
        &mut self,
        conn: Renew<'_>,
        name: &str,
        body: &[u8],
        overwrite: bool,
    ) -> Result<bool, DbError> {
        let digest = B64.encode(Sha256::digest(body));
        let enc = match &self.upload_material {
            Some(m) => Some(encrypt(m, body)?),
            None => None,
        };
        let payload = enc
            .as_ref()
            .map(|e| e.0.clone())
            .unwrap_or_else(|| body.to_vec());
        let dst = name.trim_start_matches('/').to_string();
        let chunk = chunk_size(payload.len() as u64, self.chunk) as usize;
        let multipart = payload.len() as u64 >= self.threshold;
        match &self.kind {
            Kind::S3 { path, .. } => {
                let key = format!("{path}{dst}");
                if !overwrite
                    && self
                        .s3_send(conn, "HEAD", &key, &[], &[], &[], false)?
                        .status()
                        == 200
                {
                    return Ok(false);
                }
                let mut meta = vec![("x-amz-meta-sfc-digest".to_string(), digest)];
                if let Some((_, k, iv, md)) = &enc {
                    meta.push(("x-amz-meta-x-amz-iv".into(), iv.clone()));
                    meta.push(("x-amz-meta-x-amz-key".into(), k.clone()));
                    meta.push(("x-amz-meta-x-amz-matdesc".into(), md.clone()));
                }
                let mut h = vec![(
                    "Content-Type".to_string(),
                    "application/octet-stream".to_string(),
                )];
                h.extend(meta);
                if !multipart {
                    let r = self.s3_send(conn, "PUT", &key, &[], &h, &payload, true)?;
                    if r.status() >= 300 {
                        return Err(err(format!("S3 upload failed: HTTP {}", r.status())));
                    }
                    return Ok(true);
                }
                let r = self.s3_send(conn, "POST", &key, &[("uploads", "")], &h, &[], false)?;
                if r.status() != 200 {
                    return Err(err(format!(
                        "S3 multipart initiate failed: HTTP {}",
                        r.status()
                    )));
                }
                let xml = r.into_string().unwrap_or_default();
                let upload_id = xml_text(&xml, "UploadId")
                    .ok_or_else(|| err("S3 multipart initiate: no UploadId"))?;
                let mut etags = Vec::new();
                let result = (|| -> Result<(), DbError> {
                    for (i, part) in payload.chunks(chunk).enumerate() {
                        let n = (i + 1).to_string();
                        let r = self.s3_send(
                            conn,
                            "PUT",
                            &key,
                            &[("partNumber", &n), ("uploadId", &upload_id)],
                            &[],
                            part,
                            true,
                        )?;
                        if r.status() != 200 {
                            return Err(err(format!("S3 part upload failed: HTTP {}", r.status())));
                        }
                        etags.push(r.header("ETag").unwrap_or_default().to_string());
                    }
                    let parts: String = etags
                        .iter()
                        .enumerate()
                        .map(|(i, e)| {
                            format!(
                                "<Part><ETag>{}</ETag><PartNumber>{}</PartNumber></Part>",
                                xml_escape(e),
                                i + 1
                            )
                        })
                        .collect();
                    let body =
                        format!("<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>");
                    let r = self.s3_send(
                        conn,
                        "POST",
                        &key,
                        &[("uploadId", &upload_id)],
                        &[],
                        body.as_bytes(),
                        false,
                    )?;
                    if r.status() >= 300 {
                        return Err(err(format!(
                            "S3 multipart complete failed: HTTP {}",
                            r.status()
                        )));
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    let _ = self.s3_send(
                        conn,
                        "DELETE",
                        &key,
                        &[("uploadId", &upload_id)],
                        &[],
                        &[],
                        false,
                    );
                    return Err(e);
                }
            }
            Kind::Azure { .. } => {
                if !overwrite && self.azure_send(conn, "HEAD", &dst, "", &[], &[])?.status() == 200
                {
                    return Ok(false);
                }
                let mut meta = vec![("x-ms-meta-sfcdigest".to_string(), digest)];
                if let Some((_, k, iv, md)) = &enc {
                    meta.push((
                        "x-ms-meta-encryptiondata".into(),
                        encryption_data(k, iv, "AES_CBC_128"),
                    ));
                    meta.push(("x-ms-meta-matdesc".into(), md.clone()));
                }
                if !multipart {
                    let mut h = vec![
                        ("x-ms-blob-type".to_string(), "BlockBlob".to_string()),
                        ("Content-Encoding".to_string(), "utf-8".to_string()),
                    ];
                    h.extend(meta);
                    let r = self.azure_send(conn, "PUT", &dst, "", &h, &payload)?;
                    if r.status() >= 300 {
                        return Err(err(format!("Azure upload failed: HTTP {}", r.status())));
                    }
                    return Ok(true);
                }
                let mut ids = Vec::new();
                for part in payload.chunks(chunk) {
                    let id: String = (0..20)
                        .map(|_| HEXDIGITS[rand::random::<usize>() % HEXDIGITS.len()] as char)
                        .collect();
                    let h = vec![("Content-Length".to_string(), part.len().to_string())];
                    let r = self.azure_send(
                        conn,
                        "PUT",
                        &dst,
                        &format!("comp=block&blockid={id}"),
                        &h,
                        part,
                    )?;
                    if r.status() >= 300 {
                        return Err(err(format!(
                            "Azure block upload failed: HTTP {}",
                            r.status()
                        )));
                    }
                    ids.push(id);
                }
                use md5::Digest as _;
                let md5 = B64.encode(md5::Md5::digest(&payload));
                let list: String = ids
                    .iter()
                    .map(|i| format!("<Latest>{i}</Latest>"))
                    .collect();
                let mut h = vec![
                    (
                        "x-ms-blob-content-encoding".to_string(),
                        "utf-8".to_string(),
                    ),
                    ("x-ms-blob-content-md5".to_string(), md5),
                ];
                h.extend(meta);
                let r = self.azure_send(
                    conn,
                    "PUT",
                    &dst,
                    "comp=blocklist",
                    &h,
                    format!("<BlockList>{list}</BlockList>").as_bytes(),
                )?;
                if r.status() >= 300 {
                    return Err(err(format!("Azure block list failed: HTTP {}", r.status())));
                }
            }
            Kind::Gcs {
                url_base,
                path,
                token,
                presigned,
            } => {
                let url = match (token, presigned.first()) {
                    (None, Some(p)) => p.clone(),
                    _ => format!("{url_base}/{}", quote(&format!("{path}{dst}"))),
                };
                let mut h = vec![("x-goog-meta-sfc-digest".to_string(), digest)];
                if let Some((_, k, iv, md)) = &enc {
                    h.push((
                        "x-goog-meta-encryptiondata".into(),
                        encryption_data(k, iv, "AES_CBC_256"),
                    ));
                    h.push(("x-goog-meta-matdesc".into(), md.clone()));
                }
                let build = move |r: &Remote| (url.clone(), gcs_auth(r, &h));
                let r = self.send(conn, "PUT", &build, &payload)?;
                if r.status() >= 300 {
                    return Err(err(format!("GCS upload failed: HTTP {}", r.status())));
                }
            }
        }
        Ok(true)
    }

    pub(crate) fn download(
        &mut self,
        conn: Renew<'_>,
        idx: usize,
        src: &str,
    ) -> Result<Vec<u8>, DbError> {
        let headers_of = |r: &ureq::Response| -> Headers {
            r.headers_names()
                .into_iter()
                .filter_map(|n| r.header(&n).map(|v| (n.to_lowercase(), v.to_string())))
                .collect()
        };
        let read = |r: ureq::Response| -> Result<Vec<u8>, DbError> {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut r.into_reader(), &mut buf)
                .map_err(|e| err(e.to_string()))?;
            Ok(buf)
        };
        let src = src.trim_start_matches('/').to_string();
        let ranged = |size: u64, chunk: u64, i: u64, n: u64| -> String {
            let _ = size;
            if i < n - 1 {
                format!("bytes={}-{}", i * chunk, (i + 1) * chunk - 1)
            } else {
                format!("bytes={}-", i * chunk)
            }
        };
        let (body, headers) = match &self.kind {
            Kind::S3 { path, .. } => {
                let key = format!("{path}{src}");
                // `prepare_download`: HEAD for the size and the encryption metadata.
                let head = self.s3_send(conn, "HEAD", &key, &[], &[], &[], false)?;
                let headers = headers_of(&head);
                let size: u64 = head
                    .header("Content-Length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let chunk = chunk_size(size, self.chunk);
                if size > self.threshold {
                    let n = size.div_ceil(chunk);
                    let mut out = Vec::with_capacity(size as usize);
                    for i in 0..n {
                        let h = vec![("Range".to_string(), ranged(size, chunk, i, n))];
                        let r = self.s3_send(conn, "GET", &key, &[], &h, &[], false)?;
                        if ![200, 206].contains(&r.status()) {
                            return Err(err(format!("S3 download failed: HTTP {}", r.status())));
                        }
                        out.extend(read(r)?);
                    }
                    (out, headers)
                } else {
                    let r = self.s3_send(conn, "GET", &key, &[], &[], &[], false)?;
                    if r.status() >= 300 {
                        return Err(err(format!("S3 download failed: HTTP {}", r.status())));
                    }
                    let h = headers_of(&r);
                    (read(r)?, if headers.is_empty() { h } else { headers })
                }
            }
            Kind::Azure { .. } => {
                let head = self.azure_send(conn, "HEAD", &src, "", &[], &[])?;
                let headers = headers_of(&head);
                let size: u64 = head
                    .header("Content-Length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let chunk = chunk_size(size, self.chunk);
                if size > self.threshold {
                    let n = size.div_ceil(chunk);
                    let mut out = Vec::with_capacity(size as usize);
                    for i in 0..n {
                        let h = vec![("Range".to_string(), ranged(size, chunk, i, n))];
                        let r = self.azure_send(conn, "GET", &src, "", &h, &[])?;
                        if ![200, 206].contains(&r.status()) {
                            return Err(err(format!("Azure download failed: HTTP {}", r.status())));
                        }
                        out.extend(read(r)?);
                    }
                    (out, headers)
                } else {
                    let r = self.azure_send(conn, "GET", &src, "", &[], &[])?;
                    if r.status() >= 300 {
                        return Err(err(format!("Azure download failed: HTTP {}", r.status())));
                    }
                    let h = headers_of(&r);
                    (read(r)?, if headers.is_empty() { h } else { headers })
                }
            }
            Kind::Gcs {
                url_base,
                path,
                token,
                presigned,
            } => {
                let url = match (token, presigned.get(idx)) {
                    (None, Some(p)) => p.clone(),
                    _ => format!("{url_base}/{}", quote(&format!("{path}{src}"))),
                };
                let build = move |r: &Remote| (url.clone(), gcs_auth(r, &[]));
                let r = self.send(conn, "GET", &build, &[])?;
                if r.status() >= 300 {
                    return Err(err(format!("GCS download failed: HTTP {}", r.status())));
                }
                let h = headers_of(&r);
                (read(r)?, h)
            }
        };
        let Some(Some(m)) = self.download_materials.get(idx) else {
            return Ok(body);
        };
        let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let (key, iv) = match &self.kind {
            Kind::S3 { .. } => (h("x-amz-meta-x-amz-key"), h("x-amz-meta-x-amz-iv")),
            Kind::Azure { .. } | Kind::Gcs { .. } => {
                let name = if matches!(self.kind, Kind::Azure { .. }) {
                    "x-ms-meta-encryptiondata"
                } else {
                    "x-goog-meta-encryptiondata"
                };
                let ed: Value = h(name)
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                (
                    ed["WrappedContentKey"]["EncryptedKey"]
                        .as_str()
                        .map(str::to_string),
                    ed["ContentEncryptionIV"].as_str().map(str::to_string),
                )
            }
        };
        match (key, iv) {
            (Some(k), Some(i)) => decrypt(m, &k, &i, &body),
            _ => Ok(body),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type RequestLog = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<(String, String)>)>>>;

    fn material() -> Material {
        Material {
            master_key: (0u8..16).collect(),
            query_id: "01ab".into(),
            smk_id: "42".into(),
        }
    }

    /// Ciphertext produced by the Python connector's `encrypt_stream` with this master key.
    #[test]
    fn decrypts_python_vector() {
        let body = B64
            .decode("6pJ5stc87M5Y9CNkoGSVmKPEceqHJS0fuKb6my8G+Tpem6gCX4K/WpNqzh1xhp/dSwO/47PWdsuzCqayiVY7/FfIV1bf22RVTT9Sw0KfeSZaLooUn2/L7GZpFu7Q/6B1")
            .unwrap();
        let plain = decrypt(
            &material(),
            "DnptDY/vtOp8kVoxSzZI4pVPZPLk6G6e7oLSAhZoSJk=",
            "tWqclD4jdZ+j/AOKzJMm5g==",
            &body,
        )
        .unwrap();
        assert_eq!(plain, b"hello snowflake cloud stage\n".repeat(3));
    }

    #[test]
    fn encrypt_round_trips_and_matdesc_matches_python() {
        let data = b"some file contents, not block aligned";
        let (body, key, iv, md) = encrypt(&material(), data).unwrap();
        assert_eq!(md, r#"{"queryId":"01ab","smkId":"42","keySize":"128"}"#);
        assert_eq!(decrypt(&material(), &key, &iv, &body).unwrap(), data);
        // A block-aligned input still gets a full padding block, like the connector.
        let (body16, k2, iv2, _) = encrypt(&material(), &[7u8; 16]).unwrap();
        assert_eq!(body16.len(), 32);
        assert_eq!(
            decrypt(&material(), &k2, &iv2, &body16).unwrap(),
            vec![7u8; 16]
        );
    }

    /// Signature computed with the Python connector's SigV4 helpers for the same inputs.
    #[test]
    fn sigv4_matches_python_connector() {
        let url = "https://sfc-bucket.s3.amazonaws.com/stage/path/file%20one.csv.gz";
        let h = vec![("x-amz-meta-sfc-digest".to_string(), "abc=".to_string())];
        let (_, auth) = sigv4(
            "PUT",
            url,
            &[],
            "UNSIGNED-PAYLOAD",
            &h,
            "us-west-2",
            "KEYID",
            "SECRET",
            "TOK",
            "20240102T030405Z",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=KEYID/20240102/us-west-2/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-meta-sfc-digest;x-amz-security-token, Signature=24d8ed37b56ea8d7ca5afa968a2046b7a46fd713eaa67d6334add3ed579832b1"
        );
    }

    /// Multipart requests signed by the Python connector's SigV4 helpers.
    #[test]
    fn sigv4_multipart_requests_match_python_connector() {
        let url = "https://sfc-bucket.s3.amazonaws.com/stage/path/big.csv";
        let empty = hex(&Sha256::digest(b""));
        let date = "20240102T030405Z";
        let sig = |verb: &str, q: &[(&str, &str)], hash: &str, h: &[(String, String)]| {
            let (_, a) = sigv4(
                verb,
                url,
                q,
                hash,
                h,
                "us-west-2",
                "KEYID",
                "SECRET",
                "TOK",
                date,
            );
            a.rsplit("Signature=")
                .next()
                .unwrap_or_default()
                .to_string()
        };
        let meta = vec![("x-amz-meta-sfc-digest".to_string(), "abc=".to_string())];
        assert_eq!(
            sig("POST", &[("uploads", "")], &empty, &meta),
            "52ae315e0be0cf3d15f54de0db1287cd753b7605479908cafdea1197442b3074"
        );
        assert_eq!(
            sig(
                "PUT",
                &[("partNumber", "2"), ("uploadId", "U1")],
                "UNSIGNED-PAYLOAD",
                &[]
            ),
            "5dbbb38d6c209dfff02928c9b3690b1f77fd5bf57d921f563fb144a61fc04127"
        );
        let body = r#"<CompleteMultipartUpload><Part><ETag>"e1"</ETag><PartNumber>1</PartNumber></Part></CompleteMultipartUpload>"#;
        let hash = hex(&Sha256::digest(body.as_bytes()));
        assert_eq!(
            sig("POST", &[("uploadId", "U1")], &hash, &[]),
            "d29c5f6b901d6c7643d2ad17272973fcbe6cc546696367999a4c23ae3485350c"
        );
    }

    #[test]
    fn chunk_size_matches_connector_calculator() {
        assert_eq!(
            chunk_size(100 * 1024 * 1024 * 1024, 8 * 1024 * 1024),
            10_737_419
        );
        assert_eq!(chunk_size(70 * 1024 * 1024, 8 * 1024 * 1024), 8_388_608);
    }

    #[test]
    fn xml_text_decodes_entities() {
        assert_eq!(
            xml_text(
                "<R><Bucket>b</Bucket><Key>k</Key><UploadId>U&amp;1</UploadId></R>",
                "UploadId"
            )
            .as_deref(),
            Some("U&1")
        );
    }

    /// A scripted HTTP server: answers each request with the next canned response
    /// and records `METHOD path?query` plus the lower-cased headers.
    fn stub(responses: Vec<String>) -> (String, RequestLog) {
        use std::io::{BufRead, BufReader, Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log2 = log.clone();
        let base2 = base.clone();
        std::thread::spawn(move || {
            for resp in responses {
                let (mut sock, _) = l.accept().unwrap();
                let mut r = BufReader::new(sock.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let mut headers = Vec::new();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    let h = h.trim_end().to_string();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    let (k, v) = (k.trim().to_lowercase(), v.trim().to_string());
                    if k == "content-length" {
                        len = v.parse().unwrap();
                    }
                    headers.push((k, v));
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).unwrap();
                let parts: Vec<&str> = line.split(' ').collect();
                log2.lock()
                    .unwrap()
                    .push((format!("{} {}", parts[0], parts[1]), headers));
                sock.write_all(resp.replace("{BASE}", &base2).as_bytes())
                    .unwrap();
            }
        });
        (base, log)
    }

    fn http(status: &str, headers: &[(&str, &str)], body: &str) -> String {
        let len = headers
            .iter()
            .find(|(k, _)| *k == "Content-Length")
            .map(|(_, v)| v.to_string())
            .unwrap_or(body.len().to_string());
        let headers: Vec<&(&str, &str)> = headers
            .iter()
            .filter(|(k, _)| *k != "Content-Length")
            .collect();
        let mut s = format!("HTTP/1.1 {status}\r\nContent-Length: {len}\r\nConnection: close\r\n");
        for (k, v) in headers {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        format!("{s}\r\n{body}")
    }

    fn s3_remote(endpoint: &str) -> Remote {
        Remote {
            kind: Kind::S3 {
                path: "stage/".into(),
                region: "us-west-2".into(),
                endpoint: endpoint.into(),
                key_id: "K1".into(),
                secret: "S1".into(),
                token: "T1".into(),
            },
            command: "PUT file:///x @st".into(),
            threshold: 10,
            chunk: 4,
            agent: ureq::AgentBuilder::new().redirects(0).build(),
            upload_material: None,
            download_materials: vec![None],
        }
    }

    #[test]
    fn s3_multipart_upload_sequence() {
        let init = "<InitiateMultipartUploadResult><Bucket>b</Bucket><Key>k</Key><UploadId>UP1</UploadId></InitiateMultipartUploadResult>";
        let (base, log) = stub(vec![
            http("200 OK", &[], init),
            http("200 OK", &[("ETag", "\"e1\"")], ""),
            http("200 OK", &[("ETag", "\"e2\"")], ""),
            http("200 OK", &[("ETag", "\"e3\"")], ""),
            http("200 OK", &[], ""),
        ]);
        let mut r = s3_remote(&base);
        let mut renew = |_: &str| -> Result<Value, DbError> { panic!("no renewal expected") };
        assert!(
            r.upload(&mut renew, "big.csv", b"0123456789ab", true)
                .unwrap()
        );
        let log = log.lock().unwrap();
        let lines: Vec<&str> = log.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            lines,
            [
                "POST /stage/big.csv?uploads",
                "PUT /stage/big.csv?partNumber=1&uploadId=UP1",
                "PUT /stage/big.csv?partNumber=2&uploadId=UP1",
                "PUT /stage/big.csv?partNumber=3&uploadId=UP1",
                "POST /stage/big.csv?uploadId=UP1",
            ]
        );
        let h = |i: usize, k: &str| {
            log[i]
                .1
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        assert!(h(0, "x-amz-meta-sfc-digest").is_some());
        assert_eq!(
            h(1, "x-amz-content-sha256").as_deref(),
            Some("UNSIGNED-PAYLOAD")
        );
        assert_eq!(h(4, "x-amz-content-sha256"), Some(hex(&Sha256::digest(br#"<CompleteMultipartUpload><Part><ETag>"e1"</ETag><PartNumber>1</PartNumber></Part><Part><ETag>"e2"</ETag><PartNumber>2</PartNumber></Part><Part><ETag>"e3"</ETag><PartNumber>3</PartNumber></Part></CompleteMultipartUpload>"#))));
    }

    #[test]
    fn s3_failed_part_aborts() {
        let init = "<R><Bucket>b</Bucket><Key>k</Key><UploadId>UP1</UploadId></R>";
        let (base, log) = stub(vec![
            http("200 OK", &[], init),
            http("403 Forbidden", &[], "denied"),
            http("204 No Content", &[], ""),
        ]);
        let mut r = s3_remote(&base);
        let mut renew = |_: &str| -> Result<Value, DbError> { panic!("no renewal expected") };
        assert!(
            r.upload(&mut renew, "big.csv", b"0123456789ab", true)
                .is_err()
        );
        let log = log.lock().unwrap();
        assert_eq!(log.last().unwrap().0, "DELETE /stage/big.csv?uploadId=UP1");
    }

    #[test]
    fn s3_redirect_updates_region_and_resigns() {
        let (base, log) = stub(vec![
            http(
                "307 Temporary Redirect",
                &[
                    ("Location", "{BASE}/stage/a?moved"),
                    ("x-amz-bucket-region", "eu-west-1"),
                ],
                "",
            ),
            http("200 OK", &[], ""),
        ]);
        let mut r = s3_remote(&base);
        let mut renew = |_: &str| -> Result<Value, DbError> { panic!("no renewal expected") };
        assert_eq!(
            r.s3_send(&mut renew, "HEAD", "stage/a", &[], &[], &[], false)
                .unwrap()
                .status(),
            200
        );
        let Kind::S3 { region, .. } = &r.kind else {
            unreachable!()
        };
        assert_eq!(region, "eu-west-1");
        let log = log.lock().unwrap();
        assert_eq!(log[1].0, "HEAD /stage/a?moved");
        let auth = |i: usize| {
            log[i]
                .1
                .iter()
                .find(|(n, _)| n == "authorization")
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert!(auth(0).contains("/us-west-2/s3/"));
        assert!(auth(1).contains("/eu-west-1/s3/"));
    }

    #[test]
    fn s3_method_changing_redirect_rejected_for_put() {
        let (base, _log) = stub(vec![http(
            "301 Moved Permanently",
            &[
                ("Location", "{BASE}/x"),
                ("x-amz-bucket-region", "eu-west-1"),
            ],
            "",
        )]);
        let mut r = s3_remote(&base);
        let mut renew = |_: &str| -> Result<Value, DbError> { panic!("no renewal expected") };
        let e = r
            .s3_send(&mut renew, "PUT", "stage/a", &[], &[], b"x", true)
            .unwrap_err();
        assert_eq!(
            e.raw_msg,
            "S3 returned 301 for PUT request. Expected 307/308 for method-preserving redirect."
        );
    }

    #[test]
    fn s3_expired_token_renews_and_retries() {
        let (base, log) = stub(vec![
            http(
                "400 Bad Request",
                &[],
                "<Error><Code>ExpiredToken</Code></Error>",
            ),
            http("503 Service Unavailable", &[], ""),
            http("200 OK", &[], ""),
        ]);
        let mut r = s3_remote(&base);
        let mut calls = 0;
        let mut renew = |cmd: &str| -> Result<Value, DbError> {
            calls += 1;
            assert_eq!(cmd, "PUT file:///x @st");
            Ok(
                serde_json::json!({"data": {"stageInfo": {"creds": {"AWS_KEY_ID": "K2", "AWS_SECRET_KEY": "S2", "AWS_TOKEN": "T2"}}}}),
            )
        };
        r.upload(&mut renew, "small.csv", b"abc", true).unwrap();
        assert_eq!(calls, 1);
        let log = log.lock().unwrap();
        let tok = |i: usize| {
            log[i]
                .1
                .iter()
                .find(|(n, _)| n == "x-amz-security-token")
                .map(|(_, v)| v.clone())
        };
        assert_eq!(tok(0).as_deref(), Some("T1"));
        assert_eq!(tok(1).as_deref(), Some("T2"));
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn azure_block_upload_sequence() {
        let (base, log) = stub(vec![
            http("201 Created", &[], ""),
            http("201 Created", &[], ""),
            http("201 Created", &[], ""),
            http("201 Created", &[], ""),
        ]);
        let mut r = s3_remote(&base);
        r.kind = Kind::Azure {
            account: "acct".into(),
            endpoint: base.clone(),
            container: "c".into(),
            path: "p/".into(),
            sas: "sig=1".into(),
        };
        let mut renew = |_: &str| -> Result<Value, DbError> { panic!("no renewal expected") };
        assert!(
            r.upload(&mut renew, "f.csv", b"0123456789ab", true)
                .unwrap()
        );
        let log = log.lock().unwrap();
        let re = regex::Regex::new(r"^PUT /c/p/f\.csv\?comp=block&blockid=[0-9a-fA-F]{20}&sig=1$")
            .unwrap();
        for i in 0..3 {
            assert!(re.is_match(&log[i].0), "{}", log[i].0);
        }
        assert_eq!(log[3].0, "PUT /c/p/f.csv?comp=blocklist&sig=1");
        let h = |k: &str| {
            log[3]
                .1
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        use md5::Digest as _;
        assert_eq!(
            h("x-ms-blob-content-md5"),
            Some(B64.encode(md5::Md5::digest(b"0123456789ab")))
        );
        assert_eq!(h("date").as_deref(), Some("YYYY-MM-DD"));
    }

    #[test]
    fn s3_ranged_download_sequence() {
        let (base, log) = stub(vec![
            http("200 OK", &[("Content-Length", "12")], ""),
            http("206 Partial Content", &[], "0123"),
            http("206 Partial Content", &[], "4567"),
            http("206 Partial Content", &[], "89ab"),
        ]);
        let mut r = s3_remote(&base);
        let mut renew = |_: &str| -> Result<Value, DbError> { panic!("no renewal expected") };
        assert_eq!(
            r.download(&mut renew, 0, "big.csv").unwrap(),
            b"0123456789ab"
        );
        let log = log.lock().unwrap();
        let range = |i: usize| {
            log[i]
                .1
                .iter()
                .find(|(n, _)| n == "range")
                .map(|(_, v)| v.clone())
        };
        assert_eq!(log[0].0, "HEAD /stage/big.csv");
        assert_eq!(range(1).as_deref(), Some("bytes=0-3"));
        assert_eq!(range(2).as_deref(), Some("bytes=4-7"));
        assert_eq!(range(3).as_deref(), Some("bytes=8-"));
    }

    #[test]
    fn encryption_data_uses_python_json_separators() {
        let s = encryption_data("K", "I", "AES_CBC_128");
        assert_eq!(
            s,
            r#"{"EncryptionMode": "FullBlob", "WrappedContentKey": {"KeyId": "symmKey1", "EncryptedKey": "K", "Algorithm": "AES_CBC_256"}, "EncryptionAgent": {"Protocol": "1.0", "EncryptionAlgorithm": "AES_CBC_128"}, "ContentEncryptionIV": "I", "KeyWrappingMetadata": {"EncryptionLibrary": "Java 5.3.0"}}"#
        );
    }
}
