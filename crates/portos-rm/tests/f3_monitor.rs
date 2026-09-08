//! F3 法则测试 —— 每个测试名 = 它执行的定理/纪律（对照 freeze-f3 三列表）。
//! 措辞纪律：断言不得强于被引定理——renewal 档的主张一律以"事务形状"的有穷渲染表达。

use portos_rm::identity::{ClassId, Generation, HoldingHandle, InstanceId, ResourceKey, SubjectId};
use portos_rm::ledger::GrantRequest;
use portos_rm::ledger::*;
use portos_rm::monitor::*;
use portos_rm::ra::Ex;
use portos_rm::registry::{Capacity, Claim};
use portos_rm::time::{LeaseDuration, LeaseRequest, Timestamp};

const H: &str = "blake3:plan-v1";
const NOW: u64 = 10;
const TTL: u64 = 100;

fn consent(nonce: &str, budget: u64) -> Consent {
    Consent {
        plan_hash: H.into(),
        budget,
        nonce: nonce.into(),
        ttl_expires_at: TTL,
    }
}

fn base_policy() -> Policy {
    let mut p = Policy::default();
    // 同意范围 = (动词, 目标) 白名单——范围动词敏感，attenuate 才有救回的空间。
    p.allow("post", "api.example.com");
    p.allow("post", "mail.corp");
    p.allow("send", "mail.corp");
    p.allow("read_preview", "quiet.internal");
    p.staged_verbs.insert("send".into());
    p.degrade.insert("read_full".into(), "read_preview".into());
    p.confined_targets.insert("sketchy.site".into());
    p
}

fn a(verb: &str, target: &str, payload: &str) -> WAction {
    WAction::new(verb, target, payload, 1)
}

/// [SAFE] Schneider／IJIS 2005：precise 意义下监督器上限＝safety。
/// 无扣发件时本监督器退化为截停自动机，须两面兼验：
/// ① 透明性——合法输入不改动（逐字节照放）；② 违规＝交付最长合法前缀后停。
#[test]
fn truncation_enforces_safety_precisely() {
    // ① 透明性
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    let plan = vec![
        a("post", "api.example.com", "1"),
        a("post", "mail.corp", "2"),
        a("post", "api.example.com", "3"),
    ];
    m.admit(plan.clone(), H, consent("n1", 10), NOW).unwrap();
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::Completed));
    assert_eq!(
        m.world.emitted, plan,
        "合法输入必须原样通过（precise 的透明性半边）"
    );

    // ② 最长合法前缀（sink 越界、无降档声明 ⇒ fail-stop）
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    let plan = vec![
        a("post", "api.example.com", "1"),
        a("post", "mail.corp", "2"),
        a("post", "evil.example", "3"),
        a("post", "api.example.com", "4"),
    ];
    m.admit(plan.clone(), H, consent("n1", 10), NOW).unwrap();
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::FailStop { at: 2 }));
    assert_eq!(m.world.emitted, plan[..2].to_vec(), "交付恰为最长合法前缀");
    assert!(m.trace.contains(&Ev::FailStop { at: 2 }));
}

/// [RENEW] TISSEC 2009 定理 3.3/3.4 的分离见证（有穷渲染）：
/// 事务性质（staged 内容非 commit 不可见、commit 后须完整可见）不是 safety——
/// 穷举全部确定性截停策略（首截停点刻画之），每一种都在 commit/abort
/// 两条输入上至少输掉一条；带扣发缓冲的 edit automaton 两条全赢。
/// 这就是"扣发关口把 egress 从 safety 档升到 renewal 档"的可运行形态。
#[test]
fn no_truncation_strategy_passes_transaction_witness_but_edit_does() {
    let mk = |last: &str| {
        vec![
            a("send", "mail.corp", "a"),
            a("send", "mail.corp", "b"),
            a(last, "mail.corp", ""),
        ]
    };
    let plan_commit = mk("commit");
    let plan_abort = mk("abort");
    let content = |out: &[WAction]| {
        out.iter()
            .filter(|x| x.verb == "send")
            .map(|x| x.payload.clone())
            .collect::<Vec<_>>()
    };
    let tx_valid = |input: &[WAction], out: &[WAction]| -> bool {
        if input.last().unwrap().verb == "commit" {
            content(out) == vec!["a".to_string(), "b".to_string()] // 完整、按序
        } else {
            content(out).is_empty() // 一件都不得泄漏
        }
    };

    // 截停自动机：决策只依赖已见前缀，而两条输入的前两步前缀相同 ⇒ 同一策略
    // 在两条输入上的前缀行为必然一致。确定性策略 ↔ 首截停点；输入长 3，
    // halt@3 与"永不截停"同效，故 4 类穷尽全部截停自动机。
    for halt in [Some(0), Some(1), Some(2), None] {
        let out_c = truncation_run(&plan_commit, halt);
        let out_a = truncation_run(&plan_abort, halt);
        assert!(
            !(tx_valid(&plan_commit, &out_c) && tx_valid(&plan_abort, &out_a)),
            "截停策略 {halt:?} 竟两条全过——分离见证失效"
        );
    }

    // edit automaton（本监督器，send 声明为 staged）：commit 分支
    let staged_plan = vec![a("send", "mail.corp", "a"), a("send", "mail.corp", "b")];
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    m.admit(staged_plan.clone(), H, consent("n1", 0), NOW)
        .unwrap();
    assert_eq!(*m.run(NOW), MonState::AwaitingApproval);
    assert!(
        m.world.emitted.is_empty(),
        "悬置期与 abort 分支前缀行为一致：零发射"
    );
    m.approve(consent("n2", 2), NOW).unwrap();
    assert!(
        tx_valid(&plan_commit, &m.world.emitted),
        "commit 分支：批准后完整按序放出"
    );

    // abort 分支（同一前缀行为，ttl 到期即废）
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    m.admit(staged_plan, H, consent("n1", 0), NOW).unwrap();
    assert_eq!(*m.run(NOW), MonState::AwaitingApproval);
    m.expire(TTL + 1).unwrap();
    assert!(
        tx_valid(&plan_abort, &m.world.emitted),
        "abort 分支：零泄漏"
    );
}

/// [WYS] 同意可判定、无同意零效应（m0 accept_4 在监督器层重演）：
/// 哈希不符 / ttl 过期 ⇒ 准入拒绝，连第一个动作都不看，预算池不铸造。
/// （nonce 重放的拒绝在 approve/resume 两测内断言——重放窗口在那里才存在。）
#[test]
fn wysiwys_gate_no_consent_no_effect() {
    let plan = vec![a("post", "api.example.com", "1")];

    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    let mut bad = consent("n1", 10);
    bad.plan_hash = "blake3:tampered".into();
    assert_eq!(
        m.admit(plan.clone(), H, bad, NOW),
        Err(Refusal::ConsentMismatch)
    );
    assert!(m.world.emitted.is_empty());
    assert_eq!(m.pool_spent("n1"), 0);
    assert!(m.trace.contains(&Ev::Refused {
        why: Refusal::ConsentMismatch
    }));

    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    assert_eq!(
        m.admit(plan, H, consent("n1", 10), TTL + 1),
        Err(Refusal::ExpiredTtl)
    );
    assert!(m.world.emitted.is_empty());
}

/// [SUPPR]+[INSERT] withhold ＝ suppression ＋ 批准后 insertion：
/// 压制期世界不可见；批准（新四元组）后按原序放出、恰好一次；
/// 旧 nonce 批准被拒；重复批准无处下手（状态机已 Done）。
#[test]
fn withhold_approve_emits_exactly_once_in_order() {
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    let plan = vec![
        a("send", "mail.corp", "x"),
        a("post", "api.example.com", "y"),
        a("send", "mail.corp", "z"),
    ];
    m.admit(plan, H, consent("n1", 1), NOW).unwrap();
    assert_eq!(*m.run(NOW), MonState::AwaitingApproval);
    assert_eq!(
        m.world.emitted,
        vec![a("post", "api.example.com", "y")],
        "压制期：staged 件零可见"
    );
    assert_eq!(
        m.trace
            .iter()
            .filter(|e| matches!(e, Ev::Suppressed { .. }))
            .count(),
        2
    );

    // 旧 nonce（准入时已用）不得充当批准。
    assert_eq!(m.approve(consent("n1", 2), NOW), Err(Refusal::StaleNonce));
    // 预算盖不住整批 ⇒ 整批不放（事务形状，无半截插入）。
    assert_eq!(
        m.approve(consent("n2", 1), NOW),
        Err(Refusal::ApprovalBudgetShort)
    );
    assert!(m.world.emitted.len() == 1, "被拒的批准不得漏出任何缓冲件");

    m.approve(consent("n3", 2), NOW).unwrap();
    assert_eq!(
        m.world.emitted,
        vec![
            a("post", "api.example.com", "y"),
            a("send", "mail.corp", "x"),
            a("send", "mail.corp", "z"),
        ],
        "批准后：缓冲按原序放出"
    );
    let count_x = m.world.emitted.iter().filter(|e| e.payload == "x").count();
    assert_eq!(count_x, 1, "恰好一次");
    assert_eq!(
        m.approve(consent("n4", 2), NOW),
        Err(Refusal::WrongState),
        "无重复插入窗口"
    );
    assert_eq!(m.world.emitted.len(), 3);
}

/// [TTL] feigning acceptance 的代价封顶：悬而未决的压制段不得无限悬置——
/// ttl 到期即废：缓冲永不发射、段内获取侧持有走 F2 补偿（恰好一次）、
/// 过期后的批准被拒。绝不无限悬置，也绝不迟到放行。
#[test]
fn ttl_bounds_feigning_acceptance_expiry_compensates() {
    let mut l = Ledger::new();
    l.register_class(ClassDecl {
        class_id: "reservation".into(),
        algebra: AlgebraTag::Exclusive,
        release_idempotent: true,
        lease_duration: Some(300).map(|s: u64| LeaseDuration::try_from(s).unwrap()),
        revert_grade: RevertGrade::Compensable,
    })
    .unwrap();
    l.create_pool(
        &l.registered_class::<Ex>(&ClassId::new("reservation"))
            .unwrap(),
        InstanceId::new("email-42"),
        Capacity::new(Ex::Token).unwrap(),
    )
    .unwrap();

    let mut m = Monitor::new(base_policy(), Mode::Strict, l, "fib");
    m.admit(vec![a("send", "mail.corp", "x")], H, consent("n1", 0), NOW)
        .unwrap();
    // reserve→同意→commit 的 reserve 步：段内押下一笔可补偿持有。
    m.stage_acquire(
        &m.orch
            .ledger
            .pool::<Ex>(&ResourceKey::new(
                ClassId::new("reservation"),
                InstanceId::new("email-42"),
            ))
            .unwrap(),
        Generation::new("g"),
        Claim::new(Ex::Token).unwrap(),
        Timestamp::try_from(NOW).unwrap(),
    )
    .unwrap();
    assert_eq!(*m.run(NOW), MonState::AwaitingApproval);

    // 未到期不得废（废也要有据）。
    assert_eq!(m.expire(NOW), Err(Refusal::WrongState));
    // 过了 ttl：批准窗口已死——同意已过期，段只能废。
    assert_eq!(
        m.approve(consent("n2", 2), TTL + 1),
        Err(Refusal::ExpiredTtl)
    );

    m.expire(TTL + 1).unwrap();
    assert_eq!(
        *m.state(),
        MonState::Done(MonOutcome::Aborted { expired: true })
    );
    assert!(m.world.emitted.is_empty(), "被废的段零泄漏");
    let (_, compensated) = m.orch.world.effects_fingerprint();
    assert_eq!(compensated.len(), 1, "押下的预约经 F2 补偿恰好一次");
    assert_eq!(
        m.orch
            .ledger
            .live_snapshot(&SubjectId::new(&m.seg_subject()))
            .len(),
        0
    );
    assert!(m.trace.contains(&Ev::SegmentAborted { expired: true }));

    // 迟到的批准无处落地。
    assert_eq!(
        m.approve(consent("n3", 2), TTL + 2),
        Err(Refusal::WrongState)
    );
}

/// [TTL]+[ESC]+[B12] 悬置的第二形态：escalate 的 Paused 同样押着段内持有，同样由**原**同意的
/// ttl 封顶。过期后：增量同意（哪怕自身 ttl 活着）被拒、段只能废——缓冲弃置、段内持有走 F2
/// 补偿恰好一次、前缀站着不动。v1 只封顶 AwaitingApproval，Paused 可被无限悬置（统一复核抓到）。
#[test]
fn paused_segment_is_bounded_by_original_ttl() {
    let mut l = Ledger::new();
    l.register_class(ClassDecl {
        class_id: "reservation".into(),
        algebra: AlgebraTag::Exclusive,
        release_idempotent: true,
        lease_duration: Some(300).map(|s: u64| LeaseDuration::try_from(s).unwrap()),
        revert_grade: RevertGrade::Compensable,
    })
    .unwrap();
    l.create_pool(
        &l.registered_class::<Ex>(&ClassId::new("reservation"))
            .unwrap(),
        InstanceId::new("email-42"),
        Capacity::new(Ex::Token).unwrap(),
    )
    .unwrap();
    let mut m = Monitor::new(base_policy(), Mode::Escalate, l, "fib");
    let plan: Vec<WAction> = (1..=3)
        .map(|i| a("post", "api.example.com", &i.to_string()))
        .collect();
    m.admit(plan.clone(), H, consent("n1", 1), NOW).unwrap();
    m.stage_acquire(
        &m.orch
            .ledger
            .pool::<Ex>(&ResourceKey::new(
                ClassId::new("reservation"),
                InstanceId::new("email-42"),
            ))
            .unwrap(),
        Generation::new("g"),
        Claim::new(Ex::Token).unwrap(),
        Timestamp::try_from(NOW).unwrap(),
    )
    .unwrap();
    assert_eq!(*m.run(NOW), MonState::Paused);
    assert!(m.is_suspended());
    assert_eq!(m.expire(NOW), Err(Refusal::WrongState), "未到期不得废");

    // 原同意过期：新鲜的增量同意也救不回——窗口是原同意开的。
    let mut fresh = consent("n2", 5);
    fresh.ttl_expires_at = TTL + 1000;
    assert_eq!(
        m.resume_with(fresh, TTL + 1).unwrap_err(),
        Refusal::ExpiredTtl
    );
    assert!(
        m.trace.contains(&Ev::Refused {
            why: Refusal::ExpiredTtl
        }),
        "拒绝可听见"
    );
    assert_eq!(m.world.emitted.len(), 1, "被拒的续跑不得放出任何动作");

    m.expire(TTL + 1).unwrap();
    assert_eq!(
        *m.state(),
        MonState::Done(MonOutcome::Aborted { expired: true })
    );
    assert_eq!(m.world.emitted, plan[..1].to_vec(), "前缀站着不动");
    let (_, compensated) = m.orch.world.effects_fingerprint();
    assert_eq!(compensated.len(), 1, "段内押下的预约经 F2 补偿恰好一次");
    assert_eq!(
        m.orch
            .ledger
            .live_snapshot(&SubjectId::new(&m.seg_subject()))
            .len(),
        0
    );
    assert!(!m.is_suspended());
    assert_eq!(
        m.resume_with(consent("n3", 5), TTL + 2),
        Err(Refusal::WrongState),
        "废段无处续跑"
    );
}

/// [EDIT]+[ρ-EQ] attenuate＝edit，但等价必须来自声明表（定理 2.5 的等价警告 ⇒
/// ρ 选取纪律）：有声明且改写后重检通过 ⇒ 降档放行并入 trace；无声明 ⇒ 拒绝，
/// 执行者绝不代拟等价；有声明但救不回范围 ⇒ 照样 fail-stop（edit 后须重过同一谓词）。
#[test]
fn attenuate_only_by_declared_equivalence() {
    // 有声明：read_full → read_preview（声明于策略注册，非执行时发明），改写后在范围内。
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    m.admit(
        vec![a("read_full", "quiet.internal", "doc")],
        H,
        consent("n1", 10),
        NOW,
    )
    .unwrap();
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::Completed));
    assert_eq!(
        m.world.emitted,
        vec![a("read_preview", "quiet.internal", "doc")]
    );
    assert!(m.trace.contains(&Ev::Attenuated {
        from: "read_full".into(),
        to: "read_preview".into()
    }));

    // 无声明：同一越界目标、未声明动词 ⇒ fail-stop，世界零效应。
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    m.admit(
        vec![a("scrape", "quiet.internal", "doc")],
        H,
        consent("n1", 10),
        NOW,
    )
    .unwrap();
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::FailStop { at: 0 }));
    assert!(m.world.emitted.is_empty(), "无声明等价 ⇒ 不许发明降档");

    // 有声明但救不回：read_full → read_preview 对 evil.example 依然越界 ⇒ fail-stop。
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    m.admit(
        vec![a("read_full", "evil.example", "doc")],
        H,
        consent("n1", 10),
        NOW,
    )
    .unwrap();
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::FailStop { at: 0 }));
    assert!(m.world.emitted.is_empty(), "降档动作必须重过同一 sink 谓词");
}

/// 演练第二只 bug 的墓碑法则：**insertion 也是发射**——被压制动作在批准放出时
/// 不得绕过 sink 复检。实现以缓冲不变式（先 sink 后扣发的管线序）达成：
/// 越界目标的 staged 动作根本进不了缓冲（fail-stop 于压制之前）；
/// 隔离目标的 staged 动作进替身而非缓冲（改写到替身无需同意）。
#[test]
fn insertion_is_still_an_emission_sink_holds_through_approval() {
    // 越界 staged：初版实现会先压制、批准后盲放（泄漏）；现应 fail-stop 于第 0 步。
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    let plan = vec![
        a("send", "evil.example", "leak"),
        a("send", "mail.corp", "ok"),
    ];
    m.admit(plan, H, consent("n1", 0), NOW).unwrap();
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::FailStop { at: 0 }));
    assert!(
        m.world.emitted.is_empty(),
        "越界 staged 件绝不可经批准漏入世界"
    );
    assert_eq!(
        m.approve(consent("n2", 9), NOW),
        Err(Refusal::WrongState),
        "无缓冲可批"
    );

    // 隔离目标的 staged：进替身（隔离优先于扣发——替身不是真实边界，无需同意）。
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    m.admit(
        vec![a("send", "sketchy.site", "s")],
        H,
        consent("n1", 0),
        NOW,
    )
    .unwrap();
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::Completed));
    assert!(m.world.emitted.is_empty());
    assert_eq!(m.world.standin, vec![a("send", "sketchy.site", "s")]);
}

/// confine＝改写到替身：真实世界零效应，工作完整落入替身通道（不丢、不静默）。
#[test]
fn confine_redirects_to_stand_in_zero_real_effect() {
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    let plan = vec![
        a("post", "sketchy.site", "s"),
        a("post", "api.example.com", "r"),
    ];
    m.admit(plan, H, consent("n1", 10), NOW).unwrap();
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::Completed));
    assert_eq!(
        m.world.emitted,
        vec![a("post", "api.example.com", "r")],
        "真实通道零替身件"
    );
    assert_eq!(
        m.world.standin,
        vec![a("post", "sketchy.site", "s")],
        "替身通道完整承接"
    );
    assert!(m.trace.contains(&Ev::Confined {
        verb: "post".into(),
        target: "sketchy.site".into()
    }));
}

/// [ESC] escalate＝停下—增量同意—续跑（m0 收尾清单#2 的可运行形态）：
/// 超界即停、零越界发射；旧 nonce/错哈希的"增量同意"被拒；
/// 新四元组续跑后从暂停动作精确继续——前缀不重放、暂停件不跳过。
#[test]
fn escalate_pauses_and_resumes_exactly_with_fresh_consent() {
    let mut m = Monitor::new(base_policy(), Mode::Escalate, Ledger::new(), "fib");
    let plan: Vec<WAction> = (1..=4)
        .map(|i| a("post", "api.example.com", &i.to_string()))
        .collect();
    m.admit(plan.clone(), H, consent("n1", 2), NOW).unwrap();
    assert_eq!(*m.run(NOW), MonState::Paused);
    assert_eq!(m.world.emitted, plan[..2].to_vec(), "停下时恰为预算内前缀");
    assert!(m.trace.contains(&Ev::Escalated { at: 2 }));

    assert_eq!(
        m.resume_with(consent("n1", 2), NOW).unwrap_err(),
        Refusal::StaleNonce
    );
    let mut wrong = consent("n2", 2);
    wrong.plan_hash = "blake3:other".into();
    assert_eq!(
        m.resume_with(wrong, NOW).unwrap_err(),
        Refusal::ConsentMismatch
    );
    assert_eq!(m.world.emitted.len(), 2, "被拒的增量同意不得放出任何动作");

    // 增量同意可以再次不够——escalate 循环必须可组合（真实节奏就是挤牙膏式增批）。
    assert_eq!(
        *m.resume_with(consent("n2", 1), NOW).unwrap(),
        MonState::Paused
    );
    assert_eq!(
        m.world.emitted,
        plan[..3].to_vec(),
        "第二次停下：又一段预算内前缀"
    );
    assert_eq!(
        *m.resume_with(consent("n3", 1), NOW).unwrap(),
        MonState::Done(MonOutcome::Completed)
    );
    assert_eq!(
        m.world.emitted, plan,
        "续跑：前缀不重放、暂停件不跳过、按序完成"
    );
    for i in 1..=4 {
        let n = m
            .world
            .emitted
            .iter()
            .filter(|e| e.payload == i.to_string())
            .count();
        assert_eq!(n, 1, "动作 {i} 恰好一次");
    }
    assert_eq!(
        m.pool_spent("n1") + m.pool_spent("n2") + m.pool_spent("n3"),
        4,
        "三池合计＝总发射"
    );
}

/// [GATE] 预算＝counting capability 的塌缩在 F1 语义下的正名：
/// 同意即铸池（● 容量），花费即行（◯ 碎片，一行一笔），闸门即发放方闸门。
/// 无任何减法：行只增不改，合计是重算；透支＝mint 被拒（Conflict）。
#[test]
fn budget_is_rows_not_decrement_gate_is_issuer_gate() {
    let mut m = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    let plan: Vec<WAction> = (1..=5)
        .map(|i| a("post", "api.example.com", &i.to_string()))
        .collect();
    m.admit(plan, H, consent("n1", 3), NOW).unwrap();
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::FailStop { at: 3 }));
    assert_eq!(m.world.emitted.len(), 3);
    assert_eq!(m.pool_spent("n1"), 3, "合计＝逐行折叠重算");
    assert_eq!(
        m.orch
            .ledger
            .live()
            .filter(|h| h.class_id.as_str() == "budget")
            .count(),
        3,
        "一笔花费一行，行为真相"
    );
    m.orch.ledger.invariant().unwrap();
    // 闸门就是 F1 的发放方闸门本门：直接向池再 mint 一笔也被同一检查拒绝。
    let err = m
        .orch
        .ledger
        .grant(
            &m.orch
                .ledger
                .pool::<portos_rm::ra::Count>(&ResourceKey::new(
                    ClassId::new("budget"),
                    InstanceId::new("n1"),
                ))
                .unwrap(),
            GrantRequest {
                owner: SubjectId::new("fib:spent"),
                claim: Claim::new(portos_rm::ra::Count::Value(1)).unwrap(),
                generation: Generation::new("c"),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
                now: Timestamp::try_from(NOW).unwrap(),
            },
        )
        .map(|h| h.id())
        .unwrap_err();
    assert_eq!(err, LedgerError::Conflict);
}

/// [PREFIX] fail-stop 的两半：已发射 w-effect 站着不动（前缀交付，不承诺跨效应
/// 原子性），段内获取侧持有经 F2 teardown 收回（前缀回滚——roadmap Phase C
/// 明言"不做、等 Phase D 可逆档资源支撑"的那一项，在此接通）。
#[test]
fn strict_failstop_delivers_prefix_and_rolls_back_segment_holdings() {
    let mut l = Ledger::new();
    l.register_class(ClassDecl {
        class_id: "scratch".into(),
        algebra: AlgebraTag::Exclusive,
        release_idempotent: true,
        lease_duration: Some(600).map(|s: u64| LeaseDuration::try_from(s).unwrap()),
        revert_grade: RevertGrade::Inverse,
    })
    .unwrap();
    l.create_pool(
        &l.registered_class::<Ex>(&ClassId::new("scratch")).unwrap(),
        InstanceId::new("s1"),
        Capacity::new(Ex::Token).unwrap(),
    )
    .unwrap();
    l.create_pool(
        &l.registered_class::<Ex>(&ClassId::new("scratch")).unwrap(),
        InstanceId::new("s2"),
        Capacity::new(Ex::Token).unwrap(),
    )
    .unwrap();

    let mut m = Monitor::new(base_policy(), Mode::Strict, l, "fib");
    let plan = vec![
        a("post", "api.example.com", "1"),
        a("post", "mail.corp", "2"),
        a("post", "evil.example", "3"),
    ];
    m.admit(plan.clone(), H, consent("n1", 10), NOW).unwrap();
    m.stage_acquire(
        &m.orch
            .ledger
            .pool::<Ex>(&ResourceKey::new(
                ClassId::new("scratch"),
                InstanceId::new("s1"),
            ))
            .unwrap(),
        Generation::new("g"),
        Claim::new(Ex::Token).unwrap(),
        Timestamp::try_from(NOW).unwrap(),
    )
    .unwrap();
    m.stage_acquire(
        &m.orch
            .ledger
            .pool::<Ex>(&ResourceKey::new(
                ClassId::new("scratch"),
                InstanceId::new("s2"),
            ))
            .unwrap(),
        Generation::new("g"),
        Claim::new(Ex::Token).unwrap(),
        Timestamp::try_from(NOW).unwrap(),
    )
    .unwrap();

    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::FailStop { at: 2 }));
    assert_eq!(m.world.emitted, plan[..2].to_vec(), "前缀交付");

    // [SEG-TX] 回滚由 monitor 在终态自动完成——不靠上层记得调 rollback_segment。
    assert_eq!(
        m.orch
            .ledger
            .live_snapshot(&SubjectId::new(&m.seg_subject()))
            .len(),
        0,
        "段持有清零"
    );
    let (released, _) = m.orch.world.effects_fingerprint();
    assert_eq!(released.len(), 2, "获取侧经 F2 释放");
    assert_eq!(m.world.emitted.len(), 2, "已发射的 w-effect 不因回滚消失");
    assert!(m.trace.contains(&Ev::SegmentRolledBack { holdings: 2 }));
    m.rollback_segment(NOW);
    assert_eq!(
        m.trace
            .iter()
            .filter(|e| matches!(e, Ev::SegmentRolledBack { .. }))
            .count(),
        1,
        "重复回滚幂等"
    );
}

/// [SEG-TX] 决策 4（用户裁定 2026-09-05）：段＝事务。
/// commit（Completed）⇒ 段内持有全部转授给 fiber；任何非 commit 终态（FailStop／Truncated／Aborted）⇒
/// 段内未提前 promote 的持有由 monitor 自动回滚；promote＝提前 commit 一笔，在之后的 abort 中幸存。
#[test]
fn segment_is_a_transaction_commit_promotes_abort_rolls_back() {
    let scratch = || {
        let mut l = Ledger::new();
        l.register_class(ClassDecl {
            class_id: "scratch".into(),
            algebra: AlgebraTag::Exclusive,
            release_idempotent: true,
            lease_duration: Some(600).map(|s: u64| LeaseDuration::try_from(s).unwrap()),
            revert_grade: RevertGrade::Inverse,
        })
        .unwrap();
        l.create_pool(
            &l.registered_class::<Ex>(&ClassId::new("scratch")).unwrap(),
            InstanceId::new("s1"),
            Capacity::new(Ex::Token).unwrap(),
        )
        .unwrap();
        l.create_pool(
            &l.registered_class::<Ex>(&ClassId::new("scratch")).unwrap(),
            InstanceId::new("s2"),
            Capacity::new(Ex::Token).unwrap(),
        )
        .unwrap();
        l
    };

    // abort 分支：s1 提前 promote，s2 留在段内；fail-stop 后 s1 归 fiber、s2 被释放。
    let mut m = Monitor::new(base_policy(), Mode::Strict, scratch(), "fib");
    m.admit(
        vec![
            a("post", "api.example.com", "1"),
            a("post", "evil.example", "2"),
        ],
        H,
        consent("n1", 10),
        NOW,
    )
    .unwrap();
    let h1 = m
        .stage_acquire(
            &m.orch
                .ledger
                .pool::<Ex>(&ResourceKey::new(
                    ClassId::new("scratch"),
                    InstanceId::new("s1"),
                ))
                .unwrap(),
            Generation::new("g"),
            Claim::new(Ex::Token).unwrap(),
            Timestamp::try_from(NOW).unwrap(),
        )
        .unwrap();
    let h2 = m
        .stage_acquire(
            &m.orch
                .ledger
                .pool::<Ex>(&ResourceKey::new(
                    ClassId::new("scratch"),
                    InstanceId::new("s2"),
                ))
                .unwrap(),
            Generation::new("g"),
            Claim::new(Ex::Token).unwrap(),
            Timestamp::try_from(NOW).unwrap(),
        )
        .unwrap();
    assert_eq!(
        m.promote(&HoldingHandle::new(h2.id(), Generation::new("wrong-gen"))),
        Err(PromoteError::Ledger(LedgerError::StaleGeneration))
    );
    m.promote(&HoldingHandle::new(h1.id(), Generation::new("g")))
        .unwrap();
    assert!(m.trace.contains(&Ev::Promoted { holding: h1.id() }));
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::FailStop { at: 1 }));
    assert_eq!(
        m.orch
            .ledger
            .live_snapshot(&SubjectId::new(&m.seg_subject()))
            .len(),
        0,
        "段内清零"
    );
    assert_eq!(
        m.orch
            .ledger
            .live_snapshot(&SubjectId::new("fib"))
            .iter()
            .map(|it| it.instance.to_string())
            .collect::<Vec<_>>(),
        vec!["s1"],
        "提前 commit 的幸存"
    );
    let (released, _) = m.orch.world.effects_fingerprint();
    assert_eq!(
        released
            .iter()
            .map(|(_, i, _)| i.to_string())
            .collect::<Vec<_>>(),
        vec!["s2"],
        "只有未 promote 的被释放"
    );
    assert!(m.trace.contains(&Ev::SegmentRolledBack { holdings: 1 }));
    assert_eq!(
        m.promote(&HoldingHandle::new(h2.id(), Generation::new("g"))),
        Err(PromoteError::SegmentClosed),
        "终态后无可提前"
    );

    // commit 分支：走完即 commit，段内剩余持有全部转授给 fiber，世界零释放。
    let mut m = Monitor::new(base_policy(), Mode::Strict, scratch(), "fib");
    m.admit(
        vec![a("post", "api.example.com", "1")],
        H,
        consent("n1", 10),
        NOW,
    )
    .unwrap();
    m.stage_acquire(
        &m.orch
            .ledger
            .pool::<Ex>(&ResourceKey::new(
                ClassId::new("scratch"),
                InstanceId::new("s1"),
            ))
            .unwrap(),
        Generation::new("g"),
        Claim::new(Ex::Token).unwrap(),
        Timestamp::try_from(NOW).unwrap(),
    )
    .unwrap();
    m.stage_acquire(
        &m.orch
            .ledger
            .pool::<Ex>(&ResourceKey::new(
                ClassId::new("scratch"),
                InstanceId::new("s2"),
            ))
            .unwrap(),
        Generation::new("g"),
        Claim::new(Ex::Token).unwrap(),
        Timestamp::try_from(NOW).unwrap(),
    )
    .unwrap();
    assert_eq!(*m.run(NOW), MonState::Done(MonOutcome::Completed));
    assert_eq!(
        m.orch
            .ledger
            .live_snapshot(&SubjectId::new(&m.seg_subject()))
            .len(),
        0
    );
    assert_eq!(
        m.orch.ledger.live_snapshot(&SubjectId::new("fib")).len(),
        2,
        "commit：全部归 fiber"
    );
    assert!(
        m.orch.world.effects_fingerprint().0.is_empty(),
        "commit 不动世界"
    );
    assert!(m.trace.contains(&Ev::SegmentCommitted { holdings: 2 }));
    m.orch.ledger.invariant().unwrap();

    // 截断分支：Truncated 是非 commit 终态——前缀交付，段内持有照样自动回滚。
    let mut m = Monitor::new(base_policy(), Mode::Truncate, scratch(), "fib");
    m.admit(
        vec![
            a("post", "api.example.com", "1"),
            a("post", "api.example.com", "2"),
        ],
        H,
        consent("n1", 1),
        NOW,
    )
    .unwrap();
    m.stage_acquire(
        &m.orch
            .ledger
            .pool::<Ex>(&ResourceKey::new(
                ClassId::new("scratch"),
                InstanceId::new("s1"),
            ))
            .unwrap(),
        Generation::new("g"),
        Claim::new(Ex::Token).unwrap(),
        Timestamp::try_from(NOW).unwrap(),
    )
    .unwrap();
    assert_eq!(
        *m.run(NOW),
        MonState::Done(MonOutcome::Truncated { dropped: 1 })
    );
    assert_eq!(m.world.emitted.len(), 1, "前缀站着不动");
    assert_eq!(
        m.orch
            .ledger
            .live_snapshot(&SubjectId::new(&m.seg_subject()))
            .len(),
        0
    );
    assert_eq!(
        m.orch.world.effects_fingerprint().0.len(),
        1,
        "截断也回滚段内持有"
    );
}

/// [LOUD] truncate 绝不静默：截断量显式入 trace 与结果；
/// 与 strict 的结局类型不同（读/效应不对称：读自由故可截断，效应受控故宁停不越）。
#[test]
fn truncate_never_silent() {
    let plan: Vec<WAction> = (1..=5)
        .map(|i| a("post", "api.example.com", &i.to_string()))
        .collect();

    let mut t = Monitor::new(base_policy(), Mode::Truncate, Ledger::new(), "fib");
    t.admit(plan.clone(), H, consent("n1", 2), NOW).unwrap();
    assert_eq!(
        *t.run(NOW),
        MonState::Done(MonOutcome::Truncated { dropped: 3 })
    );
    assert_eq!(t.world.emitted, plan[..2].to_vec());
    assert!(
        t.trace.contains(&Ev::Truncation { dropped: 3 }),
        "截断事件必须可听见"
    );
    assert!(t.trace.contains(&Ev::BudgetExhausted { at: 2 }));

    let mut s = Monitor::new(base_policy(), Mode::Strict, Ledger::new(), "fib");
    s.admit(plan.clone(), H, consent("n1", 2), NOW).unwrap();
    assert_eq!(
        *s.run(NOW),
        MonState::Done(MonOutcome::FailStop { at: 2 }),
        "同界不同结局"
    );
    assert_eq!(s.world.emitted, t.world.emitted, "两种模式交付同一前缀");
}
