//! Append-only audit log with a blake3 hash chain.
//!
//! Each line: {"seq", "ts", "prev", "body", "hash"} where
//! hash = blake3(prev_hex || canonical(body-with-seq-ts)).
//! Verification replays the chain. Any byte flip means a break.
//!
//! ## Documentation
//! ### architecture-v0.md §5.6

use crate::KernelError;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub struct AuditLog {
    path: PathBuf,
    file: File,
    seq: u64,
    prev: String,
}

const GENESIS: &str = "genesis";

impl AuditLog {
    pub fn open(root: &Path) -> Result<AuditLog, KernelError> {
        let path = root.join("audit.log");
        // Recover chain head by scanning (M0: logs are small; an index can come later).
        let (seq, prev) = match File::open(&path) {
            Ok(f) => {
                let mut seq = 0u64;
                let mut prev = GENESIS.to_string();
                for line in BufReader::new(f).lines() {
                    let line = line?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    let v: Value = serde_json::from_str(&line)
                        .map_err(|e| KernelError::Corrupt(format!("audit line: {e}")))?;
                    seq = v["seq"].as_u64().unwrap_or(0) + 1;
                    prev = v["hash"].as_str().unwrap_or(GENESIS).to_string();
                }
                (seq, prev)
            }
            Err(_) => (0, GENESIS.to_string()),
        };
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(AuditLog {
            path,
            file,
            seq,
            prev,
        })
    }

    fn entry_hash(prev: &str, framed: &Value) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(prev.as_bytes());
        hasher.update(serde_json::to_string(framed).unwrap().as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    pub fn append(&mut self, body: Value) -> Result<Value, KernelError> {
        let framed = json!({ "seq": self.seq, "ts": crate::db::now_unix(), "body": body });
        let hash = Self::entry_hash(&self.prev, &framed);
        let mut line = framed;
        line["prev"] = json!(self.prev);
        line["hash"] = json!(hash);
        self.file
            .write_all(serde_json::to_string(&line).unwrap().as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.seq += 1;
        self.prev = hash;
        Ok(line)
    }

    /// Verify the whole chain; returns the entries on success.
    pub fn verify(path: &Path) -> Result<Vec<Value>, KernelError> {
        let f = File::open(path)?;
        let mut prev = GENESIS.to_string();
        let mut out = Vec::new();
        for (i, line) in BufReader::new(f).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(&line)
                .map_err(|e| KernelError::Corrupt(format!("audit line {i}: {e}")))?;
            let framed = json!({ "seq": v["seq"], "ts": v["ts"], "body": v["body"] });
            let expect = Self::entry_hash(&prev, &framed);
            if v["prev"].as_str() != Some(prev.as_str())
                || v["hash"].as_str() != Some(expect.as_str())
            {
                return Err(KernelError::Corrupt(format!(
                    "audit chain broken at line {i}"
                )));
            }
            prev = expect;
            out.push(v);
        }
        Ok(out)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_verifies_and_detects_tamper() {
        let root = std::env::temp_dir().join(format!("portos-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        {
            let mut log = AuditLog::open(&root).unwrap();
            log.append(json!({"event": "a"})).unwrap();
            log.append(json!({"event": "b"})).unwrap();
            log.append(json!({"event": "c"})).unwrap();
        }
        let path = root.join("audit.log");
        assert_eq!(AuditLog::verify(&path).unwrap().len(), 3);

        // reopen appends continue the chain
        {
            let mut log = AuditLog::open(&root).unwrap();
            log.append(json!({"event": "d"})).unwrap();
        }
        assert_eq!(AuditLog::verify(&path).unwrap().len(), 4);

        // tampering breaks it
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"b\"", "\"B\"");
        std::fs::write(&path, text).unwrap();
        assert!(AuditLog::verify(&path).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
