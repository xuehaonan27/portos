//! SQLite state. Artifact metadata index, capability table, named refs.
//! The CAS payload bytes live in the object directory, never in SQLite.

use rusqlite::Connection;
use std::path::Path;

pub(crate) fn open(root: &Path) -> Result<Connection, rusqlite::Error> {
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
        -- Holding ledger: one row per fragment. The resource schema adapter
        -- adds lifecycle state and target evidence. released_at is populated
        -- only at retirement; a pending cleanup still occupies the resource.
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
        -- Historical F2 journal, retained for migration and diagnosis.
        -- M2 writes resource_cleanup through the resource schema adapter;
        -- cleanup claims commit before world actions and confirmation follows.
        CREATE TABLE IF NOT EXISTS journal (
            holding_id INTEGER PRIMARY KEY,
            grade      TEXT NOT NULL,   -- "inverse" | "compensable" | "external"
            idem_key   TEXT NOT NULL,   -- [KEY] dedupe key, stable across crashes
            state      TEXT NOT NULL,   -- "pending" | "in_flight" | "done" | "failed"
            updated_at INTEGER NOT NULL
        );
        -- Historical WP-02 witnesses, retained as migration input and diagnostics.
        -- New registrations persist checked targets on the holding itself.
        CREATE TABLE IF NOT EXISTS substrate (
            holding_id INTEGER PRIMARY KEY,
            kind       TEXT NOT NULL,   -- "process" | "port" | "file-lock"
            detail     TEXT NOT NULL    -- JSON per kind
        );
        -- F3 plan runs (WP-06): one row per run, terminal outcome as JSON.
        CREATE TABLE IF NOT EXISTS plan_runs (
            run_id      TEXT PRIMARY KEY,
            plan_hash   TEXT NOT NULL,
            subject     TEXT NOT NULL,   -- fiber subject plan:<h>#<run>
            nonce       TEXT NOT NULL,   -- active consent nonce
            state       TEXT NOT NULL,   -- admitted | running | awaiting_approval | paused | done
            started_at  INTEGER NOT NULL,
            finished_at INTEGER,
            outcome     TEXT             -- JSON terminal outcome
        );
        -- Withheld (staged) effects, in original order. Self-contained: an
        -- approval in a later process replays from these rows alone.
        CREATE TABLE IF NOT EXISTS suppression_buffer (
            run_id  TEXT NOT NULL,
            seq     INTEGER NOT NULL,
            verb    TEXT NOT NULL,
            target  TEXT NOT NULL,
            args    TEXT NOT NULL,      -- JSON (evaluated values)
            cost    INTEGER NOT NULL,
            state   TEXT NOT NULL DEFAULT 'held',  -- held | inserted | aborted
            PRIMARY KEY (run_id, seq)
        );
        -- Every emitted effect of a run, in order (the w-effect shadow).
        CREATE TABLE IF NOT EXISTS emission_log (
            run_id  TEXT NOT NULL,
            seq     INTEGER NOT NULL,
            verb    TEXT NOT NULL,
            target  TEXT NOT NULL,
            nonce   TEXT NOT NULL,      -- the consent that paid for it
            at      INTEGER NOT NULL,
            PRIMARY KEY (run_id, seq)
        );
        "#,
    )?;
    // consents.source: "signed" (user quadruple) | "derived" (WP-08
    // attachments). Idempotent migration for existing roots.
    match conn.execute_batch("ALTER TABLE consents ADD COLUMN source TEXT NOT NULL DEFAULT 'signed'") {
        Ok(()) => {}
        Err(e) if e.to_string().contains("duplicate column") => {}
        Err(e) => return Err(e),
    }
    Ok(conn)
}

/// TODO: move to utility crate later.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
