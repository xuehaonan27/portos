//! Teardown planner + executor — freeze drill F2.
//!
//! 理论标签（对照 design/freeze-f2-teardown.md 的三列表）：
//!   [T43]   任意序撤销定理（Cordis 定理 43）：两两独立的效应，其逆可按任意顺序执行、
//!           各自只撤自己的贡献 —— 波内乱序并行的许可证。
//!   [TREE]  排序约束只长在 ownership 树的边上（子先于父）—— 排序定理/提供者守卫的
//!           资源侧形态；波次 = 约束图的拓扑分层。
//!   [SAGA]  Garcia-Molina–Salem：长事务 = 步骤 + 补偿；本模块的反向日志记录
//!           "清理进行到哪"，写在动手之前（write-ahead，ARIES/WAL 纪律）。
//!   [E-IDEM] F1 法则"release 幂等" ⇒ 有逆档恢复 = 盲重放，不依赖日志内容。
//!   [KEY]   可补偿档动作非幂等 ⇒ 必须携带去重钥匙，由补偿对端凭钥匙去重
//!           （exactly-once = at-least-once + 按钥匙去重）。
//!   [CRASH] crash-only 单路径：本执行器是唯一清理通道；优雅卸载 = 提前触发之。
//!   [ρ]     动作选择由类声明的可逆档决定（theory-spec §2.5）：有逆 → release；
//!           可补偿 → 记日志的补偿。等价由声明固定，不由执行者挑选。

use crate::ledger::{Ledger, LiveItem, RevertGrade};

/// F2 的世界接口：执行器对基底的动作。演练用 [`MockWorld`]；内核实装用真基底
/// （kill 进程、撤订阅、关 target……）。执行器只按类声明的 ρ 选动作（[ρ]），
/// 世界只负责"做"与"报告成败"——恰好一次的承重仍在钥匙去重（[KEY]）。
pub trait World {
    /// 有逆档：释放持有。须幂等——对已释放实例为空操作（[E-IDEM]：盲重放安全）。
    fn release(&mut self, item: &LiveItem) -> Result<(), ()>;
    /// 可补偿档：携钥匙请求补偿；返回是否真的生效（对端凭钥匙去重）。
    fn compensate(&mut self, item: &LiveItem, key: &str) -> Result<bool, ()>;
    /// 重试前复位注入故障（演练件用；实装无故障注入，默认空操作）。
    fn reset_faults(&mut self) {}
}

// ---------------------------------------------------------------------------
// 反向日志（saga-log）。演练中为进程内结构；实装落 SQLite —— 内核
// `crates/portos-kernel/src/ledger.rs` 的 `journal` 表，与墓碑同事务写穿，
// 重启后由该主体的下一次 teardown 回放。"耐久性"由崩溃模拟约定表达：崩溃丢
// Executor、保 Ledger+Journal —— 正是"账本与日志活过内核崩溃"的模拟。
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JState {
    Pending,
    InFlight,
    Done,
    Failed,
}

#[derive(Clone, Debug)]
pub struct JournalEntry {
    pub holding_id: u64,
    pub grade: RevertGrade,
    /// [KEY] 去重钥匙：subject/holding 决定，跨崩溃稳定。
    pub idem_key: String,
    pub state: JState,
}

#[derive(Default, Clone)]
pub struct Journal(pub Vec<JournalEntry>);

impl Journal {
    /// 实装回灌：从持久层载入的条目重建日志（内核 `journal` 表 → 内存日志）。
    pub fn from_entries(entries: Vec<JournalEntry>) -> Journal {
        Journal(entries)
    }
    /// 全部条目（实装写穿持久化用）。
    pub fn entries(&self) -> &[JournalEntry] {
        &self.0
    }
    fn ensure(&mut self, holding_id: u64, grade: RevertGrade, key: &str) -> usize {        if let Some(i) = self.0.iter().position(|e| e.holding_id == holding_id) {
            return i;
        }
        self.0.push(JournalEntry {
            holding_id,
            grade,
            idem_key: key.to_string(),
            state: JState::Pending,
        });
        self.0.len() - 1
    }
    fn state_of(&self, holding_id: u64) -> Option<JState> {
        self.0.iter().find(|e| e.holding_id == holding_id).map(|e| e.state)
    }
}

// ---------------------------------------------------------------------------
// 模拟基底世界：记录动作次序（供 [TREE] 断言）、按钥匙去重补偿（供 [KEY]）、
// 可注入单点故障（供失败隔离测试）。release 对已释放实例为空操作（[E-IDEM]）。
// ---------------------------------------------------------------------------
#[derive(Default)]
pub struct MockWorld {
    /// 已在世界侧释放的 (class, instance, generation)。
    released: std::collections::BTreeSet<(String, String, String)>,
    /// 凭钥匙去重的补偿效果集：world 侧"恰好一次"的事实来源。
    compensated_keys: std::collections::BTreeSet<String>,
    /// 观察序列：每个成功动作按序记 holding_id（供次序断言）。
    pub action_order: Vec<u64>,
    /// 补偿请求计数（含被去重的重试）——区分"请求次数"与"生效次数"。
    pub compensate_requests: u64,
    /// 注入故障：对该 holding 的前 n 次动作返回失败。
    pub fail_holding: Option<(u64, u32)>,
    /// F6 [CEFF]：类 restore（把持有恢复到段起点检查点）——按钥匙去重，与补偿同款。
    restored_keys: std::collections::BTreeSet<String>,
    /// F6：restore 请求计数（含被去重的重试）。
    pub restore_requests: u64,
}

impl MockWorld {
    fn maybe_fail(&mut self, hid: u64) -> bool {
        if let Some((fh, n)) = &mut self.fail_holding {
            if *fh == hid && *n > 0 {
                *n -= 1;
                return true;
            }
        }
        false
    }
    /// F6 [CEFF] 类 restore：把某持有恢复到检查点。非幂等动作（快照恢复会覆盖中间状态），
    /// 与补偿同款靠钥匙去重 ⇒ 恰好一次。返回是否真的生效。
    pub fn restore(&mut self, key: &str) -> bool {
        self.restore_requests += 1;
        self.restored_keys.insert(key.to_string())
    }
    pub fn restored(&self) -> Vec<String> {
        self.restored_keys.iter().cloned().collect()
    }
    pub fn effects_fingerprint(&self) -> (Vec<(String, String, String)>, Vec<String>) {
        (
            self.released.iter().cloned().collect(),
            self.compensated_keys.iter().cloned().collect(),
        )
    }
}

impl World for MockWorld {
    /// [E-IDEM] 有逆档：重复 release 是空操作，永远成功。
    fn release(&mut self, item: &LiveItem) -> Result<(), ()> {
        if self.maybe_fail(item.id) {
            return Err(());
        }
        let k = (item.class_id.clone(), item.instance.clone(), item.generation.clone());
        if self.released.insert(k) {
            self.action_order.push(item.id);
        }
        Ok(())
    }
    /// [KEY] 可补偿档：非幂等动作，靠钥匙在对端去重。返回是否真的生效。
    fn compensate(&mut self, item: &LiveItem, key: &str) -> Result<bool, ()> {
        if self.maybe_fail(item.id) {
            return Err(());
        }
        self.compensate_requests += 1;
        if self.compensated_keys.insert(key.to_string()) {
            self.action_order.push(item.id);
            return Ok(true);
        }
        Ok(false) // 被去重：请求了但未再生效
    }
    fn reset_faults(&mut self) {
        self.fail_holding = None;
    }
}

// ---------------------------------------------------------------------------
// 规划器：[TREE] 约束图只含 ownership 边；波次 = 按"子树深度"由深到浅分层。
// 同一波内的项两两无约束 —— [T43] 许可任意执行序。
// ---------------------------------------------------------------------------
pub fn plan_waves(ledger: &Ledger, subject: &str) -> Vec<Vec<u64>> {
    // [B15] 规划对象是主体持有的 ownership 闭包（含跨主体后代），不是主体本人的行。
    plan_waves_over(&ledger.live_closure(subject))
}

/// 波次规划的共享核：对任意存活集（主体的闭包、某持有的子树）按"集内深度"分层。
fn plan_waves_over(items: &[LiveItem]) -> Vec<Vec<u64>> {
    if items.is_empty() {
        return Vec::new();
    }
    let depth_of = |id: u64| -> usize {
        let mut d = 0;
        let mut cur = items.iter().find(|it| it.id == id).and_then(|it| it.parent);
        while let Some(p) = cur {
            d += 1;
            cur = items.iter().find(|it| it.id == p).and_then(|it| it.parent);
        }
        d
    };
    let maxd = items.iter().map(|it| depth_of(it.id)).max().unwrap_or(0);
    let mut waves = vec![Vec::new(); maxd + 1];
    for it in items {
        // 深度最大的最先清：waves[0] = 最深层（叶），末波 = 根。
        waves[maxd - depth_of(it.id)].push(it.id);
    }
    waves.retain(|w| !w.is_empty());
    waves
}

// ---------------------------------------------------------------------------
// 执行器（含崩溃模拟）。步进计数把每个持有拆成三个可崩点：
//   写日志后(1) / 世界动作后(2) / 标 Done 后(3) —— 覆盖 [SAGA] 的暧昧窗口。
// ---------------------------------------------------------------------------
pub struct Orchestrator<W: World = MockWorld> {
    pub ledger: Ledger,
    pub journal: Journal,
    pub world: W,
}

#[derive(PartialEq, Eq, Debug)]
pub enum RunOutcome {
    Completed { failed: Vec<u64> },
    Crashed,
}

impl Orchestrator<MockWorld> {
    pub fn new(ledger: Ledger) -> Self {
        Self { ledger, journal: Journal::default(), world: MockWorld::default() }
    }
}

impl<W: World> Orchestrator<W> {
    /// 实装入口：账本＋真基底世界（内核的 kill/撤订阅…）。
    pub fn with_world(ledger: Ledger, world: W) -> Self {
        Self { ledger, journal: Journal::default(), world }
    }

    /// [CRASH] 唯一清理通道。`order_seed` 决定波内执行序（[T43]：任何种子终态相同）；
    /// `crash_at_step`：全局步进计数到达即"断电"（丢执行栈、保 ledger+journal+world）。
    pub fn teardown(
        &mut self,
        subject: &str,
        order_seed: u64,
        crash_at_step: Option<u64>,
    ) -> RunOutcome {
        teardown_with(&mut self.ledger, &mut self.journal, &mut self.world, subject, order_seed, crash_at_step, 0)
    }

    /// 崩溃后恢复 = 原样再调 teardown（无专用恢复代码路径 —— [CRASH] 单路径的另一半）：
    /// 有逆档靠 [E-IDEM] 盲重放；可补偿档靠 [SAGA] 日志的 InFlight + [KEY] 去重。
    pub fn resume(&mut self, subject: &str, order_seed: u64) -> RunOutcome {
        // 将 Failed 复位为 Pending 以允许重试（重试策略属实装；演练中显式复位）。
        for e in self.journal.0.iter_mut() {
            if e.state == JState::Failed {
                e.state = JState::Pending;
            }
        }
        self.world.reset_faults();
        self.teardown(subject, order_seed, None)
    }
}

/// 执行器本体（自由函数形态，供内核在自己的锁纪律下对共享账本调用）。
/// 语义与 [`Orchestrator::teardown`] 完全相同——后者只是它的薄包装。
pub fn teardown_with<W: World>(
    ledger: &mut Ledger,
    journal: &mut Journal,
    world: &mut W,
    subject: &str,
    order_seed: u64,
    crash_at_step: Option<u64>,
    now: u64,
) -> RunOutcome {
    run(ledger, journal, world, &|l| l.live_closure(subject), subject, order_seed, crash_at_step, now)
}

/// 子树形态（WP-03 撤销级联）：清理域是以 `root` 为根的 ownership 子树（含跨主体
/// 后代），子先于父、根最后；主体名下的其他持有不受影响。`key_scope` 是 [KEY] 去重
/// 钥匙的稳定前缀（主体名，或根持有的稳定名如 cap_id）——跨崩溃必须稳定。
#[allow(clippy::too_many_arguments)]
pub fn teardown_subtree_with<W: World>(
    ledger: &mut Ledger,
    journal: &mut Journal,
    world: &mut W,
    root: u64,
    key_scope: &str,
    order_seed: u64,
    crash_at_step: Option<u64>,
    now: u64,
) -> RunOutcome {
    run(ledger, journal, world, &|l| l.live_subtree(root), key_scope, order_seed, crash_at_step, now)
}

/// 执行器内核：`scope` 给出当前存活集（每次从账本现状重算——计划无状态，
/// 状态全在账本与日志），其余纪律两形态同一。
#[allow(clippy::too_many_arguments)]
fn run<W: World>(
    ledger: &mut Ledger,
    journal: &mut Journal,
    world: &mut W,
    scope: &dyn Fn(&Ledger) -> Vec<LiveItem>,
    key_scope: &str,
    order_seed: u64,
    crash_at_step: Option<u64>,
    now: u64,
) -> RunOutcome {
    let mut step: u64 = 0;
    let mut failed = Vec::new();
    let mut passes: u32 = 0;
    let tick = |step: &mut u64| -> bool {
        *step += 1;
        matches!(crash_at_step, Some(c) if *step >= c)
    };
    loop {
        let waves = plan_waves_over(&scope(ledger));
        if waves.is_empty() {
            return RunOutcome::Completed { failed };
        }
        passes += 1;
        assert!(passes <= 4096, "teardown failed to make progress — planner/guard bug");
        let mut progressed = false;
        for wave in waves {
            let mut order = wave.clone();
            // [T43] 波内乱序：用种子洗牌 —— 定理保证任何顺序等效，测试据此断言。
            let mut s = order_seed.wrapping_add(order.len() as u64);
            for i in (1..order.len()).rev() {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                order.swap(i, (s >> 33) as usize % (i + 1));
            }
            for hid in order {
                // [B15] 守卫与规划器看同一视图：闭包（跨主体子项也算"存活子项"）。
                let snap = scope(ledger);
                let item = match snap.iter().find(|it| it.id == hid) {
                    Some(it) => it.clone(),
                    None => continue, // 已被此前（或崩溃前）清掉
                };
                // [TREE] 守卫：仍有存活子项的父项必须延迟——不写日志、不动世界。
                // （正常波次天然满足；子项 Failed 时这里就是"失败不越级殃及"的实现点。）
                if snap.iter().any(|it| it.parent == Some(hid)) {
                    continue;
                }
                // 跳过此前已判 Failed 的（留给重试调用）
                if journal.state_of(hid) == Some(JState::Failed) {
                    if !failed.contains(&hid) {
                        failed.push(hid);
                    }
                    continue;
                }
                let key = format!("td:{key_scope}:{hid}");
                // [SAGA] write-ahead：动手之前先记日志。
                let ji = journal.ensure(hid, item.grade, &key);
                journal.0[ji].state = JState::InFlight;
                if tick(&mut step) {
                    return RunOutcome::Crashed; // 崩点 1：日志已写、世界未动
                }
                // [ρ] 动作由类声明的可逆档决定。
                let ok = match item.grade {
                    RevertGrade::Inverse => world.release(&item).is_ok(),
                    RevertGrade::Compensable => {
                        // [KEY] 可补偿：携钥匙请求，对端去重 ⇒ 恰好一次。
                        world.compensate(&item, &key).is_ok()
                    }
                    RevertGrade::External => {
                        // 纯外部残迹不该作为持有出现（emission 不入账本）；防御性放行。
                        true
                    }
                };
                if tick(&mut step) {
                    return RunOutcome::Crashed; // 崩点 2：世界已动、日志未标 Done —— 暧昧窗口
                }
                if ok {
                    // 账本侧收尾（F1 的 release：幂等、落墓碑）。守卫已保证不会越序。
                    match ledger.release(hid, &item.generation, now) {
                        Ok(()) => {}
                        Err(e) => panic!("ledger release invariant broken: {e:?}"),
                    }
                    journal.0[ji].state = JState::Done;
                    progressed = true;
                } else {
                    journal.0[ji].state = JState::Failed;
                    failed.push(hid);
                }
                if tick(&mut step) {
                    return RunOutcome::Crashed; // 崩点 3：全部完成后
                }
            }
        }
        if !progressed {
            return RunOutcome::Completed { failed };
        }
    }
}
