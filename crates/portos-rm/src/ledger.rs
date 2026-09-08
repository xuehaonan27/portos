//! The holding ledger — freeze drill F1's engineering translation.
//!
//! Schema (mirrored in schema.sql): one row per fragment (see auth.rs consequence 1),
//! generation-stable handles, ownership tree via `parent`, leases, tombstones.
//! Error taxonomy = the "⊕ 无定义的三种运行时对应" of theory-spec §2.2 plus the
//! generation/teardown checks.

use crate::auth::{auth_valid, can_mint};
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
        (Frag::Frac(c), Some(Frag::Frac(o))) => auth_valid(c, &Some(*o)),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Class declarations (the registry row — Σ/E/T/W flags the drill needs).
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RevertGrade {
    Inverse,
    Compensable,
    External,
}

#[derive(Clone, Debug)]
pub struct ClassDecl {
    pub class_id: String,
    pub algebra: AlgebraTag,
    /// E: release∘release = release (blind replay safe). All drill classes: true.
    pub release_idempotent: bool,
    /// T: lease duration; None = lifetime bound to parent only.
    pub lease_secs: Option<u64>,
    /// W: ρ 档（枚举形态；等价格的语义见 theory-spec §2.5）
    pub revert_grade: RevertGrade,
}

// ---------------------------------------------------------------------------
// Holdings.
// ---------------------------------------------------------------------------
#[derive(Clone, Debug)]
pub struct Holding {
    pub id: u64,
    pub subject: String,
    pub class_id: String,
    pub instance: String,
    pub frag: Frag,
    /// Generation witness — stable denotation for substrates that recycle names
    /// (pid start-time, CDP target nonce). RA carriers need stable identity.
    pub generation: String,
    pub parent: Option<u64>,
    pub lease_expires_at: Option<u64>,
    pub acquired_at: u64,
    pub released_at: Option<u64>, // tombstone; audit chain proper lives in aos-kernel
}

/// F2 规划器消费的持有视图。
#[derive(Clone, Debug)]
pub struct LiveItem {
    pub id: u64,
    pub parent: Option<u64>,
    pub class_id: String,
    pub instance: String,
    pub generation: String,
    pub grade: RevertGrade,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LedgerError {
    UnknownClass,
    AlgebraMismatch,
    /// ⊕ invalid or not ≼ capacity — grant refused (conflict / over-capacity).
    Conflict,
    /// Handle not in the ledger — forged or long-gone.
    ForgedHandle,
    /// Right name, wrong incarnation (ABA).
    StaleGeneration,
    /// Releasing a parent with live children outside teardown order.
    TeardownOrder,
    /// Double release on a class that declared release NOT idempotent.
    DoubleRelease,
    /// [AUTH-EDGE] 跨主体 parent 无权：父持有既不是自己的，也不属于实例化了自己的主体。
    ParentAuthority,
}

#[derive(Default, Clone)]
pub struct Ledger {
    classes: std::collections::BTreeMap<String, ClassDecl>,
    capacities: std::collections::BTreeMap<(String, String), Frag>,
    holdings: Vec<Holding>,
    next_id: u64,
    /// [AUTH-EDGE] 实例化关系：child subject → 实例化它的 subject（内核在拉起子实例时登记）。
    /// 决策 2（用户裁定 2026-09-05）：ownership 边可跨主体，但只沿这条关系——
    /// 父持有须是自己的，或属于实例化了自己的主体；关系不传递（祖父的树要经父的持有进入）。
    instantiations: std::collections::BTreeMap<String, String>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_class(&mut self, decl: ClassDecl) {
        self.classes.insert(decl.class_id.clone(), decl);
    }

    pub fn set_capacity(&mut self, class_id: &str, instance: &str, cap: Frag) {
        self.capacities
            .insert((class_id.to_string(), instance.to_string()), cap);
    }

    /// [AUTH-EDGE] 登记"`parent_subject` 实例化了 `child`"——是内核在拉起子实例（Cordis Def 52
    /// 的实例化效应）时写下的事实，不是插件自报。此后 `child` 才可把持有挂到 `parent_subject` 的持有下。
    pub fn declare_instantiation(&mut self, child: &str, parent_subject: &str) {
        self.instantiations.insert(child.to_string(), parent_subject.to_string());
    }

    /// 转授＝持有转移（endstate §8.2 库清单"转授/委托"）：只改持有者，碎片不变 ⇒ 每个 (class, instance)
    /// 的合成值不变 ⇒ frame-preserving update 平凡成立。用途：F3 段提交（段主体 → fiber）、将来跨主体转授。
    /// 世代不符拒（ABA）；来源主体不符拒（句柄不是你的）；已释放拒。
    pub fn transfer(&mut self, id: u64, generation: &str, from_subject: &str, to_subject: &str) -> Result<(), LedgerError> {
        let h = self
            .holdings
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or(LedgerError::ForgedHandle)?;
        if h.released_at.is_some() || h.subject != from_subject {
            return Err(LedgerError::ForgedHandle);
        }
        if h.generation != generation {
            return Err(LedgerError::StaleGeneration);
        }
        h.subject = to_subject.to_string();
        Ok(())
    }

    pub fn live(&self) -> impl Iterator<Item = &Holding> {
        self.holdings.iter().filter(|h| h.released_at.is_none())
    }

    /// 全部行（含墓碑）——实装写穿持久化用。
    pub fn holdings(&self) -> &[Holding] {
        &self.holdings
    }

    /// 单行查询（含墓碑）。
    pub fn holding(&self, id: u64) -> Option<&Holding> {
        self.holdings.iter().find(|h| h.id == id)
    }

    /// 实装重启时从持久层回灌一行：**不过闸门**——行已是记账真相（后果一），只恢复 id 计数。
    /// 回灌后 `invariant()` 仍应成立；不成立即持久层已损坏（实装应拒绝启动或走对账）。
    pub fn restore_row(&mut self, h: Holding) {
        self.next_id = self.next_id.max(h.id + 1);
        self.holdings.push(h);
    }

    /// 类声明是否已登记（实装：内核内置类在每次启动时重新登记）。
    pub fn has_class(&self, class_id: &str) -> bool {
        self.classes.contains_key(class_id)
    }

    /// 类声明的可逆档（实装：sweep 的世界动作按档选路，与 teardown 同纪律）。
    pub fn grade_of(&self, class_id: &str) -> Option<RevertGrade> {
        self.classes.get(class_id).map(|d| d.revert_grade)
    }

    /// 实装：逐持有租约覆盖。类声明的 `lease_secs` 只是缺省；内核 `hold` op 的
    /// `lease_secs` 走这里（per-holding 租约，见实现计划 §10 的陷阱条目）。
    pub fn set_lease(
        &mut self,
        id: u64,
        generation: &str,
        lease_expires_at: Option<u64>,
    ) -> Result<(), LedgerError> {
        let h = self
            .holdings
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or(LedgerError::ForgedHandle)?;
        if h.released_at.is_some() {
            return Err(LedgerError::ForgedHandle);
        }
        if h.generation != generation {
            return Err(LedgerError::StaleGeneration);
        }
        h.lease_expires_at = lease_expires_at;
        Ok(())
    }

    /// 某 (class, instance) 的容量元素（实装：cap 计数池的容量）。
    pub fn capacity(&self, class_id: &str, instance: &str) -> Option<&Frag> {
        self.capacities.get(&(class_id.to_string(), instance.to_string()))
    }

    fn live_frags(&self, class_id: &str, instance: &str) -> Vec<Frag> {
        self.live()
            .filter(|h| h.class_id == class_id && h.instance == instance)
            .map(|h| h.frag.clone())
            .collect()
    }

    /// Grant = issuer-gated mint (auth.rs::can_mint over the closed world).
    #[allow(clippy::too_many_arguments)]
    pub fn grant(
        &mut self,
        subject: &str,
        class_id: &str,
        instance: &str,
        want: Frag,
        generation: &str,
        parent: Option<u64>,
        now: u64,
    ) -> Result<u64, LedgerError> {
        let decl = self.classes.get(class_id).ok_or(LedgerError::UnknownClass)?;
        if want.tag() != decl.algebra {
            return Err(LedgerError::AlgebraMismatch);
        }
        if let Some(p) = parent {
            let ph = self
                .holdings
                .iter()
                .find(|h| h.id == p)
                .ok_or(LedgerError::ForgedHandle)?;
            if ph.released_at.is_some() {
                return Err(LedgerError::ForgedHandle);
            }
            // [AUTH-EDGE] 决策 2：跨主体 parent 只沿实例化关系。
            if ph.subject != subject
                && self.instantiations.get(subject).map(String::as_str) != Some(ph.subject.as_str())
            {
                return Err(LedgerError::ParentAuthority);
            }
        }
        let cap = self
            .capacities
            .get(&(class_id.to_string(), instance.to_string()))
            .ok_or(LedgerError::UnknownClass)?;
        let live = self.live_frags(class_id, instance);
        let ok = match (cap, &want) {
            (Frag::Ex(c), Frag::Ex(w)) => {
                let lv: Vec<Ex> = live
                    .iter()
                    .map(|f| match f {
                        Frag::Ex(x) => *x,
                        _ => Ex::Bot,
                    })
                    .collect();
                can_mint(c, &lv, w)
            }
            (Frag::Count(c), Frag::Count(w)) => {
                let lv: Vec<Count> = live
                    .iter()
                    .map(|f| match f {
                        Frag::Count(x) => *x,
                        _ => Count(u64::MAX),
                    })
                    .collect();
                can_mint(c, &lv, w)
            }
            (Frag::Set(c), Frag::Set(w)) => {
                let lv: Vec<GSet> = live
                    .iter()
                    .map(|f| match f {
                        Frag::Set(x) => x.clone(),
                        _ => GSet::default(),
                    })
                    .collect();
                can_mint(c, &lv, w)
            }
            (Frag::Range(c), Frag::Range(w)) => {
                let lv: Vec<Ranges> = live
                    .iter()
                    .map(|f| match f {
                        Frag::Range(x) => x.clone(),
                        _ => Ranges::bot(),
                    })
                    .collect();
                can_mint(c, &lv, w)
            }
            (Frag::Frac(c), Frag::Frac(w)) => {
                let lv: Vec<Frac> = live
                    .iter()
                    .map(|f| match f {
                        Frag::Frac(x) => *x,
                        _ => Frac::new(2, 1), // 类型错位 ⇒ 非法元（>1）
                    })
                    .collect();
                can_mint(c, &lv, w)
            }
            _ => return Err(LedgerError::AlgebraMismatch),
        };
        if !ok {
            return Err(LedgerError::Conflict);
        }
        let id = self.next_id;
        self.next_id += 1;
        let lease = decl.lease_secs.map(|s| now + s);
        self.holdings.push(Holding {
            id,
            subject: subject.to_string(),
            class_id: class_id.to_string(),
            instance: instance.to_string(),
            frag: want,
            generation: generation.to_string(),
            parent,
            lease_expires_at: lease,
            acquired_at: now,
            released_at: None,
        });
        Ok(id)
    }

    fn has_live_children(&self, id: u64) -> bool {
        self.live().any(|h| h.parent == Some(id))
    }

    /// Release = drop the row (no algebraic subtraction exists — consequence 1).
    pub fn release(&mut self, id: u64, generation: &str, now: u64) -> Result<(), LedgerError> {
        let idx = self
            .holdings
            .iter()
            .position(|h| h.id == id)
            .ok_or(LedgerError::ForgedHandle)?;
        if self.holdings[idx].generation != generation {
            return Err(LedgerError::StaleGeneration);
        }
        if self.holdings[idx].released_at.is_some() {
            let decl = self
                .classes
                .get(&self.holdings[idx].class_id)
                .ok_or(LedgerError::UnknownClass)?;
            return if decl.release_idempotent {
                Ok(())
            } else {
                Err(LedgerError::DoubleRelease)
            };
        }
        if self.has_live_children(id) {
            return Err(LedgerError::TeardownOrder);
        }
        self.holdings[idx].released_at = Some(now);
        Ok(())
    }

    pub fn renew(&mut self, id: u64, generation: &str, now: u64) -> Result<(), LedgerError> {
        let decl_secs = {
            let h = self
                .holdings
                .iter()
                .find(|h| h.id == id && h.released_at.is_none())
                .ok_or(LedgerError::ForgedHandle)?;
            if h.generation != generation {
                return Err(LedgerError::StaleGeneration);
            }
            self.classes
                .get(&h.class_id)
                .ok_or(LedgerError::UnknownClass)?
                .lease_secs
        };
        if let Some(s) = decl_secs {
            let h = self.holdings.iter_mut().find(|h| h.id == id).unwrap();
            h.lease_expires_at = Some(now + s);
        }
        Ok(())
    }

    fn depth(&self, id: u64) -> usize {
        let mut d = 0;
        let mut cur = self.holdings.iter().find(|h| h.id == id).and_then(|h| h.parent);
        while let Some(p) = cur {
            d += 1;
            cur = self.holdings.iter().find(|h| h.id == p).and_then(|h| h.parent);
        }
        d
    }

    /// Lease sweeper — the involuntary path. Children first (depth desc).
    ///
    /// [B14] 租约为 `None` 的持有"仅随 parent 生命期"（schema.sql 注释）：父项租约到期时它们
    /// 必须随之回收，否则 `TeardownOrder` 会把父项永远挡住——非自愿路径失效。故到期集取
    /// **闭包**：到期持有 ∪ 其全部租约为 None 的后代（递归）。带自身未到期租约的后代不动，
    /// 父项保守等待（子先于父不破）。
    pub fn sweep(&mut self, now: u64) -> Vec<u64> {
        let mut due: Vec<u64> = self
            .live()
            .filter(|h| matches!(h.lease_expires_at, Some(t) if t <= now))
            .map(|h| h.id)
            .collect();
        let mut i = 0;
        while i < due.len() {
            let p = due[i];
            let kids: Vec<u64> = self
                .live()
                .filter(|h| h.parent == Some(p) && h.lease_expires_at.is_none() && !due.contains(&h.id))
                .map(|h| h.id)
                .collect();
            due.extend(kids);
            i += 1;
        }
        let mut order: Vec<(usize, u64, String)> = due
            .iter()
            .map(|id| {
                let h = self.holdings.iter().find(|h| h.id == *id).expect("due id is live");
                (self.depth(*id), *id, h.generation.clone())
            })
            .collect();
        order.sort_by(|a, b| b.0.cmp(&a.0));
        let mut released = Vec::new();
        for (_, id, generation) in order {
            if self.release(id, &generation, now).is_ok() {
                released.push(id);
            }
        }
        released
    }

    /// crash-only 单路径：teardown(主体) 是唯一的回收路径；优雅卸载＝提前调用它。
    /// Children before parents (reverse topological order on the ownership tree).
    pub fn teardown(&mut self, subject: &str, now: u64) -> Vec<u64> {
        let mut mine: Vec<(usize, u64, String)> = self
            .live()
            .filter(|h| h.subject == subject)
            .map(|h| (self.depth(h.id), h.id, h.generation.clone()))
            .collect();
        mine.sort_by(|a, b| b.0.cmp(&a.0));
        let mut plan = Vec::new();
        for (_, id, generation) in mine {
            if self.release(id, &generation, now).is_ok() {
                plan.push(id);
            }
        }
        plan
    }

    /// Global invariant (recomputed, never cached in the drill):
    /// for every (class, instance): ✓(● capacity · ◯ fold(live fragments)).
    pub fn invariant(&self) -> Result<(), LedgerError> {
        for ((class_id, instance), cap) in &self.capacities {
            let frags = self.live_frags(class_id, instance);
            let outstanding = compose_frags(&frags)?;
            if !frag_auth_valid(cap, &outstanding) {
                return Err(LedgerError::Conflict);
            }
        }
        Ok(())
    }

    /// Reconciliation against a substrate view: (instance, generation) pairs alive
    /// underneath. Returns (decayed holdings, untracked substrate entries).
    pub fn reconcile(
        &self,
        class_id: &str,
        substrate: &[(String, String)],
    ) -> (Vec<u64>, Vec<(String, String)>) {
        let decayed = self
            .live()
            .filter(|h| h.class_id == class_id)
            .filter(|h| {
                !substrate
                    .iter()
                    .any(|(i, g)| *i == h.instance && *g == h.generation)
            })
            .map(|h| h.id)
            .collect();
        let untracked = substrate
            .iter()
            .filter(|(i, g)| {
                !self
                    .live()
                    .any(|h| h.class_id == class_id && h.instance == *i && h.generation == *g)
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
            grade: self
                .classes
                .get(&h.class_id)
                .map(|d| d.revert_grade)
                .unwrap_or(RevertGrade::External),
        }
    }

    /// 主体名下的持有快照（含类声明的可逆档）——只看 subject 本人的行。
    pub fn live_snapshot(&self, subject: &str) -> Vec<LiveItem> {
        self.live().filter(|h| h.subject == subject).map(|h| self.live_item(h)).collect()
    }

    /// F2 规划器消费的视图：主体持有的 **ownership 闭包**——主体名下的持有及其全部后代，
    /// 不论后代记在哪个主体名下。
    ///
    /// [B15] plugin-system 裁定 6-7／§5.3：父插件（或 enclosure）死 ⇒ 子插件的持有沿 ownership 树
    /// 拆、子先于父。只按 subject 取快照会让规划器看不见跨主体子项：父项 release 被
    /// `TeardownOrder` 拒绝，执行器把它当作账本不变量破坏而 panic。闭包让"子先于父"对整棵树成立。
    pub fn live_closure(&self, subject: &str) -> Vec<LiveItem> {
        let mut items: Vec<LiveItem> = self.live_snapshot(subject);
        let mut i = 0;
        while i < items.len() {
            let p = items[i].id;
            let kids: Vec<LiveItem> = self
                .live()
                .filter(|h| h.parent == Some(p) && !items.iter().any(|it| it.id == h.id))
                .map(|h| self.live_item(h))
                .collect();
            items.extend(kids);
            i += 1;
        }
        items
    }

    /// 子树闭包：`root` 本人及其全部后代（不论后代记在哪个主体名下）——与
    /// [`live_closure`](Self::live_closure) 同纪律，只沿 ownership 边走。撤销（revoke）
    /// 的清理域：能力入帐本后（WP-03），撤销一项授权＝释放它的持有子树——因该授权
    /// 而存在的东西（池、路由）子先于父收掉，根最后；主体名下的其他持有不动。
    pub fn live_subtree(&self, root: u64) -> Vec<LiveItem> {
        let mut items: Vec<LiveItem> = self
            .live()
            .find(|h| h.id == root)
            .map(|h| self.live_item(h))
            .into_iter()
            .collect();
        let mut i = 0;
        while i < items.len() {
            let p = items[i].id;
            let kids: Vec<LiveItem> = self
                .live()
                .filter(|h| h.parent == Some(p) && !items.iter().any(|it| it.id == h.id))
                .map(|h| self.live_item(h))
                .collect();
            items.extend(kids);
            i += 1;
        }
        items
    }

    pub fn live_count(&self) -> usize {
        self.live().count()
    }
    pub fn tombstone_count(&self) -> usize {
        self.holdings.iter().filter(|h| h.released_at.is_some()).count()
    }
}

/// Deterministic pseudo-random generator for the law tests (no dependencies).
pub struct Lcg(pub u64);
impl Lcg {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}
