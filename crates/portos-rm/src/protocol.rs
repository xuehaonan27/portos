//! 协议次序（session-type 的有穷自动机形态）— F6（RDMA 走查逼出的表列）。
//!
//! 资源类五元组的 E（等式）里本就列着"协议次序 ⇒ session type"（theory-spec §2.3），
//! F4 把它申报为 Phase F 扩充；RDMA 的 QP 状态机（RESET→INIT→RTR→RTS，post_send 只在 RTS
//! 合法）让它一上来就承重。本模块把"协议"做成**动词上的确定性安全自动机**，并给出：
//!
//!   [SAFE]  协议是 safety 性质【文献✓ Schneider／IJIS 2005】：前缀闭、违规不可救——
//!           所以 F3 的 precise 档（截停）就能精确执行它，无需扣发缓冲。运行期钩子见 monitor.rs。
//!   [STATIC] 准入期静态检查（与 F5 的 B̂ 同位）：状态**集**语义——动词逐状态步进、分支取并、
//!           `bound N` 循环按 0..=N 次迭代取并（N 是上界不是次数：`foreach … bound N` 实际
//!           跑 |xs| ≤ N 轮）。对确定性自动机，这就是"某条路径可达的状态集"，
//!           与穷举路径逐条判定**恰好一致**（法则测试对全部小计划形状逐个比对）。【推导】
//!   [REORDER] 扣发会重排世界序：被扣发的动词在批准时才发射（原序、殿后），非扣发动词即时
//!           发射。计划序合法不等于世界序合法（"先 close 后 send"，send 被扣发 ⇒ 世界里
//!           send 排在 close 后）。静态检查按**世界序投影**做：Seq(非扣发投影, 扣发投影)。
//!           该投影对分支是**过近似**（两半各自独立选分支），故结论是 sound（静态过 ⇒ 无一条
//!           世界序路径违规），无分支时精确。与 B̂ 同一哲学：准入宁紧勿漏。【推导】
//!   [SCOPE] 协议只约束其"辖域动词"；辖域外动词不改状态（协议是对一个资源类内一组动词的
//!           约束，不是对整台监督器的约束）。

#[cfg(feature = "plan-shapes")]
use crate::coeffect::Plan;
use std::collections::{BTreeMap, BTreeSet};

/// 动词上的确定性安全自动机。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Protocol {
    pub initial: String,
    /// (state, verb) → next state。缺项＝违规（安全自动机：未列出的转移一律拒）。
    pub transitions: BTreeMap<(String, String), String>,
    /// 辖域动词：出现在任何转移里的动词。辖域外动词不改状态。
    pub scoped: BTreeSet<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Violation {
    pub verb: String,
    /// 违规时所处的状态（静态检查里是可达状态集中首个使该动词无转移的状态）。
    pub state: String,
    /// 序列检查时的位置；静态检查为 None。
    pub at: Option<usize>,
}

impl Protocol {
    pub fn new(initial: &str) -> Self {
        Protocol { initial: initial.into(), transitions: BTreeMap::new(), scoped: BTreeSet::new() }
    }
    pub fn transition(mut self, from: &str, verb: &str, to: &str) -> Self {
        self.transitions.insert((from.into(), verb.into()), to.into());
        self.scoped.insert(verb.into());
        self
    }
    /// 在给定状态执行动词。辖域外动词 ⇒ 状态不变；辖域内无转移 ⇒ 违规。
    pub fn step(&self, state: &str, verb: &str) -> Result<String, Violation> {
        if !self.scoped.contains(verb) {
            return Ok(state.to_string());
        }
        self.transitions
            .get(&(state.to_string(), verb.to_string()))
            .cloned()
            .ok_or(Violation { verb: verb.into(), state: state.into(), at: None })
    }
    /// 序列判定：返回首个违规位置。这是"精确执行＝交付最长合法前缀"的判据函数。
    pub fn check_sequence(&self, verbs: &[&str]) -> Result<String, Violation> {
        let mut st = self.initial.clone();
        for (i, v) in verbs.iter().enumerate() {
            st = self.step(&st, v).map_err(|mut e| {
                e.at = Some(i);
                e
            })?;
        }
        Ok(st)
    }

    /// [STATIC] 状态集语义：从状态集出发跑完计划，得到可达状态集；任一路径违规即 Err。
    #[cfg(feature = "plan-shapes")]
    pub fn reach(&self, plan: &Plan, states: &BTreeSet<String>) -> Result<BTreeSet<String>, Violation> {
        match plan {
            Plan::Verb { verb, .. } => {
                let mut out = BTreeSet::new();
                for s in states {
                    out.insert(self.step(s, verb)?);
                }
                Ok(out)
            }
            Plan::Seq(items) => {
                let mut cur = states.clone();
                for p in items {
                    cur = self.reach(p, &cur)?;
                }
                Ok(cur)
            }
            // bound 是上界：实际跑 0..=N 轮，可达集取并；每一轮都不得违规。
            Plan::Loop { bound, body } => {
                let mut acc = states.clone();
                let mut cur = states.clone();
                for _ in 0..*bound {
                    cur = self.reach(body, &cur)?;
                    acc.extend(cur.iter().cloned());
                }
                Ok(acc)
            }
            Plan::Branch(a, b) => {
                let mut out = self.reach(a, states)?;
                out.extend(self.reach(b, states)?);
                Ok(out)
            }
        }
    }

    /// 准入期检查（计划序）。
    #[cfg(feature = "plan-shapes")]
    pub fn check_plan(&self, plan: &Plan) -> Result<BTreeSet<String>, Violation> {
        let init: BTreeSet<String> = [self.initial.clone()].into_iter().collect();
        self.reach(plan, &init)
    }

    /// [REORDER] 准入期检查（世界序）：扣发动词殿后。Seq(非扣发投影, 扣发投影)，
    /// 对分支过近似（sound）。`withhold` 为该 handler 的扣发集（F4 投影）。
    #[cfg(feature = "plan-shapes")]
    pub fn check_plan_world_order(&self, plan: &Plan, withhold: &BTreeSet<String>) -> Result<BTreeSet<String>, Violation> {
        let immediate = project(plan, &|v| !withhold.contains(v));
        let deferred = project(plan, &|v| withhold.contains(v));
        self.check_plan(&Plan::Seq(vec![immediate, deferred]))
    }
}

/// 计划投影：只保留 keep(verb) 为真的叶子，其余叶子变空序列；树形（循环/分支）原样保留。
#[cfg(feature = "plan-shapes")]
pub fn project(plan: &Plan, keep: &dyn Fn(&str) -> bool) -> Plan {
    match plan {
        Plan::Verb { verb, .. } => {
            if keep(verb) {
                plan.clone()
            } else {
                Plan::Seq(Vec::new())
            }
        }
        Plan::Seq(items) => Plan::Seq(items.iter().map(|p| project(p, keep)).collect()),
        Plan::Loop { bound, body } => Plan::Loop { bound: *bound, body: Box::new(project(body, keep)) },
        Plan::Branch(a, b) => Plan::Branch(Box::new(project(a, keep)), Box::new(project(b, keep))),
    }
}

/// 穷举计划的全部具体执行路径（循环按 0..=N 轮展开、分支两取）。测试用参照实现；
/// 规模随嵌套指数增长，只对小计划使用——它是 [STATIC] 的"真值表"。
#[cfg(feature = "plan-shapes")]
pub fn enumerate_paths(plan: &Plan) -> Vec<Vec<String>> {
    match plan {
        Plan::Verb { verb, .. } => vec![vec![verb.clone()]],
        Plan::Seq(items) => {
            let mut acc: Vec<Vec<String>> = vec![Vec::new()];
            for p in items {
                let sub = enumerate_paths(p);
                let mut next = Vec::new();
                for a in &acc {
                    for s in &sub {
                        let mut v = a.clone();
                        v.extend(s.iter().cloned());
                        next.push(v);
                    }
                }
                acc = next;
            }
            acc
        }
        Plan::Loop { bound, body } => {
            let sub = enumerate_paths(body);
            let mut out = vec![Vec::new()]; // 0 轮
            let mut prefixes: Vec<Vec<String>> = vec![Vec::new()];
            for _ in 0..*bound {
                let mut next = Vec::new();
                for a in &prefixes {
                    for s in &sub {
                        let mut v = a.clone();
                        v.extend(s.iter().cloned());
                        next.push(v);
                    }
                }
                out.extend(next.iter().cloned());
                prefixes = next;
            }
            out
        }
        Plan::Branch(a, b) => {
            let mut out = enumerate_paths(a);
            out.extend(enumerate_paths(b));
            out
        }
    }
}
