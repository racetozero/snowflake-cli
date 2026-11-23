//! Stage storage backends for PUT/GET (`*_storage_client.py`).

use crate::{Connection, DbError, ErrorKind};
use serde_json::Value;
use std::path::PathBuf;

pub(crate) enum Storage {
    /// `LOCAL_FS`: the stage is a directory (used by test servers).
    Local { location: PathBuf },
    /// A cloud stage (`S3`, `AZURE`, `GCS`).
    Remote(Box<crate::cloud::Remote>),
}

impl Storage {
    pub(crate) fn new(
        conn: &Connection,
        kind: &str,
        stage: &Value,
        data: &Value,
        command: &str,
    ) -> Result<Storage, DbError> {
        Ok(match kind {
            "LOCAL_FS" => Storage::Local {
                location: PathBuf::from(stage["location"].as_str().unwrap_or_default()),
            },
            _ => Storage::Remote(Box::new(crate::cloud::Remote::new(
                conn, kind, stage, data, command,
            )?)),
        })
    }

    /// Store `body` as `name`; `false` when the file exists and is kept.
    pub(crate) fn upload(
        &mut self,
        conn: &mut Connection,
        name: &str,
        body: &[u8],
        overwrite: bool,
    ) -> Result<bool, DbError> {
        match self {
            Storage::Local { location } => {
                let dst = location.join(name);
                std::fs::write(&dst, body).map_err(|e| {
                    DbError::new(ErrorKind::Operational, 253003, None, e.to_string())
                })?;
                Ok(true)
            }
            Storage::Remote(r) => r.upload(
                &mut |c| conn.query_request(c, false, None),
                name,
                body,
                overwrite,
            ),
        }
    }

    pub(crate) fn download(
        &mut self,
        conn: &mut Connection,
        idx: usize,
        src: &str,
    ) -> Result<Vec<u8>, DbError> {
        match self {
            Storage::Local { location } => {
                std::fs::read(location.join(src.trim_start_matches('/')))
                    .map_err(|e| DbError::new(ErrorKind::Operational, 253003, None, e.to_string()))
            }
            Storage::Remote(r) => r.download(&mut |c| conn.query_request(c, false, None), idx, src),
        }
    }
}
