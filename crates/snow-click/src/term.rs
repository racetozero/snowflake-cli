//! Terminal width and colour detection as Rich and Click do it.

use std::io::IsTerminal;

fn env_columns() -> Option<usize> {
    std::env::var("COLUMNS")
        .ok()
        .filter(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|c| c.parse().ok())
}

static SIZE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();

/// The width the terminal reports, if any standard stream is a terminal.
pub fn terminal_width() -> Option<usize> {
    *SIZE.get_or_init(|| {
        if std::io::stdin().is_terminal()
            || std::io::stdout().is_terminal()
            || std::io::stderr().is_terminal()
        {
            crate::termsize::width()
        } else {
            None
        }
    })
}

/// `rich.console.Console().width`.
pub fn rich_width() -> usize {
    if let Ok(w) = std::env::var("TERMINAL_WIDTH")
        && let Ok(n) = w.parse::<usize>()
    {
        return n;
    }
    if let Some(c) = env_columns() {
        return c;
    }
    terminal_width().unwrap_or(80)
}

/// `click.formatting.HelpFormatter().width`.
pub fn click_width() -> usize {
    let cols = env_columns().or_else(|| {
        if std::io::stdout().is_terminal() {
            crate::termsize::width()
        } else {
            None
        }
    });
    let cols = cols.unwrap_or(80);
    (cols.min(80).saturating_sub(2)).max(50)
}

/// Whether Rich would emit colour on the given stream.
pub fn color(stderr: bool) -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if ["FORCE_COLOR", "PY_COLORS", "GITHUB_ACTIONS"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
    {
        return true;
    }
    if std::env::var("TERM").is_ok_and(|t| t == "dumb" || t == "unknown") {
        return false;
    }
    if stderr {
        std::io::stderr().is_terminal()
    } else {
        std::io::stdout().is_terminal()
    }
}
