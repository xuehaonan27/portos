//! F4 法则测试（v2）—— 每个测试名 = 它执行的定理/纪律（对照 freeze-f4 三列表）。
//! 措辞纪律：投影一致性以"真的驱动 F2/F3 机器跑通"来证，不止比字面。
//! v2 新增三块墓碑（B5/B6/B7，见 freeze-f4 §2）与一致性格点的确定性穷举（决策 #8）。

use portos_rm::identity::VerbId;
use portos_rm::test_support::declarations::Declarations;
use portos_rm::identity::{ClassId, Generation, InstanceId, ResourceKey, SubjectId};
use portos_rm::ledger::GrantRequest;
use portos_rm::ledger::*;
use portos_rm::test_support::monitor::*;
use portos_rm::ra::Ex;
use portos_rm::registry::{Capacity, Claim};
use portos_rm::test_support::teardown::{Orchestrator, RunOutcome};
use portos_rm::time::{LeaseDuration, LeaseRequest, Timestamp};
use portos_rm::verbs::*;

fn held() -> VerbEntry {
    VerbEntry::consuming(ConsumeGrade::Held)
}
fn ext(amortizable: bool) -> VerbEntry {
    VerbEntry::emitting(EmitGrade::External, amortizable)
}
fn comp(with: &str, amortizable: bool) -> VerbEntry {
    VerbEntry::emitting(
        EmitGrade::Compensable {
            compensate_with: with.into(),
        },
        amortizable,
    )
}

/// 浏览器＋socket 场景真理表（D1 忠实版）：一个类同时有持有、可重复读、发射——
/// v1 表达不了的组合（page 类下的 click/submit、socket 类下的 send）现在是常态。
fn browser_verb_table() -> VerbTable {
    let mut t = Declarations::new();
    // [ρ] 类的持有档——与 F2 tests/f2_teardown.rs::browser_ledger 同源（F2 投影源）。
    for c in [
        "enclosure",
        "proc",
        "page",
        "tcp-port",
        "workspace",
        "socket",
    ] {
        t.declare_class(c, RevertGrade::Inverse).unwrap();
    }
    t.declare_class("reservation", RevertGrade::Compensable)
        .unwrap();

    // page：持有（attach）＋可重复读（snapshot）＋可摊销外部发射（click/type）
    //       ＋硬清单发射（submit，逐次同意）＋其降档（submit_draft，可补偿）。
    t.register("page", "attach", held()).unwrap();
    t.register("page", "snapshot", VerbEntry::repeatable())
        .unwrap();
    t.register("page", "click", ext(true)).unwrap();
    t.register("page", "type", ext(true)).unwrap();
    t.register("page", "submit", ext(false).degrades_to("submit_draft"))
        .unwrap();
    t.register("page", "submit_draft", comp("discard_draft", true))
        .unwrap();
    t.register("page", "discard_draft", ext(true)).unwrap();
    // 其余持有类各一个获取动词（Consuming/Held）＋零星动作。
    t.register("proc", "spawn", held()).unwrap();
    t.register("proc", "signal", ext(true)).unwrap();
    t.register("tcp-port", "bind", held()).unwrap();
    t.register("tcp-port", "probe", VerbEntry::repeatable())
        .unwrap();
    t.register("workspace", "mkdir", held()).unwrap();
    t.register("enclosure", "open", held()).unwrap();
    t.register("reservation", "reserve", held()).unwrap();
    // socket 之教训：同一类下 connect 持有、recv 消耗（外部）、send 发射（外部、可摊销）。
    t.register("socket", "connect", held()).unwrap();
    t.register(
        "socket",
        "recv",
        VerbEntry::consuming(ConsumeGrade::External),
    )
    .unwrap();
    t.register("socket", "send", ext(true)).unwrap();
    // mail：无持有；send 是硬清单（机密跨域出境）⇒ 不可摊销。
    t.register("mail", "send", ext(false)).unwrap();
    // bus：界内消息，同名 send 可补偿（retract）——D1 的对照组。
    t.register("bus", "send", comp("retract", true)).unwrap();
    t.register("bus", "retract", ext(true)).unwrap();
    // doc：可重复读＋降档声明（F3 attenuate 的表侧来源）。
    t.register(
        "doc",
        "read_full",
        VerbEntry::repeatable().degrades_to("read_preview"),
    )
    .unwrap();
    t.register("doc", "read_preview", VerbEntry::repeatable())
        .unwrap();
    // api：可补偿发射＋其补偿动词。
    t.register("api", "post", comp("post_cancel", true))
        .unwrap();
    t.register("api", "post_cancel", ext(true)).unwrap();
    t.check_all().unwrap()
}

/// [TRI]（决策 #8：确定性穷举）一致性谓词的完整真理表：
/// 9 种性格（可重复 / 界内变换（F6） / 消耗×3 世界档 / 发射×2 世界档×2 摊销）× 幂等 × 交换 ＝ 36 格，
/// 逐格对照**显式写出的规则**（非调用被测谓词）：
///   · 可重复 ⟹ 幂等（§8.3 盲重放）——4 格收 2：交换性按动词声明（B11：不可变源读 commutes=true，
///     共享可变源读 commutes=false；v2 的"可重复⟹交换"把 §8.1b 对不可变源的断言推广到全部
///     可重复读，强于来源，且让 §8.1b 第二等通道无处安放）；
///   · 界内变换：不加旗标约束（chmod 幂等、append 不幂等）——4 格都收；类级约束在 register；
///   · 消耗：允许声明同一请求的幂等重试——每种世界档 4 格都收；
///   · 发射：不加旗标约束（带幂等键的 PUT 是幂等发射）——每格都收。
/// 曾经的运行时校验"发射∧有逆"、"发射∧Held"、"可重复带世界档"在 v2 由类型排除，
/// 已不在格点内（不可表示 ＞ 被拒）。
#[test]
fn coherence_lattice_exhaustive_over_kind_and_flags() {
    let kinds: Vec<(&str, Kind)> = vec![
        ("Repeatable", Kind::Repeatable),
        ("Transforming", Kind::Transforming),
        (
            "Consuming/Held",
            Kind::Consuming {
                world: ConsumeGrade::Held,
            },
        ),
        (
            "Consuming/Comp",
            Kind::Consuming {
                world: ConsumeGrade::Compensable {
                    compensate_with: "c".into(),
                },
            },
        ),
        (
            "Consuming/Ext",
            Kind::Consuming {
                world: ConsumeGrade::External,
            },
        ),
        (
            "Emitting/Comp/amort",
            Kind::Emitting {
                world: EmitGrade::Compensable {
                    compensate_with: "c".into(),
                },
                amortizable: true,
            },
        ),
        (
            "Emitting/Comp/hard",
            Kind::Emitting {
                world: EmitGrade::Compensable {
                    compensate_with: "c".into(),
                },
                amortizable: false,
            },
        ),
        (
            "Emitting/Ext/amort",
            Kind::Emitting {
                world: EmitGrade::External,
                amortizable: true,
            },
        ),
        (
            "Emitting/Ext/hard",
            Kind::Emitting {
                world: EmitGrade::External,
                amortizable: false,
            },
        ),
    ];
    let expected_ok = |kind: &Kind, idem: bool, _comm: bool| -> bool {
        match kind {
            Kind::Repeatable => idem,
            Kind::Transforming => true,
            Kind::Consuming { .. } => true,
            Kind::Emitting { .. } => true,
        }
    };
    let (mut cells, mut accepted) = (0, 0);
    for (name, kind) in &kinds {
        for idem in [false, true] {
            for comm in [false, true] {
                let e = VerbEntry {
                    kind: kind.clone(),
                    idempotent: idem,
                    commutes: comm,
                    degrade: None,
                };
                let got = e.check_coherent().is_ok();
                assert_eq!(
                    got,
                    expected_ok(kind, idem, comm),
                    "格 {name} idem={idem} comm={comm} 判定与规则表不符"
                );
                cells += 1;
                accepted += got as u32;
            }
        }
    }
    assert_eq!(cells, 36, "格点被收缩");
    assert_eq!(
        accepted,
        2 + 4 + 3 * 4 + 4 * 4,
        "接受集大小＝2（可重复）＋4（界内变换）＋12（消耗）＋16（发射）"
    );

    // B11 的存在证据：endstate §8.1b 四等通道里的第二等（共享可变源读）现在可表达——
    // 不进预算、盲重放安全，但不可交换；第一等（不可变源读）仍是 commutes=true。
    let shared = VerbEntry::repeatable_shared();
    assert!(shared.check_coherent().is_ok());
    assert!(!shared.bears_budget() && shared.declares_idempotence() && !shared.commutes);
    assert!(VerbEntry::repeatable().commutes);
    assert!(
        VerbEntry::repeatable()
            .with_flags(false, true)
            .check_coherent()
            .is_err(),
        "可重复仍须幂等"
    );
}

/// [D1] 位置判据：同名动词、不同 handler、不同性格；投影按类，裸动词集投影不存在。
/// read：CAS 手柄可重复 / 队列手柄消耗；send：mail 逐次扣发 / bus 可补偿不扣发 / socket 可摊销不扣发。
#[test]
fn d1_same_verb_two_handlers_projections_differ() {
    let mut t = browser_verb_table();
    let mut extra = Declarations::new();
    extra.register("cas", "read", VerbEntry::repeatable()).unwrap();
    extra.register(
        "queue",
        "read",
        VerbEntry::consuming(ConsumeGrade::External),
    )
    .unwrap();
    for class in ["cas", "queue"] { t.insert(extra.check_all().unwrap().class(&ClassId::new(class)).unwrap().clone()).unwrap(); }
    assert!(!t.lookup(&ClassId::new("cas"), &VerbId::new("read")).unwrap().bears_budget());
    assert!(
        t.lookup(&ClassId::new("queue"), &VerbId::new("read")).unwrap().bears_budget(),
        "预算地位随手柄翻转"
    );
    assert_eq!(
        t.lookup(&ClassId::new("cas"), &VerbId::new("write")),
        Err(VerbError::Unknown),
        "未注册 (类,动词) 是硬错"
    );

    // B6 墓碑：同名 send 的扣发地位随 handler 而异——按类投影才保得住 D1。
    assert!(t.derive_handler_policy(&ClassId::new("mail")).unwrap().withhold.contains("send"));
    assert!(!t.derive_handler_policy(&ClassId::new("bus")).unwrap().withhold.contains("send"));
    assert!(!t.derive_handler_policy(&ClassId::new("socket")).unwrap().withhold.contains("send"));
    for h in ["mail", "bus", "socket"] {
        assert!(
            t.derive_handler_policy(&ClassId::new(h)).unwrap().budget.contains("send"),
            "{h}.send 都进预算"
        );
    }
    assert_eq!(
        t.derive_handler_policy(&ClassId::new("bus")).unwrap()
            .compensations
            .get("send")
            .map(String::as_str),
        Some("retract")
    );
}

/// [BUDGET] 消耗性读进预算——精化 D9 读/写二分（endstate §8.3 硬规则 → §9-3）。
/// socket recv 与 page snapshot 同为"读"，前者计费后者不计——二分不足，三分类才对。
#[test]
fn consuming_read_bears_budget_refines_read_write_binary() {
    let t = browser_verb_table();
    let recv = t.lookup(&ClassId::new("socket"), &VerbId::new("recv")).unwrap();
    let snap = t.lookup(&ClassId::new("page"), &VerbId::new("snapshot")).unwrap();
    assert!(
        !matches!(recv.kind(), Kind::Emitting { .. }) && !matches!(snap.kind(), Kind::Emitting { .. })
    );
    assert!(recv.bears_budget(), "队列 pop 是真实状态变更，必须进预算");
    assert!(!snap.bears_budget(), "不可变源读不占预算，只记标签");
    assert!(
        t.class(&ClassId::new("page")).unwrap().laws().iter().any(|l| matches!(&l.equation, Equation::Idempotent { operation } if operation.as_str() == "snapshot"))
            && !t.class(&ClassId::new("socket")).unwrap().laws().iter().any(|l| matches!(&l.equation, Equation::Idempotent { operation } if operation.as_str() == "recv")),
        "可重复读盲重放安全，消耗读不安全"
    );
}

/// [ρ]+[WG] B5 墓碑：持有的 ρ 是类的属性（F2），动作的世界档是动词的属性（F3）——两栏正交。
/// v1 把二者混成一栏并强制同类一致，page 类因而注册不了 click/submit、socket 注册不了 send。
#[test]
fn holding_rho_is_per_class_action_grade_is_per_verb() {
    let t = browser_verb_table();
    // 同一个 page 类：持有可精确关闭（Inverse）……
    assert_eq!(t.derive_holding_grade(&ClassId::new("page")), Some(RevertGrade::Inverse));
    // ……而它的发射动词各有自己的世界档，与持有档无关。
    assert!(
        t.lookup(&ClassId::new("page"), &VerbId::new("click")).unwrap().staged_shape(),
        "click 是外部发射"
    );
    assert!(matches!(
        t.lookup(&ClassId::new("page"), &VerbId::new("submit_draft")).unwrap().kind().clone(),
        Kind::Emitting {
            world: EmitGrade::Compensable { .. },
            ..
        }
    ));
    assert!(matches!(
        t.lookup(&ClassId::new("page"), &VerbId::new("attach")).unwrap().kind().clone(),
        Kind::Consuming {
            world: ConsumeGrade::Held
        }
    ));
    // socket：连接是持有、recv 外部消耗、send 外部发射——一类三性格。
    assert_eq!(t.derive_holding_grade(&ClassId::new("socket")), Some(RevertGrade::Inverse));
    assert!(t.lookup(&ClassId::new("socket"), &VerbId::new("send")).unwrap().staged_shape());
    assert!(matches!(
        t.lookup(&ClassId::new("socket"), &VerbId::new("recv")).unwrap().kind().clone(),
        Kind::Consuming {
            world: ConsumeGrade::External
        }
    ));
    // 无持有的 handler 没有 ρ（None 不是缺省档，是"此类不产生持有"）。
    assert_eq!(t.derive_holding_grade(&ClassId::new("mail")), None);
}

/// [STAGE] B7 墓碑：两阶段形状 ⟺ 外部发射（endstate §8.5 字面）；逐次扣发 ⟺ 发射∧不可摊销
/// （effect-plan §5.5 硬清单）。可摊销的外部发射（click）两阶段塌缩进准入，不扣发——
/// 否则"一次同意换一批自治动作"不成立。
#[test]
fn withhold_iff_non_amortizable_staged_shape_iff_external() {
    let t = browser_verb_table();
    let click = t.lookup(&ClassId::new("page"), &VerbId::new("click")).unwrap();
    assert!(
        click.staged_shape() && !click.withhold(),
        "click：两阶段由准入满足，不逐次扣发"
    );
    let submit = t.lookup(&ClassId::new("page"), &VerbId::new("submit")).unwrap();
    assert!(
        submit.staged_shape() && submit.withhold(),
        "submit：硬清单，逐次同意"
    );
    let draft = t.lookup(&ClassId::new("page"), &VerbId::new("submit_draft")).unwrap();
    assert!(
        !draft.staged_shape() && !draft.withhold(),
        "可补偿发射：既非两阶段也不扣发"
    );
    let post = t.lookup(&ClassId::new("api"), &VerbId::new("post")).unwrap();
    assert!(!post.staged_shape() && !post.withhold());
    // 可补偿但在硬清单上（删除→可从回收站恢复）：不是两阶段形状，但仍逐次同意。
    let mut s = Declarations::new();
    s.register("fs", "delete", comp("restore", false)).unwrap();
    s.register("fs", "restore", ext(true)).unwrap();
    let s = s.check_all().unwrap();
    let del = s.lookup(&ClassId::new("fs"), &VerbId::new("delete")).unwrap();
    assert!(
        !del.staged_shape() && del.withhold(),
        "硬清单优先于可补偿性"
    );
}

/// [EDIT] 降档声明住在真理表（F3 ρ-EQ：等价由注册期声明固定），且只准收窄：
/// 目标须同类已注册、严重度不升（可重复<消耗<发射；可补偿<外部；可摊销<不可摊销）。
#[test]
fn degrade_declared_and_only_narrows() {
    let t = browser_verb_table(); // 含 read_full→read_preview、submit→submit_draft，check_all 已过
    assert_eq!(
        t.derive_handler_policy(&ClassId::new("doc")).unwrap()
            .degrade
            .get("read_full")
            .map(String::as_str),
        Some("read_preview")
    );
    assert_eq!(
        t.derive_handler_policy(&ClassId::new("page")).unwrap()
            .degrade
            .get("submit")
            .map(String::as_str),
        Some("submit_draft")
    );

    // 升档伪装成降档：submit_draft（可补偿、可摊销）→ submit（外部、硬清单）——拒。
    let mut u = Declarations::new();
    u.register("page", "submit", ext(false)).unwrap();
    u.register(
        "page",
        "submit_draft",
        comp("discard", true).degrades_to("submit"),
    )
    .unwrap();
    u.register("page", "discard", ext(true)).unwrap();
    assert!(
        matches!(u.check_all(), Err(VerbError::Incoherent(_))),
        "降档不得升严重度"
    );

    // 目标未注册／跨类：拒（等价必须指向同 handler 下的已声明动词）。
    let mut v = Declarations::new();
    v.register(
        "doc",
        "read_full",
        VerbEntry::repeatable().degrades_to("read_preview"),
    )
    .unwrap();
    assert!(matches!(v.check_all(), Err(VerbError::Incoherent(_))));
    v.register("other", "read_preview", VerbEntry::repeatable())
        .unwrap();
    assert!(
        matches!(v.check_all(), Err(VerbError::Incoherent(_))),
        "跨类目标不算"
    );
}

/// [ρ] 纪律的结构保证：Held 动词要求类已声明 ρ（F2 必须知道怎么还，不许先记后补）；
/// ρ 一次声明、不可改（无 setter，重复声明即拒）；补偿链一步闭合。
#[test]
fn held_requires_declared_class_rho_and_rho_is_immutable() {
    let mut t = Declarations::new();
    t.register("port", "bind", held()).unwrap();
    assert!(matches!(t.check_all(), Err(VerbError::ClassNotDeclared)));
    t.declare_class("port", RevertGrade::Inverse).unwrap();
    assert_eq!(
        t.declare_class("port", RevertGrade::External),
        Err(VerbError::ClassAlreadyDeclared)
    );
    assert_eq!(t.check_all().unwrap().derive_holding_grade(&ClassId::new("port")), Some(RevertGrade::Inverse));
    // 补偿动词自己又要补偿——链不闭合，拒。
    let mut c = Declarations::new();
    c.register("api", "post", comp("cancel", true)).unwrap();
    c.register("api", "cancel", comp("uncancel", true)).unwrap();
    c.register("api", "uncancel", ext(true)).unwrap();
    assert!(matches!(c.check_all(), Err(VerbError::Incoherent(_))));
}

/// [PROJ] 承重件：真理表是共享的——F2 的类档与 F3 的 handler 策略都是它的投影，
/// 且投影**真的驱动**两台已冻结机器跑通：teardown 收敛＋补偿恰好一次；
/// 一批可摊销 click/type 直发、硬清单 submit 扣发待批、批准后放出。
#[test]
fn truth_table_is_shared_source_for_f2_teardown_and_f3_monitor() {
    let t = browser_verb_table();

    // ---- 投影到 F2：类档 → ClassDecl，跑一次 teardown ----
    for (class, expect) in [
        ("enclosure", RevertGrade::Inverse),
        ("proc", RevertGrade::Inverse),
        ("page", RevertGrade::Inverse),
        ("tcp-port", RevertGrade::Inverse),
        ("workspace", RevertGrade::Inverse),
        ("reservation", RevertGrade::Compensable),
    ] {
        assert_eq!(
            t.derive_holding_grade(&ClassId::new(class)),
            Some(expect),
            "F2 档投影与 browser_ledger 手写值不符：{class}"
        );
    }
    let mut l = Ledger::new();
    for class in ["enclosure", "reservation"] {
        l.register_class(ClassDecl {
            cleanup: portos_rm::cleanup::CleanupPolicy::AccountingOnly,
            class_id: class.into(),
            algebra: AlgebraTag::Exclusive,
            release_idempotent: true,
            lease_duration: Some(300).map(|s: u64| LeaseDuration::try_from(s).unwrap()),
            revert_grade: t.derive_holding_grade(&ClassId::new(class)).unwrap(),
        })
        .unwrap();
    }
    l.create_pool(
        &l.registered_class::<Ex>(&ClassId::new("enclosure"))
            .unwrap(),
        InstanceId::new("e"),
        Capacity::new(Ex::Token).unwrap(),
    )
    .unwrap();
    l.create_pool(
        &l.registered_class::<Ex>(&ClassId::new("reservation"))
            .unwrap(),
        InstanceId::new("r"),
        Capacity::new(Ex::Token).unwrap(),
    )
    .unwrap();
    let e = l
        .grant(
            &l.pool::<Ex>(&ResourceKey::new(
                ClassId::new("enclosure"),
                InstanceId::new("e"),
            ))
            .unwrap(),
            GrantRequest {
                owner: SubjectId::new("s"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(0u64).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap();
    l.grant(
        &l.pool::<Ex>(&ResourceKey::new(
            ClassId::new("reservation"),
            InstanceId::new("r"),
        ))
        .unwrap(),
        GrantRequest {
            owner: SubjectId::new("s"),
            claim: Claim::new(Ex::Token).unwrap(),
            generation: Generation::new("g"),
            parent: Some(e).map(|id| l.holding(id).expect("parent exists").handle()),
            lease: LeaseRequest::UseClassDefault,
            now: Timestamp::try_from(0u64).unwrap(),
        },
    )
    .map(|h| h.id())
    .unwrap();
    let mut o = Orchestrator::new(l);
    assert!(
        matches!(o.teardown("s", 0, None), RunOutcome::Completed { failed } if failed.is_empty())
    );
    let (_, comp) = o.world.effects_fingerprint();
    assert_eq!(
        comp.len(),
        1,
        "表投影的 Compensable 类档驱动 F2 补偿恰好一次"
    );

    // ---- 投影到 F3：page handler 的策略 → Policy，跑一批自治动作 ----
    let hp = t.derive_handler_policy(&ClassId::new("page")).unwrap();
    assert_eq!(
        hp.withhold,
        ["submit".to_string()].into_iter().collect(),
        "只有硬清单进扣发集"
    );
    assert!(
        hp.budget.contains("click")
            && hp.budget.contains("submit")
            && !hp.budget.contains("snapshot")
    );
    let mut pol = Policy::from_checked(&hp);
    for v in ["click", "type", "submit", "submit_draft"] {
        pol.allow(v, "app.example");
    }
    let mut m = Monitor::new(pol, Mode::Strict, Ledger::new(), "fib");
    let plan = vec![
        WAction::new("click", "app.example", "1", 1),
        WAction::new("type", "app.example", "2", 1),
        WAction::new("submit", "app.example", "3", 1),
    ];
    let consent = |n: &str, b: u64| Consent {
        plan_hash: "blake3:p".into(),
        budget: b,
        nonce: n.into(),
        ttl_expires_at: 100,
    };
    m.admit(plan, "blake3:p", consent("n1", 2), 10).unwrap();
    assert_eq!(
        *m.run(10),
        MonState::AwaitingApproval,
        "硬清单 submit 扣发待批"
    );
    assert_eq!(
        m.world.emitted.len(),
        2,
        "可摊销的 click/type 在一次同意下直发——一次同意换一批"
    );
    m.approve(consent("n2", 1), 10).unwrap();
    assert_eq!(m.world.emitted.len(), 3, "批准后 submit 放出");

    // mail handler：同一张表投影出另一台监督器的扣发集（D1）。
    assert_eq!(
        t.derive_handler_policy(&ClassId::new("mail")).unwrap().withhold,
        ["send".to_string()].into_iter().collect()
    );
}
