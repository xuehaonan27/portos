//! Monitor / 救济语义 — freeze drill F3.
//!
//! 理论标签（对照 design/freeze-f3-monitor.md 三列表；措辞对齐 theory-spec v0.3 §1.3/§4，
//! 依据 IJIS 2005（四类自动机、两级执行概念）与 TISSEC 2009（renewal 精确边界），
//! 措辞纪律：以下任何标签的主张不得强于被引定理本身）：
//!
//!   [SAFE]  precise enforcement 上限＝safety（Schneider；Ligatti 等确认对全部四类自动机
//!           成立）：不带缓冲的监督器每步只能「放行」或「就此截停」——逐动作谓词
//!           （sink 白名单、预算闸门）是它能执行的全部。
//!   [SUPPR] suppression：扣发（withhold）的前半——staged 动词的动作被压进缓冲，
//!           世界不可见；被糊弄的是 target（模型）而非世界，方向恰好对（feigning
//!           acceptance，§1.3 注记①）。
//!   [INSERT] insertion：扣发的后半——批准（＝staged emission 的 commit）到达后，
//!           缓冲按原序放出、恰好一次。suppression＋批准后 insertion ＝ withhold。
//!   [RENEW] TISSEC 2009 定理 3.3/3.4：edit automata effectively= 执行的恰是
//!           （无限）renewal 性质（＋eager-insertion 边角）。staged emission
//!           （reserve→同意→commit）是事务形状 ∈ renewal——扣发关口把 egress 从
//!           safety 档升到 renewal 档（天花板是 renewal，不是"任意"）。
//!   [EDIT]  attenuate ＝ edit：以**声明过的**降档动作替换原动作。
//!   [ρ-EQ]  定理 2.5 等价警告 ⇒ ρ 选取纪律（theory-spec §2.5）：「降档 ≈ 原动作」
//!           这个等价必须来自声明表（资源类/策略注册时固定），执行者永远不许
//!           事后发明等价——否则"可执行"主张空洞化。无声明 ⇒ 拒绝，绝不代拟。
//!   [TTL]   feigning acceptance 的代价：非法或悬而未决的被压制 emission 可以被
//!           无限压制（原文只保证合法输入的前缀终被输出）。工程对应物＝同意四元组
//!           的 ttl：到期即废、走补偿（F2 teardown）或放弃，绝不无限悬置。
//!   [WYS]   WYSIWYS 四元组 (plan_hash, budget, nonce, ttl)：同意可判定——准入闸
//!           只看四元组与计划字节哈希；哈希不符/nonce 重放/ttl 过期 ⇒ 0 个效应。
//!   [GATE]  预算＝counting capability 的塌缩（effect-plan §5.5）在 F1 语义下的
//!           正名：同意即铸造预算池（● 容量），花费即碎片行（◯，一行一笔，按结算契约留存），
//!           闸门即发放方闸门（can_mint 全量合成检查）。透支不是"扣减失败"，
//!           是 mint 被拒。
//!   [ESC]   escalate 超界语义（effect-plan §6.3 / m0 收尾清单#2）：停下——增量同意
//!           （新四元组、fresh nonce、同一 plan_hash）——从暂停点精确续跑。
//!   [PREFIX] fail-stop 前缀交付（m0 §8）：任何拒绝或失败 ⇒ 停机，已执行前缀连同
//!           trace 交回。已发射的 w-effect 不可回滚（不承诺跨效应原子性）；
//!           可回滚的是段内**获取侧**持有——经 F2 teardown 的前缀回滚
//!           （roadmap Phase C"不做"、Phase D 解锁的那一项）。
//!   [LOUD]  截断/降档/改写/回滚一律入 trace——绝不静默（审计观）。
//!   [SEG-TX] 段＝事务（决策 4，用户裁定 2026-09-05）：段内获取侧持有记在 `{fiber}:seg` 主体；
//!           commit（Completed）⇒ 段内持有全部转授给 fiber（F1 `transfer`，聚合值不变，parent 依赖保持）；
//!           任何非 commit 终态（FailStop／Truncated／Aborted）⇒ 段内未提前 promote 的持有由
//!           **monitor 自己**回滚（经 F2 teardown），不靠上层记得调；`promote`＝提前 commit 一笔，
//!           使其在之后的 abort 中幸存。一条路径，与 crash-only 同形。
//!
//! 三救济对应（§1.3，冻结措辞）：withhold＝suppression＋批准后 insertion；
//! attenuate＝edit；confine＝改写到替身。三模式（strict/truncate/escalate）是
//! **预算/界超限**的三态语义（读/效应不对称：读自由故可截断，效应受控故宁停不越）；
//! sink 越界不在三态商量之列——规划前提崩塌，fail-stop（或按声明降档/改写）。

use crate::identity::{
    ClassId, Generation, HoldingHandle, HoldingId, InstanceId, ResourceKey, SubjectId,
};
use crate::ledger::GrantRequest;
use crate::ledger::{AlgebraTag, ClassDecl, Frag, Ledger, LedgerError, RevertGrade};
use crate::ra::Count;
use crate::registry::{Capacity, Claim, PoolRef, RuntimeAlgebra};
use crate::teardown::{Orchestrator, RunOutcome};
use crate::time::{LeaseRequest, Timestamp};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// 动作与策略。动作自身不携带救济选择——staged/confined/degrade 全部来自策略与
// 类声明（[ρ-EQ]：等价与救济由声明固定，不由执行者临场挑选）。
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WAction {
    pub verb: String,
    pub target: String,
    /// 演练中明文；实装为 CAS 哈希（payload 永不入帧）。
    pub payload: String,
    pub cost: u64,
}

impl WAction {
    pub fn new(verb: &str, target: &str, payload: &str, cost: u64) -> Self {
        Self {
            verb: verb.into(),
            target: target.into(),
            payload: payload.into(),
            cost,
        }
    }
}

#[derive(Clone, Default)]
pub struct Policy {
    /// sink 白名单＝同意范围的 (动词, 目标) 投影：safety 档的逐动作谓词（[SAFE]）。
    /// 按动词×目标联合判定——attenuate 之所以可能把动作救回范围内，正因为
    /// 范围是动词敏感的（read_full 越界而 read_preview 在界内）。
    pub allow: BTreeSet<(String, String)>,
    /// 事务形状动词（外部∧不可补偿类的投影）：必须走 扣发→批准→放出（[SUPPR]/[INSERT]）。
    pub staged_verbs: BTreeSet<String>,
    /// attenuate 声明表：verb → 降档动词。等价由此固定（[EDIT]/[ρ-EQ]）。
    pub degrade: BTreeMap<String, String>,
    /// confine：这些目标的动作改写到替身世界（零真实效应）。
    pub confined_targets: BTreeSet<String>,
    /// F6 [CEFF]：本 handler 的界内变换动词（F4 `HandlerPolicy.contained` 投影）。
    /// 它们触及的目标（本类持有）在段回滚时由类 restore 恢复到段起点检查点。
    pub contained: BTreeSet<String>,
    /// F6 [PROTO]：本 handler 的协议自动机（F4 `HandlerPolicy.protocol` 投影）。
    /// safety 性质 ⇒ precise 档（截停）精确执行：违规即 fail-stop，交付最长合法前缀。
    pub protocol: Option<crate::protocol::Protocol>,
    /// F6：handler 名（restore 钥匙与效应类键的前缀）。每台监督器一份策略（D1）。
    pub handler: String,
}

impl Policy {
    pub fn allow(&mut self, verb: &str, target: &str) {
        self.allow.insert((verb.into(), target.into()));
    }
    fn in_scope(&self, a: &WAction) -> bool {
        self.allow.contains(&(a.verb.clone(), a.target.clone()))
    }
}

/// [WYS] 同意四元组＋签发时刻。演练中省 MAC（m0 已有 keyed-blake3 桩且数据流同构）。
#[derive(Clone, Debug)]
pub struct Consent {
    pub plan_hash: String,
    pub budget: u64,
    pub nonce: String,
    pub ttl_expires_at: u64,
}

/// 预算/界超限的三态语义（effect-plan §6.3）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Strict,
    Truncate,
    Escalate,
}

// ---------------------------------------------------------------------------
// 发射侧世界（与 teardown::MockWorld 相对：那边记边界的获取向，这边记发射向）。
// 真实/替身两个通道分开计量——confine 的"零真实效应"以此为凭。
// ---------------------------------------------------------------------------
#[derive(Default)]
pub struct EmissionWorld {
    /// 真实世界收到的发射（按序）。
    pub emitted: Vec<WAction>,
    /// 替身（隔离域）收到的发射（按序）——工作不丢，只是被改写了去处。
    pub standin: Vec<WAction>,
}

// ---------------------------------------------------------------------------
// trace：救济与截断的可听见性载体（[LOUD]）。实装即审计链条目。
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ev {
    Admitted {
        nonce: String,
    },
    Refused {
        why: Refusal,
    },
    Emitted {
        verb: String,
        target: String,
    },
    Suppressed {
        verb: String,
        target: String,
    },
    InsertedOnApproval {
        count: usize,
        nonce: String,
    },
    Attenuated {
        from: String,
        to: String,
    },
    Confined {
        verb: String,
        target: String,
    },
    BudgetExhausted {
        at: usize,
    },
    Truncation {
        dropped: usize,
    },
    Escalated {
        at: usize,
    },
    Resumed {
        nonce: String,
    },
    SegmentAborted {
        expired: bool,
    },
    SegmentRolledBack {
        holdings: usize,
    },
    /// [SEG-TX] commit：段内剩余持有转授给 fiber。
    SegmentCommitted {
        holdings: usize,
    },
    /// [SEG-TX] 提前 commit 一笔持有（此后 abort 不再回滚它）。
    Promoted {
        holding: HoldingId,
    },
    FailStop {
        at: usize,
    },
    /// F6 [PROTO]：协议违规（safety 档，与 sink 越界同级：fail-stop）。
    ProtocolViolation {
        verb: String,
        state: String,
        at: usize,
    },
    /// F6 [CEFF]：段回滚时对被界内变换触及的持有调了类 restore（恰好一次）。
    Restored {
        target: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// [WYS] 计划字节哈希与签署不符——展示与执行之间无 TOCTOU。
    ConsentMismatch,
    /// [WYS] nonce 已用过（一次性同意不得重放）。
    StaleNonce,
    /// [WYS]/[TTL] ttl 已过——同意已死，压制段到期即废。
    ExpiredTtl,
    /// 批准预算盖不住整个缓冲——事务形状：整批放出或整批不放，不做半截插入。
    ApprovalBudgetShort,
    /// 状态机位置不对（如对非悬置监督器调 approve）。
    WrongState,
    /// F6 [PROTO]：批准的整批在当前协议状态下会违规——整批拒（事务形状），段留待过期废弃。
    ProtocolViolation,
}

/// [SEG-TX] `promote` 的失败原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromoteError {
    /// 段已终态（commit 或 abort 之后无可提前）。
    SegmentClosed,
    /// 账本拒绝：句柄不在段内 / 世代不符 / 已释放。
    Ledger(LedgerError),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MonState {
    Idle,
    Running,
    /// 缓冲非空、计划走完：悬置待批（staged emission 的"同意"步）。
    AwaitingApproval,
    /// [ESC] 超界停下，等增量同意。
    Paused,
    Done(MonOutcome),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MonOutcome {
    Completed,
    /// [PREFIX] 前缀交付：at = 计划中未执行的首个下标。
    FailStop {
        at: usize,
    },
    /// 截断非失败：处理了前缀，弃量入 trace（[LOUD]）。
    Truncated {
        dropped: usize,
    },
    /// [TTL] 悬置段到期即废（或显式放弃）。
    Aborted {
        expired: bool,
    },
}

// ---------------------------------------------------------------------------
// 监督器本体：一台带 suppression 缓冲的 edit automaton（[RENEW] 档），
// 复用 F1 账本做预算闸（[GATE]）、F2 编排器做段回滚（[PREFIX]）。
// ---------------------------------------------------------------------------
pub struct Monitor {
    pub policy: Policy,
    pub mode: Mode,
    pub world: EmissionWorld,
    pub trace: Vec<Ev>,
    /// F1 账本＋F2 teardown 栈：预算池、段持有、回滚全走这里。
    pub orch: Orchestrator,
    state: MonState,
    plan: Vec<WAction>,
    cursor: usize,
    consent: Option<Consent>,
    /// 当前扣费预算池（consent 或增量同意的 nonce——同意即铸造，池随 nonce 走）。
    active_pool: String,
    used_nonces: BTreeSet<String>,
    /// [SUPPR] 扣发缓冲。演练中在内存：崩溃＝缓冲尽失＝退化的 abort——
    /// 压制的失败方向是"不发射"，对 w-effect 恰是安全侧（偏离申报）。
    buffer: Vec<WAction>,
    fiber: String,
    /// F6 [PROTO]：协议自动机当前状态（只随真实发射推进；替身/压制不推进）。
    proto_state: Option<String>,
    /// F6 [CEFF]：本段内被界内变换触及的目标（持有实例），回滚时逐个 restore。
    touched: BTreeSet<String>,
}

impl Monitor {
    pub fn new(policy: Policy, mode: Mode, mut ledger: Ledger, fiber: &str) -> Self {
        // [GATE] 预算是一个资源类：Counted 代数、无租约、Inverse 档
        //（花费行是纯记账，无世界侧动作）。
        ledger
            .register_class(ClassDecl {
                class_id: "budget".into(),
                algebra: AlgebraTag::Counted,
                release_idempotent: true,
                lease_duration: None,
                revert_grade: RevertGrade::Inverse,
            })
            .unwrap();
        Self {
            policy,
            mode,
            world: EmissionWorld::default(),
            trace: Vec::new(),
            orch: Orchestrator::new(ledger),
            state: MonState::Idle,
            plan: Vec::new(),
            cursor: 0,
            consent: None,
            active_pool: String::new(),
            used_nonces: BTreeSet::new(),
            buffer: Vec::new(),
            fiber: fiber.into(),
            proto_state: None,
            touched: BTreeSet::new(),
        }
    }

    pub fn state(&self) -> &MonState {
        &self.state
    }

    /// 扣发缓冲中尚未处置的动作数（内核自省用；穷举法则以此断言"绝不悬置"）。
    pub fn pending_suppressed(&self) -> usize {
        self.buffer.len()
    }

    /// 扣发缓冲的合计费用——批准四元组的预算必须盖住它（整批放出或整批不放）。
    pub fn pending_cost(&self) -> u64 {
        self.buffer.iter().map(|a| a.cost).sum()
    }

    /// 段主体：本计划获取侧持有的记账主体，与花费行分开——回滚段＝teardown 此主体。
    pub fn seg_subject(&self) -> String {
        format!("{}:seg", self.fiber)
    }

    /// 四元组合法性（[WYS] 的可判定内核：三个相等/序比较，O(1)）。
    fn check_quad(&self, c: &Consent, plan_hash: &str, now: u64) -> Result<(), Refusal> {
        if c.plan_hash != plan_hash {
            return Err(Refusal::ConsentMismatch);
        }
        if self.used_nonces.contains(&c.nonce) {
            return Err(Refusal::StaleNonce);
        }
        if now > c.ttl_expires_at {
            return Err(Refusal::ExpiredTtl);
        }
        Ok(())
    }

    /// [GATE] 同意即铸造：为 nonce 立预算池（● 容量行）。
    fn mint_pool(&mut self, c: &Consent) {
        self.orch
            .ledger
            .create_pool(
                &self
                    .orch
                    .ledger
                    .registered_class::<Count>(&ClassId::new("budget"))
                    .unwrap(),
                InstanceId::new(&c.nonce),
                Capacity::new(Count::Value(c.budget)).unwrap(),
            )
            .unwrap();
        self.used_nonces.insert(c.nonce.clone());
        self.active_pool = c.nonce.clone();
    }

    /// 段是否仍在悬置（缓冲待批或超界待增批）——都押着段内持有，都受原同意 ttl 封顶（[B12]）。
    pub fn is_suspended(&self) -> bool {
        matches!(self.state, MonState::AwaitingApproval | MonState::Paused)
    }

    /// [WYS] 准入闸：四元组不合法 ⇒ 0 个效应（连第一个动作都不看）。
    pub fn admit(
        &mut self,
        plan: Vec<WAction>,
        plan_hash: &str,
        consent: Consent,
        now: u64,
    ) -> Result<(), Refusal> {
        if self.state != MonState::Idle {
            return Err(Refusal::WrongState);
        }
        if let Err(why) = self.check_quad(&consent, plan_hash, now) {
            self.trace.push(Ev::Refused { why });
            return Err(why);
        }
        self.mint_pool(&consent);
        self.trace.push(Ev::Admitted {
            nonce: consent.nonce.clone(),
        });
        self.consent = Some(consent);
        self.plan = plan;
        self.cursor = 0;
        self.proto_state = self.policy.protocol.as_ref().map(|p| p.initial.clone());
        self.state = MonState::Running;
        Ok(())
    }

    /// [GATE] 花费＝碎片行的 mint：完整账本检查拒绝透支。
    fn spend(&mut self, cost: u64, now: u64) -> Result<(), LedgerError> {
        let pool = self.active_pool.clone();
        let spender = format!("{}:spent", self.fiber);
        self.orch
            .ledger
            .grant(
                &self.orch.ledger.pool::<Count>(&ResourceKey::new(
                    ClassId::new("budget"),
                    InstanceId::new(&pool),
                ))?,
                GrantRequest {
                    owner: SubjectId::new(&spender),
                    claim: Claim::new(Count::Value(cost)).unwrap(),
                    generation: Generation::new("consent"),
                    parent: None,
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(now)?,
                },
            )
            .map(|h| h.id())
            .map(|_| ())
    }

    /// 某预算池的已花费合成值（逐行折叠——行为真相，合计只是重算）。
    pub fn pool_spent(&self, nonce: &str) -> u64 {
        self.orch
            .ledger
            .live()
            .filter(|h| h.class_id.as_str() == "budget" && h.instance.as_str() == nonce)
            .map(|h| match &h.frag {
                Frag::Count(Count::Value(n)) => *n,
                _ => 0,
            })
            .sum()
    }

    /// 计划获取侧持有（演练里由测试代放，模拟计划中的 reserve/acquire 步）。
    pub fn stage_acquire<A: RuntimeAlgebra>(
        &mut self,
        pool: &PoolRef<A>,
        generation: Generation,
        claim: Claim<A>,
        now: Timestamp,
    ) -> Result<HoldingHandle, LedgerError> {
        self.orch.ledger.grant(
            pool,
            GrantRequest {
                owner: SubjectId::new(self.seg_subject()),
                claim,
                generation,
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now,
            },
        )
    }

    /// 逐动作步进：m0 的"逐效应四连"（sink 复检、预算闸、执行、记 trace）
    /// ＋三救济分派。返回 false ⇒ 状态机离开 Running。
    ///
    /// 顺序即语义（演练第二只 bug 的墓碑，见 freeze-f3 §2）：**sink 复检必须先于
    /// 扣发**。初版把 staged 检查放在最前，越界目标的 staged 动作被压进缓冲、
    /// 批准后盲放——insertion 也是发射，绕过了 safety 地板。修复＝管线重排：
    /// 缓冲的不变式是「除同意外已全合法」，批准后的盲放因此**由构造正确**
    /// （edit automaton 只输出合法序列——TISSEC 语义的实现位对应）。
    fn step(&mut self, now: u64) -> bool {
        let mut act = self.plan[self.cursor].clone();

        // ① confine：声明为隔离目标 ⇒ 改写到替身。替身不是真实边界——
        //    无需同意、不占真实预算；staged 动词也一样进替身（隔离优先）。
        if self.policy.confined_targets.contains(&act.target) {
            self.trace.push(Ev::Confined {
                verb: act.verb.clone(),
                target: act.target.clone(),
            });
            self.world.standin.push(act);
            self.cursor += 1;
            return true;
        }

        // ② [SAFE] sink 复检（safety 地板）。越界 ⇒ 查 attenuate 声明表
        //    （[EDIT]/[ρ-EQ]），改写后**重检**——声明的等价救不回来就 fail-stop；
        //    sink 越界不进三态商量（规划前提崩塌）。
        if !self.policy.in_scope(&act) {
            let rescued = match self.policy.degrade.get(&act.verb).cloned() {
                Some(degraded) => {
                    self.trace.push(Ev::Attenuated {
                        from: act.verb.clone(),
                        to: degraded.clone(),
                    });
                    act.verb = degraded;
                    self.policy.in_scope(&act) // edit 之后必须重新过同一谓词
                }
                None => false,
            };
            if !rescued {
                self.trace.push(Ev::FailStop { at: self.cursor });
                self.finish_abort(now, MonOutcome::FailStop { at: self.cursor });
                return false;
            }
        }

        // ③ [SUPPR] 事务形状动词（此处 act 已在范围内）：压进缓冲。
        //    世界不可见、预算未花（预算约束真实发射；插入时按批准池计费）。
        if self.policy.staged_verbs.contains(&act.verb) {
            self.trace.push(Ev::Suppressed {
                verb: act.verb.clone(),
                target: act.target.clone(),
            });
            self.buffer.push(act);
            self.cursor += 1;
            return true;
        }

        // ③′ F6 [PROTO] 协议复检（safety 地板的一部分，用改写后的有效动词）：
        //    无转移 ⇒ fail-stop（与 sink 越界同级，不进三态商量）。状态在发射成功后才推进。
        let next_state = match (&self.policy.protocol, &self.proto_state) {
            (Some(p), Some(st)) => match p.step(st, &act.verb) {
                Ok(n) => Some(n),
                Err(v) => {
                    self.trace.push(Ev::ProtocolViolation {
                        verb: v.verb,
                        state: v.state,
                        at: self.cursor,
                    });
                    self.trace.push(Ev::FailStop { at: self.cursor });
                    self.finish_abort(now, MonOutcome::FailStop { at: self.cursor });
                    return false;
                }
            },
            _ => None,
        };

        // ④ [GATE] 预算闸 → 执行 → 记账。
        match self.spend(act.cost, now) {
            Ok(()) => {
                self.trace.push(Ev::Emitted {
                    verb: act.verb.clone(),
                    target: act.target.clone(),
                });
                // F6 [CEFF]：界内变换触及的目标入段清单，回滚时 restore。
                if self.policy.contained.contains(&act.verb) {
                    self.touched.insert(act.target.clone());
                }
                if next_state.is_some() {
                    self.proto_state = next_state;
                }
                self.world.emitted.push(act);
                self.cursor += 1;
                true
            }
            Err(LedgerError::Conflict) => self.on_budget_exhausted(now),
            Err(e) => panic!("budget gate invariant broken: {e:?}"),
        }
    }

    /// 超界三态（effect-plan §6.3；读/效应不对称的运行时投影）。
    fn on_budget_exhausted(&mut self, now: u64) -> bool {
        self.trace.push(Ev::BudgetExhausted { at: self.cursor });
        match self.mode {
            // strict：基数意外＝规划前提崩塌，继续跑是在错误世界里消耗预算。
            Mode::Strict => {
                self.trace.push(Ev::FailStop { at: self.cursor });
                self.finish_abort(now, MonOutcome::FailStop { at: self.cursor });
            }
            // truncate：处理前缀，弃量显式入 trace（[LOUD] 绝不静默）。
            // 截断是非 commit 终态：前缀交付（已发射者站着不动），段内获取侧持有照样回滚（[SEG-TX]）。
            Mode::Truncate => {
                let dropped = self.plan.len() - self.cursor;
                self.trace.push(Ev::Truncation { dropped });
                self.finish_abort(now, MonOutcome::Truncated { dropped });
            }
            // [ESC] escalate：停下，等增量同意；动作原地保留，续跑从这里精确继续。
            Mode::Escalate => {
                self.trace.push(Ev::Escalated { at: self.cursor });
                self.state = MonState::Paused;
            }
        }
        false
    }

    /// 跑到状态机离开 Running（Done / AwaitingApproval / Paused）。
    pub fn run(&mut self, now: u64) -> &MonState {
        while self.state == MonState::Running {
            if self.cursor >= self.plan.len() {
                if self.buffer.is_empty() {
                    self.finish_commit(); // [SEG-TX] 走完且无扣发件＝commit
                } else {
                    // 计划走完但有扣发件：悬置待批（reserve→**同意**→commit 的中步）。
                    self.state = MonState::AwaitingApproval;
                }
                break;
            }
            if !self.step(now) {
                break;
            }
        }
        &self.state
    }

    /// [INSERT] 批准＝staged emission 的 commit：缓冲按原序放出、恰好一次。
    /// 批准本身是一个新四元组（同一 plan_hash、fresh nonce、盖得住整批的预算、活的 ttl）。
    /// 放出不再重跑 sink 检查——**缓冲不变式**（step 的管线序）保证入缓冲者
    /// 除同意外已全合法；预算由整批预检盖住。二者合成"批准后盲放由构造正确"。
    pub fn approve(&mut self, approval: Consent, now: u64) -> Result<(), Refusal> {
        if self.state != MonState::AwaitingApproval {
            return Err(Refusal::WrongState);
        }
        // [TTL] 悬置窗口由原同意的 ttl 封顶：过期的段只能废，不能补批。
        let orig = self.consent.as_ref().expect("admitted");
        if now > orig.ttl_expires_at {
            return Err(Refusal::ExpiredTtl);
        }
        let plan_hash = orig.plan_hash.clone();
        if let Err(why) = self.check_quad(&approval, &plan_hash, now) {
            self.trace.push(Ev::Refused { why }); // [LOUD] 批准被拒同样入 trace
            return Err(why);
        }
        // 事务形状：整批放出或整批不放——预算先盖住全部缓冲再动手。
        let total: u64 = self.buffer.iter().map(|a| a.cost).sum();
        if approval.budget < total {
            self.trace.push(Ev::Refused {
                why: Refusal::ApprovalBudgetShort,
            });
            return Err(Refusal::ApprovalBudgetShort);
        }
        // F6 [PROTO]+[REORDER]：整批在当前协议状态下预演；任一件无转移 ⇒ 整批拒。
        // 这正是"扣发重排世界序"的运行期兜底：计划序合法、世界序违规在此被拦住。
        let mut end_state = self.proto_state.clone();
        if let (Some(p), Some(st)) = (&self.policy.protocol, &self.proto_state) {
            let mut cur = st.clone();
            for a in &self.buffer {
                match p.step(&cur, &a.verb) {
                    Ok(n) => cur = n,
                    Err(_) => {
                        self.trace.push(Ev::Refused {
                            why: Refusal::ProtocolViolation,
                        });
                        return Err(Refusal::ProtocolViolation);
                    }
                }
            }
            end_state = Some(cur);
        }
        self.mint_pool(&approval);
        let batch: Vec<WAction> = std::mem::take(&mut self.buffer);
        for a in &batch {
            match self.spend(a.cost, now) {
                Ok(()) => self.world.emitted.push(a.clone()),
                Err(e) => panic!("approval pre-check should cover batch: {e:?}"),
            }
        }
        self.proto_state = end_state;
        self.trace.push(Ev::InsertedOnApproval {
            count: batch.len(),
            nonce: approval.nonce.clone(),
        });
        self.finish_commit(); // [SEG-TX] 批准放出＝commit
        Ok(())
    }

    /// [SEG-TX] 提前 commit 一笔段内持有：转授给 fiber，此后 abort 不再回滚它。
    /// 只在段未终态时可用；句柄必须在段内（转授对来源主体核对）。
    pub fn promote(&mut self, holding: &HoldingHandle) -> Result<(), PromoteError> {
        if !matches!(
            self.state,
            MonState::Running | MonState::AwaitingApproval | MonState::Paused
        ) {
            return Err(PromoteError::SegmentClosed);
        }
        let seg = self.seg_subject();
        self.orch
            .ledger
            .transfer(holding, &SubjectId::new(&seg), SubjectId::new(&self.fiber))
            .map_err(PromoteError::Ledger)?;
        self.trace.push(Ev::Promoted {
            holding: holding.id(),
        });
        Ok(())
    }

    /// [SEG-TX] commit：段内剩余持有全部转授给 fiber（一笔一转，碎片不变、聚合值不变，parent 依赖保持）；
    /// 界内变换的触及清单作废（不 restore）；状态 Done(Completed)。
    fn finish_commit(&mut self) {
        let seg = self.seg_subject();
        let items: Vec<(HoldingId, Generation)> = self
            .orch
            .ledger
            .live_snapshot(&SubjectId::new(&seg))
            .into_iter()
            .map(|it| (it.id, it.generation))
            .collect();
        for (id, generation) in &items {
            match self.orch.ledger.transfer(
                &HoldingHandle::new(*id, generation.clone()),
                &SubjectId::new(&seg),
                SubjectId::new(&self.fiber),
            ) {
                Ok(()) => {}
                Err(e) => panic!("segment commit invariant broken: {e:?}"),
            }
        }
        self.touched.clear();
        if !items.is_empty() {
            self.trace.push(Ev::SegmentCommitted {
                holdings: items.len(),
            });
        }
        self.state = MonState::Done(MonOutcome::Completed);
    }

    /// [SEG-TX] 非 commit 终态的唯一出口：弃缓冲（可听见）→ 段回滚（monitor 自己做）→ 终态。
    fn finish_abort(&mut self, now: u64, outcome: MonOutcome) {
        self.abort_buffer_on_terminal();
        self.rollback_segment(now);
        self.state = MonState::Done(outcome);
    }

    /// [TTL] 到期即废：缓冲弃置（永不发射），段内获取侧持有走 F2 补偿/释放。
    /// 绝不无限悬置——这是 feigning acceptance 代价的工程封顶。
    ///
    /// [B12] 悬置有两种形态：扣发缓冲待批（AwaitingApproval）与超界待增批（Paused）。
    /// 两者都押着段内持有、都由**原**同意的 ttl 封顶（§6.3 设计澄清④）；v1 只封顶前者，
    /// Paused 段可被无限悬置——统一复核抓到。现两态同一路径：到期即废、段回滚。
    pub fn expire(&mut self, now: u64) -> Result<(), Refusal> {
        if !matches!(self.state, MonState::AwaitingApproval | MonState::Paused) {
            return Err(Refusal::WrongState);
        }
        let orig = self.consent.as_ref().expect("admitted");
        if now <= orig.ttl_expires_at {
            return Err(Refusal::WrongState); // 还没到期，轮不到废
        }
        self.buffer.clear();
        self.trace.push(Ev::SegmentAborted { expired: true });
        self.rollback_segment(now);
        self.state = MonState::Done(MonOutcome::Aborted { expired: true });
        Ok(())
    }

    /// [ESC] 增量同意续跑（m0 收尾清单#2 的可运行形态）：新四元组合法 ⇒ 铸新池、
    /// 从暂停动作精确继续。旧 nonce 重放/错哈希/死 ttl 一律拒。
    /// [B12] 原同意的 ttl 同样封顶续跑窗口：过期段只能废（与 approve 同款），不得补批。
    pub fn resume_with(&mut self, incremental: Consent, now: u64) -> Result<&MonState, Refusal> {
        if self.state != MonState::Paused {
            return Err(Refusal::WrongState);
        }
        let orig = self.consent.as_ref().expect("admitted");
        if now > orig.ttl_expires_at {
            self.trace.push(Ev::Refused {
                why: Refusal::ExpiredTtl,
            });
            return Err(Refusal::ExpiredTtl);
        }
        let plan_hash = orig.plan_hash.clone();
        if let Err(why) = self.check_quad(&incremental, &plan_hash, now) {
            self.trace.push(Ev::Refused { why }); // [LOUD]
            return Err(why);
        }
        self.mint_pool(&incremental);
        self.trace.push(Ev::Resumed {
            nonce: incremental.nonce.clone(),
        });
        self.state = MonState::Running;
        Ok(self.run(now))
    }

    /// 终止即弃（B4 修复，穷举测试上线当场抓到——见 tests/f3_monitor_lattice.rs 头注）：
    /// run 以 FailStop/Truncated 收场＝事务未达 commit 点 ⇒ 缓冲整批废弃，且处置
    /// 必须可听见（[LOUD]）。初版让压制件无声滞留——不放出（安全侧）但也不处置，
    /// 违反"绝不悬置"。弃置的失败方向是"不发射"，对 w-effect 恰是安全侧；
    /// Escalate 的 Paused 非终态，缓冲存续待续跑，不在此列。
    fn abort_buffer_on_terminal(&mut self) {
        if !self.buffer.is_empty() {
            self.buffer.clear();
            self.trace.push(Ev::SegmentAborted { expired: false });
        }
    }

    /// [PREFIX] 段回滚：已发射的 w-effect 站着不动（不可回滚），段内获取侧持有
    /// 经 F2 teardown 收回（有逆档释放、可补偿档按钥匙补偿）。
    /// [SEG-TX] 由 `finish_abort`／`expire` 在一切非 commit 终态**自动**调用；公开只为幂等性法则
    /// （重复调用不重复 restore、不重复释放）。
    pub fn rollback_segment(&mut self, _now: u64) {
        // F6 [CEFF]：先把界内变换触及的持有恢复到段起点检查点——类 restore，
        // 钥匙 = handler:target:seg，跨重试去重 ⇒ 恰好一次（与 F2 补偿同款承重）。
        let touched: Vec<String> = std::mem::take(&mut self.touched).into_iter().collect();
        for t in touched {
            let key = format!(
                "restore:{}:{}:{}",
                self.policy.handler,
                t,
                self.seg_subject()
            );
            if self.orch.world.restore(&key) {
                self.trace.push(Ev::Restored { target: t });
            }
        }
        let subj = self.seg_subject();
        let n = self.orch.ledger.live_snapshot(&SubjectId::new(&subj)).len();
        if n == 0 {
            return;
        }
        match self.orch.teardown(&subj, 1, None) {
            RunOutcome::Completed { failed } if failed.is_empty() => {
                self.trace.push(Ev::SegmentRolledBack { holdings: n });
            }
            other => panic!("segment rollback did not converge: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// [SAFE]/[RENEW] 分离见证用：precise 档强制器＝截停自动机。无缓冲、无改写，
// 每步只能「放行本动作」或「就此永久截停」，且决策只依赖已见前缀——
// 对给定有限输入，任何确定性截停策略都由其首个截停点完全刻画，
// 故 halt_before 的穷举＝对全部截停自动机的穷举（测试据此做完备枚举）。
// ---------------------------------------------------------------------------
pub fn truncation_run(plan: &[WAction], halt_before: Option<usize>) -> Vec<WAction> {
    let cut = halt_before.unwrap_or(plan.len());
    plan.iter().take(cut).cloned().collect()
}
