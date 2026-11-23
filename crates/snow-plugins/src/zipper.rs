//! A ZIP writer laid out like Python's `zipfile.ZipFile(..., ZIP_DEFLATED)` on a
//! seekable file: no data descriptors, no extra fields, zlib level 6 raw deflate,
//! directories stored with a trailing `/`. Archive sizes therefore match upstream.

use std::io::Write;
use std::path::Path;

struct Entry {
    name: Vec<u8>,
    crc: u32,
    csize: u32,
    size: u32,
    method: u16,
    time: u16,
    date: u16,
    external: u32,
    offset: u32,
    utf8: bool,
}

#[derive(Default)]
pub struct ZipWriter {
    buf: Vec<u8>,
    entries: Vec<Entry>,
}

fn dos_time(p: &Path) -> (u16, u16) {
    let t = std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let secs = t
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(315_532_800);
    let dt = chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .naive_utc();
    use chrono::{Datelike, Timelike};
    let date =
        (((dt.year() - 1980).max(0) as u16) << 9) | ((dt.month() as u16) << 5) | dt.day() as u16;
    let time = ((dt.hour() as u16) << 11) | ((dt.minute() as u16) << 5) | (dt.second() as u16 / 2);
    (time, date)
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o100644)
}

#[cfg(not(unix))]
fn mode(p: &Path) -> u32 {
    if p.is_dir() { 0o40755 } else { 0o100644 }
}

impl ZipWriter {
    /// `ZipFile.write(path, arcname)`.
    pub fn add_path(&mut self, path: &Path, arcname: &str) -> std::io::Result<()> {
        let (time, date) = dos_time(path);
        let is_dir = path.is_dir();
        let mut name = arcname.replace('\\', "/");
        while name.starts_with('/') {
            name.remove(0);
        }
        let mut external = (mode(path) & 0xFFFF) << 16;
        let (data, method, crc, size) = if is_dir {
            if !name.ends_with('/') {
                name.push('/');
            }
            external |= 0x10;
            (Vec::new(), 0u16, 0u32, 0u32)
        } else {
            let raw = std::fs::read(path)?;
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
            e.write_all(&raw)?;
            let c = e.finish()?;
            let mut h = flate2::Crc::new();
            h.update(&raw);
            (c, 8u16, h.sum(), raw.len() as u32)
        };
        let utf8 = !name.is_ascii();
        let offset = self.buf.len() as u32;
        let flags: u16 = if utf8 { 0x800 } else { 0 };
        let b = &mut self.buf;
        b.extend_from_slice(&0x04034b50u32.to_le_bytes());
        b.extend_from_slice(&20u16.to_le_bytes());
        b.extend_from_slice(&flags.to_le_bytes());
        b.extend_from_slice(&method.to_le_bytes());
        b.extend_from_slice(&time.to_le_bytes());
        b.extend_from_slice(&date.to_le_bytes());
        b.extend_from_slice(&crc.to_le_bytes());
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&(name.len() as u16).to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(name.as_bytes());
        b.extend_from_slice(&data);
        self.entries.push(Entry {
            name: name.into_bytes(),
            crc,
            csize: data.len() as u32,
            size,
            method,
            time,
            date,
            external,
            offset,
            utf8,
        });
        Ok(())
    }

    pub fn finish(mut self) -> Vec<u8> {
        let start = self.buf.len() as u32;
        let b = &mut self.buf;
        for e in &self.entries {
            b.extend_from_slice(&0x02014b50u32.to_le_bytes());
            b.extend_from_slice(&((3u16 << 8) | 20).to_le_bytes());
            b.extend_from_slice(&20u16.to_le_bytes());
            b.extend_from_slice(&(if e.utf8 { 0x800u16 } else { 0 }).to_le_bytes());
            b.extend_from_slice(&e.method.to_le_bytes());
            b.extend_from_slice(&e.time.to_le_bytes());
            b.extend_from_slice(&e.date.to_le_bytes());
            b.extend_from_slice(&e.crc.to_le_bytes());
            b.extend_from_slice(&e.csize.to_le_bytes());
            b.extend_from_slice(&e.size.to_le_bytes());
            b.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            b.extend_from_slice(&0u16.to_le_bytes());
            b.extend_from_slice(&0u16.to_le_bytes());
            b.extend_from_slice(&0u16.to_le_bytes());
            b.extend_from_slice(&0u16.to_le_bytes());
            b.extend_from_slice(&e.external.to_le_bytes());
            b.extend_from_slice(&e.offset.to_le_bytes());
            b.extend_from_slice(&e.name);
        }
        let size = self.buf.len() as u32 - start;
        let n = self.entries.len() as u16;
        let b = &mut self.buf;
        b.extend_from_slice(&0x06054b50u32.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&n.to_le_bytes());
        b.extend_from_slice(&n.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&start.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        self.buf
    }
}

/// `zipper.IGNORED_FILES` matched with `fnmatch` on the absolute path.
fn ignored(p: &Path) -> bool {
    const PATTERNS: &[&str] = &[
        "**/.DS_Store",
        "**/.git",
        "**/.git/*",
        "**/.gitignore",
        "**/.env",
        "**/.env/*",
        "**/.venv",
        "**/.venv/*",
        "**/__pycache__",
        "**/*.zip",
        "**/*.pyc",
        "**/env",
        "**/env/*",
        "**/ENV",
        "**/ENV/*",
        "**/venv",
        "**/venv/*",
        "**/requirements.txt",
        "**/requirements.snowflake.txt",
        "**/requirements.other.txt",
        "**/snowflake.yml",
    ];
    let s = p.to_string_lossy();
    PATTERNS
        .iter()
        .any(|pat| crate::stage::fnmatchcase(&s, pat))
}

/// `zip_dir(source, dest_zip)`: every non-ignored path under `source` (`glob("**/*")`).
pub fn zip_dir(source: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p)?;
    }
    let abs = std::env::current_dir()?.join(source);
    let mut z = ZipWriter::default();
    for f in crate::artifacts::walk(&abs) {
        if ignored(&f) {
            continue;
        }
        let rel = f
            .strip_prefix(&abs)
            .unwrap_or(&f)
            .to_string_lossy()
            .into_owned();
        z.add_path(&f, &rel)?;
    }
    std::fs::write(dest, z.finish())
}
