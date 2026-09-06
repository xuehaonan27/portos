//! Coeffect scalar — freeze drill F5：manifest `requires` 的类型（ABI 层，theory-spec §6 决策 6）。
//!
//! 一句话：`requires` ＝ **两个 flat scalar 元素（caps／deps）＋一个按效应类的 counting 向量（uses）**
//! （v2 措辞，B9 之后；初稿"一个 scalar 元素"被走查推翻）。effect row 检查（requires ⊆ 挂载点
//! offers）、能力检查（⊆ 主体授权）、预算检查（逐类 ≤ 同意向量 B）是**同一个 ≤ 在不同实例上
//! 的用法**；准入＝把计划按 scalar/向量运算静态求和、再做 ≤。本模块把这句话做成可运行的类型。
//!
//! 理论标签（对照 design/freeze-f5-requires.md 三列表；措辞纪律：不得强于被引处；
//! 等级：【文献✓】被引原文核实／【推导】我方映射／【设计】自家文档决定）：
//!
//!   [DEF1]  Petricek–Orchard–Mycroft ICFP 2014, Definition 1（2026-09-05 原文核实）【文献✓】：
//!           coeffect scalar ＝ (C, ~, ⊕, use, ign, ≤)，其中 (C,~,use) 与 (C,⊕,ign) 是幺半群、
//!           (C,≤) 是**预序**，加双侧分配律 (r⊕s)~t = (r~t)⊕(s~t)、t~(r⊕s) = (t~r)⊕(t~s)。
//!           原文**不要求**交换律与吸收律（如 ign~r = ign），定义里也**没有**算子对 ≤ 的单调性。
//!           ⇒ `Scalar` trait 只承诺这些；我们的实例多出来的性质（交换、单调、join）另测另标，
//!           代码路径不得依赖它们。
//!   [INST]  两个实例照原文抄【文献✓】：bounded reuse (ℕ, ×, +, 1, 0, ≤) ↔ counting／预算；
//!           implicit parameters (P(Name), ∪, ∪, ∅, ∅, ⊆) ↔ flat／权能集合。
//!   [APP]   原文 application 规则以 ~ 对参数余效应缩放（s ~ t）【文献✓】。循环 `bound N` 就是
//!           "把 body 用 N 次的函数"——`scale(N, body) = numeral(N) ~ body`，其中
//!           numeral(N) ＝ N 个 use 的 ⊕（在 counting 里是数字 N，在 flat 里是 ∅）。
//!           效应计划"循环界逐层相乘"由此得出，不是另立的规则——retro-confirmation。
//!   [VEC]   预算不是一个数，是**按效应类的向量**（effect-plan §5.1：B: 动词类×origin 类 → 上界；
//!           m0 同意面逐类渲染"echo.emit ≤ 4"）【设计】。P–O–M 的**结构化**演算正是这个形态
//!           【文献✓】：上下文携带一个标量向量，⊕ 逐分量，标量以 ~ 逐分量缩放。
//!           `Budget = Counting^K`（K＝效应类）。对固定 K 它逐分量满足 Definition 1【推导】；
//!           但 K 事先未知（随 manifest 生长），~ 的单位"处处为 1"没有有限表示——所以
//!           `Budget` **不**声称是 scalar，只提供向量运算：⊕、标量缩放、≤、join
//!           （semimodule over ℕ）。走查发现（B9，见 freeze-f5 §2）：初稿用单个 Counting，
//!           会把"click ≤ 4 ∧ send ≤ 1"塌缩成"总量 ≤ 5"，5 次 click 混过同意面。
//!   [PROD]  `Requires = caps × deps × uses`：两个 flat scalar 元素（cap-coeffect 纵向、
//!           dep-coeffect 横向——endstate 词表"manifest requires 同时声明两者"）＋一个
//!           counting 向量。逐分量运算、逐分量 ≤（不是字典序）【推导】。
//!   [BHAT]  effect-plan §5.2【设计】：B̂＝全部出现之和（工程近似，O(|plan|)）≥ B＝路径极大
//!           （精确）。分支的"极大"需要 join——**join 不在 Definition 1 合同内**，是实例级附加
//!           （`Join` trait 单列以示区别）。flat 实例里 join＝⊕＝∪，故 flat 分量上 B̂＝B 恒等；
//!           只有 counting 分量真的过近似。
//!   [ROW]   endstate §7.2-3 effect row【设计】：挂载点发布 offers（位置天花板），能力表给
//!           grant（主体天花板），实际可用＝offers ∩ grant——∩ 是 P(Name) 的格 meet，不在
//!           Definition 1 里，是 flat 实例的附加结构；由 ⊆ 的性质得
//!           requires ≤ offers∩grant ⟺ requires ≤ offers ∧ requires ≤ grant。
//!           roadmap Phase C 的"廉价版"（requires ⊆ offers 的集合包含）就是这个 ≤。
//!   [DOWN]  theory-spec §3.2【推导】：同意对象＝↓B（向量的逐分量下集），WYSIWYS 签的是一个
//!           逐类上界；同意单调性引理（effect-plan §5.4）【设计】：B′ ≤ B ⇒ ↓B′ ⊆ ↓B——
//!           预序传递性的直接后果。
//!   [TWO]   theory-spec §3.2【推导】：预算的 (ℕ,+,≤) 与 F1 `Count` RA 的 (op, ≼) 是同一交换
//!           幺半群的两读（流侧计量／存侧持有）。可执行形态：counting 的 ⊕ 逐点＝`Count::op`，
//!           "demand ∈ ↓B" 逐点＝F1 的 `auth_valid(● Count(B), ◯ Count(demand))`——
//!           F3 的花费闸门与本模块的准入闸门是**同一个谓词**。
//!   [F4]    动词是否进预算由真理表决定（F4 `bears_budget`）【推导】：uses ＝ 1 或 0。
//!           于是 `snapshot` 循环一千次预算为零、`click` 循环一千次预算一千——"先读后谋"
//!           （effect-plan §6.2）的类型层依据。
//!   [SAT]   偏离：ℕ 以 u64 饱和运算实现（S_max 远小于 2^64；饱和只是防溢出，不是语义）。

use crate::verbs::{VerbError, VerbTable};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

// ---------------------------------------------------------------------------
// [DEF1] 合同：只写 Definition 1 有的东西。
// ---------------------------------------------------------------------------
pub trait Scalar: Sized + Clone + PartialEq + Debug {
    /// ~ 复合/缩放（application 规则所用；幺半群，单位 use）。**不是**语句先后——
    /// 语句先后共用上下文用 ⊕（B8：把 ~ 当"顺序复合"会把前后两条语句的预算相乘）。
    fn seq(&self, other: &Self) -> Self;
    /// ⊕ 同一上下文内的合并（语句先后即此；幺半群，单位 ign）。
    fn merge(&self, other: &Self) -> Self;
    /// use：~ 的单位——"用一次、不多不少"。
    fn use_() -> Self;
    /// ign：⊕ 的单位——"不用上下文"。
    fn ign() -> Self;
    /// ≤ 子余效应预序："左边的需求被右边盖住"。
    fn leq(&self, other: &Self) -> bool;

    /// 数字 N 的 scalar 化：N 个 use 的 ⊕（N=0 时为 ign）。
    /// counting：1+1+…+1 = N；flat：∅∪∅∪… = ∅（flat 只记"哪些"，不记"几次"）。
    fn numeral(n: u64) -> Self {
        let mut acc = Self::ign();
        for _ in 0..n {
            acc = acc.merge(&Self::use_());
        }
        acc
    }

    /// [APP] 循环缩放＝application 规则：`bound N { body }` 是把 body 用 N 次的函数，
    /// 其需求 = numeral(N) ~ body。嵌套循环靠 ~ 的结合律自动相乘。
    fn scale(n: u64, body: &Self) -> Self {
        Self::numeral(n).seq(body)
    }
}

/// [BHAT] 实例级附加：分支的精确上界（最小上界）。**不在 Definition 1 合同内**，
/// 单列为 trait 以免被误当成 scalar 的一部分；只有需要"精确 B"的路径才用它。
pub trait Join: Scalar {
    fn join(&self, other: &Self) -> Self;
}

// ---------------------------------------------------------------------------
// [INST] 实例一：bounded reuse (ℕ, ×, +, 1, 0, ≤) —— counting／预算。
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord, Default)]
pub struct Counting(pub u64);

impl Scalar for Counting {
    /// ~ ＝ ×：函数把参数用 s 次、参数计算又用上下文 t 次 ⇒ s×t 次。
    fn seq(&self, o: &Self) -> Self {
        Counting(self.0.saturating_mul(o.0)) // [SAT]
    }
    /// ⊕ ＝ ＋：两段计算先后各用 r、s 次 ⇒ r+s 次。
    fn merge(&self, o: &Self) -> Self {
        Counting(self.0.saturating_add(o.0)) // [SAT]
    }
    fn use_() -> Self {
        Counting(1)
    }
    fn ign() -> Self {
        Counting(0)
    }
    fn leq(&self, o: &Self) -> bool {
        self.0 <= o.0
    }
}
impl Join for Counting {
    /// max：两条分支各用 r、s 次，任一条执行时的上界。
    fn join(&self, o: &Self) -> Self {
        Counting(self.0.max(o.0))
    }
}

// ---------------------------------------------------------------------------
// [INST] 实例二：implicit parameters (P(Name), ∪, ∪, ∅, ∅, ⊆) —— flat／权能集合。
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Flat(pub BTreeSet<String>);

impl Flat {
    pub fn of(names: &[&str]) -> Self {
        Flat(names.iter().map(|s| s.to_string()).collect())
    }
    pub fn empty() -> Self {
        Flat(BTreeSet::new())
    }
    /// [ROW] 格 meet ∩ —— 不在 Definition 1 合同内，是 P(Name) 的附加结构；
    /// 用来算"实际可用＝位置天花板 ∩ 主体天花板"。
    pub fn meet(&self, o: &Flat) -> Flat {
        Flat(self.0.intersection(&o.0).cloned().collect())
    }
    /// 差集，只为报错时指出"缺哪几个"。
    pub fn minus(&self, o: &Flat) -> Flat {
        Flat(self.0.difference(&o.0).cloned().collect())
    }
}

impl Scalar for Flat {
    /// ~ ＝ ∪：先后两段各需要的隐式参数并起来。
    fn seq(&self, o: &Self) -> Self {
        Flat(self.0.union(&o.0).cloned().collect())
    }
    /// ⊕ ＝ ∪：合并上下文同样是并。
    fn merge(&self, o: &Self) -> Self {
        Flat(self.0.union(&o.0).cloned().collect())
    }
    /// use ＝ ign ＝ ∅：flat 实例分不清"用一次"和"不用"——它只追踪**哪些**，不追踪几次。
    /// 后果：scale(0, S) = S（界为 0 的循环仍被算作需要 S）——保守方向，安全。
    fn use_() -> Self {
        Flat::empty()
    }
    fn ign() -> Self {
        Flat::empty()
    }
    fn leq(&self, o: &Self) -> bool {
        self.0.is_subset(&o.0)
    }
}
impl Join for Flat {
    /// join ＝ ∪ ＝ ⊕：flat 上"两条分支的上界"与"两段合并"是同一件事——
    /// 这就是为什么 flat 分量上 B̂ ＝ B 恒等（[BHAT]）。
    fn join(&self, o: &Self) -> Self {
        self.merge(o)
    }
}

// ---------------------------------------------------------------------------
// [VEC] 预算向量 Counting^K：按效应类计数。有限支撑映射，缺席＝0（＝Counting::ign）。
// 只提供向量运算——它是 ℕ 上的半模（semimodule），不是 Definition 1 的 scalar（见头注）。
// 规范化：运算结果剔除零项，使"缺席"与"显式 0"同一表示（PartialEq 才有意义）。
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Budget(pub BTreeMap<String, Counting>);

impl Budget {
    /// 零向量＝逐分量 ign：不占任何效应类的预算。
    pub fn zero() -> Self {
        Budget(BTreeMap::new())
    }
    /// 单位向量：某效应类用一次（逐分量 use，但只在这一类上）。
    pub fn unit(class: &str) -> Self {
        Budget([(class.to_string(), Counting(1))].into_iter().collect())
    }
    /// 加权单位：某效应类记 n 个单位（按量计价：fuel 秒、字节数——F6 走查项）。
    pub fn unit_n(class: &str, n: u64) -> Self {
        Self::of(&[(class, n)])
    }
    pub fn of(entries: &[(&str, u64)]) -> Self {
        Budget(entries.iter().filter(|(_, n)| *n > 0).map(|(k, n)| (k.to_string(), Counting(*n))).collect())
    }
    pub fn get(&self, class: &str) -> Counting {
        self.0.get(class).copied().unwrap_or(Counting::ign())
    }
    fn normalized(mut m: BTreeMap<String, Counting>) -> Self {
        m.retain(|_, v| v.0 > 0);
        Budget(m)
    }
    /// ⊕ 逐分量：两段计划各自的逐类用量相加。
    pub fn merge(&self, o: &Budget) -> Budget {
        let mut m = self.0.clone();
        for (k, v) in &o.0 {
            let cur = m.entry(k.clone()).or_insert(Counting::ign());
            *cur = cur.merge(v);
        }
        Self::normalized(m)
    }
    /// 标量缩放（[APP] 在向量上的形态）：numeral(n) ~ 每个分量＝逐类乘 n。
    pub fn scale(n: u64, body: &Budget) -> Budget {
        Self::normalized(body.0.iter().map(|(k, v)| (k.clone(), Counting::scale(n, v))).collect())
    }
    /// ≤ 逐分量：每一类的需求都被盖住才算盖住（缺席＝0 总被盖住）。
    pub fn leq(&self, o: &Budget) -> bool {
        self.0.iter().all(|(k, v)| v.leq(&o.get(k)))
    }
    /// join 逐分量（实例级附加，分支精确上界用）。
    pub fn join(&self, o: &Budget) -> Budget {
        let mut m = self.0.clone();
        for (k, v) in &o.0 {
            let cur = m.entry(k.clone()).or_insert(Counting::ign());
            *cur = cur.join(v);
        }
        Self::normalized(m)
    }
    /// 各类之和——只用于展示；**准入不看总量**（B9 的教训）。
    pub fn total(&self) -> u64 {
        self.0.values().map(|c| c.0).sum()
    }
    /// 首个越界的效应类（报错点名用）。
    pub fn first_exceeding(&self, bound: &Budget) -> Option<(String, u64, u64)> {
        self.0.iter().find(|(k, v)| !v.leq(&bound.get(k))).map(|(k, v)| (k.clone(), v.0, bound.get(k).0))
    }
}

// ---------------------------------------------------------------------------
// [PROD] manifest `requires` 的类型：两个 flat scalar 元素 ＋ 一个 counting 向量。
// 这是决策 6 的答案本身（v2 形态：走查后把 uses 从单数改为向量）。
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Requires {
    /// cap-coeffect（纵向）：需要发放方授予的权能——与 effect row / 能力表比（flat scalar）。
    pub caps: Flat,
    /// dep-coeffect（横向）：需要同侪提供的服务——与挂载点的 provides 比（flat scalar）。
    pub deps: Flat,
    /// counting 向量：逐效应类用几次——与同意向量 B 逐类比（↓B）。
    pub uses: Budget,
}

impl Requires {
    /// 一个动词的 requires：权能、依赖、以及它在哪个效应类上记一次（None＝可重复，不记）。
    pub fn of(caps: &[&str], deps: &[&str], class: Option<&str>) -> Self {
        Requires {
            caps: Flat::of(caps),
            deps: Flat::of(deps),
            uses: class.map(Budget::unit).unwrap_or_else(Budget::zero),
        }
    }

    /// [F4] 从真理表取"是否进预算"：可重复动词不记；消耗/发射动词在效应类 `handler.verb` 上记一次。
    /// 权能与依赖仍由 manifest 声明（表管性格，manifest 管需求）。
    /// 效应类键取 handler.verb——effect-plan 的 origin 维度由 F3 的 (动词,目标) 范围承担（偏离申报）。
    pub fn from_table(
        table: &VerbTable,
        class: &str,
        verb: &str,
        caps: &[&str],
        deps: &[&str],
    ) -> Result<Self, VerbError> {
        Self::from_table_weighted(table, class, verb, caps, deps, 1)
    }

    /// [F4]+[SAT] 按量计价（F6 走查项，endstate 开放问题 7 的既定路径）：静态准入用**声明上界**
    /// `weight`（每次调用最多耗多少单位：fuel 秒、字节），运行期 F3 按实际 cost 扣——二者由同一
    /// 闸门（[TWO]：can_mint ＝ ↓B）衔接：实际 ≤ 声明 ⇒ 静态过则运行期必过。可重复动词恒 0。
    pub fn from_table_weighted(
        table: &VerbTable,
        class: &str,
        verb: &str,
        caps: &[&str],
        deps: &[&str],
        weight: u64,
    ) -> Result<Self, VerbError> {
        let e = table.lookup(class, verb)?;
        let key = format!("{class}::{verb}"); // D29：效应类键用 `family::verb`
        Ok(Requires {
            caps: Flat::of(caps),
            deps: Flat::of(deps),
            uses: if e.bears_budget() { Budget::unit_n(&key, weight) } else { Budget::zero() },
        })
    }

    pub fn ign() -> Self {
        Requires::default()
    }
    /// ⊕ 逐分量。
    pub fn merge(&self, o: &Self) -> Self {
        Requires { caps: self.caps.merge(&o.caps), deps: self.deps.merge(&o.deps), uses: self.uses.merge(&o.uses) }
    }
    /// [APP] 缩放逐分量：flat 分量恒等（numeral＝∅），向量分量逐类乘 n。
    pub fn scale(n: u64, body: &Self) -> Self {
        Requires { caps: Flat::scale(n, &body.caps), deps: Flat::scale(n, &body.deps), uses: Budget::scale(n, &body.uses) }
    }
    /// ≤ **逐分量**（不是字典序）：三个分量各自被盖住才算盖住。
    pub fn leq(&self, o: &Self) -> bool {
        self.caps.leq(&o.caps) && self.deps.leq(&o.deps) && self.uses.leq(&o.uses)
    }
    /// join 逐分量（实例级附加）。
    pub fn join(&self, o: &Self) -> Self {
        Requires { caps: self.caps.join(&o.caps), deps: self.deps.join(&o.deps), uses: self.uses.join(&o.uses) }
    }
    /// [DOWN] demand ∈ ↓B：同意向量逐类盖住了需求向量（权能分量走 [ROW]，不在此）。
    pub fn covered_by_budget(&self, budget: &Budget) -> bool {
        self.uses.leq(budget)
    }
}

// ---------------------------------------------------------------------------
// 计划的极小 AST 与两种求值。语言本体（守卫、变量、纯计算）归 m0 plancheck；
// 这里只留决定计量的四种形状：动词、顺序、有界循环、分支。
// **D31 隔离区**：计划语言仍在讨论中——以下计划形状的类型与求值只在 cargo feature
// `plan-shapes`（默认开，法则测试用）下编译；内核以 `default-features = false` 依赖本 crate，
// 从而在编译期就无法依赖它们。
// ---------------------------------------------------------------------------
#[cfg(feature = "plan-shapes")]
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Plan {
    Verb { handler: String, verb: String },
    Seq(Vec<Plan>),
    Loop { bound: u64, body: Box<Plan> },
    Branch(Box<Plan>, Box<Plan>),
}

#[cfg(feature = "plan-shapes")]
impl Plan {
    pub fn verb(handler: &str, verb: &str) -> Plan {
        Plan::Verb { handler: handler.into(), verb: verb.into() }
    }
    pub fn loop_(bound: u64, body: Plan) -> Plan {
        Plan::Loop { bound, body: Box::new(body) }
    }
    pub fn branch(a: Plan, b: Plan) -> Plan {
        Plan::Branch(Box::new(a), Box::new(b))
    }
}

/// 查表：(handler, verb) → 该动词的 requires。由 manifest（权能/依赖）与真理表（uses）合成。
#[cfg(feature = "plan-shapes")]
pub type Lookup<'a> = dyn Fn(&str, &str) -> Requires + 'a;

/// [BHAT] B̂：全部出现之和——顺序与分支都用 ⊕，循环用 [APP] 缩放。O(|plan|)，只用合同内的运算。
#[cfg(feature = "plan-shapes")]
pub fn demand_sum(plan: &Plan, lookup: &Lookup) -> Requires {
    match plan {
        Plan::Verb { handler, verb } => lookup(handler, verb),
        Plan::Seq(items) => items.iter().fold(Requires::ign(), |acc, p| acc.merge(&demand_sum(p, lookup))),
        Plan::Loop { bound, body } => Requires::scale(*bound, &demand_sum(body, lookup)),
        // 过近似点：两条分支都算进去（B̂ ≥ B 的来源）。
        Plan::Branch(a, b) => demand_sum(a, lookup).merge(&demand_sum(b, lookup)),
    }
}

/// [BHAT] B：路径极大——分支取 join（实例级附加），其余同 B̂。
/// effect-plan §5.2 的"精确版"：max over 根→叶路径 of Σ 出现×循环界之积。
#[cfg(feature = "plan-shapes")]
pub fn demand_paths(plan: &Plan, lookup: &Lookup) -> Requires {
    match plan {
        Plan::Verb { handler, verb } => lookup(handler, verb),
        Plan::Seq(items) => items.iter().fold(Requires::ign(), |acc, p| acc.merge(&demand_paths(p, lookup))),
        Plan::Loop { bound, body } => Requires::scale(*bound, &demand_paths(body, lookup)),
        Plan::Branch(a, b) => demand_paths(a, lookup).join(&demand_paths(b, lookup)),
    }
}

// ---------------------------------------------------------------------------
// Manifest 与挂载点，以及两道准入：装载期（驱动 vs 挂载点）与运行期（计划 vs 天花板∩预算）。
// ---------------------------------------------------------------------------
/// 驱动 manifest：每个动词一份 requires。文件格式（TOML）不在本决策——冻结的是类型与运算。
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    pub driver: String,
    pub verbs: BTreeMap<String, Requires>,
}

/// 挂载点（扩展点）：offers ＝ effect row（此位置可用的权能集合）；provides ＝ 此处可得的同侪服务面。
#[derive(Clone, Debug, Default)]
pub struct Mount {
    pub name: String,
    pub offers: Flat,
    pub provides: Flat,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AdmitError {
    /// [ROW] 装载期：某动词的权能需求超出位置天花板——"混淆位置"防线。
    VerbExceedsRow { verb: String, missing: Flat },
    /// 装载期：某动词依赖的同侪服务此处不提供。
    MissingDependency { verb: String, missing: Flat },
    /// 运行期：计划总需求超出 offers∩grant。
    ExceedsCeiling { missing: Flat },
    /// 运行期：计划总需求依赖的服务不在提供面内。
    UnmetDependency { missing: Flat },
    /// [DOWN] 运行期：某效应类的用量不在 ↓B 内（点名类——同意面逐类渲染，报错也逐类）。
    OverBudget { class: String, demand: u64, budget: u64 },
}

/// [ROW] 装载期准入（roadmap Phase C"廉价版"原样）：∀ 动词，requires.caps ⊆ offers 且 deps ⊆ provides。
/// 一次集合包含，无其它算术。
pub fn admit_mount(m: &Manifest, mount: &Mount) -> Result<(), AdmitError> {
    for (verb, req) in &m.verbs {
        if !req.caps.leq(&mount.offers) {
            return Err(AdmitError::VerbExceedsRow { verb: verb.clone(), missing: req.caps.minus(&mount.offers) });
        }
        if !req.deps.leq(&mount.provides) {
            return Err(AdmitError::MissingDependency { verb: verb.clone(), missing: req.deps.minus(&mount.provides) });
        }
    }
    Ok(())
}

/// [ROW] 实际可用天花板＝位置天花板 ∩ 主体天花板。
pub fn ceiling(offers: &Flat, grant: &Flat) -> Flat {
    offers.meet(grant)
}

/// 运行期准入：计划的 B̂（合同内运算）对三个天花板各做一次 ≤——两个 flat 的 ⊆、一个向量的逐类 ≤。
/// 返回算出的需求——同意面要渲染给人看的就是它（预算文案由内核从 B̂ 逐类确定性生成）。
#[cfg(feature = "plan-shapes")]
pub fn admit_plan(
    plan: &Plan,
    lookup: &Lookup,
    ceiling: &Flat,
    provides: &Flat,
    budget: &Budget,
) -> Result<Requires, AdmitError> {
    let d = demand_sum(plan, lookup);
    if !d.caps.leq(ceiling) {
        return Err(AdmitError::ExceedsCeiling { missing: d.caps.minus(ceiling) });
    }
    if !d.deps.leq(provides) {
        return Err(AdmitError::UnmetDependency { missing: d.deps.minus(provides) });
    }
    if let Some((class, demand, bound)) = d.uses.first_exceeding(budget) {
        return Err(AdmitError::OverBudget { class, demand, budget: bound });
    }
    Ok(d)
}
