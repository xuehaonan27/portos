//! Private M1 adapter for the existing world-action order. M2 will replace it
//! with durable cleanup obligations; these SQL transactions cannot undo OS actions.
use super::*;
use portos_rm::teardown::{JState, Journal, JournalEntry, teardown_subtree_with, teardown_with};
impl LedgerStore {
    pub(super) fn reconcile_stale_process_rows(&self) -> Result<usize, KernelError> {
        let now = Timestamp::try_from(crate::db::now_unix()).map_err(map_err)?;
        self.transaction(|tx| {
            let l = &mut tx.ledger;
            let conn = tx.sql;
            let mut stale: Vec<(HoldingId, Generation)> = Vec::new();
            for class in [CLASS_SUBSCRIPTION, CLASS_PLUGIN] {
                stale.extend(
                    l.live()
                        .filter(|h| h.class_id.as_str() == class)
                        .map(|h| (h.id, h.generation.clone())),
                );
            }
            // Children before parents: a parent row refuses release while its
            // child row is still live (TeardownOrder), and with parent/child
            // plugins both are in this list — so iterate to a fixpoint.
            let mut n = 0;
            while !stale.is_empty() {
                let mut progressed = false;
                let mut i = 0;
                while i < stale.len() {
                    let (id, generation) = stale[i].clone();
                    if l.release(&HoldingHandle::new(id, generation), now).is_ok() {
                        n += 1;
                        if l.holding(id).is_some() {
                            // Preserve the existing reconcile policy. M2 will
                            // require explicit physical completion evidence.
                            resolve_journal(conn, id, now)?;
                        }
                        stale.remove(i);
                        progressed = true;
                    } else {
                        i += 1;
                    }
                }
                if !progressed {
                    break;
                }
            }
            Ok(n)
        })
    }

    pub fn sweep_with_world<W: World>(
        &self,
        world: &mut W,
        now: Timestamp,
    ) -> Result<SweepReport, KernelError> {
        self.transaction(|tx| {
            let l = &mut tx.ledger;
            let released = l.sweep(now);
            let mut report = SweepReport::default();
            for id in released {
                let Some(h) = l.holding(id) else { continue };
                let item = LiveItem {
                    id: h.id,
                    parent: h.parent,
                    class_id: h.class_id.clone(),
                    instance: h.instance.clone(),
                    generation: h.generation.clone(),
                    grade: l.grade_of(&h.class_id).unwrap_or(RevertGrade::Inverse),
                };
                let ok = match item.grade {
                    RevertGrade::Inverse => world.release(&item).is_ok(),
                    RevertGrade::Compensable => {
                        world.compensate(&item, &format!("sw:{id}")).is_ok()
                    }
                    RevertGrade::External => true,
                };
                if ok {
                    report.released.push((
                        h.id.get(),
                        h.class_id.to_string(),
                        h.instance.to_string(),
                    ));
                } else {
                    report.failed.push(h.id.get());
                }
            }
            Ok(report)
        })
    }

    /// Reconcile the substrate classes against the world on open: a previous
    /// kernel incarnation is dead, so a live `kernel/process` row whose
    /// incarnation is still running is killed (crash-only), freed ports and
    /// owner-dead lock files are released. All tombstones commit in one
    /// transaction; counts are returned for the audit chain.
    pub(super) fn reconcile_substrate(&self) -> Result<SubstrateReconcile, KernelError> {
        let now = Timestamp::try_from(crate::db::now_unix()).map_err(map_err)?;
        self.transaction(|tx| {
            let l = &mut tx.ledger;
            let conn = tx.sql;
            let mut report = SubstrateReconcile::default();
            let rows: Vec<Holding> = l
                .live()
                .filter(|h| HOLDABLE_CLASSES.contains(&h.class_id.as_str()))
                .cloned()
                .collect();
            let mut details: std::collections::BTreeMap<HoldingId, Value> =
                std::collections::BTreeMap::new();
            for h in &rows {
                let detail: String = conn.query_row(
                    "SELECT detail FROM substrate WHERE holding_id = ?1",
                    params![h.id.to_sql()],
                    |r| r.get(0),
                )?;
                let detail = serde_json::from_str(&detail)
                    .map_err(|e| corrupt(format!("substrate {}: {e}", h.id)))?;
                validate_substrate(h.class_id.as_str(), &detail)?;
                details.insert(h.id, detail);
            }
            // Processes first: a killed orphan may own a lock file below.
            for class in [CLASS_PROCESS, CLASS_PORT, CLASS_FILE_LOCK] {
                for h in rows.iter().filter(|h| h.class_id.as_str() == class) {
                    let detail = details.get(&h.id).expect("all substrate witnesses decoded");
                    match class {
                        CLASS_PROCESS => {
                            let pid = u32::try_from(detail["pid"].as_u64().expect("validated pid"))
                                .expect("validated pid");
                            let start = detail["start"].as_u64().expect("validated start");
                            if proc_alive(pid, start) {
                                kill_pid(pid);
                                report.process_killed += 1;
                            } else {
                                report.process_tombstoned += 1;
                            }
                        }
                        CLASS_PORT => {
                            report.ports_tombstoned += 1;
                            if !port_free(h.instance.as_str()) {
                                report.ports_still_bound += 1;
                            }
                        }
                        CLASS_FILE_LOCK => {
                            let owner = detail["owner_pid"].as_u64().map(|p| p as u32);
                            let owner_start = detail["owner_start"].as_u64().unwrap_or(0);
                            if owner.is_some_and(|p| proc_alive(p, owner_start)) {
                                report.locks_kept += 1; // orphan still owns it; leave the file
                            } else {
                                let _ = std::fs::remove_file(h.instance.as_str());
                                report.locks_removed += 1;
                            }
                        }
                        _ => unreachable!(),
                    }
                    let _ = l.release(&h.handle(), now);
                    if l.holding(h.id).is_some() {
                        resolve_journal(conn, h.id, now)?;
                    }
                }
            }
            Ok(report)
        })
    }
}
pub(crate) fn proc_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) may contain spaces and parens; fields resume after the
    // last ')'. starttime is field 22 → index 19 of the remainder.
    let after = stat.rsplit_once(')')?.1.trim_start();
    after.split_whitespace().nth(19)?.parse().ok()
}

/// Is this exact process incarnation alive? `start == 0` checks existence only.
pub(crate) fn proc_alive(pid: u32, start: u64) -> bool {
    if pid == 0 {
        return false;
    }
    match proc_start_time(pid) {
        Some(s) => start == 0 || s == start,
        None => false,
    }
}

/// SIGKILL a pid (best-effort; existence/incarnation checks are the caller's).
pub(crate) fn kill_pid(pid: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
}

/// Bind probe for a `<proto>:<port>` instance: free if we could bind it.
fn port_free(instance: &str) -> bool {
    let (proto, port) = instance.rsplit_once(':').unwrap_or(("tcp", "0"));
    let port: u16 = port.parse().unwrap_or(0);
    match proto {
        "udp" => std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok(),
        _ => std::net::TcpListener::bind(("0.0.0.0", port)).is_ok(),
    }
}

/// Write one row through. Takes the connection, not the store's `Arc`: every
/// caller runs inside [`LedgerStore::transaction`], which already holds it.
fn load_pending_journal(conn: &Connection, holdings: &[HoldingId]) -> Result<Journal, KernelError> {
    let mut stmt =
        conn.prepare("SELECT holding_id, grade, idem_key FROM journal WHERE state != 'done'")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut entries = Vec::new();
    for (hid, grade, idem_key) in rows {
        let hid = HoldingId::try_from(hid).map_err(map_err)?;
        if !holdings.contains(&hid) {
            continue;
        }
        entries.push(JournalEntry {
            holding_id: hid,
            grade: grade_from_str(&grade)?,
            idem_key,
            state: JState::Pending,
        });
    }
    Ok(Journal::from_entries(entries))
}

fn upsert_journal(conn: &Connection, e: &JournalEntry, now: Timestamp) -> Result<(), KernelError> {
    conn.execute(
        "INSERT OR REPLACE INTO journal (holding_id, grade, idem_key, state, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            e.holding_id.to_sql(),
            grade_str(e.grade),
            e.idem_key,
            state_str(e.state),
            now.to_sql(),
        ],
    )?;
    Ok(())
}

/// Legacy reconciliation resolves this journal entry after its world attempt.
/// M2 separates a tombstone from confirmed physical completion.
fn resolve_journal(
    conn: &Connection,
    holding_id: HoldingId,
    now: Timestamp,
) -> Result<(), KernelError> {
    conn.execute(
        "UPDATE journal SET state = 'done', updated_at = ?2 \
         WHERE holding_id = ?1 AND state != 'done'",
        params![holding_id.to_sql(), now.to_sql()],
    )?;
    Ok(())
}

fn grade_str(g: RevertGrade) -> &'static str {
    match g {
        RevertGrade::Inverse => "inverse",
        RevertGrade::Compensable => "compensable",
        RevertGrade::External => "external",
    }
}

fn grade_from_str(s: &str) -> Result<RevertGrade, KernelError> {
    match s {
        "inverse" => Ok(RevertGrade::Inverse),
        "compensable" => Ok(RevertGrade::Compensable),
        "external" => Ok(RevertGrade::External),
        other => Err(KernelError::Corrupt(format!("journal grade: {other}"))),
    }
}

fn state_str(s: JState) -> &'static str {
    match s {
        JState::Pending => "pending",
        JState::InFlight => "in_flight",
        JState::Done => "done",
        JState::Failed => "failed",
    }
}

pub(super) fn validate_substrate(class: &str, value: &Value) -> Result<(), KernelError> {
    if !value.is_object() {
        return Err(corrupt("substrate must be an object"));
    }
    let pid = |field: &str| -> Result<(), KernelError> {
        let n = value[field]
            .as_u64()
            .ok_or_else(|| corrupt(format!("substrate.{field} required")))?;
        if n == 0 || i32::try_from(n).is_err() {
            return Err(corrupt(format!("substrate.{field} out of range")));
        }
        Ok(())
    };
    match class {
        CLASS_PROCESS => {
            pid("pid")?;
            value["start"]
                .as_u64()
                .ok_or_else(|| corrupt("substrate.start required"))?;
        }
        CLASS_FILE_LOCK => {
            if value.get("owner_pid").is_some() {
                pid("owner_pid")?;
            }
            if value.get("owner_start").is_some() && value["owner_start"].as_u64().is_none() {
                return Err(corrupt("substrate.owner_start must be unsigned"));
            }
        }
        CLASS_PORT => {}
        _ => return Err(corrupt(format!("unknown substrate class {class}"))),
    }
    Ok(())
}

impl LedgerStore {
    pub fn teardown<W: World>(
        &self,
        subject: &SubjectId,
        world: &mut W,
        now: Timestamp,
    ) -> Result<(RunOutcome, usize), KernelError> {
        self.transaction(|tx| {
            let before: Vec<_> = tx
                .ledger
                .live_closure(subject)
                .iter()
                .map(|it| it.id)
                .collect();
            let mut journal = load_pending_journal(tx.sql, &before)?;
            let outcome = teardown_with(
                &mut tx.ledger,
                &mut journal,
                world,
                subject.as_str(),
                0,
                None,
                now,
            );
            for e in journal.entries() {
                upsert_journal(tx.sql, e, now)?;
            }
            let released = before
                .iter()
                .filter(|id| {
                    tx.ledger
                        .holding(**id)
                        .is_some_and(|h| h.released_at.is_some())
                })
                .count();
            Ok((outcome, released))
        })
    }
    /// Named account coordinator; business code cannot append an arbitrary SQL
    /// callback to teardown. Derivation-tree traversal stays in CapStore.
    pub(crate) fn retire_capability<W: World>(
        &self,
        cap: &Capability,
        world: &mut W,
        now: Timestamp,
    ) -> Result<(), KernelError> {
        self.transaction(|tx| {
            let root = tx
                .ledger
                .live()
                .find(|h| h.class_id.as_str() == CLASS_CAP && h.instance.as_str() == cap.cap_id)
                .map(|h| h.id);
            if let Some(root) = root {
                let before: Vec<_> = tx
                    .ledger
                    .live_subtree(root)
                    .iter()
                    .map(|it| it.id)
                    .collect();
                let mut journal = load_pending_journal(tx.sql, &before)?;
                let outcome = teardown_subtree_with(
                    &mut tx.ledger,
                    &mut journal,
                    world,
                    root,
                    &cap.cap_id,
                    0,
                    None,
                    now,
                );
                for e in journal.entries() {
                    upsert_journal(tx.sql, e, now)?;
                }
                // Preserve the legacy cleanup result in its journal. M2 will
                // make account retirement and physical confirmation distinct.
                let _ = outcome;
            }
            let keys: Vec<(String, String)> = tx
                .sql
                .prepare("SELECT class_id,instance FROM resource_accounts WHERE account_id=?1")?
                .query_map(params![cap.cap_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            for (c, i) in keys {
                let pool = tx.pool::<Count>(&ResourceKey::new(c.into(), i.into()))?;
                tx.settle_and_zero_pool(&pool, now)?;
            }
            let mut stored = cap.clone();
            stored.revoked = true;
            store_capability(tx.sql, &stored)
        })
    }
}
