//! F8 法则测试 —— 每个测试名 = 它执行的定理/纪律（对照 attachments-v0.md v0.5 §11 拟决与 §13.3 墓碑）。

use portos_rm::attach::*;

const NAV: &str = "browser::navigate";
const GRANT: &str = "grant_5b";

fn decl(id: &str, trigger: Trigger, budget: &[(&str, u64)], n_max: u64) -> Declaration {
    Declaration {
        id: id.into(),
        h_plan: "blake3:e4a1".into(),
        trigger,
        require_integrity: true,
        budget_firing: Budget::of(budget),
        n_max,
        ttl: 1000,
        h_table: "blake3:77c0".into(),
        min_interval: 0,
        queue_depth: 2,
        overflow: Overflow::DropOldest,
        failure_budget: 3,
        run_cap: 10,
        nonce: "nonce-1".into(),
        grants: vec![GRANT.into()],
    }
}

fn manual(id: &str, budget: &[(&str, u64)], n_max: u64) -> Declaration {
    decl(id, Trigger::Manual, budget, n_max)
}

#[test]
fn total_budget_overflow_refuses_attachment_before_creating_pools() {
    let mut s = Scheduler::new(true);
    let initial_holdings = s.ledger.live_count();
    assert_eq!(
        s.attach(manual("overflow", &[(NAV, u64::MAX)], 2), 0),
        Err(AttachError::BudgetOverflow)
    );
    assert!(s.attachments.is_empty());
    assert_eq!(s.ledger.live_count(), initial_holdings);
    assert!(s.cached_outstanding.is_empty());
    assert!(s.audit.is_empty());
    assert!(s.invariants().is_ok());
    // Refusal must also leave the declaration ID reusable.
    s.attach(manual("overflow", &[(NAV, 1)], 2), 0).unwrap();
    assert!(s.invariants().is_ok());
}

/// [Q12] 回归 a：零预算（全 Repeatable）计划的 n_max 仍经 `attach::fire` 分量封顶；
/// 有预算的计划由 fire 分量与类分量同时封顶。
#[test]
fn n_max_is_enforced_through_the_fire_component_even_for_zero_budget_plans() {
    let mut s = Scheduler::new(true);
    s.attach(manual("zero", &[], 3), 0).unwrap();
    for t in 0..3 {
        assert_eq!(s.fire("zero", &Run::ok(&[], 0), t), Ok(t + 1));
    }
    assert_eq!(s.fire("zero", &Run::ok(&[], 0), 3), Err(Reject::NMax));
    assert_eq!(s.fired_count("zero"), 3);
    assert!(s.audit.iter().any(|a| matches!(a, Audit::Rejected { id, why: Reject::NMax } if id == "zero")));
    assert_eq!(s.cached_outstanding.get("zero/attach::fire"), Some(&3));
    assert!(s.invariants().is_ok(), "{:?}", s.invariants());

    s.attach(manual("spider", &[(NAV, 21)], 2), 10).unwrap();
    assert_eq!(s.fire("spider", &Run::ok(&[(NAV, 3)], 0), 10), Ok(1));
    assert_eq!(s.fire("spider", &Run::ok(&[(NAV, 21)], 0), 11), Ok(2));
    assert_eq!(s.fire("spider", &Run::ok(&[(NAV, 1)], 0), 12), Err(Reject::NMax));
    // 类分量与 fire 分量同时耗尽：B_total = scale(n_max, B_firing)。
    assert_eq!(s.cached_outstanding.get(&format!("spider/{NAV}")), Some(&42));
    assert!(s.invariants().is_ok(), "{:?}", s.invariants());
}

/// [SEQ] seq 从账本行读，重启后接续；nonce_i = H(h_attach‖seq) 唯一、确定、可预测（Q10：无妨）。
#[test]
fn seq_is_read_from_rows_and_nonces_are_unique_and_stable_across_restart() {
    let mut s = Scheduler::new(true);
    s.attach(manual("a", &[(NAV, 2)], 5), 0).unwrap();
    s.fire("a", &Run::ok(&[(NAV, 1)], 0), 0).unwrap();
    s.fire("a", &Run::ok(&[(NAV, 1)], 0), 1).unwrap();
    s.crash();
    s.recover(2);
    assert_eq!(s.next_seq("a"), 3, "seq is the number of live fire rows + 1, not a memory counter");
    assert_eq!(s.fire("a", &Run::ok(&[], 0), 3), Ok(3));
    let nonces = s.nonces("a");
    let distinct: std::collections::BTreeSet<_> = nonces.iter().collect();
    assert_eq!(distinct.len(), 3);
    let h = s.attachments["a"].h_attach.clone();
    assert_eq!(nonces[1], derive_nonce(&h, 2));
    // 可预测：另一台内核对同一份签署声明派生出同一个 nonce——这是记账，不是秘密。
    let mut other = Scheduler::new(true);
    other.attach(manual("a", &[(NAV, 2)], 5), 0).unwrap();
    other.fire("a", &Run::ok(&[], 0), 0).unwrap();
    assert_eq!(other.nonces("a")[0], nonces[0]);
    assert!(s.invariants().is_ok(), "{:?}", s.invariants());
}

/// [Q14] 回归 b：②③之间崩溃——无事务时留下有②无③，恢复规则记为已触发、空跑（占 n_max、nonce 不重用）；
/// 有事务时什么都没有，seq 不前进。
#[test]
fn fragment_without_pool_after_crash_is_a_fired_but_empty_run() {
    let mut s = Scheduler::new(false);
    s.attach(manual("a", &[(NAV, 2)], 2), 0).unwrap();
    assert_eq!(s.fire("a", &Run::crash(Crash::BetweenRows), 0), Err(Reject::Crashed));
    s.recover(1);
    assert!(s.audit.iter().any(|a| matches!(a, Audit::RecoveredEmpty { id, seq: 1 } if id == "a")));
    let f = &s.attachments["a"].firings[0];
    assert_eq!((f.seq, f.end), (1, Some(End::CompletedEmpty)));
    assert_eq!(s.fired_count("a"), 1, "the empty run occupies n_max (宁紧勿漏)");
    assert_eq!(s.fire("a", &Run::ok(&[], 0), 2), Ok(2));
    assert_ne!(s.nonces("a")[0], s.nonces("a")[1]);
    assert_eq!(s.fire("a", &Run::ok(&[], 0), 3), Err(Reject::NMax));
    assert!(s.invariants().is_ok(), "{:?}", s.invariants());

    let mut t = Scheduler::new(true);
    t.attach(manual("a", &[(NAV, 2)], 2), 0).unwrap();
    assert_eq!(t.fire("a", &Run::crash(Crash::BetweenRows), 0), Err(Reject::Crashed));
    t.recover(1);
    assert!(!t.audit.iter().any(|a| matches!(a, Audit::RecoveredEmpty { .. })));
    assert_eq!(t.fired_count("a"), 0);
    assert_eq!(t.next_seq("a"), 1, "a transaction leaves only two states: nothing, or both rows");
    assert!(t.invariants().is_ok(), "{:?}", t.invariants());
}

#[derive(Clone, Copy, Debug)]
enum Op {
    FireOk,
    FireFail,
    FireOver,
    CrashBetween,
    CrashAfter,
    RejectPre,
    Detach,
    Sweep,
    Revoke,
}
const OPS: [Op; 9] = [
    Op::FireOk,
    Op::FireFail,
    Op::FireOver,
    Op::CrashBetween,
    Op::CrashAfter,
    Op::RejectPre,
    Op::Detach,
    Op::Sweep,
    Op::Revoke,
];

fn apply(s: &mut Scheduler, op: Op, now: &mut u64) {
    *now += 1;
    match op {
        Op::FireOk => {
            s.event("t", true, *now);
            let _ = s.fire("a", &Run::ok(&[(NAV, 1)], 1), *now);
        }
        Op::FireFail => {
            s.event("t", true, *now);
            let _ = s.fire("a", &Run::fail(&[(NAV, 2)], 1), *now);
        }
        Op::FireOver => {
            s.event("t", true, *now);
            let _ = s.fire("a", &Run::ok(&[(NAV, 5)], 0), *now);
        }
        Op::CrashBetween => {
            s.event("t", true, *now);
            let _ = s.fire("a", &Run::crash(Crash::BetweenRows), *now);
            if s.crashed {
                s.recover(*now);
            }
        }
        Op::CrashAfter => {
            s.event("t", true, *now);
            let _ = s.fire("a", &Run::crash(Crash::AfterRows), *now);
            if s.crashed {
                s.recover(*now);
            }
        }
        Op::RejectPre => {
            s.event("t", false, *now);
            let _ = s.fire("a", &Run::ok(&[], 0), *now);
        }
        Op::Detach => s.detach("a", *now),
        Op::Sweep => {
            *now += 1000;
            s.tick(*now);
        }
        Op::Revoke => {
            s.revoke(GRANT, *now);
        }
    }
}

/// 回归 c（F8 格点）：任意触发／崩溃／拒绝／detach／到期／撤销序列之后，由存活碎片重算的
/// outstanding ＝ `cached_outstanding`，账本 Auth 不变式成立，已触发 ≤ n_max，nonce 唯一，
/// 已结账触发的③拒铸，退役附着名下无存活行——两种事务模式都成立。
#[test]
fn outstanding_recomputed_from_live_fragments_equals_cache_after_any_sequence() {
    let depth = 4;
    let mut runs = 0usize;
    for transactional in [true, false] {
        let total = OPS.len().pow(depth as u32);
        for code in 0..total {
            let mut s = Scheduler::new(transactional);
            s.attach(decl("a", Trigger::Topic { topic: "t".into() }, &[(NAV, 3)], 3), 0).unwrap();
            let mut now = 0u64;
            let mut c = code;
            let mut trace = Vec::new();
            for _ in 0..depth {
                let op = OPS[c % OPS.len()];
                c /= OPS.len();
                trace.push(op);
                apply(&mut s, op, &mut now);
                if let Err(e) = s.invariants() {
                    panic!("transactional={transactional} trace={trace:?}: {e}");
                }
            }
            runs += 1;
        }
    }
    assert_eq!(runs, 2 * OPS.len().pow(depth as u32));
}

/// [Q13] 任何终态走同一条结账路径：段主体无存活行、③容量置 0 且拒铸、②留存为花费、审计 Settled。
#[test]
fn settlement_is_one_path_for_every_terminal_state() {
    let scenarios: Vec<(&str, End)> = vec![
        ("completed", End::Completed),
        ("failstop", End::FailStop),
        ("truncated", End::Truncated),
        ("crashed", End::Crashed),
        ("expired", End::Expired),
        ("aborted", End::Aborted),
    ];
    for (name, expect) in scenarios {
        let mut s = Scheduler::new(true);
        let mut d = manual("a", &[(NAV, 2)], 5);
        d.ttl = 50;
        s.attach(d, 0).unwrap();
        match expect {
            End::Completed => {
                s.fire("a", &Run::ok(&[(NAV, 1)], 1), 1).unwrap();
            }
            End::FailStop => {
                s.fire("a", &Run::fail(&[(NAV, 1)], 1), 1).unwrap();
            }
            End::Truncated => {
                s.fire("a", &Run::ok(&[(NAV, 3)], 1), 1).unwrap();
            }
            End::Crashed => {
                assert_eq!(s.fire("a", &Run::crash(Crash::AfterRows), 1), Err(Reject::Crashed));
                s.recover(2);
            }
            End::Expired => {
                s.begin("a", 45).unwrap();
                s.tick(50);
            }
            End::Aborted => {
                s.begin("a", 1).unwrap();
                s.detach("a", 2);
            }
            _ => unreachable!(),
        }
        let f = &s.attachments["a"].firings[0];
        assert_eq!(f.end, Some(expect), "{name}");
        assert!(s.inflight_seq("a").is_none(), "{name}");
        assert!(!s.ledger.live().any(|h| h.subject == "attach/a:seg#1"), "{name}: segment rows gone");
        assert!(s.firing_pool_closed("a", 1, NAV), "{name}: firing pool capacity is 0");
        assert!(s.firing_pool_refuses("a", 1, NAV), "{name}: closed pool refuses minting");
        if s.status("a").is_terminal() {
            // 整体退役（规则 4）：②随①落墓碑——行仍在，只是不再存活。
            assert!(
                s.ledger.holdings().iter().any(|h| h.subject == "attach/a:spent" && h.generation == "seq:1" && h.released_at.is_some()),
                "{name}: the spend row is retained as a tombstone"
            );
        } else {
            assert_eq!(s.fired_count("a"), 1, "{name}: the spend row stays live");
        }
        assert!(s.audit.iter().any(|a| matches!(a, Audit::Settled { seq: 1, end, .. } if *end == expect)), "{name}");
        assert!(s.invariants().is_ok(), "{name}: {:?}", s.invariants());
    }
}

/// [Q11/Q13 规则 3] 未用余额不退：用了 3 次也按 21 计入总量；被拒绝的触发不铸行、不占 seq、不算失败。
#[test]
fn unused_balance_is_never_refunded_and_rejected_firings_mint_nothing() {
    let mut s = Scheduler::new(true);
    s.attach(decl("a", Trigger::Topic { topic: "t".into() }, &[(NAV, 21)], 3), 0).unwrap();
    s.event("t", true, 1);
    s.fire("a", &Run::ok(&[(NAV, 3)], 0), 1).unwrap();
    assert_eq!(s.cached_outstanding.get(&format!("a/{NAV}")), Some(&21));
    assert_eq!(s.recompute_outstanding().get(&format!("a/{NAV}")), Some(&21));
    let rows_before = s.ledger.holdings().len();
    s.event("t", false, 2);
    assert_eq!(s.fire("a", &Run::ok(&[], 0), 2), Err(Reject::Precondition));
    assert_eq!(s.ledger.holdings().len(), rows_before, "a rejected firing mints nothing");
    assert_eq!(s.next_seq("a"), 2);
    assert_eq!(s.attachments["a"].consecutive_failures, 0);
    assert!(s.invariants().is_ok());
}

/// [EMIT-TX] 用户裁定：撤回只及本次触发自己投递的未消费事件；已消费的留下；别的附着与别的触发不受波及。
#[test]
fn withdrawal_is_bounded_to_this_firings_unconsumed_events() {
    let mut s = Scheduler::new(true);
    s.attach(manual("a", &[], 5), 0).unwrap();
    s.attach(manual("b", &[], 5), 0).unwrap();
    s.fire("a", &Run::ok(&[], 1), 1).unwrap(); // a#1 → e1
    s.fire("b", &Run::ok(&[], 1), 1).unwrap(); // b#1 → e2
    s.begin("a", 2).unwrap(); // a#2 in flight
    let e3 = s.emit("a").unwrap();
    let e4 = s.emit("a").unwrap();
    assert!(s.consume(e3), "a fast reader consumed one event before the run failed");
    s.finish("a", &Run::fail(&[], 0), 3).unwrap();
    let ids: Vec<u64> = s.inbox.iter().map(|e| e.id).collect();
    assert!(ids.contains(&1) && ids.contains(&2), "other firings and other attachments untouched");
    assert!(ids.contains(&e3), "consumed events cannot be withdrawn — a person cannot be un-notified");
    assert!(!ids.contains(&e4), "this firing's unconsumed event is withdrawn");
    assert!(s.audit.iter().any(|a| matches!(a, Audit::Withdrawn { id, seq: 2, events: 1 } if id == "a")));
    assert!(s.audit.iter().any(|a| matches!(a, Audit::Settled { id, seq: 2, end: End::FailStop } if id == "a")));
    assert!(s.invariants().is_ok());
}

/// §8.3 组合性证据："泵坏了"总会到达：正常路径发出的摘要在失败段的事务之外，后来的失败撤不掉它。
#[test]
fn pump_failed_always_arrives_normal_path_emits_survive_later_failures() {
    let mut s = Scheduler::new(true);
    s.attach(manual("ci", &[(NAV, 1)], 5), 0).unwrap();
    s.fire("ci", &Run::ok(&[], 1), 1).unwrap(); // r.status ≠ 0 是正常路径：摘要照常投递
    s.fire("ci", &Run::fail(&[], 1), 2).unwrap(); // 一次真正的触发失败
    let seqs: Vec<u64> = s.inbox.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1], "the summary from the completed run stays; the failed run's event is withdrawn");
}

/// [A4] 有界队列：每次丢弃都入审计（DropOldest／DropNewest／FailStop→Paused）；Timer 停机后只追赶一次、记 missed。
#[test]
fn queue_overflow_never_drops_silently_and_timer_catches_up_once_with_missed() {
    let mut s = Scheduler::new(true);
    s.attach(decl("old", Trigger::Topic { topic: "t".into() }, &[], 10), 0).unwrap();
    let mut newest = decl("new", Trigger::Topic { topic: "t".into() }, &[], 10);
    newest.overflow = Overflow::DropNewest;
    s.attach(newest, 0).unwrap();
    let mut stop = decl("stop", Trigger::Topic { topic: "t".into() }, &[], 10);
    stop.overflow = Overflow::FailStop;
    s.attach(stop, 0).unwrap();
    for t in 1..=5 {
        s.event("t", true, t);
    }
    assert_eq!(s.queue_len("old"), 2);
    assert_eq!(s.queue_len("new"), 2);
    let dropped = |s: &Scheduler, who: &str| {
        s.audit
            .iter()
            .filter(|a| matches!(a, Audit::Dropped { id, dropped: 1, .. } if id == who))
            .count()
    };
    assert_eq!(dropped(&s, "old"), 3);
    assert_eq!(dropped(&s, "new"), 3);
    assert_eq!(s.status("stop"), Status::PausedFailure);
    assert!(dropped(&s, "stop") >= 1);

    let mut t = Scheduler::new(true);
    t.attach(decl("timer", Trigger::Timer { period: 10 }, &[], 100), 0).unwrap();
    t.tick(10);
    assert_eq!(t.queue_len("timer"), 1);
    t.fire("timer", &Run::ok(&[], 0), 10).unwrap();
    t.crash();
    t.recover(55);
    assert_eq!(t.queue_len("timer"), 1, "catch up once");
    assert!(t.audit.iter().any(|a| matches!(a, Audit::Coalesced { id, missed: 3 } if id == "timer")));
    assert!(t.invariants().is_ok());
}

/// [A4] 失败预算：连续 k 次 fail-stop ⇒ Paused；成功复位；powerbox 恢复。
#[test]
fn failure_budget_pauses_after_k_consecutive_failstops_and_resets_on_success() {
    let mut s = Scheduler::new(true);
    s.attach(manual("a", &[], 100), 0).unwrap();
    let fail = Run::fail(&[], 0);
    let ok = Run::ok(&[], 0);
    s.fire("a", &fail, 1).unwrap();
    s.fire("a", &fail, 2).unwrap();
    s.fire("a", &ok, 3).unwrap();
    s.fire("a", &fail, 4).unwrap();
    s.fire("a", &fail, 5).unwrap();
    assert_eq!(s.status("a"), Status::Active, "a success reset the count");
    s.fire("a", &fail, 6).unwrap();
    assert_eq!(s.status("a"), Status::PausedFailure);
    assert_eq!(s.fire("a", &ok, 7), Err(Reject::NotActive(Status::PausedFailure)));
    assert!(s.resume("a"));
    assert_eq!(s.fire("a", &ok, 8), Ok(7));
    assert!(s.invariants().is_ok());
}

/// [A6] detach／到期／撤销终止于同一账本形状：名下无存活行、池容量为 0；已投递事件留在 user/inbox。
#[test]
fn detach_expiry_and_revocation_end_in_the_same_ledger_shape() {
    let mut s = Scheduler::new(true);
    for id in ["d", "e", "r"] {
        let mut x = manual(id, &[(NAV, 1)], 3);
        x.ttl = 100;
        s.attach(x, 0).unwrap();
        s.fire(id, &Run::ok(&[(NAV, 1)], 1), 1).unwrap();
    }
    s.detach("d", 2);
    s.revoke(GRANT, 3); // e 与 r 都依赖同一授予 ⇒ 都被级联拆除
    assert_eq!(s.status("d"), Status::Detached);
    assert_eq!(s.status("e"), Status::Detached);
    assert_eq!(s.status("r"), Status::Detached);
    let mut t = Scheduler::new(true);
    let mut x = manual("x", &[(NAV, 1)], 3);
    x.ttl = 100;
    t.attach(x, 0).unwrap();
    t.fire("x", &Run::ok(&[(NAV, 1)], 1), 1).unwrap();
    t.tick(100);
    assert_eq!(t.status("x"), Status::Expired);
    for (sch, id) in [(&s, "d"), (&s, "e"), (&s, "r"), (&t, "x")] {
        assert!(!sch.ledger.live().any(|h| h.subject.starts_with(&format!("attach/{id}"))), "{id}: no live rows");
        assert!(sch.invariants().is_ok(), "{id}: {:?}", sch.invariants());
        assert_eq!(sch.inbox.iter().filter(|e| e.attach == id).count(), 1, "{id}: delivered event stays");
    }
    assert!(s.audit.iter().any(|a| matches!(a, Audit::Detached { id, why } if id == "e" && why == "revoked:grant_5b")));
}

/// [LEASE]+裁定三：触发段租约为 None、随根走；ttl_i ≤ 根租约剩余 ⇒ sweeper 不因在途触发等待，
/// 在途触发按 Expired 结账、事件撤回。
#[test]
fn firing_segments_carry_no_lease_so_expiry_never_waits_on_a_run_in_flight() {
    let mut s = Scheduler::new(true);
    let mut d = manual("a", &[(NAV, 1)], 5);
    d.ttl = 10;
    d.run_cap = 10;
    s.attach(d, 0).unwrap();
    let seq = s.begin("a", 9).unwrap();
    assert_eq!(s.quad("a").unwrap().ttl_expires_at, 10, "ttl_i = min(now + run_cap, lease end)");
    let seg = s.ledger.live().find(|h| h.subject == "attach/a:seg#1").unwrap();
    assert!(seg.lease_expires_at.is_none(), "segment rows carry no lease of their own");
    s.emit("a").unwrap();
    s.tick(10);
    assert_eq!(s.status("a"), Status::Expired, "the sweep did not wait on the run in flight");
    assert_eq!(s.attachments["a"].firings[0].end, Some(End::Expired));
    assert_eq!(seq, 1);
    assert!(s.unconsumed_events("a").is_empty(), "the aborted run's event was withdrawn");
    assert!(s.invariants().is_ok(), "{:?}", s.invariants());
}

/// [Q7] 策略表变更：重跑准入——通过 ⇒ Paused 待重签（新 h_attach）；不通过 ⇒ Detached。
#[test]
fn table_change_reruns_admission_paused_until_resign_or_detached() {
    let mut s = Scheduler::new(true);
    s.attach(manual("good", &[], 5), 0).unwrap();
    s.attach(manual("bad", &[], 5), 0).unwrap();
    let before = s.attachments["good"].h_attach.clone();
    s.table_change("blake3:new", &|d: &Declaration| d.id != "bad", 1);
    assert_eq!(s.status("good"), Status::PausedTableChanged);
    assert_eq!(s.status("bad"), Status::Detached);
    assert_eq!(s.fire("good", &Run::ok(&[], 0), 2), Err(Reject::NotActive(Status::PausedTableChanged)));
    assert!(s.resign("good", "blake3:new"));
    assert_ne!(s.attachments["good"].h_attach, before, "h_table is in the signed bytes");
    assert_eq!(s.fire("good", &Run::ok(&[], 0), 3), Ok(1));
    assert!(s.invariants().is_ok());
}

/// [A4] 每附着串行（在途时再触发 ⇒ Serial）；min_interval 单调约束；不同附着可并发。
#[test]
fn per_attachment_serial_and_min_interval_hold() {
    let mut s = Scheduler::new(true);
    let mut a = manual("a", &[], 5);
    a.min_interval = 5;
    s.attach(a, 0).unwrap();
    s.attach(manual("b", &[], 5), 0).unwrap();
    s.begin("a", 1).unwrap();
    assert_eq!(s.begin("a", 1), Err(Reject::Serial));
    assert_eq!(s.fire("b", &Run::ok(&[], 0), 1), Ok(1), "another attachment runs concurrently");
    s.finish("a", &Run::ok(&[], 0), 2).unwrap();
    assert_eq!(s.fire("a", &Run::ok(&[], 0), 3), Err(Reject::MinInterval));
    assert_eq!(s.fire("a", &Run::ok(&[], 0), 6), Ok(2));
    assert!(s.invariants().is_ok());
}
