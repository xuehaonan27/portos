//! WP-05 的投影法则：M0 准入（`plancheck::admit`）算出的预算，与法则 crate
//! `demand_sum`（B̂＝出现之和，effect-plan §5.2）在全部小计划网格上逐效应类相等。
//! 网格与法则侧 F5/F6 同工序（确定性穷举：深度 ≤2、三个语句原型、循环界 0..=3，
//! 3＋30＋2310＝2343 个计划）。不相等处即其中一侧的 bug——两侧独立计算，互为对偶。

use portos_kernel::plan_ir::to_ast_nodes;
use portos_kernel::plancheck::{VerbSchemas, admit};
use portos_proto::{Expr, Plan as ProtoPlan, Stmt};
use portos_rm::coeffect::{Plan as LawPlan, Requires, demand_sum};
use std::collections::BTreeMap;

fn schemas() -> VerbSchemas {
    let mut s = VerbSchemas::default();
    s.observe
        .insert("wp::list".into(), portos_proto::Label::public_trusted());
    s.external_effects.insert("wp::a".into(), false);
    s.external_effects.insert("wp::b".into(), false);
    s
}

/// 每个计量动词都在自己的效应类上记一次（真理表 "bears_budget" 的网格侧镜像）。
fn all_budgeted(handler: &str, verb: &str) -> Requires {
    Requires::of(&[], &[], Some(&format!("{handler}::{verb}")))
}

/// 与法则侧 `plans_up_to_depth` 同形的网格，建在 proto AST 上；第三个原型
/// 是不计量的 `Let`+`Observe`（投影应把它塌缩为透明）。
fn proto_plans_up_to_depth(depth: u32) -> Vec<ProtoPlan> {
    let leaves: Vec<Vec<Stmt>> = vec![
        vec![Stmt::Effect { verb: "wp::a".into(), args: vec![] }],
        vec![Stmt::Effect { verb: "wp::b".into(), args: vec![] }],
        vec![Stmt::Let {
            var: "x".into(),
            expr: Expr::Observe { verb: "wp::list".into(), args: vec![] },
        }],
    ];
    let mut all: Vec<Vec<Stmt>> = leaves;
    for _ in 0..depth {
        let mut next = Vec::new();
        for p in &all {
            for q in &all {
                next.push([p.clone(), q.clone()].concat());
                next.push(vec![Stmt::If {
                    guard: portos_proto::Guard::Exists {
                        expr: Box::new(Expr::Const { value: serde_json::json!([]) }) },
                    then_: p.clone(),
                    else_: q.clone(),
                }]);
            }
            for bound in 0..=3u32 {
                next.push(vec![Stmt::Foreach {
                    var: "x".into(),
                    list: Expr::Const { value: serde_json::json!([]) },
                    bound,
                    mode: portos_proto::Mode::Strict,
                    body: p.clone(),
                }]);
            }
        }
        all.extend(next);
    }
    all.into_iter().map(|stmts| ProtoPlan { stmts }).collect()
}

#[test]
fn plancheck_budget_equals_demand_sum_on_all_small_plans() {
    let plans = proto_plans_up_to_depth(2);
    assert_eq!(plans.len(), 3 + 30 + 2310, "计划枚举规模被收缩");
    for plan in &plans {
        let adm = admit(plan, &schemas()).unwrap();
        // plancheck 在界为 0 的循环下也会留 0 键；法则侧 Budget 规范化剔除零项。
        let got: BTreeMap<&String, u64> =
            adm.budget.iter().filter(|(_, n)| **n > 0).map(|(k, v)| (k, *v)).collect();
        let law = demand_sum(&LawPlan::from_ast(&to_ast_nodes(plan)), &all_budgeted);
        let want: BTreeMap<&String, u64> =
            law.uses.0.iter().map(|(k, c)| (k, c.value().expect("small fixture budget"))).collect();
        assert_eq!(got, want, "plancheck 预算 ≠ demand_sum（逐效应类）：{plan:?}");
    }
}
