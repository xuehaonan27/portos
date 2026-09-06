//! F6 法则测试（怪物志）—— 两个走查条目真的驱动 F1–F5 五套冻结机制跑通：
//! Workspace（microVM/容器）与 RDMA。每条测试名＝它证明的"落位"或"扩充承重"。

use portos_rm::bestiary::*;
use portos_rm::coeffect::*;
use portos_rm::ledger::*;
use portos_rm::monitor::*;
use portos_rm::ra::{Ex, Frac, Ranges};
use portos_rm::teardown::{Orchestrator, RunOutcome};
use portos_rm::verbs::*;

fn consent(n: &str, b: u64) -> Consent {
    Consent { plan_hash: "h".into(), budget: b, nonce: n.into(), ttl_expires_at: 100 }
}

/// [WORKSPACE] 条目通过全部冻结闸门：真理表一致（F4）、manifest 装载准入（F5）、
/// ownership 树 teardown 收敛且子先于父（F2）、投影：exec 界内变换进预算不扣发、
/// net_send 两阶段形状但可摊销、read_file 零预算。
#[test]
fn workspace_entry_passes_all_frozen_gates() {
    let e = workspace();
    e.table.check_all().unwrap();
    assert_eq!(admit_mount(&e.manifest, &e.mount), Ok(()));

    let hp = e.table.derive_handler_policy("vm");
    assert!(hp.contained.contains("exec") && hp.contained.contains("write_file"), "exec/写文件是界内变换");
    assert!(hp.budget.contains("exec") && !hp.withhold.contains("exec"), "变换进预算（fuel）、不扣发");
    assert!(!hp.budget.contains("read_file"), "可重复读零预算");
    let ns = e.table.lookup("vm", "net_send").unwrap();
    assert!(ns.staged_shape() && !ns.withhold(), "出网：两阶段由准入满足，可摊销不扣发");
    assert_eq!(e.table.derive_holding_grade("vm"), Some(RevertGrade::Inverse));

    // F2：enclosure ⊃ vm ⊃ {tap, overlay, snapshot, mount, proc}，子先于父收敛。
    let mut l = e.ledger;
    let ws = l.grant("agent", "enclosure", "ws-1", Frag::Ex(Ex::Token), "g", None, 0).unwrap();
    let vm = l.grant("agent", "vm", "vm-1", Frag::Ex(Ex::Token), "fc@1", Some(ws), 0).unwrap();
    for (c, i) in [("tap", "tap0"), ("rootfs-overlay", "ov-1"), ("snapshot", "snap-0"), ("mount", "m-1"), ("proc", "shell")] {
        l.grant("agent", c, i, Frag::Ex(Ex::Token), "g", Some(vm), 0).unwrap();
    }
    l.grant("agent", "vcpu", "host", Frag::Count(portos_rm::ra::Count(2)), "g", Some(vm), 0).unwrap();
    l.grant("agent", "image", "ubuntu-24.04", Frag::Set(portos_rm::ra::GSet::of(&["ro"])), "g", Some(vm), 0).unwrap();
    l.grant("other", "image", "ubuntu-24.04", Frag::Set(portos_rm::ra::GSet::of(&["ro"])), "g", None, 0).unwrap(); // 镜像可复制共享
    l.invariant().unwrap();
    let mut o = Orchestrator::new(l);
    assert!(matches!(o.teardown("agent", 3, None), RunOutcome::Completed { failed } if failed.is_empty()));
    assert_eq!(o.ledger.live_snapshot("agent").len(), 0);
    assert_eq!(o.ledger.live_snapshot("other").len(), 1, "别人的镜像份额不受影响");
    let pos = |id: u64| o.world.action_order.iter().position(|x| *x == id).unwrap();
    assert!(pos(vm) < pos(ws), "vm 先于 enclosure");
}

/// [CEFF] 段回滚＝类 restore：三次界内变换触及同一 vm，回滚只 restore 一次（钥匙去重），
/// 再次回滚不重复；已发射的 w-effect（若有）站着不动。这是 Workspace 走查里
/// "硬塞三分类都会错"的那一档的正确落位。
#[test]
fn workspace_segment_rollback_restores_touched_vm_exactly_once() {
    let e = workspace();
    let hp = e.table.derive_handler_policy("vm");
    let mut pol = Policy::default();
    pol.handler = "vm".into();
    pol.contained = hp.contained.clone();
    pol.staged_verbs = hp.withhold.clone();
    for v in ["exec", "write_file", "net_send"] {
        pol.allow(v, "vm-1");
    }
    pol.allow("net_send", "api.example.com");
    let mut m = Monitor::new(pol, Mode::Strict, Ledger::new(), "fib");
    let plan = vec![
        WAction::new("exec", "vm-1", "make", 3),
        WAction::new("write_file", "vm-1", "cfg", 1),
        WAction::new("exec", "vm-1", "test", 4),
        WAction::new("net_send", "evil.example", "leak", 1), // 越界 ⇒ fail-stop
    ];
    m.admit(plan, "h", consent("n1", 20), 10).unwrap();
    assert_eq!(*m.run(10), MonState::Done(MonOutcome::FailStop { at: 3 }));
    assert_eq!(m.world.emitted.len(), 3, "三次界内变换已执行（前缀交付）");

    // fail-stop 终态即自动回滚（[SEG-TX]）；下面两次显式调用验证幂等。
    assert_eq!(m.orch.world.restored().len(), 1, "触及同一持有三次，只 restore 一次");
    m.rollback_segment(10);
    assert_eq!(m.orch.world.restore_requests, 1);
    assert_eq!(m.trace.iter().filter(|ev| matches!(ev, Ev::Restored { .. })).count(), 1);
    m.rollback_segment(10);
    assert_eq!(m.orch.world.restored().len(), 1, "重复回滚不重复 restore（钥匙承重）");
    assert_eq!(m.world.emitted.len(), 3, "发射记录不因回滚消失——回滚的是持有状态，不是历史");
}

/// [SAT]/按量计价：静态准入用声明上界（exec ≤5/次），运行期按实际 cost 扣，同一闸门衔接：
/// 实际 ≤ 声明 ⇒ 静态过则运行期必过；超出声明的实际用量在运行期被同一闸门拦住。
#[test]
fn workspace_weighted_budget_static_bound_covers_metered_spend() {
    let e = workspace();
    let lookup = |_: &str, v: &str| e.manifest.verbs[v].clone();
    let plan = Plan::Seq(vec![Plan::loop_(3, Plan::verb("vm", "exec")), Plan::loop_(2, Plan::verb("vm", "write_file"))]);
    let d = admit_plan(&plan, &lookup, &ceiling(&e.mount.offers, &e.mount.offers), &e.mount.provides, &Budget::of(&[("vm::exec", 15), ("vm::write_file", 4)])).unwrap();
    assert_eq!(d.uses, Budget::of(&[("vm::exec", 15), ("vm::write_file", 4)]), "静态上界＝3×5＋2×2");

    // 运行期：实际 fuel 3,4,5 ≤ 声明 5/次 ⇒ 12 ≤ 15 通过；第四次 6 超预算被同一闸门拦。
    let mut pol = Policy::default();
    pol.handler = "vm".into();
    pol.allow("exec", "vm-1");
    let mut m = Monitor::new(pol, Mode::Strict, Ledger::new(), "fib");
    let metered = vec![WAction::new("exec", "vm-1", "a", 3), WAction::new("exec", "vm-1", "b", 4), WAction::new("exec", "vm-1", "c", 5), WAction::new("exec", "vm-1", "d", 6)];
    m.admit(metered, "h", consent("n1", 15), 10).unwrap();
    assert_eq!(*m.run(10), MonState::Done(MonOutcome::FailStop { at: 3 }));
    assert_eq!(m.pool_spent("n1"), 12, "实际计量 12 ≤ 静态上界 15");
}

/// [RDMA] 条目通过全部冻结闸门，且三项库级扩充在真实形状上承重：
/// 区间代数（不相交 memory window 并授、重叠拒）、分数代数（三方共享读、第四方拒）、
/// 协议列（QP 状态机投影给 qp handler）；grant_remote_access 进硬清单（逐次同意）。
#[test]
fn rdma_entry_passes_all_frozen_gates_with_interval_and_frac() {
    let e = rdma();
    e.table.check_all().unwrap();
    assert_eq!(admit_mount(&e.manifest, &e.mount), Ok(()));

    // 投影
    let mr = e.table.derive_handler_policy("mr");
    assert_eq!(mr.withhold, ["grant_remote_access".to_string()].into_iter().collect(), "暴露内存＝硬清单");
    let qp = e.table.derive_handler_policy("qp");
    assert!(qp.protocol.is_some(), "QP 状态机投影给 qp handler");
    assert!(qp.contained.contains("init") && qp.contained.contains("rts"), "状态迁移是界内变换");
    assert!(qp.budget.contains("post_send") && !qp.withhold.contains("post_send"), "RDMA WRITE 可摊销");
    assert!(matches!(e.table.lookup("cq", "poll_cq").unwrap().kind, Kind::Consuming { world: ConsumeGrade::External }));

    // F1：设备可复制共享；PD 独占；MR 子区间并授/重叠拒；共享读份额。
    let mut l = e.ledger;
    let dev = l.grant("a", "device", "mlx5_0", Frag::Set(portos_rm::ra::GSet::of(&["open"])), "g", None, 0).unwrap();
    l.grant("b", "device", "mlx5_0", Frag::Set(portos_rm::ra::GSet::of(&["open"])), "g", None, 0).unwrap();
    let pd = l.grant("a", "pd", "pd-1", Frag::Ex(Ex::Token), "g", Some(dev), 0).unwrap();
    assert_eq!(l.grant("b", "pd", "pd-1", Frag::Ex(Ex::Token), "g", None, 0), Err(LedgerError::Conflict), "PD 独占");
    let w1 = l.grant("a", "mr", "buf-A", Frag::Range(Ranges::of(&[(0, 4096)])), "g", Some(pd), 0).unwrap();
    let _w2 = l.grant("a", "mr", "buf-A", Frag::Range(Ranges::of(&[(4096, 8192)])), "g", Some(pd), 0).unwrap();
    assert_eq!(l.grant("a", "mr", "buf-A", Frag::Range(Ranges::of(&[(1000, 2000)])), "g", Some(pd), 0), Err(LedgerError::Conflict), "重叠窗口拒");
    for q in ["q1", "q2", "q3"] {
        l.grant(q, "mr-read", "buf-A", Frag::Frac(Frac::new(1, 3)), "g", None, 0).unwrap();
    }
    assert_eq!(l.grant("q4", "mr-read", "buf-A", Frag::Frac(Frac::new(1, 3)), "g", None, 0), Err(LedgerError::Conflict), "共享读份额超 1 拒");
    l.grant("a", "qp", "qp-1", Frag::Ex(Ex::Token), "g", Some(pd), 0).unwrap();
    l.invariant().unwrap();
    // F2：windows/qp 先于 pd、pd 先于 device；"b" 的设备份额不受影响。
    let mut o = Orchestrator::new(l);
    assert!(matches!(o.teardown("a", 5, None), RunOutcome::Completed { failed } if failed.is_empty()));
    let pos = |id: u64| o.world.action_order.iter().position(|x| *x == id).unwrap();
    assert!(pos(w1) < pos(pd) && pos(pd) < pos(dev));
    assert_eq!(o.ledger.live_snapshot("b").len(), 1);
}

/// [RDMA][PROTO]→F3：qp handler 的监督器带协议：合法生命周期原样通过并按量计费；
/// 违规（未 rts 即 post_send）在首违规处截停。静态检查（准入期）与运行期判定一致。
#[test]
fn rdma_qp_protocol_enforced_end_to_end() {
    let e = rdma();
    let hp = e.table.derive_handler_policy("qp");
    let mut pol = Policy::default();
    pol.handler = "qp".into();
    pol.protocol = hp.protocol.clone();
    pol.contained = hp.contained.clone();
    for v in ["init", "rtr", "rts", "post_send", "query"] {
        pol.allow(v, "qp-1");
    }
    let ok = vec![
        WAction::new("init", "qp-1", "", 1),
        WAction::new("rtr", "qp-1", "", 1),
        WAction::new("query", "qp-1", "", 0),
        WAction::new("rts", "qp-1", "", 1),
        WAction::new("post_send", "qp-1", "64k", 64),
    ];
    let mut m = Monitor::new(pol.clone(), Mode::Strict, Ledger::new(), "fib");
    m.admit(ok.clone(), "h", consent("n1", 100), 10).unwrap();
    assert_eq!(*m.run(10), MonState::Done(MonOutcome::Completed));
    assert_eq!(m.pool_spent("n1"), 67, "按量：3 次迁移各 1 ＋ 64 KiB 写");

    let bad = vec![WAction::new("init", "qp-1", "", 1), WAction::new("post_send", "qp-1", "", 64)];
    let mut m = Monitor::new(pol, Mode::Strict, Ledger::new(), "fib");
    m.admit(bad.clone(), "h", consent("n1", 100), 10).unwrap();
    assert_eq!(*m.run(10), MonState::Done(MonOutcome::FailStop { at: 1 }));
    assert_eq!(m.world.emitted.len(), 1);
    // 准入期就能预言
    let proto = hp.protocol.unwrap();
    let plan = Plan::Seq(bad.iter().map(|a| Plan::verb("qp", &a.verb)).collect());
    assert!(proto.check_plan(&plan).is_err());
    let good_plan = Plan::Seq(ok.iter().map(|a| Plan::verb("qp", &a.verb)).collect());
    assert!(proto.check_plan(&good_plan).is_ok());
}
