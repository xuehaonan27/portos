//! The holding ledger (spec F1/F2) wired into the kernel.
//!
//! The algorithm is `portos_rm::ledger::Ledger` — the law-tested code moves in
//! unchanged (roadmap H.2-2: 法则跟着语义走). This module adds exactly what the
//! drill declared as its deviations from the real thing:
//!
//!   - **Durability**: every row is written through to `kernel.sqlite`
//!     (`holdings` table, one row per fragment, tombstones kept) and reloaded
//!     on open, so the ledger outlives any plugin crash (endstate invariant 2)
//!     and a kernel restart keeps counting budgets where they were.
//!   - **Atomic compound writes**: every multi-row operation (teardown of a
//!     whole ownership closure, a batch of spend rows) commits in one SQLite
//!     transaction; on any error the in-memory ledger is restored from a
//!     snapshot, so memory and disk never diverge (attachments §4.3 rule 5).
//!   - **F2 journal persistence** (`journal` table): teardown journal entries
//!     write through in the same transaction as the tombstones. A world action
//!     that failed survives the process as a non-'done' row — reported by
//!     `open` as `journal_pending` and replayed by the next teardown of that
//!     subject ([SAGA] write-ahead, [E-IDEM] blind replay).
//!   - **Built-in resource classes** (档 0, handler = the kernel itself):
//!       `kernel/cap-count`   capability counting budgets — one capacity row
//!                            per (cap, verb) pool, one spend row per
//!                            exercise; the balance is recomputed from the rows
//!                            as a lifecycle accounting policy. Grants check the
//!                            complete composition against the pool's capacity.
//!                            Replaces the in-place decrement (roadmap H.2-1).
//!       `kernel/plugin`      one exclusive holding per spawned plugin
//!                            instance, generation = the spawn token; released
//!                            only by crash-only teardown.
//!       `kernel/subscription` one exclusive holding per event subscription,
//!                            child of the subscriber's plugin holding: a
//!                            standing inbound channel is a holding, so
//!                            unloading a plugin unsubscribes it, children
//!                            first (endstate §8.5).
//!       `kernel/process`     a plugin-registered child process (WP-02).
//!                            Instance `<plugin>/<pid>`; generation witnesses
//!                            the exact incarnation (`<pid>:<start time>`);
//!                            world release = SIGKILL that incarnation.
//!       `kernel/port`        a bound port (instance `<proto>:<port>`); world
//!                            release is a no-op, reconcile bind-probes.
//!       `kernel/file-lock`   a lock file (instance = path); world release =
//!                            remove the file, reconcile removes it when the
//!                            recorded owner pid is dead.
//!       `kernel/cap`         a granted capability as a holding (WP-03):
//!                            instance = generation = the cap id, lease =
//!                            `constraints.expires_at` (absolute), parent =
//!                            None. The holding is the single source of truth
//!                            for liveness: revocation tears down its ownership
//!                            subtree (what exists because of the grant goes
//!                            first), expiry rides the sweeper, and the issuer
//!                            gate (`CapStore::exercise`) refuses a cap whose
//!                            holding is gone. World release is a no-op — the
//!                            physical side of a grant lives in its children.
//!   - **Time (WP-02)**: leases on holdings (`set_lease`), a sweeper
//!     (`sweep_with_world`, driven by `Host::start_sweeper`) running
//!     `Ledger::sweep` plus the world side of expiry, and substrate
//!     reconciliation on open (`reconcile_substrate`): a previous kernel
//!     incarnation's registered processes are killed, freed ports and stale
//!     lock files tombstoned.
//!     (`kernel/artifact` and its TTL are deferred with gate G5 — artifacts
//!     never expire for now.)
//!
//! Lock discipline: the ledger mutex is the outermost lock. Teardown runs the
//! F2 executor while holding it and calls back into the host's `World`, which
//! takes the host's own locks; nothing may take a host lock and then the
//! ledger. `host.rs` keeps that order.

use crate::KernelError;
use portos_rm::ledger::{
    AlgebraTag, ClassDecl, Frag, Holding, Ledger, LedgerError, LiveItem, RevertGrade,
};
use portos_rm::ra::{Count, Ex, Frac, GSet, Ranges};
use portos_rm::teardown::{JState, Journal, JournalEntry, RunOutcome, World, teardown_subtree_with, teardown_with};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub const CLASS_CAP_COUNT: &str = "kernel/cap-count";
pub const CLASS_PLUGIN: &str = "kernel/plugin";
pub const CLASS_SUBSCRIPTION: &str = "kernel/subscription";
pub const CLASS_PROCESS: &str = "kernel/process";
pub const CLASS_PORT: &str = "kernel/port";
pub const CLASS_FILE_LOCK: &str = "kernel/file-lock";
pub const CLASS_CAP: &str = "kernel/cap";

/// Classes a plugin may `hold` (WP-02; open registration is Phase G).
pub const HOLDABLE_CLASSES: [&str; 3] = [CLASS_PROCESS, CLASS_PORT, CLASS_FILE_LOCK];

/// Generation string of spend rows (they are never released individually).
const SPEND_GENERATION: &str = "spend";

/// What `LedgerStore::open` found left behind by previous incarnations.
pub struct OpenReport {
    /// Live `kernel/plugin`/`kernel/subscription` rows of a previous kernel
    /// process, tombstoned children-first during reconcile.
    pub stale_rows: usize,
    /// Journal entries still owed a world action (a failed teardown step
    /// persisted). Replayed by the next teardown of their subject.
    pub journal_pending: usize,
    /// Substrate reconciliation of the WP-02 built-in classes.
    pub substrate: SubstrateReconcile,
}

/// Per-class outcome of `reconcile_substrate` (audited on open).
#[derive(Default)]
pub struct SubstrateReconcile {
    /// Live `kernel/process` rows whose incarnation was still running: killed
    /// (crash-only: a previous incarnation never survives a kernel restart).
    pub process_killed: usize,
    /// `kernel/process` rows whose pid was already gone: tombstoned.
    pub process_tombstoned: usize,
    /// `kernel/port` rows tombstoned.
    pub ports_tombstoned: usize,
    /// …of which the bind probe still finds the port occupied (an orphan the
    /// ledger never tracked holds it — reported, not hidden).
    pub ports_still_bound: usize,
    /// `kernel/file-lock` rows whose lock file was removed (owner dead).
    pub locks_removed: usize,
    /// `kernel/file-lock` rows whose owner pid is still alive (orphan): the
    /// file stays, the row is tombstoned, the count is audited.
    pub locks_kept: usize,
}

impl SubstrateReconcile {
    pub fn is_empty(&self) -> bool {
        self.process_killed == 0
            && self.process_tombstoned == 0
            && self.ports_tombstoned == 0
            && self.locks_removed == 0
            && self.locks_kept == 0
    }
}

/// The world side of one sweep tick.
#[derive(Default)]
pub struct SweepReport {
    /// (id, class, instance) per released row, world action executed.
    pub released: Vec<(u64, String, String)>,
    /// Rows whose world action failed (still tombstoned — a lease is a ledger
    /// fact); reported so the caller can audit them (never silent).
    pub failed: Vec<u64>,
}

pub struct LedgerStore {
    inner: Mutex<Ledger>,
    db: Arc<Mutex<Connection>>,
}

impl LedgerStore {
    /// Open the ledger: register the built-in classes, reload every row, and
    /// reconcile what a previous kernel process left behind.
    pub fn open(db: Arc<Mutex<Connection>>) -> Result<(LedgerStore, OpenReport), KernelError> {
        let mut l = Ledger::new();
        for (id, algebra) in [
            (CLASS_CAP_COUNT, AlgebraTag::Counted),
            (CLASS_PLUGIN, AlgebraTag::Exclusive),
            (CLASS_SUBSCRIPTION, AlgebraTag::Exclusive),
            (CLASS_PROCESS, AlgebraTag::Exclusive),
            (CLASS_PORT, AlgebraTag::Exclusive),
            (CLASS_FILE_LOCK, AlgebraTag::Exclusive),
            (CLASS_CAP, AlgebraTag::Exclusive),
        ] {
            l.register_class(ClassDecl {
                class_id: id.to_string(),
                algebra,
                release_idempotent: true,
                lease_secs: None,
                revert_grade: RevertGrade::Inverse,
            });
        }
        let rows: Vec<Holding> = {
            let conn = db.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT id, subject, class_id, instance, frag, generation, parent, \
                 lease_expires_at, acquired_at, released_at FROM holdings ORDER BY id",
            )?;
            let mapped = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in mapped {
                let (id, subject, class_id, instance, frag, generation, parent, lease, acq, rel) =
                    row?;
                out.push(Holding {
                    id: id as u64,
                    subject,
                    class_id,
                    instance,
                    frag: frag_from_json(&frag)?,
                    generation,
                    parent: parent.map(|p| p as u64),
                    lease_expires_at: lease.map(|t| t as u64),
                    acquired_at: acq as u64,
                    released_at: rel.map(|t| t as u64),
                });
            }
            out
        };
        for h in rows {
            l.restore_row(h);
        }
        let store = LedgerStore {
            inner: Mutex::new(l),
            db,
        };
        // Substrate rows first: they are children of plugin rows, so the
        // stale-process reconcile below can then release parents cleanly.
        let substrate = store.reconcile_substrate()?;
        let stale_rows = store.reconcile_stale_process_rows()?;
        let journal_pending = {
            let conn = store.db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM journal WHERE state != 'done'",
                [],
                |r| r.get::<_, i64>(0),
            )? as usize
        };
        Ok((
            store,
            OpenReport {
                stale_rows,
                journal_pending,
                substrate,
            },
        ))
    }

    /// Run `f` as one atomic compound write: the ledger mutex and the database
    /// are held for the duration (lock order: ledger → db), the in-memory
    /// ledger is snapshotted, and `f` sees both the ledger and the connection
    /// inside a `BEGIN IMMEDIATE` transaction. On any error the SQLite
    /// transaction rolls back and the snapshot is restored — a failed compound
    /// operation leaves neither in-memory nor on-disk partial state.
    pub fn transaction<T>(
        &self,
        f: impl FnOnce(&mut Ledger, &Connection) -> Result<T, KernelError>,
    ) -> Result<T, KernelError> {
        let mut l = self.inner.lock().unwrap();
        let snapshot = l.clone();
        let conn = self.db.lock().unwrap();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(&mut l, &conn) {
            Ok(t) => match conn.execute_batch("COMMIT") {
                Ok(()) => Ok(t),
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    *l = snapshot;
                    Err(e.into())
                }
            },
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                *l = snapshot;
                Err(e)
            }
        }
    }

    /// A previous kernel process is gone, and so are the plugins it spawned
    /// (the host kills them on drop; a hard crash may leave orphans that this
    /// kernel cannot yet identify against the substrate). Their `kernel/plugin`
    /// and `kernel/subscription` rows are decayed holdings: tombstone them,
    /// children first. Returns how many rows were reconciled.
    fn reconcile_stale_process_rows(&self) -> Result<usize, KernelError> {
        let now = crate::db::now_unix();
        self.transaction(|l, conn| {
            let mut stale: Vec<(u64, String)> = Vec::new();
            for class in [CLASS_SUBSCRIPTION, CLASS_PLUGIN] {
                stale.extend(
                    l.live()
                        .filter(|h| h.class_id == class)
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
                    if l.release(id, &generation, now).is_ok() {
                        n += 1;
                        if let Some(h) = l.holding(id) {
                            persist_on(conn, h)?;
                            // The row is the truth: tombstoned means its
                            // cleanup is done, whatever an earlier teardown
                            // journaled.
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

    // ---- cap-count pools (F1 applied to counting capabilities) ----

    /// Declare the capacity of one (cap, verb) pool. Idempotent; called when
    /// a capability is minted and again when the kernel reopens.
    pub fn set_pool(&self, cap_id: &str, verb: &str, capacity: u64) {
        self.inner.lock().unwrap().set_capacity(
            CLASS_CAP_COUNT,
            &pool_instance(cap_id, verb),
            Frag::Count(Count::Value(capacity)),
        );
    }

    /// Spend one unit from a pool: mint a spend row through the issuer gate.
    /// `Err(Denied)` is the gate refusing — the pool is exhausted.
    ///
    /// Spend rows are booked under `<subject>:spent`, not under the subject
    /// itself (the F3 monitor's convention): a spend is consumption of a
    /// pool that outlives the incarnation, not a holding the incarnation
    /// gives back — teardown of the plugin must not refund its budget.
    pub fn spend(&self, subject: &str, cap_id: &str, verb: &str, now: u64) -> Result<u64, KernelError> {
        let ids = self.spend_many(subject, &[(cap_id, verb, 1)], now)?;
        Ok(ids[0])
    }

    /// Mint several spend rows atomically (WP-06 plan runs, WP-08 firings:
    /// the rows of one compound action commit together or not at all). One
    /// row per entry, carrying `Count::Value(n)`; zero-count entries spend nothing
    /// and mint no row. Returns the minted row ids in entry order. A refusal
    /// by the issuer gate on any entry rolls the whole batch back.
    pub fn spend_many(
        &self,
        subject: &str,
        spends: &[(&str, &str, u64)],
        now: u64,
    ) -> Result<Vec<u64>, KernelError> {
        let spender = format!("{subject}:spent");
        self.transaction(|l, conn| {
            let mut ids = Vec::new();
            for (cap_id, verb, n) in spends {
                if *n == 0 {
                    continue;
                }
                let id = l
                    .grant(
                        &spender,
                        CLASS_CAP_COUNT,
                        &pool_instance(cap_id, verb),
                        Frag::Count(Count::Value(*n)),
                        SPEND_GENERATION,
                        None,
                        now,
                    )
                    .map_err(map_err)?;
                if let Some(h) = l.holding(id) {
                    persist_on(conn, h)?;
                }
                ids.push(id);
            }
            Ok(ids)
        })
    }

    /// The spent total of a pool: a fold over its live spend rows — never a
    /// cached counter (the ledger's representation policy).
    pub fn spent(&self, cap_id: &str, verb: &str) -> u64 {
        let instance = pool_instance(cap_id, verb);
        self.inner
            .lock()
            .unwrap()
            .live()
            .filter(|h| h.class_id == CLASS_CAP_COUNT && h.instance == instance)
            .map(|h| match &h.frag {
                Frag::Count(Count::Value(n)) => *n,
                _ => 0,
            })
            .sum()
    }

    // ---- exclusive holdings (plugins, subscriptions) ----

    /// Take an exclusive holding on `instance` of `class` for `subject`. The
    /// kernel is the issuer of these classes, so the capacity row is declared
    /// here as well; a live holding on the same instance refuses the grant.
    pub fn hold_exclusive(
        &self,
        subject: &str,
        class: &str,
        instance: &str,
        generation: &str,
        parent: Option<u64>,
        now: u64,
    ) -> Result<u64, KernelError> {
        self.transaction(|l, conn| {
            if l.capacity(class, instance).is_none() {
                l.set_capacity(class, instance, Frag::Ex(Ex::Token));
            }
            let id = l
                .grant(subject, class, instance, Frag::Ex(Ex::Token), generation, parent, now)
                .map_err(map_err)?;
            if let Some(h) = l.holding(id) {
                persist_on(conn, h)?;
            }
            Ok(id)
        })
    }

    /// Take an exclusive holding for a child spawned by another plugin:
    /// declare the instantiation edge and grant under the parent's holding in
    /// one transaction, so a refused grant leaves no stale declaration behind
    /// (decision 2: a cross-subject parent edge exists only along the
    /// instantiation relation).
    #[allow(clippy::too_many_arguments)]
    pub fn hold_exclusive_child(
        &self,
        subject: &str,
        class: &str,
        instance: &str,
        generation: &str,
        parent_subject: &str,
        parent: u64,
        now: u64,
    ) -> Result<u64, KernelError> {
        self.transaction(|l, conn| {
            l.declare_instantiation(subject, parent_subject);
            if l.capacity(class, instance).is_none() {
                l.set_capacity(class, instance, Frag::Ex(Ex::Token));
            }
            let id = l
                .grant(subject, class, instance, Frag::Ex(Ex::Token), generation, Some(parent), now)
                .map_err(map_err)?;
            if let Some(h) = l.holding(id) {
                persist_on(conn, h)?;
            }
            Ok(id)
        })
    }

    /// Release one holding (idempotent for the built-in classes).
    pub fn release(&self, id: u64, generation: &str, now: u64) -> Result<(), KernelError> {
        self.transaction(|l, conn| {
            l.release(id, generation, now).map_err(map_err)?;
            if let Some(h) = l.holding(id) {
                persist_on(conn, h)?;
            }
            Ok(())
        })
    }

    /// Transfer every live row of `from_subject` to `to_subject` in one
    /// transaction ([SEG-TX] segment commit: fragments untouched, FPU
    /// trivial). Returns how many rows moved.
    pub fn transfer_all(&self, from_subject: &str, to_subject: &str) -> Result<usize, KernelError> {
        self.transaction(|l, conn| {
            let rows: Vec<(u64, String)> = l
                .live()
                .filter(|h| h.subject == from_subject)
                .map(|h| (h.id, h.generation.clone()))
                .collect();
            for (id, generation) in &rows {
                l.transfer(*id, generation, from_subject, to_subject)
                    .map_err(map_err)?;
                if let Some(h) = l.holding(*id) {
                    persist_on(conn, h)?;
                }
            }
            Ok(rows.len())
        })
    }

    /// Hold a substrate resource (WP-02): one exclusive row of a holdable
    /// built-in class plus its substrate witness row (one transaction).
    /// `lease_secs`, when given, is a per-holding lease written to the row
    /// directly — the built-in classes declare no class-level lease (the
    /// declaration provides defaults only).
    #[allow(clippy::too_many_arguments)]
    pub fn hold_substrate(
        &self,
        subject: &str,
        class: &str,
        instance: &str,
        generation: &str,
        parent: Option<u64>,
        substrate: &Value,
        lease_secs: Option<u64>,
        now: u64,
    ) -> Result<u64, KernelError> {
        if !HOLDABLE_CLASSES.contains(&class) {
            return Err(KernelError::Denied(format!("class not holdable: {class}")));
        }
        self.transaction(|l, conn| {
            if l.capacity(class, instance).is_none() {
                l.set_capacity(class, instance, Frag::Ex(Ex::Token));
            }
            let id = l
                .grant(subject, class, instance, Frag::Ex(Ex::Token), generation, parent, now)
                .map_err(map_err)?;
            if let Some(secs) = lease_secs {
                l.set_lease(id, generation, Some(now + secs)).map_err(map_err)?;
            }
            if let Some(h) = l.holding(id) {
                persist_on(conn, h)?;
            }
            let kind = class.strip_prefix("kernel/").unwrap_or(class);
            conn.execute(
                "INSERT OR REPLACE INTO substrate (holding_id, kind, detail) VALUES (?1, ?2, ?3)",
                params![id as i64, kind, substrate.to_string()],
            )?;
            Ok(id)
        })
    }

    /// Renew a leased holding (heartbeat). With `lease_secs`, extend the
    /// per-holding lease from now; without, the law `renew` applies the class
    /// declaration's lease (a no-op for the lease-less built-ins).
    pub fn renew(
        &self,
        id: u64,
        generation: &str,
        lease_secs: Option<u64>,
        now: u64,
    ) -> Result<Option<u64>, KernelError> {
        self.transaction(|l, conn| {
            match lease_secs {
                Some(secs) => l.set_lease(id, generation, Some(now + secs)).map_err(map_err)?,
                None => l.renew(id, generation, now).map_err(map_err)?,
            }
            let expires = l.holding(id).and_then(|h| h.lease_expires_at);
            if let Some(h) = l.holding(id) {
                persist_on(conn, h)?;
            }
            Ok(expires)
        })
    }

    /// The involuntary path (WP-02): expire due leases (children first,
    /// parents wait on children with their own live lease — the conservative
    /// sweep ruling), then execute each released row's world action. All row
    /// tombstones commit in one transaction; a failed world action does not
    /// un-expire the row (the lease is the ledger fact) but is reported in
    /// the report's `failed` list — never silent.
    pub fn sweep_with_world<W: World>(
        &self,
        world: &mut W,
        now: u64,
    ) -> Result<SweepReport, KernelError> {
        self.transaction(|l, conn| {
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
                    report.released.push((h.id, h.class_id.clone(), h.instance.clone()));
                } else {
                    report.failed.push(h.id);
                }
                persist_on(conn, h)?;
            }
            Ok(report)
        })
    }

    /// Reconcile the substrate classes against the world on open: a previous
    /// kernel incarnation is dead, so a live `kernel/process` row whose
    /// incarnation is still running is killed (crash-only), freed ports and
    /// owner-dead lock files are released. All tombstones commit in one
    /// transaction; counts are returned for the audit chain.
    fn reconcile_substrate(&self) -> Result<SubstrateReconcile, KernelError> {
        let now = crate::db::now_unix();
        self.transaction(|l, conn| {
            let mut report = SubstrateReconcile::default();
            let rows: Vec<Holding> = l
                .live()
                .filter(|h| HOLDABLE_CLASSES.contains(&h.class_id.as_str()))
                .cloned()
                .collect();
            let mut details: std::collections::BTreeMap<u64, Value> = std::collections::BTreeMap::new();
            for h in &rows {
                let detail: Option<String> = conn
                    .query_row(
                        "SELECT detail FROM substrate WHERE holding_id = ?1",
                        params![h.id as i64],
                        |r| r.get(0),
                    )
                    .ok();
                if let Some(d) = detail.and_then(|d| serde_json::from_str(&d).ok()) {
                    details.insert(h.id, d);
                }
            }
            // Processes first: a killed orphan may own a lock file below.
            for class in [CLASS_PROCESS, CLASS_PORT, CLASS_FILE_LOCK] {
                for h in rows.iter().filter(|h| h.class_id == class) {
                    let detail = details.get(&h.id).cloned().unwrap_or(Value::Null);
                    match class {
                        CLASS_PROCESS => {
                            let pid = detail["pid"].as_u64().unwrap_or(0) as u32;
                            let start = detail["start"].as_u64().unwrap_or(0);
                            if proc_alive(pid, start) {
                                kill_pid(pid);
                                report.process_killed += 1;
                            } else {
                                report.process_tombstoned += 1;
                            }
                        }
                        CLASS_PORT => {
                            report.ports_tombstoned += 1;
                            if !port_free(&h.instance) {
                                report.ports_still_bound += 1;
                            }
                        }
                        CLASS_FILE_LOCK => {
                            let owner = detail["owner_pid"].as_u64().map(|p| p as u32);
                            let owner_start = detail["owner_start"].as_u64().unwrap_or(0);
                            if owner.is_some_and(|p| proc_alive(p, owner_start)) {
                                report.locks_kept += 1; // orphan still owns it; leave the file
                            } else {
                                let _ = std::fs::remove_file(&h.instance);
                                report.locks_removed += 1;
                            }
                        }
                        _ => unreachable!(),
                    }
                    let _ = l.release(h.id, &h.generation, now);
                    if let Some(row) = l.holding(h.id) {
                        persist_on(conn, row)?;
                        resolve_journal(conn, h.id, now)?;
                    }
                }
            }
            Ok(report)
        })
    }

    /// Crash-only teardown of everything `subject` holds (ownership closure,
    /// children first), executing the class inverses through `world`. The
    /// whole closure's tombstones and the journal entries commit in one
    /// transaction ([SAGA]); `world` must not call back into the ledger.
    /// Entries an earlier attempt left non-done are loaded and retried —
    /// resume is blind replay ([E-IDEM]). Returns the executor outcome and
    /// how many rows were released.
    pub fn teardown<W: World>(&self, subject: &str, world: &mut W, now: u64) -> Result<(RunOutcome, usize), KernelError> {
        self.transaction(|l, conn| {
            let before: Vec<u64> = l.live_closure(subject).iter().map(|it| it.id).collect();
            if before.is_empty() {
                return Ok((RunOutcome::Completed { failed: Vec::new() }, 0));
            }
            let mut journal = load_pending_journal(conn, &before)?;
            let outcome = teardown_with(l, &mut journal, world, subject, 0, None, now);
            let mut released = 0;
            for id in &before {
                if let Some(h) = l.holding(*id) {
                    if h.released_at.is_some() {
                        released += 1;
                    }
                    persist_on(conn, h)?;
                }
            }
            for e in journal.entries() {
                upsert_journal(conn, e, now)?;
            }
            Ok((outcome, released))
        })
    }

    pub fn live_snapshot(&self, subject: &str) -> Vec<LiveItem> {
        self.inner.lock().unwrap().live_snapshot(subject)
    }

    /// The live `kernel/cap` holding for a capability, if any (WP-03): the
    /// single source of truth for the cap's liveness — revoked, expired and
    /// swept, or died with its holder all read as `None`.
    pub fn cap_holding(&self, cap_id: &str) -> Option<Holding> {
        self.inner
            .lock()
            .unwrap()
            .live()
            .find(|h| h.class_id == CLASS_CAP && h.instance == cap_id)
            .cloned()
    }

    /// Revocation-scoped teardown (WP-03): release the ownership subtree rooted
    /// at `root` — the cap holding and everything that exists because the grant
    /// does (children first, the root last) — through `world`, journaled exactly
    /// like [`teardown`](Self::teardown) ([SAGA] write-ahead, non-done entries
    /// replayed on the next attempt). `key_scope` anchors the idempotency keys
    /// (use the cap id — stable across crashes). `extra` runs inside the same
    /// transaction after the teardown loop, so the caller's bookkeeping (mark
    /// the caps row revoked, release the cap's spend rows, close its pools)
    /// commits or rolls back with the tombstones.
    pub fn teardown_subtree<W: World>(
        &self,
        root: u64,
        key_scope: &str,
        world: &mut W,
        now: u64,
        extra: impl FnOnce(&mut Ledger, &Connection) -> Result<(), KernelError>,
    ) -> Result<(RunOutcome, usize), KernelError> {
        self.transaction(|l, conn| {
            let before: Vec<u64> = l.live_subtree(root).iter().map(|it| it.id).collect();
            let mut journal = load_pending_journal(conn, &before)?;
            let outcome =
                teardown_subtree_with(l, &mut journal, world, root, key_scope, 0, None, now);
            let mut released = 0;
            for id in &before {
                if let Some(h) = l.holding(*id) {
                    if h.released_at.is_some() {
                        released += 1;
                    }
                    persist_on(conn, h)?;
                }
            }
            for e in journal.entries() {
                upsert_journal(conn, e, now)?;
            }
            extra(l, conn)?;
            Ok((outcome, released))
        })
    }

    pub fn live_closure(&self, subject: &str) -> Vec<LiveItem> {
        self.inner.lock().unwrap().live_closure(subject)
    }

    pub fn holding(&self, id: u64) -> Option<Holding> {
        self.inner.lock().unwrap().holding(id).cloned()
    }

    /// Record that `parent_subject` instantiated `child` (decision 2: a
    /// cross-subject ownership edge is legal only along this relation).
    pub fn declare_instantiation(&self, child: &str, parent_subject: &str) {
        self.inner.lock().unwrap().declare_instantiation(child, parent_subject);
    }

    /// The global F1 invariant: for every (class, instance), ✓(● capacity · ◯ fold(live)).
    pub fn invariant(&self) -> Result<(), KernelError> {
        self.inner.lock().unwrap().invariant().map_err(map_err)
    }

    /// Row count per (class) for introspection and tests: (live, tombstones).
    pub fn counts(&self, class: &str) -> (usize, usize) {
        let l = self.inner.lock().unwrap();
        let live = l.live().filter(|h| h.class_id == class).count();
        let total = l.holdings().iter().filter(|h| h.class_id == class).count();
        (live, total - live)
    }
}

pub(crate) fn pool_instance(cap_id: &str, verb: &str) -> String {
    format!("{cap_id}/{verb}")
}

pub(crate) fn map_err(e: LedgerError) -> KernelError {
    match e {
        LedgerError::UnknownClass | LedgerError::AlgebraMismatch => {
            KernelError::Corrupt(format!("ledger: {e:?}"))
        }
        other => KernelError::Denied(format!("ledger: {other:?}")),
    }
}

// ---- substrate helpers (WP-02; Linux /proc) ----

/// Start time (clock ticks since boot) of a live process, from /proc.
/// Together with the pid it witnesses an exact incarnation (pids recycle).
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
pub(crate) fn persist_on(conn: &Connection, h: &Holding) -> Result<(), KernelError> {
    conn.execute(
        "INSERT OR REPLACE INTO holdings (id, subject, class_id, instance, frag, generation, \
         parent, lease_expires_at, acquired_at, released_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            h.id as i64,
            h.subject,
            h.class_id,
            h.instance,
            frag_to_json(&h.frag),
            h.generation,
            h.parent.map(|p| p as i64),
            h.lease_expires_at.map(|t| t as i64),
            h.acquired_at as i64,
            h.released_at.map(|t| t as i64),
        ],
    )?;
    Ok(())
}

/// Load the journal entries an earlier teardown left non-done for the given
/// holdings. Everything non-done loads as `Pending`: resume is blind replay
/// (`Orchestrator::resume`'s Failed→Pending reset), and with entries committed
/// in the teardown transaction, only `failed` can ever be on disk.
fn load_pending_journal(conn: &Connection, holdings: &[u64]) -> Result<Journal, KernelError> {
    let mut stmt = conn.prepare(
        "SELECT holding_id, grade, idem_key FROM journal WHERE state != 'done'",
    )?;
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
        if !holdings.contains(&(hid as u64)) {
            continue;
        }
        entries.push(JournalEntry {
            holding_id: hid as u64,
            grade: grade_from_str(&grade)?,
            idem_key,
            state: JState::Pending,
        });
    }
    Ok(Journal::from_entries(entries))
}

fn upsert_journal(conn: &Connection, e: &JournalEntry, now: u64) -> Result<(), KernelError> {
    conn.execute(
        "INSERT OR REPLACE INTO journal (holding_id, grade, idem_key, state, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            e.holding_id as i64,
            grade_str(e.grade),
            e.idem_key,
            state_str(e.state),
            now as i64,
        ],
    )?;
    Ok(())
}

/// A tombstoned holding's cleanup is done by definition (rows are the truth):
/// resolve any non-done journal entry it left behind.
fn resolve_journal(conn: &Connection, holding_id: u64, now: u64) -> Result<(), KernelError> {
    conn.execute(
        "UPDATE journal SET state = 'done', updated_at = ?2 \
         WHERE holding_id = ?1 AND state != 'done'",
        params![holding_id as i64, now as i64],
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

/// Fragment serialization for the rows table. Keep small fraction parts as JSON
/// numbers for compatibility; larger exact parts use decimal strings.
pub fn frag_to_json(f: &Frag) -> String {
    let v = match f {
        Frag::Ex(Ex::Token) => json!({"ex": "token"}),
        Frag::Ex(Ex::Bot) => json!({"ex": "bot"}),
        Frag::Count(Count::Value(n)) => json!({"count": n}),
        Frag::Count(Count::Invalid) => json!({"count": "invalid"}),
        Frag::Set(GSet(s)) => json!({"set": s.iter().collect::<Vec<_>>()}),
        Frag::Range(r) => json!({"range": r.spans, "bot": r.bot}),
        Frag::Frac(q) => {
            let (num, den) = q.parts();
            let part = |s: String| match s.parse::<u64>() {
                Ok(n) => json!(n),
                Err(_) => json!(s),
            };
            json!({"frac": [part(num), part(den)]})
        }
    };
    v.to_string()
}

pub fn frag_from_json(s: &str) -> Result<Frag, KernelError> {
    let v: Value = serde_json::from_str(s)
        .map_err(|e| KernelError::Corrupt(format!("holding fragment json: {e}")))?;
    if let Some(ex) = v.get("ex").and_then(|x| x.as_str()) {
        return Ok(Frag::Ex(if ex == "token" { Ex::Token } else { Ex::Bot }));
    }
    if let Some(n) = v.get("count").and_then(|x| x.as_u64()) {
        return Ok(Frag::Count(Count::Value(n)));
    }
    if v.get("count").and_then(|x| x.as_str()) == Some("invalid") {
        return Ok(Frag::Count(Count::Invalid));
    }
    if let Some(items) = v.get("set").and_then(|x| x.as_array()) {
        return Ok(Frag::Set(GSet(
            items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect(),
        )));
    }
    if let Some(spans) = v.get("range").and_then(|x| x.as_array()) {
        let spans: Vec<(u64, u64)> = spans
            .iter()
            .filter_map(|p| Some((p.get(0)?.as_u64()?, p.get(1)?.as_u64()?)))
            .collect();
        return Ok(Frag::Range(Ranges {
            spans,
            bot: v.get("bot").and_then(|b| b.as_bool()).unwrap_or(false),
        }));
    }
    if let Some(q) = v.get("frac").and_then(|x| x.as_array()) {
        let part = |v: &Value| {
            v.as_u64().map(|n| n.to_string())
                .or_else(|| v.as_str().map(str::to_owned))
        };
        let parsed = (|| {
            if q.len() != 2 {
                return None;
            }
            Frac::from_parts(&part(&q[0])?, &part(&q[1])?)
        })();
        return parsed.map(Frag::Frac)
            .ok_or_else(|| KernelError::Corrupt(format!("holding fraction: {s}")));
    }
    Err(KernelError::Corrupt(format!("unknown holding fragment: {s}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use portos_rm::ra::Ra;

    fn store(tag: &str) -> (Arc<Mutex<Connection>>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("portos-ledger-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = Arc::new(Mutex::new(crate::db::open(&root).unwrap()));
        (db, root)
    }

    #[test]
    fn fragments_round_trip_through_json() {
        let all = [
            Frag::Ex(Ex::Token),
            Frag::Count(Count::Value(7)),
            Frag::Count(Count::Value(u64::MAX)),
            Frag::Count(Count::Invalid),
            Frag::Set(GSet::of(&["a", "b"])),
            Frag::Range(Ranges::of(&[(0, 8), (16, 32)])),
            Frag::Frac(Frac::new(2, 6)),
            Frag::Frac(Frac::invalid()),
            Frag::Frac(Frac::new(1, 2).op(&Frac::new(1, u64::MAX))),
        ];
        for f in all {
            assert_eq!(frag_from_json(&frag_to_json(&f)).unwrap(), f);
        }
        assert_eq!(frag_from_json(r#"{"frac":[2,6]}"#).unwrap(), Frag::Frac(Frac::new(1, 3)));
        assert!(frag_from_json(r#"{"frac":[1,0]}"#).is_err());
        assert!(frag_from_json(r#"{"frac":[1]}"#).is_err());
    }

    /// Spend rows are the truth and survive a reopen: the pool continues where
    /// it was, and the gate refuses at capacity.
    #[test]
    fn spend_rows_persist_and_gate_refuses_at_capacity() {
        let (db, root) = store("spend");
        {
            let (l, report) = LedgerStore::open(db.clone()).unwrap();
            assert_eq!(report.stale_rows, 0);
            l.set_pool("cap_x", "emit", 2);
            l.spend("plugin:a", "cap_x", "emit", 1).unwrap();
            assert_eq!(l.spent("cap_x", "emit"), 1);
            l.invariant().unwrap();
        }
        let (l, _report) = LedgerStore::open(db.clone()).unwrap();
        l.set_pool("cap_x", "emit", 2); // the cap table re-declares pools on open
        assert_eq!(l.spent("cap_x", "emit"), 1, "spend row reloaded");
        l.spend("plugin:a", "cap_x", "emit", 2).unwrap();
        let e = l.spend("plugin:a", "cap_x", "emit", 3).unwrap_err();
        assert!(matches!(e, KernelError::Denied(_)), "third spend refused by the issuer gate");
        assert_eq!(l.spent("cap_x", "emit"), 2);
        assert_eq!(l.counts(CLASS_CAP_COUNT), (2, 0));
        l.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn count_overflow_refuses_atomically_and_survives_reopen() {
        let (db, root) = store("count-overflow");
        {
            let (l, _) = LedgerStore::open(db.clone()).unwrap();
            l.set_pool("cap_max", "use", u64::MAX);
            let err = l.spend_many("user", &[("cap_max", "use", u64::MAX), ("cap_max", "use", 1)], 1).unwrap_err();
            assert!(matches!(err, KernelError::Denied(_)));
            assert_eq!(l.spent("cap_max", "use"), 0);
            assert_eq!(l.counts(CLASS_CAP_COUNT), (0, 0));
        }
        {
            let (l, _) = LedgerStore::open(db.clone()).unwrap();
            l.set_pool("cap_max", "use", u64::MAX);
            assert_eq!(l.spent("cap_max", "use"), 0);
            l.spend_many("user", &[("cap_max", "use", u64::MAX)], 2).unwrap();
            assert!(l.spend("user", "cap_max", "use", 3).is_err());
            assert_eq!(l.spent("cap_max", "use"), u64::MAX);
            l.invariant().unwrap();
        }
        let (l, _) = LedgerStore::open(db).unwrap();
        l.set_pool("cap_max", "use", u64::MAX);
        assert_eq!(l.spent("cap_max", "use"), u64::MAX);
        assert!(l.spend("user", "cap_max", "use", 4).is_err());
        l.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A compound write commits as one transaction: when any step fails — the
    /// issuer gate refusing one spend of a batch, or an injected error — no
    /// row survives on disk and the in-memory ledger is restored
    /// (attachments §4.3 rule 5: ②③同事务).
    #[test]
    fn compound_ledger_write_is_atomic_under_injected_failure() {
        let (db, root) = store("atomic");
        {
            let (l, _report) = LedgerStore::open(db.clone()).unwrap();
            l.set_pool("cap_x", "emit", 1);
            // A batch of two spends against a pool of capacity 1: the gate
            // refuses the second, so the first must survive nowhere.
            let e = l
                .spend_many("plugin:a", &[("cap_x", "emit", 1), ("cap_x", "emit", 1)], 1)
                .unwrap_err();
            assert!(matches!(e, KernelError::Denied(_)));
            assert_eq!(l.spent("cap_x", "emit"), 0, "no partial spend in memory");
            // An injected failure after a successful row write.
            let r: Result<(), KernelError> = l.transaction(|led, conn| {
                led.set_capacity(CLASS_PLUGIN, "b", Frag::Ex(Ex::Token));
                let id = led
                    .grant("plugin:b", CLASS_PLUGIN, "b", Frag::Ex(Ex::Token), "tok", None, 1)
                    .map_err(map_err)?;
                persist_on(conn, &led.holding(id).unwrap())?;
                Err(KernelError::Corrupt("injected".into()))
            });
            assert!(r.is_err());
            assert!(l.live_snapshot("plugin:b").is_empty(), "in-memory rolled back");
            // The committed half: one spend lands and persists.
            l.spend("plugin:a", "cap_x", "emit", 1).unwrap();
            assert_eq!(l.spent("cap_x", "emit"), 1);
        }
        let (l, _report) = LedgerStore::open(db.clone()).unwrap();
        l.set_pool("cap_x", "emit", 1); // the cap table re-declares pools on open
        assert_eq!(l.spent("cap_x", "emit"), 1, "committed row reloaded");
        assert_eq!(l.inner.lock().unwrap().live_count(), 1, "no partial row on disk");
        l.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A failing world action is journaled ([SAGA]) and the record survives a
    /// reopen; the next teardown of the same subject replays it exactly once
    /// (resume = blind replay, [E-IDEM]).
    #[test]
    fn journal_entries_survive_reopen_and_replay_once() {
        use std::collections::BTreeMap;

        /// Fails the first world action on one holding; counts actions.
        struct FlakyWorld {
            fail_once: Option<u64>,
            actions: BTreeMap<u64, u32>,
        }
        impl World for FlakyWorld {
            fn release(&mut self, item: &LiveItem) -> Result<(), ()> {
                *self.actions.entry(item.id).or_insert(0) += 1;
                if self.fail_once == Some(item.id) {
                    self.fail_once = None;
                    return Err(());
                }
                Ok(())
            }
            fn compensate(&mut self, _item: &LiveItem, _key: &str) -> Result<bool, ()> {
                Ok(true)
            }
        }

        let (db, root) = store("journal");
        let journal_states = |db: &Arc<Mutex<Connection>>| -> Vec<(i64, String)> {
            let conn = db.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT holding_id, state FROM journal ORDER BY holding_id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };

        let (l, report) = LedgerStore::open(db.clone()).unwrap();
        assert_eq!(report.journal_pending, 0);
        // A holding of a class reconcile does not tombstone (a pool row), so
        // its failed entry survives the reopen below for the replay to show.
        l.set_pool("cap_x", "emit", 5);
        let c = l
            .transaction(|led, conn| {
                let id = led
                    .grant(
                        "plugin:x",
                        CLASS_CAP_COUNT,
                        &pool_instance("cap_x", "emit"),
                        Frag::Count(Count::Value(1)),
                        "g",
                        None,
                        1,
                    )
                    .map_err(map_err)?;
                persist_on(conn, &led.holding(id).unwrap())?;
                Ok(id)
            })
            .unwrap();

        let (outcome, released) = {
            let mut w = FlakyWorld { fail_once: Some(c), actions: BTreeMap::new() };
            let r = l.teardown("plugin:x", &mut w, 2).unwrap();
            assert_eq!(w.actions[&c], 1, "one failed attempt");
            r
        };
        assert!(matches!(outcome, RunOutcome::Completed { ref failed } if failed.as_slice() == [c]));
        assert_eq!(released, 0, "nothing released: the only action failed");
        assert_eq!(journal_states(&db), vec![(c as i64, "failed".to_string())]);
        assert!(l.holding(c).unwrap().released_at.is_none(), "row still live");

        // Reopen: the pending entry is reported, the row is still live.
        drop(l);
        let (l, report) = LedgerStore::open(db.clone()).unwrap();
        assert_eq!(report.journal_pending, 1, "failed entry survived the reopen");
        assert!(l.holding(c).unwrap().released_at.is_none());
        l.set_pool("cap_x", "emit", 5); // the cap table re-declares pools on open

        // The next teardown of the subject replays the entry exactly once.
        let mut w = FlakyWorld { fail_once: None, actions: BTreeMap::new() };
        let (outcome, released) = l.teardown("plugin:x", &mut w, 2).unwrap();
        assert!(matches!(outcome, RunOutcome::Completed { ref failed } if failed.is_empty()));
        assert_eq!(released, 1);
        assert_eq!(w.actions[&c], 1, "replayed exactly once on this teardown");
        assert_eq!(journal_states(&db), vec![(c as i64, "done".to_string())]);

        let (_, report) = LedgerStore::open(db.clone()).unwrap();
        assert_eq!(report.journal_pending, 0);
        l.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Plugin and subscription rows from a previous kernel process are decayed
    /// holdings: reopening tombstones them children first, and the instance
    /// becomes available again.
    #[test]
    fn stale_plugin_rows_are_reconciled_on_open() {        let (db, root) = store("stale");
        {
            let (l, _) = LedgerStore::open(db.clone()).unwrap();
            let p = l.hold_exclusive("plugin:x", CLASS_PLUGIN, "x", "tok1", None, 1).unwrap();
            l.hold_exclusive("plugin:x", CLASS_SUBSCRIPTION, "7", "sub", Some(p), 1).unwrap();
            assert!(l.hold_exclusive("plugin:x", CLASS_PLUGIN, "x", "tok2", None, 1).is_err(), "name taken while live");
            // A child plugin of x (parent/child instantiation, WP-04): the
            // parent row's release is blocked while the child lives, so
            // reconcile must reach a children-first fixpoint.
            let c = l
                .hold_exclusive_child("plugin:y", CLASS_PLUGIN, "y", "tokc", "plugin:x", p, 1)
                .unwrap();
            assert_eq!(l.holding(c).unwrap().parent, Some(p));
        }
        let (l, report) = LedgerStore::open(db.clone()).unwrap();
        assert_eq!(report.stale_rows, 3);
        assert_eq!(l.counts(CLASS_PLUGIN), (0, 2));
        assert_eq!(l.counts(CLASS_SUBSCRIPTION), (0, 1));
        l.hold_exclusive("plugin:x", CLASS_PLUGIN, "x", "tok2", None, 2).unwrap();
        l.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[derive(Default)]
    struct RecordingWorld {
        released: Vec<u64>,
    }
    impl World for RecordingWorld {
        fn release(&mut self, item: &LiveItem) -> Result<(), ()> {
            self.released.push(item.id);
            Ok(())
        }
        fn compensate(&mut self, _item: &LiveItem, _key: &str) -> Result<bool, ()> {
            Ok(true)
        }
    }

    fn register_test_class(l: &LedgerStore, class: &str, lease_secs: Option<u64>) {
        l.inner.lock().unwrap().register_class(ClassDecl {
            class_id: class.to_string(),
            algebra: AlgebraTag::Exclusive,
            release_idempotent: true,
            lease_secs,
            revert_grade: RevertGrade::Inverse,
        });
    }

    /// Conservative sweep (ruling 3, mirror of the law): an expired parent
    /// waits while a child holds its own unexpired lease; lease-None
    /// descendants go with the parent, children first, world actions included.
    #[test]
    fn expired_parent_waits_for_child_with_its_own_lease() {
        let (db, root) = store("sweep");
        let (l, _report) = LedgerStore::open(db.clone()).unwrap();
        register_test_class(&l, "test/leased", Some(1));
        register_test_class(&l, "test/leased-long", Some(3600));
        register_test_class(&l, "test/unleased", None);
        let (p, c, g) = l
            .transaction(|led, conn| {
                for (class, instance) in
                    [("test/leased", "p"), ("test/leased-long", "c"), ("test/unleased", "g")]
                {
                    led.set_capacity(class, instance, Frag::Ex(Ex::Token));
                }
                let p = led
                    .grant("test", "test/leased", "p", Frag::Ex(Ex::Token), "gp", None, 0)
                    .map_err(map_err)?;
                let c = led
                    .grant("test", "test/leased-long", "c", Frag::Ex(Ex::Token), "gc", Some(p), 0)
                    .map_err(map_err)?;
                let g = led
                    .grant("test", "test/unleased", "g", Frag::Ex(Ex::Token), "gg", Some(c), 0)
                    .map_err(map_err)?;
                for id in [p, c, g] {
                    persist_on(conn, &led.holding(id).unwrap())?;
                }
                Ok((p, c, g))
            })
            .unwrap();

        let mut world = RecordingWorld::default();
        let r = l.sweep_with_world(&mut world, 2).unwrap();
        assert!(r.released.is_empty(), "the expired parent waits on the child's own lease");
        assert_eq!(l.inner.lock().unwrap().live_count(), 3);
        assert!(world.released.is_empty(), "no world action while waiting");

        let r = l.sweep_with_world(&mut world, 3601).unwrap();
        assert_eq!(r.released.len(), 3, "once the child is due, the whole chain goes");
        assert_eq!(world.released, vec![g, c, p], "children first, world actions too");
        l.invariant().unwrap();

        // Tombstones are durable.
        drop(l);
        let (l, _report) = LedgerStore::open(db.clone()).unwrap();
        assert_eq!(l.inner.lock().unwrap().live_count(), 0, "all rows stayed tombstoned");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Substrate reconcile (WP-02): a live `kernel/process` row of a previous
    /// kernel incarnation whose process is still running is killed on open
    /// and the row tombstoned — a previous incarnation never survives a
    /// kernel restart (crash-only).
    #[test]
    fn reconcile_kills_a_live_process_of_a_previous_kernel_incarnation() {
        use std::os::unix::process::ExitStatusExt;
        let (db, root) = store("reconcile-substrate");
        let mut child = std::process::Command::new("sleep").arg("300").spawn().unwrap();
        let pid = child.id();
        let start = proc_start_time(pid).expect("child visible in /proc");
        {
            let (l, _report) = LedgerStore::open(db.clone()).unwrap();
            l.hold_substrate(
                "plugin:x",
                CLASS_PROCESS,
                &format!("x/{pid}"),
                &format!("{pid}:{start}"),
                None,
                &json!({"pid": pid, "start": start}),
                None,
                1,
            )
            .unwrap();
            assert_eq!(l.counts(CLASS_PROCESS), (1, 0));
            // drop without touching the sleeper: it plays the orphan of a
            // kernel that died hard.
        }
        let (l, report) = LedgerStore::open(db.clone()).unwrap();
        assert_eq!(report.substrate.process_killed, 1);
        assert_eq!(l.counts(CLASS_PROCESS), (0, 1), "row tombstoned");
        assert_eq!(
            child.wait().unwrap().signal(),
            Some(9),
            "the sleeper got SIGKILL from the reconcile"
        );
        assert!(!proc_alive(pid, start), "the exact incarnation is gone");
        l.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
