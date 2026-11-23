//! Path helpers mirroring `pathlib` behaviour.

use std::path::{Component, Path, PathBuf};

/// `Path(p).resolve()` (absolute, `..` removed, symlinks followed when they exist).
pub fn resolve(p: &str) -> String {
    let path = Path::new(p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    if let Ok(c) = std::fs::canonicalize(&abs) {
        return c.to_string_lossy().into_owned();
    }
    normalize(&abs).to_string_lossy().into_owned()
}

pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
