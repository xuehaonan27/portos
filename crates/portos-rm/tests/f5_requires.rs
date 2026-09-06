//! F5 法则测试 —— 每个测试名 = 它执行的定理/纪律（对照 freeze-f5 三列表）。
//!
//! 方法（决策 #8）：能穷举的都穷举——Definition 1 的法则在两个 scalar 实例与固定键集向量上
//! 逐三元组验；预算向量的半模律在小映射×小标量上全验；B̂ ≥ B 在深度 ≤2 的**全部**计划形状
//! 上验；effect row 的交集律在 3 元宇宙的全部子集三元组上验。
//! 措辞纪律：合同内的法则（Definition 1）与实例附加性质（交换、单调、join）分开断言、分开标注，
//! 后者的失败不算合同失败——但它们是我们实例的真实性质，记录在案供实装依赖时查证。

use portos_rm::auth::{auth_valid, can_mint};
use portos_rm::coeffect::*;
use portos_rm::ra::{Count, Ra};
use portos_rm::verbs::*;

// ---------------------------------------------------------------------------
// 载体枚举（确定性）：counting 取 0..=5；flat 取 3 元宇宙全部子集；
// 固定键集 K={p,q} 的向量取 0..=2 × 0..=2（它是 Counting²，逐分量 Definition 1）。
// ---------------------------------------------------------------------------
fn counting_carrier() -> Vec<Counting> {
    (0..=5).map(Counting).collect()
}
fn flat_carrier(universe: &[&str]) -> Vec<Flat> {
    let n = universe.len();
    (0..(1u32 << n))
        .map(|mask| Flat((0..n).filter(|i| mask & (1 << i) != 0).map(|i| universe[i].to_string()).collect()))
        .collect()
}

/// 固定 K 的向量 Counting²：逐分量运算——用来演示"对固定效应类集合，预算向量就是一个 scalar"。
/// 只在测试里存在：库里的 `Budget` 因 K 未知不声称 scalar（K 生长时 use＝处处 1 无有限表示）。
#[derive(Clone, PartialEq, Eq, Debug)]
struct Fixed2(Counting, Counting);
impl Scalar for Fixed2 {
    fn seq(&self, o: &Self) -> Self {
        Fixed2(self.0.seq(&o.0), self.1.seq(&o.1))
    }
    fn merge(&self, o: &Self) -> Self {
        Fixed2(self.0.merge(&o.0), self.1.merge(&o.1))
    }
    fn use_() -> Self {
        Fixed2(Counting::use_(), Counting::use_())
    }
    fn ign() -> Self {
        Fixed2(Counting::ign(), Counting::ign())
    }
    fn leq(&self, o: &Self) -> bool {
        self.0.leq(&o.0) && self.1.leq(&o.1)
    }
}
impl Join for Fixed2 {
    fn join(&self, o: &Self) -> Self {
        Fixed2(self.0.join(&o.0), self.1.join(&o.1))
    }
}
fn fixed2_carrier() -> Vec<Fixed2> {
    let mut v = Vec::new();
    for a in 0..=2u64 {
        for b in 0..=2u64 {
            v.push(Fixed2(Counting(a), Counting(b)));
        }
    }
    v
}
/// 有限支撑的预算向量小载体：键 {p,q}，值 0..=2（0 即缺席——规范化后同一表示）。
fn budget_carrier() -> Vec<Budget> {
    let mut v = Vec::new();
    for a in 0..=2u64 {
        for b in 0..=2u64 {
            v.push(Budget::of(&[("p", a), ("q", b)]));
        }
    }
    v
}

/// [DEF1] Definition 1 的全部法则，且**只有**这些：两个幺半群、预序、双侧分配律。
/// 逐三元组穷举，返回检查过的三元组数（供规模断言，防枚举空转）。
fn check_definition1<S: Scalar>(elems: &[S]) -> usize {
    let use_ = S::use_();
    let ign = S::ign();
    let mut triples = 0;
    for r in elems {
        assert_eq!(r.seq(&use_), *r, "use 不是 ~ 的右单位");
        assert_eq!(use_.seq(r), *r, "use 不是 ~ 的左单位");
        assert_eq!(r.merge(&ign), *r, "ign 不是 ⊕ 的右单位");
        assert_eq!(ign.merge(r), *r, "ign 不是 ⊕ 的左单位");
        assert!(r.leq(r), "≤ 不自反");
        for s in elems {
            for t in elems {
                triples += 1;
                assert_eq!(r.seq(s).seq(t), r.seq(&s.seq(t)), "~ 不结合");
                assert_eq!(r.merge(s).merge(t), r.merge(&s.merge(t)), "⊕ 不结合");
                // 双侧分配律，原文原形
                assert_eq!(r.merge(s).seq(t), r.seq(t).merge(&s.seq(t)), "(r⊕s)~t ≠ (r~t)⊕(s~t)");
                assert_eq!(t.seq(&r.merge(s)), t.seq(r).merge(&t.seq(s)), "t~(r⊕s) ≠ (t~r)⊕(t~s)");
                if r.leq(s) && s.leq(t) {
                    assert!(r.leq(t), "≤ 不传递");
                }
            }
        }
    }
    triples
}

/// 实例附加性质（**不在 Definition 1 内**——原文明言不要求交换律；算子单调性亦未入定义）：
/// 交换、算子对 ≤ 单调、join 是最小上界。我们的实例都满足——记录在案，但合同不依赖。
fn check_instance_extras<S: Join>(elems: &[S]) {
    for r in elems {
        for s in elems {
            assert_eq!(r.seq(s), s.seq(r), "实例性质：~ 交换");
            assert_eq!(r.merge(s), s.merge(r), "实例性质：⊕ 交换");
            let j = r.join(s);
            assert!(r.leq(&j) && s.leq(&j), "join 不是上界");
            for t in elems {
                if r.leq(s) {
                    assert!(r.seq(t).leq(&s.seq(t)) && t.seq(r).leq(&t.seq(s)), "实例性质：~ 对 ≤ 单调");
                    assert!(r.merge(t).leq(&s.merge(t)) && t.merge(r).leq(&t.merge(s)), "实例性质：⊕ 对 ≤ 单调");
                }
                if r.leq(t) && s.leq(t) {
                    assert!(j.leq(t), "join 不是最小上界");
                }
            }
        }
    }
}

/// [DEF1]+[INST] 两个 scalar 实例＋固定键集向量上穷举 Definition 1；附加性质另验另标。
#[test]
fn definition1_laws_exhaustive_on_counting_flat_and_fixed_vector() {
    let c = counting_carrier();
    let f = flat_carrier(&["a", "b", "c"]);
    let x = fixed2_carrier();
    assert_eq!(check_definition1(&c), 6 * 6 * 6);
    assert_eq!(check_definition1(&f), 8 * 8 * 8);
    assert_eq!(check_definition1(&x), 9 * 9 * 9, "固定 K 的 Counting² 逐分量满足 Definition 1");
    check_instance_extras(&c);
    check_instance_extras(&f);
    check_instance_extras(&x);
}

/// [VEC] 有限支撑预算向量＝ℕ 上的半模：⊕ 幺半群（零向量为单位）、标量缩放对 ⊕ 分配、
/// 标量加法/乘法与缩放相容、1·a=a、0·a=0；≤ 逐分量预序；join 逐分量最小上界；
/// 缺席＝显式 0（规范化）。这些恰是计划求值依赖的全部运算律。
#[test]
fn budget_vector_is_a_semimodule_over_counting() {
    let bs = budget_carrier();
    let zero = Budget::zero();
    for a in &bs {
        assert_eq!(a.merge(&zero), *a);
        assert_eq!(zero.merge(a), *a);
        assert_eq!(Budget::scale(1, a), *a, "1·a = a");
        assert_eq!(Budget::scale(0, a), zero, "0·a = 0（规范化后与零向量同一表示）");
        assert!(a.leq(a));
        for b in &bs {
            assert_eq!(a.merge(b), b.merge(a), "⊕ 交换（实例性质）");
            let j = a.join(b);
            assert!(a.leq(&j) && b.leq(&j));
            for c in &bs {
                assert_eq!(a.merge(b).merge(c), a.merge(&b.merge(c)), "⊕ 结合");
                if a.leq(b) && b.leq(c) {
                    assert!(a.leq(c));
                }
                if a.leq(c) && b.leq(c) {
                    assert!(j.leq(c), "join 是最小上界");
                }
            }
            for n in 0..=3u64 {
                assert_eq!(Budget::scale(n, &a.merge(b)), Budget::scale(n, a).merge(&Budget::scale(n, b)), "n·(a⊕b)=n·a⊕n·b");
                for m in 0..=3u64 {
                    assert_eq!(Budget::scale(n + m, a), Budget::scale(n, a).merge(&Budget::scale(m, a)), "(n+m)·a = n·a ⊕ m·a");
                    assert_eq!(Budget::scale(n * m, a), Budget::scale(n, &Budget::scale(m, a)), "(nm)·a = n·(m·a)");
                }
            }
        }
    }
    // 缺席与 0 同一：{p:0,q:1} 与 {q:1} 相等，且 leq 两向皆成立。
    let explicit = Budget([("p".to_string(), Counting(0)), ("q".to_string(), Counting(1))].into_iter().collect());
    let sparse = Budget::of(&[("q", 1)]);
    assert!(explicit.leq(&sparse) && sparse.leq(&explicit));
    assert_eq!(explicit.merge(&Budget::zero()), sparse, "经运算规范化后表示同一");

    // [PROD] requires 的 ≤ 逐分量（走查风险点：不许是字典序）：一个分量超了就不算盖住。
    let lo = Requires { caps: Flat::of(&["a"]), deps: Flat::empty(), uses: Budget::of(&[("k", 5)]) };
    let hi = Requires { caps: Flat::of(&["a", "b"]), deps: Flat::empty(), uses: Budget::of(&[("k", 2)]) };
    assert!(!lo.leq(&hi), "caps 盖住但 uses 超出 ⇒ 不 ≤");
    assert!(!hi.leq(&lo), "uses 盖住但 caps 超出 ⇒ 不 ≤");
    assert!(lo.leq(&Requires { caps: Flat::of(&["a", "b"]), deps: Flat::empty(), uses: Budget::of(&[("k", 5)]) }));
}

/// [APP] 循环缩放＝application 规则：scale(N, body) = numeral(N) ~ body。
/// counting：数字 N、嵌套相乘（~ 结合律）、且等于 N 个 body 的 ⊕（分配律后果）；
/// flat：numeral 恒为 ∅，缩放恒等（只记"哪些"不记"几次"；界 0 亦保守保留）；
/// 向量：逐类乘 N。
#[test]
fn loop_scaling_is_application_rule_numeral_seq_body() {
    for n in 0..=5u64 {
        assert_eq!(Counting::numeral(n), Counting(n), "counting 的 numeral 就是数字");
        assert_eq!(Flat::numeral(n), Flat::empty(), "flat 的 numeral 恒为 ∅");
        for body in 0..=4u64 {
            let b = Counting(body);
            assert_eq!(Counting::scale(n, &b), Counting(n * body));
            let mut rep = Counting::ign();
            for _ in 0..n {
                rep = rep.merge(&b);
            }
            assert_eq!(Counting::scale(n, &b), rep, "缩放 ≠ N 次合并——分配律被破坏");
            for m in 0..=3u64 {
                assert_eq!(Counting::scale(n, &Counting::scale(m, &b)), Counting(n * m * body), "嵌套循环界相乘");
            }
        }
    }
    let s = Flat::of(&["net"]);
    for n in 0..=3 {
        assert_eq!(Flat::scale(n, &s), s, "flat 缩放恒等（含界 0 的保守方向）");
    }
    // requires 上逐分量：向量逐类乘、caps/deps 不变。
    let r = Requires { caps: Flat::of(&["x"]), deps: Flat::of(&["svc"]), uses: Budget::of(&[("h::a", 2), ("h::b", 1)]) };
    let scaled = Requires::scale(3, &r);
    assert_eq!(scaled.caps, Flat::of(&["x"]));
    assert_eq!(scaled.deps, Flat::of(&["svc"]));
    assert_eq!(scaled.uses, Budget::of(&[("h::a", 6), ("h::b", 3)]));
}

// ---------------------------------------------------------------------------
// 计划形状的确定性枚举：深度 ≤2、三个动词原型、四种循环界。
// 3（深 0）＋ 30（深 1）＋ 2310（深 2）＝ 2343 个计划，全部验 B̂ ≥ B。
// ---------------------------------------------------------------------------
fn alphabet_lookup(handler: &str, verb: &str) -> Requires {
    match (handler, verb) {
        ("h", "a") => Requires::of(&["x"], &[], Some("h::a")), // 进预算（类 h.a）、需 x
        ("h", "b") => Requires::of(&["y"], &[], Some("h::b")), // 进预算（类 h.b）、需 y
        ("h", "c") => Requires::of(&[], &[], None),          // 可重复：零预算、零权能
        _ => unreachable!(),
    }
}
fn plans_up_to_depth(depth: u32) -> Vec<Plan> {
    let leaves: Vec<Plan> = ["a", "b", "c"].iter().map(|v| Plan::verb("h", v)).collect();
    let mut all = leaves;
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

/// [BHAT] effect-plan §5.2：B̂（全部出现之和）≥ B（路径极大），逐分量；
/// flat 分量恒等（join＝⊕）；向量分量存在严格大于的见证（分支）。
#[test]
fn occurrence_sum_overapproximates_path_max_on_all_small_plans() {
    let plans = plans_up_to_depth(2);
    assert_eq!(plans.len(), 3 + 30 + 2310, "计划枚举规模被收缩");
    let mut strict = 0;
    for p in &plans {
        let b_hat = demand_sum(p, &alphabet_lookup);
        let b = demand_paths(p, &alphabet_lookup);
        assert!(b.leq(&b_hat), "B̂ 未盖住 B：{p:?} → B̂={b_hat:?} B={b:?}");
        assert_eq!(b.caps, b_hat.caps, "flat 分量 B̂ 应恒等于 B（join＝∪＝⊕）");
        assert_eq!(b.deps, b_hat.deps);
        if b.uses != b_hat.uses {
            strict += 1;
        }
    }
    assert!(strict > 0, "无一严格见证——分支过近似未被走到");
    // 最小严格见证：branch(a, a)——路径极大 {h.a:1}，出现之和 {h.a:2}。
    let w = Plan::branch(Plan::verb("h", "a"), Plan::verb("h", "a"));
    assert_eq!(demand_paths(&w, &alphabet_lookup).uses, Budget::of(&[("h::a", 1)]));
    assert_eq!(demand_sum(&w, &alphabet_lookup).uses, Budget::of(&[("h::a", 2)]));
    // 不同类的分支：逐类 join 与逐类 ⊕ 恰好相同（各类只出现一次）——过近似只发生在同类重复处。
    let w2 = Plan::branch(Plan::verb("h", "a"), Plan::verb("h", "b"));
    assert_eq!(demand_paths(&w2, &alphabet_lookup).uses, demand_sum(&w2, &alphabet_lookup).uses);
}

/// [ROW] effect row：实际可用＝位置天花板 ∩ 主体天花板；
/// requires ≤ offers∩grant ⟺ requires ≤ offers ∧ requires ≤ grant（3 元宇宙全部三元组）。
/// 再钉一颗"混淆位置"钉子：主体持有 net，挂载点不 offers net ⇒ 用不出来。
#[test]
fn effect_row_is_position_ceiling_meet_subject_ceiling() {
    let u = flat_carrier(&["a", "b", "c"]);
    let mut checked = 0;
    for req in &u {
        for offers in &u {
            for grant in &u {
                let lhs = req.leq(&ceiling(offers, grant));
                let rhs = req.leq(offers) && req.leq(grant);
                assert_eq!(lhs, rhs, "交集律失效：req={req:?} offers={offers:?} grant={grant:?}");
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 512);

    let grant = Flat::of(&["net", "read_own"]);
    let offers = Flat::of(&["read_own"]);
    let lookup = |_: &str, _: &str| Requires::of(&["net"], &[], Some("cb::fetch"));
    let plan = Plan::verb("cb", "fetch");
    let b = Budget::of(&[("cb::fetch", 10)]);
    let r = admit_plan(&plan, &lookup, &ceiling(&offers, &grant), &Flat::empty(), &b);
    assert_eq!(r, Err(AdmitError::ExceedsCeiling { missing: Flat::of(&["net"]) }), "位置决定天花板");
    let ok = admit_plan(&plan, &lookup, &ceiling(&Flat::of(&["net", "read_own"]), &grant), &Flat::empty(), &b);
    assert!(ok.is_ok(), "同一主体同一动词，挂到 offers 含 net 的位置就过");
}

/// [DOWN] 同意＝↓B（逐类下集）；单调性引理（effect-plan §5.4）：B′ ≤ B ⇒ ↓B′ ⊆ ↓B。
/// 用途：standing 预算收窄永远无需重新确认；库更新只要不放宽就免确认。
#[test]
fn consent_monotonicity_downset() {
    let bs = budget_carrier();
    let mut checked = 0;
    for d in &bs {
        for b_lo in &bs {
            for b_hi in &bs {
                if b_lo.leq(b_hi) && d.leq(b_lo) {
                    assert!(d.leq(b_hi), "↓{b_lo:?} ⊄ ↓{b_hi:?}");
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 0);
    // 以 admit_plan 形态重述：在 B′ 下准入 ⇒ 在任何 B ≥ B′ 下准入。
    let lookup = |_: &str, _: &str| Requires::of(&["x"], &[], Some("h::a"));
    let plan = Plan::loop_(3, Plan::verb("h", "a"));
    let ceil = Flat::of(&["x"]);
    assert!(admit_plan(&plan, &lookup, &ceil, &Flat::empty(), &Budget::of(&[("h::a", 3)])).is_ok());
    assert!(admit_plan(&plan, &lookup, &ceil, &Flat::empty(), &Budget::of(&[("h::a", 7), ("h::b", 1)])).is_ok());
    assert_eq!(
        admit_plan(&plan, &lookup, &ceil, &Flat::empty(), &Budget::of(&[("h::a", 2)])),
        Err(AdmitError::OverBudget { class: "h::a".into(), demand: 3, budget: 2 })
    );
}

/// [VEC] B9 墓碑：预算按效应类逐类比较，**不看总量**。同意 {click ≤ 4, send ≤ 1}（总 5）下，
/// 5 次 click（总 5）必须被拒——单计数实现会放过它。同意面逐类渲染（m0 "echo.emit ≤ 4"），
/// 准入就必须逐类判定；总量只是展示数字。
#[test]
fn budget_is_per_effect_class_not_a_total() {
    let lookup = |_: &str, v: &str| Requires::of(&["ui"], &[], Some(&format!("page::{v}")));
    let consent = Budget::of(&[("page::click", 4), ("page::send", 1)]);
    let ceil = Flat::of(&["ui"]);
    let five_clicks = Plan::loop_(5, Plan::verb("page", "click"));
    let d = demand_sum(&five_clicks, &lookup);
    assert_eq!(d.uses.total(), consent.total(), "总量恰好相等——单计数会误判为在预算内");
    assert_eq!(
        admit_plan(&five_clicks, &lookup, &ceil, &Flat::empty(), &consent),
        Err(AdmitError::OverBudget { class: "page::click".into(), demand: 5, budget: 4 }),
        "逐类判定：click 超了就是超了，send 的余额救不了它"
    );
    let four_and_one = Plan::Seq(vec![Plan::loop_(4, Plan::verb("page", "click")), Plan::verb("page", "send")]);
    assert!(admit_plan(&four_and_one, &lookup, &ceil, &Flat::empty(), &consent).is_ok());
    // 未在同意里出现的效应类＝上界 0：任何一次都越界（缺席不是"无限"，是"零"）。
    let stray = Plan::verb("page", "delete");
    assert_eq!(
        admit_plan(&stray, &lookup, &ceil, &Flat::empty(), &consent),
        Err(AdmitError::OverBudget { class: "page::delete".into(), demand: 1, budget: 0 })
    );
}

/// [F4] 动词是否进预算由真理表决定：可重复动词不记类。
/// 先读后谋（effect-plan §6.2）的类型层依据：读循环一千次不抬高同意面上的任何数字。
#[test]
fn repeatable_verbs_cost_zero_via_truth_table() {
    let mut t = VerbTable::new();
    t.register("page", "snapshot", VerbEntry::repeatable()).unwrap();
    t.register("page", "click", VerbEntry::emitting(EmitGrade::External, true)).unwrap();
    let lookup = |h: &str, v: &str| -> Requires {
        let caps: &[&str] = if v == "click" { &["input"] } else { &["dom.read"] };
        Requires::from_table(&t, h, v, caps, &[]).unwrap()
    };
    let reads = Plan::loop_(1000, Plan::verb("page", "snapshot"));
    let clicks = Plan::loop_(1000, Plan::verb("page", "click"));
    assert_eq!(demand_sum(&reads, &lookup).uses, Budget::zero(), "可重复读循环一千次：不占任何类");
    assert_eq!(demand_sum(&clicks, &lookup).uses, Budget::of(&[("page::click", 1000)]));
    assert_eq!(demand_sum(&reads, &lookup).caps, Flat::of(&["dom.read"]), "权能仍然要：flat 不因不计数而消失");
    // 先读后谋：先 1000 次观察再点 3 次，同意面上只有 "page.click ≤ 3"。
    let plan = Plan::Seq(vec![reads.clone(), Plan::loop_(3, Plan::verb("page", "click"))]);
    let ceil = Flat::of(&["dom.read", "input"]);
    let d = admit_plan(&plan, &lookup, &ceil, &Flat::empty(), &Budget::of(&[("page::click", 3)])).unwrap();
    assert_eq!(d.uses, Budget::of(&[("page::click", 3)]));
    assert!(admit_plan(&reads, &lookup, &ceil, &Flat::empty(), &Budget::zero()).is_ok(), "零预算也能读");
    assert_eq!(
        admit_plan(&clicks, &lookup, &ceil, &Flat::empty(), &Budget::zero()),
        Err(AdmitError::OverBudget { class: "page::click".into(), demand: 1000, budget: 0 })
    );
}

/// [TWO] 预算半环与 F1 `Count` RA 是同一幺半群的两读（theory-spec §3.2）：
/// ⊕ 逐点＝`Count::op`；≤ 逐点＝`≼`；"某类 demand ∈ ↓B" 逐点＝`auth_valid(● B, ◯ demand)`；
/// 且 F3 的花费闸门 `can_mint(● B, 已花, 再花)` ＝ 本模块对累计需求的 ↓B 判定——同一个谓词，
/// 逐效应类各自成立（每类一个池，正是 F3 per-nonce 池的逐类形态）。
#[test]
fn budget_merge_is_count_ra_op_and_downset_is_auth_valid() {
    for n in 0..=6u64 {
        for m in 0..=6u64 {
            assert_eq!(Counting(n).merge(&Counting(m)).0, Count(n).op(&Count(m)).0, "⊕ ≠ Count::op");
            assert_eq!(Counting(n).leq(&Counting(m)), Count(n).included_in(&Count(m)), "≤ ≠ ≼");
            let demand = Budget::of(&[("k", n)]);
            let bound = Budget::of(&[("k", m)]);
            assert_eq!(demand.leq(&bound), auth_valid(&Count(m), &Some(Count(n))), "↓B ≠ auth_valid");
        }
    }
    for b in 0..=6u64 {
        for spent in 0..=6u64 {
            for cost in 0..=3u64 {
                let gate = can_mint(&Count(b), &[Count(spent)], &Count(cost));
                let admit = Budget::of(&[("k", spent)]).merge(&Budget::of(&[("k", cost)])).leq(&Budget::of(&[("k", b)]));
                assert_eq!(gate, admit, "B={b} spent={spent} cost={cost}");
            }
        }
    }
}

/// [ROW]+[PROD] 装载期准入＝集合包含（roadmap Phase C"廉价版"原样）：
/// ∀ 动词 requires.caps ⊆ offers ∧ deps ⊆ provides；报错点名动词与缺项。
/// 再串一遍运行期：同一 manifest 的计划在 offers∩grant 与逐类 ↓B 下准入。
#[test]
fn manifest_admission_is_set_inclusion() {
    let mut m = Manifest { driver: "browser".into(), verbs: Default::default() };
    m.verbs.insert("snapshot".into(), Requires::of(&["dom.read"], &[], None));
    m.verbs.insert("click".into(), Requires::of(&["input"], &[], Some("browser::click")));
    m.verbs.insert("download".into(), Requires::of(&["fs.write"], &["cas"], Some("browser::download")));

    let mut slot = Mount { name: "browser-slot".into(), offers: Flat::of(&["dom.read", "input"]), provides: Flat::empty() };
    assert_eq!(
        admit_mount(&m, &slot),
        Err(AdmitError::VerbExceedsRow { verb: "download".into(), missing: Flat::of(&["fs.write"]) })
    );
    slot.offers = Flat::of(&["dom.read", "input", "fs.write"]);
    assert_eq!(
        admit_mount(&m, &slot),
        Err(AdmitError::MissingDependency { verb: "download".into(), missing: Flat::of(&["cas"]) })
    );
    slot.provides = Flat::of(&["cas"]);
    assert_eq!(admit_mount(&m, &slot), Ok(()));

    // 运行期：主体只被授了 dom.read+input（没 fs.write）——download 撞主体天花板；
    // click 批在逐类 ↓B 内通过。同一张 manifest、同一个 ≤。
    let lookup = |_: &str, v: &str| m.verbs[v].clone();
    let grant = Flat::of(&["dom.read", "input"]);
    let ceil = ceiling(&slot.offers, &grant);
    let consent = Budget::of(&[("browser::click", 4), ("browser::download", 1)]);
    assert_eq!(
        admit_plan(&Plan::verb("browser", "download"), &lookup, &ceil, &slot.provides, &consent),
        Err(AdmitError::ExceedsCeiling { missing: Flat::of(&["fs.write"]) })
    );
    let batch = Plan::Seq(vec![Plan::verb("browser", "snapshot"), Plan::loop_(4, Plan::verb("browser", "click"))]);
    assert_eq!(
        admit_plan(&batch, &lookup, &ceil, &slot.provides, &consent).unwrap().uses,
        Budget::of(&[("browser::click", 4)])
    );
}
