//! `snow update` and the update notice (hidden; not part of the upstream command
//! tree, like `snow skill`). Modelled on `atl update` / `kiss update`.
//!
//! In a terminal, a release build checks GitHub for a newer release at most
//! once every 20 hours, in the background while the command runs, and prints
//! one line on stderr after the command. `snow update` runs the release's own
//! installer into a temporary directory (it downloads the archive and checks
//! its SHA-256) and then replaces the running executable. `snow upgrade` keeps
//! upstream's behaviour.

use semver::Version;
use serde_json::{Value, json};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

const REPOSITORY: &str = "racetozero/snowflake-cli";
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/racetozero/snowflake-cli/releases/latest";
const RELEASES_URL: &str = "https://github.com/racetozero/snowflake-cli/releases";
const RAW_REPOSITORY_URL: &str = "https://raw.githubusercontent.com/racetozero/snowflake-cli";
const HOMEBREW_UPGRADE_COMMAND: &str = "brew upgrade racetozero/tap/snowflake-cli";
const CACHE_MAX_AGE_SECS: u64 = 20 * 60 * 60;
const CACHE_FILE: &str = "update-check.json";
const FINISH_WAIT: Duration = Duration::from_millis(800);

type Res<T> = std::result::Result<T, String>;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("snow/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn current_version() -> Res<Version> {
    Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|e| format!("cannot parse the snow version: {e}"))
}

fn parse_release_tag(tag: &str) -> Res<Version> {
    let v = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(v).map_err(|_| format!("the latest GitHub release has an invalid tag `{tag}`"))
}

fn fetch_latest_version() -> Res<Version> {
    let r = agent()
        .get(LATEST_RELEASE_URL)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(404, _) => {
                format!("no snow release is published yet at {RELEASES_URL}")
            }
            e => format!("cannot check the latest snow release on GitHub: {e}"),
        })?;
    let v: Value = serde_json::from_reader(r.into_reader())
        .map_err(|e| format!("cannot read the latest snow release from GitHub: {e}"))?;
    parse_release_tag(v["tag_name"].as_str().unwrap_or_default())
}

/// `SNOW_SELF_UPDATE_COMMAND`, split on spaces (e.g. `cargo install snow --force`).
fn custom_update_command() -> Option<Vec<String>> {
    std::env::var("SNOW_SELF_UPDATE_COMMAND")
        .ok()
        .map(|c| c.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
        .filter(|c| !c.is_empty())
}

/// Fills `{packageName}`, `{version}` and `{installSpec}` for the release.
fn update_command_argv(template: &[String], latest: &Version) -> Vec<String> {
    template
        .iter()
        .map(|a| {
            a.replace("{installSpec}", &format!("snowflake-cli@{latest}"))
                .replace("{packageName}", "snowflake-cli")
                .replace("{version}", &latest.to_string())
        })
        .collect()
}

/// True when the resolved executable is `.../Cellar/snowflake-cli/<version>/bin/snow`.
fn installed_by_homebrew() -> bool {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .is_ok_and(|p| {
            p.ancestors()
                .nth(3)
                .is_some_and(|rack| rack.ends_with("Cellar/snowflake-cli"))
        })
}

fn installer_url(version: &Version) -> String {
    let name = if cfg!(windows) {
        "install.ps1"
    } else {
        "install.sh"
    };
    format!("{RAW_REPOSITORY_URL}/v{version}/{name}")
}

fn download_installer(version: &Version) -> Res<tempfile::TempPath> {
    let url = installer_url(version);
    let r = agent()
        .get(&url)
        .call()
        .map_err(|e| format!("cannot download the snow installer from {url}: {e}"))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut r.into_reader(), &mut bytes)
        .map_err(|e| format!("cannot read the snow installer: {e}"))?;
    let mut f = tempfile::Builder::new()
        .prefix("snow-update-")
        .suffix(if cfg!(windows) { ".ps1" } else { ".sh" })
        .tempfile()
        .map_err(|e| format!("cannot create a temporary installer file: {e}"))?;
    f.write_all(&bytes)
        .and_then(|()| f.flush())
        .map_err(|e| format!("cannot write the temporary snow installer: {e}"))?;
    Ok(f.into_temp_path())
}

fn installer_command(installer: &Path) -> Res<std::process::Command> {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").ok_or("SystemRoot is not set")?;
        let ps = PathBuf::from(root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if !ps.is_file() {
            return Err(format!("PowerShell was not found at {}", ps.display()));
        }
        let mut c = std::process::Command::new(ps);
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(installer);
        Ok(c)
    } else {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg(installer);
        Ok(c)
    }
}

fn run_staged_installer(installer: &Path, version: &Version, staging: &Path) -> Res<PathBuf> {
    let out = installer_command(installer)?
        .current_dir(staging)
        .env("SNOW_VERSION", version.to_string())
        .env("SNOW_INSTALL_DIR", staging)
        .env("SNOW_REPOSITORY", REPOSITORY)
        .env("SNOW_RELEASES_URL", RELEASES_URL)
        .env_remove("SNOW_TARGET")
        .output()
        .map_err(|e| format!("cannot start the snow release installer: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let detail = if err.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            err
        };
        return Err(format!(
            "the snow installer failed ({}): {detail}",
            out.status
        ));
    }
    let staged = staging.join(if cfg!(windows) { "snow.exe" } else { "snow" });
    if !staged.is_file() {
        return Err(format!(
            "the snow installer did not create {}",
            staged.display()
        ));
    }
    Ok(staged)
}

fn do_update() -> Res<(Value, String)> {
    if cfg!(debug_assertions) {
        return Err("`snow update` is not available in debug builds; install a release (see the README) to use it".into());
    }
    let current = current_version()?;
    let latest = fetch_latest_version()?;
    if latest <= current {
        return Ok((
            json!({"version": current.to_string(), "updated": false}),
            format!("snow v{current} is up to date."),
        ));
    }
    if let Some(t) = custom_update_command() {
        let argv = update_command_argv(&t, &latest);
        let shown = argv.join(" ");
        eprintln!("Updating snow from v{current} to v{latest} with `{shown}`...");
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .map_err(|e| format!("cannot start `{shown}` from SNOW_SELF_UPDATE_COMMAND ({e}); check that `{}` is installed and on PATH", argv[0]))?;
        if !status.success() {
            return Err(format!(
                "`{shown}` failed ({status}); fix SNOW_SELF_UPDATE_COMMAND, or remove it to use the snow installer"
            ));
        }
    } else if installed_by_homebrew() {
        return Err(format!(
            "`snow update` cannot replace a snow that Homebrew installed; run `{HOMEBREW_UPGRADE_COMMAND}`, or set SNOW_SELF_UPDATE_COMMAND to that command"
        ));
    } else {
        eprintln!("Updating snow from v{current} to v{latest}...");
        let installer = download_installer(&latest)?;
        let staging = tempfile::tempdir()
            .map_err(|e| format!("cannot create the update staging directory: {e}"))?;
        let staged = run_staged_installer(&installer, &latest, staging.path())?;
        self_replace::self_replace(&staged)
            .map_err(|e| format!("cannot replace the snow executable: {e}"))?;
    }
    Ok((
        json!({"version": latest.to_string(), "previous": current.to_string(), "updated": true}),
        format!("Updated snow from v{current} to v{latest}."),
    ))
}

/// Run `snow update [--help]`; returns the exit code.
pub fn run(args: &[String], agent_mode: bool) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        let help = "Usage: snow update\n\n  Update snow to the latest release (from GitHub, or with SNOW_SELF_UPDATE_COMMAND).\n  A Homebrew install is updated with `brew upgrade racetozero/tap/snowflake-cli`.\n";
        if agent_mode {
            println!(
                "{}",
                json!({"ok": true, "command": "update", "data": {"help": help}})
            );
        } else {
            print!("{help}");
        }
        return 0;
    }
    match do_update() {
        Ok((data, text)) => {
            if agent_mode {
                println!("{}", json!({"ok": true, "command": "update", "data": data}));
            } else {
                println!("{text}");
            }
            0
        }
        Err(msg) => {
            if agent_mode {
                println!(
                    "{}",
                    json!({"ok": false, "command": "update", "error": {"code": "internal", "message": msg}})
                );
            } else {
                eprintln!("Error: {msg}");
            }
            1
        }
    }
}

// ── the notice ────────────────────────────────────────────────────────

fn cache_path() -> PathBuf {
    snow_api::config::snowflake_home_dir().join(CACHE_FILE)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `(latest_version, last_checked_at)` from the cache.
fn read_cache(p: &Path) -> Option<(Version, u64)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    Some((
        Version::parse(v["latest_version"].as_str()?).ok()?,
        v["last_checked_at"].as_u64()?,
    ))
}

fn refresh_cache(p: &Path) -> Res<()> {
    let latest = fetch_latest_version()?;
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let body = json!({"latest_version": latest.to_string(), "last_checked_at": now()});
    std::fs::write(p, serde_json::to_vec_pretty(&body).unwrap_or_default())
        .map_err(|e| e.to_string())
}

fn flag(v: &str) -> bool {
    matches!(
        v.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn eligible(argv: &[String], stderr_terminal: bool, var: impl Fn(&str) -> Option<String>) -> bool {
    let set = |n: &str| var(n).is_some_and(|v| flag(&v));
    if !stderr_terminal || set("CI") || set("SNOW_NO_UPDATE_NOTIFIER") || set("SNOW_AGENT") {
        return false;
    }
    if argv.iter().any(|a| a == "--agent") {
        return false;
    }
    let command = argv.iter().find(|a| !a.starts_with('-'));
    !matches!(
        command.map(String::as_str),
        None | Some("update" | "upgrade" | "sql")
    )
}

fn notice(current: &Version, latest: &Version) -> String {
    let command = if installed_by_homebrew() && custom_update_command().is_none() {
        HOMEBREW_UPGRADE_COMMAND
    } else {
        "snow update"
    };
    format!("Update available: snow v{current} -> v{latest}. Run `{command}`.")
}

/// A pending update check, started before the command runs.
pub struct Check {
    latest: Option<Version>,
    refresh: Option<std::sync::mpsc::Receiver<()>>,
}

/// Start the check when this run may show a notice (release build, stderr is a
/// terminal, no CI, no agent, not `update`/`upgrade`/`sql`).
pub fn start(argv: &[String]) -> Option<Check> {
    if cfg!(debug_assertions) {
        return None;
    }
    let terminal = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if !eligible(argv, terminal, |n| std::env::var(n).ok()) {
        return None;
    }
    let path = cache_path();
    let cache = read_cache(&path);
    let refresh = cache
        .as_ref()
        .is_none_or(|(_, at)| at + CACHE_MAX_AGE_SECS < now())
        .then(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = refresh_cache(&path);
                let _ = tx.send(());
            });
            rx
        });
    let current = current_version().ok()?;
    Some(Check {
        latest: cache.map(|(v, _)| v).filter(|v| *v > current),
        refresh,
    })
}

/// Let a running refresh finish (briefly) and print the notice.
pub fn finish(check: Option<Check>) {
    let Some(c) = check else {
        return;
    };
    if let Some(rx) = c.refresh {
        let _ = rx.recv_timeout(FINISH_WAIT);
    }
    if let (Some(latest), Ok(current)) = (c.latest, current_version()) {
        eprintln!("{}", notice(&current, &latest));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_release_tags() {
        assert_eq!(
            parse_release_tag("v3.36.0").unwrap(),
            Version::new(3, 36, 0)
        );
        assert!(parse_release_tag("release-1").is_err());
    }

    #[test]
    fn custom_command_placeholders() {
        let t = argv(&[
            "mise",
            "use",
            "-g",
            "cargo:{packageName}@{version}",
            "{installSpec}",
        ]);
        assert_eq!(
            update_command_argv(&t, &Version::new(1, 2, 3)),
            argv(&[
                "mise",
                "use",
                "-g",
                "cargo:snowflake-cli@1.2.3",
                "snowflake-cli@1.2.3"
            ])
        );
    }

    #[test]
    fn notice_only_for_interactive_non_agent_commands() {
        let none = |_: &str| None;
        assert!(eligible(&argv(&["object", "list", "table"]), true, none));
        assert!(!eligible(&argv(&["object", "list", "table"]), false, none));
        assert!(!eligible(&argv(&["object", "list", "--agent"]), true, none));
        assert!(!eligible(&argv(&["object", "list"]), true, |n: &str| (n
            == "CI")
            .then(|| "true".to_string())));
        for c in ["update", "upgrade", "sql"] {
            assert!(!eligible(&argv(&[c]), true, none));
        }
        assert!(!eligible(&argv(&[]), true, none));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_update_refuses_before_network() {
        assert!(
            do_update()
                .unwrap_err()
                .contains("not available in debug builds")
        );
    }
}
