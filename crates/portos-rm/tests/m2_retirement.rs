use portos_rm::{cleanup::*, identity::*, ledger::*, ra::Ex, registry::*, time::*};
fn fixture() -> (Ledger, HoldingHandle) {
    let mut l = Ledger::new();
    let class = l
        .register_class(ClassDecl {
            cleanup: CleanupPolicy::Managed(CleanupKind::Provider),
            class_id: "provider".into(),
            algebra: AlgebraTag::Exclusive,
            release_idempotent: true,
            lease_duration: None,
            revert_grade: RevertGrade::External,
        })
        .unwrap()
        .for_algebra::<Ex>()
        .unwrap();
    let pool = l
        .create_pool(&class, "item".into(), Capacity::new(Ex::Token).unwrap())
        .unwrap();
    let h = l
        .grant_with_cleanup(&pool, request(), CleanupTarget::Provider)
        .unwrap();
    (l, h)
}
fn request() -> GrantRequest<Ex> {
    GrantRequest {
        owner: "owner".into(),
        claim: Claim::new(Ex::Token).unwrap(),
        generation: "g".into(),
        parent: None,
        lease: LeaseRequest::Unbounded,
        now: Timestamp::ZERO,
    }
}
fn worker() -> HostWitness {
    HostWitness::new(
        ProcessWitness::new(123, 1, "boot".into()).unwrap(),
        "worker".into(),
    )
    .unwrap()
}

#[test]
fn retirement_retains_capacity_and_rejects_use_until_exact_attempt_confirmation() {
    let (mut l, h) = fixture();
    let pool = l
        .pool::<Ex>(&ResourceKey::new("provider".into(), "item".into()))
        .unwrap();
    assert_eq!(
        l.release(&h, Timestamp::ZERO),
        Err(LedgerError::CleanupRequired)
    );
    let HoldingState::Retiring(id) = l
        .request_retirement(
            &h,
            CleanupKey::new("stable".into()).unwrap(),
            Timestamp::ZERO,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(l.active().count(), 0);
    assert_eq!(l.occupying().count(), 1);
    assert!(
        l.grant_with_cleanup(&pool, request(), CleanupTarget::Provider)
            .is_err()
    );
    assert!(
        l.renew(&h, LeaseRequest::Unbounded, Timestamp::ZERO)
            .is_err()
    );
    assert!(l.transfer(&h, &"owner".into(), "other".into()).is_err());
    let old = l
        .claim_cleanup(id, worker(), Timestamp::ZERO)
        .unwrap()
        .unwrap();
    assert!(
        l.claim_cleanup(id, worker(), Timestamp::ZERO)
            .unwrap()
            .is_none()
    );
    l.finish_cleanup(
        &old,
        CleanupOutcome::Unknown("response lost".into()),
        Timestamp::ZERO,
    )
    .unwrap();
    l.request_retirement(
        &h,
        CleanupKey::new("ignored replacement key".into()).unwrap(),
        Timestamp::ZERO,
    )
    .unwrap();
    let retry = l
        .claim_cleanup(id, worker(), Timestamp::ZERO)
        .unwrap()
        .unwrap();
    assert_eq!(retry.task().key.as_str(), "stable");
    assert_eq!(
        l.finish_cleanup(&old, CleanupOutcome::Confirmed, Timestamp::ZERO),
        Err(LedgerError::StaleAttempt)
    );
    assert_eq!(l.occupying().count(), 1);
    l.finish_cleanup(&retry, CleanupOutcome::AlreadyAbsent, Timestamp::ZERO)
        .unwrap();
    assert_eq!(l.occupying().count(), 0);
    let replacement = l
        .grant_with_cleanup(&pool, request(), CleanupTarget::Provider)
        .unwrap();
    assert_ne!(h.id(), replacement.id());
    assert!(
        l.finish_cleanup(&retry, CleanupOutcome::Confirmed, Timestamp::ZERO)
            .is_err()
    );
    assert!(l.resolve(&replacement).unwrap().state.is_active());
    l.invariant().unwrap();
}

#[test]
fn retirement_cannot_accept_a_new_child_or_be_reconstructed_without_its_task() {
    let (mut l, h) = fixture();
    l.request_retirement(
        &h,
        CleanupKey::new("stable".into()).unwrap(),
        Timestamp::ZERO,
    )
    .unwrap();
    let class = l.registered_class::<Ex>(&"provider".into()).unwrap();
    let child = l
        .create_pool(&class, "child".into(), Capacity::new(Ex::Token).unwrap())
        .unwrap();
    let mut r = request();
    r.parent = Some(h);
    assert!(
        l.grant_with_cleanup(&child, r, CleanupTarget::Provider)
            .is_err()
    );
    let snapshot = l.snapshot();
    for corrupt in [
        |s: &mut LedgerSnapshot| s.cleanups.clear(),
        |s: &mut LedgerSnapshot| s.holdings[0].state = HoldingState::Active,
        |s: &mut LedgerSnapshot| s.cleanups[0].state = CleanupState::Done(Completion::Confirmed),
        |s: &mut LedgerSnapshot| {
            s.cleanups[0].holding =
                HoldingHandle::new(HoldingId::try_from(0u64).unwrap(), "different".into())
        },
        |s: &mut LedgerSnapshot| s.cleanups[0].state = CleanupState::Running { worker: worker() },
        |s: &mut LedgerSnapshot| s.holdings[0].target = CleanupTarget::AccountingOnly,
    ] {
        let mut bad = snapshot.clone();
        corrupt(&mut bad);
        assert!(LedgerBuilder::new(bad).finish().is_err());
    }
    assert!(LedgerBuilder::new(snapshot).finish().is_ok());
}

#[test]
fn explicit_retirement_closes_parent_bound_children_but_preserves_independent_leases() {
    let (mut l, parent) = fixture();
    let class = l.registered_class::<Ex>(&"provider".into()).unwrap();
    let pool = l
        .create_pool(&class, "bound".into(), Capacity::new(Ex::Token).unwrap())
        .unwrap();
    let mut r = request();
    r.parent = Some(parent.clone());
    r.lease = LeaseRequest::ParentBound;
    let bound = l
        .grant_with_cleanup(&pool, r, CleanupTarget::Provider)
        .unwrap();
    let pool = l
        .create_pool(
            &class,
            "independent".into(),
            Capacity::new(Ex::Token).unwrap(),
        )
        .unwrap();
    let mut r = request();
    r.parent = Some(parent.clone());
    r.lease = LeaseRequest::Until(Timestamp::try_from(100u64).unwrap());
    let independent = l
        .grant_with_cleanup(&pool, r, CleanupTarget::Provider)
        .unwrap();
    l.request_retirement(
        &parent,
        CleanupKey::new("retire-parent".into()).unwrap(),
        Timestamp::ZERO,
    )
    .unwrap();
    assert!(matches!(
        l.resolve(&bound).unwrap().state,
        HoldingState::Retiring(_)
    ));
    assert!(l.resolve(&independent).unwrap().state.is_active());
    assert_eq!(l.cleanup_tasks().count(), 2);
    // An independent child may keep its lease, but cannot start inheriting a
    // lifetime that has already ended. Rejection must leave the domain intact.
    let before = l.snapshot();
    assert_eq!(
        l.renew(&independent, LeaseRequest::ParentBound, Timestamp::ZERO),
        Err(LedgerError::InvalidLease)
    );
    assert_eq!(l.snapshot(), before);
    l.invariant().unwrap();
}

#[test]
fn an_exclusive_target_cannot_be_admitted_again_under_a_different_pool_name() {
    let mut l = Ledger::new();
    let class = l
        .register_class(ClassDecl {
            cleanup: CleanupPolicy::Managed(CleanupKind::Port),
            class_id: "port".into(),
            algebra: AlgebraTag::Exclusive,
            release_idempotent: true,
            lease_duration: None,
            revert_grade: RevertGrade::Inverse,
        })
        .unwrap()
        .for_algebra::<Ex>()
        .unwrap();
    let first = l
        .create_pool(&class, "first".into(), Capacity::new(Ex::Token).unwrap())
        .unwrap();
    let other = l
        .create_pool(&class, "alias".into(), Capacity::new(Ex::Token).unwrap())
        .unwrap();
    let target = CleanupTarget::Port(PortWitness::new(Transport::Tcp, 1234).unwrap());
    let h = l
        .grant_with_cleanup(&first, request(), target.clone())
        .unwrap();
    l.request_retirement(
        &h,
        CleanupKey::new("retiring".into()).unwrap(),
        Timestamp::ZERO,
    )
    .unwrap();
    let before = l.snapshot();
    assert_eq!(
        l.grant_with_cleanup(&other, request(), target),
        Err(LedgerError::Conflict)
    );
    assert_eq!(l.snapshot(), before);
}
