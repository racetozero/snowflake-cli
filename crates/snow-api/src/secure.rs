//! File helpers matching `SecurePath` behaviour (private permissions).

use crate::errors::Result;
use std::path::Path;

pub fn mkdir_p(p: &Path) -> Result<()> {
    if !p.exists() {
        std::fs::create_dir_all(p)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
        }
    }
    Ok(())
}

pub fn touch(p: &Path) -> Result<()> {
    if p.exists() {
        return Ok(());
    }
    write_private(p, "")
}

pub fn restrict(p: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = p;
}

/// Write a file, creating it with 0600 permissions when it is new.
pub(crate) fn write_private(p: &Path, content: &str) -> Result<()> {
    let existed = p.exists();
    std::fs::write(p, content)?;
    if !existed {
        restrict(p);
    }
    Ok(())
}

/// `SecurePath(p).read_text()`: errors name the absolute path as `PosixPath('…')`.
pub fn read_text(p: &Path) -> std::result::Result<String, String> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let abs = crate::stage_path::pure_path(&abs.to_string_lossy());
    let repr = format!("PosixPath({})", snow_click::py_repr(&abs));
    let path = Path::new(&abs);
    if !path.exists() {
        return Err(format!("[Errno 2] No such file or directory: {repr}"));
    }
    if path.is_dir() {
        return Err(format!("[Errno 21] Is a directory: {repr}"));
    }
    std::fs::read_to_string(path)
        .map_err(|e| crate::errors::python_os_error(&e, None) + &format!(": {repr}"))
}
