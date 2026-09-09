//! F2 法则测试 —— 每个测试名 = 它执行的定理/纪律（对照 freeze-f2 三列表）。

use portos_rm::identity::{ClassId, Generation, HoldingId, InstanceId, ResourceKey, SubjectId};
use portos_rm::ledger::GrantRequest;
use portos_rm::ledger::*;
use portos_rm::ra::Ex;
use portos_rm::registry::{Capacity, Claim};
use portos_rm::test_support::teardown::*;
use portos_rm::time::{LeaseDuration, LeaseRequest, Timestamp};

/// 浏览器场景账本：隔离域 ⊃ {chromium ⊃ 3 页面, cdp-proxy, 端口, 工作区, 预约(可补偿)}。
fn browser_ledger() -> (Ledger, HoldingId) {
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
            cleanup: portos_rm::cleanup::CleanupPolicy::AccountingOnly,
            class_id: cid.into(),
            algebra: AlgebraTag::Exclusive,
            release_idempotent: true,
            lease_duration: lease.map(|s: u64| LeaseDuration::try_from(s).unwrap()),
            revert_grade: grade,
        })
        .unwrap();
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
        l.create_pool(
            &l.registered_class::<Ex>(&ClassId::new(*c)).unwrap(),
            InstanceId::new(*i),
            Capacity::new(Ex::Token).unwrap(),
        )
        .unwrap();
    }
    let e = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("enclosure"),
                InstanceId::new("drv"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let ch = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc"),
                InstanceId::new("chromium"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("pid7@t1"),
                parent: Some(e).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    l.grant(
        &l.pool::<Ex>(&ResourceKey::new(
            ClassId::new("proc"),
            InstanceId::new("cdp"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("drv"),
            claim: Claim::new(Ex::Token).unwrap(),
            generation: Generation::new("pid8@t1"),
            parent: Some(e).map(|id| l.holding(id).expect("parent exists").handle()),
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    for p in ["p1", "p2", "p3"] {
        l.grant(
            &l.pool::<Ex>(&ResourceKey::new(ClassId::new("page"), InstanceId::new(p)))
                .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("tgt"),
                parent: Some(ch).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    }
    l.grant(
        &l.pool::<Ex>(&ResourceKey::new(
            ClassId::new("tcp-port"),
            InstanceId::new("9222"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("drv"),
            claim: Claim::new(Ex::Token).unwrap(),
            generation: Generation::new("g"),
            parent: Some(e).map(|id| l.holding(id).expect("parent exists").handle()),
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    l.grant(
        &l.pool::<Ex>(&ResourceKey::new(
            ClassId::new("workspace"),
            InstanceId::new("ws"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("drv"),
            claim: Claim::new(Ex::Token).unwrap(),
            generation: Generation::new("g"),
            parent: Some(e).map(|id| l.holding(id).expect("parent exists").handle()),
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    l.grant(
        &l.pool::<Ex>(&ResourceKey::new(
            ClassId::new("reservation"),
            InstanceId::new("email-42"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("drv"),
            claim: Claim::new(Ex::Token).unwrap(),
            generation: Generation::new("g"),
            parent: Some(e).map(|id| l.holding(id).expect("parent exists").handle()),
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    (l, e)
}

fn fingerprint(
    o: &Orchestrator,
) -> (
    usize,
    usize,
    Vec<(ClassId, InstanceId, Generation)>,
    Vec<String>,
) {
    let (rel, comp) = o.world.effects_fingerprint();
    (o.ledger.live_count(), o.ledger.tombstone_count(), rel, comp)
}

/// [T43] 任意序撤销定理：波内任何执行序 ⇒ 终态逐字节相同；且 = 严格串行基准。
#[test]
fn t43_any_order_same_final_state() {
    let (l0, _) = browser_ledger();
    let mut base = Orchestrator::new(l0);
    assert!(matches!(
        base.teardown("drv", 0, None),
        RunOutcome::Completed { .. }
    ));
    let base_fp = fingerprint(&base);
    for seed in 1..20u64 {
        let (l, _) = browser_ledger();
        let mut o = Orchestrator::new(l);
        assert!(matches!(
            o.teardown("drv", seed, None),
            RunOutcome::Completed { .. }
        ));
        assert_eq!(fingerprint(&o), base_fp, "seed {seed} diverged");
    }
}

/// [TREE] ownership 边永不违反：世界侧动作序里，子恒先于父。
#[test]
fn ownership_edges_never_violated() {
    for seed in 0..20u64 {
        let (l, _) = browser_ledger();
        let edges: Vec<(HoldingId, HoldingId)> = l
            .live_snapshot(&SubjectId::new("drv"))
            .iter()
            .filter_map(|it| it.parent.map(|p| (it.id, p)))
            .collect();
        let mut o = Orchestrator::new(l);
        o.teardown("drv", seed, None);
        let pos = |id: HoldingId| o.world.action_order.iter().position(|x| *x == id);
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
        assert_eq!(
            comp.len(),
            1,
            "compensation effect count != 1 at crash {crash}"
        );
        if o.world.compensate_requests > 1 {
            witnessed_dup_request = true; // 重试确实发生过，但被钥匙去重
        }
    }
    assert!(
        witnessed_dup_request,
        "no ambiguous-window retry was exercised — test too weak"
    );
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
        .live_snapshot(&SubjectId::new("drv"))
        .iter()
        .find(|it| it.class_id.as_str() == "tcp-port")
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
    assert!(o.ledger.occupying().count() >= 2);
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
    for (cid, lease) in [
        ("enclosure", Some(600)),
        ("proc", Some(60)),
        ("tcp-port", Some(30)),
    ] {
        l.register_class(ClassDecl {
            cleanup: portos_rm::cleanup::CleanupPolicy::AccountingOnly,
            class_id: cid.into(),
            algebra: AlgebraTag::Exclusive,
            release_idempotent: true,
            lease_duration: lease.map(|s: u64| LeaseDuration::try_from(s).unwrap()),
            revert_grade: RevertGrade::Inverse,
        })
        .unwrap();
    }
    for (c, i) in [
        ("enclosure", "host"),
        ("proc", "child-main"),
        ("proc", "child-helper"),
        ("tcp-port", "9222"),
        ("tcp-port", "8080"),
    ] {
        l.create_pool(
            &l.registered_class::<Ex>(&ClassId::new(c)).unwrap(),
            InstanceId::new(i),
            Capacity::new(Ex::Token).unwrap(),
        )
        .unwrap();
    }
    // 父主体 host 持有 enclosure；子主体 child 的进程树与端口挂在 enclosure 之下（跨主体 ownership 边，
    // 沿实例化关系——决策 2 的权限规则，内核在拉起 child 时登记）。
    let encl = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("enclosure"),
                InstanceId::new("host"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("host"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    l.declare_instantiation(SubjectId::new("child"), SubjectId::new("host"));
    let cmain = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc"),
                InstanceId::new("child-main"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("child"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("pid1"),
                parent: Some(encl).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let chelp = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc"),
                InstanceId::new("child-helper"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("child"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("pid2"),
                parent: Some(cmain).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let cport = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("tcp-port"),
                InstanceId::new("9222"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("child"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("g"),
                parent: Some(cmain).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    // 子主体另有一笔不在 host 树下的持有——不得被 host 的 teardown 波及。
    let stray = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("tcp-port"),
                InstanceId::new("8080"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("child"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();

    assert_eq!(
        plan_waves(&l, "host").iter().flatten().count(),
        4,
        "规划器看见跨主体后代"
    );
    let mut o = Orchestrator::new(l);
    assert!(
        matches!(o.teardown("host", 9, None), RunOutcome::Completed { failed } if failed.is_empty())
    );
    let pos = |id: HoldingId| o.world.action_order.iter().position(|x| *x == id).unwrap();
    assert!(
        pos(chelp) < pos(cmain) && pos(cport) < pos(cmain) && pos(cmain) < pos(encl),
        "子先于父，跨主体亦然"
    );
    assert_eq!(o.ledger.live_snapshot(&SubjectId::new("host")).len(), 0);
    assert_eq!(
        o.ledger
            .live_snapshot(&SubjectId::new("child"))
            .iter()
            .map(|it| it.id)
            .collect::<Vec<_>>(),
        vec![stray],
        "树外持有不受波及"
    );
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

/// [TREE] 撤销子树（WP-03 的参照语义）：以某持有为根的清理——跨主体后代同收、
/// 子先于父、根最后；主体名下不在子树内的持有不动。
#[test]
fn subtree_teardown_is_children_first_and_spares_off_tree_holdings() {
    let fixture = || {
        let mut l = Ledger::new();
        for cid in ["cap", "pool", "route"] {
            l.register_class(ClassDecl {
                cleanup: portos_rm::cleanup::CleanupPolicy::AccountingOnly,
                class_id: cid.into(),
                algebra: AlgebraTag::Exclusive,
                release_idempotent: true,
                lease_duration: None,
                revert_grade: RevertGrade::Inverse,
            })
            .unwrap();
        }
        for (c, i) in [
            ("cap", "c1"),
            ("pool", "p1"),
            ("route", "r1"),
            ("cap", "other"),
        ] {
            l.create_pool(
                &l.registered_class::<Ex>(&ClassId::new(c)).unwrap(),
                InstanceId::new(i),
                Capacity::new(Ex::Token).unwrap(),
            )
            .unwrap();
        }
        let cap = l
            .grant(
                &l.pool::<Ex>(&ResourceKey::new(
                    ClassId::new("cap"),
                    InstanceId::new("c1"),
                ))
                .unwrap(),
                GrantRequest {
                    owner: SubjectId::new("plugin:a"),
                    claim: Claim::new(Ex::Token).unwrap(),
                    generation: Generation::new("c1"),
                    parent: None,
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(0u64).unwrap(),
                },
            )
            .map(|h| h.id())
            .unwrap();
        let pool = l
            .grant(
                &l.pool::<Ex>(&ResourceKey::new(
                    ClassId::new("pool"),
                    InstanceId::new("p1"),
                ))
                .unwrap(),
                GrantRequest {
                    owner: SubjectId::new("plugin:a"),
                    claim: Claim::new(Ex::Token).unwrap(),
                    generation: Generation::new("p1"),
                    parent: Some(cap).map(|id| l.holding(id).expect("parent exists").handle()),
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(0u64).unwrap(),
                },
            )
            .map(|h| h.id())
            .unwrap();
        // 跨主体后代：附着实例的路由挂在授权持有下（决策 2 的实例化边）。
        l.declare_instantiation(SubjectId::new("attach:x"), SubjectId::new("plugin:a"));
        let route = l
            .grant(
                &l.pool::<Ex>(&ResourceKey::new(
                    ClassId::new("route"),
                    InstanceId::new("r1"),
                ))
                .unwrap(),
                GrantRequest {
                    owner: SubjectId::new("attach:x"),
                    claim: Claim::new(Ex::Token).unwrap(),
                    generation: Generation::new("r1"),
                    parent: Some(cap).map(|id| l.holding(id).expect("parent exists").handle()),
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(0u64).unwrap(),
                },
            )
            .map(|h| h.id())
            .unwrap();
        let other = l
            .grant(
                &l.pool::<Ex>(&ResourceKey::new(
                    ClassId::new("cap"),
                    InstanceId::new("other"),
                ))
                .unwrap(),
                GrantRequest {
                    owner: SubjectId::new("plugin:a"),
                    claim: Claim::new(Ex::Token).unwrap(),
                    generation: Generation::new("other"),
                    parent: None,
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(0u64).unwrap(),
                },
            )
            .map(|h| h.id())
            .unwrap();
        (l, cap, pool, route, other)
    };
    for seed in 0..8u64 {
        let (mut l, cap, pool, route, other) = fixture();
        let mut j = Journal::default();
        let mut w = MockWorld::default();
        let out = teardown_subtree_with(
            &mut l,
            &mut j,
            &mut w,
            cap,
            "c1",
            seed,
            None,
            Timestamp::try_from(1u64).unwrap(),
        );
        assert!(matches!(out, RunOutcome::Completed { ref failed } if failed.is_empty()));
        let pos = |id: HoldingId| w.action_order.iter().position(|x| *x == id).unwrap();
        assert!(
            pos(pool) < pos(cap) && pos(route) < pos(cap),
            "子先于父，根最后"
        );
        assert!(
            l.holding(other).unwrap().released_at().is_none(),
            "子树外的持有不动"
        );
        assert!(l.holding(cap).unwrap().released_at().is_some(), "根本人落碑");
        l.invariant().unwrap();
    }
}
