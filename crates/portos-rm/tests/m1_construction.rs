use portos_rm::identity::*;
use portos_rm::ledger::*;
use portos_rm::ra::{Count, Ex, Frac, Ranges};
use portos_rm::registry::*;
use portos_rm::time::*;

fn count_pool(l: &mut Ledger, name: &str, n: u64) -> PoolRef<Count> {
    let class = l
        .register_class(ClassDecl {
            cleanup: portos_rm::cleanup::CleanupPolicy::AccountingOnly,
            class_id: ClassId::new("quota"),
            algebra: AlgebraTag::Counted,
            release_idempotent: true,
            lease_duration: None,
            revert_grade: RevertGrade::Inverse,
        })
        .unwrap()
        .for_algebra()
        .unwrap();
    l.create_pool(
        &class,
        InstanceId::new(name),
        Capacity::new(Count::Value(n)).unwrap(),
    )
    .unwrap()
}
fn request(n: u64) -> GrantRequest<Count> {
    GrantRequest {
        owner: SubjectId::new("用户"),
        claim: Claim::new(Count::Value(n)).unwrap(),
        generation: Generation::new("世代"),
        parent: None,
        lease: LeaseRequest::UseClassDefault,
        now: Timestamp::ZERO,
    }
}

#[test]
fn rejected_transitions_preserve_the_aggregate_and_bindings() {
    let mut l = Ledger::new();
    let pool = count_pool(&mut l, "额度", 5);
    l.grant(&pool, request(3)).unwrap();
    let before = l.snapshot();
    assert_eq!(
        l.resize_pool(&pool, Capacity::new(Count::Value(2)).unwrap()),
        Err(LedgerError::Conflict)
    );
    assert_eq!(l.snapshot(), before);
    let mut conflict = before.classes[0].clone();
    conflict.release_idempotent = false;
    assert!(matches!(
        l.register_class(conflict),
        Err(LedgerError::ClassConflict)
    ));
    let class = l
        .register_class(before.classes[0].clone())
        .unwrap()
        .for_algebra::<Count>()
        .unwrap();
    assert!(matches!(
        l.create_pool(
            &class,
            InstanceId::new("额度"),
            Capacity::new(Count::Value(9)).unwrap()
        ),
        Err(LedgerError::PoolExists)
    ));
    assert_eq!(l.snapshot(), before);
    let mut other = l.clone();
    assert!(matches!(
        other.grant(&pool, request(1)),
        Err(LedgerError::ForeignReference)
    ));
    assert!(matches!(
        other.create_pool(
            &class,
            InstanceId::new("new"),
            Capacity::new(Count::Value(1)).unwrap()
        ),
        Err(LedgerError::ForeignReference)
    ));
    l.settle_and_zero_pool(&pool, Timestamp::ZERO).unwrap();
    assert!(matches!(
        l.grant(&pool, request(1)),
        Err(LedgerError::Conflict)
    ));
    assert_eq!(
        l.capacity(pool.id().key()),
        Some(&Frag::Count(Count::Value(0)))
    );
    l.invariant().unwrap();
}

#[test]
fn restore_requires_all_rows_pools_and_parent_edges_to_agree() {
    let mut l = Ledger::new();
    let pool = count_pool(&mut l, "p", 10);
    let parent = l.grant(&pool, request(1)).unwrap();
    let mut child = request(2);
    child.parent = Some(parent);
    l.grant(&pool, child).unwrap();
    let baseline = l.snapshot();
    assert_eq!(
        LedgerBuilder::new(baseline.clone())
            .finish()
            .unwrap()
            .snapshot(),
        baseline
    );
    let mutations: Vec<Box<dyn Fn(&mut LedgerSnapshot)>> = vec![
        Box::new(|s| s.holdings.push(s.holdings[0].clone())),
        Box::new(|s| s.pools.clear()),
        Box::new(|s| s.classes.clear()),
        Box::new(|s| s.holdings[0].parent = Some(s.holdings[1].id)),
        Box::new(|s| s.holdings[1].parent = Some(HoldingId::try_from(99u64).unwrap())),
        Box::new(|s| s.holdings[0].state = portos_rm::cleanup::HoldingState::Retired(Timestamp::ZERO)),
        Box::new(|s| s.holdings[0].lease = Lease::ParentBound),
        Box::new(|s| s.holdings[0].generation = Generation::new("")),
        Box::new(|s| s.holdings[0].frag = Frag::Ex(Ex::Token)),
        Box::new(|s| s.pools[0].capacity = Frag::Count(Count::Invalid)),
        Box::new(|s| s.pools[0].capacity = Frag::Count(Count::Value(2))),
    ];
    for corrupt in mutations {
        let mut snapshot = baseline.clone();
        corrupt(&mut snapshot);
        assert!(LedgerBuilder::new(snapshot).finish().is_err());
    }
    // Historical tombstones don't create pools that no longer exist.
    let mut history = baseline;
    for row in &mut history.holdings {
        row.state = portos_rm::cleanup::HoldingState::Retired(Timestamp::ZERO);
    }
    history.pools.clear();
    assert!(LedgerBuilder::new(history).finish().is_ok());
}

#[test]
fn lease_resolution_generation_and_integer_boundaries_are_checked_before_change() {
    assert!(Timestamp::try_from(-1i64).is_err());
    assert!(Timestamp::try_from(u64::MAX).is_err());
    assert!(LeaseDuration::try_from(u64::MAX).is_err());
    assert!(HoldingId::try_from(-1i64).is_err());
    assert!(HoldingId::try_from(u64::MAX).is_err());
    assert_eq!(HoldingId::try_from(0u64).unwrap().get(), 0);
    let mut l = Ledger::new();
    let pool = count_pool(&mut l, "p", 5);
    let mut bad = request(1);
    bad.lease = LeaseRequest::ParentBound;
    let before = l.snapshot();
    assert!(matches!(
        l.grant(&pool, bad),
        Err(LedgerError::InvalidLease)
    ));
    let mut overflow = request(1);
    overflow.now = Timestamp::try_from(i64::MAX).unwrap();
    overflow.lease = LeaseRequest::For(LeaseDuration::try_from(1).unwrap());
    assert!(matches!(
        l.grant(&pool, overflow),
        Err(LedgerError::OutOfRange)
    ));
    assert_eq!(l.snapshot(), before);
    let handle = l.grant(&pool, request(1)).unwrap();
    assert_eq!(l.holding(handle.id()).unwrap().lease, Lease::Unbounded);
    let stale = HoldingHandle::new(handle.id(), Generation::new("old"));
    let mut child = request(1);
    child.parent = Some(stale.clone());
    assert!(matches!(
        l.grant(&pool, child),
        Err(LedgerError::StaleGeneration)
    ));
    assert_eq!(
        l.release(&stale, Timestamp::ZERO),
        Err(LedgerError::StaleGeneration)
    );
    let lease = LeaseRequest::For(LeaseDuration::try_from(3).unwrap());
    l.renew(&handle, lease, Timestamp::ZERO).unwrap();
    assert_eq!(
        l.renew(&handle, LeaseRequest::UseClassDefault, Timestamp::ZERO)
            .unwrap(),
        Lease::Until(Timestamp::try_from(3u64).unwrap())
    );
    let mut exhausted = l.snapshot();
    exhausted.holdings[0].id = HoldingId::try_from(i64::MAX).unwrap();
    let mut exhausted = LedgerBuilder::new(exhausted).finish().unwrap();
    let pool = exhausted.pool::<Count>(pool.id().key()).unwrap();
    let before = exhausted.snapshot();
    assert!(matches!(
        exhausted.grant(&pool, request(1)),
        Err(LedgerError::OutOfRange)
    ));
    assert_eq!(exhausted.snapshot(), before);
}

#[test]
fn raw_ranges_are_distinct_from_mathematical_invalid_elements() {
    assert!(Ranges::try_of(&[(5, 2)]).is_err());
    assert!(Ranges::try_of(&[(2, 2)]).is_err());
    assert!(Ranges::try_of(&[(1, 5), (3, 8)]).is_err());
    assert_eq!(
        Ranges::try_of(&[(5, 8), (0, 5)]).unwrap().spans(),
        &[(0, 8)]
    );
    assert!(Claim::new(Ranges::bot()).is_err());
    assert!(Capacity::new(Frac::invalid()).is_err());
    assert!(Claim::new(Count::Invalid).is_err());
    assert!(Claim::new(Ranges::empty()).is_ok());
    assert!(Capacity::new(Count::Value(0)).is_ok());
}

#[test]
fn pool_settlement_cannot_partially_release_rows_when_an_external_child_blocks_it() {
    let mut ledger = Ledger::new();
    let pool = count_pool(&mut ledger, "account", 3);
    ledger.grant(&pool, request(1)).unwrap();
    let parent = ledger.grant(&pool, request(1)).unwrap();
    let other = count_pool(&mut ledger, "dependent", 1);
    let mut child = request(1);
    child.parent = Some(parent);
    ledger.grant(&other, child).unwrap();
    let before = ledger.snapshot();
    assert_eq!(
        ledger.settle_and_zero_pool(&pool, Timestamp::ZERO),
        Err(LedgerError::TeardownOrder)
    );
    assert_eq!(ledger.snapshot(), before);
    ledger.invariant().unwrap();
}
