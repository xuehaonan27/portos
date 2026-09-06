//! Resource algebra (RA) — adopted from the Iris Technical Reference (appendix-4.5,
//! read 2026-08-30; see design/theory-spec-v0.md §2.2 for the frozen source).
//!
//! Adopted laws (the ones the tests in `tests/f1_ledger.rs` enforce):
//!   assoc      : (a·b)·c = a·(b·c)
//!   comm       : a·b = b·a
//!   valid-op-l : ✓(a·b) ⇒ ✓a
//!   core-id    : |a|·a = a            (where pcore is defined)
//!   core-idem  : ||a|| = |a|
//!   core-mono  : a ≼ b ⇒ |a| ≼ |b|
//!   incl       : a ≼ b ⟺ ∃c. b = a·c  (per-algebra realization; documented where no unit exists)
//!
//! Deliberate representation choices (declared deviations, see freeze record):
//!   * `op` is TOTAL and invalid compositions are explicit elements (Iris style),
//!     rather than a partial PCM `Option<Self>`. Validity does the exclusion work.
//!   * Cancellativity is NOT assumed anywhere (Iris drops it). Consequence for the
//!     ledger: fragments must be stored per-row (release = drop a row), because
//!     algebraic subtraction does not exist in general. See ledger.rs.

/// The RA contract. Kept object-safe-free and simple for the drill; the Phase D
/// implementation may generalize to serialized dynamic algebras behind the same laws.
pub trait Ra: Sized + Clone + PartialEq + std::fmt::Debug {
    /// a · b (total; may yield an invalid element).
    fn op(&self, other: &Self) -> Self;
    /// ✓ a
    fn valid(&self) -> bool;
    /// |a| — partial core. `None` where the algebra has no core (e.g. Ex).
    fn pcore(&self) -> Option<Self>;
    /// a ≼ b. For algebras with a unit this coincides with ∃c. b = a·c.
    /// For Ex (no unit) we take the reflexive closure — declared in the freeze record.
    fn included_in(&self, b: &Self) -> bool;
}

// ---------------------------------------------------------------------------
// Ex — exclusive token (named exclusive resources: a port, a process slot).
// ex·ex is invalid; no core; inclusion is reflexive (no unit exists).
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ex {
    Token,
    /// ⊥ — the invalid element produced by composing two exclusive claims.
    Bot,
}

impl Ra for Ex {
    fn op(&self, _other: &Self) -> Self {
        Ex::Bot
    }
    fn valid(&self) -> bool {
        matches!(self, Ex::Token)
    }
    fn pcore(&self) -> Option<Self> {
        None
    }
    fn included_in(&self, b: &Self) -> bool {
        self == b
    }
}

// ---------------------------------------------------------------------------
// Count — ℕ with addition (fungible quantities: quota units, budget counts).
// Unit 0 is the core. Fragments are always valid; the CAP lives in the
// authoritative element (auth.rs), not in fragment validity.
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Count(pub u64);

impl Ra for Count {
    fn op(&self, other: &Self) -> Self {
        Count(self.0.saturating_add(other.0))
    }
    fn valid(&self) -> bool {
        true
    }
    fn pcore(&self) -> Option<Self> {
        Some(Count(0))
    }
    fn included_in(&self, b: &Self) -> bool {
        self.0 <= b.0
    }
}

// ---------------------------------------------------------------------------
// GSet — grow-set with union (duplicable claims: capability name sets).
// Idempotent, self-core (duplicable), inclusion = subset.
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct GSet(pub std::collections::BTreeSet<String>);

impl GSet {
    pub fn of(items: &[&str]) -> Self {
        GSet(items.iter().map(|s| s.to_string()).collect())
    }
}

impl Ra for GSet {
    fn op(&self, other: &Self) -> Self {
        GSet(self.0.union(&other.0).cloned().collect())
    }
    fn valid(&self) -> bool {
        true
    }
    fn pcore(&self) -> Option<Self> {
        Some(self.clone())
    }
    fn included_in(&self, b: &Self) -> bool {
        self.0.is_subset(&b.0)
    }
}

// ---------------------------------------------------------------------------
// Ranges — F6（RDMA 走查）：不相交半开区间集（字节范围锁、MR 子区间、memory window）。
// endstate §8.2 库清单"子区间持有：不相交区间可组合"的 RA 形态；Iris 里对应
// "disjoint sets/ranges" 一族。规范化表示（排序、去重叠、合并相邻）使相等判定即结构相等。
//   op    : 两集合并；任何重叠 ⇒ ⊥（显式非法元）
//   valid : 非 ⊥
//   pcore : Some(∅) —— 有单位（空集）
//   ≼     : 覆盖（a 的每个点都在 b 中）＝ ∃c. b = a·c（c ＝ b∖a，规范化保证唯一）；
//           ⊥ 的约定：a ≼ ⊥ 对一切 a 成立（总能选重叠的 c），⊥ ≼ b ⟺ b = ⊥。
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Ranges {
    /// 规范化：按起点排序、两两不相交、相邻已合并。
    pub spans: Vec<(u64, u64)>,
    /// ⊥：合成时发生重叠。
    pub bot: bool,
}

impl Ranges {
    pub fn of(spans: &[(u64, u64)]) -> Self {
        // 构造时若重叠即 ⊥（与 op 一致）。
        let mut r = Ranges::default();
        for s in spans {
            r = r.op(&Ranges { spans: vec![*s], bot: false });
        }
        r
    }
    pub fn empty() -> Self {
        Ranges::default()
    }
    pub fn bot() -> Self {
        Ranges { spans: Vec::new(), bot: true }
    }
    fn normalize(mut v: Vec<(u64, u64)>) -> Result<Vec<(u64, u64)>, ()> {
        v.retain(|(a, b)| a < b); // 空区间不占位
        v.sort_unstable();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(v.len());
        for (a, b) in v {
            if let Some(last) = out.last_mut() {
                if a < last.1 {
                    return Err(()); // 重叠
                }
                if a == last.1 {
                    last.1 = b; // 相邻合并
                    continue;
                }
            }
            out.push((a, b));
        }
        Ok(out)
    }
    /// 覆盖判定：self 的每个点都在 other 中。
    fn covered_by(&self, other: &Ranges) -> bool {
        self.spans.iter().all(|(a, b)| other.spans.iter().any(|(c, d)| c <= a && b <= d))
    }
}

impl Ra for Ranges {
    fn op(&self, other: &Self) -> Self {
        if self.bot || other.bot {
            return Ranges::bot();
        }
        let mut v = self.spans.clone();
        v.extend(other.spans.iter().copied());
        match Ranges::normalize(v) {
            Ok(spans) => Ranges { spans, bot: false },
            Err(()) => Ranges::bot(),
        }
    }
    fn valid(&self) -> bool {
        !self.bot
    }
    fn pcore(&self) -> Option<Self> {
        Some(Ranges::empty())
    }
    fn included_in(&self, b: &Self) -> bool {
        if b.bot {
            return true;
        }
        if self.bot {
            return false;
        }
        self.covered_by(b)
    }
}

// ---------------------------------------------------------------------------
// Frac — F6（RDMA 走查）：分数持有（Boyland fractional permissions；Iris frac RA）。
// 读共享＝各持一份正分数，合成回 1 才是独占；endstate §8.2 库清单所列。
//   op    : 相加（有理数，gcd 规范化）；和 > 1 ⇒ 非法（显式，不另设 ⊥）
//   valid : 0 < q ≤ 1
//   pcore : None —— 无核（与 Iris frac 一致；也无单位）
//   ≼     : q₁ ≤ q₂（自反闭包——与 F1 对 Ex 的申报同款：Iris 原为严格 <，
//           我们取自反以使 ✓(●1·◯1) 成立；偏离申报见 F6 记录）
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frac {
    pub num: u64,
    pub den: u64,
}

impl Frac {
    pub fn new(num: u64, den: u64) -> Self {
        assert!(den > 0, "denominator must be positive");
        let g = gcd(num, den);
        Frac { num: num / g, den: den / g }
    }
    pub fn one() -> Self {
        Frac { num: 1, den: 1 }
    }
    fn cmp_key(&self) -> (u128, u128) {
        // 比较 num/den：交叉相乘（u128 防溢出）
        (self.num as u128, self.den as u128)
    }
    fn leq(&self, o: &Frac) -> bool {
        let (a, b) = self.cmp_key();
        let (c, d) = o.cmp_key();
        a * d <= c * b
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    if a == 0 {
        return b.max(1);
    }
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

impl Ra for Frac {
    fn op(&self, other: &Self) -> Self {
        // a/b + c/d = (ad + cb)/(bd)，饱和防溢出（演练分母极小）
        let num = (self.num as u128 * other.den as u128 + other.num as u128 * self.den as u128).min(u64::MAX as u128) as u64;
        let den = (self.den as u128 * other.den as u128).min(u64::MAX as u128) as u64;
        Frac::new(num, den)
    }
    fn valid(&self) -> bool {
        self.num > 0 && self.num <= self.den
    }
    fn pcore(&self) -> Option<Self> {
        None
    }
    fn included_in(&self, b: &Self) -> bool {
        self.leq(b)
    }
}

// ---------------------------------------------------------------------------
// Law checkers — used by tests/f1_ledger.rs over sampled elements.
// ---------------------------------------------------------------------------
pub fn law_assoc<A: Ra>(a: &A, b: &A, c: &A) -> bool {
    a.op(b).op(c) == a.op(&b.op(c))
}
pub fn law_comm<A: Ra>(a: &A, b: &A) -> bool {
    a.op(b) == b.op(a)
}
pub fn law_valid_op_l<A: Ra>(a: &A, b: &A) -> bool {
    !a.op(b).valid() || a.valid()
}
pub fn law_core_id<A: Ra>(a: &A) -> bool {
    match a.pcore() {
        None => true,
        Some(ca) => ca.op(a) == *a,
    }
}
pub fn law_core_idem<A: Ra>(a: &A) -> bool {
    match a.pcore() {
        None => true,
        Some(ca) => ca.pcore() == Some(ca.clone()),
    }
}
pub fn law_core_mono<A: Ra>(a: &A, b: &A) -> bool {
    if !a.included_in(b) {
        return true;
    }
    match (a.pcore(), b.pcore()) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(ca), Some(cb)) => ca.included_in(&cb),
    }
}

/// Frame-preserving update, checked empirically against a frame sample:
///   a ⤳ b  ⟺  ∀f. ✓(a·f) ⇒ ✓(b·f)
/// (Iris Technical Reference, adopted verbatim; sampled rather than quantified.)
pub fn fpu_holds<A: Ra>(a: &A, b: &A, frames: &[A]) -> bool {
    frames.iter().all(|f| !a.op(f).valid() || b.op(f).valid())
}
