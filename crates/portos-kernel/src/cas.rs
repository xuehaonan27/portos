//! Content-addressed object store.
//! Layout: <root>/objects/<hh>/<rest-of-hash>. Ingest is tmp + rename;
//! reads stream out as chunked bytes on the data plane — payloads never move
//! through any JSON frame or model context.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use portos_proto::{ArtifactId, ArtifactMeta, Label, artifact::id_for_bytes};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{KernelError, db};

pub struct Cas {
    objects: PathBuf,
    tmp: PathBuf,
    db: Arc<Mutex<Connection>>,
}

impl Cas {
    pub fn new(root: &Path, db: Arc<Mutex<Connection>>) -> Result<Cas, KernelError> {
        let objects = root.join("objects");
        let tmp = root.join("tmp");
        std::fs::create_dir_all(&objects)?;
        std::fs::create_dir_all(&tmp)?;
        Ok(Cas { objects, tmp, db })
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, KernelError> {
        let hexpart = id
            .strip_prefix("blake3:")
            .ok_or_else(|| KernelError::Corrupt(format!("bad artifact id: {id}")))?;
        if hexpart.len() < 3 || !hexpart.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(KernelError::Corrupt(format!("bad artifact id: {id}")));
        }
        Ok(self.objects.join(&hexpart[..2]).join(&hexpart[2..]))
    }

    pub fn put_bytes(
        &self,
        bytes: &[u8],
        r#type: &str,
        labels: Label,
        origin: &str,
    ) -> Result<ArtifactMeta, KernelError> {
        let id = id_for_bytes(bytes);
        let path = self.path_for(&id)?;
        if !path.exists() {
            std::fs::create_dir_all(path.parent().unwrap())?;
            let tmp = self.tmp.join(format!("ingest-{}", rand_suffix()));
            {
                let mut f = File::create(&tmp)?;
                f.write_all(bytes)?;
                f.sync_all()?;
            }
            std::fs::rename(&tmp, &path)?;
        }
        let meta = ArtifactMeta {
            id: id.clone(),
            r#type: r#type.to_string(),
            size: bytes.len() as u64,
            labels,
            origin: origin.to_string(),
            created_at: db::now_unix(),
            ttl_secs: None,
        };
        self.index(&meta)?;
        Ok(meta)
    }

    /// Streaming ingest for large payloads: hash while writing, then rename.
    pub fn put_stream<R: Read>(
        &self,
        mut r: R,
        r#type: &str,
        labels: Label,
        origin: &str,
    ) -> Result<ArtifactMeta, KernelError> {
        let tmp = self.tmp.join(format!("ingest-{}", rand_suffix()));
        let mut hasher = blake3::Hasher::new();
        let mut size: u64 = 0;
        {
            let mut f = File::create(&tmp)?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = r.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                f.write_all(&buf[..n])?;
                size += n as u64;
            }
            f.sync_all()?;
        }
        let id = format!("blake3:{}", hasher.finalize().to_hex());
        let path = self.path_for(&id)?;
        if path.exists() {
            let _ = std::fs::remove_file(&tmp);
        } else {
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::rename(&tmp, &path)?;
        }
        let meta = ArtifactMeta {
            id,
            r#type: r#type.to_string(),
            size,
            labels,
            origin: origin.to_string(),
            created_at: db::now_unix(),
            ttl_secs: None,
        };
        self.index(&meta)?;
        Ok(meta)
    }

    fn index(&self, meta: &ArtifactMeta) -> Result<(), KernelError> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR REPLACE INTO artifacts (id,type,size,labels,origin,created_at,ttl_secs)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                meta.id,
                meta.r#type,
                meta.size as i64,
                serde_json::to_string(&meta.labels).unwrap(),
                meta.origin,
                meta.created_at as i64,
                meta.ttl_secs.map(|t| t as i64),
            ],
        )?;
        Ok(())
    }

    pub fn meta(&self, id: &ArtifactId) -> Result<ArtifactMeta, KernelError> {
        let db = self.db.lock().unwrap();
        let row = db
            .query_row(
                "SELECT id,type,size,labels,origin,created_at,ttl_secs FROM artifacts WHERE id=?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, Option<i64>>(6)?,
                    ))
                },
            )
            .optional()?;
        let (id, ty, size, labels, origin, created_at, ttl) =
            row.ok_or_else(|| KernelError::NotFound(id.clone()))?;
        Ok(ArtifactMeta {
            id,
            r#type: ty,
            size: size as u64,
            labels: serde_json::from_str(&labels)
                .map_err(|e| KernelError::Corrupt(format!("labels json: {e}")))?,
            origin,
            created_at: created_at as u64,
            ttl_secs: ttl.map(|t| t as u64),
        })
    }

    /// Kernel-internal read handle to the object. Dereference to plugins goes
    /// out as a chunked byte stream (host `read` op, decisions-v1.md D25);
    /// the payload never transits any JSON frame or model context.
    pub fn open_read(&self, id: &ArtifactId) -> Result<File, KernelError> {
        let path = self.path_for(id)?;
        Ok(File::open(path)?)
    }

    pub fn set_ref(&self, name: &str, id: &ArtifactId) -> Result<(), KernelError> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR REPLACE INTO refs (name, artifact_id) VALUES (?1,?2)",
            params![name, id],
        )?;
        Ok(())
    }

    pub fn get_ref(&self, name: &str) -> Result<Option<ArtifactId>, KernelError> {
        let db = self.db.lock().unwrap();
        Ok(db
            .query_row(
                "SELECT artifact_id FROM refs WHERE name=?1",
                params![name],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }
}

fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_kernel_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("portos-cas-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn put_get_roundtrip_and_dedup() {
        let root = tmp_kernel_root("rt");
        let db = Arc::new(Mutex::new(crate::db::open(&root).unwrap()));
        let cas = Cas::new(&root, db).unwrap();
        let m1 = cas
            .put_bytes(
                b"hello",
                "text/plain",
                Label::with_integ("test:src"),
                "test",
            )
            .unwrap();
        let m2 = cas
            .put_bytes(
                b"hello",
                "text/plain",
                Label::with_integ("test:src"),
                "test",
            )
            .unwrap();
        assert_eq!(m1.id, m2.id, "content addressing dedups");
        let mut f = cas.open_read(&m1.id).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello");
        let meta = cas.meta(&m1.id).unwrap();
        assert!(meta.labels.integ.contains("test:src"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
