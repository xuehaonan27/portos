//! F6 法则测试（协议次序）—— RDMA 的 QP 状态机把 E 里的"协议次序"顶成承重。
//! 三件事各有一条法则：协议是 safety（前缀闭、不可救）；静态可达集＝路径穷举；
//! 扣发重排下的世界序投影 sound（无分支时精确）；以及 F3 截停器对它的精确执行。

use portos_rm::coeffect::Plan;
use portos_rm::ledger::Ledger;
use portos_rm::monitor::*;
use portos_rm::protocol::*;

/// QP 状态机：reset→init→rtr→rts；post_send 只在 rts；query 不在辖域。
fn qp() -> Protocol {
    Protocol::new("reset")
        .transition("reset", "init", "init")
        .transition("init", "rtr", "rtr")
        .transition("rtr", "rts", "rts")
        .transition("rts", "post_send", "rts")
}

fn all_sequences(alphabet: &[&str], max_len: usize) -> Vec<Vec<String>> {
    let mut out = vec![Vec::new()];
    let mut layer = vec![Vec::<String>::new()];
    for _ in 0..max_len {
        let mut next = Vec::new();
        for s in &layer {
            for v in alphabet {
                let mut t = s.clone();
                t.push(v.to_string());
                next.push(t);
            }
        }
        out.extend(next.iter().cloned());
        layer = next;
    }
    out
}

/// [SAFE] 协议是 safety 性质（Schneider／IJIS 2005 的定义做成可运行）：
/// 合法序列的每个前缀合法；一旦违规，任何延展都违规且违规位置不变（不可救）。
/// 5 元字母表长 ≤4 的全部 781 条序列。
#[test]
fn protocol_is_a_safety_property_prefix_closed_and_irremediable() {
    let p = qp();
    let alpha = ["init", "rtr", "rts", "post_send", "query"];
    let seqs = all_sequences(&alpha, 4);
    assert_eq!(seqs.len(), 1 + 5 + 25 + 125 + 625);
    for s in &seqs {
        let refs: Vec<&str> = s.iter().map(String::as_str).collect();
        match p.check_sequence(&refs) {
            Ok(_) => {
                for k in 0..refs.len() {
                    assert!(p.check_sequence(&refs[..k]).is_ok(), "前缀闭失效：{refs:?} 的前缀 {k}");
                }
            }
            Err(v) => {
                let at = v.at.unwrap();
                for ext in &alpha {
                    let mut e = refs.clone();
                    e.push(ext);
                    let again = p.check_sequence(&e).unwrap_err();
                    assert_eq!(again.at, Some(at), "违规应不可救且位置稳定：{e:?}");
                }
            }
        }
    }
    // 辖域外动词不改状态：query 可在任何时候查。
    assert!(p.check_sequence(&["query", "init", "query", "rtr", "rts", "query", "post_send"]).is_ok());
}

fn plans_up_to_depth(leaves: &[&str], depth: u32) -> Vec<Plan> {
    let mut all: Vec<Plan> = leaves.iter().map(|v| Plan::verb("qp", v)).collect();
    for _ in 0..depth {
        let mut next = Vec::new();
        for p in &all {
            for q in &all {
                next.push(Plan::Seq(vec![p.clone(), q.clone()]));
                next.push(Plan::branch(p.clone(), q.clone()));
            }
            for bound in 0..=3u64 {
                next.push(Plan::loop_(bound, p.clone()));
            }
        }
        all.extend(next);
    }
    all
}

/// [STATIC] 状态集语义 ＝ 路径穷举（决策 #8 风格）：深度 ≤2 的**全部** 5668 个计划形状，
/// `check_plan` 的判定与"展开全部路径（循环 0..=N 轮、分支两取）逐条判定"逐个一致，
/// 且可达集 ＝ 合法路径终态集。
#[test]
fn static_reachability_equals_path_enumeration_on_all_small_plans() {
    let p = qp();
    let plans = plans_up_to_depth(&["init", "rtr", "rts", "post_send"], 2);
    assert_eq!(plans.len(), 4 + (16 + 16 + 16) + (52 * 52 * 2 + 52 * 4), "计划枚举规模被收缩");
    let mut ok_count = 0;
    for plan in &plans {
        let paths = enumerate_paths(plan);
        let mut ends = std::collections::BTreeSet::new();
        let mut all_ok = true;
        for path in &paths {
            let refs: Vec<&str> = path.iter().map(String::as_str).collect();
            match p.check_sequence(&refs) {
                Ok(end) => {
                    ends.insert(end);
                }
                Err(_) => all_ok = false,
            }
        }
        match p.check_plan(plan) {
            Ok(reach) => {
                assert!(all_ok, "静态判过而某路径违规：{plan:?}");
                assert_eq!(reach, ends, "可达集 ≠ 路径终态集：{plan:?}");
                ok_count += 1;
            }
            Err(_) => assert!(!all_ok, "静态判违规而全部路径合法：{plan:?}"),
        }
    }
    assert!(ok_count > 0 && ok_count < plans.len(), "两类结局都须被见证");
}

/// [REORDER] 扣发重排世界序：计划序合法 ≠ 世界序合法。
/// 世界序投影对无分支计划**精确**、有分支计划 **sound**（静态过 ⇒ 全部世界序路径合法）。
#[test]
fn world_order_projection_is_sound_and_exact_without_branches() {
    // 协议：先授远端访问（硬清单，扣发）再 post_send。
    let p = Protocol::new("closed")
        .transition("closed", "grant", "open")
        .transition("open", "post_send", "open");
    let withhold: std::collections::BTreeSet<String> = ["grant".to_string()].into_iter().collect();

    // 最小反例：计划序 grant;post_send 合法；世界序 post_send;grant（grant 殿后）违规。
    let plan = Plan::Seq(vec![Plan::verb("mr", "grant"), Plan::verb("qp", "post_send")]);
    assert!(p.check_plan(&plan).is_ok(), "计划序合法");
    assert!(p.check_plan_world_order(&plan, &withhold).is_err(), "世界序违规——扣发把 grant 推到了 post_send 之后");

    let world_order = |path: &[String]| -> Vec<String> {
        let mut w: Vec<String> = path.iter().filter(|v| !withhold.contains(*v)).cloned().collect();
        w.extend(path.iter().filter(|v| withhold.contains(*v)).cloned());
        w
    };
    let plans = plans_up_to_depth(&["grant", "post_send", "query"], 2);
    let mut exact_checked = 0;
    let mut sound_checked = 0;
    for plan in &plans {
        let brute_all_ok = enumerate_paths(plan).iter().all(|path| {
            let w = world_order(path);
            let refs: Vec<&str> = w.iter().map(String::as_str).collect();
            p.check_sequence(&refs).is_ok()
        });
        let static_ok = p.check_plan_world_order(plan, &withhold).is_ok();
        if has_branch(plan) {
            if static_ok {
                assert!(brute_all_ok, "soundness 失效（有分支）：{plan:?}");
            }
            sound_checked += 1;
        } else {
            assert_eq!(static_ok, brute_all_ok, "无分支应精确：{plan:?}");
            exact_checked += 1;
        }
    }
    assert!(exact_checked > 0 && sound_checked > 0);
}

fn has_branch(p: &Plan) -> bool {
    match p {
        Plan::Verb { .. } => false,
        Plan::Seq(items) => items.iter().any(has_branch),
        Plan::Loop { body, .. } => has_branch(body),
        Plan::Branch(..) => true,
    }
}

fn consent(n: &str, b: u64) -> Consent {
    Consent { plan_hash: "h".into(), budget: b, nonce: n.into(), ttl_expires_at: 100 }
}

/// [SAFE]→F3：协议由截停器精确执行——合法流原样通过；违规流在首个违规处 fail-stop、
/// 交付最长合法前缀、trace 点名违规动词与状态；与静态检查判定一致。
#[test]
fn protocol_precisely_enforced_by_truncation_tier_in_monitor() {
    let mut pol = Policy::default();
    for v in ["init", "rtr", "rts", "post_send"] {
        pol.allow(v, "qp-1");
    }
    pol.protocol = Some(qp());
    pol.handler = "qp".into();

    let valid = vec![WAction::new("init", "qp-1", "", 1), WAction::new("rtr", "qp-1", "", 1), WAction::new("rts", "qp-1", "", 1), WAction::new("post_send", "qp-1", "", 1)];
    let mut m = Monitor::new(pol.clone(), Mode::Strict, Ledger::new(), "fib");
    m.admit(valid.clone(), "h", consent("n1", 10), 10).unwrap();
    assert_eq!(*m.run(10), MonState::Done(MonOutcome::Completed));
    assert_eq!(m.world.emitted, valid, "合法流原样通过（透明性）");

    let invalid = vec![WAction::new("init", "qp-1", "", 1), WAction::new("post_send", "qp-1", "", 1), WAction::new("rtr", "qp-1", "", 1)];
    let mut m = Monitor::new(pol, Mode::Strict, Ledger::new(), "fib");
    m.admit(invalid.clone(), "h", consent("n1", 10), 10).unwrap();
    assert_eq!(*m.run(10), MonState::Done(MonOutcome::FailStop { at: 1 }));
    assert_eq!(m.world.emitted, invalid[..1].to_vec(), "最长合法前缀");
    assert!(m.trace.contains(&Ev::ProtocolViolation { verb: "post_send".into(), state: "init".into(), at: 1 }));
    // 与静态检查一致
    let plan = Plan::Seq(invalid.iter().map(|a| Plan::verb("qp", &a.verb)).collect());
    assert!(qp().check_plan(&plan).is_err());
}

/// [REORDER]→F3：运行期兜底的两个位置——即时动词在改变了的状态下无转移 ⇒ step 处 fail-stop；
/// 批准整批在当前状态下无转移 ⇒ approve 整批拒（事务形状），段留待过期废弃。
#[test]
fn withhold_reorder_is_caught_at_step_or_at_approval() {
    // 情形一：grant（扣发）在后放、post_send 即时——post_send 在 closed 状态无转移 ⇒ step 处 fail-stop。
    let p = Protocol::new("closed")
        .transition("closed", "grant", "open")
        .transition("open", "post_send", "open");
    let mut pol = Policy::default();
    pol.allow("grant", "buf");
    pol.allow("post_send", "peer");
    pol.staged_verbs.insert("grant".into());
    pol.protocol = Some(p);
    let mut m = Monitor::new(pol, Mode::Strict, Ledger::new(), "fib");
    m.admit(vec![WAction::new("grant", "buf", "", 1), WAction::new("post_send", "peer", "", 1)], "h", consent("n1", 10), 10).unwrap();
    assert_eq!(*m.run(10), MonState::Done(MonOutcome::FailStop { at: 1 }), "世界序违规在 step 处被截停");
    assert!(m.world.emitted.is_empty(), "grant 在缓冲、post_send 被截：世界零发射");
    assert!(m.trace.iter().any(|e| matches!(e, Ev::SegmentAborted { expired: false })), "终止即弃：缓冲里的 grant 可听见地废弃");

    // 情形二：即时动词把状态推到终态，批准整批在终态无转移 ⇒ approve 整批拒。
    // 协议：s0 -grant-> s1 -close-> closed；s0 -close-> closed。计划序 grant;close 合法；
    // 世界序 close;grant——close 即时（s0→closed 合法），grant 在批准时从 closed 出发无转移。
    let p2 = Protocol::new("s0")
        .transition("s0", "grant", "s1")
        .transition("s1", "close", "closed")
        .transition("s0", "close", "closed");
    let mut pol = Policy::default();
    pol.allow("grant", "buf");
    pol.allow("close", "buf");
    pol.staged_verbs.insert("grant".into());
    pol.protocol = Some(p2.clone());
    let mut m = Monitor::new(pol, Mode::Strict, Ledger::new(), "fib");
    let plan = vec![WAction::new("grant", "buf", "", 1), WAction::new("close", "buf", "", 1)];
    m.admit(plan.clone(), "h", consent("n1", 10), 10).unwrap();
    assert_eq!(*m.run(10), MonState::AwaitingApproval);
    assert_eq!(m.approve(consent("n2", 5), 10), Err(Refusal::ProtocolViolation), "批准整批违规 ⇒ 整批拒");
    assert_eq!(m.world.emitted.len(), 1, "只有 close 在世界里；grant 一件未漏");
    m.expire(101).unwrap(); // 段只能废
    assert_eq!(*m.state(), MonState::Done(MonOutcome::Aborted { expired: true }));
    // 静态世界序检查应在准入期就预言这一点。
    let static_plan = Plan::Seq(plan.iter().map(|a| Plan::verb("mr", &a.verb)).collect());
    let withhold: std::collections::BTreeSet<String> = ["grant".to_string()].into_iter().collect();
    assert!(p2.check_plan(&static_plan).is_ok(), "计划序合法");
    assert!(p2.check_plan_world_order(&static_plan, &withhold).is_err(), "世界序违规——准入期即可拦");
}
