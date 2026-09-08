//! F3 组合面确定性穷举（用户决定 #8：确定性穷举，并在注释中详细注明）。
//!
//! ## 为什么要这份测试（B3 的教训）
//!
//! F3 演练中管线序 bug（越界 staged 件经批准盲放）出现时，12 条单法则测试**全绿**——
//! 单项功能各有测试，漏洞在组合点（staged×越界）。本文件的对策不是再加几条
//! 人想到的组合，而是**穷举一个封闭格点**：只要 bug 落在格点内，它必被踩到；
//! 格点的边界写在下方，收缩它会使 run 数断言（见 sweep 末尾）变红。
//! 方法选确定性穷举而非随机（PBT），依既定偏好：随机测试对同一格点是
//! 无放回抽样的劣化版，且失败不可稳定复现。
//!
//! ## 格点五轴（封闭边界，共 (8+8²+8³)×3×4×3 = 21,024 局）
//!
//! 1. **动作原型 × 计划**：8 种原型（下表）的全部长 ≤3 串。
//!    为什么是这 8 种：step() 管线恰有四道闸（confine → sink/attenuate → staged →
//!    budget），每道闸各有"命中/未命中"及其细分——8 原型对管线分支是**满射**：
//!    | 原型 | 动作 | 命中的分支 |
//!    |---|---|---|
//!    | Ok            | post ok.com       | 四道全过，直达发射 |
//!    | Staged        | send ok.com       | 界内 staged → 压缓冲 |
//!    | OffDegOk      | read_full deg.com | 越界，声明降档**救回**（read_preview deg.com ∈ 白名单） |
//!    | OffDegOff     | read_full far.com | 越界，声明降档**救不回**（改写后重检仍越界 ⇒ 停） |
//!    | OffNoDeg      | post bad.com      | 越界，无降档声明 ⇒ 停 |
//!    | Confined      | post box.com      | 隔离目标 → 替身 |
//!    | ConfinedStaged| send box.com      | 隔离 ∧ staged：隔离优先（替身不是真实边界，无需同意） |
//!    | StagedOff     | send bad.com      | staged ∧ 越界：sink 先于扣发 ⇒ 停于压制之前（B3 墓碑） |
//!    长 ≤3 已足：管线无跨 3 步以上的状态耦合（缓冲、预算、暂停都在每步重判），
//!    三元组合覆盖"任一分支后接任一分支再接任一分支"。
//! 2. **模式**：Strict / Truncate / Escalate（超界三态语义）。
//! 3. **初始预算**：0..=3（动作费用全为 1，故覆盖"从第一步就不够"到"全够"）。
//! 4. **收尾**：ApproveEnough（批准且预算盖住整批）/ ApproveShortThenExpire
//!    （先以差 1 的预算批准——必须整批拒绝——再过期废段）/ ExpireOnly（直接过期）。
//!    对无缓冲的局收尾轴天然惰性（照跑不剪枝，保持格点规则简单可查）。
//! 5. **escalate 增批节奏**：暂停后每次只增 1 预算（挤牙膏式），故一局内可多次
//!    暂停-续跑——覆盖增量同意的重复组合。
//!
//! ## 断言的是法则，不是复刻实现
//!
//! 每局跑到终态后验一组**不变式**（见 check 函数注释）；不重建一个"预期输出
//! 预言机"——那等于把实现写两遍，两遍一起错。不变式之外，另设**见证断言**
//! （sweep 末尾）：每类结局、每种救济事件、重复暂停、整批拒绝……都必须在
//! 全扫描中至少出现一次——防止格点空转（F2 中 witnessed_dup_request 的一般化：
//! 测试若从未踩到暧昧路径，它就太弱了）。
//!
//! ## 本测试上线时抓到的问题（B4，详见 freeze-f3 §2）
//!
//! 「绝不悬置」不变式当场变红：run 以 FailStop/Truncated 终止时，先前压进
//! 缓冲的 staged 动作**无声滞留**——既不放出（安全侧）也不处置入 trace（违反
//! 绝不静默/绝不悬置）。修复：终止即弃——终态路径清缓冲并记
//! SegmentAborted{expired:false}（事务形状：run 未达 commit 点即 abort）。

use portos_rm::ledger::Ledger;
use portos_rm::monitor::*;

const H: &str = "blake3:lattice-plan";
const NOW: u64 = 10;
const TTL: u64 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arch {
    Ok,
    Staged,
    OffDegOk,
    OffDegOff,
    OffNoDeg,
    Confined,
    ConfinedStaged,
    StagedOff,
}
use Arch::*;
const ARCHS: [Arch; 8] = [Ok, Staged, OffDegOk, OffDegOff, OffNoDeg, Confined, ConfinedStaged, StagedOff];

impl Arch {
    /// 原型 → 具体动作。payload 记计划位序号（穷举里恰好一次/顺序断言的锚点）。
    fn action(self, pos: usize) -> WAction {
        let (verb, target) = match self {
            Ok => ("post", "ok.com"),
            Staged => ("send", "ok.com"),
            OffDegOk => ("read_full", "deg.com"),
            OffDegOff => ("read_full", "far.com"),
            OffNoDeg => ("post", "bad.com"),
            Confined => ("post", "box.com"),
            ConfinedStaged => ("send", "box.com"),
            StagedOff => ("send", "bad.com"),
        };
        WAction::new(verb, target, &pos.to_string(), 1)
    }
    /// 该原型在 sink 关口必停（无救济可用）——FailStop 位置断言的依据。
    fn sink_fails(self) -> bool {
        matches!(self, OffDegOff | OffNoDeg | StagedOff)
    }
    fn is_staged_inscope(self) -> bool {
        matches!(self, Staged)
    }
    fn is_confined(self) -> bool {
        matches!(self, Confined | ConfinedStaged)
    }
}

fn policy() -> Policy {
    let mut p = Policy::default();
    p.allow("post", "ok.com");
    p.allow("send", "ok.com");
    p.allow("read_preview", "deg.com"); // 降档后的动作在白名单——OffDegOk 的"救回"依据
    p.staged_verbs.insert("send".into());
    p.degrade.insert("read_full".into(), "read_preview".into());
    p.confined_targets.insert("box.com".into());
    p
}

fn consent(nonce: &str, budget: u64) -> Consent {
    Consent { plan_hash: H.into(), budget, nonce: nonce.into(), ttl_expires_at: TTL }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ending {
    ApproveEnough,
    ApproveShortThenExpire,
    ExpireOnly,
}
const ENDINGS: [Ending; 3] = [Ending::ApproveEnough, Ending::ApproveShortThenExpire, Ending::ExpireOnly];

/// 全扫描的见证收集（防格点空转）。
#[derive(Default)]
struct Witness {
    runs: u64,
    completed: u64,
    failstop: u64,
    truncated: u64,
    aborted_expired: u64,
    inserted: bool,
    terminal_discard: bool, // SegmentAborted{expired:false}——B4 修复的存在证据
    approval_short: bool,
    truncation_ev: bool,
    attenuated: bool,
    confined: bool,
    multi_resume: bool, // 同一局 ≥2 次续跑（挤牙膏式增批确被走到）
    mixed_emission: bool, // 同一局同时有即时发射与批准插入
}

/// 驱动一局到终态：Paused ⇒ 增 1 预算续跑；AwaitingApproval ⇒ 按收尾轴处置。
/// 有界循环——超界即"悬置"，直接判死（终态可判性是被测性质的一部分）。
fn drive(archs: &[Arch], mode: Mode, budget: u64, ending: Ending, w: &mut Witness) -> Monitor {
    let mut m = Monitor::new(policy(), mode, Ledger::new(), "fib");
    let plan: Vec<WAction> = archs.iter().enumerate().map(|(i, a)| a.action(i)).collect();
    m.admit(plan, H, consent("n0", budget), NOW).unwrap();
    m.run(NOW);
    let mut fresh = 0u64;
    let mut resumes = 0u32;
    let mut guard = 0u32;
    loop {
        guard += 1;
        assert!(guard <= 16, "driver stuck — 状态机悬置于 {:?}（{:?} {:?} b={budget} {:?}）", m.state(), archs, mode, ending);
        match m.state().clone() {
            MonState::Done(_) => break,
            MonState::Paused => {
                fresh += 1;
                resumes += 1;
                m.resume_with(consent(&format!("r{fresh}"), 1), NOW).unwrap();
            }
            MonState::AwaitingApproval => {
                let need = m.pending_cost();
                assert!(need >= 1, "空缓冲不该悬置待批");
                match ending {
                    Ending::ApproveEnough => {
                        fresh += 1;
                        m.approve(consent(&format!("a{fresh}"), need), NOW).unwrap();
                    }
                    Ending::ApproveShortThenExpire => {
                        fresh += 1;
                        // 差 1 的批准必须**整批**拒绝（事务形状：无半截插入）……
                        let r = m.approve(consent(&format!("a{fresh}"), need - 1), NOW);
                        assert_eq!(r, Err(Refusal::ApprovalBudgetShort));
                        w.approval_short = true;
                        // ……然后过期废段（到期即废，走 [TTL] 路径）。
                        m.expire(TTL + 1).unwrap();
                    }
                    Ending::ExpireOnly => {
                        m.expire(TTL + 1).unwrap();
                    }
                }
            }
            MonState::Running | MonState::Idle => unreachable!("run() 不应停在 {:?}", m.state()),
        }
    }
    if resumes >= 2 {
        w.multi_resume = true;
    }
    m
}

/// 逐局不变式。每条都是法则（部分正确性），合起来不构成实现的复刻。
fn check(m: &Monitor, archs: &[Arch], mode: Mode, ending: Ending, w: &mut Witness) {
    let pol = policy();
    let pos_of = |a: &WAction| a.payload.parse::<usize>().unwrap();

    // 【零越界泄漏】真实世界的每一件发射 (动词,目标) ∈ 白名单——B3 的一般化：
    // 无论经直发、降档还是批准插入，出界零件数。
    for e in &m.world.emitted {
        assert!(
            pol.allow.contains(&(e.verb.clone(), e.target.clone())),
            "越界泄漏：{:?}（{:?} {:?} {:?}）", e, archs, mode, ending
        );
    }

    // 【替身分账】替身通道只承接隔离目标件；真实通道零隔离目标件（上一条已含）。
    for s in &m.world.standin {
        assert!(archs[pos_of(s)].is_confined(), "替身通道混入非隔离件：{:?}", s);
    }

    // 【恰好一次】两通道合并后：payload（=计划位）无重复；动词只能是原型动词
    // 或其声明降档（执行者不得发明第三种）。
    let mut seen = std::collections::BTreeSet::new();
    for e in m.world.emitted.iter().chain(m.world.standin.iter()) {
        assert!(seen.insert(e.payload.clone()), "计划位 {} 被发射两次", e.payload);
        let orig = archs[pos_of(e)].action(pos_of(e));
        let declared_deg = pol.degrade.get(&orig.verb);
        assert!(
            e.verb == orig.verb || Some(&e.verb) == declared_deg,
            "发明了未声明的动词改写：{} → {}", orig.verb, e.verb
        );
    }

    // 【预算守恒＝发放方闸门】真实发射的合计费用 == 账本中花费行折叠（行为真相，
    // 按计费政策留存）；账本全局不变量成立。替身与被压制件零花费。
    let spent: u64 = m
        .orch
        .ledger
        .live()
        .filter(|h| h.class_id.as_str() == "budget")
        .map(|h| match &h.frag {
            portos_rm::ledger::Frag::Count(portos_rm::ra::Count::Value(n)) => *n,
            _ => 0,
        })
        .sum();
    let emitted_cost: u64 = m.world.emitted.iter().map(|e| e.cost).sum();
    assert_eq!(spent, emitted_cost, "花费行合计 ≠ 真实发射合计（{:?} {:?}）", archs, mode);
    m.orch.ledger.invariant().unwrap();

    // 【staged 只经插入，且插入殿后】staged 位出现在真实世界 ⇔ 走过批准插入；
    // 插入发生在 commit（run 之后），故 staged 件必在全部即时件之后，两段各自保序。
    let staged_emitted: Vec<usize> =
        m.world.emitted.iter().filter(|e| archs[pos_of(e)].is_staged_inscope()).map(|e| pos_of(e)).collect();
    if !staged_emitted.is_empty() {
        assert!(m.trace.iter().any(|ev| matches!(ev, Ev::InsertedOnApproval { .. })), "staged 件未经插入出现于世界");
        assert_eq!(ending, Ending::ApproveEnough, "只有足额批准的收尾才可能放出 staged 件");
        w.inserted = true;
    }
    let immediate: Vec<usize> =
        m.world.emitted.iter().filter(|e| !archs[pos_of(e)].is_staged_inscope()).map(|e| pos_of(e)).collect();
    assert!(immediate.windows(2).all(|p| p[0] < p[1]), "即时段乱序：{:?}", immediate);
    assert!(staged_emitted.windows(2).all(|p| p[0] < p[1]), "插入段乱序：{:?}", staged_emitted);
    if let Some(last_im) = m.world.emitted.iter().rposition(|e| !archs[pos_of(e)].is_staged_inscope()) {
        if let Some(first_st) = m.world.emitted.iter().position(|e| archs[pos_of(e)].is_staged_inscope()) {
            assert!(first_st > last_im, "插入未殿后");
        }
    }
    if !immediate.is_empty() && !staged_emitted.is_empty() {
        w.mixed_emission = true;
    }

    // 【绝不悬置 / 绝不静默】（B4 的目标不变式）终态缓冲必空；被压制而未插入的
    // 件数必须有处置痕迹（SegmentAborted）盖住——无声滞留即红。
    assert_eq!(m.pending_suppressed(), 0, "终态仍有悬置缓冲（{:?} {:?} b? e={:?}）", archs, mode, ending);
    let suppressed = m.trace.iter().filter(|ev| matches!(ev, Ev::Suppressed { .. })).count();
    let inserted: usize = m
        .trace
        .iter()
        .filter_map(|ev| match ev {
            Ev::InsertedOnApproval { count, .. } => Some(*count),
            _ => None,
        })
        .sum();
    if suppressed > inserted {
        assert!(
            m.trace.iter().any(|ev| matches!(ev, Ev::SegmentAborted { .. })),
            "有压制件被弃置却无 SegmentAborted 痕迹（{:?} {:?} {:?}）", archs, mode, ending
        );
    }

    // 【结局与痕迹对账＋前缀界】
    let MonState::Done(out) = m.state() else { unreachable!() };
    let all_world_pos: Vec<usize> =
        m.world.emitted.iter().chain(m.world.standin.iter()).map(|e| pos_of(e)).collect();
    match out {
        MonOutcome::Completed => {
            w.completed += 1;
        }
        MonOutcome::FailStop { at } => {
            w.failstop += 1;
            assert!(m.trace.contains(&Ev::FailStop { at: *at }));
            assert!(all_world_pos.iter().all(|p| p < at), "FailStop 后仍有 ≥at 的世界动作");
            // 停点归因：要么该位在 sink 关口必停，要么是 Strict 的预算截停。
            let sink = archs[*at].sink_fails();
            let budget_ev = m.trace.contains(&Ev::BudgetExhausted { at: *at });
            match mode {
                Mode::Strict => assert!(sink || budget_ev, "Strict FailStop 停点无因"),
                Mode::Truncate | Mode::Escalate => assert!(sink, "非 Strict 的 FailStop 只能因 sink"),
            }
        }
        MonOutcome::Truncated { dropped } => {
            w.truncated += 1;
            assert_eq!(mode, Mode::Truncate, "只有 Truncate 模式产出 Truncated");
            assert!(m.trace.contains(&Ev::Truncation { dropped: *dropped }), "截断必须可听见");
            w.truncation_ev = true;
            let cut = archs.len() - dropped;
            assert!(all_world_pos.iter().all(|p| *p < cut), "截断点之后仍有世界动作");
        }
        MonOutcome::Aborted { expired } => {
            assert!(*expired, "穷举里 Aborted 只经 ttl 过期产生");
            w.aborted_expired += 1;
            assert!(m.trace.iter().any(|ev| matches!(ev, Ev::SegmentAborted { expired: true })));
            // 过期废段：staged 件零泄漏（上面恰好一次+零越界已保证不重不漏，
            // 此处再钉死：ExpireOnly/Short 收尾下 staged 位绝不在真实世界）。
            assert!(staged_emitted.is_empty(), "被废的段泄漏了 staged 件");
        }
    }

    // 【终态路径的救济痕迹见证】（供 sweep 末尾防空转）
    if m.trace.iter().any(|ev| matches!(ev, Ev::SegmentAborted { expired: false })) {
        w.terminal_discard = true;
    }
    if m.trace.iter().any(|ev| matches!(ev, Ev::Attenuated { .. })) {
        w.attenuated = true;
    }
    if m.trace.iter().any(|ev| matches!(ev, Ev::Confined { .. })) {
        w.confined = true;
    }
}

/// 主扫描：21,024 局全格点。任何一格断言失败都会携带完整格点坐标
///（原型串/模式/预算/收尾）——确定性穷举的失败天然可复现。
#[test]
fn deterministic_exhaustion_over_combination_lattice() {
    let mut w = Witness::default();
    for len in 1..=3usize {
        let count = 8usize.pow(len as u32);
        for code in 0..count {
            // 以 8 进制解码出原型串——枚举顺序确定、与平台无关。
            let mut archs = Vec::with_capacity(len);
            let mut c = code;
            for _ in 0..len {
                archs.push(ARCHS[c % 8]);
                c /= 8;
            }
            for mode in [Mode::Strict, Mode::Truncate, Mode::Escalate] {
                for budget in 0..=3u64 {
                    for ending in ENDINGS {
                        let m = drive(&archs, mode, budget, ending, &mut w);
                        check(&m, &archs, mode, ending, &mut w);
                        w.runs += 1;
                    }
                }
            }
        }
    }

    // 格点规模冻结：收缩枚举空间（少一个原型/少一轴）即红——
    // 防止未来"顺手简化"悄悄削弱覆盖。
    assert_eq!(w.runs, (8 + 64 + 512) * 3 * 4 * 3, "格点被收缩");

    // 见证断言：每类结局、每种救济路径都必须真的被走到过（防格点空转）。
    assert!(w.completed > 0 && w.failstop > 0 && w.truncated > 0 && w.aborted_expired > 0, "有结局类未见证");
    assert!(w.inserted, "无一局走到批准插入");
    assert!(w.terminal_discard, "无一局走到终止弃缓冲（B4 路径未被踩到）");
    assert!(w.approval_short, "无一局见证整批拒绝");
    assert!(w.truncation_ev && w.attenuated && w.confined, "有救济/截断痕迹未见证");
    assert!(w.multi_resume, "无一局出现 ≥2 次增批续跑");
    assert!(w.mixed_emission, "无一局同时含即时发射与批准插入");
}
