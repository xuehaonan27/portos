//! 法则表的可执行形态 —— 每个测试名注明它执行的法则与其冻结来源。
//! 来源缩写：[RA]=Iris RA 公理；[FPU]=frame-preserving update；[AUTH]=Auth 合法性；
//! [C1]=行式生命周期记账；[C2]=中央容量检查与 FPU 的区别；
//! [GEN]=世代化句柄；[CRASH]=crash-only 单路径；[T]=租约/对账。

use portos_rm::auth::{auth_valid, can_mint, compose};
use portos_rm::ledger::*;
use portos_rm::ra::*;

fn sample_counts(rng: &mut Lcg, n: usize) -> Vec<Count> {
    (0..n).map(|_| Count::Value(rng.next() % 7)).collect()
}
fn sample_sets(rng: &mut Lcg, n: usize) -> Vec<GSet> {
    let names = ["a", "b", "c", "d"];
    (0..n)
        .map(|_| {
            let k = (rng.next() % 4) as usize;
            GSet(names.iter().take(k).map(|s| s.to_string()).collect())
        })
        .collect()
}

/// [RA] assoc / comm / valid-op-l / core 三律，对三个内置代数抽样执行。
#[test]
fn ra_laws_all_algebras() {
    let exs = [Ex::Token, Ex::Bot];
    for a in exs {
        for b in exs {
            for c in exs {
                assert!(law_assoc(&a, &b, &c));
                assert!(law_comm(&a, &b));
                assert!(law_valid_op_l(&a, &b));
                assert!(law_core_id(&a) && law_core_idem(&a) && law_core_mono(&a, &b));
            }
        }
    }
    let mut rng = Lcg(42);
    let cs = sample_counts(&mut rng, 24);
    for a in &cs {
        for b in &cs {
            for c in &cs {
                assert!(law_assoc(a, b, c));
                assert!(law_comm(a, b));
                assert!(law_valid_op_l(a, b));
            }
            assert!(law_core_id(a) && law_core_idem(a) && law_core_mono(a, b));
        }
    }
    let ss = sample_sets(&mut rng, 12);
    for a in &ss {
        for b in &ss {
            for c in &ss {
                assert!(law_assoc(a, b, c));
                assert!(law_comm(a, b));
                assert!(law_valid_op_l(a, b));
            }
            assert!(law_core_id(a) && law_core_idem(a) && law_core_mono(a, b));
        }
    }
}

/// [AUTH] ✓(●a·◯b) ⟺ b ≼ a ∧ ✓a —— 构造正反例。
#[test]
fn auth_validity_iff() {
    assert!(auth_valid(&Count::Value(5), &Some(Count::Value(5))));
    assert!(auth_valid(&Count::Value(5), &Some(Count::Value(3))));
    assert!(!auth_valid(&Count::Value(5), &Some(Count::Value(6))));
    assert!(auth_valid(&Count::Value(5), &None));
    assert!(auth_valid(&Ex::Token, &Some(Ex::Token)));
    assert!(!auth_valid(&Ex::Token, &Some(Ex::Bot)));
    assert!(!auth_valid(&Ex::Bot, &None)); // ✓a 失败
}

fn drill_ledger() -> Ledger {
    let mut l = Ledger::new();
    l.register_class(ClassDecl {
        class_id: "tcp-port".into(),
        algebra: AlgebraTag::Exclusive,
        release_idempotent: true,
        lease_secs: Some(30),
        revert_grade: RevertGrade::Inverse,
    });
    l.register_class(ClassDecl {
        class_id: "quota".into(),
        algebra: AlgebraTag::Counted,
        release_idempotent: true,
        lease_secs: None,
        revert_grade: RevertGrade::Inverse,
    });
    l.register_class(ClassDecl {
        class_id: "proc-tree".into(),
        algebra: AlgebraTag::Exclusive,
        release_idempotent: true,
        lease_secs: Some(60),
        revert_grade: RevertGrade::Inverse,
    });
    l.set_capacity("tcp-port", "8080", Frag::Ex(Ex::Token));
    l.set_capacity("quota", "pool", Frag::Count(Count::Value(5)));
    for i in 0..8 {
        l.set_capacity("proc-tree", &format!("slot{i}"), Frag::Ex(Ex::Token));
    }
    l
}

/// [C2]+[AUTH] 独占双授予被拒（⊕ 无定义的运行时对应之一：冲突）。
#[test]
fn exclusive_double_grant_refused() {
    let mut l = drill_ledger();
    let a = l.grant("drv-a", "tcp-port", "8080", Frag::Ex(Ex::Token), "g1", None, 0);
    assert!(a.is_ok());
    let b = l.grant("drv-b", "tcp-port", "8080", Frag::Ex(Ex::Token), "g2", None, 0);
    assert_eq!(b.unwrap_err(), LedgerError::Conflict);
    l.invariant().unwrap();
}

/// [AUTH] 计数不可透支（与能力表 counting cap 同一法则）。
#[test]
fn counting_no_overdraft() {
    let mut l = drill_ledger();
    l.grant("a", "quota", "pool", Frag::Count(Count::Value(3)), "g", None, 0).unwrap();
    l.grant("b", "quota", "pool", Frag::Count(Count::Value(2)), "g", None, 0).unwrap();
    let over = l.grant("c", "quota", "pool", Frag::Count(Count::Value(1)), "g", None, 0);
    assert_eq!(over.unwrap_err(), LedgerError::Conflict);
    l.invariant().unwrap();
}

/// [GEN] 名字会被基底回收 ⇒ 旧世代的 release 必须被拒（ABA 防护）。
#[test]
fn stale_generation_rejected() {
    let mut l = drill_ledger();
    let id = l
        .grant("drv", "proc-tree", "slot0", Frag::Ex(Ex::Token), "pid100@t1", None, 0)
        .unwrap();
    assert_eq!(
        l.release(id, "pid100@t2", 1).unwrap_err(),
        LedgerError::StaleGeneration
    );
    l.release(id, "pid100@t1", 1).unwrap();
}

/// [T]+[E] release 幂等（盲重放安全）；租约到期 sweep ≡ 显式 release。
#[test]
fn release_idempotent_and_sweep_equiv() {
    let mut l = drill_ledger();
    let id = l
        .grant("drv", "tcp-port", "8080", Frag::Ex(Ex::Token), "g", None, 0)
        .unwrap();
    l.release(id, "g", 5).unwrap();
    l.release(id, "g", 6).unwrap(); // 幂等：第二次 OK 而非错误
    // sweep 路径等价：新账本走租约到期
    let mut l2 = drill_ledger();
    l2.grant("drv", "tcp-port", "8080", Frag::Ex(Ex::Token), "g", None, 0)
        .unwrap();
    let swept = l2.sweep(31); // lease 30s
    assert_eq!(swept.len(), 1);
    assert_eq!(l.live_count(), l2.live_count());
    l2.invariant().unwrap();
}

/// [AUTH]+[C1] 随机操作序列下全局不变量恒成立（碎片折叠 ≼ 容量）。
#[test]
fn invariant_under_random_ops() {
    let mut l = drill_ledger();
    let mut rng = Lcg(7);
    let mut lives: Vec<(u64, String)> = Vec::new();
    for step in 0..300u64 {
        match rng.next() % 3 {
            0 => {
                let want = Frag::Count(Count::Value(rng.next() % 3));
                if let Ok(id) = l.grant("s", "quota", "pool", want, "g", None, step) {
                    lives.push((id, "g".into()));
                }
            }
            1 => {
                let slot = format!("slot{}", rng.next() % 8);
                let generation = format!("generation{}", rng.next() % 2);
                if let Ok(id) = l.grant("s", "proc-tree", &slot, Frag::Ex(Ex::Token), &generation, None, step) {
                    lives.push((id, generation));
                }
            }
            _ => {
                if !lives.is_empty() {
                    let i = (rng.next() as usize) % lives.len();
                    let (id, generation) = lives.swap_remove(i);
                    let _ = l.release(id, &generation, step);
                }
            }
        }
        l.invariant().unwrap();
    }
}

/// [CRASH] ownership 树逆拓扑：子先于父；teardown 后主体名下零 live。
#[test]
fn teardown_children_before_parents() {
    let mut l = drill_ledger();
    let encl = l
        .grant("drv", "proc-tree", "slot0", Frag::Ex(Ex::Token), "e", None, 0)
        .unwrap();
    let chromium = l
        .grant("drv", "proc-tree", "slot1", Frag::Ex(Ex::Token), "c", Some(encl), 0)
        .unwrap();
    let page = l
        .grant("drv", "proc-tree", "slot2", Frag::Ex(Ex::Token), "p", Some(chromium), 0)
        .unwrap();
    let port = l
        .grant("drv", "tcp-port", "8080", Frag::Ex(Ex::Token), "g", Some(encl), 0)
        .unwrap();
    // 违反次序的直接 release 被拒
    assert_eq!(l.release(encl, "e", 1).unwrap_err(), LedgerError::TeardownOrder);
    let plan = l.teardown("drv", 2);
    let pos = |id: u64| plan.iter().position(|x| *x == id).unwrap();
    assert!(pos(page) < pos(chromium) && pos(chromium) < pos(encl));
    assert!(pos(port) < pos(encl));
    assert_eq!(l.live_count(), 0);
    l.invariant().unwrap();
}

/// [CRASH] 单路径：优雅 teardown 与 kill 后租约 sweep 收敛到同一终态。
#[test]
fn crash_only_single_path() {
    let build = |l: &mut Ledger| {
        let e = l
            .grant("drv", "proc-tree", "slot0", Frag::Ex(Ex::Token), "e", None, 0)
            .unwrap();
        l.grant("drv", "proc-tree", "slot1", Frag::Ex(Ex::Token), "c", Some(e), 0)
            .unwrap();
        l.grant("drv", "tcp-port", "8080", Frag::Ex(Ex::Token), "g", Some(e), 0)
            .unwrap();
    };
    let mut graceful = drill_ledger();
    build(&mut graceful);
    graceful.teardown("drv", 10);
    let mut crashed = drill_ledger();
    build(&mut crashed);
    crashed.sweep(100); // 全部租约过期（proc 60s / port 30s）
    assert_eq!(graceful.live_count(), 0);
    assert_eq!(crashed.live_count(), 0);
    assert_eq!(graceful.tombstone_count(), crashed.tombstone_count());
    graceful.invariant().unwrap();
    crashed.invariant().unwrap();
}

/// A concrete Auth(Count) fixture: frames may carry fragments or an authority.
/// Two authorities conflict; fragment-only ownership does not carry a capacity.
#[derive(Clone, PartialEq, Debug)]
struct AuthC {
    cap: Option<Count>,
    conflicting_authorities: bool,
    frag: Count,
}
impl AuthC {
    fn full(cap: u64, frag: Count) -> Self {
        Self { cap: Some(Count::Value(cap)), conflicting_authorities: false, frag }
    }
    fn fragment(frag: u64) -> Self {
        Self { cap: None, conflicting_authorities: false, frag: Count::Value(frag) }
    }
}
impl Ra for AuthC {
    fn op(&self, o: &Self) -> Self {
        let conflicting_authorities = self.conflicting_authorities
            || o.conflicting_authorities || (self.cap.is_some() && o.cap.is_some());
        AuthC {
            cap: if conflicting_authorities { None } else { self.cap.or(o.cap) },
            conflicting_authorities,
            frag: self.frag.op(&o.frag),
        }
    }
    fn valid(&self) -> bool {
        !self.conflicting_authorities && match self.cap {
            None => self.frag.valid(),
            Some(cap) => auth_valid(&cap, &Some(self.frag)),
        }
    }
    fn pcore(&self) -> Option<Self> {
        Some(AuthC {
            cap: None,
            conflicting_authorities: self.conflicting_authorities,
            frag: Count::Value(0),
        })
    }
    fn included_in(&self, b: &Self) -> bool {
        let authority_included = b.conflicting_authorities || (!self.conflicting_authorities
            && match (self.cap, b.cap) {
                (None, _) => true,
                (Some(a), Some(b)) => a == b,
                (Some(_), None) => false,
            });
        authority_included && self.frag.included_in(&b.frag)
    }
}

#[test]
fn auth_fixture_has_exclusive_authority_and_satisfies_ra_laws() {
    let mut elems = Vec::new();
    for n in 0..=3 {
        elems.push(AuthC::fragment(n));
        for cap in 0..=2 {
            elems.push(AuthC::full(cap, Count::Value(n)));
        }
        elems.push(AuthC::full(0, Count::Value(n)).op(&AuthC::full(1, Count::Value(0))));
    }
    for a in &elems {
        assert!(law_core_id(a) && law_core_idem(a));
        for b in &elems {
            assert!(law_comm(a, b) && law_valid_op_l(a, b) && law_core_mono(a, b));
            for c in &elems {
                assert!(law_assoc(a, b, c));
            }
        }
    }
    assert!(!AuthC::full(1, Count::Value(0)).op(&AuthC::full(1, Count::Value(0))).valid());
}

/// [FPU] release（丢一笔碎片）是 frame-preserving update：a ⤳ b ⟺ ∀f. ✓(a·f) ⇒ ✓(b·f)。
/// [B13] 初版只在 Count 碎片上验——Count 碎片恒合法，FPU 空洞成立（统一复核抓到）。
/// 现在在 Auth 复合元素上验：容量 1..=6、全部"碎片和 ≤ 容量"的 live 组合、丢任一笔、
/// 帧取 0..=cap+1 全部——并要求样本里既有 ✓(a·f) 成立也有失败的帧（检查确实咬合）。
#[test]
fn release_is_frame_preserving() {
    let (mut bites_valid, mut bites_invalid, mut checked) = (false, false, 0usize);
    for cap in 1..=6u64 {
        for x in 0..=cap {
            for y in 0..=(cap - x) {
                for z in 0..=(cap - x - y) {
                    let live = [Count::Value(x), Count::Value(y), Count::Value(z)];
                    for drop_idx in 0..3 {
                        let kept: Vec<Count> = live.iter().enumerate().filter(|(i, _)| *i != drop_idx).map(|(_, c)| *c).collect();
                        let a = AuthC::full(cap, compose(&live).unwrap());
                        let b = AuthC::full(cap, compose(&kept).unwrap());
                        let frames: Vec<AuthC> = (0..=cap + 1).map(AuthC::fragment).collect();
                        assert!(fpu_holds(&a, &b, &frames), "release 破坏了 FPU：cap={cap} live={live:?} drop={drop_idx}");
                        bites_valid |= frames.iter().any(|f| a.op(f).valid());
                        bites_invalid |= frames.iter().any(|f| !a.op(f).valid());
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(checked > 100 && bites_valid && bites_invalid, "帧样本未同时覆盖合法与非法合成——检查空洞");
}

/// [T]+[B14] 租约为 None 的持有"仅随 parent 生命期"：父项租约到期时 sweep 必须把它们一并
/// 回收（子先于父），否则父项被 TeardownOrder 永远挡住、非自愿路径失效。
/// 有自身未到期租约的子项不受父项牵连——父项保守等待，等子项到期再一起走。
#[test]
fn sweep_cascades_to_parent_bound_children() {
    // 场景一：proc(租约 60) ⊃ quota(租约 None)：sweep(100) 两者皆回收，子先于父。
    let mut l = drill_ledger();
    let proc_ = l.grant("drv", "proc-tree", "slot0", Frag::Ex(Ex::Token), "p", None, 0).unwrap();
    let quota = l.grant("drv", "quota", "pool", Frag::Count(Count::Value(2)), "g", Some(proc_), 0).unwrap();
    let swept = l.sweep(100);
    assert_eq!(swept, vec![quota, proc_], "租约 None 的子项随父项一起回收，且子先于父");
    assert_eq!(l.live_count(), 0);
    l.invariant().unwrap();

    // 场景二：proc(租约 60) ⊃ port(租约 30)：sweep(45) 只收子；sweep(70) 再收父。
    let mut l = drill_ledger();
    let proc_ = l.grant("drv", "proc-tree", "slot0", Frag::Ex(Ex::Token), "p", None, 0).unwrap();
    let port = l.grant("drv", "tcp-port", "8080", Frag::Ex(Ex::Token), "g", Some(proc_), 0).unwrap();
    assert_eq!(l.sweep(45), vec![port]);
    assert_eq!(l.sweep(70), vec![proc_]);

    // 场景三：父先到期、子有自己的长租约：父项保守等待（不越序强拆），子到期后一起走。
    let mut l = drill_ledger();
    let proc_ = l.grant("drv", "proc-tree", "slot0", Frag::Ex(Ex::Token), "p", None, 0).unwrap();
    let port = l.grant("drv", "tcp-port", "8080", Frag::Ex(Ex::Token), "g", Some(proc_), 40).unwrap(); // 到期 70
    assert_eq!(l.sweep(65), Vec::<u64>::new(), "父到期但子未到期：父保守存活");
    assert_eq!(l.live_count(), 2);
    assert_eq!(l.sweep(71), vec![port, proc_]);
    l.invariant().unwrap();
}

/// [AUTH-EDGE] 决策 2（用户裁定 2026-09-05）：ownership 边可跨主体，但只沿**实例化关系**——
/// 父持有须是自己的，或属于实例化了自己的主体（内核在拉起子实例时登记）；
/// 第三方不得把持有挂到别人的树下；关系不传递（祖父的树要经父的持有进入）。
#[test]
fn cross_subject_parent_requires_instantiation_authority() {
    let mut l = drill_ledger();
    let encl = l.grant("host", "proc-tree", "slot0", Frag::Ex(Ex::Token), "e", None, 0).unwrap();
    // 未登记实例化关系：child 不得挂到 host 的持有下。
    assert_eq!(
        l.grant("child", "proc-tree", "slot1", Frag::Ex(Ex::Token), "c", Some(encl), 0).unwrap_err(),
        LedgerError::ParentAuthority
    );
    l.declare_instantiation("child", "host");
    let cproc = l.grant("child", "proc-tree", "slot1", Frag::Ex(Ex::Token), "c", Some(encl), 0).unwrap();
    // 自己树内照常。
    l.grant("child", "tcp-port", "8080", Frag::Ex(Ex::Token), "g", Some(cproc), 0).unwrap();
    // 第三方既非持有者也非被其实例化：拒。
    assert_eq!(
        l.grant("stranger", "proc-tree", "slot2", Frag::Ex(Ex::Token), "s", Some(cproc), 0).unwrap_err(),
        LedgerError::ParentAuthority
    );
    // 不传递：grandchild 由 child 实例化，不能越过 child 直接挂到 host 的 enclosure 下。
    l.declare_instantiation("grandchild", "child");
    assert_eq!(
        l.grant("grandchild", "proc-tree", "slot3", Frag::Ex(Ex::Token), "gc", Some(encl), 0).unwrap_err(),
        LedgerError::ParentAuthority
    );
    l.grant("grandchild", "proc-tree", "slot3", Frag::Ex(Ex::Token), "gc", Some(cproc), 0).unwrap();
    l.invariant().unwrap();
    // 树的形状：host ⊃ child ⊃ {port, grandchild}——teardown(host) 的闭包看得见全部四笔。
    assert_eq!(l.live_closure("host").len(), 4);
}

/// [TRANSFER] 转授保持每个 (class, instance) 的合成值与容量不变量；
/// 世代不符拒（ABA）、来源主体不符拒（句柄不是你的）、已释放拒。
#[test]
fn transfer_preserves_aggregate_and_holder_checks() {
    let mut l = drill_ledger();
    let a = l.grant("fib:seg", "quota", "pool", Frag::Count(Count::Value(3)), "g", None, 0).unwrap();
    let b = l.grant("other", "quota", "pool", Frag::Count(Count::Value(2)), "g", None, 0).unwrap();
    let outstanding_before: u64 = l.live().map(|h| match h.frag { Frag::Count(Count::Value(n)) => n, _ => 0 }).sum();
    assert_eq!(l.transfer(a, "wrong", "fib:seg", "fib").unwrap_err(), LedgerError::StaleGeneration);
    assert_eq!(l.transfer(a, "g", "someone-else", "fib").unwrap_err(), LedgerError::ForgedHandle);
    l.transfer(a, "g", "fib:seg", "fib").unwrap();
    assert_eq!(l.live_snapshot("fib:seg").len(), 0);
    assert_eq!(l.live_snapshot("fib").iter().map(|it| it.id).collect::<Vec<_>>(), vec![a]);
    let outstanding_after: u64 = l.live().map(|h| match h.frag { Frag::Count(Count::Value(n)) => n, _ => 0 }).sum();
    assert_eq!(outstanding_before, outstanding_after, "合成值不变——转授不是 mint 也不是 release");
    l.invariant().unwrap();
    // 转授后池仍然按同一闸门守：再要 1 就超（3+2+1 > 5）。
    assert_eq!(l.grant("x", "quota", "pool", Frag::Count(Count::Value(1)), "g", None, 1).unwrap_err(), LedgerError::Conflict);
    l.release(b, "g", 2).unwrap();
    assert_eq!(l.transfer(b, "g", "other", "fib").unwrap_err(), LedgerError::ForgedHandle, "已释放不可转授");
}

/// A central capacity check can pass while the proposed Auth update is not FPU.
#[test]
fn capacity_check_does_not_imply_fpu() {
    let a = AuthC::full(10, Count::Value(3));
    let b = AuthC::full(10, Count::Value(4));
    let breaking_frame = AuthC::fragment(7);
    assert!(can_mint(&Count::Value(10), &[Count::Value(3)], &Count::Value(1)));
    assert!(a.op(&breaking_frame).valid());
    assert!(!b.op(&breaking_frame).valid());
    assert!(!fpu_holds(&a, &b, &[breaking_frame]));
    // Complete-ledger accounting separately enforces the pool capacity.
    assert!(can_mint(&Count::Value(5), &[Count::Value(1), Count::Value(2)], &Count::Value(2)));
    assert!(!can_mint(&Count::Value(5), &[Count::Value(1), Count::Value(2)], &Count::Value(3)));
}

/// [T] 基底对账：检出衰变（基底已亡）与账外（基底有而账本无）。
#[test]
fn reconcile_detects_decay_and_untracked() {
    let mut l = drill_ledger();
    l.grant("drv", "proc-tree", "slot0", Frag::Ex(Ex::Token), "pid1@t1", None, 0)
        .unwrap();
    l.grant("drv", "proc-tree", "slot1", Frag::Ex(Ex::Token), "pid2@t1", None, 0)
        .unwrap();
    let substrate = vec![
        ("slot0".to_string(), "pid1@t1".to_string()), // 健在
        ("slot9".to_string(), "pid9@t1".to_string()), // 账外
    ];
    let (decayed, untracked) = l.reconcile("proc-tree", &substrate);
    assert_eq!(decayed.len(), 1); // slot1 已亡于基底
    assert_eq!(untracked, vec![("slot9".to_string(), "pid9@t1".to_string())]);
}
