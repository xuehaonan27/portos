//! 法则表的可执行形态 —— 每个测试名注明它执行的法则与其冻结来源。
//! 来源缩写：[RA]=Iris RA 公理；[FPU]=frame-preserving update；[AUTH]=Auth 合法性；
//! [C1]=行式生命周期记账；[C2]=中央容量检查与 FPU 的区别；
//! [GEN]=世代化句柄；[CRASH]=crash-only 单路径；[T]=租约/对账。

use portos_rm::auth::{auth_valid, can_mint, compose};
use portos_rm::identity::{
    ClassId, Generation, HoldingHandle, HoldingId, InstanceId, ResourceKey, SubjectId,
};
use portos_rm::ledger::GrantRequest;
use portos_rm::ledger::*;
use portos_rm::ra::*;
use portos_rm::registry::{Capacity, Claim};
use portos_rm::time::{LeaseDuration, LeaseRequest, Timestamp};

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
        lease_duration: Some(30).map(|s: u64| LeaseDuration::try_from(s).unwrap()),
        revert_grade: RevertGrade::Inverse,
    })
    .unwrap();
    l.register_class(ClassDecl {
        class_id: "quota".into(),
        algebra: AlgebraTag::Counted,
        release_idempotent: true,
        lease_duration: None,
        revert_grade: RevertGrade::Inverse,
    })
    .unwrap();
    l.register_class(ClassDecl {
        class_id: "proc-tree".into(),
        algebra: AlgebraTag::Exclusive,
        release_idempotent: true,
        lease_duration: Some(60).map(|s: u64| LeaseDuration::try_from(s).unwrap()),
        revert_grade: RevertGrade::Inverse,
    })
    .unwrap();
    l.create_pool(
        &l.registered_class::<Ex>(&ClassId::new("tcp-port")).unwrap(),
        InstanceId::new("8080"),
        Capacity::new(Ex::Token).unwrap(),
    )
    .unwrap();
    l.create_pool(
        &l.registered_class::<Count>(&ClassId::new("quota")).unwrap(),
        InstanceId::new("pool"),
        Capacity::new(Count::Value(5)).unwrap(),
    )
    .unwrap();
    for i in 0..8 {
        l.create_pool(
            &l.registered_class::<Ex>(&ClassId::new("proc-tree"))
                .unwrap(),
            InstanceId::new(&format!("slot{i}")),
            Capacity::new(Ex::Token).unwrap(),
        )
        .unwrap();
    }
    l
}

/// [C2]+[AUTH] 独占双授予被拒（⊕ 无定义的运行时对应之一：冲突）。
#[test]
fn exclusive_double_grant_refused() {
    let mut l = drill_ledger();
    let a = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("tcp-port"),
                InstanceId::new("8080"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv-a"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("g1"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id());
    assert!(a.is_ok());
    let b = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("tcp-port"),
                InstanceId::new("8080"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv-b"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("g2"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id());
    assert_eq!(b.unwrap_err(), LedgerError::Conflict);
    l.invariant().unwrap();
}

/// [AUTH] 计数不可透支（与能力表 counting cap 同一法则）。
#[test]
fn counting_no_overdraft() {
    let mut l = drill_ledger();
    l.grant(
        &l.pool::<Count>(&ResourceKey::new(
            ClassId::new("quota"),
            InstanceId::new("pool"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("a"),
            claim: Claim::new(Count::Value(3)).unwrap(),
            generation: Generation::new("g"),
            parent: None,
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    l.grant(
        &l.pool::<Count>(&ResourceKey::new(
            ClassId::new("quota"),
            InstanceId::new("pool"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("b"),
            claim: Claim::new(Count::Value(2)).unwrap(),
            generation: Generation::new("g"),
            parent: None,
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    let over = l
        .grant(
            &l.pool::<Count>(&ResourceKey::new(
                ClassId::new("quota"),
                InstanceId::new("pool"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("c"),
                claim: Claim::new(Count::Value(1)).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id());
    assert_eq!(over.unwrap_err(), LedgerError::Conflict);
    l.invariant().unwrap();
}

/// [GEN] 名字会被基底回收 ⇒ 旧世代的 release 必须被拒（ABA 防护）。
#[test]
fn stale_generation_rejected() {
    let mut l = drill_ledger();
    let id = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot0"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("pid100@t1"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    assert_eq!(
        l.release(
            &HoldingHandle::new(id, Generation::new("pid100@t2")),
            Timestamp::try_from(1u64).unwrap()
        )
        .unwrap_err(),
        LedgerError::StaleGeneration
    );
    l.release(
        &HoldingHandle::new(id, Generation::new("pid100@t1")),
        Timestamp::try_from(1u64).unwrap(),
    )
    .unwrap();
}

/// [T]+[E] release 幂等（盲重放安全）；租约到期 sweep ≡ 显式 release。
#[test]
fn release_idempotent_and_sweep_equiv() {
    let mut l = drill_ledger();
    let id = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("tcp-port"),
                InstanceId::new("8080"),
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
    l.release(
        &HoldingHandle::new(id, Generation::new("g")),
        Timestamp::try_from(5u64).unwrap(),
    )
    .unwrap();
    l.release(
        &HoldingHandle::new(id, Generation::new("g")),
        Timestamp::try_from(6u64).unwrap(),
    )
    .unwrap(); // 幂等：第二次 OK 而非错误
    // sweep 路径等价：新账本走租约到期
    let mut l2 = drill_ledger();
    l2.grant(
        &l2.pool::<Ex>(&ResourceKey::new(
            ClassId::new("tcp-port"),
            InstanceId::new("8080"),
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
    let swept = l2.sweep(Timestamp::try_from(31u64).unwrap()); // lease 30s
    assert_eq!(swept.len(), 1);
    assert_eq!(l.live_count(), l2.live_count());
    l2.invariant().unwrap();
}

/// [AUTH]+[C1] 随机操作序列下全局不变量恒成立（碎片折叠 ≼ 容量）。
#[test]
fn invariant_under_random_ops() {
    let mut l = drill_ledger();
    let mut rng = Lcg(7);
    let mut lives: Vec<(HoldingId, String)> = Vec::new();
    for step in 0..300u64 {
        match rng.next() % 3 {
            0 => {
                let want = Count::Value(rng.next() % 3);
                if let Ok(id) = l
                    .grant(
                        &l.pool::<Count>(&ResourceKey::new(
                            ClassId::new("quota"),
                            InstanceId::new("pool"),
                        ))
                        .unwrap(),
                        GrantRequest {
                            owner: SubjectId::new("s"),
                            claim: Claim::new(want).unwrap(),
                            generation: Generation::new("g"),
                            parent: None,
                            lease: LeaseRequest::UseClassDefault,
                            now: Timestamp::try_from(step).unwrap(),
                        },
                    )
                    .map(|h| h.id())
                {
                    lives.push((id, "g".into()));
                }
            }
            1 => {
                let slot = format!("slot{}", rng.next() % 8);
                let generation = format!("generation{}", rng.next() % 2);
                if let Ok(id) = l
                    .grant(
                        &l.pool::<Ex>(&ResourceKey::new(
                            ClassId::new("proc-tree"),
                            InstanceId::new(&slot),
                        ))
                        .unwrap(),
                        GrantRequest {
                            owner: SubjectId::new("s"),
                            claim: Claim::new(Ex::Token).unwrap(),
                            generation: Generation::new(&generation),
                            parent: None,
                            lease: LeaseRequest::UseClassDefault,
                            now: Timestamp::try_from(step).unwrap(),
                        },
                    )
                    .map(|h| h.id())
                {
                    lives.push((id, generation));
                }
            }
            _ => {
                if !lives.is_empty() {
                    let i = (rng.next() as usize) % lives.len();
                    let (id, generation) = lives.swap_remove(i);
                    let _ = l.release(
                        &HoldingHandle::new(id, Generation::new(&generation)),
                        Timestamp::try_from(step).unwrap(),
                    );
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
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot0"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("e"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let chromium = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot1"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("c"),
                parent: Some(encl).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let page = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot2"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("p"),
                parent: Some(chromium).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let port = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("tcp-port"),
                InstanceId::new("8080"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("g"),
                parent: Some(encl).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    // 违反次序的直接 release 被拒
    assert_eq!(
        l.release(
            &HoldingHandle::new(encl, Generation::new("e")),
            Timestamp::try_from(1u64).unwrap()
        )
        .unwrap_err(),
        LedgerError::TeardownOrder
    );
    let plan = l.teardown(&SubjectId::new("drv"), Timestamp::try_from(2u64).unwrap());
    let pos = |id: HoldingId| plan.iter().position(|x| *x == id).unwrap();
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
            .grant(
                &l.pool::<Ex>(&ResourceKey::new(
                    ClassId::new("proc-tree"),
                    InstanceId::new("slot0"),
                ))
                .unwrap(),
                GrantRequest {
                    owner: SubjectId::new("drv"),
                    claim: Claim::new(Ex::Token).unwrap(),
                    generation: Generation::new("e"),
                    parent: None,
                    lease: LeaseRequest::UseClassDefault,
                    now: Timestamp::try_from(0u64).unwrap(),
                },
            )
            .map(|h| h.id())
            .unwrap();
        l.grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot1"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("c"),
                parent: Some(e).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
        l.grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("tcp-port"),
                InstanceId::new("8080"),
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
    };
    let mut graceful = drill_ledger();
    build(&mut graceful);
    graceful.teardown(&SubjectId::new("drv"), Timestamp::try_from(10u64).unwrap());
    let mut crashed = drill_ledger();
    build(&mut crashed);
    crashed.sweep(Timestamp::try_from(100u64).unwrap()); // 全部租约过期（proc 60s / port 30s）
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
        Self {
            cap: Some(Count::Value(cap)),
            conflicting_authorities: false,
            frag,
        }
    }
    fn fragment(frag: u64) -> Self {
        Self {
            cap: None,
            conflicting_authorities: false,
            frag: Count::Value(frag),
        }
    }
}
impl Ra for AuthC {
    fn op(&self, o: &Self) -> Self {
        let conflicting_authorities = self.conflicting_authorities
            || o.conflicting_authorities
            || (self.cap.is_some() && o.cap.is_some());
        AuthC {
            cap: if conflicting_authorities {
                None
            } else {
                self.cap.or(o.cap)
            },
            conflicting_authorities,
            frag: self.frag.op(&o.frag),
        }
    }
    fn valid(&self) -> bool {
        !self.conflicting_authorities
            && match self.cap {
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
        let authority_included = b.conflicting_authorities
            || (!self.conflicting_authorities
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
    assert!(
        !AuthC::full(1, Count::Value(0))
            .op(&AuthC::full(1, Count::Value(0)))
            .valid()
    );
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
                        let kept: Vec<Count> = live
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| *i != drop_idx)
                            .map(|(_, c)| *c)
                            .collect();
                        let a = AuthC::full(cap, compose(&live).unwrap());
                        let b = AuthC::full(cap, compose(&kept).unwrap());
                        let frames: Vec<AuthC> = (0..=cap + 1).map(AuthC::fragment).collect();
                        assert!(
                            fpu_holds(&a, &b, &frames),
                            "release 破坏了 FPU：cap={cap} live={live:?} drop={drop_idx}"
                        );
                        bites_valid |= frames.iter().any(|f| a.op(f).valid());
                        bites_invalid |= frames.iter().any(|f| !a.op(f).valid());
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(
        checked > 100 && bites_valid && bites_invalid,
        "帧样本未同时覆盖合法与非法合成——检查空洞"
    );
}

/// [T]+[B14] 租约为 None 的持有"仅随 parent 生命期"：父项租约到期时 sweep 必须把它们一并
/// 回收（子先于父），否则父项被 TeardownOrder 永远挡住、非自愿路径失效。
/// 有自身未到期租约的子项不受父项牵连——父项保守等待，等子项到期再一起走。
#[test]
fn sweep_cascades_to_parent_bound_children() {
    // 场景一：proc(租约 60) ⊃ quota(租约 None)：sweep(100) 两者皆回收，子先于父。
    let mut l = drill_ledger();
    let proc_ = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot0"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("p"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let quota = l
        .grant(
            &l.pool::<Count>(&ResourceKey::new(
                ClassId::new("quota"),
                InstanceId::new("pool"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Count::Value(2)).unwrap(),
                generation: Generation::new("g"),
                parent: Some(proc_).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let swept = l.sweep(Timestamp::try_from(100u64).unwrap());
    assert_eq!(
        swept,
        vec![quota, proc_],
        "租约 None 的子项随父项一起回收，且子先于父"
    );
    assert_eq!(l.live_count(), 0);
    l.invariant().unwrap();

    // 场景二：proc(租约 60) ⊃ port(租约 30)：sweep(45) 只收子；sweep(70) 再收父。
    let mut l = drill_ledger();
    let proc_ = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot0"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("p"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let port = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("tcp-port"),
                InstanceId::new("8080"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("g"),
                parent: Some(proc_).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    assert_eq!(l.sweep(Timestamp::try_from(45u64).unwrap()), vec![port]);
    assert_eq!(l.sweep(Timestamp::try_from(70u64).unwrap()), vec![proc_]);

    // 场景三：父先到期、子有自己的长租约：父项保守等待（不越序强拆），子到期后一起走。
    let mut l = drill_ledger();
    let proc_ = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot0"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("p"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let port = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("tcp-port"),
                InstanceId::new("8080"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("drv"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("g"),
                parent: Some(proc_).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(40u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap(); // 到期 70
    assert_eq!(
        l.sweep(Timestamp::try_from(65u64).unwrap()),
        Vec::<HoldingId>::new(),
        "父到期但子未到期：父保守存活"
    );
    assert_eq!(l.live_count(), 2);
    assert_eq!(
        l.sweep(Timestamp::try_from(71u64).unwrap()),
        vec![port, proc_]
    );
    l.invariant().unwrap();
}

/// [AUTH-EDGE] 决策 2（用户裁定 2026-09-05）：ownership 边可跨主体，但只沿**实例化关系**——
/// 父持有须是自己的，或属于实例化了自己的主体（内核在拉起子实例时登记）；
/// 第三方不得把持有挂到别人的树下；关系不传递（祖父的树要经父的持有进入）。
#[test]
fn cross_subject_parent_requires_instantiation_authority() {
    let mut l = drill_ledger();
    let encl = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot0"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("host"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("e"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    // 未登记实例化关系：child 不得挂到 host 的持有下。
    assert_eq!(
        l.grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot1")
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("child"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("c"),
                parent: Some(encl).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap()
            }
        )
        .map(|h| h.id())
        .unwrap_err(),
        LedgerError::ParentAuthority
    );
    l.declare_instantiation(SubjectId::new("child"), SubjectId::new("host"));
    let cproc = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot1"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("child"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("c"),
                parent: Some(encl).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    // 自己树内照常。
    l.grant(
        &l.pool::<Ex>(&ResourceKey::new(
            ClassId::new("tcp-port"),
            InstanceId::new("8080"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("child"),
            claim: Claim::new(Ex::Token).unwrap(),
            generation: Generation::new("g"),
            parent: Some(cproc).map(|id| l.holding(id).expect("parent exists").handle()),
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    // 第三方既非持有者也非被其实例化：拒。
    assert_eq!(
        l.grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot2")
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("stranger"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("s"),
                parent: Some(cproc).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap()
            }
        )
        .map(|h| h.id())
        .unwrap_err(),
        LedgerError::ParentAuthority
    );
    // 不传递：grandchild 由 child 实例化，不能越过 child 直接挂到 host 的 enclosure 下。
    l.declare_instantiation(SubjectId::new("grandchild"), SubjectId::new("child"));
    assert_eq!(
        l.grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("proc-tree"),
                InstanceId::new("slot3")
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("grandchild"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("gc"),
                parent: Some(encl).map(|id| l.holding(id).expect("parent exists").handle()),
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap()
            }
        )
        .map(|h| h.id())
        .unwrap_err(),
        LedgerError::ParentAuthority
    );
    l.grant(
        &l.pool::<Ex>(&ResourceKey::new(
            ClassId::new("proc-tree"),
            InstanceId::new("slot3"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("grandchild"),
            claim: Claim::new(Ex::Token).unwrap(),
            generation: Generation::new("gc"),
            parent: Some(cproc).map(|id| l.holding(id).expect("parent exists").handle()),
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    l.invariant().unwrap();
    // 树的形状：host ⊃ child ⊃ {port, grandchild}——teardown(host) 的闭包看得见全部四笔。
    assert_eq!(l.live_closure(&SubjectId::new("host")).len(), 4);
}

/// [TRANSFER] 转授保持每个 (class, instance) 的合成值与容量不变量；
/// 世代不符拒（ABA）、来源主体不符拒（句柄不是你的）、已释放拒。
#[test]
fn transfer_preserves_aggregate_and_holder_checks() {
    let mut l = drill_ledger();
    let a = l
        .grant(
            &l.pool::<Count>(&ResourceKey::new(
                ClassId::new("quota"),
                InstanceId::new("pool"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("fib:seg"),
                claim: Claim::new(Count::Value(3)).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let b = l
        .grant(
            &l.pool::<Count>(&ResourceKey::new(
                ClassId::new("quota"),
                InstanceId::new("pool"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("other"),
                claim: Claim::new(Count::Value(2)).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    let outstanding_before: u64 = l
        .live()
        .map(|h| match h.frag {
            Frag::Count(Count::Value(n)) => n,
            _ => 0,
        })
        .sum();
    assert_eq!(
        l.transfer(
            &HoldingHandle::new(a, Generation::new("wrong")),
            &SubjectId::new("fib:seg"),
            SubjectId::new("fib")
        )
        .unwrap_err(),
        LedgerError::StaleGeneration
    );
    assert_eq!(
        l.transfer(
            &HoldingHandle::new(a, Generation::new("g")),
            &SubjectId::new("someone-else"),
            SubjectId::new("fib")
        )
        .unwrap_err(),
        LedgerError::ForgedHandle
    );
    l.transfer(
        &HoldingHandle::new(a, Generation::new("g")),
        &SubjectId::new("fib:seg"),
        SubjectId::new("fib"),
    )
    .unwrap();
    assert_eq!(l.live_snapshot(&SubjectId::new("fib:seg")).len(), 0);
    assert_eq!(
        l.live_snapshot(&SubjectId::new("fib"))
            .iter()
            .map(|it| it.id)
            .collect::<Vec<_>>(),
        vec![a]
    );
    let outstanding_after: u64 = l
        .live()
        .map(|h| match h.frag {
            Frag::Count(Count::Value(n)) => n,
            _ => 0,
        })
        .sum();
    assert_eq!(
        outstanding_before, outstanding_after,
        "合成值不变——转授不是 mint 也不是 release"
    );
    l.invariant().unwrap();
    // 转授后池仍然按同一闸门守：再要 1 就超（3+2+1 > 5）。
    assert_eq!(
        l.grant(
            &l.pool::<Count>(&ResourceKey::new(
                ClassId::new("quota"),
                InstanceId::new("pool")
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("x"),
                claim: Claim::new(Count::Value(1)).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(1u64).unwrap()
            }
        )
        .map(|h| h.id())
        .unwrap_err(),
        LedgerError::Conflict
    );
    l.release(
        &HoldingHandle::new(b, Generation::new("g")),
        Timestamp::try_from(2u64).unwrap(),
    )
    .unwrap();
    assert_eq!(
        l.transfer(
            &HoldingHandle::new(b, Generation::new("g")),
            &SubjectId::new("other"),
            SubjectId::new("fib")
        )
        .unwrap_err(),
        LedgerError::ForgedHandle,
        "已释放不可转授"
    );
}

/// A central capacity check can pass while the proposed Auth update is not FPU.
#[test]
fn capacity_check_does_not_imply_fpu() {
    let a = AuthC::full(10, Count::Value(3));
    let b = AuthC::full(10, Count::Value(4));
    let breaking_frame = AuthC::fragment(7);
    assert!(can_mint(
        &Count::Value(10),
        &[Count::Value(3)],
        &Count::Value(1)
    ));
    assert!(a.op(&breaking_frame).valid());
    assert!(!b.op(&breaking_frame).valid());
    assert!(!fpu_holds(&a, &b, &[breaking_frame]));
    // Complete-ledger accounting separately enforces the pool capacity.
    assert!(can_mint(
        &Count::Value(5),
        &[Count::Value(1), Count::Value(2)],
        &Count::Value(2)
    ));
    assert!(!can_mint(
        &Count::Value(5),
        &[Count::Value(1), Count::Value(2)],
        &Count::Value(3)
    ));
}

/// [T] 基底对账：检出衰变（基底已亡）与账外（基底有而账本无）。
#[test]
fn reconcile_detects_decay_and_untracked() {
    let mut l = drill_ledger();
    l.grant(
        &l.pool::<Ex>(&ResourceKey::new(
            ClassId::new("proc-tree"),
            InstanceId::new("slot0"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("drv"),
            claim: Claim::new(Ex::Token).unwrap(),
            generation: Generation::new("pid1@t1"),
            parent: None,
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    l.grant(
        &l.pool::<Ex>(&ResourceKey::new(
            ClassId::new("proc-tree"),
            InstanceId::new("slot1"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("drv"),
            claim: Claim::new(Ex::Token).unwrap(),
            generation: Generation::new("pid2@t1"),
            parent: None,
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    let substrate = vec![
        (InstanceId::new("slot0"), Generation::new("pid1@t1")), // 健在
        (InstanceId::new("slot9"), Generation::new("pid9@t1")), // 账外
    ];
    let (decayed, untracked) = l.reconcile(&ClassId::new("proc-tree"), &substrate);
    assert_eq!(decayed.len(), 1); // slot1 已亡于基底
    assert_eq!(
        untracked,
        vec![(InstanceId::new("slot9"), Generation::new("pid9@t1"))]
    );
}
