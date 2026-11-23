//! `snow init`: create a project from a template (local directory or git repository).

use crate::pep440::Version;
use snow_api::identifiers::{is_valid_quoted_identifier, is_valid_unquoted_identifier};
use snow_api::output::Output;
use snow_api::project::PyV;
use snow_api::{Cli, Result, SnowError};
use std::path::{Path, PathBuf};

const DEFAULT_REPO: &str = "https://github.com/snowflakedb/snowflake-cli-templates";

fn invalid(m: impl Into<String>) -> SnowError {
    SnowError::cli(m.into()).named("InvalidTemplateError")
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        let mut kids: Vec<_> = std::fs::read_dir(src)?.filter_map(|e| e.ok()).collect();
        kids.sort_by_key(|e| e.file_name());
        for e in kids {
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// `to_snowflake_identifier` template filter.
fn to_snowflake_identifier(value: String) -> std::result::Result<String, minijinja::Error> {
    if value.is_empty() {
        return Ok(value);
    }
    if is_valid_quoted_identifier(&value) {
        return Ok(value);
    }
    let result = regex::Regex::new(r"[. -]+")
        .expect("re")
        .replace_all(&value, "_")
        .into_owned();
    if !is_valid_unquoted_identifier(&result) {
        return Err(minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            format!(
                "Value '{value}' cannot be converted to valid Snowflake identifier. Consider enclosing it in double quotes: \"\""
            ),
        ));
    }
    Ok(result)
}

/// Python `int(s)`: surrounding whitespace, a sign and digit-group underscores.
fn py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    regex::Regex::new(r"^[+-]?\d+(_\d+)*$")
        .expect("re")
        .is_match(t)
        .then(|| t.replace('_', "").parse().ok())
        .flatten()
}

fn py_float(s: &str) -> Option<f64> {
    let t = s.trim();
    let ok = regex::Regex::new(
        r"(?i)^[+-]?((\d+(_\d+)*)?\.?\d+(_\d+)*(e[+-]?\d+)?|\d+(_\d+)*\.|inf|infinity|nan)$",
    )
    .expect("re");
    ok.is_match(t)
        .then(|| t.replace('_', "").parse().ok())
        .flatten()
}

fn type_name(v: &PyV) -> &'static str {
    match v {
        PyV::None => "NoneType",
        PyV::Bool(_) => "bool",
        PyV::Int(_) => "int",
        PyV::Float(_) => "float",
        PyV::List(_) => "list",
        PyV::Dict(_) => "dict",
        _ => "str",
    }
}

/// pydantic's `input_value` repr, truncated in the middle past 50 characters.
fn input_repr(v: &PyV) -> String {
    let r = v.py_repr();
    let chars: Vec<char> = r.chars().collect();
    if chars.len() <= 50 {
        return r;
    }
    let head: String = chars[..25].iter().collect();
    let tail: String = chars[chars.len() - 24..].iter().collect();
    format!("{head}...{tail}")
}

struct VErr {
    loc: String,
    msg: String,
    kind: &'static str,
    input: PyV,
}

/// `Template(**yaml)` field validation, formatted like a pydantic `ValidationError`.
fn validate_template(meta: &PyV) -> Result<()> {
    let mut errs: Vec<VErr> = Vec::new();
    let mut push = |loc: String, msg: &str, kind: &'static str, input: &PyV| {
        errs.push(VErr {
            loc,
            msg: msg.into(),
            kind,
            input: input.clone(),
        })
    };
    let opt_str =
        |push: &mut dyn FnMut(String, &str, &'static str, &PyV), loc: String, v: Option<&PyV>| {
            if let Some(v) = v
                && !matches!(v, PyV::None | PyV::Str(_))
            {
                push(loc, "Input should be a valid string", "string_type", v);
            }
        };
    let PyV::Dict(d) = meta else {
        return Ok(());
    };
    opt_str(
        &mut push,
        "minimum_cli_version".into(),
        d.get("minimum_cli_version"),
    );
    if let Some(v) = d.get("files_to_render") {
        match v {
            PyV::List(l) => {
                for (i, x) in l.iter().enumerate() {
                    if !matches!(x, PyV::Str(_)) {
                        push(
                            format!("files_to_render.{i}"),
                            "Input should be a valid string",
                            "string_type",
                            x,
                        );
                    }
                }
            }
            other => push(
                "files_to_render".into(),
                "Input should be a valid list",
                "list_type",
                other,
            ),
        }
    }
    if let Some(v) = d.get("variables") {
        match v {
            PyV::List(l) => {
                for (i, x) in l.iter().enumerate() {
                    let PyV::Dict(var) = x else {
                        push(
                            format!("variables.{i}"),
                            "Input should be a valid dictionary or instance of TemplateVariable",
                            "model_type",
                            x,
                        );
                        continue;
                    };
                    match var.get("name") {
                        None => push(
                            format!("variables.{i}.name"),
                            "Field required",
                            "missing",
                            x,
                        ),
                        Some(PyV::Str(_)) => {}
                        Some(other) => push(
                            format!("variables.{i}.name"),
                            "Input should be a valid string",
                            "string_type",
                            other,
                        ),
                    }
                    if let Some(t) = var.get("type")
                        && !matches!(t, PyV::None)
                        && !matches!(t, PyV::Str(s) if ["string", "float", "int"].contains(&s.as_str()))
                    {
                        push(
                            format!("variables.{i}.type"),
                            "Input should be 'string', 'float' or 'int'",
                            "literal_error",
                            t,
                        );
                    }
                    opt_str(
                        &mut push,
                        format!("variables.{i}.prompt"),
                        var.get("prompt"),
                    );
                    opt_str(
                        &mut push,
                        format!("variables.{i}.default_computed"),
                        var.get("default_computed"),
                    );
                }
            }
            other => push(
                "variables".into(),
                "Input should be a valid list",
                "list_type",
                other,
            ),
        }
    }
    if errs.is_empty() {
        return Ok(());
    }
    let n = errs.len();
    let mut out = format!(
        "{n} validation error{} for Template",
        if n == 1 { "" } else { "s" }
    );
    for e in errs {
        out.push_str(&format!(
            "\n{}\n  {} [type={}, input_value={}, input_type={}]\n    For further information visit https://errors.pydantic.dev/2.12/v/{}",
            e.loc,
            e.msg,
            e.kind,
            input_repr(&e.input),
            type_name(&e.input),
            e.kind
        ));
    }
    Err(SnowError::unexpected(out).named("ValidationError"))
}

/// Jinja's end-tag expectations for an open block (`end_token_stack`).
fn looking_for(tag: &str, after_else: bool) -> Vec<&'static str> {
    match (tag, after_else) {
        ("if", false) => vec!["elif", "else", "endif"],
        ("if", true) => vec!["endif"],
        ("for", false) => vec!["endfor", "else"],
        ("for", true) => vec!["endfor"],
        ("macro", _) => vec!["endmacro"],
        ("call", _) => vec!["endcall"],
        ("filter", _) => vec!["endfilter"],
        ("block", _) => vec!["endblock"],
        ("with", _) => vec!["endwith"],
        ("autoescape", _) => vec!["endautoescape"],
        ("set", _) => vec!["endset"],
        _ => vec![],
    }
}

fn line_at(src: &str, pos: usize) -> usize {
    src[..pos.min(src.len())].matches('\n').count() + 1
}

/// Jinja's wording and line for a template syntax error, recovered by scanning the
/// `<!! !!>` tags (MiniJinja reports these differently).
fn jinja_syntax(src: &str, e: &minijinja::Error) -> (usize, String) {
    const KNOWN: &[&str] = &[
        "if",
        "elif",
        "else",
        "endif",
        "for",
        "endfor",
        "macro",
        "endmacro",
        "call",
        "endcall",
        "filter",
        "endfilter",
        "block",
        "endblock",
        "with",
        "endwith",
        "autoescape",
        "endautoescape",
        "set",
        "endset",
        "include",
        "import",
        "from",
        "extends",
        "break",
        "continue",
        "do",
        "raw",
        "endraw",
    ];
    let mut stack: Vec<(&str, bool)> = Vec::new();
    let mut i = 0;
    let mut last_token_line = 1;
    let describe = |stack: &[(&str, bool)]| -> String {
        let mut m = String::new();
        if let Some((t, e)) = stack.last() {
            let tags: Vec<String> = looking_for(t, *e)
                .iter()
                .map(|x| format!("'{x}'"))
                .collect();
            m.push_str(&format!(
                " Jinja was looking for the following tags: {}.",
                tags.join(" or ")
            ));
            m.push_str(&format!(
                " The innermost block that needs to be closed is '{t}'."
            ));
        }
        m
    };
    while i < src.len() {
        let next_block = src[i..].find("<!!").map(|p| p + i);
        let next_var = src[i..].find("<!").map(|p| p + i);
        let Some(start) = next_var else { break };
        if next_block == Some(start) {
            let body_start = start + 3;
            let Some(end) = src[body_start..].find("!!>").map(|p| p + body_start) else {
                break;
            };
            let body = src[body_start..end].trim_start_matches(['-', '+']).trim();
            let name: String = body
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let name_line = line_at(
                src,
                body_start + src[body_start..end].find(&name).unwrap_or(0),
            );
            if !name.is_empty() && !KNOWN.contains(&name.as_str()) {
                return (
                    name_line,
                    format!("Encountered unknown tag '{name}'.{}", describe(&stack)),
                );
            }
            match name.as_str() {
                "elif" => {}
                "else" => {
                    if let Some(top) = stack.last_mut() {
                        top.1 = true;
                    }
                }
                "set" if body.contains('=') => {}
                n if n.starts_with("end") => {
                    stack.pop();
                }
                n if !looking_for(n, false).is_empty() => {
                    let tag = KNOWN.iter().find(|k| **k == n).copied().unwrap_or("if");
                    stack.push((tag, false));
                }
                _ => {}
            }
            last_token_line = line_at(src, end);
            i = end + 3;
        } else {
            let Some(end) = src[start + 2..].find("!>").map(|p| p + start + 2) else {
                break;
            };
            last_token_line = line_at(src, end);
            i = end + 2;
        }
    }
    let detail = e.detail().unwrap_or_default();
    if let Some(expected) = detail.strip_prefix("unexpected end of input") {
        if !stack.is_empty() && expected.contains("end of block") {
            return (
                last_token_line,
                format!("Unexpected end of template.{}", describe(&stack)),
            );
        }
        if let Some(x) = expected.strip_prefix(", expected ") {
            let x = match x {
                "end of variable block" => "end of print statement",
                "end of block" => "end of statement block",
                other => other,
            };
            let line = src.trim_end().matches('\n').count() + 1;
            return (line, format!("unexpected end of template, expected '{x}'."));
        }
    }
    (
        e.line().unwrap_or(1),
        crate::rendering::syntax_message(e, src),
    )
}

/// Jinja's message for a StrictUndefined access in `src` at the error's range.
fn jinja_undefined(
    src: &str,
    e: &minijinja::Error,
    data: &std::collections::BTreeMap<String, minijinja::Value>,
) -> String {
    // Widen the error range leftwards to the start of its dotted name (`x.y.z`).
    let snippet = e
        .range()
        .map(|r| {
            let b = src.as_bytes();
            let mut start = r.start.min(src.len());
            while start > 0
                && (b[start - 1].is_ascii_alphanumeric() || b"_. ".contains(&b[start - 1]))
            {
                start -= 1;
            }
            src.get(start..r.end).unwrap_or(src)
        })
        .unwrap_or(src);
    let re = regex::Regex::new(r"[A-Za-z_]\w*((\s*\.\s*[A-Za-z_]\w*)*)").expect("re");
    for m in re.find_iter(snippet) {
        let parts: Vec<&str> = m.as_str().split('.').map(str::trim).collect();
        let Some(v) = data.get(parts[0]) else {
            if [
                "true", "false", "none", "True", "False", "None", "not", "and", "or", "in", "is",
                "if",
            ]
            .contains(&parts[0])
            {
                continue;
            }
            return format!("'{}' is undefined", parts[0]);
        };
        if parts.len() > 1 {
            let kind = match v.kind() {
                minijinja::value::ValueKind::Number if v.is_integer() => "int",
                minijinja::value::ValueKind::Number => "float",
                minijinja::value::ValueKind::Bool => "bool",
                _ => "str",
            };
            return format!("'{kind} object' has no attribute '{}'", parts[1]);
        }
    }
    e.detail().unwrap_or("undefined value").to_string()
}

pub fn init(cli: &mut Cli) -> Result<Output> {
    let path = cli.str("path").unwrap_or_default();
    if Path::new(&path).exists() {
        return Err(SnowError::click(format!(
            "The directory {path} already exists. Please specify a different path for the project."
        )));
    }
    let template = cli.str("template");
    let source = cli
        .str("template_source")
        .or_else(|| {
            cli.config
                .value(&["cli"], "init_template_source")
                .map(|v| v.py_str())
        })
        .unwrap_or_else(|| DEFAULT_REPO.into());
    let flags: indexmap::IndexMap<String, String> =
        crate::common::parse_key_value_variables(&cli.list("variables"))?
            .into_iter()
            .map(|v| (v.key, v.value))
            .collect();
    if source.to_lowercase().starts_with("http://") {
        return Err(SnowError::cli(
            "Plaintext http:// template sources are not allowed. Use https://, git@, or a local path.",
        ));
    }
    if cli.config.bool_value(&["cli"], "air_gapped", Some(false))? == Some(true)
        && source.trim_end_matches('/') == DEFAULT_REPO
    {
        return Err(SnowError::cli(
            "The default snow init template repository is disabled because cli.air_gapped is set. Set cli.init_template_source or SNOWFLAKE_CLI_INIT_TEMPLATE_SOURCE to a local path or an in-region git URL, or pass --template-source.",
        ));
    }
    let remote = source.starts_with("git@") || source.to_lowercase().starts_with("https://");
    let tmp = tempfile::tempdir()?;
    let root: PathBuf = if remote {
        crate::logs::info(
            "snowflake.cli._plugins.init.commands",
            &format!("Downloading remote template from {source}"),
        );
        let out = std::process::Command::new("git")
            .args(["clone", "--depth", "1", &source])
            .arg(tmp.path())
            .output()
            .map_err(|e| SnowError::unexpected(e.to_string()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).into_owned();
            if regex::Regex::new("fatal: repository '.*' not found")
                .expect("re")
                .is_match(&err)
            {
                return Err(SnowError::click(format!(
                    "Repository '{source}' does not exist"
                )));
            }
            return Err(SnowError::unexpected(err).named("GitCommandError"));
        }
        let root = match &template {
            Some(t) => tmp.path().join(t),
            None => {
                let _ = std::fs::remove_dir_all(tmp.path().join(".git"));
                tmp.path().to_path_buf()
            }
        };
        if !root.exists() {
            return Err(SnowError::click(format!(
                "Template '{}' cannot be found under {source}",
                template.clone().unwrap_or_else(|| "None".into())
            )));
        }
        root
    } else {
        let src = PathBuf::from(&source);
        if !src.exists() {
            return Err(crate::common::file_not_found(&src));
        }
        let origin = match &template {
            Some(t) => src.join(t),
            None => src.clone(),
        };
        crate::logs::info(
            "snowflake.cli._plugins.init.commands",
            &format!("Copying local template from {}", origin.display()),
        );
        if !origin.exists() {
            return Err(SnowError::click(format!(
                "Template '{}' cannot be found under SecurePath(\"{source}\")",
                template.clone().unwrap_or_else(|| "None".into())
            )));
        }
        let name = origin
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        let dst = tmp.path().join(&name);
        copy_tree(&origin, &dst)?;
        dst
    };
    let args_err = "Check whether --template and --template-source arguments are correct.";
    let meta_path = root.join("template.yml");
    if !meta_path.exists() {
        return Err(invalid(format!("File template.yml not found. {args_err}")));
    }
    let meta = snow_api::project::yaml::safe_load(&std::fs::read_to_string(&meta_path)?)
        .map_err(|e| {
            let m = match e {
                snow_api::project::yaml::LoadError::Syntax(m) => m,
                snow_api::project::yaml::LoadError::Duplicate(k) => format!("duplicate key: {k}"),
            };
            SnowError::unexpected(m).named("ParserError")
        })?
        .unwrap_or(PyV::Dict(Default::default()));
    validate_template(&meta)?;
    let files: Vec<String> = meta.str_list("files_to_render");
    for f in &files {
        let full = root.join(f);
        if !full.exists() {
            return Err(invalid(format!(
                "[files_to_render] contains not-existing file: {f}"
            )));
        }
        if full.is_dir() {
            return Err(invalid(format!(
                "[files_to_render] contains a dictionary: {f}"
            )));
        }
    }
    let min = meta.str_of("minimum_cli_version").filter(|m| !m.is_empty());
    if let Some(min) = min
        && Version::parse(&min).ok_or_else(|| {
            SnowError::unexpected(format!("Invalid version: {}", snow_click::py_repr(&min)))
                .named("InvalidVersion")
        })? > Version::parse(snow_api::VERSION).expect("own version")
    {
        return Err(SnowError::click(format!(
            "Snowflake CLI version ({}) is too low - minimum version required by template is {min}. Please upgrade before continuing.",
            snow_api::VERSION
        )));
    }
    let no_interactive = cli.flag("no_interactive");
    let mut data = std::collections::BTreeMap::<String, minijinja::Value>::new();
    for var in meta
        .get("variables")
        .and_then(PyV::as_list)
        .cloned()
        .unwrap_or_default()
    {
        let name = var.str_of("name").unwrap_or_default();
        let ty = var.str_of("type");
        if var.get_some("default").is_some() && var.get_some("default_computed").is_some() {
            return Err(invalid(format!(
                "Variable '{name}' has both 'default' and 'default_computed' set. These are mutually exclusive."
            )));
        }
        let convert = |s: &str| -> std::result::Result<minijinja::Value, String> {
            match ty.as_deref() {
                Some("int") => py_int(s)
                    .map(minijinja::Value::from)
                    .ok_or_else(|| format!("'{s}' is not a valid integer.")),
                Some("float") => py_float(s)
                    .map(minijinja::Value::from)
                    .ok_or_else(|| format!("'{s}' is not a valid float.")),
                _ => Ok(minijinja::Value::from(s.to_string())),
            }
        };
        let value = if let Some(v) = flags.get(&name) {
            convert(v).map_err(|_| {
                let m = match ty.as_deref() {
                    Some("int") => format!(
                        "invalid literal for int() with base 10: {}",
                        snow_click::py_repr(v)
                    ),
                    _ => format!(
                        "could not convert string to float: {}",
                        snow_click::py_repr(v)
                    ),
                };
                SnowError::unexpected(m).named("ValueError")
            })?
        } else {
            let default = match var.get_some("default") {
                Some(d) => Some(d.clone()),
                None => match var.str_of("default_computed") {
                    Some(k) => {
                        let key = match k.as_str() {
                            "connection.account" => "account",
                            "connection.role" => "role",
                            "connection.db" => "database",
                            "connection.schema" => "schema",
                            other => {
                                return Err(invalid(format!(
                                    "Unknown default_computed value: '{other}'. Supported values: 'connection.account', 'connection.role', 'connection.db', 'connection.schema'"
                                )));
                            }
                        };
                        let name = cli.config.default_connection_name();
                        cli.config
                            .connections()
                            .get(&name)
                            .and_then(|c| c.get(key))
                            .map(|v| PyV::Str(v.py_str()))
                    }
                    None => None,
                },
            };
            let as_value = |d: &PyV| match d {
                PyV::Int(i) => minijinja::Value::from(*i),
                PyV::Float(f) => minijinja::Value::from(*f),
                PyV::Bool(b) => minijinja::Value::from(*b),
                other => minijinja::Value::from(snow_api::sanitize::for_terminal(&other.py_str())),
            };
            if no_interactive {
                match &default {
                    Some(d) => as_value(d),
                    None => {
                        return Err(SnowError::click(format!(
                            "Cannot determine value of variable {name}"
                        )));
                    }
                }
            } else {
                let prompt = var
                    .str_of("prompt")
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| name.clone());
                let text = format!(
                    "From template: {}",
                    snow_api::sanitize::for_terminal(&prompt)
                );
                let dshow = default.as_ref().map(|d| d.py_str()).unwrap_or_default();
                loop {
                    let ans = crate::common::prompt_default(cli, &text, &dshow)?;
                    match convert(&ans) {
                        Ok(v) => break v,
                        Err(e) => println!("Error: {e}"),
                    }
                }
            }
        };
        data.insert(name, value);
    }
    data.insert(
        "project_dir_name".into(),
        minijinja::Value::from(
            Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
    );
    data.insert(
        "snowflake_cli_version".into(),
        minijinja::Value::from(snow_api::VERSION),
    );
    let mut env = minijinja::Environment::new();
    env.set_keep_trailing_newline(true);
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    env.set_syntax(
        minijinja::syntax::SyntaxConfig::builder()
            .block_delimiters("<!!", "!!>")
            .variable_delimiters("<!", "!>")
            .build()
            .map_err(|e| SnowError::unexpected(e.to_string()))?,
    );
    // A ClickException raised by the filter propagates as-is, not as a template error.
    let filter_error: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let fe = filter_error.clone();
    env.add_filter("to_snowflake_identifier", move |v: String| {
        to_snowflake_identifier(v)
            .inspect_err(|e| *fe.lock().expect("lock") = e.detail().map(str::to_string))
    });
    for f in &files {
        let full = root.join(f);
        let src = std::fs::read_to_string(&full)?;
        let rendered = env
            .template_from_str(&src)
            .and_then(|t| t.render(&data))
            .map_err(|e| {
                if let Some(m) = filter_error.lock().expect("lock").take() {
                    return SnowError::click(m);
                }
                match e.kind() {
                    minijinja::ErrorKind::SyntaxError => {
                        let (line, msg) = jinja_syntax(&src, &e);
                        invalid(format!(
                            "Invalid template syntax in line {line} of file {f}:\n{msg}"
                        ))
                    }
                    minijinja::ErrorKind::UndefinedError
                    | minijinja::ErrorKind::UnknownMethod
                    | minijinja::ErrorKind::InvalidOperation => {
                        invalid(jinja_undefined(&src, &e, &data))
                    }
                    _ => invalid(format!(
                        "Invalid template syntax in file {f}:\n{}",
                        e.detail().unwrap_or_default()
                    )),
                }
            })?;
        std::fs::write(&full, rendered)?;
    }
    std::fs::remove_file(&meta_path)?;
    if let Some(parent) = Path::new(&path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    copy_tree(&root, Path::new(&path))?;
    Ok(Output::message(format!(
        "Initialized the new project in {path}"
    )))
}
