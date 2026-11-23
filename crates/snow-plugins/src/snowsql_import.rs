//! `snow helpers import-snowsql-connections`.

use crate::common::{confirm, prompt};
use indexmap::IndexMap;
use snow_api::output::Output;
use snow_api::{Cli, Result, SnowError};
use std::path::{Path, PathBuf};

const KEY_PRECEDENCE: [(&str, &[&str]); 10] = [
    ("account", &["account", "accountname"]),
    ("user", &["user", "username"]),
    ("database", &["database", "dbname", "databasename"]),
    ("schema", &["schema", "schemaname"]),
    ("warehouse", &["warehouse", "warehousename"]),
    ("role", &["role", "rolename"]),
    ("password", &["password", "pwd"]),
    (
        "private_key_file",
        &["private_key_path", "private_key_file"],
    ),
    (
        "private_key_passphrase",
        &["private_key_passphrase", "private_key_file_pwd"],
    ),
    ("proxy_password", &["proxy_password", "proxy_pwd"]),
];
const INT_KEYS: [&str; 2] = ["login_timeout", "client_prefetch_threads"];
const BOOL_KEYS: [&str; 8] = [
    "client_session_keep_alive",
    "client_store_temporary_credential",
    "insecure_mode",
    "ocsp_fail_open",
    "oauth_disable_pkce",
    "oauth_enable_refresh_tokens",
    "oauth_enable_single_use_refresh_tokens",
    "disable_request_pooling",
];
const OPTION_KEYS: [&str; 5] = [
    "login_timeout",
    "client_session_keep_alive",
    "ocsp_fail_open",
    "insecure_mode",
    "client_store_temporary_credential",
];
/// `ConnectionConfig` field order.
const KNOWN: [&str; 32] = [
    "account",
    "user",
    "password",
    "host",
    "region",
    "port",
    "protocol",
    "database",
    "schema",
    "warehouse",
    "role",
    "authenticator",
    "workload_identity_provider",
    "private_key_file",
    "private_key_raw",
    "private_key_passphrase",
    "token",
    "session_token",
    "master_token",
    "token_file_path",
    "oauth_client_id",
    "oauth_client_secret",
    "oauth_authorization_url",
    "oauth_token_request_url",
    "oauth_redirect_uri",
    "oauth_scope",
    "oauth_enable_pkce",
    "oauth_enable_refresh_tokens",
    "oauth_enable_single_use_refresh_tokens",
    "client_store_temporary_credential",
    "secondary_roles",
    "server_session_keep_alive",
];

#[derive(Clone, Debug, PartialEq)]
enum V {
    S(String),
    I(i64),
    B(bool),
}

type Raw = IndexMap<String, String>;
type Conn = IndexMap<String, V>;

/// A ConfigObj file read with `list_values=False`: top-level sections of key/value pairs.
struct Ini {
    sections: IndexMap<String, Option<Vec<(String, String)>>>,
}

fn parse_ini(text: &str) -> std::result::Result<Ini, ()> {
    let sec_re = regex::Regex::new(r##"^\s*((?:\[\s*)+)(.*?)((?:\s*\])+)\s*(#.*)?$"##).expect("re");
    let key_re = regex::Regex::new(r##"^(\s*)((?:".*?")|(?:'.*?')|(?:[^'"=].*?))\s*=\s*(.*)$"##)
        .expect("re");
    let val_re =
        regex::Regex::new(r##"^((?:".*?")|(?:'.*?')|(?:[^'"#].*?)|(?:))\s*(#.*)?$"##).expect("re");
    let mut sections: IndexMap<String, Option<Vec<(String, String)>>> = IndexMap::new();
    let mut cur: Option<String> = None;
    let mut nested = false;
    let mut top: Vec<String> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let s = line.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        if let Some(c) = sec_re.captures(line) {
            let open = c[1].matches('[').count();
            let close = c[3].matches(']').count();
            if open != close {
                return Err(());
            }
            let mut name = c[2].trim().to_string();
            if name.len() >= 2
                && (name.starts_with('"') && name.ends_with('"')
                    || name.starts_with('\'') && name.ends_with('\''))
            {
                name = name[1..name.len() - 1].to_string();
            }
            if open == 1 {
                if sections.contains_key(&name) {
                    return Err(());
                }
                sections.insert(name.clone(), Some(Vec::new()));
                cur = Some(name);
                nested = false;
            } else {
                if cur.is_none() {
                    return Err(());
                }
                nested = true;
            }
            continue;
        }
        let Some(c) = key_re.captures(line) else {
            return Err(());
        };
        let mut key = c[2].to_string();
        if key.len() >= 2 && (key.starts_with('"') || key.starts_with('\'')) {
            key = key[1..key.len() - 1].to_string();
        }
        let raw = c[3].to_string();
        let value = if raw.starts_with("'''") || raw.starts_with("\"\"\"") {
            let q = &raw[..3];
            let rest = &raw[3..];
            if let Some(end) = rest.find(q) {
                rest[..end].to_string()
            } else {
                let mut acc = format!("{rest}\n");
                loop {
                    let Some(l) = lines.get(i) else {
                        return Err(());
                    };
                    i += 1;
                    if let Some(end) = l.find(q) {
                        acc.push_str(&l[..end]);
                        break acc;
                    }
                    acc.push_str(l);
                    acc.push('\n');
                }
            }
        } else {
            let Some(v) = val_re.captures(&raw) else {
                return Err(());
            };
            v[1].to_string()
        };
        if nested {
            continue;
        }
        match &cur {
            Some(name) => {
                let items = sections.get_mut(name).and_then(Option::as_mut).ok_or(())?;
                if items.iter().any(|(k, _)| *k == key) {
                    return Err(());
                }
                items.push((key, value));
            }
            None => {
                if top.contains(&key) {
                    return Err(());
                }
                top.push(key);
            }
        }
    }
    Ok(Ini { sections })
}

fn config_value(v: &str) -> String {
    let b = v.as_bytes();
    if b.len() >= 2 && b[0] == b[b.len() - 1] && (b[0] == b'"' || b[0] == b'\'') {
        return v[1..v.len() - 1].to_string();
    }
    v.to_string()
}

fn name_for_prompt(s: &str) -> String {
    snow_api::sql::sanitize_for_terminal(s)
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(c, '\u{2028}' | '\u{2029}' | '\u{85}' | '\u{a0}' | '\u{ad}')
        })
        .collect()
}

fn blank(s: &str) -> bool {
    s.trim().is_empty()
}

fn coerce(key: &str, v: &str) -> V {
    if INT_KEYS.contains(&key) {
        return v
            .trim()
            .replace('_', "")
            .parse::<i64>()
            .map(V::I)
            .unwrap_or_else(|_| V::S(v.into()));
    }
    if BOOL_KEYS.contains(&key) {
        return match v.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => V::B(true),
            "0" | "false" | "no" | "off" => V::B(false),
            _ => V::S(v.into()),
        };
    }
    V::S(v.into())
}

fn convert(raw: &Raw) -> Conn {
    let mut consumed = std::collections::HashSet::new();
    let mut out = Conn::new();
    for (cli_key, sources) in KEY_PRECEDENCE {
        consumed.extend(sources.iter().filter(|s| raw.contains_key(**s)).copied());
        if let Some(s) = sources
            .iter()
            .find(|s| raw.get(**s).is_some_and(|v| !blank(v)))
        {
            out.insert(cli_key.to_string(), coerce(cli_key, &raw[*s]));
        }
    }
    for (k, v) in raw {
        if !consumed.contains(k.as_str()) {
            out.insert(k.clone(), coerce(k, v));
        }
    }
    out
}

fn drop_blank(c: &mut Conn) {
    c.retain(|_, v| !matches!(v, V::S(s) if blank(s)));
}

fn key_only_with_jwt(cli: &mut Cli, c: &mut Conn, name: &str) {
    if !c
        .get("private_key_file")
        .is_some_and(|v| !matches!(v, V::S(s) if s.is_empty()))
    {
        return;
    }
    let auth = match c.get("authenticator") {
        None | Some(V::S(_))
            if c.get("authenticator")
                .is_none_or(|v| matches!(v, V::S(s) if s.is_empty())) =>
        {
            c.insert("authenticator".into(), V::S("SNOWFLAKE_JWT".into()));
            return;
        }
        Some(V::S(s)) => s.clone(),
        Some(V::B(b)) => if *b { "True" } else { "False" }.into(),
        Some(V::I(i)) => i.to_string(),
        None => return,
    };
    if auth.to_uppercase() == "SNOWFLAKE_JWT" {
        return;
    }
    c.shift_remove("private_key_file");
    c.shift_remove("private_key_passphrase");
    cli.console.step(&format!(
        "Connection [{}] keeps authenticator '{}' and omits the private key. The CLI can open that authenticator only without a private key.",
        name_for_prompt(name),
        name_for_prompt(&auth)
    ));
}

fn snowsql_home() -> PathBuf {
    match std::env::var("WORKSPACE") {
        Ok(w) if !w.trim().is_empty() => PathBuf::from(w),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_default()),
    }
}

fn read_ini(p: &Path) -> Result<Ini> {
    let shown = name_for_prompt(&p.display().to_string());
    let text = match std::fs::read(p) {
        Ok(b) => match String::from_utf8(b) {
            Ok(s) => s.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(s),
            Err(_) => {
                return Err(SnowError::cli(format!(
                    "Could not read SnowSQL config file [{shown}]: the file could not be read."
                )));
            }
        },
        Err(e) => {
            let reason = snow_api::errors::strerror(&e);
            return Err(SnowError::cli(format!(
                "Could not read SnowSQL config file [{shown}]: {reason}."
            )));
        }
    };
    parse_ini(&text).map_err(|_| {
        SnowError::cli(format!(
            "Could not read SnowSQL config file [{shown}]: the file could not be parsed."
        ))
    })
}

fn items(ini: &Ini, section: &str) -> Vec<(String, String)> {
    match ini.sections.get(section) {
        Some(Some(v)) => v
            .iter()
            .map(|(k, v)| (k.to_lowercase(), config_value(v)))
            .collect(),
        _ => Vec::new(),
    }
}

fn merge(target: &mut Raw, items: Vec<(String, String)>) {
    for (k, v) in items {
        target.insert(k, config_value(&v));
    }
}

pub fn import_snowsql_connections(cli: &mut Cli) -> Result<Output> {
    let home = snowsql_home();
    let mut files: Vec<PathBuf> = [
        "/etc/snowsql.cnf",
        "/etc/snowflake/snowsql.cnf",
        "/usr/local/etc/snowsql.cnf",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    files.push(home.join(".snowsql.cnf"));
    files.push(home.join(".snowsql").join("config"));
    files.extend(
        cli.list("custom_snowsql_config_files")
            .into_iter()
            .map(PathBuf::from),
    );
    files.push(PathBuf::from("/usr/lib64/snowflake/snowsql/config"));
    let default_name = cli
        .str("default_cli_connection_name")
        .unwrap_or_else(|| "default".into());

    let (mut default_raw, mut named_raw, mut options_raw) =
        (Raw::new(), IndexMap::<String, Raw>::new(), Raw::new());
    for f in &files {
        let shown = name_for_prompt(&f.display().to_string());
        if !f.exists() {
            cli.console.step(&format!(
                "SnowSQL config file [{shown}] does not exist. Skipping."
            ));
            continue;
        }
        cli.console
            .step(&format!("Trying to read connections from [{shown}]."));
        let ini = read_ini(f)?;
        let d = items(&ini, "connections");
        if !d.is_empty() {
            cli.console.step(&format!(
                "Reading SnowSQL's default connection configuration from [{shown}]"
            ));
            merge(&mut default_raw, d);
        }
        merge(&mut options_raw, items(&ini, "options"));
        for section in ini
            .sections
            .keys()
            .filter(|s| s.starts_with("connections."))
        {
            let ss = name_for_prompt(section);
            cli.console.step(&format!(
                "Reading SnowSQL's connection configuration [{ss}] from [{shown}]"
            ));
            let it = items(&ini, section);
            if it.is_empty() {
                cli.console.step(&format!(
                    "Empty connection configuration [{ss}] in [{shown}]. Skipping."
                ));
                continue;
            }
            merge(
                named_raw
                    .entry(section["connections.".len()..].to_string())
                    .or_default(),
                it,
            );
        }
    }
    let mut default_conn = if default_raw.is_empty() {
        Conn::new()
    } else {
        convert(&default_raw)
    };
    let mut named: IndexMap<String, Conn> = named_raw
        .iter()
        .map(|(k, v)| (k.clone(), convert(v)))
        .collect();
    drop_blank(&mut default_conn);
    named.values_mut().for_each(drop_blank);
    let opts: Conn = OPTION_KEYS
        .iter()
        .filter_map(|k| {
            options_raw
                .get(*k)
                .filter(|v| !blank(v))
                .map(|v| (k.to_string(), coerce(k, v)))
        })
        .collect();
    for c in std::iter::once(&mut default_conn).chain(named.values_mut()) {
        if !c.is_empty() {
            for (k, v) in &opts {
                c.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    let empty: Vec<String> = named
        .iter()
        .filter(|(_, c)| c.is_empty())
        .map(|(k, _)| k.clone())
        .collect();
    for n in empty {
        cli.console.step(&format!(
            "Empty connection configuration [connections.{}] after merging. Skipping.",
            name_for_prompt(&n)
        ));
        named.shift_remove(&n);
    }
    let names: Vec<String> = named.keys().cloned().collect();
    for n in &names {
        let mut c = named.shift_remove(n).unwrap_or_default();
        key_only_with_jwt(cli, &mut c, n);
        named.insert(n.clone(), c);
    }
    let mut unnamed = None;
    if !default_conn.is_empty() {
        let mut cand = default_name;
        let mut left = 3;
        let chosen = loop {
            if !blank(&cand) && !named.contains_key(&cand) {
                break cand;
            }
            if left == 0 {
                return Err(SnowError::cli(
                    "Could not choose a name for the SnowSQL default connection.",
                ));
            }
            left -= 1;
            let text = if blank(&cand) {
                "Default connection name cannot be blank. Choose a different name for the SnowSQL default connection".to_string()
            } else {
                format!(
                    "Chosen default connection name '{}' is already taken by other connection being imported from SnowSQL. Please choose a different name for your default connection",
                    name_for_prompt(&cand)
                )
            };
            cand = prompt(cli, &text, false)?.unwrap_or_default();
        };
        key_only_with_jwt(cli, &mut default_conn, &chosen);
        named.shift_remove(&chosen);
        named.insert(chosen.clone(), default_conn);
        unnamed = Some(chosen);
    }

    let existing = cli.config.connections();
    let mut save = Vec::new();
    for (n, c) in named {
        if existing.contains_key(&n) {
            let q = format!(
                "Connection '{}' already exists in Snowflake CLI, do you want to use SnowSQL definition and override existing connection in Snowflake CLI?",
                name_for_prompt(&n)
            );
            if !confirm(cli, &q, false)? {
                continue;
            }
        }
        save.push((n, c));
    }
    for (n, c) in &save {
        cli.console.step(&format!(
            "Saving [{}] connection in Snowflake CLI's config.",
            name_for_prompt(n)
        ));
        let mut known = toml_edit::Table::new();
        let to_item = |v: &V| match v {
            V::S(s) => toml_edit::value(s.as_str()),
            V::I(i) => toml_edit::value(*i),
            V::B(b) => toml_edit::value(*b),
        };
        let alias = |k: &str| {
            if k == "private_key_file_pwd" {
                "private_key_passphrase".to_string()
            } else {
                k.to_string()
            }
        };
        for k in KNOWN {
            if let Some((_, v)) = c.iter().find(|(ck, _)| alias(ck) == k) {
                known.insert(k, to_item(v));
            }
        }
        for (k, v) in c {
            if !KNOWN.contains(&alias(k).as_str()) {
                known.insert(k, to_item(v));
            }
        }
        cli.config.add_connection(n, known)?;
    }
    if let Some(u) = unnamed
        && save.iter().any(|(n, _)| *n == u)
    {
        cli.console.step(&format!(
            "Setting [{}] connection as Snowflake CLI's default connection.",
            name_for_prompt(&u)
        ));
        cli.config
            .set_value(&["default_connection_name"], toml_edit::value(u.as_str()))?;
    }
    Ok(Output::message(
        "Connections successfully imported from SnowSQL to Snowflake CLI.",
    ))
}
