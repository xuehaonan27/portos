//! 动词真理表 — freeze drill F4（v2：同日对抗走查后重铸）。
//!
//! 一句话：这张表是**注册期一次性声明**，F2 的 teardown 档与 F3 的 monitor 分级
//! 都是它的投影。"共享真理表"（endstate §3.6）的"共享"就是这个意思——资源模型
//! （F2）与效应计划（F3）读同一张表，而非各自声明。
//!
//! v1→v2 的三处重铸（见 freeze-f4 §2；每处都是理论列逼出来的）：
//!   B5 持有的 ρ 档（类↔世界关系，F2 用）与动作的世界档（补偿性，F3 用）是两栏，
//!      v1 混成一栏并强制同类一致，导致 page 类无法注册 click/submit。v2 拆开：
//!      类级 `holding_rho`（declare_class）+ 动词级 `Kind` 内嵌世界档。
//!   B6 投影必须按 handler（D1）：v1 的裸动词集投影把 mail.send 与 bus.send 混同。
//!      v2 只提供按类投影 `derive_handler_policy(class)`，裸动词投影 API 不存在。
//!   B7 staged ≠ 逐次扣发：外部发射的两阶段（reserve→同意→commit）在**可摊销**时
//!      塌缩进准入（铸池＝reserve、四元组＝同意、发射＝commit）；逐次扣发只对
//!      **不可摊销**（effect-plan §5.5 硬清单）成立。v1 把每个 click 都送去等批。
//!
//! 理论标签（对照 design/freeze-f4-verbtable.md 三列表；措辞纪律：不得强于被引处；
//! 等级：【文献✓】被引原文核实／【推导】我方映射／【设计】自家文档决定）：
//!
//!   [PP]    效应＝操作＋等式（Plotkin–Power；theory-spec §2.3 采纳）【文献✓】。
//!           本表即资源类 Σ/E 的查表形态。
//!   [D1]    位置判据（theory-spec §1.2；Plotkin–Pretnar "含义由 handler 赋予"）【文献✓】：
//!           动词性格随 (handler, 动词) 对——表以 (class, verb) 为键，投影按类。
//!   [TRI]   动词三分类 可重复/消耗/发射（endstate §8.3）【设计＋推导】：
//!           可重复＝不动世界、盲重放安全；消耗＝新的调用消耗资源，重试可凭操作标识去重；发射＝w-effect，
//!           是影子、不入 RA。
//!   [ρ]     持有的可逆档 ρ 是**类**对世界的关系（theory-spec §2.5：release∘acquire≈id
//!           在哪个等价下成立）【文献✓（TISSEC 2.5 警告）＋推导】——注册期固定、
//!           同类唯一、无 setter。它回答"持有怎么还"，不回答"动作能否补偿"。
//!   [WG]    动作的世界档（effect-plan §7-2：效应类型可选注册补偿动作＝saga）【设计】：
//!           Held（世界变更物化为持有，回收由类 ρ 承载）／Compensable{补偿动词}／External。
//!           发射不能是 Held（emission 不入账本）——由类型排除，非运行时校验。
//!   [STAGE] 外部∧不可补偿发射 ⇒ 两阶段（endstate §8.5）【设计】；可摊销时塌缩进准入
//!           （effect-plan §5.4 standing 同意＝↓B）【设计】；不可摊销（§5.5 硬清单）⇒
//!           逐次同意＝F3 扣发【设计】。
//!   [BUDGET] 消耗性读进预算（endstate §8.3 硬规则→§9-3；socket 之教训）【设计＋推导】：
//!           bears_budget ⟺ ¬Repeatable。
//!   [EDIT]  attenuate 声明表住在这里（F3 [ρ-EQ]：等价由注册期声明固定）【推导】；
//!           降档只准收窄（m0 §5 attenuate 只准收窄的同型）【设计】。
//!   [PROJ]  投影统一：`derive_holding_grade`→F2、`derive_handler_policy`→F3。
//!           本演练的承重件：两台已冻结机器所需都是这张表的投影。
//!   [CEFF]  F6（Workspace 走查）：四象限里的 **c-effect**——对既有持有的界内变换（microVM 里
//!           exec/写文件、QP 状态迁移）——三分类里没有它。硬塞会错：记 Emitting 则每次都等同意且
//!           不可回滚；记 Consuming/Held 则 teardown 去释放不存在的持有。新增 `Kind::Transforming`：
//!           不出界、进预算（fuel）、不扣发、其逆由**类的 ρ**（快照/restore、状态复位）承载——
//!           所以只允许注册在 ρ ≠ External 的类下【推导；四象限＋Cordis 边界推进定律】。
//!   [PROTO] F6（RDMA 走查）：协议次序列（QP 状态机）。按类声明一台安全自动机（protocol.rs），
//!           投影给 F3 做运行期精确执行、给准入做静态可达集检查【文献✓ safety；推导】。

use crate::ledger::RevertGrade;
use std::collections::{BTreeMap, BTreeSet};

/// [WG] 消耗类动作的世界档。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConsumeGrade {
    /// 世界变更物化为一笔持有（open/spawn/bind/reserve）：怎么还由类的 ρ 决定（F2）。
    Held,
    /// 可补偿（dequeue 之 nack/requeue）：登记补偿动词。
    Compensable { compensate_with: String },
    /// 外部不可补偿（recv：字节已从对端消失）。
    External,
}

/// [WG] 发射类动作的世界档——没有 Held（emission 不入账本）、没有 Inverse（影子不可 RA-求逆）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EmitGrade {
    Compensable { compensate_with: String },
    External,
}

/// [TRI]+[WG]+[STAGE] 动词性格：三分类内嵌各自合法的世界档——非法组合不可表示。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 不动世界；无世界档。
    Repeatable,
    /// [CEFF] 界内变换：改变本类某个既有持有的状态，不出界。逆由类的 ρ 承载
    /// （段回滚＝restore 到段起点检查点／状态复位），故不带世界档、不扣发、进预算。
    Transforming,
    Consuming { world: ConsumeGrade },
    /// `amortizable`：一次同意可覆盖一批（standing/↓B）；false ＝ 硬清单，逐次同意。
    Emitting { world: EmitGrade, amortizable: bool },
}

/// 一行真理表：(类,动词) 的性格＋等式旗标＋降档声明。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VerbEntry {
    pub kind: Kind,
    /// E 等式：幂等 ⇒ 盲重放许可（F1 release 幂等、F2 盲重放同源）。
    pub idempotent: bool,
    /// Summary of a declared commutation law, including its operation pairs,
    /// parameters, observations and inverse interactions. The flag alone does
    /// not establish the independence required by F2/T43.
    pub commutes: bool,
    /// [EDIT] 降档声明：越界时可改写为同类的哪个动词（F3 Policy.degrade 的来源）。
    pub degrade: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VerbError {
    /// 注册期一致性校验未过——不合理条目当场拒。
    Incoherent(&'static str),
    /// 查表未命中（裸动词名或未注册的 (类,动词)）——D1 下这是硬错，不是缺省放行。
    Unknown,
    /// Held 动词所在类未声明 ρ——F2 不知道怎么还，不许先记后补。
    ClassNotDeclared,
    /// 类 ρ 重复声明（注册期固定，不许改）。
    ClassAlreadyDeclared,
}

impl VerbEntry {
    /// 不可变源读（endstate §8.1b 第一等）：幂等且与一切交换。
    pub fn repeatable() -> Self {
        Self { kind: Kind::Repeatable, idempotent: true, commutes: true, degrade: None }
    }
    /// 共享可变源读（endstate §8.1b 第二等，B11）：不动世界、不进预算、盲重放安全，
    /// 但结果依赖与写者的交错——不可交换。
    pub fn repeatable_shared() -> Self {
        Self { kind: Kind::Repeatable, idempotent: true, commutes: false, degrade: None }
    }
    pub fn transforming() -> Self {
        Self { kind: Kind::Transforming, idempotent: false, commutes: false, degrade: None }
    }
    pub fn consuming(world: ConsumeGrade) -> Self {
        Self { kind: Kind::Consuming { world }, idempotent: false, commutes: false, degrade: None }
    }
    pub fn emitting(world: EmitGrade, amortizable: bool) -> Self {
        Self { kind: Kind::Emitting { world, amortizable }, idempotent: false, commutes: false, degrade: None }
    }
    pub fn with_flags(mut self, idempotent: bool, commutes: bool) -> Self {
        self.idempotent = idempotent;
        self.commutes = commutes;
        self
    }
    pub fn degrades_to(mut self, verb: &str) -> Self {
        self.degrade = Some(verb.to_string());
        self
    }

    /// [TRI] 注册期一致性（只断言被引处明确支持的方向；其余组合由类型已排除）：
    ///   · 可重复 ⟹ 幂等（endstate §8.3"盲重放"）。**交换性不由性格蕴含、按动词声明**：
    ///     endstate §8.1b 的"与一切交换"只对**不可变源**读成立（CAS、快照：commutes=true）；
    ///     **共享可变源**读（正被别人写的文件：§8.1b 第二等）不动世界、不进预算、盲重放安全，
    ///     但结果依赖交错，声明 commutes=false——不许被当作可乱序/并行重排的项。
    ///     （B11：v2 曾把 §8.1b 对不可变源的断言推广到全部可重复读，强于来源，且让第二等
    ///     通道无处安放——措辞纪律，与 B1 同类。）
    ///   · 消耗：不加幂等旗标约束；新的消费与同一请求的幂等重试是不同概念。
    ///   · 发射：不加旗标约束（带幂等键的 PUT 是幂等发射，合法）。
    ///   · 界内变换：不加旗标约束（chmod 幂等、append 不幂等，皆合法）；类级约束在 register。
    /// 不主张的：可重复 ⇔ 某代数（可重复观察独占资源不要求资源可复制——那会 overstate）。
    pub fn check_coherent(&self) -> Result<(), VerbError> {
        match self.kind {
            Kind::Repeatable if !self.idempotent => {
                Err(VerbError::Incoherent("repeatable verb must be idempotent (blind-replay safe)"))
            }
            _ => Ok(()),
        }
    }

    /// [BUDGET] 精化 D9：消耗、发射与界内变换进预算（变换消耗 fuel），可重复读不进（只记标签）。
    pub fn bears_budget(&self) -> bool {
        !matches!(self.kind, Kind::Repeatable)
    }

    /// [CEFF] 界内变换：其效果可由类 ρ 在段回滚时撤销（F3 rollback_segment 的 restore 路径）。
    pub fn contained(&self) -> bool {
        matches!(self.kind, Kind::Transforming)
    }

    /// [STAGE] 两阶段形状：外部∧不可补偿的发射（endstate §8.5 的字面）。
    /// 可摊销时由准入满足（铸池→四元组→发射），不可摊销时由 F3 扣发满足。
    pub fn staged_shape(&self) -> bool {
        matches!(self.kind, Kind::Emitting { world: EmitGrade::External, .. })
    }

    /// [STAGE] 逐次扣发（F3 Policy.staged_verbs 的真正来源）⟺ 发射 ∧ 不可摊销。
    /// 注意可补偿但在硬清单上的发射（删除→可从回收站恢复）也逐次同意。
    pub fn withhold(&self) -> bool {
        matches!(self.kind, Kind::Emitting { amortizable: false, .. })
    }

    /// 需要登记补偿动词的动作（saga 的补偿端）。
    pub fn compensation_verb(&self) -> Option<&str> {
        match &self.kind {
            Kind::Consuming { world: ConsumeGrade::Compensable { compensate_with } }
            | Kind::Emitting { world: EmitGrade::Compensable { compensate_with }, .. } => {
                Some(compensate_with)
            }
            _ => None,
        }
    }

    /// [PP] 幂等旗标是 F1/F2"盲重放"许可的表侧来源。
    pub fn blind_replay_safe(&self) -> bool {
        self.idempotent
    }

    /// [EDIT] 严重度序（降档只准沿此序不升）：可重复 < 界内变换 < 消耗 < 发射；
    /// 发射内：可补偿 < 外部；可摊销 < 不可摊销。
    fn severity(&self) -> (u8, u8, u8) {
        match &self.kind {
            Kind::Repeatable => (0, 0, 0),
            Kind::Transforming => (1, 0, 0),
            Kind::Consuming { world } => (2, if matches!(world, ConsumeGrade::External) { 1 } else { 0 }, 0),
            Kind::Emitting { world, amortizable } => (
                3,
                if matches!(world, EmitGrade::External) { 1 } else { 0 },
                if *amortizable { 0 } else { 1 },
            ),
        }
    }
}

/// F3 每台 handler 监督器所需的策略投影（按类，[D1]）。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HandlerPolicy {
    /// → F3 `Policy.staged_verbs`（逐次扣发集）。
    pub withhold: BTreeSet<String>,
    /// → 预算闸：进预算的动词（消耗＋发射）。
    pub budget: BTreeSet<String>,
    /// → F3 `Policy.degrade`（attenuate 声明表）。
    pub degrade: BTreeMap<String, String>,
    /// → saga：需登记补偿动词的动作。
    pub compensations: BTreeMap<String, String>,
    /// [CEFF] → F3：界内变换动词（段回滚时对其触及的持有调类 restore）。
    pub contained: BTreeSet<String>,
    /// [PROTO] → F3 运行期钩子／准入静态检查：该类声明的协议自动机。
    pub protocol: Option<crate::protocol::Protocol>,
}

/// [D1] 真理表：键＝(类/handler, 动词)。裸动词名不可查、裸动词集不可投影。
#[derive(Default, Clone)]
pub struct VerbTable {
    entries: BTreeMap<(String, String), VerbEntry>,
    /// [ρ] 类的持有档：注册期一次声明、唯一、无 setter。F2 `ClassDecl.revert_grade` 的来源。
    holding_rho: BTreeMap<String, RevertGrade>,
    /// [PROTO] 类的协议自动机（可选）。
    protocols: BTreeMap<String, crate::protocol::Protocol>,
}

impl VerbTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// [ρ] 声明类的持有档（一次、不可改）。只有会产生持有的类需要声明。
    pub fn declare_class(&mut self, class: &str, rho: RevertGrade) -> Result<(), VerbError> {
        if self.holding_rho.contains_key(class) {
            return Err(VerbError::ClassAlreadyDeclared);
        }
        self.holding_rho.insert(class.to_string(), rho);
        Ok(())
    }

    /// 注册一行：跑一致性校验；Held 动词要求类已声明 ρ（F2 必须知道怎么还）。
    pub fn register(&mut self, class: &str, verb: &str, entry: VerbEntry) -> Result<(), VerbError> {
        entry.check_coherent()?;
        if matches!(entry.kind, Kind::Consuming { world: ConsumeGrade::Held })
            && !self.holding_rho.contains_key(class)
        {
            return Err(VerbError::ClassNotDeclared);
        }
        if matches!(entry.kind, Kind::Transforming) {
            // [CEFF] 界内变换的逆由类 ρ 承载：类必须已声明，且 ρ 不得为 External
            //（External ＝ 花了就是花了，谈不上"界内可逆变换"）。
            match self.holding_rho.get(class) {
                None => return Err(VerbError::ClassNotDeclared),
                Some(RevertGrade::External) => {
                    return Err(VerbError::Incoherent("transforming verb needs a class whose holdings are reversible (ρ ≠ External)"))
                }
                Some(_) => {}
            }
        }
        self.entries.insert((class.to_string(), verb.to_string()), entry);
        Ok(())
    }

    /// [PROTO] 声明类的协议自动机（一次）。辖域动词须已注册于该类（check_all 复核）。
    pub fn declare_protocol(&mut self, class: &str, proto: crate::protocol::Protocol) -> Result<(), VerbError> {
        if self.protocols.contains_key(class) {
            return Err(VerbError::ClassAlreadyDeclared);
        }
        self.protocols.insert(class.to_string(), proto);
        Ok(())
    }

    /// [D1] 查表以 (类,动词) 为键。未命中＝Unknown（硬错，非缺省放行）。
    pub fn lookup(&self, class: &str, verb: &str) -> Result<&VerbEntry, VerbError> {
        self.entries
            .get(&(class.to_string(), verb.to_string()))
            .ok_or(VerbError::Unknown)
    }

    /// 全表一致性：逐条校验＋跨行约束（[EDIT] 降档目标须同类已注册且严重度不升；
    /// 补偿动词须同类已注册且本身不再要求补偿——补偿链在一步内闭合）。
    pub fn check_all(&self) -> Result<(), VerbError> {
        for ((class, _verb), e) in &self.entries {
            e.check_coherent()?;
            if let Some(d) = &e.degrade {
                let target = self
                    .entries
                    .get(&(class.clone(), d.clone()))
                    .ok_or(VerbError::Incoherent("degrade target must be a registered verb of the same class"))?;
                if target.severity() > e.severity() {
                    return Err(VerbError::Incoherent("degrade may only narrow (attenuation never escalates)"));
                }
            }
            if let Some(c) = e.compensation_verb() {
                let comp = self
                    .entries
                    .get(&(class.clone(), c.to_string()))
                    .ok_or(VerbError::Incoherent("compensation verb must be registered in the same class"))?;
                if comp.compensation_verb().is_some() {
                    return Err(VerbError::Incoherent("compensation verb must not itself require compensation"));
                }
            }
        }
        // [PROTO] 协议辖域动词必须是本类已注册动词——协议约束的是本类的一组动词。
        for (class, proto) in &self.protocols {
            for v in &proto.scoped {
                if !self.entries.contains_key(&(class.clone(), v.clone())) {
                    return Err(VerbError::Incoherent("protocol mentions a verb not registered in its class"));
                }
            }
        }
        Ok(())
    }

    // --- [PROJ] 两个按类投影：喂 F2 / 喂 F3 ---

    /// → F2：类的持有档（＝ ClassDecl.revert_grade 的来源）。未声明＝None（该类不产生持有）。
    pub fn derive_holding_grade(&self, class: &str) -> Option<RevertGrade> {
        self.holding_rho.get(class).copied()
    }

    /// → F3：该 handler 的策略投影。同名动词在别的 handler 下的性格与此无关（[D1]）。
    pub fn derive_handler_policy(&self, class: &str) -> HandlerPolicy {
        let mut p = HandlerPolicy::default();
        for ((c, v), e) in &self.entries {
            if c != class {
                continue;
            }
            if e.withhold() {
                p.withhold.insert(v.clone());
            }
            if e.bears_budget() {
                p.budget.insert(v.clone());
            }
            if let Some(d) = &e.degrade {
                p.degrade.insert(v.clone(), d.clone());
            }
            if let Some(cv) = e.compensation_verb() {
                p.compensations.insert(v.clone(), cv.to_string());
            }
            if e.contained() {
                p.contained.insert(v.clone());
            }
        }
        p.protocol = self.protocols.get(class).cloned();
        p
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
