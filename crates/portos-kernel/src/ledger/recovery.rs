use super::*;
use portos_rm::ledger::{LedgerSnapshot, PoolRecord};

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

/// Called inside an IMMEDIATE transaction. Failed migration leaves every old row
/// and the old schema intact, including enough context to diagnose the bad row.
pub(super) fn load(conn: &Connection) -> Result<Ledger, KernelError> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS resource_schema(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,revision INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS resource_classes(class_id TEXT PRIMARY KEY,algebra TEXT NOT NULL,release_idempotent INTEGER NOT NULL,lease_secs INTEGER,revert_grade TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS resource_pools(class_id TEXT NOT NULL,instance TEXT NOT NULL,capacity TEXT NOT NULL,PRIMARY KEY(class_id,instance));
        CREATE TABLE IF NOT EXISTS resource_instantiations(child TEXT PRIMARY KEY,parent TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS resource_accounts(account_id TEXT NOT NULL,effect_class TEXT NOT NULL,owner TEXT NOT NULL,class_id TEXT NOT NULL,instance TEXT NOT NULL,PRIMARY KEY(account_id,effect_class),UNIQUE(class_id,instance));
    ")?;
    let version: Option<i64> = conn
        .query_row(
            "SELECT version FROM resource_schema WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if version.is_some_and(|v| v != 1) {
        return Err(corrupt(format!("unsupported resource schema {version:?}")));
    }
    let legacy = version.is_none();
    if legacy {
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(holdings)")?
            .query_map([], |r| r.get(1))?
            .collect::<Result<_, _>>()?;
        if !columns.iter().any(|c| c == "lease_kind") {
            conn.execute_batch("ALTER TABLE holdings ADD COLUMN lease_kind TEXT")?;
        }
        let initial = LedgerSnapshot {
            classes: builtins(),
            ..Default::default()
        };
        persist(conn, &initial)?;
    }
    let mut snapshot = codec::decode(conn, legacy)?;
    // Built-ins are immutable contracts, checked against persisted descriptions.
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
    let ledger = LedgerBuilder::new(snapshot)
        .finish()
        .map_err(|e| corrupt(format!("ledger graph rejected: {e:?}")))?;
    validate_auxiliary(conn, &ledger)?;
    if legacy {
        persist(conn, &ledger.snapshot())?;
        conn.execute(
            "INSERT INTO resource_schema(singleton,version) VALUES(1,1)",
            [],
        )?;
    }
    Ok(ledger)
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

pub(super) fn validate_auxiliary(conn: &Connection, ledger: &Ledger) -> Result<(), KernelError> {
    let rows: Vec<(String, String, String, String, String)> = conn
        .prepare("SELECT account_id,effect_class,owner,class_id,instance FROM resource_accounts")?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    for (account, effect, _owner, c, i) in rows {
        let key = ResourceKey::new(c.into(), i.into());
        ledger
            .pool::<Count>(&key)
            .map_err(|e| corrupt(format!("account {account}/{effect}: {e:?}")))?;
    }
    let rows: Vec<(i64, String, String, i64)> = conn
        .prepare("SELECT holding_id,grade,state,updated_at FROM journal")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    for (id, grade, state, time) in rows {
        let id = HoldingId::try_from(id).map_err(|e| corrupt(format!("journal id: {e:?}")))?;
        let h = ledger
            .holding(id)
            .ok_or_else(|| corrupt(format!("journal has no holding {id}")))?;
        if ledger.grade_of(&h.class_id) != Some(codec::grade(&grade)?) {
            return Err(corrupt(format!("journal {id}: grade differs")));
        }
        if !["pending", "in_flight", "done", "failed"].contains(&state.as_str()) {
            return Err(corrupt(format!("journal {id}: unknown state {state}")));
        }
        Timestamp::try_from(time).map_err(|e| corrupt(format!("journal {id}: time {e:?}")))?;
    }
    let rows: Vec<(i64, String, String)> = conn
        .prepare("SELECT holding_id,kind,detail FROM substrate")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut witnessed = std::collections::BTreeSet::new();
    for (id, kind, detail) in rows {
        let id = HoldingId::try_from(id).map_err(|e| corrupt(format!("substrate id: {e:?}")))?;
        let h = ledger
            .holding(id)
            .ok_or_else(|| corrupt(format!("substrate has no holding {id}")))?;
        if h.class_id.as_str().strip_prefix("kernel/") != Some(kind.as_str()) {
            return Err(corrupt(format!("substrate {id}: kind differs")));
        }
        let value: Value =
            serde_json::from_str(&detail).map_err(|e| corrupt(format!("substrate {id}: {e}")))?;
        legacy_cleanup::validate_substrate(h.class_id.as_str(), &value)?;
        if h.class_id.as_str() == CLASS_PROCESS {
            let expected = format!(
                "{}:{}",
                value["pid"].as_u64().expect("validated pid"),
                value["start"].as_u64().expect("validated start")
            );
            if h.generation.as_str() != expected {
                return Err(corrupt(format!(
                    "holding {id}: process generation differs from substrate"
                )));
            }
        }
        witnessed.insert(id);
    }
    for h in ledger
        .live()
        .filter(|h| HOLDABLE_CLASSES.contains(&h.class_id.as_str()))
    {
        if !witnessed.contains(&h.id) {
            return Err(corrupt(format!(
                "live holding {} has no substrate witness",
                h.id
            )));
        }
    }
    Ok(())
}
