//! Resource algebra (RA), following Iris 4.5 §2.3 and `.dev/design/spec.md` §2.2.
//!
//! Adopted laws (the ones the tests in `tests/f1_ledger.rs` enforce):
//!   assoc      : (a·b)·c = a·(b·c)
//!   comm       : a·b = b·a
//!   valid-op-l : ✓(a·b) ⇒ ✓a
//!   core-id    : |a|·a = a            (where pcore is defined)
//!   core-idem  : ||a|| = |a|
//!   core-mono  : |a| defined ∧ a ≼ b ⇒ |b| defined ∧ |a| ≼ |b|
//!   incl       : a ≼ b ⟺ ∃c ∈ M. b = a·c
//!
//! Representation choices:
//!   * `op` is TOTAL and invalid compositions are explicit elements (Iris style),
//!     rather than a partial PCM `Option<Self>`. Validity does the exclusion work.
//!   * Cancellativity is not required; particular instances may support subtraction.
//!     Keeping each holding is the ledger's choice for ownership and lifecycle tracking.
//!   * `Option<A>` adds the empty unit used by the ledger's capacity predicate.
//!     Base Ex/Frac keep their non-reflexive inclusion relation on valid elements.
//!   * Count is bounded by u64::MAX; overflow is an invalid absorbing element.
//!     Frac uses exact rational arithmetic and one invalid absorbing element.

use num_bigint::BigInt;
use num_rational::BigRational;

/// The RA contract. Kept object-safe-free and simple for the drill; the Phase D
/// implementation may generalize to serialized dynamic algebras behind the same laws.
pub trait Ra: Sized + Clone + PartialEq + std::fmt::Debug {
    /// a · b (total; may yield an invalid element).
    fn op(&self, other: &Self) -> Self;
    /// ✓ a
    fn valid(&self) -> bool;
    /// |a| — partial core. `None` where the algebra has no core (e.g. Ex).
    fn pcore(&self) -> Option<Self>;
    /// a ≼ b iff ∃c ∈ Self. b = a·c. Without a unit this need not be reflexive.
    fn included_in(&self, b: &Self) -> bool;
}

/// Iris §4.3: lift an RA by adding an empty, valid unit.
impl<A: Ra> Ra for Option<A> {
    fn op(&self, other: &Self) -> Self {
        match (self, other) {
            (None, b) => b.clone(),
            (a, None) => a.clone(),
            (Some(a), Some(b)) => Some(a.op(b)),
        }
    }
    fn valid(&self) -> bool {
        self.as_ref().is_none_or(Ra::valid)
    }
    fn pcore(&self) -> Option<Self> {
        // The outer Some means the lifted core is defined, even when it is empty.
        Some(self.as_ref().and_then(Ra::pcore))
    }
    fn included_in(&self, b: &Self) -> bool {
        match (self, b) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(a), Some(b)) => a == b || a.included_in(b),
        }
    }
}

// ---------------------------------------------------------------------------
// Ex — exclusive token (named exclusive resources: a port, a process slot).
// ex·ex is invalid; a valid token has no core and is not included in itself.
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
        match self {
            Ex::Token => None,
            Ex::Bot => Some(Ex::Bot),
        }
    }
    fn included_in(&self, b: &Self) -> bool {
        matches!(b, Ex::Bot)
    }
}

// ---------------------------------------------------------------------------
// Count — bounded nonnegative counts with exact addition or an invalid overflow.
// Unit 0 is the core. Each pool's capacity is a separate ledger constraint.
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Count {
    Value(u64),
    Invalid,
}

impl Default for Count {
    fn default() -> Self {
        Count::Value(0)
    }
}

impl Count {
    pub fn value(self) -> Option<u64> {
        match self {
            Count::Value(n) => Some(n),
            Count::Invalid => None,
        }
    }
}

impl Ra for Count {
    fn op(&self, other: &Self) -> Self {
        match (self, other) {
            (Count::Value(a), Count::Value(b)) => {
                a.checked_add(*b).map(Count::Value).unwrap_or(Count::Invalid)
            }
            _ => Count::Invalid,
        }
    }
    fn valid(&self) -> bool {
        matches!(self, Count::Value(_))
    }
    fn pcore(&self) -> Option<Self> {
        Some(Count::Value(0))
    }
    fn included_in(&self, b: &Self) -> bool {
        match (self, b) {
            (_, Count::Invalid) => true,
            (Count::Invalid, _) => false,
            (Count::Value(a), Count::Value(b)) => a <= b,
        }
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
//   op    : 精确有理数相加；和 > 1 ⇒ 唯一的吸收非法元
//   valid : 0 < q ≤ 1
//   pcore : 合法正分数无核，非法元自核；没有单位元
//   ≼     : 合法分数上为严格 <；Option<Frac> 的包含关系自然扩为 ≤
// ---------------------------------------------------------------------------
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Frac(Option<BigRational>);

impl Frac {
    pub fn new(num: u64, den: u64) -> Self {
        assert!(den > 0, "denominator must be positive");
        Self::from_ratio(BigRational::new(num.into(), den.into()))
    }
    pub fn one() -> Self {
        Frac::new(1, 1)
    }
    pub fn invalid() -> Self {
        Frac(None)
    }
    fn from_ratio(q: BigRational) -> Self {
        if q.numer() > &BigInt::from(0) && q.numer() <= q.denom() {
            Frac(Some(q))
        } else {
            Frac::invalid()
        }
    }
    /// Exact decimal parts for persistence. Invalid is represented by 0/1.
    pub fn parts(&self) -> (String, String) {
        match &self.0 {
            Some(q) => (q.numer().to_string(), q.denom().to_string()),
            None => ("0".into(), "1".into()),
        }
    }
    /// Restore exact parts without rounding or a fixed integer width.
    pub fn from_parts(num: &str, den: &str) -> Option<Self> {
        let num: BigInt = num.parse().ok()?;
        let den: BigInt = den.parse().ok()?;
        if den <= BigInt::from(0) {
            return None;
        }
        Some(Self::from_ratio(BigRational::new(num, den)))
    }
}

impl Ra for Frac {
    fn op(&self, other: &Self) -> Self {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => Self::from_ratio(a + b),
            _ => Frac::invalid(),
        }
    }
    fn valid(&self) -> bool {
        self.0.is_some()
    }
    fn pcore(&self) -> Option<Self> {
        if self.valid() { None } else { Some(Frac::invalid()) }
    }
    fn included_in(&self, b: &Self) -> bool {
        match (&self.0, &b.0) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(a), Some(b)) => a < b,
        }
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
///   a ⤳ b  ⟺  ∀f ∈ M?. ✓(a·f) ⇒ ✓(b·f)
/// Always checks the empty frame as well. Passing a finite sample is not a proof.
pub fn fpu_holds<A: Ra>(a: &A, b: &A, frames: &[A]) -> bool {
    (!a.valid() || b.valid())
        && frames.iter().all(|f| !a.op(f).valid() || b.op(f).valid())
}
