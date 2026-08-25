//! SQLite state. Artifact metadata index, capability table, named refs.
//! The CAS payload bytes live in the object directory, never in SQLite.

use rusqlite::Connection;
use std::path::Path;

pub fn open(root: &Path) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open(root.join("kernel.sqlite"))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS artifacts (
            id         TEXT PRIMARY KEY,
            type       TEXT NOT NULL,
            size       INTEGER NOT NULL,
            labels     TEXT NOT NULL,          -- JSON Label
            origin     TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            ttl_secs   INTEGER
        );
        CREATE TABLE IF NOT EXISTS refs (
            name        TEXT PRIMARY KEY,
            artifact_id TEXT NOT NULL REFERENCES artifacts(id)
        );
        CREATE TABLE IF NOT EXISTS caps (
            cap_id  TEXT PRIMARY KEY,
            json    TEXT NOT NULL,             -- full Capability JSON
            parent  TEXT,
            revoked INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS consents (
            nonce      TEXT PRIMARY KEY,
            json       TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        "#,
    )?;
    Ok(conn)
}

/// TODO: move to utility crate later.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
