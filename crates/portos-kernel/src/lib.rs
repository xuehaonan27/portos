//! # portos-kernel
//! kernel library for PortOS.
//!
//! ## Documentation
//! ### docs/architecture-v0.md §3.2
//! Four responsibilities and nothing else.
//!   1. capabilities and policy: [`caps`], [`plancheck`]
//!   2. objects and handles: [`cas`]
//!   3. plugin lifecycle and IPC: [`host`]
//!   4. audit: [`audit`]
//!
//! ### M0 stage implementations
//! Other components:
//!   - a prototype of effect-plan interpreter ([`interp`]).
//!   - consent quadruple ([`consent`]).
//!
//! Plugin domain knowledges MUST NOT appear in this crate, which is an
//! architectural invariant.
//!
//! #### m0-kernel-v0.md
//! TODO:
//! - Currently no sandbox (container, microVM, etc) used, only plain child
//! processes.
//! - CLI consent with a local keyed-MAC stub.
//! - No egress proxy (trait stub only).
//! - Using threads, should be replaced with async later.

pub mod audit;
pub mod caps;
pub mod cas;
pub mod consent;
pub mod db;
pub mod egress;
pub mod host;
pub mod interp;
pub mod metrics;
pub mod plancheck;

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
    pub consent_key: consent::ConsentKey,
}

impl Kernel {
    pub fn open(root: &Path) -> Result<Kernel, KernelError> {
        std::fs::create_dir_all(root)?;
        let conn = db::open(root)?;
        let db = Arc::new(Mutex::new(conn));
        let cas = cas::Cas::new(root, db.clone())?;
        let caps = caps::CapStore::new(db.clone());
        let audit = Arc::new(Mutex::new(audit::AuditLog::open(root)?));
        let consent_key = consent::ConsentKey::load_or_create(root)?;
        Ok(Kernel {
            root: root.to_path_buf(),
            cas,
            caps,
            audit,
            consent_key,
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
        }
    }
}
impl std::error::Error for KernelError {}
