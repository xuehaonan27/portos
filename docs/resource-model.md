# The PortOS Resource Model

This document describes how PortOS models, accounts for, and governs
resources. The kernel uses its ledger and resource facilities.

## Position and discipline

Resource management answers one question per engineering decision: what is
the theoretical basis, where was it copied from, and which laws must become
tests. The selection criterion is correctness and fit, not novelty. Buy
before build, copy before derive. Claims rest on named sources: the Iris
resource algebras, algebraic effect handlers (Plotkin–Pretnar), runtime
enforcement automata (Ligatti–Bauer–Walker), and coeffect calculi
(Petricek–Orchard–Mycroft). The wording of a claim must never be stronger
than the theorem it cites.

## Boundary, quadrants, and shadows

An operation is a **w-effect** iff its interpreter sits outside the
mediation domain.A context requirement is a **w-coeffect** iff its satisfier
sits outside. This follows the algebraic effect reading: an operation has 
only a signature and equations, meaning is given by the handler. The same 
verb can be in-boundary under one handler and a world effect under another.
Switching handlers switches models, and therefore switches meaning.

The kernel can only govern **shadows**: a w-effect casts an acquisition
record in the resource ledger, a w-coeffect casts a taint-labeled artifact
in the data plane. The **mediation-granularity lemma** fixes the cost:
mandatory accounting granularity equals mandatory mediation granularity,
and default-deny is what makes accounting granular. RDMA is the negative
example — once a memory region carries remote access and the rkey is out,
the peer reads and writes without the local CPU; the mediation point exists
only at the moment access is granted.

## The resource model

A resource class is declared as **R = (Σ, E, M, T, W)**, the four modules:
shared structure M, interface theory Σ/E, time & failure T, and world
relation W. Filling in the declaration is not itself a proof of runtime
semantics, module completeness, or a general recovery theorem. The
declarations connect to runtime semantics through the state-transition and
resource-interpretation skeleton described below.

### M: shared structure and the central ledger

The mathematics uses Iris resource algebras (RA). Composition is
associative and commutative, validity is downward closed, core is a partial
function with the usual axioms, and inclusion is `a ≼ b ≜ ∃c. b = a·c`.
Cancellation is not an axiom. There is no generic remainder operation, which
neither forbids cancellative instances nor partial refunds per resource
class. Frame-preserving updates (FPU), the `Auth` construction, and local
update follow the Iris definitions. Local update is a sufficient rule, not
a criterion for all Auth FPUs.

The runtime choice is a **central ledger**: the issuer keeps the complete
live holding records of each pool, and `grant` composes all live fragments
with the request in one ledger transaction, checking validity and the
capacity bound — the invariant is `outstanding ≼ capacity`. A full-ledger
check is not thereby an Iris FPU: at fixed capacity 10, moving from ●10·◯3
to ●10·◯4 passes the ledger check while breaking the frame ◯7 that was
compatible with the old value. The issuer is the pool's registrar,
serializer, and invariant keeper, not an omniscient frame knower. Closing a
pool settles its holdings first and only then sets capacity to zero.

Classes without a unit element (Ex, Frac) gain an empty holding via the
`Option` construction, giving `Auth(Option(M))` as the capacity-validity
reading; the runtime does not represent the full Auth resource and its
holding distribution.

**Row-based accounting** is a representation choice: keeping every fragment
as a row supports generic release, attribution, leases, and reconciliation.
Release writes a tombstone; aggregates can always be recomputed from live
rows, and any cached or incremental value must agree with them. Refund and
spend-retention policy is fixed by the lifecycle contract, not by the RA
axioms.

**Algebra instances**: Ex (exclusive plus an invalid element), ℕ (addition,
core 0), GSet (set union, self-core), Ranges (disjoint union of intervals,
unit ∅, overlap is invalid), Frac (exact rational addition on (0,1] with an
absorbing invalid element above 1). The machine `Count` is `0..=u64::MAX`
plus an explicit invalid element — overflow must be invalid, never saturate
into a still-grantable amount. Frac numerators and denominators are exact;
truncation must not break associativity or re-validate invalid fragments.

**Generational handles**: interpreting fragments onto real resources
requires stable denotation — recyclable names use `(name, generation)` or
substrate-native handles. RA composition constrains fragment compatibility
only; it implies nothing about the independence of operations, results, or
inverses.

### Σ/E: interface theory

Σ is the operation signature, E the equation set (parallel or out-of-order
reclamation when declared independence premises hold, blind replay for
idempotent operations, protocol orderings, in-flight cancellation). A
resource class's Σ/E is an equational theory and its manager is a handler —
a model — of that theory. Since handler correctness is undecidable in
general, managers are certified in tiers (correct-by-construction SDK,
protocol static verification, white-box proof, property-based testing as
the floor) rather than verified by the kernel.

**Verb kinds.** Every verb of a class carries one of four kinds:

| Kind | World tier | Budgeted | Withheld | Where the inverse lives |
|---|---|---|---|---|
| Repeatable (reads that do not move the world) | none | no | no | not needed |
| Transforming (in-boundary transformations: exec, file writes, state transitions) | none | yes (fuel) | no | per-operation recovery contract (snapshot restore, state reset); registering the class at ρ ≠ External is a prerequisite, not a proof |
| Consuming (queue pop, acquire) | Held / Compensable / External | yes | no | Held → the class ρ; Compensable → a compensation verb; External → none |
| Emitting (w-effect) | Compensable / External (Held and Inverse are excluded by the type) | yes | only when non-amortizable (hard list) | Compensable → a compensation verb; External → none |

Coherence direction: repeatable implies idempotent. New consumption and a
retry of the same consumption request are distinct — `reserve(k, q)` may
create the holding on first call and return the same holding on retry.
Idempotence does not guarantee identical results across reads; repeatable
reads costing no action budget does not mean bandwidth or CPU metering is
zero. Commutativity is declared per verb with its operand pair, parameter,
state conditions, and observation scope — never inferred from the kind or
from RA commutativity. **Protocols**: a class may declare a deterministic
protocol automaton over its own verbs; protocol order is a safety property,
precisely enforceable by truncation.

### T: time and failure

Leases (Gray–Cheriton), region nesting, reconciliation modes (substrate
query, write-ahead log, accept-the-loss), and decay/revocation
notification. Expiry, cleanup-requested, and cleanup-completed are distinct
events; unknown or failed outcomes remain cleanup obligations. A child with
its own unexpired lease keeps a conservatively waiting parent alive — a
lease is a promise to its holder. If children can renew indefinitely, no
unconditional physical reclamation deadline can be promised for the parent.
The time extension is the weakest module of the model.

### W: world relation and reversibility tiers

ρ is a recovery contract over a stated state domain, operation set, and
observation surface; a class declares a tier as a summary, and each
guarantee must still name its recovery boundary, permitted interleavings,
revertible operations, and the observational equivalence that related
operations must respect. Closing a newly created object establishes an
acquire/release recovery relation only — it does not automatically invert
prior transformations of existing objects. External means no recovery
guarantee; it can still require closing channels, releasing occupancy, and
recording residue. Comparing tiers across layers requires an abstraction
mapping from the substrate state domain into the class state domain first;
equivalences over different state domains cannot be compared directly.

Holding-recovery tiers are class attributes; action-compensation tiers are
verb attributes. A page class can hold an Inverse guarantee on
acquire/close while its `click` is External.

The **⊥ tier** is the conservative admission tier: Σ = {opaque release},
E = ∅, M = Ex, T = mandatory lease, W = External. It provides an
accountable cleanup entry point for anything not yet modeled.

### Connecting the modules

The runtime skeleton C = (S, H, L, J) tracks actual resource state,
holdings, lifecycle relations, and outstanding cleanup obligations;
operations are transitions labeled with results, usage, and observations,
tied to fragments by a resource interpretation. The general exit guarantee
is: release only holdings within the cleanup scope, preserve holdings that
must survive outside it, and keep recording uncompleted cleanup; races
already lost and observations already published stand as history. Stronger
observational recovery additionally requires correctness of operations and
their inverses, independence, and equivalence compatibility. In a pool of
capacity 1, A's occupancy makes B's acquire fail, and A's release does not
retract B's failure observation — RA validity alone does not supply
independence premises.

A resource instance, one subject's holding of it, and the scope responsible
for cleanup are interpreted separately. `transfer` changes the holder while
keeping the `parent` existence dependency: a holding still attached under
its old provider's tree is reclaimed with that tree. Transfer preserves the
aggregate value; surviving across providers requires an explicit handover
of dependency, cleanup duty, and recovery boundary.

## Metering: budgets, quotas, labels

**The metering contract is the coeffect scalar** (Petricek–Orchard–Mycroft,
Definition 1): `(C, ~, ⊕, use, ign, ≤)` where `(C,~,use)` and `(C,⊕,ign)`
are monoids, `(C,≤)` is a preorder, and two-sided distributivity holds. The
contract does not require commutativity or absorption, and code paths must
not depend on them. `~` is composition/scaling (the application rule: a
function using its argument s times where the argument costs t yields
`s ~ t`); `⊕` is merge within one context (sequential statements sharing a
context). Reading `~` as sequential composition multiplies statement
budgets — wrong. Two instances are copied verbatim: **flat** (implicit
parameters; capability sets) and **counting** (bounded reuse; budgets).
Loop bounds multiply layer by layer, exactly the application rule at
runtime: `scale(N, body) = numeral(N) ~ body`.

**A budget is a vector per effect class, not a number** (`Budget =
Counting^K`, finite support, absent class = bound 0). It provides semimodule
operations only: with K open-ended, the unit of `~` has no finite
representation, so `Budget` does not claim to be a scalar. A manifest's
`requires` is two flat scalar elements (caps, deps) plus one counting
vector (uses). Consent is the downset ↓B — WYSIWYS signs an upper bound,
and consent monotonicity `B′ ≤ B ⇒ ↓B′ ⊆ ↓B` is preorder transitivity. The
**effect row** caps what a mount point may use: actual availability is
position ceiling ∩ subject grant.

Budgets and holdings share the additive structure (ℕ,+,≤) but differ in
lifecycle: occupancy can be re-released, consumed usage does not vanish
when the consumer exits, and whether unused reservation is refunded is a
settlement-contract question. Attachments charge the full declared budget
per firing and refund nothing at the end — a conservative policy that
neither the RA nor the scalar contract mandates. Machine overflow is an
explicit rejection, never a saturated accept.

**Labels (taint)**: the running scheme is dynamic and coarse-grained — a
Denning lattice, join propagation, checks at egress and first consumption,
session staining. The static fine-grained blueprint (graded IFC, two-point
security semiring, noninterference) is verified reading, to be copied when
fine-grained demand appears.

## Governance: monitors, plans, audit

**Enforcement tiers.** Under precise enforcement (step-lockstep, legal
input untouched), all four automaton classes enforce only safety properties
(Schneider's bound). With a withholding buffer, effective enforcement rises
to renewal properties (plus an eager-insertion corner): staged emission —
reserve, consent, commit — has transaction shape, and transactions are the
canonical non-safety renewal property. Protocol orderings are safety; a
truncation tier suffices. The equivalence chosen by the enforcer can
trivialize "enforceable," so the equivalence must be fixed by the resource
class declaration — never picked by the enforcer after the fact.

**Three remedies** map onto the automaton classes: withhold = suppression
plus post-approval insertion; attenuate = edit (rewrite only through
declared degrade equivalences, then re-check the same sink predicate);
confine = rewrite to a stand-in (zero real budget, no consent). The power
of suppression comes from feigning acceptance — the deceived party is the
target, not the world — and its price is that withheld emissions could be
suppressed forever. The engineering bound is the consent tuple's **ttl**:
both suspension shapes (awaiting approval, paused for more budget) are
capped by the original consent's ttl; expiry discards the buffer and rolls
back the segment's holdings.

**Plans.** Rice's theorem pushes the plan language to be total; admission
is a static sum over the scalar/vector contract, O(|plan|). The monitor
re-checks per effect at runtime — the two-level arrangement of a static
type checker plus runtime dynamics. Withheld verbs reorder world order, so
admission also checks a static world-order projection (sound, exact for
branch-free plans), and the runtime steps the protocol automaton at the
emission point and pre-runs the whole batch at approval.

**Segment = transaction.** A run segment commits only on completion:
commit transfers the segment's holdings to the fiber; any non-commit
terminal state (fail-stop, truncated, aborted) rolls back holdings that
were not promoted early; `promote` commits one holding ahead of time and
survives a later abort. Rollback is triggered by the monitor itself — the
same single-path shape as crash-only teardown. Prefix delivery holds only
for effects already emitted; segment holdings roll back even on truncation,
and truncation is never silent — the dropped tail is explicit in the trace.

**Audit.** Noninterference is a hyperproperty no single-trace monitor can
enforce; k-safety properties can be monitored over k traces, and
deterministic replay supplies the second trace — the audit side of the
house.

## The six decisions

**F1 — holdings ledger and generational handles.** Three tables (resource
class, authoritative pool, holding row) with a live partial index. Grant
composes all live fragments of a pool with the request in one transaction;
release is idempotent and writes a tombstone; lease expiry is release by
sweeper; a lease of `None` binds the child to the parent's lifetime, and
sweeps take the closure over such children, children before parents.
Cross-subject parenting is allowed only along the instantiation relation
(kernel-recorded at spawn, never self-declared, not transitive).
Generational handles make stale references unforgeable (ABA rejected).
Errors are classified: Conflict, ForgedHandle, StaleGeneration,
TeardownOrder, DoubleRelease, ParentAuthority.

**F2 — teardown.** A saga log with write-ahead discipline: plan waves over
the ownership tree (ordering constraints live only on ownership edges, read
as existence dependencies of holdings), execute with in-wave shuffling —
pairwise-independent inverses may run in any order. Crash-only: graceful
shutdown and crash recovery are the same function; the journal lets it
resume, and idempotent release makes blind replay safe. Exactly-once =
at-least-once + key deduplication at the peer. Failed branches are
isolated, never wedging their parents. Teardown cascades across subjects
along the ownership closure, children before parents; holdings attached
outside the tree are untouched.

**F3 — monitor and remedies.** The admission gate plus an edit-automaton
supervisor. Without withheld verbs it degenerates to a truncation automaton
enforcing safety precisely; the withholding buffer lifts it to renewal.
Consent mints a budget pool (an authoritative capacity row); spending
grants fragment rows; the gate is the issuer gate. The three modes:
strict (fail-stop, deliver the emitted prefix, roll back segment
holdings), truncate (never silent), escalate (pause, then resume exactly
with fresh incremental consent). WYSIWYS consent is an O(1) decidable
gate: hash equality, fresh nonce, live ttl. The sink predicate ranges over
(verb, target) jointly, sink re-check precedes withholding, and anything
entering the buffer is already legal but for consent — so approved batches
emit blindly, exactly once, in order.

**F4 — the verb truth table.** Keyed by (class, verb); projections are per
class only — a bare verb name is unanswerable, because the same verb under
two handlers has different w-status. Holding ρ is declared once per class,
immutably; verb kinds embed their tiers so that incoherent combinations
(Emitting with an inverse, tiered Repeatable) are unrepresentable.
Withholding applies iff Emitting and non-amortizable; staged shape iff
Emitting and External. Degrade declarations live in the table and may only
narrow. The table is the shared source projected into teardown (holding
tiers) and the monitor (withhold set, degrade table, budget set,
compensation registry, protocols).

**F5 — manifest `requires`.** Two flat elements (caps, deps) and one
per-effect-class counting vector (uses), compared componentwise. Mount
admission is set inclusion (caps ⊆ offers, deps ⊆ provides); plan admission
is the static demand sum against the consent downset. The occurrence sum
over-approximates the path maximum — admission is tight rather than
permissive. Whether a verb counts at all is decided by the truth table:
repeatable verbs cost zero.

**F6 — protocols, intervals, fractions, metering.** The `Transforming`
kind fills the c-effect quadrant; classes hosting it must declare ρ ≠
External, and segment rollback restores each touched holding exactly once
through the class's keyed restore. Ranges and Frac join the algebra
library behind the same issuer gate (disjoint windows grant in parallel,
overlaps refuse; three thirds compose to exactly one, a fourth refuses).
Protocols are safety automata stepped at emission and rehearsed at
approval; static reachability equals path enumeration, and the world-order
projection stays sound over branches. Usage-based pricing connects through
the same gate: static declared upper bounds at admission, metered actuals
at runtime.

## The bestiary

The bestiary is the model's case-filing instrument: place a real resource
into the four-module questionnaire, and where placement is awkward, record
which existing mathematical object is missing an element — never "add
another column."

- **socket**: reading a plain file looks like a pure coeffect; reading a
  socket consumes and is observed by the peer (flow-control windows,
  backpressure). The lesson split channels into four grades by read nature
  (immutable source / shared mutable source / queue stream / active
  source) and replaced "everything is a file" with "everything is a
  resource class with a declared algebra."
- **Workspace (microVM / container)**: the textbook case of reifying a
  piece of the world inside the boundary. VM instances are Ex, quotas are
  Count, shared read-only images are GSet; snapshots are holdings (they
  occupy disk); exec and file writes forced the `Transforming` kind into
  existence — filing them as Emitting would demand consent per call with
  no rollback, filing them as Consuming/Held would tear down holdings that
  do not exist.
- **RDMA**: hardware bypass turns "where is the mediation point" into a
  design decision. It loaded three library extensions (Ranges for memory
  windows, Frac for shared QP reads, the protocol column for the QP state
  machine) and one interface split: ibverbs fuses registering memory and
  granting remote access into one call, while the driver interface splits
  `reg_mr` (Held) from `grant_remote_access` (Emitting, External, hard
  list) — the mediation point exists because the interface creates it.

Two very different resources produced no new axis beyond the four modules:
new situations land inside known mathematical objects.

## Open problems

The compensation metatheory (reconstruction under coarse ρ); formalizing
the time extension; k-safety audit replay; escrow overdraft bounds for the
distributed tier; the fifth concern "location/QoS" (RDMA is the first
candidate, registered but not modeled); graded weights for usage-based
pricing.
