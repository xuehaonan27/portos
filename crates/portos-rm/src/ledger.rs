//! Resource aggregate with typed pool admission and checked reconstruction.
//! Holdings and capacities change only through invariant-preserving operations.
//! Persistence and external resource cleanup belong to the kernel adapter.

use crate::auth::auth_valid;
use crate::ra::{Count, Ex, Frac, GSet, Ra, Ranges};

// ---------------------------------------------------------------------------
// Dynamic fragment over the built-in algebra library (closed set for the drill;
// Phase D may generalize behind the same laws — declared deviation in F1).
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Debug)]
pub enum Frag {
    Ex(Ex),
    Count(Count),
    Set(GSet),
    /// F6：不相交区间（MR 子区间／memory window／字节范围锁）。
    Range(Ranges),
    /// F6：分数持有（共享读：各持正分数，合成回 1 为独占）。
    Frac(Frac),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlgebraTag {
    Exclusive,
    Counted,
    Set,
    Range,
    Frac,
}

impl Frag {
    pub fn tag(&self) -> AlgebraTag {
        match self {
            Frag::Ex(_) => AlgebraTag::Exclusive,
            Frag::Count(_) => AlgebraTag::Counted,
            Frag::Set(_) => AlgebraTag::Set,
            Frag::Range(_) => AlgebraTag::Range,
            Frag::Frac(_) => AlgebraTag::Frac,
        }
    }
    pub fn op(&self, other: &Frag) -> Option<Frag> {
        match (self, other) {
            (Frag::Ex(a), Frag::Ex(b)) => Some(Frag::Ex(a.op(b))),
            (Frag::Count(a), Frag::Count(b)) => Some(Frag::Count(a.op(b))),
            (Frag::Set(a), Frag::Set(b)) => Some(Frag::Set(a.op(b))),
            (Frag::Range(a), Frag::Range(b)) => Some(Frag::Range(a.op(b))),
            (Frag::Frac(a), Frag::Frac(b)) => Some(Frag::Frac(a.op(b))),
            _ => None, // algebra mismatch — schema-level type error
        }
    }
    pub fn valid(&self) -> bool {
        match self {
            Frag::Ex(a) => a.valid(),
            Frag::Count(a) => a.valid(),
            Frag::Set(a) => a.valid(),
            Frag::Range(a) => a.valid(),
            Frag::Frac(a) => a.valid(),
        }
    }
    pub fn included_in(&self, b: &Frag) -> bool {
        match (self, b) {
            (Frag::Ex(a), Frag::Ex(b)) => a.included_in(b),
            (Frag::Count(a), Frag::Count(b)) => a.included_in(b),
            (Frag::Set(a), Frag::Set(b)) => a.included_in(b),
            (Frag::Range(a), Frag::Range(b)) => a.included_in(b),
            (Frag::Frac(a), Frag::Frac(b)) => a.included_in(b),
            _ => false,
        }
    }
}

fn compose_frags(frags: &[Frag]) -> Result<Option<Frag>, LedgerError> {
    let mut it = frags.iter();
    let Some(first) = it.next() else {
        return Ok(None);
    };
    let mut acc = first.clone();
    for f in it {
        acc = acc.op(f).ok_or(LedgerError::AlgebraMismatch)?;
    }
    Ok(Some(acc))
}

fn frag_auth_valid(capacity: &Frag, outstanding: &Option<Frag>) -> bool {
    match (capacity, outstanding) {
        (Frag::Ex(c), None) => auth_valid(c, &None),
        (Frag::Ex(c), Some(Frag::Ex(o))) => auth_valid(c, &Some(*o)),
        (Frag::Count(c), None) => auth_valid(c, &None),
        (Frag::Count(c), Some(Frag::Count(o))) => auth_valid(c, &Some(*o)),
        (Frag::Set(c), None) => auth_valid(c, &None),
        (Frag::Set(c), Some(Frag::Set(o))) => auth_valid(c, &Some(o.clone())),
        (Frag::Range(c), None) => auth_valid(c, &None),
        (Frag::Range(c), Some(Frag::Range(o))) => auth_valid(c, &Some(o.clone())),
        (Frag::Frac(c), None) => auth_valid(c, &None),
        (Frag::Frac(c), Some(Frag::Frac(o))) => auth_valid(c, &Some(o.clone())),
        _ => false,
    }
}

use crate::identity::{
    ClassId, Generation, HoldingHandle, HoldingId, InstanceId, PoolId, ResourceKey, SubjectId,
};
use crate::registry::{Capacity, Claim, ClassBinding, PoolRef, RegisteredClass, RuntimeAlgebra};
use crate::time::{Lease, LeaseDuration, LeaseRequest, Timestamp};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RevertGrade {
    Inverse,
    Compensable,
    External,
}

/// A declaration is input. Registration checks it and returns an immutable binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassDecl {
    pub class_id: ClassId,
    pub algebra: AlgebraTag,
    pub release_idempotent: bool,
    pub lease_duration: Option<LeaseDuration>,
    pub revert_grade: RevertGrade,
}

/// Untrusted reconstruction data, accepted only by LedgerBuilder::finish.
#[derive(Clone, Debug, PartialEq)]
pub struct HoldingRecord {
    pub id: HoldingId,
    pub subject: SubjectId,
    pub class_id: ClassId,
    pub instance: InstanceId,
    pub frag: Frag,
    pub generation: Generation,
    pub parent: Option<HoldingId>,
    pub lease: Lease,
    pub acquired_at: Timestamp,
    pub released_at: Option<Timestamp>,
}

/// The live aggregate never exposes mutable records, even through a cloned view.
#[derive(Clone, Debug, PartialEq)]
pub struct Holding {
    record: HoldingRecord,
}
impl std::ops::Deref for Holding {
    type Target = HoldingRecord;
    fn deref(&self) -> &Self::Target {
        &self.record
    }
}
impl Holding {
    pub fn handle(&self) -> HoldingHandle {
        HoldingHandle::new(self.id, self.generation.clone())
    }
    pub fn key(&self) -> ResourceKey {
        ResourceKey::new(self.class_id.clone(), self.instance.clone())
    }
}

#[derive(Clone, Debug)]
pub struct LiveItem {
    pub id: HoldingId,
    pub parent: Option<HoldingId>,
    pub class_id: ClassId,
    pub instance: InstanceId,
    pub generation: Generation,
    pub grade: RevertGrade,
}
impl LiveItem {
    pub fn handle(&self) -> HoldingHandle {
        HoldingHandle::new(self.id, self.generation.clone())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LedgerError {
    UnknownClass,
    UnknownPool,
    AlgebraMismatch,
    Conflict,
    ForgedHandle,
    StaleGeneration,
    TeardownOrder,
    DoubleRelease,
    ParentAuthority,
    InvalidValue,
    InvalidLease,
    OutOfRange,
    ClassConflict,
    PoolExists,
    ForeignReference,
    DuplicateId,
    InvalidGraph,
    InvalidGeneration,
}

#[derive(Clone, Debug)]
pub struct GrantRequest<A: RuntimeAlgebra> {
    pub owner: SubjectId,
    pub claim: Claim<A>,
    pub generation: Generation,
    pub parent: Option<HoldingHandle>,
    pub lease: LeaseRequest,
    pub now: Timestamp,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PoolRecord {
    pub key: ResourceKey,
    pub capacity: Frag,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LedgerSnapshot {
    pub classes: Vec<ClassDecl>,
    pub pools: Vec<PoolRecord>,
    pub holdings: Vec<HoldingRecord>,
    pub instantiations: Vec<(SubjectId, SubjectId)>,
}

/// Reconstruction has no operational methods. Only a checked graph becomes Ledger.
pub struct LedgerBuilder {
    snapshot: LedgerSnapshot,
}
impl LedgerBuilder {
    pub fn new(snapshot: LedgerSnapshot) -> Self {
        Self { snapshot }
    }
    pub fn finish(self) -> Result<Ledger, LedgerError> {
        let mut ledger = Ledger::new();
        for decl in self.snapshot.classes {
            if ledger.classes.contains_key(&decl.class_id) {
                return Err(LedgerError::ClassConflict);
            }
            ledger.register_class(decl)?;
        }
        for pool in self.snapshot.pools {
            if ledger.capacities.insert(pool.key, pool.capacity).is_some() {
                return Err(LedgerError::PoolExists);
            }
        }
        for (child, parent) in self.snapshot.instantiations {
            if ledger.instantiations.insert(child, parent).is_some() {
                return Err(LedgerError::InvalidGraph);
            }
        }
        for row in self.snapshot.holdings {
            ledger.next_id = ledger
                .next_id
                .max(row.id.get().checked_add(1).ok_or(LedgerError::OutOfRange)?);
            ledger.holdings.push(Holding { record: row });
        }
        ledger.invariant()?;
        Ok(ledger)
    }
}

static NEXT_LEDGER: AtomicU64 = AtomicU64::new(1);
pub struct Ledger {
    identity: u64,
    classes: BTreeMap<ClassId, ClassDecl>,
    capacities: BTreeMap<ResourceKey, Frag>,
    holdings: Vec<Holding>,
    next_id: u64,
    instantiations: BTreeMap<SubjectId, SubjectId>,
}
impl Default for Ledger {
    fn default() -> Self {
        Self {
            identity: NEXT_LEDGER
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .expect("ledger identity exhausted"),
            classes: BTreeMap::new(),
            capacities: BTreeMap::new(),
            holdings: Vec::new(),
            next_id: 0,
            instantiations: BTreeMap::new(),
        }
    }
}
impl Clone for Ledger {
    fn clone(&self) -> Self {
        let mut copy = Self::new();
        copy.classes = self.classes.clone();
        copy.capacities = self.capacities.clone();
        copy.holdings = self.holdings.clone();
        copy.next_id = self.next_id;
        copy.instantiations = self.instantiations.clone();
        copy
    }
}
impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn snapshot(&self) -> LedgerSnapshot {
        LedgerSnapshot {
            classes: self.classes.values().cloned().collect(),
            pools: self
                .capacities
                .iter()
                .map(|(key, capacity)| PoolRecord {
                    key: key.clone(),
                    capacity: capacity.clone(),
                })
                .collect(),
            holdings: self.holdings.iter().map(|h| h.record.clone()).collect(),
            instantiations: self
                .instantiations
                .iter()
                .map(|(c, p)| (c.clone(), p.clone()))
                .collect(),
        }
    }
    pub fn register_class(&mut self, decl: ClassDecl) -> Result<ClassBinding, LedgerError> {
        if let Some(existing) = self.classes.get(&decl.class_id) {
            if existing != &decl {
                return Err(LedgerError::ClassConflict);
            }
        }
        let binding = ClassBinding {
            ledger: self.identity,
            id: decl.class_id.clone(),
            algebra: decl.algebra,
        };
        self.classes.entry(decl.class_id.clone()).or_insert(decl);
        Ok(binding)
    }
    pub fn registered_class<A: RuntimeAlgebra>(
        &self,
        id: &ClassId,
    ) -> Result<RegisteredClass<A>, LedgerError> {
        let decl = self.classes.get(id).ok_or(LedgerError::UnknownClass)?;
        ClassBinding {
            ledger: self.identity,
            id: id.clone(),
            algebra: decl.algebra,
        }
        .for_algebra()
    }
    pub fn create_pool<A: RuntimeAlgebra>(
        &mut self,
        class: &RegisteredClass<A>,
        instance: InstanceId,
        capacity: Capacity<A>,
    ) -> Result<PoolRef<A>, LedgerError> {
        if class.binding.ledger != self.identity {
            return Err(LedgerError::ForeignReference);
        }
        let current = self.registered_class::<A>(class.id())?;
        let key = ResourceKey::new(current.id().clone(), instance);
        if let Some(existing) = self.capacities.get(&key) {
            if A::from_fragment(existing) == Some(capacity.value()) {
                return Ok(PoolRef::new(self.identity, PoolId::new(key)));
            }
            return Err(LedgerError::PoolExists);
        }
        self.capacities
            .insert(key.clone(), capacity.into_fragment());
        Ok(PoolRef::new(self.identity, PoolId::new(key)))
    }
    pub fn pool<A: RuntimeAlgebra>(&self, key: &ResourceKey) -> Result<PoolRef<A>, LedgerError> {
        let cap = self.capacities.get(key).ok_or(LedgerError::UnknownPool)?;
        if cap.tag() != A::TAG {
            return Err(LedgerError::AlgebraMismatch);
        }
        Ok(PoolRef::new(self.identity, PoolId::new(key.clone())))
    }
    fn check_pool<A: RuntimeAlgebra>(&self, pool: &PoolRef<A>) -> Result<&Frag, LedgerError> {
        if pool.ledger != self.identity {
            return Err(LedgerError::ForeignReference);
        }
        let cap = self
            .capacities
            .get(pool.id().key())
            .ok_or(LedgerError::UnknownPool)?;
        if cap.tag() != A::TAG {
            return Err(LedgerError::AlgebraMismatch);
        }
        Ok(cap)
    }
    pub fn resize_pool<A: RuntimeAlgebra>(
        &mut self,
        pool: &PoolRef<A>,
        capacity: Capacity<A>,
    ) -> Result<(), LedgerError> {
        self.check_pool(pool)?;
        let cap = capacity.into_fragment();
        if !frag_auth_valid(&cap, &compose_frags(&self.live_frags(pool.id().key()))?) {
            return Err(LedgerError::Conflict);
        }
        self.capacities.insert(pool.id().key().clone(), cap);
        Ok(())
    }
    /// Settle exactly this account pool; failure leaves the original graph intact.
    pub fn settle_and_zero_pool(
        &mut self,
        pool: &PoolRef<Count>,
        now: Timestamp,
    ) -> Result<(), LedgerError> {
        self.check_pool(pool)?;
        let mut staged = self.clone();
        let mut rows: Vec<_> = staged
            .live()
            .filter(|h| h.key() == *pool.id().key())
            .map(|h| (staged.depth(h.id), h.handle()))
            .collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, handle) in rows {
            staged.release(&handle, now)?;
        }
        staged
            .capacities
            .insert(pool.id().key().clone(), Frag::Count(Count::Value(0)));
        // Preserve references to this aggregate for this atomic in-memory operation.
        staged.identity = self.identity;
        *self = staged;
        Ok(())
    }
    pub fn capacity(&self, key: &ResourceKey) -> Option<&Frag> {
        self.capacities.get(key)
    }
    pub fn has_class(&self, id: &ClassId) -> bool {
        self.classes.contains_key(id)
    }
    pub fn grade_of(&self, id: &ClassId) -> Option<RevertGrade> {
        self.classes.get(id).map(|d| d.revert_grade)
    }
    pub fn declare_instantiation(&mut self, child: SubjectId, parent: SubjectId) {
        self.instantiations.insert(child, parent);
    }
    pub fn live(&self) -> impl Iterator<Item = &Holding> {
        self.holdings.iter().filter(|h| h.released_at.is_none())
    }
    pub fn holdings(&self) -> &[Holding] {
        &self.holdings
    }
    pub fn holding(&self, id: HoldingId) -> Option<&Holding> {
        self.holdings.iter().find(|h| h.id == id)
    }
    pub fn resolve(&self, handle: &HoldingHandle) -> Result<&Holding, LedgerError> {
        let h = self.holding(handle.id()).ok_or(LedgerError::ForgedHandle)?;
        if &h.generation != handle.generation() {
            return Err(LedgerError::StaleGeneration);
        }
        Ok(h)
    }
    fn live_frags(&self, key: &ResourceKey) -> Vec<Frag> {
        self.live()
            .filter(|h| h.key() == *key)
            .map(|h| h.frag.clone())
            .collect()
    }
    pub fn grant<A: RuntimeAlgebra>(
        &mut self,
        pool: &PoolRef<A>,
        request: GrantRequest<A>,
    ) -> Result<HoldingHandle, LedgerError> {
        let cap = self.check_pool(pool)?;
        let key = pool.id().key();
        let decl = self
            .classes
            .get(key.class())
            .ok_or(LedgerError::UnknownClass)?;
        if request.generation.as_str().is_empty() {
            return Err(LedgerError::InvalidGeneration);
        }
        if let Some(parent) = &request.parent {
            let ph = self.resolve(parent)?;
            if ph.released_at.is_some() {
                return Err(LedgerError::ForgedHandle);
            }
            if ph.subject != request.owner
                && self.instantiations.get(&request.owner) != Some(&ph.subject)
            {
                return Err(LedgerError::ParentAuthority);
            }
        }
        let lease =
            request
                .lease
                .resolve(decl.lease_duration, request.parent.is_some(), request.now)?;
        let want = request.claim.into_fragment();
        let mut all = self.live_frags(key);
        all.push(want.clone());
        if !frag_auth_valid(cap, &compose_frags(&all)?) {
            return Err(LedgerError::Conflict);
        }
        let id = HoldingId::try_from(self.next_id)?;
        let next_id = self.next_id.checked_add(1).ok_or(LedgerError::OutOfRange)?;
        let handle = HoldingHandle::new(id, request.generation.clone());
        self.holdings.push(Holding {
            record: HoldingRecord {
                id,
                subject: request.owner,
                class_id: key.class().clone(),
                instance: key.instance().clone(),
                frag: want,
                generation: request.generation,
                parent: request.parent.map(|p| p.id()),
                lease,
                acquired_at: request.now,
                released_at: None,
            },
        });
        self.next_id = next_id;
        Ok(handle)
    }
    pub fn transfer(
        &mut self,
        handle: &HoldingHandle,
        from: &SubjectId,
        to: SubjectId,
    ) -> Result<(), LedgerError> {
        let h = self.resolve(handle)?;
        if h.released_at.is_some() || &h.subject != from {
            return Err(LedgerError::ForgedHandle);
        }
        self.holdings
            .iter_mut()
            .find(|h| h.id == handle.id())
            .unwrap()
            .record
            .subject = to;
        Ok(())
    }
    pub fn release(&mut self, handle: &HoldingHandle, now: Timestamp) -> Result<(), LedgerError> {
        let h = self.resolve(handle)?;
        if h.released_at.is_some() {
            return if self
                .classes
                .get(&h.class_id)
                .ok_or(LedgerError::UnknownClass)?
                .release_idempotent
            {
                Ok(())
            } else {
                Err(LedgerError::DoubleRelease)
            };
        }
        if self.live().any(|h| h.parent == Some(handle.id())) {
            return Err(LedgerError::TeardownOrder);
        }
        self.holdings
            .iter_mut()
            .find(|h| h.id == handle.id())
            .unwrap()
            .record
            .released_at = Some(now);
        Ok(())
    }
    pub fn renew(
        &mut self,
        handle: &HoldingHandle,
        request: LeaseRequest,
        now: Timestamp,
    ) -> Result<Lease, LedgerError> {
        let h = self.resolve(handle)?;
        if h.released_at.is_some() {
            return Err(LedgerError::ForgedHandle);
        }
        let decl = self
            .classes
            .get(&h.class_id)
            .ok_or(LedgerError::UnknownClass)?;
        let lease = if request == LeaseRequest::UseClassDefault && decl.lease_duration.is_none() {
            h.lease // Historical heartbeat on classes without defaults preserves the override.
        } else {
            request.resolve(decl.lease_duration, h.parent.is_some(), now)?
        };
        self.holdings
            .iter_mut()
            .find(|h| h.id == handle.id())
            .unwrap()
            .record
            .lease = lease;
        Ok(lease)
    }
    fn depth(&self, id: HoldingId) -> usize {
        let mut depth = 0;
        let mut parent = self.holding(id).and_then(|h| h.parent);
        while let Some(p) = parent {
            depth += 1;
            parent = self.holding(p).and_then(|h| h.parent);
        }
        depth
    }
    /// Expired parents wait for children with independent, unexpired leases.
    pub fn sweep(&mut self, now: Timestamp) -> Vec<HoldingId> {
        let mut due: BTreeSet<_> = self
            .live()
            .filter(|h| matches!(h.lease, Lease::Until(t) if t <= now))
            .map(|h| h.id)
            .collect();
        loop {
            let kids: Vec<_> = self
                .live()
                .filter(|h| {
                    h.lease == Lease::ParentBound && h.parent.is_some_and(|p| due.contains(&p))
                })
                .map(|h| h.id)
                .collect();
            let old = due.len();
            due.extend(kids);
            if due.len() == old {
                break;
            }
        }
        self.release_order(due, now)
    }
    fn release_order(&mut self, ids: BTreeSet<HoldingId>, now: Timestamp) -> Vec<HoldingId> {
        let mut order: Vec<_> = ids
            .into_iter()
            .map(|id| (self.depth(id), self.holding(id).unwrap().handle()))
            .collect();
        order.sort_by(|a, b| b.0.cmp(&a.0));
        order
            .into_iter()
            .filter_map(|(_, h)| self.release(&h, now).ok().map(|_| h.id()))
            .collect()
    }
    pub fn teardown(&mut self, subject: &SubjectId, now: Timestamp) -> Vec<HoldingId> {
        self.release_order(
            self.live()
                .filter(|h| &h.subject == subject)
                .map(|h| h.id)
                .collect(),
            now,
        )
    }
    /// Includes orphan rows and the entire parent graph, not just capacity keys.
    pub fn invariant(&self) -> Result<(), LedgerError> {
        let mut ids = BTreeSet::new();
        for h in &self.holdings {
            if !ids.insert(h.id) {
                return Err(LedgerError::DuplicateId);
            }
            if h.generation.as_str().is_empty() {
                return Err(LedgerError::InvalidGeneration);
            }
            let decl = self
                .classes
                .get(&h.class_id)
                .ok_or(LedgerError::UnknownClass)?;
            if h.frag.tag() != decl.algebra {
                return Err(LedgerError::AlgebraMismatch);
            }
            if !h.frag.valid() {
                return Err(LedgerError::InvalidValue);
            }
            if h.lease == Lease::ParentBound && h.parent.is_none() {
                return Err(LedgerError::InvalidLease);
            }
            if h.released_at.is_none() && !self.capacities.contains_key(&h.key()) {
                return Err(LedgerError::UnknownPool);
            }
            let mut ancestors = BTreeSet::from([h.id]);
            let mut parent = h.parent;
            while let Some(id) = parent {
                if !ancestors.insert(id) {
                    return Err(LedgerError::InvalidGraph);
                }
                let ph = self.holding(id).ok_or(LedgerError::InvalidGraph)?;
                if h.released_at.is_none() && ph.released_at.is_some() {
                    return Err(LedgerError::InvalidGraph);
                }
                parent = ph.parent;
            }
        }
        for (key, cap) in &self.capacities {
            let decl = self
                .classes
                .get(key.class())
                .ok_or(LedgerError::UnknownClass)?;
            if cap.tag() != decl.algebra {
                return Err(LedgerError::AlgebraMismatch);
            }
            if !cap.valid() {
                return Err(LedgerError::InvalidValue);
            }
            if !frag_auth_valid(cap, &compose_frags(&self.live_frags(key))?) {
                return Err(LedgerError::Conflict);
            }
        }
        Ok(())
    }
    pub fn reconcile(
        &self,
        class: &ClassId,
        substrate: &[(InstanceId, Generation)],
    ) -> (Vec<HoldingId>, Vec<(InstanceId, Generation)>) {
        let decayed = self
            .live()
            .filter(|h| {
                &h.class_id == class
                    && !substrate
                        .iter()
                        .any(|(i, g)| i == &h.instance && g == &h.generation)
            })
            .map(|h| h.id)
            .collect();
        let untracked = substrate
            .iter()
            .filter(|(i, g)| {
                !self
                    .live()
                    .any(|h| &h.class_id == class && &h.instance == i && &h.generation == g)
            })
            .cloned()
            .collect();
        (decayed, untracked)
    }
    fn live_item(&self, h: &Holding) -> LiveItem {
        LiveItem {
            id: h.id,
            parent: h.parent,
            class_id: h.class_id.clone(),
            instance: h.instance.clone(),
            generation: h.generation.clone(),
            grade: self.classes[&h.class_id].revert_grade,
        }
    }
    pub fn live_snapshot(&self, subject: &SubjectId) -> Vec<LiveItem> {
        self.live()
            .filter(|h| &h.subject == subject)
            .map(|h| self.live_item(h))
            .collect()
    }
    fn closure(&self, mut items: Vec<LiveItem>) -> Vec<LiveItem> {
        let mut i = 0;
        while i < items.len() {
            let p = items[i].id;
            let kids: Vec<_> = self
                .live()
                .filter(|h| h.parent == Some(p) && !items.iter().any(|it| it.id == h.id))
                .map(|h| self.live_item(h))
                .collect();
            items.extend(kids);
            i += 1;
        }
        items
    }
    pub fn live_closure(&self, subject: &SubjectId) -> Vec<LiveItem> {
        self.closure(self.live_snapshot(subject))
    }
    pub fn live_subtree(&self, root: HoldingId) -> Vec<LiveItem> {
        self.closure(
            self.live()
                .filter(|h| h.id == root)
                .map(|h| self.live_item(h))
                .collect(),
        )
    }
    pub fn live_count(&self) -> usize {
        self.live().count()
    }
    pub fn tombstone_count(&self) -> usize {
        self.holdings.len() - self.live_count()
    }
}

/// Deterministic pseudo-random generator for the law tests.
pub struct Lcg(pub u64);
impl Lcg {
    pub fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// ```compile_fail
/// use portos_rm::{ledger::Holding, identity::SubjectId};
/// fn rewrite(mut h: Holding) { h.subject = SubjectId::new("other"); }
/// ```
/// ```compile_fail
/// use portos_rm::{ledger::{LedgerBuilder, GrantRequest}, registry::PoolRef, ra::Count};
/// fn unfinished(mut builder: LedgerBuilder, pool: &PoolRef<Count>, request: GrantRequest<Count>) {
///     builder.grant(pool, request);
/// }
/// ```
/// ```compile_fail
/// use portos_rm::ledger::{Ledger, HoldingRecord};
/// fn inject(l: &mut Ledger, row: HoldingRecord) { l.restore_row(row); }
/// ```
const _: () = ();
