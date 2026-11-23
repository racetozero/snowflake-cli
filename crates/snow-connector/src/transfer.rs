//! PUT and GET (file transfer to and from stages), after the Python
//! connector's `SnowflakeFileTransferAgent`: the server answers the statement
//! with a transfer plan (`command`, `src_locations`, `stageInfo`), the client
//! moves the files and builds the result rows itself.

use crate::{Column, Connection, Cursor, DbError, ErrorKind};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Whether `sql` is a PUT or GET statement, which the client executes.
pub fn is_file_transfer(sql: &str) -> bool {
    let t = strip_leading_comments(sql).to_ascii_uppercase();
    t.starts_with("PUT ")
        || t.starts_with("GET ")
        || t.starts_with("PUT\n")
        || t.starts_with("GET\n")
}

pub(crate) fn strip_leading_comments(sql: &str) -> &str {
    let mut s = sql.trim_start();
    loop {
        if let Some(r) = s.strip_prefix("--") {
            s = r
                .split_once('\n')
                .map(|(_, b)| b)
                .unwrap_or("")
                .trim_start();
        } else if let Some(r) = s.strip_prefix("/*") {
            s = r
                .split_once("*/")
                .map(|(_, b)| b)
                .unwrap_or("")
                .trim_start();
        } else {
            return s;
        }
    }
}

fn perr(errno: i64, msg: String) -> DbError {
    DbError::new(ErrorKind::Programming, errno, None, msg)
}

/// `fnmatch.translate` for one path component.
fn fnmatch_regex(pat: &str) -> regex::Regex {
    let mut r = String::from("^");
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            '*' => r.push_str(".*"),
            '?' => r.push('.'),
            '[' => {
                let mut j = i;
                if j < chars.len() && chars[j] == '!' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    r.push_str("\\[");
                } else {
                    let stuff: Vec<char> = chars[i..j].to_vec();
                    i = j + 1;
                    let (neg, body) = match stuff.split_first() {
                        Some(('!', rest)) => (true, rest),
                        _ => (false, &stuff[..]),
                    };
                    // Every member is literal except `-` ranges (Rust regex nests `[`).
                    let class: String = body
                        .iter()
                        .map(|c| {
                            if *c == '-' {
                                "-".to_string()
                            } else {
                                format!("\\x{{{:x}}}", *c as u32)
                            }
                        })
                        .collect();
                    r.push_str(&format!("[{}{class}]", if neg { "^" } else { "" }));
                }
            }
            other => r.push_str(&regex::escape(&other.to_string())),
        }
    }
    r.push('$');
    regex::Regex::new(&r).unwrap_or_else(|_| regex::Regex::new("^$").expect("re"))
}

fn has_magic(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// `glob.glob(pattern)` (relative patterns resolve against the working directory).
pub fn glob_paths(pattern: &str) -> Vec<PathBuf> {
    let abs = if pattern.starts_with('/') {
        pattern.to_string()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(pattern)
            .to_string_lossy()
            .into_owned()
    };
    glob(&abs)
}

/// `glob.glob(pattern)` for an absolute pattern (sorted).
fn glob(pattern: &str) -> Vec<PathBuf> {
    if !has_magic(pattern) {
        return if Path::new(pattern).exists() {
            vec![PathBuf::from(pattern)]
        } else {
            vec![]
        };
    }
    let mut cur: Vec<PathBuf> = vec![PathBuf::from("/")];
    for comp in pattern.split('/').filter(|c| !c.is_empty()) {
        let mut next = Vec::new();
        for base in &cur {
            if has_magic(comp) {
                let re = fnmatch_regex(comp);
                let Ok(rd) = std::fs::read_dir(base) else {
                    continue;
                };
                let mut names: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                for n in names {
                    if n.starts_with('.') && !comp.starts_with('.') {
                        continue;
                    }
                    if re.is_match(&n) {
                        next.push(base.join(n));
                    }
                }
            } else {
                let p = base.join(comp);
                if p.exists() || std::fs::symlink_metadata(&p).is_ok() {
                    next.push(p);
                }
            }
        }
        cur = next;
    }
    cur
}

fn expand_user(p: &str) -> String {
    if let Some(rest) = p.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
        && let Ok(h) = std::env::var("HOME")
    {
        return format!("{h}{rest}");
    }
    p.to_string()
}

/// A detected or requested source compression (`CompressionTypes`).
#[derive(Clone, Copy, PartialEq)]
struct Compression {
    name: &'static str,
    supported: bool,
}

fn by_subtype(s: &str) -> Option<Compression> {
    let s = s.to_lowercase();
    let (name, supported) = match s.as_str() {
        "gzip" | "x-gzip" => ("GZIP", true),
        "zlib" | "deflate" => ("DEFLATE", true),
        "raw_deflate" => ("RAW_DEFLATE", true),
        "bzip2" | "x-bzip2" | "x-bz2" | "x-bzip" | "bz2" => ("BZIP2", true),
        "lzip" | "x-lzip" => ("LZIP", false),
        "lzma" | "x-lzma" => ("LZMA", false),
        "lzo" | "x-lzo" => ("LZO", false),
        "xz" | "x-xz" => ("XZ", false),
        "compress" | "x-compress" => ("COMPRESS", false),
        "parquet" => ("PARQUET", true),
        "zstd" | "x-zstd" => ("ZSTD", true),
        "br" | "x-br" => ("BROTLI", true),
        "orc" => ("ORC", true),
        _ => return None,
    };
    Some(Compression { name, supported })
}

fn compression_repr(c: &Compression) -> String {
    let (ext, mime, subs): (&str, &str, &str) = match c.name {
        "GZIP" => (".gz", "application", "['gzip', 'x-gzip']"),
        "DEFLATE" => (".deflate", "application", "['zlib', 'deflate']"),
        "RAW_DEFLATE" => (".raw_deflate", "application", "['raw_deflate']"),
        "BZIP2" => (
            ".bz2",
            "application",
            "['bzip2', 'x-bzip2', 'x-bz2', 'x-bzip', 'bz2']",
        ),
        "LZIP" => (".lz", "application", "['lzip', 'x-lzip']"),
        "LZMA" => (".lzma", "application", "['lzma', 'x-lzma']"),
        "LZO" => (".lzo", "application", "['lzo', 'x-lzo']"),
        "XZ" => (".xz", "application", "['xz', 'x-xz']"),
        "COMPRESS" => (".Z", "application", "['compress', 'x-compress']"),
        "PARQUET" => (".parquet", "snowflake", "['parquet']"),
        "ZSTD" => (".zst", "application", "['zstd', 'x-zstd']"),
        "BROTLI" => (".br", "application", "['br', 'x-br']"),
        _ => (".orc", "snowflake", "['orc']"),
    };
    format!(
        "CompressionType(name='{}', file_extension='{ext}', mime_type='{mime}', mime_subtypes={subs}, is_supported={})",
        c.name,
        if c.supported { "True" } else { "False" }
    )
}

/// `mimetypes.guess_type(name)[1]` plus the connector's magic-number checks.
fn detect(path: &Path) -> Option<&'static str> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lower = name.to_lowercase();
    let enc = [
        (".tgz", "gzip"),
        (".taz", "gzip"),
        (".tz", "gzip"),
        (".tbz2", "bzip2"),
        (".txz", "xz"),
    ]
    .iter()
    .find(|(s, _)| lower.ends_with(s))
    .map(|(_, e)| *e)
    .or_else(|| {
        [
            (".gz", "gzip"),
            (".Z", "compress"),
            (".bz2", "bzip2"),
            (".xz", "xz"),
            (".br", "br"),
        ]
        .iter()
        .find(|(s, _)| {
            if *s == ".Z" {
                name.ends_with(s)
            } else {
                lower.ends_with(s)
            }
        })
        .map(|(_, e)| *e)
    });
    if enc.is_some() {
        return enc;
    }
    let mut head = [0u8; 4];
    let n = std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    let head = &head[..n];
    if name.ends_with(".br") {
        Some("br")
    } else if head.len() >= 3 && &head[..3] == b"ORC" {
        Some("orc")
    } else if head == b"PAR1" {
        Some("parquet")
    } else if head == [0x28, 0xb5, 0x2f, 0xfd] {
        Some("zstd")
    } else {
        None
    }
}

/// `SnowflakeFileUtil.compress_file_with_gzip` (level 9, normalised header).
fn gzip(path: &Path) -> std::io::Result<Vec<u8>> {
    let data = std::fs::read(path)?;
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fname = format!("{base}_c");
    let latin1 = fname.chars().all(|c| (c as u32) < 256);
    let mut b = flate2::GzBuilder::new().mtime(0).operating_system(255);
    if latin1 && !fname.is_empty() {
        b = b.filename(" ".repeat(fname.chars().count()));
    }
    let mut e = b.write(Vec::new(), flate2::Compression::new(9));
    e.write_all(&data)?;
    e.finish()
}

struct Meta {
    name: String,
    src: PathBuf,
    src_size: u64,
    dst_name: String,
    dst_size: u64,
    src_comp: Option<Compression>,
    dst_comp: Option<Compression>,
    require_compress: bool,
    status: &'static str,
}

fn text_col(name: &str) -> Column {
    Column {
        name: name.into(),
        type_name: "text".into(),
        scale: None,
    }
}

fn fixed_col(name: &str) -> Column {
    Column {
        name: name.into(),
        type_name: "fixed".into(),
        scale: Some(0),
    }
}

pub(crate) fn execute(conn: &mut Connection, sql: &str) -> Result<Cursor, DbError> {
    let v = conn.query_request(sql, false, None)?;
    if v["success"].as_bool() != Some(true) {
        return conn.cursor_from(sql, v);
    }
    let data = &v["data"];
    let Some(command) = data["command"].as_str() else {
        return Err(DbError::new(
            ErrorKind::Database,
            253002,
            None,
            "Failed to parse server's response",
        ));
    };
    let Some(locations) = data["src_locations"].as_array() else {
        return Err(DbError::new(
            ErrorKind::Database,
            253002,
            None,
            "Failed to parse the location",
        ));
    };
    let locations: Vec<String> = locations
        .iter()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect();
    let stage = &data["stageInfo"];
    let loc_type = stage["locationType"]
        .as_str()
        .unwrap_or_default()
        .to_uppercase();
    if !["LOCAL_FS", "S3", "AZURE", "GCS"].contains(&loc_type.as_str()) {
        return Err(DbError::new(
            ErrorKind::Operational,
            253001,
            None,
            format!("Destination location type is not valid: {loc_type}"),
        ));
    }
    let mut storage = crate::storage::Storage::new(conn, &loc_type, stage, data, sql)?;
    let qid = data["queryId"].as_str().map(str::to_string);
    let mut cursor = Cursor {
        query: sql.to_string(),
        sfqid: qid,
        ..Default::default()
    };
    if command == "UPLOAD" {
        let mut files = Vec::new();
        for l in &locations {
            let mut f = expand_user(l);
            if !f.starts_with('/') {
                f = std::env::current_dir()
                    .unwrap_or_default()
                    .join(&f)
                    .to_string_lossy()
                    .into_owned();
            }
            files.extend(glob(&f));
        }
        if files.is_empty() {
            let repr = format!(
                "[{}]",
                locations
                    .iter()
                    .map(|l| format!("'{}'", l.replace('\\', "\\\\").replace('\'', "\\'")))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Err(perr(253006, format!("File doesn't exist: {repr}")));
        }
        let auto_compress = data
            .get("autoCompress")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let source_compression = data["sourceCompression"]
            .as_str()
            .unwrap_or("")
            .to_lowercase();
        let overwrite = data["overwrite"].as_bool().unwrap_or(false);
        let mut metas = Vec::new();
        for f in &files {
            let s = f.to_string_lossy().into_owned();
            if !f.exists() {
                return Err(perr(253006, format!("File doesn't exist: {s}")));
            }
            if f.is_dir() {
                return Err(perr(253006, format!("Not a file but a directory: {s}")));
            }
            let name = f
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            metas.push(Meta {
                dst_name: name.clone(),
                name,
                src: f.clone(),
                src_size: std::fs::metadata(f).map(|m| m.len()).unwrap_or(0),
                dst_size: 0,
                src_comp: None,
                dst_comp: None,
                require_compress: false,
                status: "ERROR",
            });
        }
        let (auto_detect, user) = match source_compression.as_str() {
            "auto_detect" => (true, None),
            "none" | "" => (false, None),
            other => match by_subtype(other).filter(|c| c.supported) {
                Some(c) => (false, Some(c)),
                None => {
                    let r = by_subtype(other)
                        .map(|c| compression_repr(&c))
                        .unwrap_or_else(|| "None".into());
                    return Err(perr(253007, format!("Feature is not supported: {r}")));
                }
            },
        };
        for m in metas.iter_mut() {
            let c = if auto_detect {
                let c = detect(&m.src).and_then(by_subtype);
                if let Some(c) = c.filter(|c| !c.supported) {
                    return Err(perr(
                        253007,
                        format!("Feature is not supported: {}", compression_repr(&c)),
                    ));
                }
                c
            } else {
                user
            };
            match c {
                Some(c) => {
                    m.src_comp = Some(c);
                    m.dst_comp = Some(c);
                    m.require_compress = false;
                    m.dst_name = m.name.clone();
                }
                None => {
                    m.require_compress = auto_compress;
                    if auto_compress {
                        m.dst_name = format!("{}.gz", m.name);
                        m.dst_comp = Some(Compression {
                            name: "GZIP",
                            supported: true,
                        });
                    } else {
                        m.dst_name = m.name.clone();
                    }
                }
            }
        }
        for m in metas.iter_mut() {
            let body = if m.require_compress {
                gzip(&m.src).map_err(|e| perr(253003, e.to_string()))?
            } else {
                std::fs::read(&m.src).map_err(|e| perr(253003, e.to_string()))?
            };
            match storage.upload(conn, &m.dst_name, &body, overwrite)? {
                true => {
                    m.dst_size = body.len() as u64;
                    m.status = "UPLOADED";
                }
                false => {
                    m.dst_size = 0;
                    m.status = "SKIPPED";
                }
            }
        }
        cursor.columns = vec![
            text_col("source"),
            text_col("target"),
            fixed_col("source_size"),
            fixed_col("target_size"),
            text_col("source_compression"),
            text_col("target_compression"),
            text_col("status"),
            text_col("message"),
        ];
        let mut rows: Vec<Vec<Option<String>>> = metas
            .iter()
            .map(|m| {
                vec![
                    Some(m.name.clone()),
                    Some(m.dst_name.clone()),
                    Some(m.src_size.to_string()),
                    Some(m.dst_size.to_string()),
                    Some(m.src_comp.map(|c| c.name).unwrap_or("NONE").to_string()),
                    Some(m.dst_comp.map(|c| c.name).unwrap_or("NONE").to_string()),
                    Some(m.status.to_string()),
                    Some(String::new()),
                ]
            })
            .collect();
        rows.sort_by(|a, b| {
            py_row_cmp(
                a,
                b,
                &[false, false, true, true, false, false, false, false],
            )
        });
        cursor.rows = rows;
    } else {
        let local = expand_user(data["localLocation"].as_str().unwrap_or_default());
        if !Path::new(&local).is_dir() {
            return Err(perr(
                253005,
                format!("The local path is not a directory: {local}"),
            ));
        }
        if locations.is_empty() {
            return Err(DbError::new(
                ErrorKind::Operational,
                253006,
                None,
                "While getting file(s) there was an error: the file does not exist.",
            ));
        }
        let mut rows = Vec::new();
        for (idx, src) in locations.iter().enumerate() {
            let base = Path::new(src)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let body = storage.download(conn, idx, src)?;
            let dst = Path::new(&local).join(&base);
            std::fs::write(&dst, &body).map_err(|e| perr(253003, e.to_string()))?;
            let size = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
            rows.push(vec![
                Some(base),
                Some(size.to_string()),
                Some("DOWNLOADED".into()),
                Some(String::new()),
            ]);
        }
        rows.sort_by(|a, b| py_row_cmp(a, b, &[false, true, false, false]));
        cursor.columns = vec![
            text_col("file"),
            fixed_col("size"),
            text_col("status"),
            text_col("message"),
        ];
        cursor.rows = rows;
    }
    cursor.rowcount = Some(cursor.rows.len() as i64);
    cursor.raw = json!({});
    Ok(cursor)
}

/// `sorted(rowset)`: lexicographic over cells, numbers compared numerically.
fn py_row_cmp(a: &[Option<String>], b: &[Option<String>], numeric: &[bool]) -> std::cmp::Ordering {
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let o = if numeric.get(i) == Some(&true) {
            let p = |v: &Option<String>| {
                v.as_deref()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0)
            };
            p(x).cmp(&p(y))
        } else {
            x.cmp(y)
        };
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
    std::cmp::Ordering::Equal
}
