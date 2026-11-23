//! Root options: `--info`, `--structure`, `--docs`.

use snow_api::Cli;
use snow_api::output::{Output, Pv, Row};
use snow_click::Spec;

/// How this build was installed, in upstream's `CLIInstallationSource` vocabulary.
pub const INSTALLATION_SOURCE: &str = "binary";

pub fn info(cli: &Cli) -> Output {
    let flags = feature_flags(cli);
    let rows: Vec<Row> = vec![
        ("version", Pv::s(snow_api::display_version())),
        (
            "default_config_file_path",
            Pv::s(cli.config.file_path.display().to_string()),
        ),
        ("python_version", Pv::s("3.12.0 (snow-rs) [Rust]")),
        ("system_info", Pv::s(platform())),
        ("installation_source", Pv::s(INSTALLATION_SOURCE)),
        ("feature_flags", flags),
        (
            "SNOWFLAKE_HOME",
            Pv::opt(std::env::var("SNOWFLAKE_HOME").ok()),
        ),
        (
            "snowflake_connector_python_version",
            Pv::s(snow_connector::CLIENT_APP_VERSION),
        ),
    ]
    .into_iter()
    .map(|(k, v)| {
        [("key".to_string(), Pv::s(k)), ("value".to_string(), v)]
            .into_iter()
            .collect()
    })
    .collect();
    Output::Collection(rows)
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn feature_flags(cli: &Cli) -> Pv {
    let mut out = indexmap::IndexMap::new();
    if let Some(sec) = cli.config.section(&["cli", "features"]) {
        for (k, v) in sec {
            let b = snow_api::config::try_cast_to_bool(&v)
                .map(Pv::Bool)
                .unwrap_or(Pv::s("UNKNOWN"));
            out.insert(k, b);
        }
    }
    Pv::Dict(out)
}

/// `build_command_catalog`.
pub fn structure(spec: &Spec) -> Output {
    Output::Object(Some(node(spec, 0, "snow")))
}

fn node(spec: &Spec, idx: u16, name: &str) -> Row {
    let c = spec.cmd(idx);
    let mut r = Row::new();
    r.insert("name".into(), Pv::s(name));
    r.insert("help".into(), Pv::s(c.structure_help));
    let mut args = Vec::new();
    let mut opts = Vec::new();
    for p in c.params {
        let par = spec.param(*p);
        if par.hidden {
            continue;
        }
        let mut o = Row::new();
        o.insert("name".into(), Pv::s(par.name));
        o.insert("help".into(), Pv::s(par.help.unwrap_or("").trim()));
        o.insert("required".into(), Pv::Bool(par.required));
        if !par.is_argument {
            o.insert(
                "flags".into(),
                Pv::List(
                    par.opts
                        .iter()
                        .chain(par.secondary.iter())
                        .map(|s| Pv::s(*s))
                        .collect(),
                ),
            );
            if par.is_flag {
                o.insert("is_flag".into(), Pv::Bool(true));
            }
        }
        if let snow_click::spec::Ty::Choice { choices, .. } = par.ty
            && !choices.is_empty()
        {
            o.insert(
                "choices".into(),
                Pv::List(choices.iter().map(|s| Pv::s(*s)).collect()),
            );
        }
        if let Some(d) = par.structure_default
            && let Ok(j) = serde_json::from_str::<serde_json::Value>(d)
        {
            o.insert("default".into(), Pv::from_json(&j));
        }
        if par.is_argument {
            args.push(Pv::Dict(o));
        } else {
            opts.push(Pv::Dict(o));
        }
    }
    if !args.is_empty() {
        r.insert("arguments".into(), Pv::List(args));
    }
    if !opts.is_empty() {
        r.insert("options".into(), Pv::List(opts));
    }
    let mut children: Vec<(&str, u16)> =
        c.children.iter().map(|i| (spec.cmd(*i).name, *i)).collect();
    children.sort_by(|a, b| a.0.cmp(b.0));
    let kids: Vec<Pv> = children
        .into_iter()
        .filter(|(_, i)| !spec.cmd(*i).hidden)
        .map(|(n, i)| Pv::Dict(node(spec, i, n)))
        .collect();
    if !kids.is_empty() {
        r.insert("commands".into(), Pv::List(kids));
    }
    r
}
