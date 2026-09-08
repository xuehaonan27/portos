use super::*;

impl Ledger {
    pub fn cleanup_policy(&self, class: &ClassId) -> Result<CleanupPolicy, LedgerError> {
        self.classes
            .get(class)
            .map(|c| c.cleanup)
            .ok_or(LedgerError::UnknownClass)
    }
    pub fn cleanup_tasks(&self) -> impl Iterator<Item = &CleanupTask> {
        self.cleanups.values()
    }
    pub fn cleanup_task(&self, id: CleanupId) -> Option<&CleanupTask> {
        self.cleanups.get(&id)
    }

    /// Repeated requests keep the original key and never start a second obligation.
    /// Children with independent leases remain usable until their own retirement.
    pub fn request_retirement(
        &mut self,
        handle: &HoldingHandle,
        key: CleanupKey,
        now: Timestamp,
    ) -> Result<HoldingState, LedgerError> {
        let root = self.resolve(handle)?;
        if !root.state.is_active() {
            return Ok(root.state);
        }
        let mut selected = vec![(handle.clone(), key.clone())];
        let mut i = 0;
        while i < selected.len() {
            let parent = selected[i].0.id();
            selected.extend(
                self.active()
                    .filter(|h| h.parent == Some(parent) && h.lease == Lease::ParentBound)
                    .map(|h| {
                        (
                            h.handle(),
                            CleanupKey::new(format!("{}:bound:{}", key.as_str(), h.id.get()))
                                .expect("derived nonempty key"),
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            i += 1;
        }
        // Validate the complete batch before changing any state.
        let mut keys = self
            .cleanups
            .values()
            .map(|t| t.key.clone())
            .collect::<BTreeSet<_>>();
        for (_, key) in &selected {
            if !keys.insert(key.clone()) {
                return Err(LedgerError::InvalidCleanup);
            }
        }
        for (handle, key) in selected {
            let h = self
                .holdings
                .iter_mut()
                .find(|h| h.id == handle.id())
                .unwrap();
            let id = CleanupId::for_holding(h.id);
            let state = match &h.target {
                CleanupTarget::Unresolved { reason, .. } => CleanupState::Blocked(reason.clone()),
                _ => CleanupState::Pending,
            };
            self.cleanups.insert(
                id,
                CleanupTask {
                    record: CleanupRecord {
                        id,
                        key,
                        holding: handle,
                        state,
                        attempts: 0,
                        requested_at: now,
                        updated_at: now,
                    },
                },
            );
            h.record.state = HoldingState::Retiring(id);
        }
        Ok(self.resolve(handle)?.state)
    }

    /// A claim is produced only for a retirement task with no occupying child.
    /// Storage must commit this transition before invoking the executor.
    pub fn claim_cleanup(
        &mut self,
        id: CleanupId,
        worker: HostWitness,
        now: Timestamp,
    ) -> Result<Option<CleanupWork>, LedgerError> {
        let task = self.cleanups.get(&id).ok_or(LedgerError::InvalidCleanup)?;
        if matches!(
            task.state,
            CleanupState::Running { .. } | CleanupState::Done(_)
        ) {
            return Ok(None);
        }
        let h = self.resolve(&task.holding)?;
        if h.state != HoldingState::Retiring(id) {
            return Err(LedgerError::InvalidCleanup);
        }
        if self.occupying().any(|child| child.parent == Some(h.id)) {
            return Ok(None);
        }
        let target = h.target.clone();
        let resource = h.key();
        let grade = self.classes[&h.class_id].revert_grade;
        let attempts = task
            .attempts
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or(LedgerError::OutOfRange)?;
        let task = self.cleanups.get_mut(&id).unwrap();
        task.record.attempts = attempts;
        task.record.state = CleanupState::Running { worker };
        task.record.updated_at = now;
        Ok(Some(CleanupWork {
            task: task.record.clone(),
            target,
            resource,
            grade,
        }))
    }

    pub fn finish_cleanup(
        &mut self,
        work: &CleanupWork,
        outcome: CleanupOutcome,
        now: Timestamp,
    ) -> Result<(), LedgerError> {
        let task = self
            .cleanups
            .get(&work.task.id)
            .ok_or(LedgerError::StaleAttempt)?;
        if task.record != work.task || !matches!(task.state, CleanupState::Running { .. }) {
            return Err(LedgerError::StaleAttempt);
        }
        let h = self.resolve(&task.holding)?;
        if h.state != HoldingState::Retiring(task.id) || h.target != work.target {
            return Err(LedgerError::StaleAttempt);
        }
        if self.occupying().any(|child| child.parent == Some(h.id)) {
            return Err(LedgerError::TeardownOrder);
        }
        let state = match outcome {
            CleanupOutcome::Confirmed => CleanupState::Done(Completion::Confirmed),
            CleanupOutcome::AlreadyAbsent => CleanupState::Done(Completion::AlreadyAbsent),
            CleanupOutcome::Retryable(s) => CleanupState::Retryable(s),
            CleanupOutcome::Unknown(s) => CleanupState::Unknown(s),
            CleanupOutcome::Blocked(s) => CleanupState::Blocked(s),
        };
        if state.is_done() {
            self.holdings
                .iter_mut()
                .find(|h| h.id == work.task.holding.id())
                .unwrap()
                .record
                .state = HoldingState::Retired(now);
        }
        let task = self.cleanups.get_mut(&work.task.id).unwrap();
        task.record.state = state;
        task.record.updated_at = now;
        Ok(())
    }

    /// Only the coordinator decides whether an owning worker has disappeared.
    /// Matching the whole record prevents a recovery observation from resetting
    /// an attempt another worker has since claimed.
    pub fn abandon_cleanup(
        &mut self,
        observed: &CleanupRecord,
        now: Timestamp,
    ) -> Result<(), LedgerError> {
        let task = self
            .cleanups
            .get_mut(&observed.id)
            .ok_or(LedgerError::StaleAttempt)?;
        if &task.record != observed || !matches!(task.state, CleanupState::Running { .. }) {
            return Err(LedgerError::StaleAttempt);
        }
        task.record.state =
            CleanupState::Unknown("worker interrupted before durable confirmation".into());
        task.record.updated_at = now;
        Ok(())
    }

    pub fn due_retirements(&self, now: Timestamp) -> Vec<HoldingHandle> {
        let mut due: BTreeSet<_> = self
            .occupying()
            .filter(|h| !h.state.is_active() || matches!(h.lease, Lease::Until(t) if t <= now))
            .map(|h| h.id)
            .collect();
        loop {
            let before = due.len();
            due.extend(
                self.active()
                    .filter(|h| {
                        h.lease == Lease::ParentBound && h.parent.is_some_and(|p| due.contains(&p))
                    })
                    .map(|h| h.id)
                    .collect::<Vec<_>>(),
            );
            if due.len() == before {
                break;
            }
        }
        self.active()
            .filter(|h| due.contains(&h.id))
            .map(Holding::handle)
            .collect()
    }
    pub fn occupying_closure(&self, subject: &SubjectId) -> Vec<LiveItem> {
        self.occupying_descendants(
            self.occupying()
                .filter(|h| &h.subject == subject)
                .map(|h| self.live_item(h))
                .collect(),
        )
    }
    pub fn occupying_subtree(&self, root: HoldingId) -> Vec<LiveItem> {
        self.occupying_descendants(
            self.occupying()
                .filter(|h| h.id == root)
                .map(|h| self.live_item(h))
                .collect(),
        )
    }
    fn occupying_descendants(&self, mut items: Vec<LiveItem>) -> Vec<LiveItem> {
        let mut i = 0;
        while i < items.len() {
            let p = items[i].id;
            items.extend(
                self.occupying()
                    .filter(|h| h.parent == Some(p) && !items.iter().any(|it| it.id == h.id))
                    .map(|h| self.live_item(h))
                    .collect::<Vec<_>>(),
            );
            i += 1;
        }
        items
    }
    pub(super) fn cleanup_invariant(&self) -> Result<(), LedgerError> {
        let occupying = self.occupying().collect::<Vec<_>>();
        for (i, h) in occupying.iter().enumerate() {
            if occupying[i + 1..]
                .iter()
                .any(|other| h.target.conflicts_with(&other.target))
            {
                return Err(LedgerError::Conflict);
            }
        }
        let mut keys = BTreeSet::new();
        for h in &self.holdings {
            if h.target.policy() != self.classes[&h.class_id].cleanup {
                return Err(LedgerError::InvalidCleanup);
            }
            if h.state.is_active()
                && h.lease == Lease::ParentBound
                && h.parent
                    .and_then(|p| self.holding(p))
                    .is_some_and(|p| !p.state.is_active())
            {
                return Err(LedgerError::InvalidCleanup);
            }
            if matches!(h.target, CleanupTarget::Unresolved { .. }) && h.state.is_active() {
                return Err(LedgerError::InvalidCleanup);
            }
            if let HoldingState::Retiring(id) = h.state {
                if !self
                    .cleanups
                    .get(&id)
                    .is_some_and(|t| t.holding == h.handle() && !t.state.is_done())
                {
                    return Err(LedgerError::InvalidCleanup);
                }
            }
            if matches!(h.state, HoldingState::Retired(_))
                && matches!(h.target.policy(), CleanupPolicy::Managed(_))
            {
                if !self
                    .cleanups
                    .get(&CleanupId::for_holding(h.id))
                    .is_some_and(|t| t.holding == h.handle() && t.state.is_done())
                {
                    return Err(LedgerError::InvalidCleanup);
                }
            }
        }
        for t in self.cleanups.values() {
            let h = self.resolve(&t.holding)?;
            if t.id != CleanupId::for_holding(h.id)
                || !keys.insert(t.key.clone())
                || t.attempts > i64::MAX as u64
            {
                return Err(LedgerError::InvalidCleanup);
            }
            let valid = match t.state {
                CleanupState::Done(Completion::Confirmed) => {
                    t.attempts > 0 && matches!(h.state, HoldingState::Retired(_))
                }
                CleanupState::Done(Completion::AlreadyAbsent) => {
                    matches!(h.state, HoldingState::Retired(_))
                }
                CleanupState::Running { .. } => {
                    t.attempts > 0 && h.state == HoldingState::Retiring(t.id)
                }
                _ => h.state == HoldingState::Retiring(t.id),
            };
            if !valid {
                return Err(LedgerError::InvalidCleanup);
            }
        }
        Ok(())
    }
}
