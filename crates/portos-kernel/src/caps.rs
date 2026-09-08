//! Capability table. Currently using SQLite to store it.
//!
//! Counting constraints are **pools in the holding ledger** (spec F1, roadmap
//! H.2-1): `constraints.counts[verb]` is the declared capacity of the
//! (cap, verb) pool, never mutated; every exercise mints one spend row through
//! the issuer gate, and the balance is recomputed from the rows. The in-place
//! decrement this replaced was the drill's first "already-frozen theory
//! flowing back into existing code" item.
//!
//! A granted capability is itself a holding (WP-03): `mint`/`attenuate` grant
//! a `kernel/cap` row (subject = the grantee, instance = generation = the cap
//! id, lease = `constraints.expires_at`) in the same transaction as the caps
//! row and the pool capacities. **The holding is the single source of truth
//! for liveness** — revocation tears down the grant's ownership subtree
//! (children first, then the cap holding; what exists because of the grant
//! dies with it), expiry rides the sweeper, and a cap whose holder subject was
//! torn down is dead. The `caps.revoked` flag remains as derivation-tree
//! bookkeeping; the issuer gate consults only the holding (plus the expiry
//! column, to cover sweeper lag).

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use portos_proto::{Capability, Constraints};
use portos_rm::ledger::{Frag, Ledger};
use portos_rm::ra::{Count, Ex};
use portos_rm::teardown::World;
use rusqlite::{Connection, OptionalExtension, params};

use crate::KernelError;
use crate::ledger::{
    CLASS_CAP, CLASS_CAP_COUNT, LedgerStore, map_err, persist_on, pool_instance,
};

pub struct CapStore {
    // Currently we use a database to store capabilities.
    db: Arc<Mutex<Connection>>,
    ledger: Arc<LedgerStore>,
}

impl CapStore {
    pub fn new(db: Arc<Mutex<Connection>>, ledger: Arc<LedgerStore>) -> CapStore {
        CapStore { db, ledger }
    }

    /// Declare every live (cap, verb) pool's capacity in the ledger. Called on
    /// open (the ledger reloads spend rows, the cap table owns capacities).
    /// Revoked caps keep their pools closed (revocation zeroed them and
    /// released their spend rows in the same transaction).
    pub fn rebuild_pools(&self) -> Result<(), KernelError> {
        let rows: Vec<String> = {
            let db = self.db.lock().unwrap();
            let mut stmt = db.prepare("SELECT json FROM caps WHERE revoked=0")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()?
        };
        for j in rows {
            if let Ok(cap) = serde_json::from_str::<Capability>(&j) {
                self.declare_pools(&cap);
            }
        }
        Ok(())
    }

    fn declare_pools(&self, cap: &Capability) {
        for (verb, n) in &cap.constraints.counts {
            self.ledger.set_pool(&cap.cap_id, verb, *n);
        }
    }

    /// Remaining balance of a counted verb: capacity minus the fold of spend
    /// rows. `None` when the verb is uncounted (unlimited).
    pub fn counts_left(&self, cap: &Capability, verb: &str) -> Option<u64> {
        cap.constraints
            .counts
            .get(verb)
            .map(|cap_n| cap_n.saturating_sub(self.ledger.spent(&cap.cap_id, verb)))
    }

    /// The one minting path (`mint` and `attenuate` converge here): the caps
    /// row, the (cap, verb) pool capacities and the `kernel/cap` holding (with
    /// the cap's expiry as its lease) commit in a single transaction.
    fn store_minted(&self, cap: &Capability) -> Result<(), KernelError> {
        let now = crate::db::now_unix();
        let cap = cap.clone();
        self.ledger.transaction(move |l, conn| {
            store_on(conn, &cap)?;
            for (verb, n) in &cap.constraints.counts {
                l.set_capacity(
                    CLASS_CAP_COUNT,
                    &pool_instance(&cap.cap_id, verb),
                    Frag::Count(Count::Value(*n)),
                );
            }
            if l.capacity(CLASS_CAP, &cap.cap_id).is_none() {
                l.set_capacity(CLASS_CAP, &cap.cap_id, Frag::Ex(Ex::Token));
            }
            let id = l
                .grant(
                    &cap.subject,
                    CLASS_CAP,
                    &cap.cap_id,
                    Frag::Ex(Ex::Token),
                    &cap.cap_id,
                    None,
                    now,
                )
                .map_err(map_err)?;
            if let Some(exp) = cap.constraints.expires_at {
                // An absolute lease: the sweeper expires the grant when the
                // constraint says so (no separate timer).
                l.set_lease(id, &cap.cap_id, Some(exp)).map_err(map_err)?;
            }
            let h = l.holding(id).expect("just granted");
            persist_on(conn, h)?;
            Ok(())
        })
    }

    pub fn get(&self, cap_id: &str) -> Result<Capability, KernelError> {
        let db = self.db.lock().unwrap();
        let json = db
            .query_row(
                "SELECT json FROM caps WHERE cap_id=?1",
                params![cap_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| KernelError::NotFound(cap_id.to_string()))?;
        serde_json::from_str(&json).map_err(|e| KernelError::Corrupt(format!("cap json: {e}")))
    }

    pub fn mint(
        &self,
        subject: &str,
        resource: &str,
        verbs: BTreeSet<String>,
        constraints: Constraints,
        parent: Option<String>,
    ) -> Result<Capability, KernelError> {
        let cap = Capability {
            cap_id: format!("cap_{}", rand_id()),
            subject: subject.to_string(),
            resource: resource.to_string(),
            verbs,
            constraints,
            parent,
            revoked: false,
        };
        self.store_minted(&cap)?;
        Ok(cap)
    }

    /// Attenuation: mint a child that must narrow the parent in every field.
    /// The parent must be alive — the holding decides (WP-03).
    pub fn attenuate(
        &self,
        parent_id: &str,
        subject: &str,
        verbs: BTreeSet<String>,
        constraints: Constraints,
    ) -> Result<Capability, KernelError> {
        let parent = self.get(parent_id)?;
        if self.ledger.cap_holding(parent_id).is_none() {
            return Err(KernelError::Denied("parent revoked".into()));
        }
        let child = Capability {
            cap_id: format!("cap_{}", rand_id()),
            subject: subject.to_string(),
            resource: parent.resource.clone(),
            verbs,
            constraints,
            parent: Some(parent.cap_id.clone()),
            revoked: false,
        };
        if !child.is_valid_attenuation_of(&parent) {
            return Err(KernelError::Denied("attenuation must narrow".into()));
        }
        self.store_minted(&child)?;
        Ok(child)
    }

    pub fn exercise(&self, cap_id: &str, verb: &str, now: u64) -> Result<(), KernelError> {
        let cap = self.get(cap_id)?;
        // The holding is the single source of truth for liveness (WP-03):
        // revoked, expired-and-swept, or died-with-its-holder — all read as
        // "no holding".
        if self.ledger.cap_holding(cap_id).is_none() {
            return Err(KernelError::Denied("cap revoked".into()));
        }
        // Belt for the sweeper's lag: the lease expires the holding on the
        // next tick, but the constraint itself refuses right now.
        if let Some(exp) = cap.constraints.expires_at {
            if now > exp {
                return Err(KernelError::Denied("cap expired".into()));
            }
        }
        if !cap.verbs.contains(verb) {
            return Err(KernelError::Denied(format!("verb not granted: {verb}")));
        }
        if cap.constraints.counts.contains_key(verb) {
            // F1: spending is minting one unit into the pool through the
            // issuer gate; exhaustion is the gate refusing, not a counter
            // hitting zero.
            match self.ledger.spend(&cap.subject, &cap.cap_id, verb, now) {
                Ok(_) => {}
                Err(KernelError::Denied(_)) => {
                    return Err(KernelError::Denied(format!("budget exhausted: {verb}")));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Find a live capability granting `verb` on `resource` to `subject` and
    /// exercise it (counting budgets are ledger pools). This is the
    /// authorization gate behind plugin→kernel `invoke` (ABI v2): the caller
    /// names a verb, not a cap id — the kernel resolves which grant covers it.
    /// Several candidate caps may exist; the first that exercises cleanly
    /// wins, so an exhausted budget falls through to a fresh grant.
    pub fn find_and_exercise(
        &self,
        subject: &str,
        resource: &str,
        verb: &str,
        now: u64,
    ) -> Result<String, KernelError> {
        let candidates: Vec<Capability> = {
            let db = self.db.lock().unwrap();
            let mut stmt = db.prepare("SELECT json FROM caps WHERE revoked=0")?;
            let rows: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()?;
            rows.iter()
                .filter_map(|j| serde_json::from_str::<Capability>(j).ok())
                .filter(|c| c.subject == subject && c.resource == resource && c.verbs.contains(verb))
                .collect()
        };
        // The holding decides liveness (WP-03) — a dead cap never shadows a
        // live one. Filtered outside the db lock (lock order: ledger → db).
        let candidates: Vec<Capability> = candidates
            .into_iter()
            .filter(|c| self.ledger.cap_holding(&c.cap_id).is_some())
            .collect();
        if candidates.is_empty() {
            return Err(KernelError::Denied(format!(
                "no capability: {subject} → {resource} verb {verb}"
            )));
        }
        let mut last = KernelError::Denied("no capability".into());
        for cap in candidates {
            match self.exercise(&cap.cap_id, verb, now) {
                Ok(()) => return Ok(cap.cap_id),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// All live capabilities held by `subject` — the raw material of grants
    /// introspection. "Live" = the `kernel/cap` holding is alive (WP-03: the
    /// holding, not the row flag, is the truth); the expiry column is a
    /// pre-filter for sweeper lag.
    pub fn list_live(&self, subject: &str, now: u64) -> Result<Vec<Capability>, KernelError> {
        let rows: Vec<String> = {
            let db = self.db.lock().unwrap();
            let mut stmt = db.prepare("SELECT json FROM caps WHERE revoked=0")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()?
        };
        Ok(rows
            .iter()
            .filter_map(|j| serde_json::from_str::<Capability>(j).ok())
            .filter(|c| {
                c.subject == subject
                    && c.constraints.expires_at.map(|e| now <= e).unwrap_or(true)
            })
            .filter(|c| self.ledger.cap_holding(&c.cap_id).is_some())
            .collect())
    }

    /// Revoke a capability and everything attenuated from it. The derivation
    /// tree (CDT) is walked on the caps table — it is its own graph, never
    /// encoded as ownership edges (plugin-system §3.3). Then, per cap, one
    /// atomic step: mark the caps row revoked, tear down the grant's holding
    /// subtree through `world` (children first — what exists because of the
    /// grant dies before it), release the cap's spend rows and close its
    /// pools (✓(● 0 · ◯ 0): the F1 invariant survives the closing). Each cap's
    /// step is idempotent, so re-revoking after a crash resumes where it
    /// stopped.
    pub fn revoke<W: World>(
        &self,
        cap_id: &str,
        world: &mut W,
        now: u64,
    ) -> Result<u64, KernelError> {
        let mut frontier = vec![cap_id.to_string()];
        let mut cascade: Vec<Capability> = Vec::new();
        while let Some(id) = frontier.pop() {
            let cap = self.get(&id)?;
            let kids: Vec<String> = {
                let db = self.db.lock().unwrap();
                let mut stmt = db.prepare("SELECT cap_id FROM caps WHERE parent=?1")?;
                stmt.query_map(params![id], |r| r.get::<_, String>(0))?
                    .collect::<Result<_, _>>()?
            };
            cascade.push(cap);
            frontier.extend(kids);
        }
        let mut n = 0u64;
        for cap in cascade {
            let holding = self.ledger.cap_holding(&cap.cap_id);
            let extra = |l: &mut Ledger, conn: &Connection| -> Result<(), KernelError> {
                let mut stored = cap.clone();
                stored.revoked = true;
                store_on(conn, &stored)?;
                // The account closes: live spend rows are released (their
                // tombstones keep the history; nothing is refunded into a
                // live pool — the pool dies in the next line), then every
                // pool's capacity goes to zero.
                let prefix = format!("{}/", cap.cap_id);
                let spends: Vec<(u64, String)> = l
                    .live()
                    .filter(|h| h.class_id == CLASS_CAP_COUNT && h.instance.starts_with(&prefix))
                    .map(|h| (h.id, h.generation.clone()))
                    .collect();
                for (sid, generation) in spends {
                    l.release(sid, &generation, now).map_err(map_err)?;
                    if let Some(h) = l.holding(sid) {
                        persist_on(conn, h)?;
                    }
                }
                for verb in cap.constraints.counts.keys() {
                    l.set_capacity(
                        CLASS_CAP_COUNT,
                        &pool_instance(&cap.cap_id, verb),
                        Frag::Count(Count::Value(0)),
                    );
                }
                Ok(())
            };
            match holding {
                Some(h) => {
                    self.ledger
                        .teardown_subtree(h.id, &cap.cap_id, world, now, extra)?;
                }
                // Already swept or revoked earlier: the bookkeeping still
                // commits, the teardown is an empty no-op.
                None => self.ledger.transaction(extra)?,
            }
            if !cap.revoked {
                n += 1;
            }
        }
        Ok(n)
    }
}

fn store_on(conn: &Connection, cap: &Capability) -> Result<(), KernelError> {
    conn.execute(
        "INSERT OR REPLACE INTO caps (cap_id, json, parent, revoked) VALUES (?1,?2,?3,?4)",
        params![
            cap.cap_id,
            serde_json::to_string(cap).unwrap(),
            cap.parent,
            cap.revoked as i64
        ],
    )?;
    Ok(())
}

fn rand_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::CLASS_SUBSCRIPTION;
    use portos_rm::teardown::MockWorld;
    use std::collections::BTreeMap;

    fn store(tag: &str) -> (CapStore, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("portos-caps-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = Arc::new(Mutex::new(crate::db::open(&root).unwrap()));
        let (ledger, _report) = LedgerStore::open(db.clone()).unwrap();
        (CapStore::new(db, Arc::new(ledger)), root)
    }

    fn verbs(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn counting_exercise_never_overdraws() {
        let (caps, root) = store("count");
        let mut counts = BTreeMap::new();
        counts.insert("emit".to_string(), 2u64);
        let cap = caps
            .mint(
                "session:test",
                "toy:echo",
                verbs(&["emit"]),
                Constraints {
                    expires_at: None,
                    counts,
                },
                None,
            )
            .unwrap();
        assert!(caps.exercise(&cap.cap_id, "emit", 0).is_ok());
        assert!(caps.exercise(&cap.cap_id, "emit", 0).is_ok());
        let e = caps.exercise(&cap.cap_id, "emit", 0);
        assert!(
            matches!(e, Err(KernelError::Denied(_))),
            "third emit must be denied"
        );
        // The declared capacity is untouched; the balance is a fold over
        // spend rows (F1 consequence 1).
        let stored = caps.get(&cap.cap_id).unwrap();
        assert_eq!(stored.constraints.counts["emit"], 2);
        assert_eq!(caps.counts_left(&stored, "emit"), Some(0));
        assert_eq!(caps.counts_left(&stored, "list"), None);
        // Minting granted the cap's holding (WP-03).
        assert!(caps.ledger.cap_holding(&cap.cap_id).is_some());
        caps.ledger.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn attenuation_must_narrow_and_revocation_cascades() {
        let (caps, root) = store("att");
        let parent = caps
            .mint(
                "session:test",
                "toy:echo",
                verbs(&["emit", "list"]),
                Constraints::default(),
                None,
            )
            .unwrap();
        // widening is rejected
        let bad = caps.attenuate(
            &parent.cap_id,
            "plugin:kid",
            verbs(&["emit", "delete"]),
            Constraints::default(),
        );
        assert!(bad.is_err());
        // narrowing is fine
        let kid = caps
            .attenuate(
                &parent.cap_id,
                "plugin:kid",
                verbs(&["emit"]),
                Constraints::default(),
            )
            .unwrap();
        assert!(caps.exercise(&kid.cap_id, "emit", 0).is_ok());
        // revoking the parent kills the child too
        let n = caps
            .revoke(&parent.cap_id, &mut MockWorld::default(), 1)
            .unwrap();
        assert_eq!(n, 2);
        assert!(caps.exercise(&kid.cap_id, "emit", 0).is_err());
        // Both holdings are tombstoned (the holding is the truth).
        assert!(
            caps.ledger
                .cap_holding(&parent.cap_id)
                .is_none()
        );
        assert!(caps.ledger.cap_holding(&kid.cap_id).is_none());
        // Attenuation from a dead parent is refused.
        assert!(
            caps.attenuate(&parent.cap_id, "plugin:kid", verbs(&["emit"]), Constraints::default())
                .is_err()
        );
        caps.ledger.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// WP-03: revocation tears down the grant's ownership subtree — what
    /// exists because of the grant dies children-first, then the cap holding;
    /// the caps row is marked, spend rows released, pools closed — all in one
    /// transaction per cap.
    #[test]
    fn revoking_a_grant_releases_its_holding_and_children_first() {
        let (caps, root) = store("revsub");
        let mut counts = BTreeMap::new();
        counts.insert("emit".to_string(), 2u64);
        let cap = caps
            .mint(
                "plugin:p",
                "driver:toy",
                verbs(&["emit"]),
                Constraints {
                    expires_at: None,
                    counts,
                },
                None,
            )
            .unwrap();
        caps.exercise(&cap.cap_id, "emit", 1).unwrap(); // one live spend row
        let cap_holding = caps.ledger.cap_holding(&cap.cap_id).unwrap().id;
        // Something that exists because of the grant: a holding parented
        // under the cap holding (WP-08's pools/routes will look like this).
        let child = caps
            .ledger
            .hold_exclusive("plugin:p", CLASS_SUBSCRIPTION, "dep-1", "dep", Some(cap_holding), 1)
            .unwrap();
        let mut world = MockWorld::default();
        let n = caps.revoke(&cap.cap_id, &mut world, 2).unwrap();
        assert_eq!(n, 1);
        // Children first, the cap holding last.
        let pos = |id: u64| world.action_order.iter().position(|x| *x == id).unwrap();
        assert!(pos(child) < pos(cap_holding), "child released before the cap holding");
        for id in [child, cap_holding] {
            assert!(
                caps.ledger.holding(id).unwrap().released_at.is_some(),
                "holding {id} tombstoned"
            );
        }
        // The account is closed: spend rows tombstoned, pool capacity zero,
        // the gate refuses — and the F1 invariant ✓(● 0 · ◯ 0) holds.
        assert_eq!(caps.ledger.spent(&cap.cap_id, "emit"), 0);
        assert!(caps.exercise(&cap.cap_id, "emit", 2).is_err());
        caps.ledger.invariant().unwrap();
        // Re-revoking is a no-op (idempotent resume).
        let n = caps.revoke(&cap.cap_id, &mut world, 3).unwrap();
        assert_eq!(n, 0);
        caps.ledger.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// WP-03: expiry rides the sweeper — the lease on the cap's holding
    /// expires it, no separate timer; and the constraint itself refuses even
    /// before the sweeper's tick.
    #[test]
    fn expired_capability_is_refused_by_the_gate_after_sweep() {
        let (caps, root) = store("exp");
        let cap = caps
            .mint(
                "plugin:p",
                "driver:toy",
                verbs(&["emit"]),
                Constraints {
                    expires_at: Some(100),
                    counts: BTreeMap::new(),
                },
                None,
            )
            .unwrap();
        let holding = caps.ledger.cap_holding(&cap.cap_id).unwrap();
        assert_eq!(holding.lease_expires_at, Some(100), "lease = the cap's expiry");
        assert!(caps.exercise(&cap.cap_id, "emit", 50).is_ok());
        // Sweeper lag: past expiry but not yet swept — the constraint refuses.
        assert!(caps.ledger.cap_holding(&cap.cap_id).is_some());
        assert!(caps.exercise(&cap.cap_id, "emit", 101).is_err());
        // After the sweep the holding is gone; the gate reads the holding.
        let mut world = MockWorld::default();
        let report = caps.ledger.sweep_with_world(&mut world, 101).unwrap();
        assert_eq!(report.released.len(), 1);
        assert!(caps.ledger.cap_holding(&cap.cap_id).is_none());
        assert!(caps.exercise(&cap.cap_id, "emit", 50).is_err(), "even at an earlier clock");
        caps.ledger.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance cross-check: caps and holdings never disagree after a
    /// reopen — live cap ⟺ live holding; revoked cap ⟺ tombstoned holding,
    /// pools not resurrected, budget where it was.
    #[test]
    fn caps_and_holdings_agree_after_reopen() {
        let root =
            std::env::temp_dir().join(format!("portos-caps-reopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = Arc::new(Mutex::new(crate::db::open(&root).unwrap()));
        let (live_id, dead_id) = {
            let (ledger, _r) = LedgerStore::open(db.clone()).unwrap();
            let caps = CapStore::new(db.clone(), Arc::new(ledger));
            let mut counts = BTreeMap::new();
            counts.insert("emit".to_string(), 2u64);
            let live = caps
                .mint(
                    "plugin:p",
                    "driver:toy",
                    verbs(&["emit"]),
                    Constraints { expires_at: None, counts: counts.clone() },
                    None,
                )
                .unwrap();
            let dead = caps
                .mint(
                    "plugin:p",
                    "driver:toy",
                    verbs(&["emit"]),
                    Constraints { expires_at: None, counts },
                    None,
                )
                .unwrap();
            caps.exercise(&live.cap_id, "emit", 1).unwrap();
            caps.revoke(&dead.cap_id, &mut MockWorld::default(), 2).unwrap();
            (live.cap_id, dead.cap_id)
        };
        // Reopen: the ledger reloads rows, the cap table re-declares pools.
        let (ledger, _r) = LedgerStore::open(db.clone()).unwrap();
        let ledger = Arc::new(ledger);
        let caps = CapStore::new(db.clone(), ledger.clone());
        caps.rebuild_pools().unwrap();
        ledger.invariant().unwrap();
        assert!(ledger.cap_holding(&live_id).is_some());
        assert!(ledger.cap_holding(&dead_id).is_none(), "revoked stays dead across reopen");
        // The live cap's budget survived (spend rows reloaded, pool redeclared).
        assert_eq!(caps.counts_left(&caps.get(&live_id).unwrap(), "emit"), Some(1));
        assert!(caps.exercise(&live_id, "emit", 3).is_ok());
        assert!(caps.exercise(&dead_id, "emit", 3).is_err());
        // list_live agrees with the holdings.
        let live: Vec<String> = caps
            .list_live("plugin:p", 3)
            .unwrap()
            .iter()
            .map(|c| c.cap_id.clone())
            .collect();
        assert_eq!(live, vec![live_id]);
        ledger.invariant().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
