//! # portos-kernel
//! kernel library for PortOS.
//!
//! Four responsibilities and nothing else:
//!   1. capabilities: who may invoke which verb ([`caps`]) — also the join
//!      that builds the model's tool surface, since a granted verb plus the
//!      driver's advertised metadata is a tool definition.
//!   2. objects and handles: the content-addressed store ([`cas`]) behind
//!      the data plane, so payloads never travel through model context.
//!   3. plugin lifecycle and IPC ([`host`]): spawn, verb routing, the event
//!      bus, chunked artifact streaming.
//!   4. audit ([`audit`]): a hash-chained record of what was invoked.
//!
//! Plugin domain knowledges MUST NOT appear in this crate, which is an
//! architectural invariant.
//!
//! TODO:
//! - Currently no sandbox (container, microVM, etc) used, only plain child
//! processes.
//! - Using threads, should be replaced with async later.

pub mod audit;
pub mod caps;
pub mod cas;
pub mod db;
pub mod host;
pub mod metrics;
pub mod routes;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A handle to an opened kernel state directory.
pub struct Kernel {
    pub root: PathBuf,
    // Content Addressed Store
    pub cas: cas::Cas,
    /// Capability table
    pub caps: caps::CapStore,
    pub audit: Arc<Mutex<audit::AuditLog>>,
}

impl Kernel {
    pub fn open(root: &Path) -> Result<Kernel, KernelError> {
        std::fs::create_dir_all(root)?;
        let conn = db::open(root)?;
        let db = Arc::new(Mutex::new(conn));
        let cas = cas::Cas::new(root, db.clone())?;
        let caps = caps::CapStore::new(db.clone());
        let audit = Arc::new(Mutex::new(audit::AuditLog::open(root)?));
        Ok(Kernel {
            root: root.to_path_buf(),
            cas,
            caps,
            audit,
        })
    }
}

/// Errors of portos-kernel.
///
/// TODO: use a solid error crate.
#[derive(Debug)]
pub enum KernelError {
    Io(std::io::Error),
    Db(rusqlite::Error),
    Corrupt(String),
    Denied(String),
    NotFound(String),
    /// Several instances answer the verb and the caller named none.
    Ambiguous(String),
}

impl From<std::io::Error> for KernelError {
    fn from(e: std::io::Error) -> Self {
        KernelError::Io(e)
    }
}
impl From<rusqlite::Error> for KernelError {
    fn from(e: rusqlite::Error) -> Self {
        KernelError::Db(e)
    }
}
impl std::fmt::Display for KernelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KernelError::Io(e) => write!(f, "io: {e}"),
            KernelError::Db(e) => write!(f, "db: {e}"),
            KernelError::Corrupt(s) => write!(f, "corrupt: {s}"),
            KernelError::Denied(s) => write!(f, "denied: {s}"),
            KernelError::NotFound(s) => write!(f, "not found: {s}"),
            KernelError::Ambiguous(s) => write!(f, "ambiguous: {s}"),
        }
    }
}
impl std::error::Error for KernelError {}
