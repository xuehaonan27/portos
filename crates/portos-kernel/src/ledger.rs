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
//!   - **Built-in resource classes** (档 0, handler = the kernel itself):
//!       `kernel/cap-count`   capability counting budgets — one capacity row
//!                            per (cap, verb) pool, one spend row per
//!                            exercise; the balance is only ever recomputed
//!                            (F1 consequence 1: no subtraction) and the gate
//!                            is the issuer gate `can_mint` (consequence 2).
//!                            Replaces the in-place decrement (roadmap H.2-1).
//!       `kernel/plugin`      one exclusive holding per spawned plugin
//!                            instance, generation = the spawn token; released
//!                            only by crash-only teardown.
//!       `kernel/subscription` one exclusive holding per event subscription,
//!                            child of the subscriber's plugin holding: a
//!                            standing inbound channel is a holding, so
//!                            unloading a plugin unsubscribes it, children
//!                            first (endstate §8.5).
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
use portos_rm::teardown::{Journal, RunOutcome, World, teardown_with};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub const CLASS_CAP_COUNT: &str = "kernel/cap-count";
pub const CLASS_PLUGIN: &str = "kernel/plugin";
pub const CLASS_SUBSCRIPTION: &str = "kernel/subscription";

/// Generation string of spend rows (they are never released individually).
const SPEND_GENERATION: &str = "spend";

pub struct LedgerStore {
    inner: Mutex<Ledger>,
    db: Arc<Mutex<Connection>>,
}

impl LedgerStore {
    /// Open the ledger: register the built-in classes, reload every row, and
    /// reconcile what a previous kernel process left behind.
    pub fn open(db: Arc<Mutex<Connection>>) -> Result<(LedgerStore, usize), KernelError> {
        let mut l = Ledger::new();
        for (id, algebra) in [
            (CLASS_CAP_COUNT, AlgebraTag::Counted),
            (CLASS_PLUGIN, AlgebraTag::Exclusive),
            (CLASS_SUBSCRIPTION, AlgebraTag::Exclusive),
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
        let stale = store.reconcile_stale_process_rows()?;
        Ok((store, stale))
    }

    /// A previous kernel process is gone, and so are the plugins it spawned
    /// (the host kills them on drop; a hard crash may leave orphans that this
    /// kernel cannot yet identify against the substrate). Their `kernel/plugin`
    /// and `kernel/subscription` rows are decayed holdings: tombstone them,
    /// children first. Returns how many rows were reconciled.
    fn reconcile_stale_process_rows(&self) -> Result<usize, KernelError> {
        let now = crate::db::now_unix();
        let mut l = self.inner.lock().unwrap();
        let mut stale: Vec<(u64, String)> = Vec::new();
        for class in [CLASS_SUBSCRIPTION, CLASS_PLUGIN] {
            stale.extend(
                l.live()
                    .filter(|h| h.class_id == class)
                    .map(|h| (h.id, h.generation.clone())),
            );
        }
        let mut n = 0;
        for (id, generation) in &stale {
            if l.release(*id, generation, now).is_ok() {
                n += 1;
                if let Some(h) = l.holding(*id) {
                    persist(&self.db, h)?;
                }
            }
        }
        Ok(n)
    }

    // ---- cap-count pools (F1 applied to counting capabilities) ----

    /// Declare the capacity of one (cap, verb) pool. Idempotent; called when
    /// a capability is minted and again when the kernel reopens.
    pub fn set_pool(&self, cap_id: &str, verb: &str, capacity: u64) {
        self.inner.lock().unwrap().set_capacity(
            CLASS_CAP_COUNT,
            &pool_instance(cap_id, verb),
            Frag::Count(Count(capacity)),
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
        let spender = format!("{subject}:spent");
        let mut l = self.inner.lock().unwrap();
        let id = l
            .grant(
                &spender,
                CLASS_CAP_COUNT,
                &pool_instance(cap_id, verb),
                Frag::Count(Count(1)),
                SPEND_GENERATION,
                None,
                now,
            )
            .map_err(map_err)?;
        if let Some(h) = l.holding(id) {
            persist(&self.db, h)?;
        }
        Ok(id)
    }

    /// The spent total of a pool: a fold over its live spend rows — never a
    /// cached counter (F1 consequence 1).
    pub fn spent(&self, cap_id: &str, verb: &str) -> u64 {
        let instance = pool_instance(cap_id, verb);
        self.inner
            .lock()
            .unwrap()
            .live()
            .filter(|h| h.class_id == CLASS_CAP_COUNT && h.instance == instance)
            .map(|h| match &h.frag {
                Frag::Count(Count(n)) => *n,
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
        let mut l = self.inner.lock().unwrap();
        if l.capacity(class, instance).is_none() {
            l.set_capacity(class, instance, Frag::Ex(Ex::Token));
        }
        let id = l
            .grant(subject, class, instance, Frag::Ex(Ex::Token), generation, parent, now)
            .map_err(map_err)?;
        if let Some(h) = l.holding(id) {
            persist(&self.db, h)?;
        }
        Ok(id)
    }

    /// Release one holding (idempotent for the built-in classes).
    pub fn release(&self, id: u64, generation: &str, now: u64) -> Result<(), KernelError> {
        let mut l = self.inner.lock().unwrap();
        l.release(id, generation, now).map_err(map_err)?;
        if let Some(h) = l.holding(id) {
            persist(&self.db, h)?;
        }
        Ok(())
    }

    /// Crash-only teardown of everything `subject` holds (ownership closure,
    /// children first), executing the class inverses through `world`. The
    /// ledger lock is held throughout; `world` must not call back into the
    /// ledger. Returns the executor outcome and how many rows were released.
    pub fn teardown<W: World>(&self, subject: &str, world: &mut W, now: u64) -> Result<(RunOutcome, usize), KernelError> {
        let mut l = self.inner.lock().unwrap();
        let before: Vec<u64> = l.live_closure(subject).iter().map(|it| it.id).collect();
        if before.is_empty() {
            return Ok((RunOutcome::Completed { failed: Vec::new() }, 0));
        }
        let mut journal = Journal::default();
        let outcome = teardown_with(&mut l, &mut journal, world, subject, 0, None, now);
        let mut released = 0;
        for id in before {
            if let Some(h) = l.holding(id) {
                if h.released_at.is_some() {
                    released += 1;
                }
                persist(&self.db, h)?;
            }
        }
        Ok((outcome, released))
    }

    pub fn live_snapshot(&self, subject: &str) -> Vec<LiveItem> {
        self.inner.lock().unwrap().live_snapshot(subject)
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

fn pool_instance(cap_id: &str, verb: &str) -> String {
    format!("{cap_id}/{verb}")
}

fn map_err(e: LedgerError) -> KernelError {
    match e {
        LedgerError::UnknownClass | LedgerError::AlgebraMismatch => {
            KernelError::Corrupt(format!("ledger: {e:?}"))
        }
        other => KernelError::Denied(format!("ledger: {other:?}")),
    }
}

fn persist(db: &Arc<Mutex<Connection>>, h: &Holding) -> Result<(), KernelError> {
    let conn = db.lock().unwrap();
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

/// Fragment serialization for the rows table. The RA library of the law
/// crate is dependency-free, so the wire form lives here.
pub fn frag_to_json(f: &Frag) -> String {
    let v = match f {
        Frag::Ex(Ex::Token) => json!({"ex": "token"}),
        Frag::Ex(Ex::Bot) => json!({"ex": "bot"}),
        Frag::Count(Count(n)) => json!({"count": n}),
        Frag::Set(GSet(s)) => json!({"set": s.iter().collect::<Vec<_>>()}),
        Frag::Range(r) => json!({"range": r.spans, "bot": r.bot}),
        Frag::Frac(q) => json!({"frac": [q.num, q.den]}),
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
        return Ok(Frag::Count(Count(n)));
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
        let num = q.first().and_then(|n| n.as_u64()).unwrap_or(0);
        let den = q.get(1).and_then(|d| d.as_u64()).unwrap_or(1);
        return Ok(Frag::Frac(Frac::new(num, den.max(1))));
    }
    Err(KernelError::Corrupt(format!("unknown holding fragment: {s}")))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Frag::Count(Count(7)),
            Frag::Set(GSet::of(&["a", "b"])),
            Frag::Range(Ranges::of(&[(0, 8), (16, 32)])),
            Frag::Frac(Frac::new(2, 6)),
        ];
        for f in all {
            assert_eq!(frag_from_json(&frag_to_json(&f)).unwrap(), f);
        }
    }

    /// Spend rows are the truth and survive a reopen: the pool continues where
    /// it was, and the gate refuses at capacity (no subtraction anywhere).
    #[test]
    fn spend_rows_persist_and_gate_refuses_at_capacity() {
        let (db, root) = store("spend");
        {
            let (l, stale) = LedgerStore::open(db.clone()).unwrap();
            assert_eq!(stale, 0);
            l.set_pool("cap_x", "emit", 2);
            l.spend("plugin:a", "cap_x", "emit", 1).unwrap();
            assert_eq!(l.spent("cap_x", "emit"), 1);
            l.invariant().unwrap();
        }
        let (l, _) = LedgerStore::open(db.clone()).unwrap();
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

    /// Plugin and subscription rows from a previous kernel process are decayed
    /// holdings: reopening tombstones them children first, and the instance
    /// becomes available again.
    #[test]
    fn stale_plugin_rows_are_reconciled_on_open() {
        let (db, root) = store("stale");
        {
            let (l, _) = LedgerStore::open(db.clone()).unwrap();
            let p = l.hold_exclusive("plugin:x", CLASS_PLUGIN, "x", "tok1", None, 1).unwrap();
            l.hold_exclusive("plugin:x", CLASS_SUBSCRIPTION, "7", "sub", Some(p), 1).unwrap();
            assert!(l.hold_exclusive("plugin:x", CLASS_PLUGIN, "x", "tok2", None, 1).is_err(), "name taken while live");
        }
        let (l, stale) = LedgerStore::open(db.clone()).unwrap();
        assert_eq!(stale, 2);
        assert_eq!(l.counts(CLASS_PLUGIN), (0, 1));
        assert_eq!(l.counts(CLASS_SUBSCRIPTION), (0, 1));
        l.hold_exclusive("plugin:x", CLASS_PLUGIN, "x", "tok2", None, 2).unwrap();
        l.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
