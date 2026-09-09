//! F2 deterministic cleanup drill using the production retirement transitions.
//! Crashes preserve the in-memory ledger and mock world; this does not model
//! SQLite durability. Wave shuffling exercises the mock's independence contract,
//! never a permission inferred from a parent graph or a verb boolean.

use crate::cleanup::*;
use crate::identity::{ClassId, Generation, HoldingHandle, HoldingId, InstanceId, SubjectId};
use crate::ledger::{Ledger, LiveItem, RevertGrade};
use crate::time::Timestamp;

// ---------------------------------------------------------------------------
// Journal is a drill trace, not recovery authority. Cleanup tasks in Ledger
// retain stable keys and attempts even if this observation trace is discarded.
// Only the kernel's storage coordinator establishes durable write ordering.
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
    pub holding_id: HoldingId,
    pub grade: RevertGrade,
    /// [KEY] 去重钥匙：subject/holding 决定，跨崩溃稳定。
    pub idem_key: String,
    pub state: JState,
}

#[derive(Default, Clone)]
pub struct Journal(pub Vec<JournalEntry>);

impl Journal {
    /// 演练回灌：从保留的原始条目重建进程内日志。
    pub fn from_entries(entries: Vec<JournalEntry>) -> Journal {
        Journal(entries)
    }
    /// Read the simulated action trace; production persistence uses CleanupRecord.
    pub fn entries(&self) -> &[JournalEntry] {
        &self.0
    }
    fn ensure(&mut self, holding_id: HoldingId, grade: RevertGrade, key: &str) -> usize {
        if let Some(i) = self.0.iter().position(|e| e.holding_id == holding_id) {
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
}

// ---------------------------------------------------------------------------
// 模拟基底世界：记录动作次序（供 [TREE] 断言）、按钥匙去重补偿（供 [KEY]）、
// 可注入单点故障（供失败隔离测试）。release 对已释放实例为空操作（[E-IDEM]）。
// ---------------------------------------------------------------------------
#[derive(Default)]
pub struct MockWorld {
    /// 已在世界侧释放的 (class, instance, generation)。
    released: std::collections::BTreeSet<(ClassId, InstanceId, Generation)>,
    /// 凭钥匙去重的补偿效果集：world 侧"恰好一次"的事实来源。
    compensated_keys: std::collections::BTreeSet<String>,
    /// 观察序列：每个成功动作按序记 holding_id（供次序断言）。
    pub action_order: Vec<HoldingId>,
    /// 补偿请求计数（含被去重的重试）——区分"请求次数"与"生效次数"。
    pub compensate_requests: u64,
    /// 注入故障：对该 holding 的前 n 次动作返回失败。
    pub fail_holding: Option<(HoldingId, u32)>,
    /// F6 [CEFF]：类 restore（把持有恢复到段起点检查点）——按钥匙去重，与补偿同款。
    restored_keys: std::collections::BTreeSet<String>,
    /// F6：restore 请求计数（含被去重的重试）。
    pub restore_requests: u64,
}

impl MockWorld {
    fn maybe_fail(&mut self, hid: HoldingId) -> bool {
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
    pub fn effects_fingerprint(&self) -> (Vec<(ClassId, InstanceId, Generation)>, Vec<String>) {
        (
            self.released.iter().cloned().collect(),
            self.compensated_keys.iter().cloned().collect(),
        )
    }
}

impl MockWorld {
    /// [E-IDEM] 有逆档：重复 release 是空操作，永远成功。
    fn release(&mut self, item: &LiveItem) -> Result<(), ()> {
        if self.maybe_fail(item.id) {
            return Err(());
        }
        let k = (
            item.class_id.clone(),
            item.instance.clone(),
            item.generation.clone(),
        );
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
    pub fn reset_faults(&mut self) {
        self.fail_holding = None;
    }
}

// ---------------------------------------------------------------------------
// 规划器：[TREE] 约束图只含 ownership 边；波次 = 按"子树深度"由深到浅分层。
// 同一波内没有 parent 次序约束；还须由 World 的独立性契约满足 [T43] 的前提。
// ---------------------------------------------------------------------------
pub fn plan_waves(ledger: &Ledger, subject: &str) -> Vec<Vec<HoldingId>> {
    // [B15] 规划对象是主体持有的 ownership 闭包（含跨主体后代），不是主体本人的行。
    plan_waves_over(&ledger.occupying_closure(&SubjectId::new(subject)))
}

/// 波次规划的共享核：对任意存活集（主体的闭包、某持有的子树）按"集内深度"分层。
fn plan_waves_over(items: &[LiveItem]) -> Vec<Vec<HoldingId>> {
    if items.is_empty() {
        return Vec::new();
    }
    let depth_of = |id: HoldingId| -> usize {
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
pub struct Orchestrator<W: CleanupExecutor = MockWorld> {
    pub ledger: Ledger,
    pub journal: Journal,
    pub world: W,
}

#[derive(PartialEq, Eq, Debug)]
pub enum RunOutcome {
    Completed { failed: Vec<HoldingId> },
    Crashed,
}

impl Orchestrator<MockWorld> {
    pub fn new(ledger: Ledger) -> Self {
        Self {
            ledger,
            journal: Journal::default(),
            world: MockWorld::default(),
        }
    }
}

impl Orchestrator<MockWorld> {
    pub fn resume(&mut self, subject: &str, order_seed: u64) -> RunOutcome {
        self.world.reset_faults();
        self.teardown(subject, order_seed, None)
    }
}

impl<W: CleanupExecutor> Orchestrator<W> {
    /// Drill entry with a cleanup executor; the domain transitions are shared.
    pub fn with_world(ledger: Ledger, world: W) -> Self {
        Self {
            ledger,
            journal: Journal::default(),
            world,
        }
    }

    /// [CRASH] 唯一清理通道。`order_seed` 决定波内执行序；满足 World 的独立性契约时，
    /// [T43] 保证不同种子的终态在所声明的观察下等价。
    /// `crash_at_step`：全局步进计数到达即"断电"（丢执行栈、保 ledger+journal+world）。
    pub fn teardown(
        &mut self,
        subject: &str,
        order_seed: u64,
        crash_at_step: Option<u64>,
    ) -> RunOutcome {
        teardown_with(
            &mut self.ledger,
            &mut self.journal,
            &mut self.world,
            subject,
            order_seed,
            crash_at_step,
            Timestamp::ZERO,
        )
    }
}

/// Drill executor; production storage has its own durable coordinator.
/// 语义与 [`Orchestrator::teardown`] 完全相同——后者只是它的薄包装。
pub fn teardown_with<W: CleanupExecutor>(
    ledger: &mut Ledger,
    journal: &mut Journal,
    world: &mut W,
    subject: &str,
    order_seed: u64,
    crash_at_step: Option<u64>,
    now: Timestamp,
) -> RunOutcome {
    run(
        ledger,
        journal,
        world,
        &|l| l.occupying_closure(&SubjectId::new(subject)),
        subject,
        order_seed,
        crash_at_step,
        now,
    )
}

/// 子树形态（WP-03 撤销级联）：清理域是以 `root` 为根的 ownership 子树（含跨主体
/// 后代），子先于父、根最后；主体名下的其他持有不受影响。`key_scope` 是 [KEY] 去重
/// 钥匙的稳定前缀（主体名，或根持有的稳定名如 cap_id）——跨崩溃必须稳定。
#[allow(clippy::too_many_arguments)]
pub fn teardown_subtree_with<W: CleanupExecutor>(
    ledger: &mut Ledger,
    journal: &mut Journal,
    world: &mut W,
    root: HoldingId,
    key_scope: &str,
    order_seed: u64,
    crash_at_step: Option<u64>,
    now: Timestamp,
) -> RunOutcome {
    run(
        ledger,
        journal,
        world,
        &|l| l.occupying_subtree(root),
        key_scope,
        order_seed,
        crash_at_step,
        now,
    )
}

/// 执行器内核：`scope` 给出当前存活集（每次从账本现状重算——计划无状态，
/// 状态全在账本与日志），其余纪律两形态同一。
#[allow(clippy::too_many_arguments)]
fn run<W: CleanupExecutor>(
    ledger: &mut Ledger,
    journal: &mut Journal,
    world: &mut W,
    scope: &dyn Fn(&Ledger) -> Vec<LiveItem>,
    key_scope: &str,
    order_seed: u64,
    crash_at_step: Option<u64>,
    now: Timestamp,
) -> RunOutcome {
    let worker = HostWitness::new(
        ProcessWitness::new(1, 1, Generation::new("drill-boot")).unwrap(),
        Generation::new("drill-worker"),
    )
    .unwrap();
    let items = scope(ledger);
    // Recover only attempts in this selected scope. A simulation crash kills its
    // sole worker; a real coordinator must establish that fact from its witness.
    for item in &items {
        let record = ledger
            .cleanup_tasks()
            .find(|t| t.holding.id() == item.id && matches!(t.state, CleanupState::Running { .. }))
            .map(|t| (**t).clone());
        if let Some(record) = record {
            ledger.abandon_cleanup(&record, now).unwrap();
        }
    }
    for item in items {
        ledger
            .request_retirement(
                &item.handle(),
                CleanupKey::new(format!("td:{key_scope}:{}", item.id)).unwrap(),
                now,
            )
            .unwrap();
    }
    let mut step = 0;
    let mut failed = Vec::new();
    let tick = |step: &mut u64| {
        *step += 1;
        crash_at_step.is_some_and(|c| *step >= c)
    };
    for wave in plan_waves_over(&scope(ledger)) {
        let mut order = wave;
        let mut seed = order_seed.wrapping_add(order.len() as u64);
        for i in (1..order.len()).rev() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            order.swap(i, (seed >> 33) as usize % (i + 1));
        }
        for hid in order {
            let task = ledger
                .cleanup_tasks()
                .find(|t| t.holding.id() == hid)
                .unwrap()
                .id;
            let Some(work) = ledger.claim_cleanup(task, worker.clone(), now).unwrap() else {
                continue;
            };
            let ji = journal.ensure(hid, work.grade(), work.task().key.as_str());
            journal.0[ji].state = JState::InFlight;
            if tick(&mut step) {
                return RunOutcome::Crashed;
            }
            let outcome = world.execute(&work);
            if tick(&mut step) {
                return RunOutcome::Crashed;
            }
            ledger.finish_cleanup(&work, outcome, now).unwrap();
            let done = ledger.cleanup_task(task).unwrap().state.is_done();
            journal.0[ji].state = if done { JState::Done } else { JState::Failed };
            if !done {
                failed.push(hid);
            }
            if tick(&mut step) {
                return RunOutcome::Crashed;
            }
        }
    }
    RunOutcome::Completed { failed }
}

// The drill world can also exercise the durable kernel coordinator. Its effects
// are idempotent by exact identity, and compensation deduplicates the stable key.
impl crate::cleanup::CleanupExecutor for MockWorld {
    fn execute(&mut self, work: &crate::cleanup::CleanupWork) -> crate::cleanup::CleanupOutcome {
        use crate::cleanup::CleanupOutcome;
        let item = LiveItem {
            id: work.task().holding.id(),
            parent: None,
            class_id: work.resource().class().clone(),
            instance: work.resource().instance().clone(),
            generation: work.task().holding.generation().clone(),
            grade: work.grade(),
        };
        let result = if work.grade() == RevertGrade::Compensable {
            self.compensate(&item, work.task().key.as_str()).map(|_| ())
        } else {
            self.release(&item)
        };
        match result {
            Ok(()) => CleanupOutcome::Confirmed,
            Err(()) => CleanupOutcome::Retryable("injected drill failure".into()),
        }
    }
}

/// Select a cleanup scope by handles, including ownership descendants. Labels
/// and subject formatting are not used to recover membership.
pub fn teardown_handles(
    ledger: &mut Ledger,
    journal: &mut Journal,
    world: &mut impl CleanupExecutor,
    handles: &[HoldingHandle],
    now: Timestamp,
) -> RunOutcome {
    for handle in handles {
        assert_eq!(
            ledger.holding(handle.id()).map(|h| h.handle()).as_ref(),
            Some(handle)
        );
    }
    run(
        ledger,
        journal,
        world,
        &|l| {
            let mut items = std::collections::BTreeMap::new();
            for h in handles {
                for item in l.occupying_subtree(h.id()) {
                    items.insert(item.id, item);
                }
            }
            items.into_values().collect()
        },
        "handles",
        0,
        None,
        now,
    )
}

/// Pure accounting fixtures still use the same retirement/claim/finish path.
/// Physical targets require a real or explicitly chosen mock executor.
pub struct AccountingWorld;
impl CleanupExecutor for AccountingWorld {
    fn execute(&mut self, work: &CleanupWork) -> CleanupOutcome {
        match work.target() {
            CleanupTarget::AccountingOnly => CleanupOutcome::Confirmed,
            _ => CleanupOutcome::Blocked("accounting fixture has no physical provider".into()),
        }
    }
}

pub trait LedgerDrill {
    fn sweep(&mut self, now: Timestamp) -> Vec<HoldingId>;
    fn teardown(&mut self, subject: &SubjectId, now: Timestamp) -> Vec<HoldingId>;
}
impl LedgerDrill for Ledger {
    fn sweep(&mut self, now: Timestamp) -> Vec<HoldingId> {
        let due = self.due_retirements(now);
        let selected: std::collections::BTreeSet<_> = due
            .iter()
            .map(|h| h.id())
            .chain(
                self.cleanup_tasks()
                    .filter(|t| !t.state.is_done())
                    .map(|t| t.holding.id()),
            )
            .collect();
        let mut journal = Journal::default();
        run(
            self,
            &mut journal,
            &mut AccountingWorld,
            &|l| {
                l.occupying()
                    .filter(|h| selected.contains(&h.id))
                    .map(|h| LiveItem {
                        id: h.id,
                        parent: h.parent,
                        class_id: h.class_id.clone(),
                        instance: h.instance.clone(),
                        generation: h.generation.clone(),
                        grade: l.grade_of(&h.class_id).unwrap(),
                    })
                    .collect()
            },
            "sweep",
            0,
            None,
            now,
        );
        journal
            .entries()
            .iter()
            .filter(|e| e.state == JState::Done)
            .map(|e| e.holding_id)
            .collect()
    }
    fn teardown(&mut self, subject: &SubjectId, now: Timestamp) -> Vec<HoldingId> {
        let mut journal = Journal::default();
        teardown_with(
            self,
            &mut journal,
            &mut AccountingWorld,
            subject.as_str(),
            0,
            None,
            now,
        );
        journal
            .entries()
            .iter()
            .filter(|e| e.state == JState::Done)
            .map(|e| e.holding_id)
            .collect()
    }
}
