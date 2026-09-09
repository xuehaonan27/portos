//! SQLite state. Artifact metadata index, capability table, named refs.
//! The CAS payload bytes live in the object directory, never in SQLite.

use rusqlite::Connection;
use std::path::Path;

pub(crate) fn open(root: &Path) -> Result<Connection, crate::KernelError> {
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
    match conn
        .execute_batch("ALTER TABLE consents ADD COLUMN source TEXT NOT NULL DEFAULT 'signed'")
    {
        Ok(()) => {}
        Err(e) if e.to_string().contains("duplicate column") => {}
        Err(e) => return Err(e.into()),
    }
    migrate_plan_segments(&conn)?;
    Ok(conn)
}

fn migrate_plan_segments(conn: &Connection) -> Result<(), crate::KernelError> {
    let tx = conn.unchecked_transaction()?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='plan_segments')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        tx.execute_batch("CREATE TABLE plan_segments (run_id TEXT PRIMARY KEY REFERENCES plan_runs(run_id), subject TEXT NOT NULL UNIQUE)")?;
        let rows = {
            let mut q = tx.prepare("SELECT run_id, plan_hash, subject FROM plan_runs")?;
            q.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        for (run, plan, fiber) in rows {
            // The legacy association is derived only from a known plan record.
            if fiber != format!("plan:{plan}#{run}") {
                return Err(crate::KernelError::Corrupt(format!(
                    "cannot migrate segment for plan run {run}: unexpected subject"
                )));
            }
            tx.execute(
                "INSERT INTO plan_segments (run_id, subject) VALUES (?1, ?2)",
                rusqlite::params![run, format!("{fiber}:seg")],
            )?;
        }
    }
    let inconsistent: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM plan_runs r LEFT JOIN plan_segments s ON s.run_id=r.run_id WHERE s.run_id IS NULL OR s.subject='' OR s.subject=r.subject) OR EXISTS(SELECT 1 FROM plan_segments s LEFT JOIN plan_runs r ON r.run_id=s.run_id WHERE r.run_id IS NULL)", [], |r| r.get(0),
    )?;
    if inconsistent {
        return Err(crate::KernelError::Corrupt(
            "invalid plan/segment association".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}

/// TODO: move to utility crate later.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[cfg(test)]
mod m3_tests {
    use super::*;

    fn legacy(fiber: &str) -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE plan_runs (run_id TEXT PRIMARY KEY, plan_hash TEXT NOT NULL, subject TEXT NOT NULL)").unwrap();
        c.execute(
            "INSERT INTO plan_runs VALUES ('run_1', 'hash', ?1)",
            [fiber],
        )
        .unwrap();
        c
    }

    #[test]
    fn plan_segment_migration_is_atomic_and_only_interprets_known_legacy_records() {
        let c = legacy("plan:hash#run_1");
        migrate_plan_segments(&c).unwrap();
        migrate_plan_segments(&c).unwrap();
        let subject: String = c
            .query_row("SELECT subject FROM plan_segments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(subject, "plan:hash#run_1:seg");
        let malformed = legacy("unrelated:seg");
        assert!(matches!(
            migrate_plan_segments(&malformed),
            Err(crate::KernelError::Corrupt(_))
        ));
        let exists: bool = malformed
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='plan_segments')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists, "failed migration rolls back the new table too");
    }

    #[test]
    fn missing_current_associations_are_not_reconstructed_from_labels() {
        let c = legacy("plan:hash#run_1");
        migrate_plan_segments(&c).unwrap();
        c.execute("DELETE FROM plan_segments", []).unwrap();
        assert!(matches!(
            migrate_plan_segments(&c),
            Err(crate::KernelError::Corrupt(_))
        ));
    }
}
