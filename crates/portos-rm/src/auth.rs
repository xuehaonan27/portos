//! Capacity predicates for the complete ledger of one (class, instance).
//!
//! The validity predicate can be interpreted in Iris Auth(Option<A>): lifting A
//! supplies the unit missing from Ex/Frac. This module does not implement the full
//! Auth algebra or prove a local update for grant. A central capacity check and
//! an update preserving every possible external frame are different obligations.
//!
//! The ledger keeps each holding to track ownership, leases and release. This is
//! a representation choice: no generic subtraction exists for arbitrary RAs, but
//! particular instances may support it. See `.dev/design/spec.md` §2.2–§3.2.

use crate::ra::Ra;

/// Fold a set of live fragments. `None` = no outstanding claims at all
/// (the unit of Option<A>, even when the base algebra A has no unit).
pub fn compose<A: Ra>(frags: &[A]) -> Option<A> {
    let mut it = frags.iter();
    let first = it.next()?.clone();
    Some(it.fold(first, |acc, f| acc.op(f)))
}

/// Capacity validity, interpreted as ✓(● Some(cap) · ◯ outstanding) in Auth(Option<A>).
pub fn auth_valid<A: Ra>(capacity: &A, outstanding: &Option<A>) -> bool {
    if !capacity.valid() {
        return false;
    }
    outstanding.valid() && outstanding.included_in(&Some(capacity.clone()))
}

/// The issuer gate for granting `want` given the live fragments:
/// the *full* new composition must be a valid fragment included in ● capacity.
/// This preserves the complete ledger's capacity invariant; it is not an Iris FPU
/// or local-update check. Callers must supply all live claims in the pool.
pub fn can_mint<A: Ra>(capacity: &A, live: &[A], want: &A) -> bool {
    let mut all: Vec<A> = live.to_vec();
    all.push(want.clone());
    auth_valid(capacity, &compose(&all))
}
