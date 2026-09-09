use super::codec::{frag_from_json, frag_to_json};
use super::*;
use portos_rm::ra::Ra;
use portos_rm::ra::{Frac, GSet, Ranges};
use portos_rm::time::LeaseDuration;
use serde_json::json;

pub(super) fn store(tag: &str) -> (Arc<Mutex<Connection>>, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-ledger-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = Arc::new(Mutex::new(crate::db::open(&root).unwrap()));
    (db, root)
}

#[test]
fn fragments_round_trip_through_json() {
    let all = [
        Frag::Ex(Ex::Token),
        Frag::Count(Count::Value(7)),
        Frag::Count(Count::Value(u64::MAX)),
        Frag::Count(Count::Invalid),
        Frag::Set(GSet::of(&["a", "b"])),
        Frag::Range(Ranges::of(&[(0, 8), (16, 32)])),
        Frag::Frac(Frac::new(2, 6)),
        Frag::Frac(Frac::invalid()),
        Frag::Frac(Frac::new(1, 2).op(&Frac::new(1, u64::MAX))),
    ];
    for f in all {
        assert_eq!(frag_from_json(&frag_to_json(&f)).unwrap(), f);
    }
    assert_eq!(
        frag_from_json(r#"{"frac":[2,6]}"#).unwrap(),
        Frag::Frac(Frac::new(1, 3))
    );
    assert!(frag_from_json(r#"{"frac":[1,0]}"#).is_err());
    assert!(frag_from_json(r#"{"frac":[1]}"#).is_err());
}

/// Spend rows are the truth and survive a reopen: the pool continues where
/// it was, and the gate refuses at capacity.
#[test]
fn spend_rows_persist_and_gate_refuses_at_capacity() {
    let (db, root) = store("spend");
    {
        let (l, report) = LedgerStore::open(db.clone()).unwrap();
        assert_eq!(report.stale_rows, 0);
        l.transaction(|tx| {
            tx.create_count_pool(
                &AccountId::new("cap_x"),
                &EffectClass::new("emit"),
                &SubjectId::new("fixture"),
                Capacity::new(Count::Value(2)).unwrap(),
            )
            .map(|_| ())
        })
        .unwrap();
        l.spend_many(
            &SubjectId::new("plugin:a"),
            &[SpendRequest {
                account: AccountId::new("cap_x"),
                effect: EffectClass::new("emit"),
                amount: 1,
            }],
            Timestamp::try_from(1u64).unwrap(),
        )
        .unwrap();
        assert_eq!(
            l.spent(&AccountId::new("cap_x"), &EffectClass::new("emit"))
                .unwrap(),
            1
        );
        l.invariant().unwrap();
    }
    let (l, _report) = LedgerStore::open(db.clone()).unwrap();
    assert_eq!(
        l.spent(&AccountId::new("cap_x"), &EffectClass::new("emit"))
            .unwrap(),
        1,
        "spend row reloaded"
    );
    l.spend_many(
        &SubjectId::new("plugin:a"),
        &[SpendRequest {
            account: AccountId::new("cap_x"),
            effect: EffectClass::new("emit"),
            amount: 1,
        }],
        Timestamp::try_from(2u64).unwrap(),
    )
    .unwrap();
    let e = l
        .spend_many(
            &SubjectId::new("plugin:a"),
            &[SpendRequest {
                account: AccountId::new("cap_x"),
                effect: EffectClass::new("emit"),
                amount: 1,
            }],
            Timestamp::try_from(3u64).unwrap(),
        )
        .unwrap_err();
    assert!(
        matches!(e, KernelError::Denied(_)),
        "third spend refused by the issuer gate"
    );
    assert_eq!(
        l.spent(&AccountId::new("cap_x"), &EffectClass::new("emit"))
            .unwrap(),
        2
    );
    assert_eq!(l.counts(&ClassId::new(CLASS_CAP_COUNT)).unwrap(), (2, 0));
    l.invariant().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn count_overflow_refuses_atomically_and_survives_reopen() {
    let (db, root) = store("count-overflow");
    {
        let (l, _) = LedgerStore::open(db.clone()).unwrap();
        l.transaction(|tx| {
            tx.create_count_pool(
                &AccountId::new("cap_max"),
                &EffectClass::new("use"),
                &SubjectId::new("fixture"),
                Capacity::new(Count::Value(u64::MAX)).unwrap(),
            )
            .map(|_| ())
        })
        .unwrap();
        let err = l
            .spend_many(
                &SubjectId::new("user"),
                &[
                    SpendRequest {
                        account: AccountId::new("cap_max"),
                        effect: EffectClass::new("use"),
                        amount: u64::MAX,
                    },
                    SpendRequest {
                        account: AccountId::new("cap_max"),
                        effect: EffectClass::new("use"),
                        amount: 1,
                    },
                ],
                Timestamp::try_from(1u64).unwrap(),
            )
            .unwrap_err();
        assert!(matches!(err, KernelError::Denied(_)));
        assert_eq!(
            l.spent(&AccountId::new("cap_max"), &EffectClass::new("use"))
                .unwrap(),
            0
        );
        assert_eq!(l.counts(&ClassId::new(CLASS_CAP_COUNT)).unwrap(), (0, 0));
    }
    {
        let (l, _) = LedgerStore::open(db.clone()).unwrap();
        l.transaction(|tx| {
            tx.create_count_pool(
                &AccountId::new("cap_max"),
                &EffectClass::new("use"),
                &SubjectId::new("fixture"),
                Capacity::new(Count::Value(u64::MAX)).unwrap(),
            )
            .map(|_| ())
        })
        .unwrap();
        assert_eq!(
            l.spent(&AccountId::new("cap_max"), &EffectClass::new("use"))
                .unwrap(),
            0
        );
        l.spend_many(
            &SubjectId::new("user"),
            &[SpendRequest {
                account: AccountId::new("cap_max"),
                effect: EffectClass::new("use"),
                amount: u64::MAX,
            }],
            Timestamp::try_from(2u64).unwrap(),
        )
        .unwrap();
        assert!(
            l.spend_many(
                &SubjectId::new("user"),
                &[SpendRequest {
                    account: AccountId::new("cap_max"),
                    effect: EffectClass::new("use"),
                    amount: 1
                }],
                Timestamp::try_from(3u64).unwrap()
            )
            .is_err()
        );
        assert_eq!(
            l.spent(&AccountId::new("cap_max"), &EffectClass::new("use"))
                .unwrap(),
            u64::MAX
        );
        l.invariant().unwrap();
    }
    let (l, _) = LedgerStore::open(db).unwrap();
    l.transaction(|tx| {
        tx.create_count_pool(
            &AccountId::new("cap_max"),
            &EffectClass::new("use"),
            &SubjectId::new("fixture"),
            Capacity::new(Count::Value(u64::MAX)).unwrap(),
        )
        .map(|_| ())
    })
    .unwrap();
    assert_eq!(
        l.spent(&AccountId::new("cap_max"), &EffectClass::new("use"))
            .unwrap(),
        u64::MAX
    );
    assert!(
        l.spend_many(
            &SubjectId::new("user"),
            &[SpendRequest {
                account: AccountId::new("cap_max"),
                effect: EffectClass::new("use"),
                amount: 1
            }],
            Timestamp::try_from(4u64).unwrap()
        )
        .is_err()
    );
    l.invariant().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

/// A compound write commits as one transaction: when any step fails — the
/// issuer gate refusing one spend of a batch, or an injected error — no
/// row survives on disk and the in-memory ledger is restored
/// (attachments §4.3 rule 5: ②③同事务).
#[test]
fn compound_ledger_write_is_atomic_under_injected_failure() {
    let (db, root) = store("atomic");
    {
        let (l, _report) = LedgerStore::open(db.clone()).unwrap();
        l.transaction(|tx| {
            tx.create_count_pool(
                &AccountId::new("cap_x"),
                &EffectClass::new("emit"),
                &SubjectId::new("fixture"),
                Capacity::new(Count::Value(1)).unwrap(),
            )
            .map(|_| ())
        })
        .unwrap();
        // A batch of two spends against a pool of capacity 1: the gate
        // refuses the second, so the first must survive nowhere.
        let e = l
            .spend_many(
                &SubjectId::new("plugin:a"),
                &[
                    SpendRequest {
                        account: AccountId::new("cap_x"),
                        effect: EffectClass::new("emit"),
                        amount: 1,
                    },
                    SpendRequest {
                        account: AccountId::new("cap_x"),
                        effect: EffectClass::new("emit"),
                        amount: 1,
                    },
                ],
                Timestamp::try_from(1u64).unwrap(),
            )
            .unwrap_err();
        assert!(matches!(e, KernelError::Denied(_)));
        assert_eq!(
            l.spent(&AccountId::new("cap_x"), &EffectClass::new("emit"))
                .unwrap(),
            0,
            "no partial spend in memory"
        );
        // An injected failure after a successful row write.
        let r: Result<(), KernelError> = l.transaction(|led| {
            led.create_pool(
                &led.registered_class::<Ex>(&ClassId::new(CLASS_CAP))
                    .unwrap(),
                InstanceId::new("b"),
                Capacity::new(Ex::Token).unwrap(),
            )
            .unwrap();
            let _id = led
                .grant(
                    &led.pool::<Ex>(&ResourceKey::new(
                        ClassId::new(CLASS_CAP),
                        InstanceId::new("b"),
                    ))
                    .unwrap(),
                    GrantRequest {
                        owner: SubjectId::new("plugin:b"),
                        claim: Claim::new(Ex::Token).unwrap(),
                        generation: Generation::new("tok"),
                        parent: None,
                        lease: LeaseRequest::UseClassDefault,
                        now: Timestamp::try_from(1u64).unwrap(),
                    },
                )
                .map(|h| h.id())?;
            Err(KernelError::Corrupt("injected".into()))
        });
        assert!(r.is_err());
        assert!(
            l.live_snapshot(&SubjectId::new("plugin:b"))
                .unwrap()
                .is_empty(),
            "in-memory rolled back"
        );
        // The committed half: one spend lands and persists.
        l.spend_many(
            &SubjectId::new("plugin:a"),
            &[SpendRequest {
                account: AccountId::new("cap_x"),
                effect: EffectClass::new("emit"),
                amount: 1,
            }],
            Timestamp::try_from(1u64).unwrap(),
        )
        .unwrap();
        assert_eq!(
            l.spent(&AccountId::new("cap_x"), &EffectClass::new("emit"))
                .unwrap(),
            1
        );
    }
    let (l, _report) = LedgerStore::open(db.clone()).unwrap();
    assert_eq!(
        l.spent(&AccountId::new("cap_x"), &EffectClass::new("emit"))
            .unwrap(),
        1,
        "committed row reloaded"
    );
    assert_eq!(
        l.inner.lock().unwrap().ledger.live_count(),
        1,
        "no partial row on disk"
    );
    l.invariant().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

pub(super) fn register_test_class(l: &LedgerStore, class: &str, lease_secs: Option<u64>) {
    l.transaction(|tx| {
        tx.register_class(ClassDecl {
            cleanup: CleanupPolicy::Managed(CleanupKind::Provider),
            class_id: class.into(),
            algebra: AlgebraTag::Exclusive,
            release_idempotent: true,
            lease_duration: lease_secs.map(|n| LeaseDuration::try_from(n).unwrap()),
            revert_grade: RevertGrade::Inverse,
        })
        .map(|_| ())
    })
    .unwrap();
}
pub(super) fn provider_hold(
    l: &LedgerStore,
    class: &str,
    instance: &str,
    owner: &str,
    parent: Option<HoldingHandle>,
    now: Timestamp,
) -> HoldingHandle {
    l.hold_managed(
        ExclusiveRequest {
            owner: owner.into(),
            resource: ResourceKey::new(class.into(), instance.into()),
            generation: Generation::new("fixture"),
            parent,
            lease: LeaseRequest::UseClassDefault,
        },
        CleanupTarget::Provider,
        None,
        now,
    )
    .unwrap()
}
#[derive(Default)]
struct RecordingWorld {
    released: Vec<HoldingId>,
}
impl CleanupExecutor for RecordingWorld {
    fn execute(&mut self, work: &CleanupWork) -> CleanupOutcome {
        self.released.push(work.task().holding.id());
        CleanupOutcome::Confirmed
    }
}

/// Durable tasks retain failed cleanup across restart; accounting rows do not
/// pretend to have physical world actions.
#[test]
fn journal_entries_survive_reopen_and_replay_once() {
    struct FlakyWorld {
        fail: bool,
        actions: usize,
    }
    impl CleanupExecutor for FlakyWorld {
        fn execute(&mut self, _: &CleanupWork) -> CleanupOutcome {
            self.actions += 1;
            if self.fail {
                CleanupOutcome::Retryable("offline".into())
            } else {
                CleanupOutcome::Confirmed
            }
        }
    }
    let (db, root) = store("journal");
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    register_test_class(&l, "test/physical", None);
    let h = provider_hold(&l, "test/physical", "x", "plugin:x", None, Timestamp::ZERO);
    let mut world = FlakyWorld {
        fail: true,
        actions: 0,
    };
    let report = l
        .release_with_world(&h, &mut world, Timestamp::ZERO)
        .unwrap();
    assert_eq!(world.actions, 1);
    assert_eq!(report.pending.len(), 1);
    assert!(report.completed.is_empty());
    let key = report.pending[0].key.clone();
    assert!(matches!(
        l.holding(h.id()).unwrap().unwrap().state,
        HoldingState::Retiring(_)
    ));
    assert!(
        l.live_snapshot(&SubjectId::new("plugin:x"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        l.occupying_snapshot(&SubjectId::new("plugin:x"))
            .unwrap()
            .len(),
        1
    );
    drop(l);
    let (l, report) = LedgerStore::open(db.clone()).unwrap();
    assert_eq!(report.cleanup_pending, 1);
    assert_eq!(l.cleanup_tasks().unwrap()[0].key, key);
    let mut world = FlakyWorld {
        fail: false,
        actions: 0,
    };
    let report = l.retry_cleanup(&mut world, Timestamp::ZERO).unwrap();
    assert_eq!(world.actions, 1);
    assert_eq!(report.completed, vec![h.clone()]);
    l.retry_cleanup(&mut world, Timestamp::ZERO).unwrap();
    assert_eq!(world.actions, 1);
    drop(l);
    let (l, report) = LedgerStore::open(db).unwrap();
    assert_eq!(report.cleanup_pending, 0);
    assert!(l.holding(h.id()).unwrap().unwrap().released_at().is_some());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stale_plugin_rows_are_reconciled_on_open() {
    let (db, root) = store("stale");
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    // A previous boot is unambiguously absent, even when the numeric PID exists.
    let process = ProcessWitness::new(std::process::id(), 1, "previous-boot".into()).unwrap();
    let host = HostWitness::new(process.clone(), "old-host".into()).unwrap();
    let parent = l
        .hold_managed(
            ExclusiveRequest {
                owner: "plugin:x".into(),
                resource: ResourceKey::new(CLASS_PLUGIN.into(), "x".into()),
                generation: "tok1".into(),
                parent: None,
                lease: LeaseRequest::Unbounded,
            },
            CleanupTarget::Plugin {
                host: host.clone(),
                process: process.clone(),
            },
            None,
            Timestamp::ZERO,
        )
        .unwrap();
    l.hold_managed(
        ExclusiveRequest {
            owner: "plugin:x".into(),
            resource: ResourceKey::new(CLASS_SUBSCRIPTION.into(), "7".into()),
            generation: "sub".into(),
            parent: Some(parent.clone()),
            lease: LeaseRequest::ParentBound,
        },
        CleanupTarget::Subscription {
            host: host.clone(),
            subscription: 7,
        },
        None,
        Timestamp::ZERO,
    )
    .unwrap();
    // Distinct child incarnation, still from the absent boot.
    let child_process = ProcessWitness::new(std::process::id(), 2, "previous-boot".into()).unwrap();
    l.hold_managed(
        ExclusiveRequest {
            owner: "plugin:y".into(),
            resource: ResourceKey::new(CLASS_PLUGIN.into(), "y".into()),
            generation: "child".into(),
            parent: Some(parent),
            lease: LeaseRequest::ParentBound,
        },
        CleanupTarget::Plugin {
            host,
            process: child_process,
        },
        Some("plugin:x".into()),
        Timestamp::ZERO,
    )
    .unwrap();
    drop(l);
    let (l, report) = LedgerStore::open(db).unwrap();
    assert_eq!(report.stale_rows, 3);
    assert_eq!(l.counts(&CLASS_PLUGIN.into()).unwrap(), (0, 2));
    assert_eq!(l.counts(&CLASS_SUBSCRIPTION.into()).unwrap(), (0, 1));
    assert!(l.cleanup_tasks().unwrap().iter().all(|t| t.state.is_done()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn expired_parent_waits_for_child_with_its_own_lease() {
    let (db, root) = store("sweep");
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    register_test_class(&l, "test/leased", Some(1));
    register_test_class(&l, "test/long", Some(3600));
    register_test_class(&l, "test/unleased", None);
    let p = provider_hold(&l, "test/leased", "p", "test", None, Timestamp::ZERO);
    let c = provider_hold(
        &l,
        "test/long",
        "c",
        "test",
        Some(p.clone()),
        Timestamp::ZERO,
    );
    let g = provider_hold(
        &l,
        "test/unleased",
        "g",
        "test",
        Some(c.clone()),
        Timestamp::ZERO,
    );
    let mut world = RecordingWorld::default();
    let report = l
        .sweep_with_world(&mut world, Timestamp::try_from(2u64).unwrap())
        .unwrap();
    assert!(report.released.is_empty());
    assert!(world.released.is_empty());
    assert!(matches!(
        l.holding(p.id()).unwrap().unwrap().state,
        HoldingState::Retiring(_)
    ));
    assert!(l.holding(c.id()).unwrap().unwrap().state.is_active());
    assert_eq!(l.occupying_snapshot(&"test".into()).unwrap().len(), 3);
    assert!(
        l.renew(&p, LeaseRequest::Unbounded, Timestamp::ZERO)
            .is_err()
    );
    l.renew(&c, LeaseRequest::UseClassDefault, Timestamp::ZERO)
        .unwrap();
    let report = l
        .sweep_with_world(&mut world, Timestamp::try_from(3601u64).unwrap())
        .unwrap();
    assert_eq!(report.released.len(), 3);
    assert_eq!(world.released, vec![g.id(), c.id(), p.id()]);
    drop(l);
    let (l, _) = LedgerStore::open(db).unwrap();
    assert_eq!(l.inner.lock().unwrap().ledger.live_count(), 0);
    std::fs::remove_dir_all(root).unwrap();
}

/// Substrate reconcile (WP-02): a live `kernel/process` row of a previous
/// kernel incarnation whose process is still running is killed on open
/// and the row tombstoned — a previous incarnation never survives a
/// kernel restart (crash-only).
#[test]
fn reconcile_kills_a_live_process_of_a_previous_kernel_incarnation() {
    use std::os::unix::process::ExitStatusExt;
    let (db, root) = store("reconcile-substrate");
    let mut child = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .unwrap();
    let pid = child.id();
    let start = proc_start_time(pid).expect("child visible in /proc");
    {
        let (l, _report) = LedgerStore::open(db.clone()).unwrap();
        l.hold_substrate(
            ExclusiveRequest {
                owner: SubjectId::new("plugin:x"),
                resource: ResourceKey::new(
                    ClassId::new(CLASS_PROCESS),
                    InstanceId::new(&format!("x/{pid}")),
                ),
                generation: Generation::new(&format!("{pid}:{start}")),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
            },
            &json!({"pid": pid, "start": start}),
            Timestamp::try_from(1u64).unwrap(),
        )
        .map(|h| h.id())
        .unwrap();
        assert_eq!(l.counts(&ClassId::new(CLASS_PROCESS)).unwrap(), (1, 0));
        // drop without touching the sleeper: it plays the orphan of a
        // kernel that died hard.
    }
    let (l, report) = LedgerStore::open(db.clone()).unwrap();
    assert_eq!(report.substrate.process_killed, 1);
    assert_eq!(
        l.counts(&ClassId::new(CLASS_PROCESS)).unwrap(),
        (0, 1),
        "row tombstoned"
    );
    assert_eq!(
        child.wait().unwrap().signal(),
        Some(9),
        "the sleeper got SIGKILL from the reconcile"
    );
    assert!(!proc_alive(pid, start), "the exact incarnation is gone");
    l.invariant().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

fn account_fixture(store: &LedgerStore, account: &AccountId, amount: u64) {
    store
        .transaction(|tx| {
            tx.create_count_pool(
                account,
                &EffectClass::new("emit"),
                &SubjectId::new("owner"),
                Capacity::new(Count::Value(amount)).unwrap(),
            )
            .map(|_| ())
        })
        .unwrap();
}
fn spend_request(account: &AccountId, amount: u64) -> SpendRequest {
    SpendRequest {
        account: account.clone(),
        effect: EffectClass::new("emit"),
        amount,
    }
}

#[test]
fn sql_failure_and_unwind_discard_staging_without_poisoning_the_store() {
    let (db, root) = store("rollback-unwind");
    let (store, _) = LedgerStore::open(db.clone()).unwrap();
    let account = AccountId::new("a");
    account_fixture(&store, &account, 3);
    let owner = SubjectId::new("owner");
    let request = spend_request(&account, 1);
    let before = store.inner.lock().unwrap().ledger.snapshot();
    db.lock().unwrap().execute_batch("CREATE TRIGGER fail_holding BEFORE INSERT ON holdings BEGIN SELECT RAISE(ABORT,'injected write failure'); END;").unwrap();
    assert!(
        store
            .spend_many(&owner, &[request.clone()], Timestamp::ZERO)
            .is_err()
    );
    assert_eq!(store.inner.lock().unwrap().ledger.snapshot(), before);
    db.lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_holding")
        .unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<(), KernelError> = store.transaction(|tx| {
            tx.spend(&owner, &request, Timestamp::ZERO)?;
            panic!("injected unwind");
        });
    }));
    assert!(panic.is_err());
    assert_eq!(store.inner.lock().unwrap().ledger.snapshot(), before);
    // The rollback path releases both mutexes before resuming the unwind.
    store
        .spend_many(&owner, &[request], Timestamp::ZERO)
        .unwrap();
    drop(store);
    let (store, _) = LedgerStore::open(db).unwrap();
    assert_eq!(store.spent(&account, &EffectClass::new("emit")).unwrap(), 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_commit_stops_writing_until_reopen() {
    let (db, root) = store("commit-failure");
    let (store, _) = LedgerStore::open(db.clone()).unwrap();
    let account = AccountId::new("a");
    account_fixture(&store, &account, 2);
    // A deferred constraint fails at COMMIT, after every staged write succeeds.
    db.lock().unwrap().execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE commit_parent(id INTEGER PRIMARY KEY); CREATE TABLE commit_child(parent INTEGER REFERENCES commit_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_commit AFTER INSERT ON holdings BEGIN INSERT INTO commit_child(parent) VALUES(42); END;").unwrap();
    let before = store.inner.lock().unwrap().ledger.snapshot();
    let request = spend_request(&account, 1);
    let owner = SubjectId::new("owner");
    assert!(
        store
            .spend_many(&owner, &[request.clone()], Timestamp::ZERO)
            .is_err()
    );
    assert!(!store.inner.lock().unwrap().writable);
    assert!(
        store.spent(&account, &EffectClass::new("emit")).is_err(),
        "uncertain storage is not presented as current state"
    );
    assert_eq!(
        store.inner.lock().unwrap().ledger.snapshot(),
        before,
        "uncommitted state never published"
    );
    db.lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_commit")
        .unwrap();
    assert!(
        store
            .spend_many(&owner, &[request.clone()], Timestamp::ZERO)
            .is_err(),
        "no silent resumption after an uncertain commit"
    );
    drop(store);
    let (store, _) = LedgerStore::open(db).unwrap();
    assert_eq!(store.spent(&account, &EffectClass::new("emit")).unwrap(), 0);
    store
        .spend_many(&owner, &[request], Timestamp::ZERO)
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn count_resize_zero_settlement_and_account_associations_survive_reopen() {
    let (db, root) = store("pool-authority");
    let (store, _) = LedgerStore::open(db.clone()).unwrap();
    let account = AccountId::new("a");
    let neighbour = AccountId::new("a/b");
    account_fixture(&store, &account, 5);
    account_fixture(&store, &neighbour, 7);
    let owner = SubjectId::new("worker");
    assert!(
        store
            .spend_many(&owner, &[spend_request(&account, 0)], Timestamp::ZERO)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.counts(&ClassId::new(CLASS_CAP_COUNT)).unwrap(),
        (0, 0)
    );
    store
        .spend_many(
            &owner,
            &[spend_request(&account, 3), spend_request(&neighbour, 2)],
            Timestamp::ZERO,
        )
        .unwrap();
    let before = store.inner.lock().unwrap().ledger.snapshot();
    assert!(
        store
            .transaction(|tx| {
                let p = tx.count_pool(&account, &EffectClass::new("emit"))?;
                tx.resize_pool(&p, Capacity::new(Count::Value(2)).unwrap())
            })
            .is_err()
    );
    assert_eq!(store.inner.lock().unwrap().ledger.snapshot(), before);
    store
        .transaction(|tx| {
            let p = tx.count_pool(&account, &EffectClass::new("emit"))?;
            tx.resize_pool(&p, Capacity::new(Count::Value(4)).unwrap())
        })
        .unwrap();
    drop(store);
    let (store, _) = LedgerStore::open(db.clone()).unwrap();
    assert_eq!(
        store
            .count_balance(&account, &EffectClass::new("emit"))
            .unwrap(),
        Some(1)
    );
    store
        .transaction(|tx| {
            let p = tx.count_pool(&account, &EffectClass::new("emit"))?;
            tx.settle_and_zero_pool(&p, Timestamp::ZERO)
        })
        .unwrap();
    drop(store);
    let (store, _) = LedgerStore::open(db).unwrap();
    assert_eq!(
        store
            .count_balance(&account, &EffectClass::new("emit"))
            .unwrap(),
        Some(0)
    );
    assert_eq!(
        store
            .count_balance(&neighbour, &EffectClass::new("emit"))
            .unwrap(),
        Some(5),
        "account a never settles a/b by prefix"
    );
    assert!(
        store
            .spend_many(&owner, &[spend_request(&account, 1)], Timestamp::ZERO)
            .is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn another_store_cannot_overwrite_a_newer_commit() {
    let (db, root) = store("store-version");
    let (old, _) = LedgerStore::open(db.clone()).unwrap();
    let account = AccountId::new("a");
    account_fixture(&old, &account, 1);
    let (current, _) = LedgerStore::open(db).unwrap();
    current
        .spend_many(
            &SubjectId::new("owner"),
            &[spend_request(&account, 1)],
            Timestamp::ZERO,
        )
        .unwrap();
    assert!(
        old.spend_many(
            &SubjectId::new("owner"),
            &[spend_request(&account, 1)],
            Timestamp::ZERO
        )
        .is_err()
    );
    assert_eq!(
        current.spent(&account, &EffectClass::new("emit")).unwrap(),
        1
    );
    assert!(old.live_snapshot(&SubjectId::new("owner")).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_migration_imports_capacity_once_and_retains_history() {
    let (db, root) = store("legacy-import");
    let cap = Capability {
        cap_id: "legacy".into(),
        subject: "owner".into(),
        resource: "driver:toy".into(),
        verbs: ["emit".to_string()].into_iter().collect(),
        constraints: portos_proto::Constraints {
            expires_at: None,
            counts: [("emit".to_string(), 4)].into_iter().collect(),
        },
        parent: None,
        revoked: false,
    };
    {
        let conn = db.lock().unwrap();
        store_capability(&conn, &cap).unwrap();
        conn.execute("INSERT INTO holdings(id,subject,class_id,instance,frag,generation,acquired_at) VALUES(0,'owner:spent',?1,'legacy/emit',?2,'spend',0)",params![CLASS_CAP_COUNT,frag_to_json(&Frag::Count(Count::Value(2)))]).unwrap();
        conn.execute("INSERT INTO holdings(id,subject,class_id,instance,frag,generation,acquired_at) VALUES(1,'owner',?1,'legacy',?2,'legacy',0)",params![CLASS_CAP,frag_to_json(&Frag::Ex(Ex::Token))]).unwrap();
    }
    let account = AccountId::new("legacy");
    let effect = EffectClass::new("emit");
    let (store, _) = LedgerStore::open(db.clone()).unwrap();
    assert_eq!(store.count_balance(&account, &effect).unwrap(), Some(2));
    store
        .transaction(|tx| {
            let pool = tx.create_count_pool(
                &account,
                &effect,
                &SubjectId::new("owner"),
                Capacity::new(Count::Value(4)).unwrap(),
            )?;
            assert_eq!(pool.id().key().instance().as_str(), "legacy/emit");
            Ok(())
        })
        .unwrap();
    assert_eq!(
        store
            .holding(HoldingId::try_from(0u64).unwrap())
            .unwrap()
            .unwrap()
            .lease,
        Lease::Unbounded
    );
    store
        .transaction(|tx| {
            let p = tx.count_pool(&account, &effect)?;
            tx.resize_pool(&p, Capacity::new(Count::Value(3)).unwrap())
        })
        .unwrap();
    drop(store);
    for _ in 0..2 {
        let (store, _) = LedgerStore::open(db.clone()).unwrap();
        assert_eq!(
            store.count_balance(&account, &effect).unwrap(),
            Some(1),
            "old caps capacity never overwrites the durable pool"
        );
        assert_eq!(
            store.counts(&ClassId::new(CLASS_CAP_COUNT)).unwrap(),
            (1, 0)
        );
    }
    let conn = db.lock().unwrap();
    assert_eq!(
        conn.query_row("SELECT version FROM resource_schema", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM resource_accounts", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(conn);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn corrupt_legacy_and_versioned_graphs_never_become_ready() {
    let (db, root) = store("legacy-bad");
    db.lock().unwrap().execute("INSERT INTO holdings(id,subject,class_id,instance,frag,generation,acquired_at) VALUES(0,'owner',?1,'missing/emit',?2,'g',0)",params![CLASS_CAP_COUNT,frag_to_json(&Frag::Count(Count::Value(1)))]).unwrap();
    assert!(LedgerStore::open(db.clone()).is_err());
    assert_eq!(
        db.lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM holdings", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1,
        "bad input preserved"
    );
    assert_eq!(
        db.lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='resource_schema'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0,
        "failed migration rolled back its version"
    );
    std::fs::remove_dir_all(root).unwrap();
    for (name, sql) in [
        (
            "bad-capacity",
            "UPDATE resource_pools SET capacity='{\"count\":0}'",
        ),
        ("missing-pool", "DELETE FROM resource_pools"),
        ("negative-time", "UPDATE holdings SET acquired_at=-1"),
        ("parent-cycle", "UPDATE holdings SET parent=id"),
        ("bad-generation", "UPDATE holdings SET generation=''"),
        (
            "bad-class",
            "UPDATE resource_classes SET algebra='frac' WHERE class_id='kernel/cap-count'",
        ),
    ] {
        let (db, root) = store(name);
        let (ledger, _) = LedgerStore::open(db.clone()).unwrap();
        let account = AccountId::new("a");
        account_fixture(&ledger, &account, 2);
        ledger
            .spend_many(
                &SubjectId::new("owner"),
                &[spend_request(&account, 1)],
                Timestamp::ZERO,
            )
            .unwrap();
        drop(ledger);
        db.lock().unwrap().execute_batch(sql).unwrap();
        assert!(LedgerStore::open(db.clone()).is_err(), "{name}");
        assert_eq!(
            db.lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM holdings", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn all_five_algebras_round_trip_as_registered_pools_and_claims() {
    let (db, root) = store("five-algebras");
    let (store, _) = LedgerStore::open(db.clone()).unwrap();
    fn add<A: RuntimeAlgebra>(
        tx: &mut LedgerTxn<'_>,
        name: &str,
        value: A,
    ) -> Result<(), KernelError> {
        let c = tx
            .register_class(ClassDecl {
                cleanup: portos_rm::cleanup::CleanupPolicy::AccountingOnly,
                class_id: ClassId::new(name),
                algebra: A::TAG,
                release_idempotent: true,
                lease_duration: None,
                revert_grade: RevertGrade::Inverse,
            })?
            .for_algebra::<A>()
            .map_err(map_err)?;
        let p = tx.create_pool(
            &c,
            InstanceId::new("实例"),
            Capacity::new(value.clone()).unwrap(),
        )?;
        tx.grant(
            &p,
            GrantRequest {
                owner: SubjectId::new("owner"),
                claim: Claim::new(value).unwrap(),
                generation: Generation::new("g"),
                parent: None,
                lease: LeaseRequest::Unbounded,
                now: Timestamp::ZERO,
            },
        )?;
        Ok(())
    }
    store
        .transaction(|tx| {
            add(tx, "exclusive", Ex::Token)?;
            add(tx, "count", Count::Value(u64::MAX))?;
            add(tx, "set", GSet::of(&["共享"]))?;
            add(tx, "ranges", Ranges::of(&[(0, 32)]))?;
            add(tx, "fraction", Frac::new(1, 3))
        })
        .unwrap();
    let expected = store.inner.lock().unwrap().ledger.snapshot();
    drop(store);
    let (store, _) = LedgerStore::open(db).unwrap();
    assert_eq!(store.inner.lock().unwrap().ledger.snapshot(), expected);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_fragment_shapes_are_rejected_without_default_resources() {
    for value in [
        r#"{"ex":"unknown"}"#,
        r#"{"ex":"token","count":1}"#,
        r#"{"set":["good",2]}"#,
        r#"{"range":[[5,2]],"bot":false}"#,
        r#"{"range":[[1,2,3]]}"#,
        r#"{"range":[],"bot":"false"}"#,
        r#"{"frac":[1,0]}"#,
    ] {
        assert!(frag_from_json(value).is_err(), "{value}");
    }
}

#[test]
fn crash_worker() {
    let Ok(root) = std::env::var("PORTOS_M1_CRASH_ROOT") else {
        return;
    };
    let phase = std::env::var("PORTOS_M1_CRASH_PHASE").unwrap();
    let db = Arc::new(Mutex::new(
        crate::db::open(std::path::Path::new(&root)).unwrap(),
    ));
    let (store, _) = LedgerStore::open(db).unwrap();
    let request = spend_request(&AccountId::new("a"), 1);
    let owner = SubjectId::new("owner");
    if phase == "before" {
        let _: Result<(), KernelError> = store.transaction(|tx| {
            tx.spend(&owner, &request, Timestamp::ZERO)?;
            // Private fixture puts the crash after SQL writes and before COMMIT.
            persist(tx.sql, &tx.ledger.snapshot())?;
            std::process::exit(77)
        });
    } else {
        store
            .spend_many(&owner, &[request], Timestamp::ZERO)
            .unwrap();
        std::process::exit(77)
    }
}

#[test]
fn process_interruption_before_and_after_commit_has_no_third_state() {
    let (db, root) = store("process-crash");
    let (store, _) = LedgerStore::open(db.clone()).unwrap();
    let account = AccountId::new("a");
    account_fixture(&store, &account, 2);
    drop(store);
    for (phase, expected) in [("before", 0), ("after", 1)] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "ledger::tests::crash_worker", "--nocapture"])
            .env("PORTOS_M1_CRASH_ROOT", &root)
            .env("PORTOS_M1_CRASH_PHASE", phase)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(77));
        let (store, _) = LedgerStore::open(db.clone()).unwrap();
        assert_eq!(
            store.spent(&account, &EffectClass::new("emit")).unwrap(),
            expected
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn physical_holdings_require_consistent_witnesses_before_publication() {
    let (db, root) = store("witness-admission");
    let (store, _) = LedgerStore::open(db).unwrap();
    let before = store.inner.lock().unwrap().ledger.snapshot();
    let missing = store.transaction(|tx| {
        let class = tx.registered_class::<Ex>(&ClassId::new(CLASS_PROCESS))?;
        let pool = tx.create_pool(
            &class,
            InstanceId::new("child"),
            Capacity::new(Ex::Token).unwrap(),
        )?;
        tx.grant(
            &pool,
            GrantRequest {
                owner: SubjectId::new("owner"),
                claim: Claim::new(Ex::Token).unwrap(),
                generation: Generation::new("42:10"),
                parent: None,
                lease: LeaseRequest::Unbounded,
                now: Timestamp::ZERO,
            },
        )
    });
    assert!(missing.is_err());
    assert_eq!(store.inner.lock().unwrap().ledger.snapshot(), before);
    let mismatch = store.hold_substrate(
        ExclusiveRequest {
            owner: SubjectId::new("owner"),
            resource: ResourceKey::new(ClassId::new(CLASS_PROCESS), InstanceId::new("child")),
            generation: Generation::new("42:11"),
            parent: None,
            lease: LeaseRequest::Unbounded,
        },
        &json!({"pid":42,"start":10}),
        Timestamp::ZERO,
    );
    assert!(mismatch.is_err());
    assert_eq!(store.inner.lock().unwrap().ledger.snapshot(), before);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sqlite_full_during_write_keeps_memory_and_recovery_at_the_last_commit() {
    let (db, root) = store("sqlite-full");
    let (store, _) = LedgerStore::open(db.clone()).unwrap();
    let account = AccountId::new("a");
    account_fixture(&store, &account, 2);
    let before = store.inner.lock().unwrap().ledger.snapshot();
    let previous_limit = {
        let conn = db.lock().unwrap();
        let previous: i64 = conn
            .query_row("PRAGMA max_page_count", [], |r| r.get(0))
            .unwrap();
        let pages: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        conn.pragma_update(None, "max_page_count", pages).unwrap();
        previous
    };
    // Bound only this test database, so an overflow page produces SQLITE_FULL
    // without filling the filesystem or relying on a synthetic application error.
    let error = store
        .spend_many(
            &SubjectId::new("owner".repeat(32768)),
            &[spend_request(&account, 1)],
            Timestamp::ZERO,
        )
        .unwrap_err();
    assert!(
        matches!(error, KernelError::Db(rusqlite::Error::SqliteFailure(ref e, _)) if e.code == rusqlite::ErrorCode::DiskFull)
    );
    assert_eq!(store.inner.lock().unwrap().ledger.snapshot(), before);
    db.lock()
        .unwrap()
        .pragma_update(None, "max_page_count", previous_limit)
        .unwrap();
    drop(store);
    let (store, _) = LedgerStore::open(db).unwrap();
    assert_eq!(store.inner.lock().unwrap().ledger.snapshot(), before);
    store
        .spend_many(
            &SubjectId::new("owner"),
            &[spend_request(&account, 1)],
            Timestamp::ZERO,
        )
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
