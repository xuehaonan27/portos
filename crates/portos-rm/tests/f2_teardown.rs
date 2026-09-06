//! F2 法则测试 —— 每个测试名 = 它执行的定理/纪律（对照 freeze-f2 三列表）。

use portos_rm::ledger::*;
use portos_rm::ra::Ex;
use portos_rm::teardown::*;

/// 浏览器场景账本：隔离域 ⊃ {chromium ⊃ 3 页面, cdp-proxy, 端口, 工作区, 预约(可补偿)}。
fn browser_ledger() -> (Ledger, u64) {
    let mut l = Ledger::new();
    for (cid, lease, grade) in [
        ("enclosure", Some(600), RevertGrade::Inverse),
        ("proc", Some(60), RevertGrade::Inverse),
        ("page", Some(60), RevertGrade::Inverse),
        ("tcp-port", Some(30), RevertGrade::Inverse),
        ("workspace", Some(600), RevertGrade::Inverse),
        ("reservation", Some(300), RevertGrade::Compensable),
    ] {
        l.register_class(ClassDecl {
            class_id: cid.into(),
            algebra: AlgebraTag::Exclusive,
            release_idempotent: true,
            lease_secs: lease,
            revert_grade: grade,
        });
    }
    let caps: &[(&str, &str)] = &[
        ("enclosure", "drv"),
        ("proc", "chromium"),
        ("proc", "cdp"),
        ("page", "p1"),
        ("page", "p2"),
        ("page", "p3"),
        ("tcp-port", "9222"),
        ("workspace", "ws"),
        ("reservation", "email-42"),
    ];
    for (c, i) in caps {
        l.set_capacity(c, i, Frag::Ex(Ex::Token));
    }
    let e = l.grant("drv", "enclosure", "drv", Frag::Ex(Ex::Token), "g", None, 0).unwrap();
    let ch = l.grant("drv", "proc", "chromium", Frag::Ex(Ex::Token), "pid7@t1", Some(e), 0).unwrap();
    l.grant("drv", "proc", "cdp", Frag::Ex(Ex::Token), "pid8@t1", Some(e), 0).unwrap();
    for p in ["p1", "p2", "p3"] {
        l.grant("drv", "page", p, Frag::Ex(Ex::Token), "tgt", Some(ch), 0).unwrap();
    }
    l.grant("drv", "tcp-port", "9222", Frag::Ex(Ex::Token), "g", Some(e), 0).unwrap();
    l.grant("drv", "workspace", "ws", Frag::Ex(Ex::Token), "g", Some(e), 0).unwrap();
    l.grant("drv", "reservation", "email-42", Frag::Ex(Ex::Token), "g", Some(e), 0).unwrap();
    (l, e)
}

fn fingerprint(o: &Orchestrator) -> (usize, usize, Vec<(String, String, String)>, Vec<String>) {
    let (rel, comp) = o.world.effects_fingerprint();
    (o.ledger.live_count(), o.ledger.tombstone_count(), rel, comp)
}

/// [T43] 任意序撤销定理：波内任何执行序 ⇒ 终态逐字节相同；且 = 严格串行基准。
#[test]
fn t43_any_order_same_final_state() {
    let (l0, _) = browser_ledger();
    let mut base = Orchestrator::new(l0);
    assert!(matches!(base.teardown("drv", 0, None), RunOutcome::Completed { .. }));
    let base_fp = fingerprint(&base);
    for seed in 1..20u64 {
        let (l, _) = browser_ledger();
        let mut o = Orchestrator::new(l);
        assert!(matches!(o.teardown("drv", seed, None), RunOutcome::Completed { .. }));
        assert_eq!(fingerprint(&o), base_fp, "seed {seed} diverged");
    }
}

/// [TREE] ownership 边永不违反：世界侧动作序里，子恒先于父。
#[test]
fn ownership_edges_never_violated() {
    for seed in 0..20u64 {
        let (l, _) = browser_ledger();
        let edges: Vec<(u64, u64)> = l
            .live_snapshot("drv")
            .iter()
            .filter_map(|it| it.parent.map(|p| (it.id, p)))
            .collect();
        let mut o = Orchestrator::new(l);
        o.teardown("drv", seed, None);
        let pos = |id: u64| o.world.action_order.iter().position(|x| *x == id);
        for (child, parent) in &edges {
            let (Some(pc), Some(pp)) = (pos(*child), pos(*parent)) else {
                panic!("missing action for edge {child}->{parent}");
            };
            assert!(pc < pp, "child {child} after parent {parent} (seed {seed})");
        }
    }
}

/// [SAGA]+[E-IDEM]+[KEY] 大考：在每一个可崩点断电、重启续清，
/// 终态与一次不被打断的清理逐字节相同。
#[test]
fn crash_at_every_point_resumes_to_same_state() {
    let (l0, _) = browser_ledger();
    let mut base = Orchestrator::new(l0);
    base.teardown("drv", 3, None);
    let base_fp = fingerprint(&base);
    // 9 个持有 × 3 崩点 = 27 步；逐步注入崩溃。
    for crash in 1..=27u64 {
        let (l, _) = browser_ledger();
        let mut o = Orchestrator::new(l);
        let out = o.teardown("drv", 3, Some(crash));
        if out == RunOutcome::Crashed {
            // 崩溃丢的是执行栈；ledger/journal/world 存续 —— 恢复走同一条路径。
            let r = o.resume("drv", 3);
            assert!(matches!(r, RunOutcome::Completed { .. }));
        }
        assert_eq!(fingerprint(&o), base_fp, "crash point {crash} diverged");
        o.ledger.invariant().unwrap();
    }
}

/// [KEY] 可补偿动作在崩溃+重试调度下恰好生效一次；
/// 且暧昧窗口（世界已动、日志未 Done）确实产生了重复请求 —— 去重钥匙是承重件。
#[test]
fn compensation_exactly_once_and_key_is_load_bearing() {
    // 找到"预约补偿刚在世界生效、日志未标 Done"的崩点：逐点扫描。
    let mut witnessed_dup_request = false;
    for crash in 1..=27u64 {
        let (l, _) = browser_ledger();
        let mut o = Orchestrator::new(l);
        if o.teardown("drv", 3, Some(crash)) == RunOutcome::Crashed {
            o.resume("drv", 3);
        }
        let (_, comp) = o.world.effects_fingerprint();
        assert_eq!(comp.len(), 1, "compensation effect count != 1 at crash {crash}");
        if o.world.compensate_requests > 1 {
            witnessed_dup_request = true; // 重试确实发生过，但被钥匙去重
        }
    }
    assert!(witnessed_dup_request, "no ambiguous-window retry was exercised — test too weak");
}

/// [E-IDEM] 有逆档不依赖日志：清理途中把日志整本丢掉，盲重放仍收敛到同一终态。
/// （对比：可补偿档丢日志前必须先看世界侧钥匙集 —— 此处仅验证有逆档的独立性。）
#[test]
fn inverse_grade_survives_journal_loss() {
    let (l0, _) = browser_ledger();
    let mut base = Orchestrator::new(l0);
    base.teardown("drv", 5, None);
    let base_fp = fingerprint(&base);

    let (l, _) = browser_ledger();
    let mut o = Orchestrator::new(l);
    if o.teardown("drv", 5, Some(10)) == RunOutcome::Crashed {
        o.journal = Journal::default(); // 日志尽失
        o.resume("drv", 5);
    }
    // 世界侧补偿钥匙集仍在（对端记得），所以整体终态仍一致 ——
    // 这演示的是：恰好一次的最终承重在 [KEY] 对端去重，日志是把请求次数
    // 从 O(重启数) 压到 O(1) 的优化与审计件。
    assert_eq!(fingerprint(&o), base_fp);
}

/// 失败隔离：一件动作反复失败不拖死全场 —— 独立分支照常完成，
/// 失败项入 Failed，复位重试后收敛。[CRASH] 租约稍后重试的演练版。
#[test]
fn failed_branch_does_not_wedge() {
    let (l, _) = browser_ledger();
    let port_id = l
        .live_snapshot("drv")
        .iter()
        .find(|it| it.class_id == "tcp-port")
        .unwrap()
        .id;
    let mut o = Orchestrator::new(l);
    o.world.fail_holding = Some((port_id, 2)); // 前两次动它必失败
    let out = o.teardown("drv", 7, None);
    match out {
        RunOutcome::Completed { failed } => assert_eq!(failed, vec![port_id]),
        _ => panic!("should complete with one failed branch"),
    }
    // 端口的父（隔离域）被 TeardownOrder 保守挡下 —— 失败不越级殃及，也不越序强拆。
    assert!(o.ledger.live_count() >= 2);
    let r = o.resume("drv", 7);
    assert!(matches!(r, RunOutcome::Completed { failed } if failed.is_empty()));
    assert_eq!(o.ledger.live_count(), 0);
    o.ledger.invariant().unwrap();
}

/// [TREE]+[B15] ownership 边可以跨主体（plugin-system 裁定 6-7／§5.3：父插件死 ⇒ 子插件的持有
/// 沿 ownership 树拆）。规划器与执行器守卫都按主体持有的 ownership **闭包**工作：
/// 父主体 teardown 级联到子主体挂在其下的持有，子先于父；子主体挂在别处的持有不受波及。
/// v1 只看主体本人的行：父项 release 被 TeardownOrder 拒 ⇒ 执行器 panic（统一复核抓到）。
#[test]
fn teardown_cascades_across_subjects_along_ownership() {
    let mut l = Ledger::new();
    for (cid, lease) in [("enclosure", Some(600)), ("proc", Some(60)), ("tcp-port", Some(30))] {
        l.register_class(ClassDecl { class_id: cid.into(), algebra: AlgebraTag::Exclusive, release_idempotent: true, lease_secs: lease, revert_grade: RevertGrade::Inverse });
    }
    for (c, i) in [("enclosure", "host"), ("proc", "child-main"), ("proc", "child-helper"), ("tcp-port", "9222"), ("tcp-port", "8080")] {
        l.set_capacity(c, i, Frag::Ex(Ex::Token));
    }
    // 父主体 host 持有 enclosure；子主体 child 的进程树与端口挂在 enclosure 之下（跨主体 ownership 边，
    // 沿实例化关系——决策 2 的权限规则，内核在拉起 child 时登记）。
    let encl = l.grant("host", "enclosure", "host", Frag::Ex(Ex::Token), "g", None, 0).unwrap();
    l.declare_instantiation("child", "host");
    let cmain = l.grant("child", "proc", "child-main", Frag::Ex(Ex::Token), "pid1", Some(encl), 0).unwrap();
    let chelp = l.grant("child", "proc", "child-helper", Frag::Ex(Ex::Token), "pid2", Some(cmain), 0).unwrap();
    let cport = l.grant("child", "tcp-port", "9222", Frag::Ex(Ex::Token), "g", Some(cmain), 0).unwrap();
    // 子主体另有一笔不在 host 树下的持有——不得被 host 的 teardown 波及。
    let stray = l.grant("child", "tcp-port", "8080", Frag::Ex(Ex::Token), "g", None, 0).unwrap();

    assert_eq!(plan_waves(&l, "host").iter().flatten().count(), 4, "规划器看见跨主体后代");
    let mut o = Orchestrator::new(l);
    assert!(matches!(o.teardown("host", 9, None), RunOutcome::Completed { failed } if failed.is_empty()));
    let pos = |id: u64| o.world.action_order.iter().position(|x| *x == id).unwrap();
    assert!(pos(chelp) < pos(cmain) && pos(cport) < pos(cmain) && pos(cmain) < pos(encl), "子先于父，跨主体亦然");
    assert_eq!(o.ledger.live_snapshot("host").len(), 0);
    assert_eq!(o.ledger.live_snapshot("child").iter().map(|it| it.id).collect::<Vec<_>>(), vec![stray], "树外持有不受波及");
    o.ledger.invariant().unwrap();
}

/// [CRASH] 单路径（F1 的测试在 F2 机器上重演）：优雅清理与"崩溃后恢复"走同一代码、
/// 收敛同一终态；恢复没有专用路径 —— resume 就是再调 teardown。
#[test]
fn crash_only_single_path_holds_with_journal() {
    let (l0, _) = browser_ledger();
    let mut graceful = Orchestrator::new(l0);
    graceful.teardown("drv", 11, None);

    let (l1, _) = browser_ledger();
    let mut crashed = Orchestrator::new(l1);
    if crashed.teardown("drv", 11, Some(7)) == RunOutcome::Crashed {
        crashed.resume("drv", 11);
    }
    assert_eq!(fingerprint(&graceful), fingerprint(&crashed));
}
