//! # portos-kernel
//! kernel library for PortOS.
//!
//! ## Documentation
//! ### docs/architecture-v0.md §3.2
//! Four responsibilities and nothing else.
//!   1. capabilities and policy: [`caps`], [`plancheck`]
//!   2. objects and handles: [`cas`], and holdings: [`ledger`] (the F1/F2
//!      resource ledger from `portos-rm`, persisted here)
//!   3. plugin lifecycle and IPC: [`host`]
//!   4. audit: [`audit`]
//!
//! ### M0 stage implementations
//! Other components:
//!   - plan runs: admission ([`plancheck`]) → consent ([`consent`]) → run
//!     under the F3 monitor ([`plans`], WP-06).
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
pub mod host;
pub mod ledger;
pub mod metrics;
pub mod plan_ir;
pub mod plancheck;
pub mod plans;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A handle to an opened kernel state directory.
pub struct Kernel {
    pub root: PathBuf,
    // Content Addressed Store
    pub cas: cas::Cas,
    /// Capability table
    pub caps: caps::CapStore,
    /// Holding ledger (counting-budget pools, plugin and subscription holdings).
    pub ledger: Arc<ledger::LedgerStore>,
    pub audit: Arc<Mutex<audit::AuditLog>>,
    pub consent_key: consent::ConsentKey,
    /// The shared SQLite connection (holdings, journal, substrate, plan runs).
    pub(crate) db: Arc<Mutex<rusqlite::Connection>>,
}

impl Kernel {
    pub fn open(root: &Path) -> Result<Kernel, KernelError> {
        std::fs::create_dir_all(root)?;
        let conn = db::open(root)?;
        let db = Arc::new(Mutex::new(conn));
        let cas = cas::Cas::new(root, db.clone())?;
        let (ledger, report) = ledger::LedgerStore::open(db.clone())?;
        let ledger = Arc::new(ledger);
        let caps = caps::CapStore::new(db.clone(), ledger.clone());
        let audit = Arc::new(Mutex::new(audit::AuditLog::open(root)?));
        if report.stale_rows > 0 || !report.substrate.is_empty() {
            let _ = audit.lock().unwrap().append(serde_json::json!({
                "event": "ledger.reconciled", "stale_rows": report.stale_rows,
                "substrate": {
                    "process_killed": report.substrate.process_killed,
                    "process_tombstoned": report.substrate.process_tombstoned,
                    "ports_tombstoned": report.substrate.ports_tombstoned,
                    "ports_still_bound": report.substrate.ports_still_bound,
                    "locks_removed": report.substrate.locks_removed,
                    "locks_kept": report.substrate.locks_kept,
                },
                "note": "previous kernel process reconciled: plugin/subscription rows tombstoned, substrate classes checked against the world",
            }));
        }
        if report.journal_pending > 0 {
            let _ = audit.lock().unwrap().append(serde_json::json!({
                "event": "ledger.journal_pending", "pending": report.journal_pending,
                "note": "failed teardown actions replayed by the next teardown of their subject",
            }));
        }
        let consent_key = consent::ConsentKey::load_or_create(root)?;
        Ok(Kernel {
            root: root.to_path_buf(),
            cas,
            caps,
            ledger,
            audit,
            consent_key,
            db,
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
