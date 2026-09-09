//! 附着与触发器 — drill F8（`.dev/design/attachments-v0.md` v0.5 的行形；用户 2026-09-06 立项）。
//!
//! 一句话：附着＝已入册计划 × 触发器 × standing 同意 × 主体；每次触发＝一个段（裁定四：段＝事务），
//! 三种账本行（附着池 ●／触发碎片 ◯／触发池 ●）用 F1 的行式记账与每实例容量承载，结账只用
//! 受控清理与结算（终态＝段 cleanup、容量置 0、附着存续期花费不退款）。本模块是确定性模拟：没有
//! 计划语言（一次触发"做了什么"由测试给的 [`Run`] 脚本代替解释器），没有时钟
//! （`now` 由调用者推进），崩溃以丢掉易失状态、保留账本及显式关联表表达。
//!
//! 理论标签（对照附着卷 §11 拟决与 §13.3 墓碑；等级：【文献✓】被引原文核实／【推导】我方映射／
//! 【设计】自家文档决定）：
//!
//!   [A1]    附着＝计划×触发器×standing 同意×主体；L 零改动；触发器为挂载点一行【设计＋推导】。
//!   [Q12]   触发次数是池的**独立分量**：合成效应类 `attach::fire`，B_firing 分量＝1、B_total 分量
//!           ＝n_max。零预算（全 Repeatable）计划的 n_max 仍由它封顶【推导，锚 F1 每实例容量】。
//!   [Q13]   结账用 F1/F2 已有的词：触发终态＝段主体 teardown（子先于父），③容量置 0（段拆完后
//!           ③下无存活碎片，置 0 是平凡的 frame-preserving update），②为 `attach/<id>:spent` 下的
//!           花费行在附着存续期保留；不退余额是满额计费政策【设计】。
//!   [Q14]   ②③同一事务；恢复规则：有②无③的 seq 按"已触发、空跑"计（占 n_max，宁紧勿漏）【设计】。
//!   [SEQ]   seq_i 从关联的 fire 池行数取下一值；恢复读 FiringKey→行／池关联，不解码 generation。
//!           nonce_i＝H(h_attach‖seq_i)
//!           可预测但无妨——它是防重放记账不是秘密（Q10）【推导，锚 F1 行式记账契约】。
//!   [EMIT-TX] 用户裁定 2026-09-06：触发 fail-stop 撤回**本次触发自己投递的未消费**事件；已消费的
//!           不可撤（人不能被反通知）；撤回跨主体但有界、可审计【设计，锚裁定四】。
//!   [LEASE] 附着 ttl＝根持有的租约；到期由 `Ledger::sweep` 释放（唯一到期路径，Q9）。触发段与
//!           订阅／路由租约为 None、随父走 ⇒ 裁定三的保守 sweep 不因在途触发等待【推导】。
//!   [ROW]   触发器 row 不含硬清单（位置上无人）；h_table 变 ⇒ 重准入：不通过 Detached、通过
//!           Paused 待重签（Q7）【设计】。
//!   [A4]    每附着串行；min_interval；有界队列＋声明的溢出策略、绝不静默；失败预算＝连续 fail-stop
//!           计数、成功复位；停机后追赶一次记 missed【设计】。
//!   [A6]    detach／到期／撤销终止于同一条 teardown 路径；显式撤销级联（能力未入账本前）【设计】。
//!
//! 法则见 tests/f8_attach.rs（每个测试名＝它执行的定理/纪律）。

use crate::identity::{
    AttachmentId, ClassId, EffectClass, Generation, HoldingHandle, HoldingId, InstanceId,
    ResourceKey, SubjectId,
};
use crate::ledger::GrantRequest;
use crate::ledger::{AlgebraTag, ClassDecl, Frag, Ledger, LedgerError, RevertGrade};
use crate::ra::{Count, Ex};
use crate::registry::{Capacity, Claim, PoolRef};
use crate::test_support::teardown::{AccountingWorld, Journal, LedgerDrill, teardown_handles};
use crate::time::{LeaseDuration, LeaseRequest, Timestamp};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// [Q12] 内核加入的合成效应类：每次触发花费 1，池容量＝n_max。
pub const FIRE_CLASS: &str = "attach::fire";
/// Count pools have opaque instance IDs; attachment/firing maps retain their owners.
pub const CLASS_POOL: &str = "attach/pool";
/// 触发段类（Ex，租约 None）：实例 `<id>#<seq>`，主体 `attach/<id>:seg#<seq>`。
pub const CLASS_SEG: &str = "attach/segment";
/// Topic 触发的基底＝内核订阅持有（按 topic 模式匹配、不绑定发布者，D35）。
pub const CLASS_SUB: &str = "kernel/subscription";
/// Timer 触发的节拍持有（租约 None、随根走；到期由根的租约承载）。
pub const CLASS_TIMER: &str = "kernel/timer";
/// 投递路由（到 user/inbox；detach 只释放它，事件留在收件箱）。
pub const CLASS_ROUTE: &str = "attach/route";
/// 收件箱类：M＝Ex 每消费方一份，ρ＝Inverse（以仍未消费的队列状态为恢复范围；既有观察不撤回）。
pub const CLASS_INBOX: &str = "kernel/inbox";
/// powerbox 主体与其根持有（附着由它实例化，裁定二）。
pub const CLASS_USER_ROOT: &str = "user/root";
pub const USER: &str = "user";
pub const USER_INBOX: &str = "user/inbox";

/// 按效应类 `family::verb` 的 Counting 向量（spec §3.3；缺席类＝0）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Budget(pub BTreeMap<String, u64>);

impl Budget {
    pub fn of(pairs: &[(&str, u64)]) -> Self {
        Budget(pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect())
    }
    pub fn get(&self, class: &str) -> u64 {
        self.0.get(class).copied().unwrap_or(0)
    }
    /// F5 application 规则：`scale(N, body) = numeral(N) ~ body`（逐分量乘）。
    pub fn scale(&self, n: u64) -> Option<Budget> {
        let scaled: Option<BTreeMap<_, _>> = self
            .0
            .iter()
            .map(|(k, v)| v.checked_mul(n).map(|amount| (k.clone(), amount)))
            .collect();
        scaled.map(Budget)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    Timer { period: u64 },
    Topic { topic: String },
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overflow {
    Coalesce,
    DropOldest,
    DropNewest,
    FailStop,
}

/// 附着声明。**规范字节**（签署范围，附着卷 §3.1 注）：除 `grants` 外的全部字段；`grants`
/// 是内核记账（撤销级联用），不入 h_attach。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    pub id: String,
    pub h_plan: String,
    pub trigger: Trigger,
    /// 载荷前置条件：要求完整性可信（Topic 事件带 integrity 标签）。
    pub require_integrity: bool,
    pub budget_firing: Budget,
    pub n_max: u64,
    pub ttl: u64,
    pub h_table: String,
    pub min_interval: u64,
    pub queue_depth: usize,
    pub overflow: Overflow,
    /// 连续 fail-stop 上限 k（Count，成功即复位）。
    pub failure_budget: u64,
    /// 单次运行时长上限（给 ttl_i 用）。
    pub run_cap: u64,
    pub nonce: String,
    /// 所依赖的 standing 授予 id（撤销级联用；不入规范字节）。
    pub grants: Vec<String>,
}

impl Declaration {
    /// h_attach＝规范字节的内容哈希（演练用 FNV-1a 代替 blake3；法则只用它的确定性与唯一性）。
    pub fn h_attach(&self) -> String {
        let canonical = format!(
            "id={};plan={};trigger={:?};pre={};budget={:?};n_max={};ttl={};table={};min={};depth={};overflow={:?};k={};cap={};nonce={}",
            self.id,
            self.h_plan,
            self.trigger,
            self.require_integrity,
            self.budget_firing.0,
            self.n_max,
            self.ttl,
            self.h_table,
            self.min_interval,
            self.queue_depth,
            self.overflow,
            self.failure_budget,
            self.run_cap,
            self.nonce
        );
        fnv(canonical.as_bytes())
    }
}

fn fnv(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// [SEQ] nonce_i＝H(h_attach ‖ seq_i)：确定性派生，可预测但无妨（用途是防重放记账）。
pub fn derive_nonce(h_attach: &str, seq: u64) -> String {
    fnv(format!("{h_attach}|{seq}").as_bytes())
}

/// 派生四元组（F3 `check_quad` 的输入形状：哈希相等 ∧ nonce 新鲜 ∧ ttl 存活）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quad {
    pub h_plan: String,
    pub budget: Budget,
    pub nonce: String,
    pub ttl_expires_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Active,
    PausedFailure,
    PausedTableChanged,
    Expired,
    Detached,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Expired | Status::Detached)
    }
}

/// 触发终态：任何一个都走同一条结账路径（§4.3 结账）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Completed,
    /// [Q14] 恢复规则：有②无③的 seq＝已触发、空跑。
    CompletedEmpty,
    FailStop,
    /// strict 模式下预算越界＝fail-stop（记为 Truncated 以区分来源）。
    Truncated,
    /// 根租约到期，在途触发被 sweep 拆掉。
    Expired,
    /// 用户 detach／撤销时在途触发被中止。
    Aborted,
    /// 崩溃后恢复时发现的在途触发（③已写、未结账）。
    Crashed,
}

impl End {
    /// 计入失败预算的终态（触发失败，非业务失败；到期／中止不算）。
    pub fn counts_as_failure(self) -> bool {
        matches!(self, End::FailStop | End::Truncated | End::Crashed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crash {
    /// ②已写、③未写（无事务时留下第三态；有事务时②回滚）。
    BetweenRows,
    /// ②③已写、触发在途（恢复时按 Crashed 结账）。
    AfterRows,
}

/// 一次触发"做了什么"——解释器的替身（D31：本 crate 不含计划语言）。
#[derive(Clone, Debug)]
pub struct Run {
    /// 依次铸造的效应碎片 (class, n)。
    pub effects: Vec<(String, u64)>,
    /// 依次投递到 user/inbox 的事件数。
    pub emits: u64,
    /// 脚本给定的终态（Completed／FailStop；预算越界会改写为 Truncated）。
    pub end: End,
    pub crash: Option<Crash>,
}

impl Run {
    pub fn ok(effects: &[(&str, u64)], emits: u64) -> Self {
        Run {
            effects: effects.iter().map(|(c, n)| (c.to_string(), *n)).collect(),
            emits,
            end: End::Completed,
            crash: None,
        }
    }
    pub fn fail(effects: &[(&str, u64)], emits: u64) -> Self {
        Run {
            end: End::FailStop,
            ..Run::ok(effects, emits)
        }
    }
    pub fn crash(c: Crash) -> Self {
        Run {
            crash: Some(c),
            ..Run::ok(&[], 0)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxEvent {
    pub id: u64,
    pub attach: String,
    pub seq: u64,
    pub consumed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reject {
    NotActive(Status),
    /// 每附着串行：上一触发未结账。
    Serial,
    MinInterval,
    NoEvent,
    /// 载荷不满足前置条件：拒绝、不铸行、不算失败。
    Precondition,
    /// [Q12] `attach::fire` 分量的闸门拒绝。
    NMax,
    /// 崩溃：调用者须 `recover` 后再继续。
    Crashed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Audit {
    Attached {
        id: String,
        h_attach: String,
    },
    Fired {
        id: String,
        seq: u64,
        nonce: String,
    },
    Settled {
        id: String,
        seq: u64,
        end: End,
    },
    Rejected {
        id: String,
        why: Reject,
    },
    Dropped {
        id: String,
        policy: Overflow,
        dropped: u64,
    },
    Coalesced {
        id: String,
        missed: u64,
    },
    Paused {
        id: String,
        status: Status,
    },
    Resumed {
        id: String,
    },
    Detached {
        id: String,
        why: String,
    },
    Expired {
        id: String,
    },
    RecoveredEmpty {
        id: String,
        seq: u64,
    },
    Withdrawn {
        id: String,
        seq: u64,
        events: u64,
    },
    TableChanged {
        id: String,
        h_table: String,
    },
    Crashed,
    Recovered,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub at: u64,
    pub integrity_ok: bool,
    /// Timer 合并：本事件代表的错过次数。
    pub missed: u64,
}

#[derive(Clone, Debug)]
pub struct FiringRecord {
    pub seq: u64,
    pub nonce: String,
    pub started_at: u64,
    pub end: Option<End>,
}

#[derive(Clone, Debug)]
pub struct AttachState {
    pub decl: Declaration,
    pub h_attach: String,
    pub status: Status,
    pub consecutive_failures: u64,
    pub root: HoldingId,
    pub last_fire_at: Option<u64>,
    pub last_tick: u64,
    pub firings: Vec<FiringRecord>,
}

#[derive(Clone, Debug)]
struct InFlight {
    seq: u64,
    seg_subject: String,
    seg_holding: HoldingId,
    quad: Quad,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachError {
    BudgetOverflow,
    Duplicate,
    Ledger(LedgerError),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FiringKey {
    attachment: AttachmentId,
    sequence: u64,
}
impl FiringKey {
    fn new(id: &str, sequence: u64) -> Self {
        Self {
            attachment: AttachmentId::new(id),
            sequence,
        }
    }
}
struct AttachmentResources {
    root: HoldingHandle,
    pools: BTreeMap<EffectClass, PoolRef<Count>>,
}
struct FiringResources {
    fire: HoldingHandle,
    spent: Vec<HoldingHandle>,
    segment: Option<HoldingHandle>,
    pools: BTreeMap<EffectClass, PoolRef<Count>>,
}

/// Simulated durable state includes the ledger, attachment/firing associations,
/// inbox and audit. Queues and in-flight cursors are volatile.
pub struct Scheduler {
    pub ledger: Ledger,
    pub attachments: BTreeMap<String, AttachState>,
    pub inbox: Vec<InboxEvent>,
    pub audit: Vec<Audit>,
    /// `cached_outstanding`：按池实例缓存的存活碎片合计——只是缓存，真相是行（F1 行式记账契约）。
    pub cached_outstanding: BTreeMap<String, u64>,
    /// [Q14] ②③是否在同一事务里写入。
    pub transactional: bool,
    pub crashed: bool,
    next_event_id: u64,
    next_pool: u64,
    resources: BTreeMap<AttachmentId, AttachmentResources>,
    firing_resources: BTreeMap<FiringKey, FiringResources>,
    queues: BTreeMap<String, VecDeque<Event>>,
    inflight: BTreeMap<String, InFlight>,
}

fn spent_subject(id: &str) -> String {
    format!("attach/{id}:spent")
}
fn attach_subject(id: &str) -> String {
    format!("attach/{id}")
}
fn seg_subject(id: &str, seq: u64) -> String {
    format!("attach/{id}:seg#{seq}")
}
fn seg_instance(id: &str, seq: u64) -> String {
    format!("{id}#{seq}")
}
fn root_class(id: &str) -> String {
    // T（租约）是类的声明参数而附着各有 ttl ⇒ 每附着一个根类声明（类＝声明，含 T）。
    format!("attach/root:{id}")
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Scheduler {
    pub fn new(transactional: bool) -> Self {
        let mut ledger = Ledger::new();
        for (cid, algebra, lease) in [
            (CLASS_POOL, AlgebraTag::Counted, None),
            (CLASS_SEG, AlgebraTag::Exclusive, None),
            (CLASS_SUB, AlgebraTag::Exclusive, None),
            (CLASS_TIMER, AlgebraTag::Exclusive, None),
            (CLASS_ROUTE, AlgebraTag::Exclusive, None),
            (CLASS_INBOX, AlgebraTag::Exclusive, None),
            (CLASS_USER_ROOT, AlgebraTag::Exclusive, None),
        ] {
            ledger
                .register_class(ClassDecl {
                    cleanup: crate::cleanup::CleanupPolicy::AccountingOnly,
                    class_id: cid.into(),
                    algebra,
                    release_idempotent: true,
                    lease_duration: lease.map(|s: u64| LeaseDuration::try_from(s).unwrap()),
                    revert_grade: RevertGrade::Inverse,
                })
                .unwrap();
        }
        // powerbox 主体的根持有与收件箱（归消费方 user，与任何附着的生死无关——Q1）。
        ledger
            .create_pool(
                &ledger
                    .registered_class::<Ex>(&ClassId::new(CLASS_USER_ROOT))
                    .unwrap(),
                InstanceId::new(USER),
                Capacity::new(Ex::Token).unwrap(),
            )
            .unwrap();
        let user_root = ledger
            .grant(
                &ledger
                    .pool::<Ex>(&ResourceKey::new(
                        ClassId::new(CLASS_USER_ROOT),
                        InstanceId::new(USER),
                    ))
                    .unwrap(),
                GrantRequest {
                    owner: SubjectId::new(USER),
                    claim: Claim::new(Ex::Token).unwrap(),
                    generation: Generation::new("g"),
                    parent: None,
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(0u64).unwrap(),
                },
            )
            .map(|h| h.id())
            .unwrap();
        ledger
            .create_pool(
                &ledger
                    .registered_class::<Ex>(&ClassId::new(CLASS_INBOX))
                    .unwrap(),
                InstanceId::new(USER_INBOX),
                Capacity::new(Ex::Token).unwrap(),
            )
            .unwrap();
        ledger
            .grant(
                &ledger
                    .pool::<Ex>(&ResourceKey::new(
                        ClassId::new(CLASS_INBOX),
                        InstanceId::new(USER_INBOX),
                    ))
                    .unwrap(),
                GrantRequest {
                    owner: SubjectId::new(USER),
                    claim: Claim::new(Ex::Token).unwrap(),
                    generation: Generation::new("g"),
                    parent: Some(user_root)
                        .map(|id| ledger.holding(id).expect("parent exists").handle()),
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(0u64).unwrap(),
                },
            )
            .map(|h| h.id())
            .unwrap();
        Scheduler {
            ledger,
            attachments: BTreeMap::new(),
            inbox: Vec::new(),
            audit: Vec::new(),
            cached_outstanding: BTreeMap::new(),
            transactional,
            crashed: false,
            next_event_id: 1,
            next_pool: 0,
            resources: BTreeMap::new(),
            firing_resources: BTreeMap::new(),
            queues: BTreeMap::new(),
            inflight: BTreeMap::new(),
        }
    }

    fn create_budget_pool(&mut self, capacity: u64) -> PoolRef<Count> {
        loop {
            let instance = InstanceId::new(format!("attach-budget:{}", self.next_pool));
            self.next_pool = self
                .next_pool
                .checked_add(1)
                .expect("drill pool IDs exhausted");
            if self
                .ledger
                .capacity(&ResourceKey::new(
                    ClassId::new(CLASS_POOL),
                    instance.clone(),
                ))
                .is_some()
            {
                continue;
            }
            let pool = self
                .ledger
                .create_pool(
                    &self
                        .ledger
                        .registered_class::<Count>(&ClassId::new(CLASS_POOL))
                        .unwrap(),
                    instance,
                    Capacity::new(Count::Value(capacity)).unwrap(),
                )
                .unwrap();
            self.cached_outstanding
                .insert(pool.id().key().instance().to_string(), 0);
            return pool;
        }
    }
    fn total_pool(&self, id: &str, class: &str) -> &PoolRef<Count> {
        &self.resources[&AttachmentId::new(id)].pools[&EffectClass::new(class)]
    }
    fn firing_pool(&self, id: &str, seq: u64, class: &str) -> Option<&PoolRef<Count>> {
        self.firing_resources
            .get(&FiringKey::new(id, seq))?
            .pools
            .get(&EffectClass::new(class))
    }
    pub fn total_spent(&self, id: &str, class: &str) -> u64 {
        let key = self.total_pool(id, class).id().key();
        self.ledger
            .occupying()
            .filter(|h| &h.key() == key)
            .map(|h| match h.frag {
                Frag::Count(Count::Value(n)) => n,
                _ => unreachable!("typed Count pool"),
            })
            .sum()
    }

    fn user_root(&self) -> HoldingId {
        self.ledger
            .active()
            .find(|h| h.class_id.as_str() == CLASS_USER_ROOT)
            .map(|h| h.id)
            .expect("user root")
    }

    /// 附着期：铸附着池①（每类一个池实例，含 `attach::fire`＝n_max）、根持有（租约＝ttl）、
    /// 触发器持有、投递路由；`user` 实例化附着（裁定二：跨主体 parent 沿实例化关系）。
    pub fn attach(&mut self, decl: Declaration, now: u64) -> Result<String, AttachError> {
        assert!(!self.crashed, "recover() first");
        if self.attachments.contains_key(&decl.id) {
            return Err(AttachError::Duplicate);
        }
        let id = decl.id.clone();
        let h_attach = decl.h_attach();
        // ① 附着池：B_total = scale(n_max, B_firing) ⊕ attach::fire = n_max。
        let total = decl
            .budget_firing
            .scale(decl.n_max)
            .ok_or(AttachError::BudgetOverflow)?;
        let mut pools = BTreeMap::new();
        for (class, cap) in total
            .0
            .iter()
            .chain(std::iter::once((&FIRE_CLASS.to_string(), &decl.n_max)))
        {
            pools.insert(EffectClass::new(class), self.create_budget_pool(*cap));
        }
        // 根持有：类＝声明含 T（租约＝ttl）。
        self.ledger
            .register_class(ClassDecl {
                cleanup: crate::cleanup::CleanupPolicy::AccountingOnly,
                class_id: root_class(&id).into(),
                algebra: AlgebraTag::Exclusive,
                release_idempotent: true,
                lease_duration: Some(decl.ttl).map(|s: u64| LeaseDuration::try_from(s).unwrap()),
                revert_grade: RevertGrade::Inverse,
            })
            .unwrap();
        self.ledger
            .create_pool(
                &self
                    .ledger
                    .registered_class::<Ex>(&ClassId::new(&root_class(&id)))
                    .unwrap(),
                InstanceId::new(&id),
                Capacity::new(Ex::Token).unwrap(),
            )
            .unwrap();
        let subject = attach_subject(&id);
        self.ledger
            .declare_instantiation(SubjectId::new(&subject), SubjectId::new(USER));
        let user_root = self.user_root();
        let root = self
            .ledger
            .grant(
                &self
                    .ledger
                    .pool::<Ex>(&ResourceKey::new(
                        ClassId::new(&root_class(&id)),
                        InstanceId::new(&id),
                    ))
                    .unwrap(),
                GrantRequest {
                    owner: SubjectId::new(&subject),
                    claim: Claim::new(Ex::Token).unwrap(),
                    generation: Generation::new(&h_attach),
                    parent: Some(user_root)
                        .map(|id| self.ledger.holding(id).expect("parent exists").handle()),
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(now).unwrap(),
                },
            )
            .map(|h| h.id())
            .map_err(AttachError::Ledger)?;
        self.resources.insert(
            AttachmentId::new(&id),
            AttachmentResources {
                root: self.ledger.holding(root).unwrap().handle(),
                pools,
            },
        );
        // 触发器持有（租约 None、随根走）。
        match &decl.trigger {
            Trigger::Topic { topic } => {
                let inst = format!("{id}/{topic}");
                self.ledger
                    .create_pool(
                        &self
                            .ledger
                            .registered_class::<Ex>(&ClassId::new(CLASS_SUB))
                            .unwrap(),
                        InstanceId::new(&inst),
                        Capacity::new(Ex::Token).unwrap(),
                    )
                    .unwrap();
                self.ledger
                    .grant(
                        &self
                            .ledger
                            .pool::<Ex>(&ResourceKey::new(
                                ClassId::new(CLASS_SUB),
                                InstanceId::new(&inst),
                            ))
                            .unwrap(),
                        GrantRequest {
                            owner: SubjectId::new(&subject),
                            claim: Claim::new(Ex::Token).unwrap(),
                            generation: Generation::new("g"),
                            parent: Some(root)
                                .map(|id| self.ledger.holding(id).expect("parent exists").handle()),
                            lease: LeaseRequest::UseClassDefault,
                            now: Timestamp::try_from(now).unwrap(),
                        },
                    )
                    .map(|h| h.id())
                    .map_err(AttachError::Ledger)?;
            }
            Trigger::Timer { .. } => {
                self.ledger
                    .create_pool(
                        &self
                            .ledger
                            .registered_class::<Ex>(&ClassId::new(CLASS_TIMER))
                            .unwrap(),
                        InstanceId::new(&id),
                        Capacity::new(Ex::Token).unwrap(),
                    )
                    .unwrap();
                self.ledger
                    .grant(
                        &self
                            .ledger
                            .pool::<Ex>(&ResourceKey::new(
                                ClassId::new(CLASS_TIMER),
                                InstanceId::new(&id),
                            ))
                            .unwrap(),
                        GrantRequest {
                            owner: SubjectId::new(&subject),
                            claim: Claim::new(Ex::Token).unwrap(),
                            generation: Generation::new("g"),
                            parent: Some(root)
                                .map(|id| self.ledger.holding(id).expect("parent exists").handle()),
                            lease: LeaseRequest::UseClassDefault,
                            now: Timestamp::try_from(now).unwrap(),
                        },
                    )
                    .map(|h| h.id())
                    .map_err(AttachError::Ledger)?;
            }
            Trigger::Manual => {}
        }
        // 投递路由（到 user/inbox）。
        let route_inst = format!("{id}->{USER_INBOX}");
        self.ledger
            .create_pool(
                &self
                    .ledger
                    .registered_class::<Ex>(&ClassId::new(CLASS_ROUTE))
                    .unwrap(),
                InstanceId::new(&route_inst),
                Capacity::new(Ex::Token).unwrap(),
            )
            .unwrap();
        self.ledger
            .grant(
                &self
                    .ledger
                    .pool::<Ex>(&ResourceKey::new(
                        ClassId::new(CLASS_ROUTE),
                        InstanceId::new(&route_inst),
                    ))
                    .unwrap(),
                GrantRequest {
                    owner: SubjectId::new(&subject),
                    claim: Claim::new(Ex::Token).unwrap(),
                    generation: Generation::new("g"),
                    parent: Some(root)
                        .map(|id| self.ledger.holding(id).expect("parent exists").handle()),
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(now).unwrap(),
                },
            )
            .map(|h| h.id())
            .map_err(AttachError::Ledger)?;
        self.attachments.insert(
            id.clone(),
            AttachState {
                decl,
                h_attach: h_attach.clone(),
                status: Status::Active,
                consecutive_failures: 0,
                root,
                last_fire_at: None,
                last_tick: now,
                firings: Vec::new(),
            },
        );
        self.queues.insert(id.clone(), VecDeque::new());
        self.audit.push(Audit::Attached {
            id: id.clone(),
            h_attach,
        });
        Ok(id)
    }

    // ------------------------------------------------------------------
    // 事件与队列（A4）
    // ------------------------------------------------------------------

    /// Topic 事件到达：进有界队列，溢出按声明，**每次丢弃都入审计**。
    pub fn event(&mut self, topic: &str, integrity_ok: bool, now: u64) {
        assert!(!self.crashed, "recover() first");
        let ids: Vec<String> = self
            .attachments
            .values()
            .filter(|a| matches!(&a.decl.trigger, Trigger::Topic { topic: t } if t == topic))
            .filter(|a| !a.status.is_terminal())
            .map(|a| a.decl.id.clone())
            .collect();
        for id in ids {
            self.enqueue(
                &id,
                Event {
                    at: now,
                    integrity_ok,
                    missed: 0,
                },
            );
        }
    }

    fn enqueue(&mut self, id: &str, ev: Event) {
        let (depth, policy) = {
            let a = &self.attachments[id];
            (a.decl.queue_depth, a.decl.overflow)
        };
        let q = self.queues.entry(id.to_string()).or_default();
        if q.len() < depth.max(1) {
            q.push_back(ev);
            return;
        }
        match policy {
            Overflow::Coalesce => {
                // 合并进队尾那一条：记 missed，不静默。
                if let Some(last) = q.back_mut() {
                    last.missed += 1 + ev.missed;
                    last.at = ev.at;
                }
                self.audit.push(Audit::Coalesced {
                    id: id.to_string(),
                    missed: 1,
                });
            }
            Overflow::DropOldest => {
                q.pop_front();
                q.push_back(ev);
                self.audit.push(Audit::Dropped {
                    id: id.to_string(),
                    policy,
                    dropped: 1,
                });
            }
            Overflow::DropNewest => {
                self.audit.push(Audit::Dropped {
                    id: id.to_string(),
                    policy,
                    dropped: 1,
                });
            }
            Overflow::FailStop => {
                self.audit.push(Audit::Dropped {
                    id: id.to_string(),
                    policy,
                    dropped: 1,
                });
                self.pause(id, Status::PausedFailure);
            }
        }
    }

    fn pause(&mut self, id: &str, status: Status) {
        if let Some(a) = self.attachments.get_mut(id) {
            if !a.status.is_terminal() && a.status != status {
                a.status = status;
                self.audit.push(Audit::Paused {
                    id: id.to_string(),
                    status,
                });
            }
        }
    }

    pub fn queue_len(&self, id: &str) -> usize {
        self.queues.get(id).map(|q| q.len()).unwrap_or(0)
    }

    /// 推进时钟：租约 sweep（唯一到期路径，[LEASE]）＋ Timer 节拍入队。
    pub fn tick(&mut self, now: u64) {
        assert!(!self.crashed, "recover() first");
        let released = self.ledger.sweep(Timestamp::try_from(now).unwrap());
        let expired: Vec<String> = self
            .attachments
            .values()
            .filter(|a| !a.status.is_terminal() && released.contains(&a.root))
            .map(|a| a.decl.id.clone())
            .collect();
        for id in expired {
            self.retire(&id, Status::Expired, End::Expired, "lease expired", now);
        }
        let timers: Vec<(String, u64)> = self
            .attachments
            .values()
            .filter(|a| !a.status.is_terminal())
            .filter_map(|a| match a.decl.trigger {
                Trigger::Timer { period } => Some((a.decl.id.clone(), period)),
                _ => None,
            })
            .collect();
        for (id, period) in timers {
            let due = {
                let a = self.attachments.get_mut(&id).unwrap();
                if now >= a.last_tick + period {
                    a.last_tick = now;
                    true
                } else {
                    false
                }
            };
            if due {
                self.enqueue(
                    &id,
                    Event {
                        at: now,
                        integrity_ok: true,
                        missed: 0,
                    },
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // 触发（三种行、派生四元组、段、结账）
    // ------------------------------------------------------------------

    /// [SEQ] seq＝该附着 `attach::fire` 存活碎片行数＋1（行为真相；内存计数不算数）。
    pub fn next_seq(&self, id: &str) -> u64 {
        self.fired_count(id) + 1
    }

    /// [Q12] 已触发次数＝存活 fire 碎片行数（含恢复为空跑的）。
    pub fn fired_count(&self, id: &str) -> u64 {
        let Some(resources) = self.resources.get(&AttachmentId::new(id)) else {
            return 0;
        };
        let key = resources.pools[&EffectClass::new(FIRE_CLASS)].id().key();
        self.ledger.active().filter(|h| &h.key() == key).count() as u64
    }

    /// 一次触发＝`begin`（消费事件、前置条件、读 seq、铸②、同事务铸③＋段持有、派生四元组）
    /// ＋ `finish`（执行脚本、结账）。分开暴露是为了让"在途"成为一等状态：到期、detach、
    /// 消费方读收件箱、崩溃都可能发生在两者之间。
    pub fn fire(&mut self, id: &str, run: &Run, now: u64) -> Result<u64, Reject> {
        self.begin_with(id, now, run.crash)?;
        self.finish(id, run, now)
    }

    pub fn begin(&mut self, id: &str, now: u64) -> Result<u64, Reject> {
        self.begin_with(id, now, None)
    }

    fn begin_with(&mut self, id: &str, now: u64, crash: Option<Crash>) -> Result<u64, Reject> {
        assert!(!self.crashed, "recover() first");
        let (status, trigger, min_interval, last_fire, require_integrity) =
            match self.attachments.get(id) {
                Some(a) => (
                    a.status,
                    a.decl.trigger.clone(),
                    a.decl.min_interval,
                    a.last_fire_at,
                    a.decl.require_integrity,
                ),
                None => return Err(Reject::Unknown),
            };
        let reject = |me: &mut Self, why: Reject| {
            me.audit.push(Audit::Rejected {
                id: id.to_string(),
                why: why.clone(),
            });
            Err(why)
        };
        if status != Status::Active {
            return reject(self, Reject::NotActive(status));
        }
        if self.inflight.contains_key(id) {
            return reject(self, Reject::Serial);
        }
        if let Some(t) = last_fire {
            if now < t + min_interval {
                return reject(self, Reject::MinInterval);
            }
        }
        if trigger != Trigger::Manual {
            let ev = match self.queues.get_mut(id).and_then(|q| q.pop_front()) {
                Some(ev) => ev,
                None => return reject(self, Reject::NoEvent),
            };
            if require_integrity && !ev.integrity_ok {
                return reject(self, Reject::Precondition);
            }
        }
        // ---- ② 触发碎片：fire 分量先过闸门（＝n_max），其余分量按 B_firing 满额记 ----
        let seq = self.next_seq(id);
        let spent = spent_subject(id);
        let generation = format!("seq:{seq}");
        let fire_row = match self
            .ledger
            .grant(
                &self.total_pool(id, FIRE_CLASS).clone(),
                GrantRequest {
                    owner: SubjectId::new(&spent),
                    claim: Claim::new(Count::Value(1)).unwrap(),
                    generation: Generation::new(&generation),
                    parent: None,
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(now).unwrap(),
                },
            )
            .map(|h| h.id())
        {
            Ok(h) => h,
            Err(LedgerError::Conflict) => return reject(self, Reject::NMax),
            Err(e) => panic!("fire row: {e:?}"),
        };
        let budget = self.attachments[id].decl.budget_firing.clone();
        let mut spend_rows = vec![fire_row];
        for (class, n) in budget.0.iter().filter(|(_, n)| **n > 0) {
            let h = self.ledger.grant(&self.total_pool(id, class).clone(), GrantRequest { owner: SubjectId::new(&spent), claim: Claim::new(Count::Value(*n)).unwrap(), generation: Generation::new(&generation), parent: None, lease: LeaseRequest::UseClassDefault, now: Timestamp::try_from(now).unwrap() }).map(|h| h.id())
                .expect("B_total = scale(n_max, B_firing): a class pool cannot run out before the fire pool");
            spend_rows.push(h);
        }
        let firing_key = FiringKey::new(id, seq);
        self.firing_resources.insert(
            firing_key.clone(),
            FiringResources {
                fire: self.ledger.holding(fire_row).unwrap().handle(),
                spent: spend_rows
                    .iter()
                    .map(|id| self.ledger.holding(*id).unwrap().handle())
                    .collect(),
                segment: None,
                pools: BTreeMap::new(),
            },
        );
        if crash == Some(Crash::BetweenRows) {
            // [Q14] 崩在②③之间。
            if self.transactional {
                // 事务未提交：②不存在（演练以释放表达——墓碑不计存活、不计 seq）。
                for h in spend_rows {
                    self.ledger
                        .release(
                            &HoldingHandle::new(h, Generation::new(&generation)),
                            Timestamp::try_from(now).unwrap(),
                        )
                        .unwrap();
                }
                self.firing_resources.remove(&firing_key);
            } else {
                // 逐行写入：②留在盘上，③没有——第三态，交给恢复规则。
                for (class, n) in budget.0.iter().filter(|(_, n)| **n > 0) {
                    *self
                        .cached_outstanding
                        .entry(self.total_pool(id, class).id().key().instance().to_string())
                        .or_insert(0) += n;
                }
                *self
                    .cached_outstanding
                    .entry(
                        self.total_pool(id, FIRE_CLASS)
                            .id()
                            .key()
                            .instance()
                            .to_string(),
                    )
                    .or_insert(0) += 1;
            }
            self.crash();
            return Err(Reject::Crashed);
        }
        for (class, n) in budget.0.iter().filter(|(_, n)| **n > 0) {
            *self
                .cached_outstanding
                .entry(self.total_pool(id, class).id().key().instance().to_string())
                .or_insert(0) += n;
        }
        *self
            .cached_outstanding
            .entry(
                self.total_pool(id, FIRE_CLASS)
                    .id()
                    .key()
                    .instance()
                    .to_string(),
            )
            .or_insert(0) += 1;
        // ③ and the durable association are created with the same simulated commit.
        for (class, n) in budget.0.iter().filter(|(_, n)| **n > 0) {
            let pool = self.create_budget_pool(*n);
            self.firing_resources
                .get_mut(&firing_key)
                .unwrap()
                .pools
                .insert(EffectClass::new(class), pool);
        }
        let h_attach = self.attachments[id].h_attach.clone();
        let nonce = derive_nonce(&h_attach, seq);
        let seg = seg_subject(id, seq);
        let root = self.attachments[id].root;
        self.ledger
            .declare_instantiation(SubjectId::new(&seg), SubjectId::new(&attach_subject(id)));
        self.ledger
            .create_pool(
                &self
                    .ledger
                    .registered_class::<Ex>(&ClassId::new(CLASS_SEG))
                    .unwrap(),
                InstanceId::new(&seg_instance(id, seq)),
                Capacity::new(Ex::Token).unwrap(),
            )
            .unwrap();
        let seg_holding = self
            .ledger
            .grant(
                &self
                    .ledger
                    .pool::<Ex>(&ResourceKey::new(
                        ClassId::new(CLASS_SEG),
                        InstanceId::new(&seg_instance(id, seq)),
                    ))
                    .unwrap(),
                GrantRequest {
                    owner: SubjectId::new(&seg),
                    claim: Claim::new(Ex::Token).unwrap(),
                    generation: Generation::new(&nonce),
                    parent: Some(root)
                        .map(|id| self.ledger.holding(id).expect("parent exists").handle()),
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(now).unwrap(),
                },
            )
            .map(|h| h.id())
            .expect("segment row");
        self.firing_resources.get_mut(&firing_key).unwrap().segment =
            Some(self.ledger.holding(seg_holding).unwrap().handle());
        // ---- 派生四元组：ttl_i = min(now + run_cap, 根租约到期) ----
        let lease_end = self
            .ledger
            .holding(root)
            .and_then(|h| h.lease.expires_at().map(|t| t.get()))
            .unwrap_or(u64::MAX);
        let run_cap = self.attachments[id].decl.run_cap;
        let quad = Quad {
            h_plan: self.attachments[id].decl.h_plan.clone(),
            budget: budget.clone(),
            nonce: nonce.clone(),
            ttl_expires_at: now.saturating_add(run_cap).min(lease_end),
        };
        {
            let a = self.attachments.get_mut(id).unwrap();
            a.firings.push(FiringRecord {
                seq,
                nonce: nonce.clone(),
                started_at: now,
                end: None,
            });
            a.last_fire_at = Some(now);
        }
        self.inflight.insert(
            id.to_string(),
            InFlight {
                seq,
                seg_subject: seg,
                seg_holding,
                quad,
            },
        );
        self.audit.push(Audit::Fired {
            id: id.to_string(),
            seq,
            nonce,
        });
        if crash == Some(Crash::AfterRows) {
            self.crash();
            return Err(Reject::Crashed);
        }
        Ok(seq)
    }

    /// 在途触发向 user/inbox 投递一条事件（带 (attach, seq)，供有界撤回）。
    pub fn emit(&mut self, id: &str) -> Result<u64, Reject> {
        assert!(!self.crashed, "recover() first");
        let seq = match self.inflight.get(id) {
            Some(f) => f.seq,
            None => return Err(Reject::NoEvent),
        };
        let eid = self.next_event_id;
        self.next_event_id += 1;
        self.inbox.push(InboxEvent {
            id: eid,
            attach: id.to_string(),
            seq,
            consumed: false,
        });
        Ok(eid)
    }

    /// 执行脚本并结账：效应碎片经③的闸门（strict：越界即 fail-stop）；`run.emits` 条事件；终态。
    pub fn finish(&mut self, id: &str, run: &Run, now: u64) -> Result<u64, Reject> {
        assert!(!self.crashed, "recover() first");
        let (seq, seg, seg_holding) = match self.inflight.get(id) {
            Some(f) => (f.seq, f.seg_subject.clone(), f.seg_holding),
            None => return Err(Reject::NoEvent),
        };
        let mut end = run.end;
        for (class, n) in &run.effects {
            let r = self
                .ledger
                .grant(
                    &self
                        .firing_pool(id, seq, class)
                        .expect("admitted effect pool")
                        .clone(),
                    GrantRequest {
                        owner: SubjectId::new(&seg),
                        claim: Claim::new(Count::Value(*n)).unwrap(),
                        generation: Generation::new("eff"),
                        parent: Some(seg_holding)
                            .map(|id| self.ledger.holding(id).expect("parent exists").handle()),
                        lease: LeaseRequest::UseClassDefault,
                        now: Timestamp::try_from(now).unwrap(),
                    },
                )
                .map(|h| h.id());
            match r {
                Ok(_) => {
                    *self
                        .cached_outstanding
                        .entry(
                            self.firing_pool(id, seq, class)
                                .unwrap()
                                .id()
                                .key()
                                .instance()
                                .to_string(),
                        )
                        .or_insert(0) += n;
                }
                Err(_) => {
                    end = End::Truncated;
                    break;
                }
            }
        }
        if !matches!(end, End::Truncated) {
            for _ in 0..run.emits {
                self.emit(id)?;
            }
        }
        self.settle(id, seq, end, now);
        Ok(seq)
    }

    /// 结账（§4.3；一条路径，任何终态）：撤回本次触发自己投递的未消费事件（非 commit 终态，[EMIT-TX]）
    /// → 段主体 F2 teardown（子先于父）→ ③容量置 0 → 失败预算 → 记录。
    fn settle(&mut self, id: &str, seq: u64, end: End, now: u64) {
        if let Some(f) = self.inflight.remove(id) {
            debug_assert_eq!(f.seq, seq);
        }
        if !matches!(end, End::Completed | End::CompletedEmpty) {
            self.withdraw(id, seq);
        }
        if let Some(segment) = self
            .firing_resources
            .get(&FiringKey::new(id, seq))
            .and_then(|r| r.segment.clone())
        {
            teardown_handles(
                &mut self.ledger,
                &mut Journal::default(),
                &mut AccountingWorld,
                &[segment],
                Timestamp::try_from(now).unwrap(),
            );
        }
        let pools: Vec<_> = self
            .firing_resources
            .get(&FiringKey::new(id, seq))
            .map(|r| r.pools.values().cloned().collect())
            .unwrap_or_default();
        for pool in pools {
            self.ledger
                .settle_and_zero_pool(&pool, Timestamp::try_from(now).unwrap())
                .unwrap();
        }
        self.cached_outstanding = self.recompute_outstanding();
        let paused = {
            let a = self.attachments.get_mut(id).unwrap();
            if let Some(r) = a.firings.iter_mut().find(|r| r.seq == seq) {
                r.end = Some(end);
            } else {
                a.firings.push(FiringRecord {
                    seq,
                    nonce: derive_nonce(&a.h_attach, seq),
                    started_at: now,
                    end: Some(end),
                });
            }
            if end.counts_as_failure() {
                a.consecutive_failures += 1;
                a.consecutive_failures >= a.decl.failure_budget && !a.status.is_terminal()
            } else {
                if matches!(end, End::Completed | End::CompletedEmpty) {
                    a.consecutive_failures = 0;
                }
                false
            }
        };
        self.audit.push(Audit::Settled {
            id: id.to_string(),
            seq,
            end,
        });
        if paused {
            self.pause(id, Status::PausedFailure);
        }
    }

    /// [EMIT-TX] 撤回只及**本次触发自己投递的未消费**事件；已消费的留下（人不能被反通知）。
    fn withdraw(&mut self, id: &str, seq: u64) {
        let before = self.inbox.len();
        self.inbox
            .retain(|e| !(e.attach == id && e.seq == seq && !e.consumed));
        let n = (before - self.inbox.len()) as u64;
        if n > 0 {
            self.audit.push(Audit::Withdrawn {
                id: id.to_string(),
                seq,
                events: n,
            });
        }
    }

    /// 消费方 pop（Consuming/Compensable{requeue}）：已消费即不可撤回。
    pub fn consume(&mut self, event_id: u64) -> bool {
        match self.inbox.iter_mut().find(|e| e.id == event_id) {
            Some(e) if !e.consumed => {
                e.consumed = true;
                true
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------------
    // 退役：detach／到期／撤销 → 同一条 teardown 路径（A6）
    // ------------------------------------------------------------------

    pub fn detach(&mut self, id: &str, now: u64) {
        assert!(!self.crashed, "recover() first");
        self.retire(id, Status::Detached, End::Aborted, "user", now);
    }

    /// [A6] 显式撤销级联（能力未入账本前的内核固定算法）：先拆依赖该授予的附着，再完成吊销。
    pub fn revoke(&mut self, grant_id: &str, now: u64) -> Vec<String> {
        assert!(!self.crashed, "recover() first");
        let ids: Vec<String> = self
            .attachments
            .values()
            .filter(|a| !a.status.is_terminal() && a.decl.grants.iter().any(|g| g == grant_id))
            .map(|a| a.decl.id.clone())
            .collect();
        for id in &ids {
            self.retire(
                id,
                Status::Detached,
                End::Aborted,
                &format!("revoked:{grant_id}"),
                now,
            );
        }
        ids
    }

    fn retire(&mut self, id: &str, status: Status, inflight_end: End, why: &str, now: u64) {
        if self
            .attachments
            .get(id)
            .map(|a| a.status.is_terminal())
            .unwrap_or(true)
        {
            return;
        }
        if let Some(f) = self.inflight.get(id).cloned() {
            self.settle(id, f.seq, inflight_end, now);
        }
        // 段主体若仍有行（崩溃遗留），一并拆。
        let seqs: Vec<u64> = self.attachments[id].firings.iter().map(|r| r.seq).collect();
        for seq in seqs {
            let pending = self
                .firing_resources
                .get(&FiringKey::new(id, seq))
                .and_then(|r| r.segment.as_ref())
                .and_then(|h| self.ledger.holding(h.id()))
                .is_some_and(|h| h.state.occupies());
            if pending {
                self.settle(id, seq, inflight_end, now);
            }
        }
        let root = self.resources[&AttachmentId::new(id)].root.clone();
        teardown_handles(
            &mut self.ledger,
            &mut Journal::default(),
            &mut AccountingWorld,
            &[root],
            Timestamp::try_from(now).unwrap(),
        );
        let mut pools: Vec<_> = self.resources[&AttachmentId::new(id)]
            .pools
            .values()
            .cloned()
            .collect();
        pools.extend(
            self.firing_resources
                .iter()
                .filter(|(key, _)| key.attachment == AttachmentId::new(id))
                .flat_map(|(_, r)| r.pools.values().cloned()),
        );
        for pool in pools {
            self.ledger
                .settle_and_zero_pool(&pool, Timestamp::try_from(now).unwrap())
                .unwrap();
        }
        self.cached_outstanding = self.recompute_outstanding();
        self.queues.remove(id);
        let a = self.attachments.get_mut(id).unwrap();
        a.status = status;
        match status {
            Status::Expired => self.audit.push(Audit::Expired { id: id.to_string() }),
            _ => self.audit.push(Audit::Detached {
                id: id.to_string(),
                why: why.to_string(),
            }),
        }
    }

    // ------------------------------------------------------------------
    // 策略表变更（Q7）、暂停／恢复
    // ------------------------------------------------------------------

    /// h_table 变 ⇒ 对每个未退役附着重跑准入：不通过 ⇒ Detached；通过 ⇒ Paused 待重签。
    pub fn table_change(
        &mut self,
        new_h_table: &str,
        admits: &dyn Fn(&Declaration) -> bool,
        now: u64,
    ) {
        assert!(!self.crashed, "recover() first");
        let ids: Vec<String> = self
            .attachments
            .values()
            .filter(|a| !a.status.is_terminal())
            .map(|a| a.decl.id.clone())
            .collect();
        for id in ids {
            let ok = admits(&self.attachments[&id].decl);
            if ok {
                self.audit.push(Audit::TableChanged {
                    id: id.clone(),
                    h_table: new_h_table.to_string(),
                });
                self.pause(&id, Status::PausedTableChanged);
            } else {
                self.retire(&id, Status::Detached, End::Aborted, "table rejected", now);
            }
        }
    }

    /// 重签：新 h_table 进规范字节 ⇒ 新 h_attach；同一附着身份。
    pub fn resign(&mut self, id: &str, new_h_table: &str) -> bool {
        match self.attachments.get_mut(id) {
            Some(a) if a.status == Status::PausedTableChanged => {
                a.decl.h_table = new_h_table.to_string();
                a.h_attach = a.decl.h_attach();
                a.status = Status::Active;
                self.audit.push(Audit::Resumed { id: id.to_string() });
                true
            }
            _ => false,
        }
    }

    /// powerbox 恢复失败暂停。
    pub fn resume(&mut self, id: &str) -> bool {
        match self.attachments.get_mut(id) {
            Some(a) if a.status == Status::PausedFailure => {
                a.status = Status::Active;
                a.consecutive_failures = 0;
                self.audit.push(Audit::Resumed { id: id.to_string() });
                true
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------------
    // 崩溃与恢复（crash-only：持久＝账本行＋附着表＋收件箱＋审计；易失＝队列、在途段）
    // ------------------------------------------------------------------

    pub fn crash(&mut self) {
        self.crashed = true;
        self.queues.clear();
        self.inflight.clear();
        self.audit.push(Audit::Crashed);
    }

    /// 重启对账：① 有②无③的 seq 按空跑计（[Q14]）；② 有③（段行存活）的按 Crashed 结账；
    /// ③ 缓存从行重算；④ Timer 只追赶一次、记 missed。
    pub fn recover(&mut self, now: u64) {
        self.crashed = false;
        self.audit.push(Audit::Recovered);
        let ids: Vec<String> = self.attachments.keys().cloned().collect();
        for id in ids {
            let seqs: Vec<_> = self
                .firing_resources
                .iter()
                .filter(|(k, r)| {
                    k.attachment == AttachmentId::new(&id)
                        && self
                            .ledger
                            .holding(r.fire.id())
                            .is_some_and(|h| h.state.is_active())
                })
                .map(|(k, _)| k.sequence)
                .collect();
            for seq in seqs {
                let seg_row = self.firing_resources[&FiringKey::new(&id, seq)]
                    .segment
                    .as_ref()
                    .and_then(|h| self.ledger.holding(h.id()))
                    .cloned();
                match seg_row {
                    None => {
                        // 有②无③：已触发、空跑。
                        let known = self.attachments[&id].firings.iter().any(|r| r.seq == seq);
                        if !known {
                            let h_attach = self.attachments[&id].h_attach.clone();
                            self.attachments
                                .get_mut(&id)
                                .unwrap()
                                .firings
                                .push(FiringRecord {
                                    seq,
                                    nonce: derive_nonce(&h_attach, seq),
                                    started_at: now,
                                    end: Some(End::CompletedEmpty),
                                });
                            self.audit.push(Audit::RecoveredEmpty {
                                id: id.clone(),
                                seq,
                            });
                        }
                    }
                    Some(row) if row.released_at().is_none() => {
                        // 在途触发：③已写、未结账 ⇒ Crashed 结账（撤回、拆段、容量置 0）。
                        if !self.attachments[&id].status.is_terminal() {
                            self.settle(&id, seq, End::Crashed, now);
                        }
                    }
                    Some(_) => {}
                }
            }
            self.queues.entry(id.clone()).or_default();
            // Timer 追赶一次：合并为一条事件，记 missed。
            let (period, active, last_tick) = {
                let a = &self.attachments[&id];
                (
                    match a.decl.trigger {
                        Trigger::Timer { period } => Some(period),
                        _ => None,
                    },
                    a.status == Status::Active,
                    a.last_tick,
                )
            };
            if let (Some(period), true) = (period, active) {
                if period > 0 && now >= last_tick + period {
                    let elapsed = (now - last_tick) / period;
                    self.attachments.get_mut(&id).unwrap().last_tick = now;
                    self.enqueue(
                        &id,
                        Event {
                            at: now,
                            integrity_ok: true,
                            missed: elapsed - 1,
                        },
                    );
                    self.audit.push(Audit::Coalesced {
                        id: id.clone(),
                        missed: elapsed - 1,
                    });
                }
            }
        }
        self.cached_outstanding = self.recompute_outstanding();
    }

    // ------------------------------------------------------------------
    // 真相与缓存（F1 行式记账契约）
    // ------------------------------------------------------------------

    /// outstanding 的真相：按池实例折叠存活碎片行。
    pub fn recompute_outstanding(&self) -> BTreeMap<String, u64> {
        let mut out: BTreeMap<String, u64> = BTreeMap::new();
        for ((class, inst), _) in self.ledger_capacities() {
            if class == CLASS_POOL {
                out.insert(inst, 0);
            }
        }
        for h in self
            .ledger
            .active()
            .filter(|h| h.class_id.as_str() == CLASS_POOL)
        {
            if let Frag::Count(Count::Value(n)) = h.frag {
                *out.entry(h.instance.to_string()).or_insert(0) += n;
            }
        }
        out
    }

    fn ledger_capacities(&self) -> Vec<((String, String), Frag)> {
        self.ledger
            .snapshot()
            .pools
            .into_iter()
            .map(|p| {
                (
                    (p.key.class().to_string(), p.key.instance().to_string()),
                    p.capacity,
                )
            })
            .collect()
    }

    /// 触发池是否已关闭（容量置 0 ⇒ `can_mint` 对任何正值拒）。
    pub fn firing_pool_closed(&self, id: &str, seq: u64, class: &str) -> bool {
        self.firing_pool(id, seq, class).is_some_and(|pool| {
            self.ledger.capacity(pool.id().key()) == Some(&Frag::Count(Count::Value(0)))
        })
    }
    pub fn firing_pool_refuses(&self, id: &str, seq: u64, class: &str) -> bool {
        let Some(pool) = self.firing_pool(id, seq, class) else {
            return true;
        };
        let Some(Frag::Count(cap)) = self.ledger.capacity(pool.id().key()) else {
            return true;
        };
        let live: Vec<_> = self
            .ledger
            .occupying()
            .filter(|h| &h.key() == pool.id().key())
            .filter_map(|h| match h.frag {
                Frag::Count(c) => Some(c),
                _ => None,
            })
            .collect();
        !crate::auth::can_mint(cap, &live, &Count::Value(1))
    }

    pub fn nonces(&self, id: &str) -> Vec<String> {
        self.attachments[id]
            .firings
            .iter()
            .map(|r| r.nonce.clone())
            .collect()
    }

    pub fn status(&self, id: &str) -> Status {
        self.attachments[id].status
    }

    pub fn inflight_seq(&self, id: &str) -> Option<u64> {
        self.inflight.get(id).map(|f| f.seq)
    }

    pub fn quad(&self, id: &str) -> Option<&Quad> {
        self.inflight.get(id).map(|f| &f.quad)
    }

    pub fn unconsumed_events(&self, id: &str) -> Vec<u64> {
        self.inbox
            .iter()
            .filter(|e| e.attach == id && !e.consumed)
            .map(|e| e.id)
            .collect()
    }

    /// 全局不变式（每步重算，绝不信缓存）：账本 Auth 不变式；缓存＝折叠；已触发 ≤ n_max；
    /// nonce 唯一；已结账触发的③已关闭；退役附着名下无存活行、①容量为 0。
    pub fn invariants(&self) -> Result<(), String> {
        self.ledger
            .invariant()
            .map_err(|e| format!("ledger invariant: {e:?}"))?;
        let truth = self.recompute_outstanding();
        for (inst, n) in &truth {
            let c = self.cached_outstanding.get(inst).copied().unwrap_or(0);
            if c != *n {
                return Err(format!("cache {inst}: cached {c} != recomputed {n}"));
            }
        }
        for (inst, c) in &self.cached_outstanding {
            if truth.get(inst).copied().unwrap_or(0) != *c {
                return Err(format!(
                    "cache {inst}: cached {c} but truth {:?}",
                    truth.get(inst)
                ));
            }
        }
        for a in self.attachments.values() {
            let id = &a.decl.id;
            if self.fired_count(id) > a.decl.n_max {
                return Err(format!(
                    "{id}: fired {} > n_max {}",
                    self.fired_count(id),
                    a.decl.n_max
                ));
            }
            let mut seen = BTreeSet::new();
            for r in &a.firings {
                if !seen.insert(r.nonce.clone()) {
                    return Err(format!("{id}: duplicate nonce at seq {}", r.seq));
                }
                if r.end.is_some() && self.inflight.get(id).map(|f| f.seq) != Some(r.seq) {
                    for class in a.decl.budget_firing.0.keys() {
                        if !self.firing_pool_refuses(id, r.seq, class) {
                            return Err(format!(
                                "{id}: settled seq {} pool {class} still mints",
                                r.seq
                            ));
                        }
                    }
                }
            }
            if a.status.is_terminal() {
                let resources = &self.resources[&AttachmentId::new(id)];
                let mut handles = vec![resources.root.clone()];
                for (_, r) in self
                    .firing_resources
                    .iter()
                    .filter(|(k, _)| k.attachment == AttachmentId::new(id))
                {
                    handles.extend(r.spent.clone());
                    handles.extend(r.segment.clone());
                }
                if handles
                    .iter()
                    .any(|h| !self.ledger.occupying_subtree(h.id()).is_empty())
                {
                    return Err(format!("{id}: terminal but associated rows remain"));
                }
                for pool in resources.pools.values() {
                    if self.ledger.capacity(pool.id().key()) != Some(&Frag::Count(Count::Value(0)))
                    {
                        return Err(format!("{id}: terminal but associated pool is not closed"));
                    }
                }
            }
        }
        // 收件箱不属于任何附着：退役后已投递事件仍在。
        Ok(())
    }
}
