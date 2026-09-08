//! Resource storage: validated startup, staged transactions, durable pools.
//! Legacy physical cleanup remains synchronous until milestone M2.
use crate::KernelError;
use portos_proto::Capability;
use portos_rm::identity::{
    AccountId, ClassId, EffectClass, Generation, HoldingHandle, HoldingId, InstanceId, ResourceKey,
    SpendRequest, SubjectId,
};
use portos_rm::ledger::{
    AlgebraTag, ClassDecl, Frag, GrantRequest, Holding, Ledger, LedgerBuilder, LedgerError,
    LiveItem, RevertGrade,
};
use portos_rm::ra::{Count, Ex};
use portos_rm::registry::{
    Capacity, Claim, ClassBinding, PoolRef, RegisteredClass, RuntimeAlgebra,
};
use portos_rm::teardown::{RunOutcome, World};
use portos_rm::time::{Lease, LeaseRequest, Timestamp};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::Value;
use std::sync::{Arc, Mutex};
mod codec;
mod legacy_cleanup;
mod recovery;
use codec::{corrupt, persist};
#[cfg(test)]
mod tests;

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

struct StoreState {
    ledger: Ledger,
    writable: bool,
    revision: i64,
}

pub struct LedgerStore {
    inner: Mutex<StoreState>,
    db: Arc<Mutex<Connection>>,
}

/// Fields, connection and mutable aggregate stay inside the storage module.
/// Each transaction publishes a new view; pool references are scoped to that view.
pub struct LedgerTxn<'a> {
    ledger: Ledger,
    sql: &'a Transaction<'a>,
}

#[derive(Clone, Debug)]
pub struct ExclusiveRequest {
    pub owner: SubjectId,
    pub resource: ResourceKey,
    pub generation: Generation,
    pub parent: Option<HoldingHandle>,
    pub lease: LeaseRequest,
}

impl LedgerStore {
    pub(crate) fn open(db: Arc<Mutex<Connection>>) -> Result<(Self, OpenReport), KernelError> {
        let (ledger, revision) = {
            let mut conn = db.lock().map_err(|_| corrupt("database lock poisoned"))?;
            let sql = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let ledger = recovery::load(&sql)?;
            let revision: i64 = sql.query_row(
                "SELECT revision FROM resource_schema WHERE singleton=1",
                [],
                |r| r.get(0),
            )?;
            if revision < 0 {
                return Err(corrupt("negative ledger revision"));
            }
            sql.commit()?;
            (ledger, revision)
        };
        let store = Self {
            inner: Mutex::new(StoreState {
                ledger,
                writable: true,
                revision,
            }),
            db,
        };
        // All classes, pools and rows are validated before this limited bootstrap
        // coordinator runs. No operational store escapes until reconciliation ends.
        let substrate = store.reconcile_substrate()?;
        let stale_rows = store.reconcile_stale_process_rows()?;
        let journal_pending = {
            let conn = store
                .db
                .lock()
                .map_err(|_| corrupt("database lock poisoned"))?;
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM journal WHERE state != 'done'",
                [],
                |r| r.get(0),
            )?;
            usize::try_from(n).map_err(|_| corrupt("journal count out of range"))?
        };
        Ok((
            store,
            OpenReport {
                substrate,
                stale_rows,
                journal_pending,
            },
        ))
    }

    /// SQL is committed before the staged aggregate is published. Unwind or a
    /// normal refusal rolls back both. Failed rollback/commit requires reopening.
    pub fn transaction<T>(
        &self,
        f: impl FnOnce(&mut LedgerTxn<'_>) -> Result<T, KernelError>,
    ) -> Result<T, KernelError> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| corrupt("ledger lock poisoned; reopen required"))?;
        if !state.writable {
            return Err(corrupt("ledger requires recovery before further writes"));
        }
        let mut conn = self
            .db
            .lock()
            .map_err(|_| corrupt("database lock poisoned; reopen required"))?;
        let sql = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision: i64 = sql.query_row(
            "SELECT revision FROM resource_schema WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if revision != state.revision {
            state.writable = false;
            return Err(corrupt(
                "ledger changed through another store; reopen required",
            ));
        }
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| corrupt("ledger revision exhausted"))?;
        let mut tx = LedgerTxn {
            ledger: state.ledger.clone(),
            sql: &sql,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let value = f(&mut tx)?;
            tx.ledger.invariant().map_err(map_err)?;
            recovery::validate_auxiliary(tx.sql, &tx.ledger)?;
            persist(tx.sql, &tx.ledger.snapshot())?;
            tx.sql.execute(
                "UPDATE resource_schema SET revision=?1 WHERE singleton=1",
                params![next_revision],
            )?;
            Ok::<_, KernelError>(value)
        }));
        let staged = tx.ledger;
        match result {
            Ok(Ok(value)) => {
                if let Err(e) = sql.commit() {
                    state.writable = false;
                    return Err(e.into());
                }
                state.ledger = staged;
                state.revision = next_revision;
                Ok(value)
            }
            Ok(Err(e)) => {
                if sql.rollback().is_err() {
                    state.writable = false;
                }
                Err(e)
            }
            Err(panic) => {
                if sql.rollback().is_err() {
                    state.writable = false;
                }
                drop(conn);
                drop(state);
                std::panic::resume_unwind(panic)
            }
        }
    }

    pub fn spend_many(
        &self,
        subject: &SubjectId,
        spends: &[SpendRequest],
        now: Timestamp,
    ) -> Result<Vec<HoldingHandle>, KernelError> {
        self.transaction(|tx| {
            let mut handles = Vec::new();
            for spend in spends {
                if let Some(handle) = tx.spend(subject, spend, now)? {
                    handles.push(handle);
                }
            }
            Ok(handles)
        })
    }
    fn with_read<T>(
        &self,
        read: impl FnOnce(&Ledger, &Connection) -> Result<T, KernelError>,
    ) -> Result<T, KernelError> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| corrupt("ledger lock poisoned"))?;
        if !state.writable {
            return Err(corrupt(
                "ledger requires recovery; current state is unknown",
            ));
        }
        let conn = self
            .db
            .lock()
            .map_err(|_| corrupt("database lock poisoned"))?;
        let revision: i64 = conn.query_row(
            "SELECT revision FROM resource_schema WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if revision != state.revision {
            state.writable = false;
            return Err(corrupt("ledger view is stale; reopen required"));
        }
        read(&state.ledger, &conn)
    }
    pub fn spent(&self, account: &AccountId, effect: &EffectClass) -> Result<u64, KernelError> {
        self.with_read(|ledger, conn| {
            let Some(key) = account_key(conn, account, effect)? else {
                return Ok(0);
            };
            Ok(spent_in(ledger, &key))
        })
    }
    pub fn count_balance(
        &self,
        account: &AccountId,
        effect: &EffectClass,
    ) -> Result<Option<u64>, KernelError> {
        self.with_read(|ledger, conn| {
            let Some(key) = account_key(conn, account, effect)? else {
                return Ok(None);
            };
            match ledger.capacity(&key) {
                Some(Frag::Count(Count::Value(cap))) => Ok(Some(
                    cap.checked_sub(spent_in(ledger, &key))
                        .ok_or_else(|| corrupt("overdrawn pool"))?,
                )),
                _ => Err(corrupt("account has no counted pool")),
            }
        })
    }
    pub fn hold_exclusive(
        &self,
        request: ExclusiveRequest,
        now: Timestamp,
    ) -> Result<HoldingHandle, KernelError> {
        self.transaction(|tx| tx.hold_exclusive(request, now))
    }
    pub fn hold_exclusive_child(
        &self,
        request: ExclusiveRequest,
        parent_subject: SubjectId,
        now: Timestamp,
    ) -> Result<HoldingHandle, KernelError> {
        self.transaction(|tx| {
            tx.ledger
                .declare_instantiation(request.owner.clone(), parent_subject);
            tx.hold_exclusive(request, now)
        })
    }
    pub fn hold_substrate(
        &self,
        request: ExclusiveRequest,
        substrate: &Value,
        now: Timestamp,
    ) -> Result<HoldingHandle, KernelError> {
        self.transaction(|tx| tx.hold_substrate(request, substrate, now))
    }
    pub fn release(&self, handle: &HoldingHandle, now: Timestamp) -> Result<(), KernelError> {
        self.transaction(|tx| tx.release(handle, now))
    }
    pub fn renew(
        &self,
        handle: &HoldingHandle,
        lease: LeaseRequest,
        now: Timestamp,
    ) -> Result<Lease, KernelError> {
        self.transaction(|tx| tx.renew(handle, lease, now))
    }
    pub fn transfer_all(&self, from: &SubjectId, to: &SubjectId) -> Result<usize, KernelError> {
        self.transaction(|tx| {
            let handles: Vec<_> = tx
                .ledger
                .live_snapshot(from)
                .into_iter()
                .map(|h| h.handle())
                .collect();
            for h in &handles {
                tx.ledger.transfer(h, from, to.clone()).map_err(map_err)?;
            }
            Ok(handles.len())
        })
    }
    pub fn live_snapshot(&self, subject: &SubjectId) -> Result<Vec<LiveItem>, KernelError> {
        self.with_read(|l, _| Ok(l.live_snapshot(subject)))
    }
    pub fn live_closure(&self, subject: &SubjectId) -> Result<Vec<LiveItem>, KernelError> {
        self.with_read(|l, _| Ok(l.live_closure(subject)))
    }
    pub fn holding(&self, id: HoldingId) -> Result<Option<Holding>, KernelError> {
        self.with_read(|l, _| Ok(l.holding(id).cloned()))
    }
    pub fn cap_holding(&self, account: &AccountId) -> Result<Option<Holding>, KernelError> {
        self.with_read(|l, _| {
            Ok(l.live()
                .find(|h| {
                    h.class_id.as_str() == CLASS_CAP && h.instance.as_str() == account.as_str()
                })
                .cloned())
        })
    }
    pub fn invariant(&self) -> Result<(), KernelError> {
        self.with_read(|l, _| l.invariant().map_err(map_err))
    }
    pub fn counts(&self, class: &ClassId) -> Result<(usize, usize), KernelError> {
        self.with_read(|l, _| {
            let live = l.live().filter(|h| &h.class_id == class).count();
            let total = l.holdings().iter().filter(|h| &h.class_id == class).count();
            Ok((live, total - live))
        })
    }
}

impl LedgerTxn<'_> {
    pub fn register_class(&mut self, declaration: ClassDecl) -> Result<ClassBinding, KernelError> {
        self.ledger.register_class(declaration).map_err(map_err)
    }
    pub fn registered_class<A: RuntimeAlgebra>(
        &self,
        id: &ClassId,
    ) -> Result<RegisteredClass<A>, KernelError> {
        self.ledger.registered_class(id).map_err(map_err)
    }
    pub fn create_pool<A: RuntimeAlgebra>(
        &mut self,
        class: &RegisteredClass<A>,
        instance: InstanceId,
        capacity: Capacity<A>,
    ) -> Result<PoolRef<A>, KernelError> {
        self.ledger
            .create_pool(class, instance, capacity)
            .map_err(map_err)
    }
    pub fn pool<A: RuntimeAlgebra>(&self, key: &ResourceKey) -> Result<PoolRef<A>, KernelError> {
        self.ledger.pool(key).map_err(map_err)
    }
    pub fn resize_pool<A: RuntimeAlgebra>(
        &mut self,
        pool: &PoolRef<A>,
        capacity: Capacity<A>,
    ) -> Result<(), KernelError> {
        self.ledger.resize_pool(pool, capacity).map_err(map_err)
    }
    pub fn grant<A: RuntimeAlgebra>(
        &mut self,
        pool: &PoolRef<A>,
        request: GrantRequest<A>,
    ) -> Result<HoldingHandle, KernelError> {
        self.ledger.grant(pool, request).map_err(map_err)
    }
    pub fn release(&mut self, handle: &HoldingHandle, now: Timestamp) -> Result<(), KernelError> {
        self.ledger.release(handle, now).map_err(map_err)
    }
    pub fn renew(
        &mut self,
        handle: &HoldingHandle,
        lease: LeaseRequest,
        now: Timestamp,
    ) -> Result<Lease, KernelError> {
        self.ledger.renew(handle, lease, now).map_err(map_err)
    }
    pub fn holding(&self, id: HoldingId) -> Option<&Holding> {
        self.ledger.holding(id)
    }
    pub fn settle_and_zero_pool(
        &mut self,
        pool: &PoolRef<Count>,
        now: Timestamp,
    ) -> Result<(), KernelError> {
        self.ledger.settle_and_zero_pool(pool, now).map_err(map_err)
    }
    pub fn create_count_pool(
        &mut self,
        account: &AccountId,
        effect: &EffectClass,
        owner: &SubjectId,
        capacity: Capacity<Count>,
    ) -> Result<PoolRef<Count>, KernelError> {
        let key = ResourceKey::new(
            ClassId::new(CLASS_CAP_COUNT),
            InstanceId::new(format!("{account}/{effect}")),
        );
        let existing: Option<(String,String,String)> = self.sql.query_row("SELECT owner,class_id,instance FROM resource_accounts WHERE account_id=?1 AND effect_class=?2", params![account.as_str(), effect.as_str()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if existing.as_ref().is_some_and(|(o, c, i)| {
            o != owner.as_str() || c != key.class().as_str() || i != key.instance().as_str()
        }) {
            return Err(corrupt("account binding changed"));
        }
        let class = self.registered_class::<Count>(key.class())?;
        if self
            .ledger
            .capacity(&key)
            .is_some_and(|cap| Count::from_fragment(cap) != Some(capacity.value()))
        {
            return Err(map_err(LedgerError::PoolExists));
        }
        // Check collisions before touching the staged aggregate. The association,
        // rather than a display prefix, decides which pool belongs to this account.
        self.sql.execute("INSERT INTO resource_accounts(account_id,effect_class,owner,class_id,instance) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(account_id,effect_class) DO NOTHING", params![account.as_str(), effect.as_str(), owner.as_str(), key.class().as_str(), key.instance().as_str()])?;
        self.create_pool(&class, key.instance().clone(), capacity)
    }
    pub fn count_pool(
        &self,
        account: &AccountId,
        effect: &EffectClass,
    ) -> Result<PoolRef<Count>, KernelError> {
        let key = account_key(self.sql, account, effect)?
            .ok_or_else(|| KernelError::Denied("unknown count account".into()))?;
        self.pool(&key)
    }
    pub fn spend(
        &mut self,
        subject: &SubjectId,
        request: &SpendRequest,
        now: Timestamp,
    ) -> Result<Option<HoldingHandle>, KernelError> {
        if request.amount == 0 {
            return Ok(None);
        }
        let pool = self.count_pool(&request.account, &request.effect)?;
        self.grant(
            &pool,
            GrantRequest {
                owner: SubjectId::new(format!("{subject}:spent")),
                claim: Claim::new(Count::Value(request.amount)).map_err(map_err)?,
                generation: Generation::new(SPEND_GENERATION),
                parent: None,
                lease: LeaseRequest::Unbounded,
                now,
            },
        )
        .map(Some)
    }
    fn hold_exclusive(
        &mut self,
        request: ExclusiveRequest,
        now: Timestamp,
    ) -> Result<HoldingHandle, KernelError> {
        let class = self.registered_class::<Ex>(request.resource.class())?;
        let pool = self.create_pool(
            &class,
            request.resource.instance().clone(),
            Capacity::new(Ex::Token).map_err(map_err)?,
        )?;
        self.grant(
            &pool,
            GrantRequest {
                owner: request.owner,
                claim: Claim::new(Ex::Token).map_err(map_err)?,
                generation: request.generation,
                parent: request.parent,
                lease: request.lease,
                now,
            },
        )
    }
    fn hold_substrate(
        &mut self,
        request: ExclusiveRequest,
        substrate: &Value,
        now: Timestamp,
    ) -> Result<HoldingHandle, KernelError> {
        let class = request.resource.class().as_str().to_string();
        if !HOLDABLE_CLASSES.contains(&class.as_str()) {
            return Err(KernelError::Denied(format!("class not holdable: {class}")));
        }
        legacy_cleanup::validate_substrate(&class, substrate)?;
        let handle = self.hold_exclusive(request, now)?;
        self.sql.execute(
            "INSERT INTO substrate(holding_id,kind,detail) VALUES(?1,?2,?3)",
            params![
                handle.id().to_sql(),
                class.strip_prefix("kernel/").unwrap(),
                substrate.to_string()
            ],
        )?;
        Ok(handle)
    }
    /// Capability table bookkeeping and resource registration have one owner.
    pub(crate) fn register_capability(
        &mut self,
        cap: &Capability,
        now: Timestamp,
    ) -> Result<(), KernelError> {
        for (effect, amount) in &cap.constraints.counts {
            self.create_count_pool(
                &AccountId::new(&cap.cap_id),
                &EffectClass::new(effect),
                &SubjectId::new(&cap.subject),
                Capacity::new(Count::Value(*amount)).map_err(map_err)?,
            )?;
        }
        let lease = cap
            .constraints
            .expires_at
            .map(Timestamp::try_from)
            .transpose()
            .map_err(map_err)?
            .map(LeaseRequest::Until)
            .unwrap_or(LeaseRequest::Unbounded);
        self.hold_exclusive(
            ExclusiveRequest {
                owner: SubjectId::new(&cap.subject),
                resource: ResourceKey::new(ClassId::new(CLASS_CAP), InstanceId::new(&cap.cap_id)),
                generation: Generation::new(&cap.cap_id),
                parent: None,
                lease,
            },
            now,
        )?;
        store_capability(self.sql, cap)
    }
}

fn account_key(
    conn: &Connection,
    account: &AccountId,
    effect: &EffectClass,
) -> Result<Option<ResourceKey>, KernelError> {
    let raw: Option<(String, String)> = conn.query_row("SELECT class_id,instance FROM resource_accounts WHERE account_id=?1 AND effect_class=?2", params![account.as_str(), effect.as_str()], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    Ok(raw.map(|(c, i)| ResourceKey::new(c.into(), i.into())))
}
fn spent_in(ledger: &Ledger, key: &ResourceKey) -> u64 {
    ledger
        .live()
        .filter(|h| h.key() == *key)
        .map(|h| match h.frag {
            Frag::Count(Count::Value(n)) => n,
            _ => unreachable!("count pool checked on creation"),
        })
        .sum()
}
fn store_capability(conn: &Connection, cap: &Capability) -> Result<(), KernelError> {
    conn.execute("INSERT INTO caps(cap_id,json,parent,revoked) VALUES(?1,?2,?3,?4) ON CONFLICT(cap_id) DO UPDATE SET json=excluded.json,parent=excluded.parent,revoked=excluded.revoked", params![cap.cap_id, serde_json::to_string(cap).map_err(|e| corrupt(e.to_string()))?, cap.parent, i64::from(cap.revoked)])?;
    Ok(())
}
pub(crate) fn map_err(e: LedgerError) -> KernelError {
    KernelError::Denied(format!("ledger: {e:?}"))
}
pub(crate) use legacy_cleanup::{kill_pid, proc_alive, proc_start_time};

/// ```compile_fail
/// use portos_kernel::ledger::LedgerStore;
/// fn raw_transaction(store: &LedgerStore) {
///     store.transaction(|ledger, connection| Ok(()));
/// }
/// ```
/// ```compile_fail
/// use portos_kernel::ledger::LedgerTxn;
/// fn raw_ledger(tx: &mut LedgerTxn<'_>) { let _ = &mut tx.ledger; }
/// ```
/// ```compile_fail
/// use portos_kernel::ledger::LedgerTxn;
/// fn raw_connection(tx: &LedgerTxn<'_>) { let _ = tx.sql; }
/// ```
const _: () = ();

/// Bootstrap connections are not part of the operational kernel interface.
/// ```compile_fail
/// use portos_kernel::Kernel;
/// fn connection(kernel: &Kernel) { let _ = &kernel.db; }
/// ```
/// ```compile_fail
/// use portos_kernel::ledger::LedgerStore;
/// let bootstrap = LedgerStore::open;
/// ```
const _: () = ();
