//! `SnowparkAnnotationProcessor` (`codegen/snowpark/python_processor.py`): run each
//! Python artifact in a sandbox interpreter to collect `@udf`/`@sproc` registrations,
//! write their DDL under `generated/`, hook it into the setup script and comment the
//! decorators out of the deployed module.

use super::package::Package;
use snow_api::identifiers::{
    is_valid_identifier, is_valid_string_literal, to_identifier, to_string_literal,
};
use snow_api::project::PyV;
use snow_api::{Cli, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TEMPLATE: &str = include_str!("../../data/callback_source.py.jinja");
const SNOWPARK_LIB: &str = "snowflake-snowpark-python";
const TIMEOUT: Duration = Duration::from_secs(30);

/// Where `execute_script_in_sandbox` looks for an interpreter.
enum Env {
    Auto,
    Venv(Option<PathBuf>),
    Conda(Option<String>),
    /// `sys.executable`: a standalone binary has none, so the PATH lookup stands in.
    Current,
}

/// `_determine_virtual_env(project_root, processor)`.
fn determine_env(cli: &mut Cli, root: &Path, processor: &PyV) -> Env {
    let Some(env) = processor
        .get_some("properties")
        .and_then(|p| p.get_some("env"))
    else {
        return Env::Auto;
    };
    let Some(ty) = env.str_of("type") else {
        return Env::Auto;
    };
    match ty.to_uppercase().as_str() {
        "CONDA" => {
            let name = env.str_of("name");
            if name.is_none() {
                cli.console.warning("No name found in project definition file for the conda environment to run the Snowpark processor in. Will attempt to auto-detect the current conda environment.");
            }
            Env::Conda(name)
        }
        "VENV" => {
            let path = env.str_of("path");
            if path.is_none() {
                cli.console.warning("No path found in project definition file for the conda environment to run the Snowpark processor in. Will attempt to auto-detect the current venv path.");
            }
            Env::Venv(path.map(|p| {
                let p = PathBuf::from(p);
                if p.is_absolute() { p } else { root.join(p) }
            }))
        }
        "CURRENT" => Env::Current,
        _ => Env::Auto,
    }
}

/// The interpreter command line, or the `SandboxExecutionError` text.
fn interpreter(env: &Env) -> std::result::Result<Vec<String>, String> {
    let venv = |p: Option<PathBuf>| -> std::result::Result<Vec<String>, String> {
        let root = match p.or_else(|| std::env::var_os("VIRTUAL_ENV").map(PathBuf::from)) {
            Some(r) => std::path::absolute(&r).unwrap_or(r),
            None => return Err("No venv root found".into()),
        };
        let root = root.canonicalize().unwrap_or(root);
        if !root.is_dir() {
            return Err(format!(
                "venv path must be an existing directory: {}",
                root.display()
            ));
        }
        let exe = root.join("bin").join("python3");
        if !exe.is_file() {
            return Err(format!(
                "No venv Python executable found: {}",
                root.display()
            ));
        }
        Ok(vec![exe.to_string_lossy().into_owned()])
    };
    let conda = |n: Option<String>| -> std::result::Result<Vec<String>, String> {
        let Some(name) = n.or_else(|| std::env::var("CONDA_DEFAULT_ENV").ok()) else {
            return Err("No conda environment found".into());
        };
        let Some(c) = crate::ai::which("conda") else {
            return Err("conda command not found, make sure it is installed on your system and in your PATH".into());
        };
        Ok(vec![
            c.to_string_lossy().into_owned(),
            "run".into(),
            "-n".into(),
            name,
            "--no-capture-output".into(),
            "python".into(),
        ])
    };
    let system = || -> std::result::Result<Vec<String>, String> {
        crate::ai::which("python3")
            .or_else(|| crate::ai::which("python"))
            .map(|p| vec![p.to_string_lossy().into_owned()])
            .ok_or_else(|| "No Python executable found".to_string())
    };
    match env {
        Env::Venv(p) => venv(p.clone()),
        Env::Conda(n) => conda(n.clone()),
        Env::Current => system(),
        Env::Auto => {
            if std::env::var_os("VIRTUAL_ENV").is_some() {
                venv(None)
            } else if std::env::var_os("CONDA_DEFAULT_ENV").is_some() {
                conda(None)
            } else {
                system()
            }
        }
    }
}

/// `subprocess.run([...,"-"], input=script, capture_output=True, timeout=30, cwd=cwd)`.
fn run_script(
    argv: &[String],
    script: &str,
    cwd: &Path,
) -> std::result::Result<(i32, String, String), String> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .arg("-")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| snow_api::errors::python_os_error(&e, Some(&argv[0])))?;
    let mut stdin = child.stdin.take().expect("stdin");
    let script = script.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(script.as_bytes());
    });
    let mut out = child.stdout.take().expect("stdout");
    let mut err = child.stderr.take().expect("stderr");
    let ro = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        s
    });
    let re = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = err.read_to_end(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let shown: Vec<String> = argv
                .iter()
                .cloned()
                .chain(std::iter::once("-".to_string()))
                .map(|a| format!("'{a}'"))
                .collect();
            return Err(format!(
                "Command '[{}]' timed out after 30 seconds",
                shown.join(", ")
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = writer.join();
    let stdout = String::from_utf8_lossy(&ro.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&re.join().unwrap_or_default()).into_owned();
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

/// Python `json.loads` error text for the common failures.
fn json_error(text: &str, e: &serde_json::Error) -> String {
    let (line, col) = (e.line(), e.column());
    let offset: usize = text
        .split('\n')
        .take(line.saturating_sub(1))
        .map(|l| l.chars().count() + 1)
        .sum::<usize>()
        + col.saturating_sub(1);
    let kind = if e.to_string().starts_with("trailing characters") {
        "Extra data"
    } else {
        "Expecting value"
    };
    if text.trim().is_empty() {
        let n = text.chars().count();
        let line = text.matches('\n').count() + 1;
        let col = text
            .rsplit('\n')
            .next()
            .map(|l| l.chars().count())
            .unwrap_or(0)
            + 1;
        return format!("Expecting value: line {line} column {col} (char {n})");
    }
    format!("{kind}: line {line} column {col} (char {offset})")
}

/// A collected extension function after `NativeAppExtensionFunction(**json)` and `_normalize`.
struct ExtFn {
    ty: String,
    name: String,
    handler: String,
    returns: String,
    runtime: String,
    signature: Vec<(String, String, Option<String>)>,
    imports: Vec<String>,
    packages: Vec<String>,
    eais: Vec<String>,
    secrets: Vec<(String, String)>,
    schema: Option<String>,
    app_roles: Vec<String>,
    execute_as_caller: bool,
}

fn ensure_literal(v: &str) -> String {
    if is_valid_string_literal(v) {
        v.to_string()
    } else {
        to_string_literal(v)
    }
}

/// pydantic validation of one entry; `None` is "Invalid extension function definition".
fn parse_fn(v: &serde_json::Value) -> Option<ExtFn> {
    use serde_json::Value as J;
    let o = v.as_object()?;
    const KNOWN: [&str; 14] = [
        "type",
        "lineno",
        "name",
        "handler",
        "imports",
        "packages",
        "runtime",
        "returns",
        "signature",
        "external_access_integrations",
        "secrets",
        "execute_as_caller",
        "schema",
        "application_roles",
    ];
    if o.keys().any(|k| !KNOWN.contains(&k.as_str())) {
        return None;
    }
    let opt_str = |k: &str| -> Option<Option<String>> {
        match o.get(k) {
            None | Some(J::Null) => Some(None),
            Some(J::String(s)) => Some(Some(s.clone())),
            _ => None,
        }
    };
    let str_list = |k: &str| -> Option<Vec<String>> {
        match o.get(k) {
            None | Some(J::Null) => Some(vec![]),
            Some(J::Array(a)) => a.iter().map(|x| x.as_str().map(str::to_string)).collect(),
            _ => None,
        }
    };
    let ty = o.get("type")?.as_str()?.to_string();
    if ![
        "procedure",
        "function",
        "table function",
        "aggregate function",
    ]
    .contains(&ty.as_str())
    {
        return None;
    }
    match o.get("lineno") {
        None | Some(J::Null) => {}
        Some(J::Number(n)) if n.is_i64() => {}
        _ => return None,
    }
    let handler = o.get("handler")?.as_str()?.to_string();
    let returns = o.get("returns")?.as_str()?.to_string();
    let runtime = match o.get("runtime") {
        None | Some(J::Null) => "None".to_string(),
        Some(J::String(s)) => s.clone(),
        Some(J::Number(n)) => snow_api::pyfmt::float_repr(n.as_f64()?),
        _ => return None,
    };
    let mut signature = Vec::new();
    for a in o.get("signature")?.as_array()? {
        let a = a.as_object()?;
        if a.keys()
            .any(|k| !["name", "type", "default"].contains(&k.as_str()))
        {
            return None;
        }
        let default = match a.get("default") {
            None | Some(J::Null) => None,
            Some(J::String(s)) => Some(s.clone()),
            _ => return None,
        };
        signature.push((
            a.get("name")?.as_str()?.to_string(),
            a.get("type")?.as_str()?.to_string(),
            default,
        ));
    }
    let secrets = match o.get("secrets") {
        None | Some(J::Null) => vec![],
        Some(J::Object(m)) => m
            .iter()
            .map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
            .collect::<Option<_>>()?,
        _ => return None,
    };
    let execute_as_caller = match o.get("execute_as_caller") {
        None | Some(J::Null) => false,
        Some(J::Bool(b)) => *b,
        _ => return None,
    };
    let name = opt_str("name")?;
    Some(ExtFn {
        ty,
        name: name.unwrap_or_else(|| handler.clone()),
        handler,
        returns,
        runtime,
        signature,
        imports: str_list("imports")?,
        packages: str_list("packages")?,
        eais: str_list("external_access_integrations")?,
        secrets,
        schema: opt_str("schema")?,
        app_roles: str_list("application_roles")?,
        execute_as_caller,
    })
}

fn stage_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `_normalize(extension_fn, py_file, deploy_root)`; `py_rel` is relative to the deploy root.
fn normalize(cli: &mut Cli, f: &mut ExtFn, py_rel: &Path, deploy_root: &Path) {
    let stem = py_rel
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    f.handler = format!("{stem}.{}", f.handler);
    let re = regex::Regex::new(&format!(
        r"^'{SNOWPARK_LIB}\s*((<|<=|!=|==|>=|>|~=|===)\s*[a-zA-Z0-9_.*+!-]+)?'$"
    ))
    .expect("re");
    f.packages = f
        .packages
        .iter()
        .map(|p| {
            let n = ensure_literal(p.trim());
            if re.is_match(&n) {
                ensure_literal(SNOWPARK_LIB)
            } else {
                n
            }
        })
        .collect();
    let lib = ensure_literal(SNOWPARK_LIB);
    if !f.packages.contains(&lib) {
        f.packages.push(lib);
    }
    let mut set = std::collections::BTreeSet::new();
    set.insert(format!("/{}", stage_path(py_rel)));
    for raw in &f.imports {
        if !deploy_root.join(raw).exists() {
            cli.console.warning(&format!(
                "{raw} does not exist in the deploy root. Skipping validation of this import."
            ));
        }
        if raw.starts_with('@') || raw.starts_with('/') {
            set.insert(raw.clone());
        } else {
            set.insert(format!("/{}", stage_path(Path::new(raw))));
        }
    }
    f.imports = set.into_iter().collect();
}

fn qualified_name(f: &ExtFn) -> String {
    let name = to_identifier(&f.name);
    match &f.schema {
        Some(s) if !s.is_empty() => {
            if is_valid_identifier(s) {
                format!("{s}.{name}")
            } else {
                let full: Vec<String> = s.split('.').map(to_identifier).collect();
                format!("{}.{name}", full.join("."))
            }
        }
        _ => name,
    }
}

/// `generate_create_sql_ddl_statement(extension_fn)`.
fn create_sql(f: &ExtFn) -> String {
    let object_type = match f.ty.as_str() {
        "procedure" => "PROCEDURE",
        "aggregate function" => "AGGREGATE FUNCTION",
        _ => "FUNCTION",
    };
    let args: Vec<String> = f
        .signature
        .iter()
        .map(|(n, t, d)| match d {
            Some(d) => format!("{n} {t} DEFAULT {d}"),
            None => format!("{n} {t}"),
        })
        .collect();
    let mut q = format!(
        "CREATE OR REPLACE\n{object_type} {}({})\nRETURNS {}\nLANGUAGE PYTHON\nRUNTIME_VERSION={}",
        qualified_name(f),
        args.join(", "),
        f.returns,
        f.runtime
    );
    let lits = |v: &[String]| {
        v.iter()
            .map(|x| ensure_literal(x))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !f.imports.is_empty() {
        q.push_str(&format!("\nIMPORTS=({})", lits(&f.imports)));
    }
    if !f.packages.is_empty() {
        let p: Vec<String> = f.packages.iter().map(|p| p.trim().to_string()).collect();
        q.push_str(&format!("\nPACKAGES=({})", lits(&p)));
    }
    if !f.eais.is_empty() {
        q.push_str(&format!(
            "\nEXTERNAL_ACCESS_INTEGRATIONS=({})",
            lits(&f.eais)
        ));
    }
    if !f.secrets.is_empty() {
        let s: Vec<String> = f
            .secrets
            .iter()
            .map(|(k, v)| format!("{}={v}", ensure_literal(k)))
            .collect();
        q.push_str(&format!("\nSECRETS=({})", s.join(", ")));
    }
    q.push_str(&format!("\nHANDLER={}", ensure_literal(&f.handler)));
    if f.ty == "procedure" {
        q.push_str(if f.execute_as_caller {
            "\nEXECUTE AS CALLER"
        } else {
            "\nEXECUTE AS OWNER"
        });
    }
    q.push_str(";\n");
    q
}

/// `generate_grant_sql_ddl_statements(extension_fn)`.
fn grant_sql(cli: &mut Cli, f: &ExtFn) -> Option<String> {
    if f.app_roles.is_empty() {
        cli.console.warning(&format!(
            "Skipping generation of 'GRANT USAGE ON ...' SQL statement for {} {} due to lack of application roles.",
            f.ty.to_uppercase(),
            f.handler
        ));
        return None;
    }
    let object_type = if f.ty == "procedure" {
        "PROCEDURE"
    } else {
        "FUNCTION"
    };
    let types: Vec<&str> = f.signature.iter().map(|(_, t, _)| t.as_str()).collect();
    let stmts: Vec<String> = f
        .app_roles
        .iter()
        .map(|r| {
            format!(
                "GRANT USAGE ON {object_type} {}({})\nTO APPLICATION ROLE {r};",
                qualified_name(f),
                types.join(", ")
            )
        })
        .collect();
    Some(stmts.join("\n"))
}

/// The `ast` decorator id: `None` unless the expression is a Name/Attribute/Call chain.
fn decorator_has_id(expr: &str) -> bool {
    let e = expr.trim();
    if !e
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
    {
        return false;
    }
    // The outermost node: a trailing subscript makes it a Subscript (no id).
    let mut depth = 0i32;
    let mut last_top = ' ';
    for c in e.chars() {
        match c {
            '(' | '[' | '{' => {
                if depth == 0 {
                    last_top = c;
                }
                depth += 1;
            }
            ')' | ']' | '}' => depth -= 1,
            _ if depth == 0 && !c.is_whitespace() => last_top = c,
            _ => {}
        }
    }
    last_top != '['
}

/// `deannotate_module_source(code, extension_fns)`: comment out the decorators of the
/// collected handlers.
fn deannotate(code: &str, handlers: &[String]) -> String {
    let lines: Vec<&str> = py_splitlines(code);
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let def_re = regex::Regex::new(r"^\s*(def|class)\s+([A-Za-z_][A-Za-z0-9_]*)").expect("re");
    let mut i = 0;
    let mut pending: Vec<(usize, usize, String)> = Vec::new();
    let mut found = false;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if let Some(rest) = t.strip_prefix('@') {
            let start = i;
            let mut expr = rest.to_string();
            let mut depth = bracket_depth(rest);
            while depth > 0 && i + 1 < lines.len() {
                i += 1;
                expr.push('\n');
                expr.push_str(lines[i]);
                depth += bracket_depth(lines[i]);
            }
            pending.push((start, i, expr));
        } else if let Some(c) = def_re.captures(lines[i]) {
            if !pending.is_empty() && handlers.iter().any(|h| h == &c[2]) {
                found = true;
                for (s, e, expr) in &pending {
                    if decorator_has_id(expr) {
                        for line in out.iter_mut().take(e + 1).skip(*s) {
                            *line = format!("#: {line}");
                        }
                    }
                }
            }
            pending.clear();
        } else if !(t.is_empty() || t.starts_with('#')) {
            pending.clear();
        }
        i += 1;
    }
    if !found {
        return code.to_string();
    }
    out.join("\n")
}

fn bracket_depth(s: &str) -> i32 {
    let mut d = 0;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '#' => break,
                '(' | '[' | '{' => d += 1,
                ')' | ']' | '}' => d -= 1,
                _ => {}
            },
        }
    }
    d
}

/// `str.splitlines()`.
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let b: Vec<(usize, char)> = s.char_indices().collect();
    let mut k = 0;
    while k < b.len() {
        let (idx, c) = b[k];
        let brk = matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if brk {
            out.push(&s[start..idx]);
            let mut next = idx + c.len_utf8();
            if c == '\r' && k + 1 < b.len() && b[k + 1].1 == '\n' {
                next += 1;
                k += 1;
            }
            start = next;
        }
        k += 1;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `SnowparkAnnotationProcessor.process(artifact, processor)`.
pub fn process(cli: &mut Cli, pkg: &Package, artifact: &PyV, processor: &PyV) -> Result<()> {
    cli.console
        .warning("Snowpark processor is not supported and will be removed soon.");
    let deploy_root = pkg.deploy_root();
    let mut map = crate::artifacts::BundleMap::new(&pkg.project_root, &deploy_root);
    let (src, dest, ignore) = crate::artifacts::mapping_of(artifact);
    map.add(&src, dest.as_deref(), &ignore)?;
    let env = determine_env(cli, &pkg.project_root, processor);
    let mut files: Vec<(PathBuf, PathBuf)> = map
        .all_mappings_expanded()
        .into_iter()
        .filter(|(s, _)| s.is_file() && s.extension().is_some_and(|e| e == "py"))
        .collect();
    files.sort();
    files.dedup();
    let root = crate::artifacts::resolve_without_follow(&pkg.project_root);
    let mut collected: Vec<(PathBuf, Vec<ExtFn>)> = Vec::new();
    for (s, d) in files {
        let shown = s
            .strip_prefix(&root)
            .or_else(|_| s.strip_prefix(&pkg.project_root))
            .unwrap_or(&s)
            .to_string_lossy()
            .into_owned();
        cli.console
            .step(&format!("Processing Snowpark annotations from {shown}"));
        let py_file = d
            .canonicalize()
            .unwrap_or_else(|_| d.clone())
            .to_string_lossy()
            .into_owned();
        let Some(json) = execute_in_sandbox(cli, &py_file, &deploy_root, &env) else {
            continue;
        };
        let rel = d.strip_prefix(&deploy_root).unwrap_or(&d).to_path_buf();
        let mut fns = Vec::new();
        for entry in json.as_array().cloned().unwrap_or_default() {
            match parse_fn(&entry) {
                Some(mut f) => {
                    normalize(cli, &mut f, &rel, &deploy_root);
                    fns.push(f);
                }
                None => cli.console.warning("Invalid extension function definition"),
            }
        }
        if !fns.is_empty() {
            collected.push((d, fns));
        }
    }
    collected.sort_by(|a, b| a.0.cmp(&b.0));
    let generated = pkg.generated_root().join("snowpark");
    let mut sql_files = Vec::new();
    for (py, fns) in &collected {
        let rel = py.strip_prefix(&deploy_root).unwrap_or(py);
        let sql_file = generated.join(rel).with_extension("sql");
        if sql_file.exists() {
            cli.console.warning(&format!(
                "                File {} already exists, will append SQL statements to this file.\n            ",
                sql_file.display()
            ));
        }
        if let Some(p) = sql_file.parent() {
            std::fs::create_dir_all(p)?;
        }
        sql_files.push(sql_file.clone());
        let mut newline = false;
        for f in fns {
            let create = create_sql(f);
            let grants = grant_sql(cli, f);
            let mut text = String::new();
            if newline {
                text.push('\n');
            }
            newline = true;
            text.push_str(&format!(
                "-- Generated by the Snowflake CLI from {}\n-- DO NOT EDIT\n{create}",
                rel.display()
            ));
            if let Some(g) = &grants {
                text.push('\n');
                text.push_str(g);
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&sql_file)?
                .write_all(text.as_bytes())?;
        }
        let code = std::fs::read_to_string(py)?;
        let handlers: Vec<String> = fns
            .iter()
            .map(|f| f.handler.rsplit('.').next().unwrap_or_default().to_string())
            .collect();
        crate::artifacts::write_in_bundle(py, deannotate(&code, &handlers))?;
    }
    if !sql_files.is_empty() {
        edit_setup_script(cli, &sql_files, &deploy_root, &generated)?;
    }
    Ok(())
}

/// `_execute_in_sandbox(py_file, deploy_root, kwargs)`: the parsed stdout, or `None` after a warning.
fn execute_in_sandbox(
    cli: &mut Cli,
    py_file: &str,
    deploy_root: &Path,
    env: &Env,
) -> Option<serde_json::Value> {
    let quoted = snow_api::pyfmt::json_dumps(&serde_json::Value::String(py_file.to_string()), None)
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\'', "\\u0027");
    let script = TEMPLATE.replace("{{py_file | tojson}}", &quoted);
    let warn_err = |cli: &mut Cli, e: &str| {
        cli.console.warning(&format!(
            "Could not fetch Snowpark objects from {py_file} due to {e}, continuing execution for the rest of the Python files."
        ));
    };
    let argv = match interpreter(env) {
        Ok(a) => a,
        Err(e) => {
            warn_err(cli, &format!("Failed to execute Python script. {e}"));
            return None;
        }
    };
    let (code, stdout, stderr) = match run_script(&argv, &script, deploy_root) {
        Ok(r) => r,
        Err(e) => {
            warn_err(cli, &e);
            return None;
        }
    };
    if code != 0 {
        cli.console.warning(&format!(
            "Could not fetch Snowpark objects from {py_file} due to the following error:\n {stderr}"
        ));
        cli.console
            .warning("Continuing execution for the rest of the Python files.");
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(&stdout) {
        Ok(v) => Some(v),
        Err(e) => {
            cli.console.warning(&format!(
                "Could not load JSON into Python due to the following exception: {}",
                json_error(&stdout, &e)
            ));
            cli.console
                .warning("Continuing execution for the rest of the Python files.");
            None
        }
    }
}

/// `edit_setup_script_with_exec_imm_sql(collected_sql_files, deploy_root, generated_root)`.
fn edit_setup_script(
    cli: &mut Cli,
    sql_files: &[PathBuf],
    deploy_root: &Path,
    generated: &Path,
) -> Result<()> {
    let gen_file = generated.join("__generated.sql");
    std::fs::create_dir_all(generated)?;
    if gen_file.exists() {
        cli.console.warning(&format!(
            "            File {} already exists.\n            Could not complete code generation of Snowpark Extension Functions.\n            ",
            gen_file.display()
        ));
        return Ok(());
    }
    let mut text = String::new();
    for f in sql_files {
        let rel = f.strip_prefix(deploy_root).unwrap_or(f);
        text.push_str(&format!("EXECUTE IMMEDIATE FROM '/{}';\n", stage_path(rel)));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gen_file)?
        .write_all(text.as_bytes())?;
    let setup = super::package::setup_script_file(deploy_root)?;
    let code = std::fs::read_to_string(&setup)?;
    let rel = gen_file.strip_prefix(deploy_root).unwrap_or(&gen_file);
    crate::artifacts::write_in_bundle(
        &setup,
        format!("{code}\nEXECUTE IMMEDIATE FROM '/{}';\n", stage_path(rel)),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deannotate_comments_decorators_of_collected_handlers_only() {
        let code = "from snowflake.snowpark.functions import udf\n\n@udf(\n    name='x',\n)\ndef echo(a):\n    return a\n\n@other\ndef keep():\n    pass\n";
        let got = deannotate(code, &["echo".into()]);
        assert_eq!(
            got,
            "from snowflake.snowpark.functions import udf\n\n#: @udf(\n#:     name='x',\n#: )\ndef echo(a):\n    return a\n\n@other\ndef keep():\n    pass"
        );
    }

    #[test]
    fn subscript_decorators_have_no_id() {
        assert!(!decorator_has_id("decos[0]"));
        assert!(decorator_has_id("a.b(x).c"));
        assert!(decorator_has_id("udf(name='x')"));
    }
}
