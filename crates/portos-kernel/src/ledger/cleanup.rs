//! One durable retirement path: commit intent, claim, act without storage locks,
//! then commit a generation- and attempt-checked result.
use super::*;
use std::collections::BTreeSet;
use std::sync::OnceLock;

static WORKERS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
fn workers() -> &'static Mutex<BTreeSet<String>> {
    WORKERS.get_or_init(Mutex::default)
}
struct Worker(HostWitness);
impl Worker {
    fn new() -> Result<Self, KernelError> {
        let session = hex::encode(rand::random::<[u8; 16]>());
        let identity = HostWitness::new(
            substrate::capture_process(std::process::id())?,
            session.clone().into(),
        )
        .map_err(map_err)?;
        workers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session);
        Ok(Self(identity))
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        workers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.0.session().as_str());
    }
}

#[derive(Clone, Debug, Default)]
pub struct CleanupReport {
    pub completed: Vec<HoldingHandle>,
    /// Includes waits for children, running attempts, and failed/unknown results.
    pub pending: Vec<CleanupRecord>,
}
impl LedgerTxn<'_> {
    pub fn request_retirement(
        &mut self,
        handle: &HoldingHandle,
        now: Timestamp,
    ) -> Result<HoldingState, KernelError> {
        let realm: String = self.sql.query_row(
            "SELECT realm FROM resource_schema WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        let key =
            CleanupKey::new(format!("cleanup:{realm}:{}", handle.id().get())).map_err(map_err)?;
        self.ledger
            .request_retirement(handle, key, now)
            .map_err(map_err)
    }
    fn request_many(
        &mut self,
        handles: impl IntoIterator<Item = HoldingHandle>,
        now: Timestamp,
    ) -> Result<(), KernelError> {
        for handle in handles {
            self.request_retirement(&handle, now)?;
        }
        Ok(())
    }
}

impl LedgerStore {
    pub fn cleanup_tasks(&self) -> Result<Vec<CleanupRecord>, KernelError> {
        self.with_read(|l, _| Ok(l.cleanup_tasks().map(|t| (**t).clone()).collect()))
    }
    pub fn occupying_snapshot(&self, subject: &SubjectId) -> Result<Vec<LiveItem>, KernelError> {
        self.with_read(|l, _| Ok(l.occupying_closure(subject)))
    }
    pub fn request_retirement(
        &self,
        handle: &HoldingHandle,
        now: Timestamp,
    ) -> Result<HoldingState, KernelError> {
        self.transaction(|tx| tx.request_retirement(handle, now))
    }
    /// Retry all durable work, including obligations left by earlier calls.
    pub fn retry_cleanup<W: CleanupExecutor>(
        &self,
        world: &mut W,
        now: Timestamp,
    ) -> Result<CleanupReport, KernelError> {
        self.run_cleanup(world, None, now)
    }
    fn recover_workers(&self, now: Timestamp) -> Result<(), KernelError> {
        let current = substrate::capture_process(std::process::id())?;
        for record in self.cleanup_tasks()? {
            let CleanupState::Running { worker } = &record.state else {
                continue;
            };
            let gone = if worker.process() == &current {
                !workers()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(worker.session().as_str())
            } else {
                substrate::process_present(worker.process()).is_ok_and(|p| !p)
            };
            if gone {
                self.transaction(|tx| tx.ledger.abandon_cleanup(&record, now).map_err(map_err))?;
            }
        }
        Ok(())
    }
    fn run_cleanup<W: CleanupExecutor>(
        &self,
        world: &mut W,
        scope: Option<&BTreeSet<HoldingId>>,
        now: Timestamp,
    ) -> Result<CleanupReport, KernelError> {
        self.recover_workers(now)?;
        let worker = Worker::new()?;
        let mut attempted = BTreeSet::new();
        let mut report = CleanupReport::default();
        loop {
            let mut progress = false;
            // A child completed by a concurrent runner must wake an already
            // retiring ancestor, even when that ancestor belongs to another
            // original request. Its intent is already durable.
            let permitted = scope
                .map(|scope| {
                    self.with_read(|l, _| {
                        let mut ids = scope.clone();
                        for id in scope {
                            let mut parent = l.holding(*id).and_then(|h| h.parent);
                            while let Some(p) = parent {
                                if let Some(h) = l.holding(p) {
                                    if matches!(h.state, HoldingState::Retiring(_)) {
                                        ids.insert(p);
                                    }
                                    parent = h.parent;
                                } else {
                                    break;
                                }
                            }
                        }
                        Ok(ids)
                    })
                })
                .transpose()?;
            let tasks = self.cleanup_tasks()?;
            for task in tasks.iter().filter(|t| {
                !t.state.is_done()
                    && permitted
                        .as_ref()
                        .is_none_or(|ids| ids.contains(&t.holding.id()))
            }) {
                if attempted.contains(&task.id) {
                    continue;
                }
                let work = self.transaction(|tx| {
                    tx.ledger
                        .claim_cleanup(task.id, worker.0.clone(), now)
                        .map_err(map_err)
                })?;
                let Some(work) = work else { continue };
                attempted.insert(task.id);
                // Both the intent and this attempt are committed. Reentrant reads
                // and independent ledger transactions from the executor are safe.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match work
                    .target()
                {
                    CleanupTarget::AccountingOnly => CleanupOutcome::Confirmed,
                    CleanupTarget::Unresolved { reason, .. } => {
                        CleanupOutcome::Blocked(reason.clone())
                    }
                    _ => world.execute(&work),
                }));
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(panic) => {
                        let _ = self.transaction(|tx| {
                            tx.ledger
                                .finish_cleanup(
                                    &work,
                                    CleanupOutcome::Unknown("cleanup executor unwound".into()),
                                    now,
                                )
                                .map_err(map_err)
                        });
                        std::panic::resume_unwind(panic)
                    }
                };
                let complete = matches!(
                    outcome,
                    CleanupOutcome::Confirmed | CleanupOutcome::AlreadyAbsent
                );
                self.transaction(|tx| {
                    tx.ledger
                        .finish_cleanup(&work, outcome, now)
                        .map_err(map_err)
                })?;
                if complete {
                    report.completed.push(work.task().holding.clone());
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }
        report.pending = self
            .cleanup_tasks()?
            .into_iter()
            .filter(|t| !t.state.is_done() && scope.is_none_or(|ids| ids.contains(&t.holding.id())))
            .collect();
        Ok(report)
    }
    pub fn release_with_world<W: CleanupExecutor>(
        &self,
        handle: &HoldingHandle,
        world: &mut W,
        now: Timestamp,
    ) -> Result<CleanupReport, KernelError> {
        self.request_retirement(handle, now)?;
        let ids = self.with_read(|l, _| {
            let mut ids = BTreeSet::from([handle.id()]);
            ids.extend(
                l.occupying_subtree(handle.id())
                    .into_iter()
                    .filter(|h| l.holding(h.id).is_some_and(|h| !h.state.is_active()))
                    .map(|h| h.id),
            );
            Ok(ids)
        })?;
        self.run_cleanup(world, Some(&ids), now)
    }
    pub fn sweep_with_world<W: CleanupExecutor>(
        &self,
        world: &mut W,
        now: Timestamp,
    ) -> Result<SweepReport, KernelError> {
        self.transaction(|tx| tx.request_many(tx.ledger.due_retirements(now), now))?;
        let result = self.retry_cleanup(world, now)?;
        let mut report = SweepReport::default();
        for handle in result.completed {
            let h = self
                .holding(handle.id())?
                .ok_or_else(|| corrupt("completed holding disappeared"))?;
            report
                .released
                .push((h.id.get(), h.class_id.to_string(), h.instance.to_string()));
        }
        report.pending = result.pending;
        Ok(report)
    }
    pub fn teardown<W: CleanupExecutor>(
        &self,
        subject: &SubjectId,
        world: &mut W,
        now: Timestamp,
    ) -> Result<CleanupReport, KernelError> {
        let ids = self.request_teardown(subject, now)?;
        let report = self.run_cleanup(world, Some(&ids), now)?;
        Ok(report)
    }
    pub(crate) fn request_teardown(
        &self,
        subject: &SubjectId,
        now: Timestamp,
    ) -> Result<BTreeSet<HoldingId>, KernelError> {
        self.transaction(|tx| {
            let items = tx.ledger.occupying_closure(subject);
            let ids = items.iter().map(|h| h.id).collect::<BTreeSet<_>>();
            tx.request_many(items.into_iter().map(|h| h.handle()), now)?;
            Ok(ids)
        })
    }
    pub fn teardown_holding<W: CleanupExecutor>(
        &self,
        root: &HoldingHandle,
        world: &mut W,
        now: Timestamp,
    ) -> Result<CleanupReport, KernelError> {
        let ids = self.transaction(|tx| {
            tx.ledger.resolve(root).map_err(map_err)?;
            let items = tx.ledger.occupying_subtree(root.id());
            let ids = items.iter().map(|h| h.id).collect::<BTreeSet<_>>();
            tx.request_many(items.into_iter().map(|h| h.handle()), now)?;
            Ok(ids)
        })?;
        let report = self.run_cleanup(world, Some(&ids), now)?;
        Ok(report)
    }
    /// Host exit includes independently granted holdings owned by the plugin.
    /// The incarnation guard prevents a delayed old EOF from collecting a new
    /// plugin's grants under the same subject name.
    pub(crate) fn teardown_owner_incarnation<W: CleanupExecutor>(
        &self,
        root: &HoldingHandle,
        world: &mut W,
        now: Timestamp,
    ) -> Result<CleanupReport, KernelError> {
        let ids = self.transaction(|tx| {
            let h = tx.ledger.resolve(root).map_err(map_err)?;
            let items = if h.state.occupies() {
                tx.ledger.occupying_closure(&h.subject)
            } else {
                Vec::new()
            };
            let ids = items.iter().map(|h| h.id).collect::<BTreeSet<_>>();
            tx.request_many(items.into_iter().map(|h| h.handle()), now)?;
            Ok(ids)
        })?;
        let report = self.run_cleanup(world, Some(&ids), now)?;
        Ok(report)
    }
    pub(crate) fn retire_capability<W: CleanupExecutor>(
        &self,
        cap: &Capability,
        world: &mut W,
        now: Timestamp,
    ) -> Result<(), KernelError> {
        let ids = self.transaction(|tx| {
            let root = tx
                .ledger
                .occupying()
                .find(|h| h.class_id.as_str() == CLASS_CAP && h.instance.as_str() == cap.cap_id)
                .map(|h| h.id);
            let items = root
                .map(|id| tx.ledger.occupying_subtree(id))
                .unwrap_or_default();
            let ids = items.iter().map(|h| h.id).collect::<BTreeSet<_>>();
            // Count settlement is accounting-only and remains in this transaction.
            let keys = tx
                .sql
                .prepare("SELECT class_id,instance FROM resource_accounts WHERE account_id=?1")?
                .query_map(params![cap.cap_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (c, i) in keys {
                let pool = tx.pool::<Count>(&ResourceKey::new(c.into(), i.into()))?;
                tx.settle_and_zero_pool(&pool, now)?;
            }
            tx.request_many(items.into_iter().map(|h| h.handle()), now)?;
            let mut cap = cap.clone();
            cap.revoked = true;
            store_capability(tx.sql, &cap)?;
            Ok(ids)
        })?;
        self.run_cleanup(world, Some(&ids), now)?;
        Ok(())
    }
    pub(super) fn reconcile_on_open(&self) -> Result<OpenReport, KernelError> {
        let now = Timestamp::try_from(crate::db::now_unix()).map_err(map_err)?;
        let before = self.with_read(|l, _| {
            Ok(l.occupying()
                .filter(|h| {
                    HOLDABLE_CLASSES.contains(&h.class_id.as_str())
                        || [CLASS_PLUGIN, CLASS_SUBSCRIPTION].contains(&h.class_id.as_str())
                })
                .cloned()
                .collect::<Vec<_>>())
        })?;
        self.transaction(|tx| {
            tx.request_many(before.iter().map(Holding::handle), now)?;
            tx.request_many(tx.ledger.due_retirements(now), now)
        })?;
        let report = self.retry_cleanup(&mut substrate::BootstrapWorld, now)?;
        let completed = report
            .completed
            .iter()
            .map(|h| h.id())
            .collect::<BTreeSet<_>>();
        let mut substrate = SubstrateReconcile::default();
        let mut stale_rows = 0;
        for h in before {
            if completed.contains(&h.id) {
                match h.class_id.as_str() {
                    CLASS_PROCESS => {
                        if self.cleanup_tasks()?.iter().any(|t| {
                            t.holding.id() == h.id
                                && t.state == CleanupState::Done(Completion::Confirmed)
                        }) {
                            substrate.process_killed += 1;
                        } else {
                            substrate.process_tombstoned += 1;
                        }
                    }
                    CLASS_FILE_LOCK => substrate.locks_removed += 1,
                    CLASS_PORT => substrate.ports_tombstoned += 1,
                    CLASS_PLUGIN | CLASS_SUBSCRIPTION => stale_rows += 1,
                    _ => {}
                }
            } else {
                match h.class_id.as_str() {
                    CLASS_FILE_LOCK => substrate.locks_kept += 1,
                    CLASS_PORT => substrate.ports_still_bound += 1,
                    _ => {}
                }
            }
        }
        Ok(OpenReport {
            stale_rows,
            substrate,
            cleanup_pending: report.pending.len(),
        })
    }
}
