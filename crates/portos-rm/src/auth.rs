//! Auth discipline — the issuer's ledger view of one (class, instance),
//! adopted from the Iris Auth construction (appendix-4.5; theory-spec §2.2):
//!
//!   ● capacity  — the authoritative element held by the kernel (the issuer);
//!   ◯ fragment  — one holding's claim;
//!   validity    :  ✓(● a · ◯ b)  ⟺  b ≼ a  ∧  ✓ a.
//!
//! Two theorem-consequences drive the shape of this module (freeze record F1):
//!
//! 1. **No cancellativity ⇒ rows are the truth.** RAs have no subtraction, so the
//!    composed outstanding value cannot be "decremented" on release. Therefore the
//!    ledger stores one fragment per holding row; release deletes a row; the
//!    composed value is only ever *recomputed* (fold), never mutated. Any cached
//!    total is an optimization to be reconciled against the fold.
//!
//! 2. **Mint is not frame-preserving in an open world ⇒ the issuer gate.** Growing
//!    a fragment out of thin air fails FPU against unseen frames (test
//!    `mint_not_fpu_in_open_world_hence_issuer`). It becomes legal exactly because
//!    the issuer sees *all* frames (closed world of the Auth ledger) and checks the
//!    full composition against ● capacity. "发放方" is this closed-world position.

use crate::ra::Ra;

/// Fold a set of live fragments. `None` = no outstanding claims at all
/// (distinct from a unit, which some algebras — Ex — do not have).
pub fn compose<A: Ra>(frags: &[A]) -> Option<A> {
    let mut it = frags.iter();
    let first = it.next()?.clone();
    Some(it.fold(first, |acc, f| acc.op(f)))
}

/// The Auth validity law:  ✓(● cap · ◯ outstanding).
pub fn auth_valid<A: Ra>(capacity: &A, outstanding: &Option<A>) -> bool {
    if !capacity.valid() {
        return false;
    }
    match outstanding {
        None => true,
        Some(b) => b.valid() && b.included_in(capacity),
    }
}

/// The issuer gate for granting `want` given the live fragments:
/// the *full* new composition must be a valid fragment included in ● capacity.
/// This is the local update (● a, ◯ b) ⤳ (● a, ◯ b·want) checked in the closed world.
pub fn can_mint<A: Ra>(capacity: &A, live: &[A], want: &A) -> bool {
    let mut all: Vec<A> = live.to_vec();
    all.push(want.clone());
    auth_valid(capacity, &compose(&all))
}
