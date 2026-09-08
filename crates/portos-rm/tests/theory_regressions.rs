//! Concrete counterexamples from the resource-model theory review.

use portos_rm::auth::can_mint;
use portos_rm::ra::{
    Count, Ex, Frac, Ra, fpu_holds, law_assoc, law_comm, law_core_id, law_core_idem, law_core_mono,
    law_valid_op_l,
};
use portos_rm::verbs::{ConsumeGrade, VerbEntry};

#[test]
fn exclusive_update_must_preserve_validity_with_no_frame() {
    assert!(fpu_holds(&Ex::Token, &Ex::Token, &[]));
    assert!(!fpu_holds(&Ex::Token, &Ex::Bot, &[]));
    assert!(!fpu_holds(&Ex::Token, &Ex::Bot, &[Ex::Token, Ex::Bot]));
}

#[test]
fn capacity_gate_rejects_count_overflow() {
    assert!(can_mint(
        &Count::Value(u64::MAX),
        &[Count::Value(u64::MAX - 1)],
        &Count::Value(1)
    ));
    assert!(!can_mint(
        &Count::Value(u64::MAX),
        &[Count::Value(u64::MAX)],
        &Count::Value(1)
    ));
}

#[test]
fn invalid_fraction_cannot_become_valid_by_composition() {
    let overfull = Frac::new(u64::MAX, u64::MAX - 1);
    assert!(!overfull.valid());
    assert!(!overfull.op(&Frac::new(1, 2)).valid());
}

#[test]
fn fraction_composition_is_exact_at_large_denominators() {
    let half = Frac::new(1, 2);
    let tiny = Frac::new(1, u64::MAX);
    assert!(law_assoc(&half, &tiny, &tiny));
    assert!(can_mint(&Frac::new(3, 4), &[half], &tiny));
}

#[test]
fn consuming_request_can_declare_idempotent_retries() {
    let reserve = VerbEntry::consuming(ConsumeGrade::Held).with_flags(true, false);
    assert!(reserve.check_coherent().is_ok());
    assert!(reserve.bears_budget());
    assert!(reserve.blind_replay_safe());
}

fn check_ra_laws<A: Ra>(elems: &[A]) {
    for a in elems {
        assert!(law_core_id(a) && law_core_idem(a));
        for b in elems {
            assert!(law_comm(a, b) && law_valid_op_l(a, b) && law_core_mono(a, b));
            for c in elems {
                assert!(law_assoc(a, b, c), "associativity: {a:?}, {b:?}, {c:?}");
            }
        }
    }
}

#[test]
fn count_overflow_is_absorbing_and_obeys_ra_laws() {
    let elems = [
        Count::Value(0),
        Count::Value(1),
        Count::Value(u64::MAX - 1),
        Count::Value(u64::MAX),
        Count::Invalid,
    ];
    check_ra_laws(&elems);
    assert_eq!(Count::Value(u64::MAX).op(&Count::Value(1)), Count::Invalid);
    assert!(!can_mint(
        &Count::Value(u64::MAX),
        &[Count::Invalid],
        &Count::Value(0)
    ));
}

#[test]
fn option_supplies_units_without_changing_base_inclusion() {
    assert!(!Ex::Token.included_in(&Ex::Token));
    assert!(!Frac::one().included_in(&Frac::one()));
    let ex = [None, Some(Ex::Token), Some(Ex::Bot)];
    let frac = [
        None,
        Some(Frac::new(1, 4)),
        Some(Frac::new(1, 2)),
        Some(Frac::one()),
        Some(Frac::invalid()),
    ];
    check_ra_laws(&ex);
    check_ra_laws(&frac);
    for a in &ex {
        assert_eq!(None.op(a), *a);
        assert!(a.included_in(a));
    }
    for a in &frac {
        assert_eq!(None.op(a), *a);
        assert!(a.included_in(a));
    }
    assert_eq!(Some(Ex::Token).pcore(), Some(None));
}

#[test]
fn static_budget_overflow_is_not_covered_by_a_maximum_pool() {
    use portos_rm::coeffect::Budget;
    let cap = Budget::unit_n("use", u64::MAX);
    assert!(!cap.merge(&Budget::unit("use")).leq(&cap));
    assert!(!Budget::scale(2, &cap).leq(&cap));
}
