//! F4 declarations are checked as a whole before publication.
//!
//! A `CheckedClass` establishes structural coherence, not the truth of an
//! equation or the correctness of a provider's inverse. Holding recovery and
//! operation recovery remain separate contracts. Legacy boolean law summaries
//! have an explicit unspecified scope and never authorize scheduling changes.

use crate::identity::{ClassId, VerbId};
use crate::ledger::RevertGrade;
use crate::protocol::Protocol;
use std::collections::{BTreeMap, BTreeSet};

/// [WG] 消耗类动作的世界档。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConsumeGrade {
    /// 世界变更物化为一笔持有（open/spawn/bind/reserve）：怎么还由类的 ρ 决定（F2）。
    Held,
    /// 可补偿（dequeue 之 nack/requeue）：登记补偿动词。
    Compensable { compensate_with: VerbId },
    /// 外部不可补偿（recv：字节已从对端消失）。
    External,
}

/// [WG] 发射类动作的世界档——没有 Held（emission 不入账本）、没有 Inverse（影子不可 RA-求逆）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EmitGrade {
    Compensable { compensate_with: VerbId },
    External,
}

/// [TRI]+[WG]+[STAGE] 动词性格：三分类内嵌各自合法的世界档——非法组合不可表示。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 不动世界；无世界档。
    Repeatable,
    /// Changes an existing holding inside the boundary. Declaring a compatible
    /// holding grade does not construct a checkpoint or prove a selective inverse.
    Transforming,
    Consuming {
        world: ConsumeGrade,
    },
    /// `amortizable`：一次同意可覆盖一批（standing/↓B）；false ＝ 硬清单，逐次同意。
    Emitting {
        world: EmitGrade,
        amortizable: bool,
    },
}

/// Raw verb declaration. Only a checked class can supply a runtime CheckedVerb.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VerbEntry {
    pub kind: Kind,
    /// Retained as an unscoped provider assertion after checking.
    pub idempotent: bool,
    /// Legacy summary without an operation pair or observation scope. Checking
    /// binds it to this verb with Unspecified scope; it grants no reordering.
    pub commutes: bool,
    /// [EDIT] 降档声明：越界时可改写为同类的哪个动词（F3 Policy.degrade 的来源）。
    pub degrade: Option<VerbId>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VerbError {
    /// 注册期一致性校验未过——不合理条目当场拒。
    Incoherent(&'static str),
    /// 查表未命中（裸动词名或未注册的 (类,动词)）——D1 下这是硬错，不是缺省放行。
    Unknown,
    /// Held 动词所在类未声明 ρ——F2 不知道怎么还，不许先记后补。
    ClassNotDeclared,
    /// 类 ρ 重复声明（注册期固定，不许改）。
    ClassAlreadyDeclared,
    DuplicateVerb,
}

impl VerbEntry {
    /// Immutable-source read declaration. Legacy flags remain unscoped assertions.
    pub fn repeatable() -> Self {
        Self {
            kind: Kind::Repeatable,
            idempotent: true,
            commutes: true,
            degrade: None,
        }
    }
    /// 共享可变源读（endstate §8.1b 第二等，B11）：不动世界、不进预算、盲重放安全，
    /// 但结果依赖与写者的交错——不可交换。
    pub fn repeatable_shared() -> Self {
        Self {
            kind: Kind::Repeatable,
            idempotent: true,
            commutes: false,
            degrade: None,
        }
    }
    pub fn transforming() -> Self {
        Self {
            kind: Kind::Transforming,
            idempotent: false,
            commutes: false,
            degrade: None,
        }
    }
    pub fn consuming(world: ConsumeGrade) -> Self {
        Self {
            kind: Kind::Consuming { world },
            idempotent: false,
            commutes: false,
            degrade: None,
        }
    }
    pub fn emitting(world: EmitGrade, amortizable: bool) -> Self {
        Self {
            kind: Kind::Emitting { world, amortizable },
            idempotent: false,
            commutes: false,
            degrade: None,
        }
    }
    pub fn with_flags(mut self, idempotent: bool, commutes: bool) -> Self {
        self.idempotent = idempotent;
        self.commutes = commutes;
        self
    }
    pub fn degrades_to(mut self, verb: &str) -> Self {
        self.degrade = Some(VerbId::new(verb));
        self
    }

    /// [TRI] 注册期一致性（只断言被引处明确支持的方向；其余组合由类型已排除）：
    ///   · 可重复 ⟹ 幂等（endstate §8.3"盲重放"）。**交换性不由性格蕴含、按动词声明**：
    ///     endstate §8.1b 的"与一切交换"只对**不可变源**读成立（CAS、快照：commutes=true）；
    ///     **共享可变源**读（正被别人写的文件：§8.1b 第二等）不动世界、不进预算、盲重放安全，
    ///     但结果依赖交错，声明 commutes=false——不许被当作可乱序/并行重排的项。
    ///     （B11：v2 曾把 §8.1b 对不可变源的断言推广到全部可重复读，强于来源，且让第二等
    ///     通道无处安放——措辞纪律，与 B1 同类。）
    ///   · 消耗：不加幂等旗标约束；新的消费与同一请求的幂等重试是不同概念。
    ///   · 发射：不加旗标约束（带幂等键的 PUT 是幂等发射，合法）。
    ///   · 界内变换：不加旗标约束（chmod 幂等、append 不幂等，皆合法）；类级约束在 ClassDeclarationDraft::check。
    /// 不主张的：可重复 ⇔ 某代数（可重复观察独占资源不要求资源可复制——那会 overstate）。
    pub fn check_coherent(&self) -> Result<(), VerbError> {
        match self.kind {
            Kind::Repeatable if !self.idempotent => Err(VerbError::Incoherent(
                "repeatable verb must declare idempotence",
            )),
            _ => Ok(()),
        }
    }

    /// [BUDGET] 精化 D9：消耗、发射与界内变换进预算（变换消耗 fuel），可重复读不进（只记标签）。
    pub fn bears_budget(&self) -> bool {
        !matches!(self.kind, Kind::Repeatable)
    }

    /// Declares an enclosed transformation; a recovery implementation is still required.
    pub fn contained(&self) -> bool {
        matches!(self.kind, Kind::Transforming)
    }

    /// [STAGE] 两阶段形状：外部∧不可补偿的发射（endstate §8.5 的字面）。
    /// 可摊销时由准入满足（铸池→四元组→发射），不可摊销时由 F3 扣发满足。
    pub fn staged_shape(&self) -> bool {
        matches!(
            self.kind,
            Kind::Emitting {
                world: EmitGrade::External,
                ..
            }
        )
    }

    /// [STAGE] 逐次扣发（F3 Policy.staged_verbs 的真正来源）⟺ 发射 ∧ 不可摊销。
    /// 注意可补偿但在硬清单上的发射（删除→可从回收站恢复）也逐次同意。
    pub fn withhold(&self) -> bool {
        matches!(
            self.kind,
            Kind::Emitting {
                amortizable: false,
                ..
            }
        )
    }

    /// 需要登记补偿动词的动作（saga 的补偿端）。
    pub fn compensation_verb(&self) -> Option<&VerbId> {
        match &self.kind {
            Kind::Consuming {
                world: ConsumeGrade::Compensable { compensate_with },
            }
            | Kind::Emitting {
                world: EmitGrade::Compensable { compensate_with },
                ..
            } => Some(compensate_with),
            _ => None,
        }
    }

    /// Unchecked legacy summary; it grants no replay or reordering permission.
    pub fn declares_idempotence(&self) -> bool {
        self.idempotent
    }

    /// [EDIT] 严重度序（降档只准沿此序不升）：可重复 < 界内变换 < 消耗 < 发射；
    /// 发射内：可补偿 < 外部；可摊销 < 不可摊销。
    fn severity(&self) -> (u8, u8, u8) {
        match &self.kind {
            Kind::Repeatable => (0, 0, 0),
            Kind::Transforming => (1, 0, 0),
            Kind::Consuming { world } => (
                2,
                if matches!(world, ConsumeGrade::External) {
                    1
                } else {
                    0
                },
                0,
            ),
            Kind::Emitting { world, amortizable } => (
                3,
                if matches!(world, EmitGrade::External) {
                    1
                } else {
                    0
                },
                if *amortizable { 0 } else { 1 },
            ),
        }
    }
}

/// A law's parameters, observations and environmental assumptions must be named.
/// Neither variant is a machine-checked proof of independence.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LawScope {
    Unspecified,
    Declared {
        parameters: String,
        observations: String,
        assumptions: String,
    },
}
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Equation {
    Idempotent {
        operation: VerbId,
    },
    Commutes {
        left: VerbId,
        right: VerbId,
    },
    /// Compatibility metadata lacking a second operation and a scope.
    LegacyCommutationSummary {
        operation: VerbId,
    },
}
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EvidenceSource {
    ProviderAssertion(String),
    ReviewedContract(String),
}
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LawDeclaration {
    pub equation: Equation,
    pub scope: LawScope,
    pub source: EvidenceSource,
}

/// Raw registration input. No runtime lookup or policy projection is available.
#[derive(Clone, Debug)]
pub struct ClassDeclarationDraft {
    pub class: ClassId,
    pub holding_grade: Option<RevertGrade>,
    pub verbs: Vec<(VerbId, VerbEntry)>,
    pub protocol: Option<Protocol>,
    pub laws: Vec<LawDeclaration>,
}
impl ClassDeclarationDraft {
    pub fn new(class: ClassId) -> Self {
        Self {
            class,
            holding_grade: None,
            verbs: Vec::new(),
            protocol: None,
            laws: Vec::new(),
        }
    }
    pub fn check(self) -> Result<CheckedClass, VerbError> {
        if self.class.as_str().is_empty() {
            return Err(VerbError::Incoherent("class name must be nonempty"));
        }
        let mut entries = BTreeMap::new();
        for (verb, entry) in self.verbs {
            if verb.as_str().is_empty() {
                return Err(VerbError::Incoherent("verb name must be nonempty"));
            }
            entry.check_coherent()?;
            if matches!(
                entry.kind,
                Kind::Consuming {
                    world: ConsumeGrade::Held
                } | Kind::Transforming
            ) && self.holding_grade.is_none()
            {
                return Err(VerbError::ClassNotDeclared);
            }
            if entry.contained() && self.holding_grade == Some(RevertGrade::External) {
                return Err(VerbError::Incoherent(
                    "transforming needs a compatible holding recovery declaration",
                ));
            }
            if entries.insert(verb, CheckedVerb { entry }).is_some() {
                return Err(VerbError::DuplicateVerb);
            }
        }
        for e in entries.values() {
            if let Some(d) = &e.entry.degrade {
                let target = entries.get(d).ok_or(VerbError::Incoherent(
                    "degrade target must be in the same class",
                ))?;
                if target.entry.severity() > e.entry.severity() {
                    return Err(VerbError::Incoherent("degrade may only narrow"));
                }
            }
            if let Some(c) = e.entry.compensation_verb() {
                let target = entries.get(c).ok_or(VerbError::Incoherent(
                    "compensation target must be in the same class",
                ))?;
                if target.entry.compensation_verb().is_some() {
                    return Err(VerbError::Incoherent(
                        "compensation must not itself require compensation",
                    ));
                }
            }
        }
        if let Some(p) = &self.protocol {
            for v in p.scoped() {
                if !entries.contains_key(&VerbId::new(v)) {
                    return Err(VerbError::Incoherent(
                        "protocol mentions an unregistered verb",
                    ));
                }
            }
        }
        let mut laws = self.laws;
        for law in &laws {
            let LawScope::Declared {
                parameters,
                observations,
                assumptions,
            } = &law.scope
            else {
                return Err(VerbError::Incoherent(
                    "new equations require an explicit scope and operation pair",
                ));
            };
            let registered = match &law.equation {
                Equation::Idempotent { operation } => entries.contains_key(operation),
                Equation::Commutes { left, right } => {
                    entries.contains_key(left) && entries.contains_key(right)
                }
                Equation::LegacyCommutationSummary { .. } => {
                    return Err(VerbError::Incoherent(
                        "new equations require an explicit scope and operation pair",
                    ));
                }
            };
            if !registered {
                return Err(VerbError::Incoherent(
                    "equation mentions an unregistered operation",
                ));
            }
            if [parameters, observations, assumptions]
                .iter()
                .any(|s| s.trim().is_empty())
            {
                return Err(VerbError::Incoherent(
                    "a scoped equation needs parameters, observations and assumptions",
                ));
            }
            let (EvidenceSource::ProviderAssertion(source)
            | EvidenceSource::ReviewedContract(source)) = &law.source;
            if source.trim().is_empty() {
                return Err(VerbError::Incoherent("equation needs an evidence source"));
            }
        }
        for (verb, e) in &entries {
            for equation in [
                e.entry.idempotent.then(|| Equation::Idempotent {
                    operation: verb.clone(),
                }),
                e.entry
                    .commutes
                    .then(|| Equation::LegacyCommutationSummary {
                        operation: verb.clone(),
                    }),
            ]
            .into_iter()
            .flatten()
            {
                laws.push(LawDeclaration {
                    equation,
                    scope: LawScope::Unspecified,
                    source: EvidenceSource::ProviderAssertion("legacy verb metadata".into()),
                });
            }
        }
        Ok(CheckedClass {
            class: self.class,
            holding_grade: self.holding_grade,
            entries,
            protocol: self.protocol,
            laws,
        })
    }
}

/// An immutable operation from a checked class; cannot be built from one raw row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedVerb {
    entry: VerbEntry,
}
impl CheckedVerb {
    pub fn kind(&self) -> &Kind {
        &self.entry.kind
    }
    pub fn bears_budget(&self) -> bool {
        self.entry.bears_budget()
    }
    pub fn contained(&self) -> bool {
        self.entry.contained()
    }
    pub fn staged_shape(&self) -> bool {
        self.entry.staged_shape()
    }
    pub fn withhold(&self) -> bool {
        self.entry.withhold()
    }
    pub fn compensation_verb(&self) -> Option<&VerbId> {
        self.entry.compensation_verb()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedClass {
    class: ClassId,
    holding_grade: Option<RevertGrade>,
    entries: BTreeMap<VerbId, CheckedVerb>,
    protocol: Option<Protocol>,
    laws: Vec<LawDeclaration>,
}
impl CheckedClass {
    pub fn id(&self) -> &ClassId {
        &self.class
    }
    pub fn holding_grade(&self) -> Option<RevertGrade> {
        self.holding_grade
    }
    pub fn protocol(&self) -> Option<&Protocol> {
        self.protocol.as_ref()
    }
    pub fn lookup(&self, verb: &VerbId) -> Result<&CheckedVerb, VerbError> {
        self.entries.get(verb).ok_or(VerbError::Unknown)
    }
    /// Every operation in these assertions belongs to `self.id()`.
    pub fn laws(&self) -> &[LawDeclaration] {
        &self.laws
    }
    pub fn handler_policy(&self) -> HandlerPolicy {
        let mut p = HandlerPolicy {
            class: self.class.clone(),
            withhold: BTreeSet::new(),
            budget: BTreeSet::new(),
            degrade: BTreeMap::new(),
            compensations: BTreeMap::new(),
            contained: BTreeSet::new(),
            protocol: self.protocol.clone(),
        };
        for (v, e) in &self.entries {
            if e.withhold() {
                p.withhold.insert(v.to_string());
            }
            if e.bears_budget() {
                p.budget.insert(v.to_string());
            }
            if e.contained() {
                p.contained.insert(v.to_string());
            }
            if let Some(d) = &e.entry.degrade {
                p.degrade.insert(v.to_string(), d.to_string());
            }
            if let Some(c) = e.compensation_verb() {
                p.compensations.insert(v.to_string(), c.to_string());
            }
        }
        p
    }
}

/// Read-only projection. Display strings are local to the named class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandlerPolicy {
    class: ClassId,
    withhold: BTreeSet<String>,
    budget: BTreeSet<String>,
    degrade: BTreeMap<String, String>,
    compensations: BTreeMap<String, String>,
    contained: BTreeSet<String>,
    protocol: Option<Protocol>,
}
impl HandlerPolicy {
    pub fn class(&self) -> &ClassId {
        &self.class
    }
    pub fn withhold(&self) -> &BTreeSet<String> {
        &self.withhold
    }
    pub fn budget(&self) -> &BTreeSet<String> {
        &self.budget
    }
    pub fn degrade(&self) -> &BTreeMap<String, String> {
        &self.degrade
    }
    pub fn compensations(&self) -> &BTreeMap<String, String> {
        &self.compensations
    }
    pub fn contained(&self) -> &BTreeSet<String> {
        &self.contained
    }
    pub fn protocol(&self) -> Option<&Protocol> {
        self.protocol.as_ref()
    }
}

/// Runtime registry: admission accepts only an entire checked class.
#[derive(Default, Clone, Debug)]
pub struct VerbTable {
    classes: BTreeMap<ClassId, CheckedClass>,
}
impl VerbTable {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&mut self, class: CheckedClass) -> Result<(), VerbError> {
        if self.classes.contains_key(class.id()) {
            return Err(VerbError::ClassAlreadyDeclared);
        }
        self.classes.insert(class.id().clone(), class);
        Ok(())
    }
    pub fn class(&self, class: &ClassId) -> Result<&CheckedClass, VerbError> {
        self.classes.get(class).ok_or(VerbError::Unknown)
    }
    pub fn lookup(&self, class: &ClassId, verb: &VerbId) -> Result<&CheckedVerb, VerbError> {
        self.class(class)?.lookup(verb)
    }
    pub fn derive_holding_grade(&self, class: &ClassId) -> Option<RevertGrade> {
        self.classes
            .get(class)
            .and_then(CheckedClass::holding_grade)
    }
    pub fn derive_handler_policy(&self, class: &ClassId) -> Result<HandlerPolicy, VerbError> {
        Ok(self.class(class)?.handler_policy())
    }
    pub fn len(&self) -> usize {
        self.classes.values().map(|c| c.entries.len()).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// ```compile_fail
/// use portos_rm::{verbs::{VerbTable, ClassDeclarationDraft}, identity::ClassId};
/// let draft = ClassDeclarationDraft::new(ClassId::new("device"));
/// VerbTable::new().insert(draft);
/// ```
/// ```compile_fail
/// use portos_rm::{verbs::CheckedClass, identity::VerbId};
/// fn change(class: &mut CheckedClass) { class.entries.remove(&VerbId::new("read")); }
/// ```
/// ```compile_fail
/// use portos_rm::verbs::HandlerPolicy;
/// fn bypass(policy: HandlerPolicy) { policy.withhold().clear(); }
/// ```
const _: () = ();
