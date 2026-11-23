//! Path helpers mirroring `pathlib` behaviour.

use std::path::{Component, Path, PathBuf};

/// `Path(p).resolve()`: absolute, `..` folded, and symlinks followed (with
/// Windows 8.3 short names expanded) on the longest part of the path that
/// exists. The rest is appended unchanged, so two paths under the same existing
/// directory resolve to the same prefix whether or not they exist yet.
pub fn resolve(p: impl AsRef<Path>) -> PathBuf {
    let abs = std::env::current_dir().unwrap_or_default().join(p);
    if let Ok(c) = abs.canonicalize() {
        return strip_verbatim(c);
    }
    let abs = normalize(&abs);
    let mut missing = Vec::new();
    let mut base = abs.as_path();
    while let (Some(parent), Some(name)) = (base.parent(), base.file_name()) {
        missing.push(name);
        base = parent;
        if let Ok(c) = base.canonicalize() {
            let mut out = strip_verbatim(c);
            out.extend(missing.iter().rev());
            return out;
        }
    }
    abs
}

/// `canonicalize` on Windows returns `\\?\C:\…` or `\\?\UNC\server\share\…`;
/// Python's `resolve()` returns `C:\…` and `\\server\share\…`. Other paths pass through.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    use std::path::Prefix;
    let mut components = p.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return p;
    };
    let mut out = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:", drive as char)),
        Prefix::VerbatimUNC(server, share) => {
            let mut s = std::ffi::OsString::from(r"\\");
            s.push(server);
            s.push(r"\");
            s.push(share);
            PathBuf::from(s)
        }
        _ => return p,
    };
    out.extend(components);
    out
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

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn missing_paths_resolve_under_the_same_prefix_as_their_existing_parent() {
        let root = std::env::temp_dir().join(format!("snow-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let real = resolve(&root);
        assert_eq!(resolve(root.join("a/b")), real.join("a").join("b"));
        assert_eq!(resolve(root.join("a/../c")), real.join("c"));
        assert!(
            !real.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            real.display()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
