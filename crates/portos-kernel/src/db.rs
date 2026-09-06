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
        -- Holding ledger (spec F1): one row per fragment, release = tombstone,
        -- the composed value is only ever recomputed. Mirrors
        -- crates/portos-rm/schema.sql `holding`; write-through from ledger.rs.
        CREATE TABLE IF NOT EXISTS holdings (
            id               INTEGER PRIMARY KEY,
            subject          TEXT NOT NULL,
            class_id         TEXT NOT NULL,
            instance         TEXT NOT NULL,
            frag             TEXT NOT NULL,      -- JSON fragment (ledger.rs::frag_to_json)
            generation       TEXT NOT NULL,
            parent           INTEGER REFERENCES holdings(id),
            lease_expires_at INTEGER,
            acquired_at      INTEGER NOT NULL,
            released_at      INTEGER
        );
        CREATE INDEX IF NOT EXISTS holdings_live
            ON holdings(class_id, instance) WHERE released_at IS NULL;
        CREATE INDEX IF NOT EXISTS holdings_subject
            ON holdings(subject) WHERE released_at IS NULL;
        -- F2 teardown journal (saga-log, spec §6.2 [SAGA]): one row per
        -- holding ever torn down, written in the same transaction as the
        -- tombstones. Non-'done' rows are world actions still owed; they are
        -- replayed by the next teardown of their subject. Reference DDL:
        -- crates/portos-rm/schema.sql `teardown_journal`.
        CREATE TABLE IF NOT EXISTS journal (
            holding_id INTEGER PRIMARY KEY,
            grade      TEXT NOT NULL,   -- "inverse" | "compensable" | "external"
            idem_key   TEXT NOT NULL,   -- [KEY] dedupe key, stable across crashes
            state      TEXT NOT NULL,   -- "pending" | "in_flight" | "done" | "failed"
            updated_at INTEGER NOT NULL
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
