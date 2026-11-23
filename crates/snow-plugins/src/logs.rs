//! File and console logging (`_app/loggers.py`).

use snow_api::Cli;
use std::io::Write;
use std::sync::Mutex;

struct Logger {
    file: Option<std::fs::File>,
    file_level: u8,
    console_level: u8,
}

static LOGGER: Mutex<Option<Logger>> = Mutex::new(None);

pub const DEBUG: u8 = 10;
pub const INFO: u8 = 20;
pub const WARNING: u8 = 30;
pub const ERROR: u8 = 40;

fn level_name(l: u8) -> &'static str {
    match l {
        DEBUG => "DEBUG",
        INFO => "INFO",
        WARNING => "WARNING",
        _ => "ERROR",
    }
}

/// Configure logging from `[cli.logs]` (`create_initial_loggers` / `create_loggers`).
pub fn init(cli: &mut Cli, verbose: bool, debug: bool) {
    let logs_dir = cli
        .config
        .value(&["cli", "logs"], "path")
        .map(|c| c.py_str())
        .unwrap_or_else(|| {
            cli.config
                .file_path
                .parent()
                .map(|p| p.join("logs").to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let save = cli
        .config
        .value(&["cli", "logs"], "save_logs")
        .and_then(|c| snow_api::config::try_cast_to_bool(&c))
        .unwrap_or(true);
    let level = cli
        .config
        .value(&["cli", "logs"], "level")
        .map(|c| c.py_str().to_lowercase())
        .unwrap_or_else(|| "info".into());
    let mut file_level = match level.as_str() {
        "debug" => DEBUG,
        "warning" | "warn" => WARNING,
        "error" | "critical" => ERROR,
        _ => INFO,
    };
    if debug {
        file_level = DEBUG;
    }
    let console_level = if debug {
        DEBUG
    } else if verbose {
        INFO
    } else {
        ERROR
    };
    let mut global = console_level;
    if save && file_level < global {
        global = file_level;
    }
    cli.connector_info = global <= INFO;
    let mut file = None;
    if save {
        let dir = std::path::Path::new(&logs_dir);
        let _ = snow_api::secure::mkdir_p(dir);
        file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("snowflake-cli.log"))
            .ok();
    }
    if let Ok(mut g) = LOGGER.lock() {
        *g = Some(Logger {
            file,
            file_level,
            console_level,
        });
    }
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn log(level: u8, name: &str, msg: &str) {
    let Ok(mut g) = LOGGER.lock() else { return };
    let Some(l) = g.as_mut() else { return };
    if level >= l.file_level
        && let Some(f) = l.file.as_mut()
    {
        let _ = writeln!(f, "{} {} [{}] {}", now(), level_name(level), name, msg);
    }
    if level >= l.console_level {
        if l.console_level == DEBUG {
            eprintln!("{} {} [{}] {}", now(), level_name(level), name, msg);
        } else {
            eprintln!("{} {} {}", now(), level_name(level), msg);
        }
    }
}

/// A record from a third-party logger whose console format is just the message.
pub fn log_plain(level: u8, msg: &str) {
    let Ok(g) = LOGGER.lock() else { return };
    let Some(l) = g.as_ref() else { return };
    if level >= l.console_level {
        eprintln!("{msg}");
    }
}

pub fn info(name: &str, msg: &str) {
    log(INFO, name, msg);
}
pub fn debug(name: &str, msg: &str) {
    log(DEBUG, name, msg);
}
