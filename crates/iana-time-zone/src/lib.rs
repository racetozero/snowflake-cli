//! The system time zone name for chrono's `Local` fallback, read from the
//! `/etc/localtime` link (or `/etc/timezone`).
//!
//! chrono reads `TZ` and `/etc/localtime` itself and asks this crate only when
//! both fail. The crates.io version answers on macOS through CoreFoundation,
//! and linking that framework costs every `snow` process about 3 ms of startup.
#![forbid(unsafe_code)]

use std::io::{Error, ErrorKind};

pub fn get_timezone() -> Result<String, Error> {
    if let Ok(target) = std::fs::read_link("/etc/localtime")
        && let Some((_, name)) = target.to_string_lossy().split_once("zoneinfo/")
    {
        return Ok(name.to_owned());
    }
    let name = std::fs::read_to_string("/etc/timezone")?;
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::new(ErrorKind::InvalidData, "/etc/timezone is empty"));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(unix)]
    fn names_the_system_zone() {
        if std::path::Path::new("/etc/localtime").exists() {
            assert!(super::get_timezone().is_ok_and(|n| !n.is_empty() && !n.starts_with('/')));
        }
    }
}
