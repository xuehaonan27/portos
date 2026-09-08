use super::*;
use portos_rm::ledger::{LedgerSnapshot, PoolRecord};
use std::collections::BTreeSet;

pub(super) fn builtin_policy(id: &str) -> CleanupPolicy {
    use CleanupKind::*;
    CleanupPolicy::Managed(match id {
        CLASS_CAP_COUNT | CLASS_CAP => return CleanupPolicy::AccountingOnly,
        CLASS_PROCESS => Process,
        CLASS_FILE_LOCK => FileLock,
        CLASS_PORT => Port,
        CLASS_PLUGIN => Plugin,
        CLASS_SUBSCRIPTION => Subscription,
        _ => Provider,
    })
}
fn builtins() -> Vec<ClassDecl> {
    [
        CLASS_CAP_COUNT,
        CLASS_PLUGIN,
        CLASS_SUBSCRIPTION,
        CLASS_PROCESS,
        CLASS_PORT,
        CLASS_FILE_LOCK,
        CLASS_CAP,
    ]
    .into_iter()
    .map(|id| ClassDecl {
        cleanup: builtin_policy(id),
        class_id: ClassId::new(id),
        algebra: if id == CLASS_CAP_COUNT {
            AlgebraTag::Counted
        } else {
            AlgebraTag::Exclusive
        },
        release_idempotent: true,
        lease_duration: None,
        revert_grade: RevertGrade::Inverse,
    })
    .collect()
}
pub(super) fn load(conn: &Connection) -> Result<Ledger, KernelError> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS resource_schema(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,revision INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS resource_classes(class_id TEXT PRIMARY KEY,algebra TEXT NOT NULL,release_idempotent INTEGER NOT NULL,lease_secs INTEGER,revert_grade TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS resource_pools(class_id TEXT NOT NULL,instance TEXT NOT NULL,capacity TEXT NOT NULL,PRIMARY KEY(class_id,instance));
        CREATE TABLE IF NOT EXISTS resource_instantiations(child TEXT PRIMARY KEY,parent TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS resource_accounts(account_id TEXT NOT NULL,effect_class TEXT NOT NULL,owner TEXT NOT NULL,class_id TEXT NOT NULL,instance TEXT NOT NULL,PRIMARY KEY(account_id,effect_class),UNIQUE(class_id,instance));
        CREATE TABLE IF NOT EXISTS resource_cleanup(holding_id INTEGER PRIMARY KEY,record TEXT NOT NULL);
    ")?;
    let version: Option<i64> = conn
        .query_row(
            "SELECT version FROM resource_schema WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if version.is_some_and(|v| v != 1 && v != 2) {
        return Err(corrupt(format!("unsupported resource schema {version:?}")));
    }
    let legacy = version.is_none();
    let migrate = version != Some(2);
    if migrate {
        for (table, column, ty) in [
            ("holdings", "lease_kind", "TEXT"),
            ("holdings", "state", "TEXT"),
            ("holdings", "target", "TEXT"),
            ("resource_classes", "cleanup", "TEXT"),
            ("resource_schema", "realm", "TEXT"),
        ] {
            let columns = conn
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            if !columns.iter().any(|c| c == column) {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))?;
            }
        }
        if legacy {
            persist(
                conn,
                &LedgerSnapshot {
                    classes: builtins(),
                    ..Default::default()
                },
            )?;
        }
    }
    let mut snapshot = codec::decode(conn, legacy, migrate)?;
    for builtin in builtins() {
        if snapshot
            .classes
            .iter()
            .find(|c| c.class_id == builtin.class_id)
            != Some(&builtin)
        {
            return Err(corrupt(format!(
                "builtin declaration differs: {}",
                builtin.class_id
            )));
        }
    }
    if legacy {
        import_legacy(conn, &mut snapshot)?;
    }
    if migrate {
        // Check the original M1 graph before introducing retirement obligations.
        // Historical accounting validity must not be repaired by the migration.
        let mut original = snapshot.clone();
        for class in &mut original.classes {
            class.cleanup = CleanupPolicy::AccountingOnly;
        }
        for h in &mut original.holdings {
            h.target = CleanupTarget::AccountingOnly;
        }
        LedgerBuilder::new(original)
            .finish()
            .map_err(|e| corrupt(format!("legacy graph rejected: {e:?}")))?;
        let realm = hex::encode(rand::random::<[u8; 16]>());
        migrate_retirements(conn, &mut snapshot, &realm)?;
        for d in &snapshot.classes {
            conn.execute(
                "UPDATE resource_classes SET cleanup=?2 WHERE class_id=?1",
                params![d.class_id.as_str(), cleanup_codec::policy_name(d.cleanup)],
            )?;
        }
        if legacy {
            conn.execute(
                "INSERT INTO resource_schema(singleton,version,realm) VALUES(1,2,?1)",
                params![realm],
            )?;
        } else {
            conn.execute(
                "UPDATE resource_schema SET version=2,realm=?1 WHERE singleton=1",
                params![realm],
            )?;
        }
    }
    let realm: String = conn.query_row(
        "SELECT realm FROM resource_schema WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    if realm.is_empty() {
        return Err(corrupt("empty resource realm"));
    }
    let ledger = LedgerBuilder::new(snapshot)
        .finish()
        .map_err(|e| corrupt(format!("ledger graph rejected: {e:?}")))?;
    validate_auxiliary(conn, &ledger)?;
    if migrate {
        persist(conn, &ledger.snapshot())?;
    }
    Ok(ledger)
}

/// Legacy tombstones are not receipts. Missing evidence is retained as a blocked
/// obligation; only an actual absence observation permits historical discharge.
fn migrate_retirements(
    conn: &Connection,
    s: &mut LedgerSnapshot,
    realm: &str,
) -> Result<(), KernelError> {
    let now = Timestamp::try_from(crate::db::now_unix()).map_err(map_err)?;
    if !s.cleanups.is_empty() {
        return Err(corrupt("unversioned cleanup records"));
    }
    for h in &mut s.holdings {
        let policy = s
            .classes
            .iter()
            .find(|d| d.class_id == h.class_id)
            .ok_or_else(|| corrupt("unknown legacy holding class"))?
            .cleanup;
        let CleanupPolicy::Managed(kind) = policy else {
            continue;
        };
        let detail: Option<String> = conn
            .query_row(
                "SELECT detail FROM substrate WHERE holding_id=?1",
                params![h.id.to_sql()],
                |r| r.get(0),
            )
            .optional()?;
        let detail: Option<Value> = detail
            .map(|v| {
                serde_json::from_str(&v)
                    .map_err(|e| corrupt(format!("legacy substrate {}: {e}", h.id)))
            })
            .transpose()?;
        let absent = match kind {
            CleanupKind::FileLock => match std::fs::symlink_metadata(h.instance.as_str()) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                _ => false,
            },
            CleanupKind::Process => {
                let (pid, start) = detail
                    .as_ref()
                    .and_then(|v| {
                        Some((
                            u32::try_from(v["pid"].as_u64()?).ok()?,
                            v["start"].as_u64()?,
                        ))
                    })
                    .ok_or_else(|| {
                        corrupt(format!("legacy process {} has no valid witness", h.id))
                    })?;
                if pid == 0
                    || pid > i32::MAX as u32
                    || start == 0
                    || h.generation.as_str() != format!("{pid}:{start}")
                {
                    return Err(corrupt("legacy process generation mismatch"));
                }
                substrate::legacy_process_absent(pid, start).unwrap_or(false)
            }
            _ => false,
        };
        h.target = if kind == CleanupKind::Port {
            substrate::capture(
                &ResourceKey::new(h.class_id.clone(), h.instance.clone()),
                &serde_json::json!({}),
            )?
        } else {
            CleanupTarget::Unresolved {
                kind,
                reason: if absent {
                    "legacy target was observed absent during migration".into()
                } else {
                    "legacy record lacks a complete incarnation witness; external reconciliation required".into()
                },
            }
        };
        let id = CleanupId::try_from(h.id.to_sql()).map_err(map_err)?;
        let state = if absent {
            CleanupState::Done(Completion::AlreadyAbsent)
        } else if kind == CleanupKind::Port {
            CleanupState::Pending
        } else {
            CleanupState::Blocked("legacy incarnation requires external reconciliation".into())
        };
        h.state = if absent {
            HoldingState::Retired(h.released_at().unwrap_or(now))
        } else {
            HoldingState::Retiring(id)
        };
        s.cleanups.push(CleanupRecord {
            id,
            key: CleanupKey::new(format!("cleanup:{realm}:{}", h.id.get())).map_err(map_err)?,
            holding: HoldingHandle::new(h.id, h.generation.clone()),
            state,
            attempts: 0,
            requested_at: now,
            updated_at: now,
        });
    }
    // A historical parent cannot be treated as discharged while an uncertain
    // child still depends on it. Preserve that dependency during migration.
    loop {
        let parents = s
            .holdings
            .iter()
            .filter(|h| h.state.occupies())
            .filter_map(|h| h.parent)
            .collect::<BTreeSet<_>>();
        let retiring = s
            .holdings
            .iter()
            .filter(|h| matches!(h.state, HoldingState::Retiring(_)))
            .map(|h| h.id)
            .collect::<BTreeSet<_>>();
        let mut changed = false;
        for h in &mut s.holdings {
            if (parents.contains(&h.id) && !h.state.occupies())
                || (h.state.is_active()
                    && h.lease == Lease::ParentBound
                    && h.parent.is_some_and(|p| retiring.contains(&p)))
            {
                let id = CleanupId::try_from(h.id.to_sql()).map_err(map_err)?;
                h.state = HoldingState::Retiring(id);
                if let Some(task) = s.cleanups.iter_mut().find(|t| t.id == id) {
                    task.state = CleanupState::Pending;
                } else {
                    s.cleanups.push(CleanupRecord {
                        id,
                        key: CleanupKey::new(format!("cleanup:{realm}:{}", h.id.get()))
                            .map_err(map_err)?,
                        holding: HoldingHandle::new(h.id, h.generation.clone()),
                        state: CleanupState::Pending,
                        attempts: 0,
                        requested_at: now,
                        updated_at: now,
                    });
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Ok(())
}

pub(super) fn validate_auxiliary(conn: &Connection, ledger: &Ledger) -> Result<(), KernelError> {
    for row in conn
        .prepare("SELECT account_id,effect_class,class_id,instance FROM resource_accounts")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
    {
        let (account, effect, c, i) = row?;
        ledger
            .pool::<Count>(&ResourceKey::new(c.into(), i.into()))
            .map_err(|e| corrupt(format!("account {account}/{effect}: {e:?}")))?;
    }
    for row in conn
        .prepare("SELECT holding_id,grade,state,updated_at FROM journal")?
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
    {
        let (id, grade, state, time) = row?;
        let id = HoldingId::try_from(id).map_err(map_err)?;
        let h = ledger
            .holding(id)
            .ok_or_else(|| corrupt("legacy journal has no holding"))?;
        if ledger.grade_of(&h.class_id) != Some(codec::grade(&grade)?)
            || !["pending", "in_flight", "done", "failed"].contains(&state.as_str())
        {
            return Err(corrupt("invalid legacy journal"));
        }
        Timestamp::try_from(time).map_err(map_err)?;
    }
    for h in ledger.holdings() {
        if let CleanupTarget::Process(w) = &h.target {
            if h.generation.as_str() != format!("{}:{}", w.pid(), w.start_ticks()) {
                return Err(corrupt(
                    "process holding incarnation differs from cleanup target",
                ));
            }
        }
    }
    Ok(())
}

fn import_legacy(conn: &Connection, snapshot: &mut LedgerSnapshot) -> Result<(), KernelError> {
    let rows: Vec<(String, String, i64)> = conn
        .prepare("SELECT cap_id,json,revoked FROM caps")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut pools = std::collections::BTreeMap::new();
    for (id, json, revoked) in rows {
        let cap: Capability = serde_json::from_str(&json)
            .map_err(|e| corrupt(format!("legacy account {id}: {e}")))?;
        if cap.cap_id != id || ![0, 1].contains(&revoked) || cap.revoked != (revoked == 1) {
            return Err(corrupt(format!("inconsistent legacy account {id}")));
        }
        for (effect, capacity) in &cap.constraints.counts {
            let instance = format!("{id}/{effect}");
            let key = ResourceKey::new(ClassId::new(CLASS_CAP_COUNT), InstanceId::new(&instance));
            if pools
                .insert(
                    key,
                    Frag::Count(Count::Value(if cap.revoked { 0 } else { *capacity })),
                )
                .is_some()
            {
                return Err(corrupt(format!("ambiguous legacy pool {instance}")));
            }
            conn.execute("INSERT INTO resource_accounts(account_id,effect_class,owner,class_id,instance) VALUES(?1,?2,?3,?4,?5)", params![id,effect,cap.subject,CLASS_CAP_COUNT,instance])?;
        }
    }
    for h in &snapshot.holdings {
        let class = snapshot
            .classes
            .iter()
            .find(|c| c.class_id == h.class_id)
            .ok_or_else(|| corrupt(format!("holding {}: unknown class {}", h.id, h.class_id)))?;
        if class.algebra == AlgebraTag::Exclusive {
            pools
                .entry(ResourceKey::new(h.class_id.clone(), h.instance.clone()))
                .or_insert(Frag::Ex(Ex::Token));
        }
    }
    if !snapshot.pools.is_empty() {
        return Err(corrupt(
            "unversioned pool storage; cannot safely infer migration state",
        ));
    }
    snapshot.pools = pools
        .into_iter()
        .map(|(key, capacity)| PoolRecord { key, capacity })
        .collect();
    Ok(())
}
