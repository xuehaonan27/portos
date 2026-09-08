//! F6 法则测试（代数库扩充）—— RDMA 走查把 endstate §8.2 库清单里的两个点顶成承重：
//! 不相交区间（MR 子区间／memory window）与分数持有（多 QP 共享读）。
//! 方法：F1 的 RA 法则检查器原样复用，载体确定性穷举；≼ 的定义（∃c. b = a·c）逐对核对。

use portos_rm::identity::{ClassId, Generation, HoldingHandle, InstanceId, ResourceKey, SubjectId};
use portos_rm::ledger::GrantRequest;
use portos_rm::ledger::*;
use portos_rm::ra::*;
use portos_rm::registry::{Capacity, Claim};
use portos_rm::time::{LeaseRequest, Timestamp};

/// 区间载体：[0,4) 上全部单元格并集（16 个合法元，相邻自动合并）＋ ⊥。
/// 单元格互不相交，故任意两元合成要么合法（不交）要么 ⊥（有公共格）——覆盖两条分支。
fn ranges_carrier() -> Vec<Ranges> {
    let mut v: Vec<Ranges> = (0u32..16)
        .map(|mask| {
            Ranges::of(
                &(0..4)
                    .filter(|i| mask & (1 << i) != 0)
                    .map(|i| (i as u64, i as u64 + 1))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    v.push(Ranges::bot());
    v.sort_by_key(|r| format!("{r:?}"));
    v.dedup();
    v
}

/// 分数载体：k/12（k=1..=12，合法）＋ 13/12（非法：>1）。
fn frac_carrier() -> Vec<Frac> {
    (1..=13u64).map(|k| Frac::new(k, 12)).collect()
}

fn ra_laws_all_triples<A: Ra>(elems: &[A]) -> usize {
    let mut n = 0;
    for a in elems {
        assert!(law_core_id(a), "core-id 失败：{a:?}");
        assert!(law_core_idem(a), "core-idem 失败：{a:?}");
        for b in elems {
            assert!(law_comm(a, b), "交换律失败：{a:?} {b:?}");
            assert!(law_valid_op_l(a, b), "valid-op-l 失败：{a:?} {b:?}");
            assert!(law_core_mono(a, b), "core-mono 失败：{a:?} {b:?}");
            for c in elems {
                assert!(law_assoc(a, b, c), "结合律失败：{a:?} {b:?} {c:?}");
                n += 1;
            }
        }
    }
    n
}

/// [RA] 区间与分数满足 F1 采纳的全部 RA 公理（Iris TR appendix-4.5）；≼ 的定义逐对核对：
/// 区间：a ≼ b ⟺ ∃c. b = a·c（载体含全部补集，故可穷举 c）；
/// 分数：基元上为严格包含，Option<Frac> 上自然获得自反性。
#[test]
fn ranges_and_frac_satisfy_ra_laws_and_inclusion_definition() {
    let rs = ranges_carrier();
    assert_eq!(rs.len(), 17);
    assert_eq!(ra_laws_all_triples(&rs), 17 * 17 * 17);
    for a in &rs {
        for b in &rs {
            if !a.valid() || !b.valid() {
                continue;
            }
            let exists_c = rs.iter().any(|c| c.valid() && a.op(c) == *b);
            assert_eq!(
                a.included_in(b),
                exists_c,
                "区间 ≼ 与 ∃c 定义不符：{a:?} ≼ {b:?}"
            );
        }
        // ⊥ 约定：一切 ≼ ⊥；⊥ 只 ≼ ⊥。
        assert!(a.included_in(&Ranges::bot()));
        assert_eq!(Ranges::bot().included_in(a), !a.valid());
    }
    // 相邻合并的规范化：[0,1)·[1,2) 与 [0,2) 是同一元（否则结合律比较会漏）。
    assert_eq!(
        Ranges::of(&[(0, 1)]).op(&Ranges::of(&[(1, 2)])),
        Ranges::of(&[(0, 2)])
    );
    // 重叠即 ⊥：[0,2)·[1,3)。
    assert!(!Ranges::of(&[(0, 2)]).op(&Ranges::of(&[(1, 3)])).valid());

    let fs = frac_carrier();
    assert_eq!(ra_laws_all_triples(&fs), 13 * 13 * 13);
    for a in &fs {
        for b in &fs {
            if !a.valid() || !b.valid() {
                continue;
            }
            let exists_c = fs.iter().any(|c| c.valid() && a.op(c) == *b);
            assert_eq!(
                a.included_in(b),
                exists_c,
                "分数包含关系与补差不符：{a:?} {b:?}"
            );
            assert_eq!(
                Some(a.clone()).included_in(&Some(b.clone())),
                a == b || exists_c
            );
        }
    }
    assert!(
        !Frac::new(1, 2).op(&Frac::new(2, 3)).valid(),
        "1/2 + 2/3 > 1 非法"
    );
    assert!(
        Frac::new(1, 2).op(&Frac::new(1, 2)).valid(),
        "1/2 + 1/2 = 1 合法（恰好独占）"
    );
}

/// [ISSUER] 账本对新代数的发放方闸门与 F1 一致：不相交子区间可并授，重叠被拒（Conflict）；
/// 分数份额合成不得超 1。全程 `invariant()` 成立。
#[test]
fn ledger_gates_subranges_and_fraction_shares() {
    let mut l = Ledger::new();
    l.register_class(ClassDecl {
        cleanup: portos_rm::cleanup::CleanupPolicy::AccountingOnly,
        class_id: "mr".into(),
        algebra: AlgebraTag::Range,
        release_idempotent: true,
        lease_duration: None,
        revert_grade: RevertGrade::Inverse,
    })
    .unwrap();
    l.register_class(ClassDecl {
        cleanup: portos_rm::cleanup::CleanupPolicy::AccountingOnly,
        class_id: "mr-read".into(),
        algebra: AlgebraTag::Frac,
        release_idempotent: true,
        lease_duration: None,
        revert_grade: RevertGrade::Inverse,
    })
    .unwrap();
    l.create_pool(
        &l.registered_class::<Ranges>(&ClassId::new("mr")).unwrap(),
        InstanceId::new("buf"),
        Capacity::new(Ranges::of(&[(0, 8192)])).unwrap(),
    )
    .unwrap();
    l.create_pool(
        &l.registered_class::<Frac>(&ClassId::new("mr-read"))
            .unwrap(),
        InstanceId::new("buf"),
        Capacity::new(Frac::one()).unwrap(),
    )
    .unwrap();

    // 子区间：不相交并授；重叠拒；越出容量拒。
    let w1 = l
        .grant(
            &l.pool::<Ranges>(&ResourceKey::new(
                ClassId::new("mr"),
                InstanceId::new("buf"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("qp-a"),
                claim: Claim::new(Ranges::of(&[(0, 4096)])).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    l.grant(
        &l.pool::<Ranges>(&ResourceKey::new(
            ClassId::new("mr"),
            InstanceId::new("buf"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("qp-b"),
            claim: Claim::new(Ranges::of(&[(4096, 8192)])).unwrap(),
            generation: Generation::new("g"),
            parent: None,
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    assert_eq!(
        l.grant(
            &l.pool::<Ranges>(&ResourceKey::new(
                ClassId::new("mr"),
                InstanceId::new("buf")
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("qp-c"),
                claim: Claim::new(Ranges::of(&[(2048, 6144)])).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap()
            }
        )
        .map(|h| h.id()),
        Err(LedgerError::Conflict),
        "重叠窗口被拒"
    );
    l.release(
        &HoldingHandle::new(w1, Generation::new("g")),
        Timestamp::try_from(1u64).unwrap(),
    )
    .unwrap();
    assert_eq!(
        l.grant(
            &l.pool::<Ranges>(&ResourceKey::new(
                ClassId::new("mr"),
                InstanceId::new("buf")
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("qp-c"),
                claim: Claim::new(Ranges::of(&[(8192, 9000)])).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap()
            }
        )
        .map(|h| h.id()),
        Err(LedgerError::Conflict),
        "越出注册区被拒"
    );
    l.grant(
        &l.pool::<Ranges>(&ResourceKey::new(
            ClassId::new("mr"),
            InstanceId::new("buf"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("qp-c"),
            claim: Claim::new(Ranges::of(&[(0, 1024)])).unwrap(),
            generation: Generation::new("g"),
            parent: None,
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(2u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap(); // 释放后可再授
    l.invariant().unwrap();

    // 分数份额：三个 1/3 合成恰为 1；第四个被拒。
    for q in ["r1", "r2", "r3"] {
        l.grant(
            &l.pool::<Frac>(&ResourceKey::new(
                ClassId::new("mr-read"),
                InstanceId::new("buf"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new(q),
                claim: Claim::new(Frac::new(1, 3)).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    }
    assert_eq!(
        l.grant(
            &l.pool::<Frac>(&ResourceKey::new(
                ClassId::new("mr-read"),
                InstanceId::new("buf")
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("r4"),
                claim: Claim::new(Frac::new(1, 3)).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap()
            }
        )
        .map(|h| h.id()),
        Err(LedgerError::Conflict),
        "份额超 1 被拒"
    );
    l.invariant().unwrap();
    // 代数错位是 schema 级类型错，不是冲突。
    assert!(matches!(
        l.pool::<Frac>(&ResourceKey::new(
            ClassId::new("mr"),
            InstanceId::new("buf")
        )),
        Err(LedgerError::AlgebraMismatch)
    ));
}
