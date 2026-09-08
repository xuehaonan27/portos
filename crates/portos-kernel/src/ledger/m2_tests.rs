use super::tests::{provider_hold, register_test_class, store};
use super::*;
use serde_json::json;

struct Execute<F>(F);
impl<F: FnMut(&CleanupWork) -> CleanupOutcome> CleanupExecutor for Execute<F> {
    fn execute(&mut self, w: &CleanupWork) -> CleanupOutcome {
        self.0(w)
    }
}
fn provider(l: &LedgerStore) -> HoldingHandle {
    register_test_class(l, "test/provider", None);
    provider_hold(
        l,
        "test/provider",
        "resource",
        "owner",
        None,
        Timestamp::ZERO,
    )
}
fn lock(l: &LedgerStore, path: &std::path::Path) -> HoldingHandle {
    std::fs::write(path, b"lock").unwrap();
    l.hold_substrate(
        ExclusiveRequest {
            owner: "owner".into(),
            resource: ResourceKey::new(CLASS_FILE_LOCK.into(), path.to_str().unwrap().into()),
            generation: "lock-generation".into(),
            parent: None,
            lease: LeaseRequest::Unbounded,
        },
        &json!({}),
        Timestamp::ZERO,
    )
    .unwrap()
}

#[test]
fn cleanup_intent_failure_cannot_reach_the_world() {
    let (db, root) = store("m2-intent-failure");
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    let h = provider(&l);
    db.lock().unwrap().execute_batch("CREATE TRIGGER refuse_cleanup BEFORE INSERT ON resource_cleanup BEGIN SELECT RAISE(ABORT,'intent refused'); END;").unwrap();
    let mut calls = 0;
    let result = l.release_with_world(
        &h,
        &mut Execute(|_: &CleanupWork| {
            calls += 1;
            CleanupOutcome::Confirmed
        }),
        Timestamp::ZERO,
    );
    assert!(result.is_err());
    assert_eq!(calls, 0);
    assert!(l.holding(h.id()).unwrap().unwrap().state.is_active());
    assert!(l.cleanup_tasks().unwrap().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn executor_observes_committed_work_and_can_reenter_storage_without_locks() {
    let (db, root) = store("m2-unlocked");
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    let h = provider(&l);
    let mut seen = None;
    let report = l
        .release_with_world(
            &h,
            &mut Execute(|work: &CleanupWork| {
                let conn = db.try_lock().expect("world must not hold database mutex");
                let state: String = conn
                    .query_row(
                        "SELECT state FROM holdings WHERE id=?1",
                        params![h.id().to_sql()],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(state, "retiring");
                let json: String = conn
                    .query_row(
                        "SELECT record FROM resource_cleanup WHERE holding_id=?1",
                        params![h.id().to_sql()],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(cleanup_codec::task(&json).unwrap(), *work.task());
                drop(conn);
                assert!(l.live_snapshot(&"owner".into()).unwrap().is_empty());
                assert_eq!(l.occupying_snapshot(&"owner".into()).unwrap().len(), 1);
                l.transaction(|tx| {
                    tx.create_count_pool(
                        &"audit".into(),
                        &"emit".into(),
                        &"owner".into(),
                        Capacity::new(Count::Value(1)).unwrap(),
                    )
                    .map(|_| ())
                })
                .unwrap();
                seen = Some(work.task().key.clone());
                CleanupOutcome::Unknown("response unavailable".into())
            }),
            Timestamp::ZERO,
        )
        .unwrap();
    assert_eq!(report.pending.len(), 1);
    assert!(matches!(report.pending[0].state, CleanupState::Unknown(_)));
    assert!(
        l.hold_managed(
            ExclusiveRequest {
                owner: "other".into(),
                resource: ResourceKey::new("test/provider".into(), "resource".into()),
                generation: "new".into(),
                parent: None,
                lease: LeaseRequest::Unbounded
            },
            CleanupTarget::Provider,
            None,
            Timestamp::ZERO
        )
        .is_err()
    );
    let retry = l
        .retry_cleanup(
            &mut Execute(|work: &CleanupWork| {
                assert_eq!(Some(work.task().key.clone()), seen);
                CleanupOutcome::Confirmed
            }),
            Timestamp::ZERO,
        )
        .unwrap();
    assert_eq!(retry.completed, vec![h]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn world_success_with_failed_confirmation_is_recovered_as_absence() {
    let (db, root) = store("m2-confirm-failure");
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    let path = root.join("lock");
    let h = lock(&l, &path);
    db.lock().unwrap().execute_batch("CREATE TRIGGER refuse_confirmation BEFORE UPDATE ON holdings WHEN NEW.state='retired' BEGIN SELECT RAISE(ABORT,'confirmation refused'); END;").unwrap();
    assert!(
        l.release_with_world(&h, &mut substrate::BootstrapWorld, Timestamp::ZERO)
            .is_err()
    );
    assert!(!path.exists());
    assert!(matches!(
        l.holding(h.id()).unwrap().unwrap().state,
        HoldingState::Retiring(_)
    ));
    let key = l.cleanup_tasks().unwrap()[0].key.clone();
    db.lock()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_confirmation")
        .unwrap();
    drop(l);
    let (l, report) = LedgerStore::open(db).unwrap();
    assert_eq!(report.cleanup_pending, 0);
    let tasks = l.cleanup_tasks().unwrap();
    assert_eq!(tasks[0].key, key);
    assert_eq!(
        tasks[0].state,
        CleanupState::Done(Completion::AlreadyAbsent)
    );
    assert_eq!(tasks[0].attempts, 2);
    assert!(l.holding(h.id()).unwrap().unwrap().released_at().is_some());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn executor_unwind_retains_unknown_work_without_poisoning_storage() {
    let (db, root) = store("m2-world-panic");
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    let path = root.join("lock");
    let h = lock(&l, &path);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        l.release_with_world(
            &h,
            &mut Execute(|_: &CleanupWork| {
                std::fs::remove_file(&path).unwrap();
                panic!("lost execution frame")
            }),
            Timestamp::ZERO,
        )
    }));
    assert!(result.is_err());
    assert!(matches!(
        l.cleanup_tasks().unwrap()[0].state,
        CleanupState::Unknown(_)
    ));
    l.invariant().unwrap();
    l.retry_cleanup(&mut substrate::BootstrapWorld, Timestamp::ZERO)
        .unwrap();
    assert!(l.holding(h.id()).unwrap().unwrap().released_at().is_some());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_workers_do_not_execute_an_owned_attempt_twice() {
    let (db, root) = store("m2-workers");
    let (l, _) = LedgerStore::open(db).unwrap();
    let l = Arc::new(l);
    let h = provider(&l);
    let (started, ready) = std::sync::mpsc::channel();
    let (finish, wait) = std::sync::mpsc::channel();
    let first = l.clone();
    let handle = h.clone();
    let thread = std::thread::spawn(move || {
        first.release_with_world(
            &handle,
            &mut Execute(|_: &CleanupWork| {
                started.send(()).unwrap();
                wait.recv().unwrap();
                CleanupOutcome::Confirmed
            }),
            Timestamp::ZERO,
        )
    });
    ready
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let mut calls = 0;
    let report = l
        .retry_cleanup(
            &mut Execute(|_: &CleanupWork| {
                calls += 1;
                CleanupOutcome::Confirmed
            }),
            Timestamp::ZERO,
        )
        .unwrap();
    assert_eq!(calls, 0);
    assert_eq!(report.pending.len(), 1);
    finish.send(()).unwrap();
    assert_eq!(thread.join().unwrap().unwrap().completed, vec![h]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_child_runner_resumes_its_already_retiring_parent() {
    let (db, root) = store("m2-parent-wakeup");
    let (l, _) = LedgerStore::open(db).unwrap();
    let l = Arc::new(l);
    let parent = provider(&l);
    let child = provider_hold(
        &l,
        "test/provider",
        "child",
        "owner",
        Some(parent.clone()),
        Timestamp::ZERO,
    );
    let (started, ready) = std::sync::mpsc::channel();
    let (finish, wait) = std::sync::mpsc::channel();
    let child_store = l.clone();
    let child_handle = child.clone();
    let thread = std::thread::spawn(move || {
        let mut first = true;
        child_store.release_with_world(
            &child_handle,
            &mut Execute(|_: &CleanupWork| {
                if first {
                    first = false;
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                }
                CleanupOutcome::Confirmed
            }),
            Timestamp::ZERO,
        )
    });
    ready
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let report = l
        .release_with_world(
            &parent,
            &mut Execute(|_: &CleanupWork| panic!("parent must wait")),
            Timestamp::ZERO,
        )
        .unwrap();
    assert_eq!(report.pending.len(), 2);
    finish.send(()).unwrap();
    let report = thread.join().unwrap().unwrap();
    assert_eq!(report.completed, vec![child, parent.clone()]);
    assert!(
        l.holding(parent.id())
            .unwrap()
            .unwrap()
            .released_at()
            .is_some()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn opening_storage_does_not_invent_retirement_for_a_provider_owned_resource() {
    let (db, root) = store("m2-provider-lifetime");
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    let h = provider(&l);
    drop(l);
    let (l, _) = LedgerStore::open(db).unwrap();
    assert!(l.holding(h.id()).unwrap().unwrap().state.is_active());
    assert!(l.cleanup_tasks().unwrap().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn teardown_waits_for_failed_children_and_does_not_refund_spend() {
    let (db, root) = store("m2-parent");
    let (l, _) = LedgerStore::open(db).unwrap();
    let p = provider(&l);
    let c = provider_hold(
        &l,
        "test/provider",
        "child",
        "owner",
        Some(p.clone()),
        Timestamp::ZERO,
    );
    let independent = provider_hold(
        &l,
        "test/provider",
        "independent",
        "owner",
        None,
        Timestamp::ZERO,
    );
    l.transaction(|tx| {
        tx.create_count_pool(
            &"account".into(),
            &"emit".into(),
            &"owner".into(),
            Capacity::new(Count::Value(2)).unwrap(),
        )
        .map(|_| ())
    })
    .unwrap();
    l.spend_many(
        &"owner".into(),
        &[SpendRequest {
            account: "account".into(),
            effect: "emit".into(),
            amount: 2,
        }],
        Timestamp::ZERO,
    )
    .unwrap();
    let mut order = Vec::new();
    let report = l
        .teardown(
            &"owner".into(),
            &mut Execute(|w: &CleanupWork| {
                order.push(w.task().holding.id());
                if w.task().holding == c {
                    CleanupOutcome::Retryable("not yet".into())
                } else {
                    CleanupOutcome::Confirmed
                }
            }),
            Timestamp::ZERO,
        )
        .unwrap();
    assert_eq!(report.completed.len(), 1);
    assert!(order.contains(&independent.id()));
    assert!(!order.contains(&p.id()));
    assert!(
        l.renew(&p, LeaseRequest::Unbounded, Timestamp::ZERO)
            .is_err()
    );
    let mut order = Vec::new();
    l.retry_cleanup(
        &mut Execute(|w: &CleanupWork| {
            order.push(w.task().holding.id());
            CleanupOutcome::Confirmed
        }),
        Timestamp::ZERO,
    )
    .unwrap();
    assert_eq!(order, vec![c.id(), p.id()]);
    assert_eq!(l.spent(&"account".into(), &"emit".into()).unwrap(), 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn external_recovery_grade_does_not_skip_a_physical_cleanup() {
    let (db, root) = store("m2-external");
    let (l, _) = LedgerStore::open(db).unwrap();
    l.transaction(|tx| {
        tx.register_class(ClassDecl {
            cleanup: CleanupPolicy::Managed(CleanupKind::Provider),
            class_id: "external/connection".into(),
            algebra: AlgebraTag::Exclusive,
            release_idempotent: true,
            lease_duration: None,
            revert_grade: RevertGrade::External,
        })
        .map(|_| ())
    })
    .unwrap();
    let h = provider_hold(
        &l,
        "external/connection",
        "socket",
        "owner",
        None,
        Timestamp::ZERO,
    );
    let mut calls = 0;
    let report = l
        .release_with_world(
            &h,
            &mut Execute(|_: &CleanupWork| {
                calls += 1;
                CleanupOutcome::Blocked("provider offline".into())
            }),
            Timestamp::ZERO,
        )
        .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(report.pending.len(), 1);
    assert!(l.holding(h.id()).unwrap().unwrap().state.occupies());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn old_process_incarnation_cannot_signal_a_current_process() {
    let (db, root) = store("m2-pid-reuse");
    let (l, _) = LedgerStore::open(db).unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let current = substrate::capture_process(child.id()).unwrap();
    let old = ProcessWitness::new(
        current.pid(),
        current.start_ticks() + 1,
        current.boot().clone(),
    )
    .unwrap();
    let h = l
        .hold_managed(
            ExclusiveRequest {
                owner: "owner".into(),
                resource: ResourceKey::new(CLASS_PROCESS.into(), "pid-slot".into()),
                generation: format!("{}:{}", old.pid(), old.start_ticks()).into(),
                parent: None,
                lease: LeaseRequest::Unbounded,
            },
            CleanupTarget::Process(old),
            None,
            Timestamp::ZERO,
        )
        .unwrap();
    let report = l
        .release_with_world(&h, &mut substrate::BootstrapWorld, Timestamp::ZERO)
        .unwrap();
    assert!(report.pending.is_empty());
    assert!(child.try_wait().unwrap().is_none());
    assert_eq!(
        l.cleanup_tasks().unwrap()[0].state,
        CleanupState::Done(Completion::AlreadyAbsent)
    );
    child.kill().unwrap();
    child.wait().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_process_can_retire_its_owned_lock_before_it_exits() {
    let (db, root) = store("m2-process-lock");
    let (l, _) = LedgerStore::open(db).unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let w = substrate::capture_process(child.id()).unwrap();
    let p = l
        .hold_substrate(
            ExclusiveRequest {
                owner: "owner".into(),
                resource: ResourceKey::new(CLASS_PROCESS.into(), "process".into()),
                generation: format!("{}:{}", w.pid(), w.start_ticks()).into(),
                parent: None,
                lease: LeaseRequest::Unbounded,
            },
            &json!({"pid":w.pid(),"start":w.start_ticks()}),
            Timestamp::ZERO,
        )
        .unwrap();
    let path = root.join("owned-lock");
    std::fs::write(&path, b"lock").unwrap();
    let c = l
        .hold_substrate(
            ExclusiveRequest {
                owner: "owner".into(),
                resource: ResourceKey::new(CLASS_FILE_LOCK.into(), path.to_str().unwrap().into()),
                generation: "lock".into(),
                parent: Some(p.clone()),
                lease: LeaseRequest::ParentBound,
            },
            &json!({"owner_pid":w.pid(),"owner_start":w.start_ticks()}),
            Timestamp::ZERO,
        )
        .unwrap();
    let mut order = Vec::new();
    let report = l
        .teardown_holding(
            &p,
            &mut Execute(|work: &CleanupWork| {
                if work.task().holding == c {
                    assert!(substrate::process_present(&w).unwrap());
                }
                order.push(work.task().holding.id());
                substrate::execute_target(work)
            }),
            Timestamp::ZERO,
        )
        .unwrap();
    assert_eq!(report.completed.len(), 2);
    assert!(report.pending.is_empty());
    assert_eq!(order, vec![c.id(), p.id()]);
    assert!(!path.exists());
    child.wait().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_exited_group_leader_is_not_absence_while_other_threads_run() {
    let mut child=std::process::Command::new("python3").args(["-c","import threading,time,ctypes; threading.Thread(target=lambda:time.sleep(30)).start(); ctypes.CDLL(None).pthread_exit(None)"]).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut leader_exited = false;
    while std::time::Instant::now() < deadline {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id())).unwrap();
        if stat
            .rsplit_once(')')
            .unwrap()
            .1
            .trim_start()
            .starts_with("Z ")
        {
            leader_exited = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !leader_exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!("fixture group leader did not exit");
    }
    let witness = substrate::capture_process(child.id()).unwrap();
    assert!(substrate::process_present(&witness).unwrap());
    assert!(matches!(
        substrate::cleanup_process(&witness),
        CleanupOutcome::Confirmed
    ));
    child.wait().unwrap();
    assert!(!substrate::process_present(&witness).unwrap());
}

#[test]
fn delayed_old_owner_exit_cannot_collect_a_replacement_instances_grants() {
    let (db, root) = store("m2-old-owner");
    let (l, _) = LedgerStore::open(db).unwrap();
    let host = HostWitness::new(
        ProcessWitness::new(1, 1, "old-boot".into()).unwrap(),
        "host".into(),
    )
    .unwrap();
    let request = |generation: &str| ExclusiveRequest {
        owner: "plugin:same-name".into(),
        resource: ResourceKey::new(CLASS_PLUGIN.into(), "same-name".into()),
        generation: generation.into(),
        parent: None,
        lease: LeaseRequest::Unbounded,
    };
    let old = l
        .hold_managed(
            request("old"),
            CleanupTarget::Plugin {
                host: host.clone(),
                process: host.process().clone(),
            },
            None,
            Timestamp::ZERO,
        )
        .unwrap();
    l.teardown_owner_incarnation(&old, &mut substrate::BootstrapWorld, Timestamp::ZERO)
        .unwrap();
    let new = l
        .hold_managed(
            request("new"),
            CleanupTarget::Plugin {
                host: host.clone(),
                process: host.process().clone(),
            },
            None,
            Timestamp::ZERO,
        )
        .unwrap();
    let grant = l
        .hold_exclusive(
            ExclusiveRequest {
                owner: "plugin:same-name".into(),
                resource: ResourceKey::new(CLASS_CAP.into(), "new-grant".into()),
                generation: "grant".into(),
                parent: None,
                lease: LeaseRequest::Unbounded,
            },
            Timestamp::ZERO,
        )
        .unwrap();
    let report = l
        .teardown_owner_incarnation(
            &old,
            &mut Execute(|_: &CleanupWork| panic!("stale callback reached the world")),
            Timestamp::ZERO,
        )
        .unwrap();
    assert!(report.completed.is_empty());
    assert!(l.holding(new.id()).unwrap().unwrap().state.is_active());
    assert!(l.holding(grant.id()).unwrap().unwrap().state.is_active());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn replaced_lock_file_is_left_untouched_and_keeps_the_pool_occupied() {
    let (db, root) = store("m2-lock-replaced");
    let (l, _) = LedgerStore::open(db).unwrap();
    let path = root.join("lock");
    let h = lock(&l, &path);
    std::fs::rename(&path, root.join("original")).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    let report = l
        .release_with_world(&h, &mut substrate::BootstrapWorld, Timestamp::ZERO)
        .unwrap();
    assert!(matches!(report.pending[0].state, CleanupState::Blocked(_)));
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    assert!(l.holding(h.id()).unwrap().unwrap().state.occupies());
    assert!(
        l.renew(&h, LeaseRequest::Unbounded, Timestamp::ZERO)
            .is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn busy_port_waits_until_absence_is_confirmed() {
    let listener = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (db, root) = store("m2-port");
    let (l, _) = LedgerStore::open(db).unwrap();
    let h = l
        .hold_substrate(
            ExclusiveRequest {
                owner: "owner".into(),
                resource: ResourceKey::new(CLASS_PORT.into(), format!("tcp:{port}").into()),
                generation: "port".into(),
                parent: None,
                lease: LeaseRequest::Unbounded,
            },
            &json!({}),
            Timestamp::ZERO,
        )
        .unwrap();
    let report = l
        .release_with_world(&h, &mut substrate::BootstrapWorld, Timestamp::ZERO)
        .unwrap();
    assert!(matches!(
        report.pending[0].state,
        CleanupState::Retryable(_)
    ));
    drop(listener);
    let report = l
        .retry_cleanup(&mut substrate::BootstrapWorld, Timestamp::ZERO)
        .unwrap();
    assert_eq!(report.completed, vec![h]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_tombstone_with_missing_identity_becomes_blocked_not_confirmed() {
    let (db, root) = store("m2-migration");
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    let path = root.join("old-lock");
    let h = lock(&l, &path);
    drop(l);
    {
        let conn = db.lock().unwrap();
        conn.execute_batch("UPDATE resource_schema SET version=1,realm=NULL; UPDATE resource_classes SET cleanup=NULL; UPDATE holdings SET state=NULL,target=NULL,released_at=1;").unwrap();
        conn.execute(
            "INSERT INTO substrate(holding_id,kind,detail) VALUES(?1,'file-lock','{}')",
            params![h.id().to_sql()],
        )
        .unwrap();
    }
    let (l, report) = LedgerStore::open(db.clone()).unwrap();
    assert_eq!(report.cleanup_pending, 1);
    assert!(path.exists());
    assert!(matches!(
        l.holding(h.id()).unwrap().unwrap().state,
        HoldingState::Retiring(_)
    ));
    let tasks = l.cleanup_tasks().unwrap();
    assert!(matches!(tasks[0].state, CleanupState::Blocked(_)));
    let key = tasks[0].key.clone();
    drop(l);
    let (l, _) = LedgerStore::open(db).unwrap();
    assert_eq!(l.cleanup_tasks().unwrap()[0].key, key);
    assert!(path.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn corrupt_cleanup_state_cannot_become_an_operational_store() {
    let (db, root) = store("m2-corrupt");
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    let h = provider(&l);
    l.request_retirement(&h, Timestamp::ZERO).unwrap();
    drop(l);
    db.lock().unwrap().execute_batch("UPDATE resource_cleanup SET record=json_set(record,'$.generation','another-incarnation')").unwrap();
    assert!(LedgerStore::open(db.clone()).is_err());
    let state: String = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT state FROM holdings WHERE id=?1",
            params![h.id().to_sql()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "retiring");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cleanup_crash_worker() {
    let Ok(root) = std::env::var("PORTOS_M2_CRASH_ROOT") else {
        return;
    };
    let stage = std::env::var("PORTOS_M2_CRASH_STAGE").unwrap();
    let root = std::path::PathBuf::from(root);
    let db = Arc::new(Mutex::new(crate::db::open(&root).unwrap()));
    let (l, _) = LedgerStore::open(db.clone()).unwrap();
    let path = root.join("lock");
    let h = lock(&l, &path);
    if stage == "intent" {
        l.request_retirement(&h, Timestamp::ZERO).unwrap();
        std::process::exit(77);
    }
    if stage == "effect" {
        let _ = l.release_with_world(
            &h,
            &mut Execute(|w: &CleanupWork| {
                assert!(matches!(
                    substrate::execute_target(w),
                    CleanupOutcome::Confirmed
                ));
                std::process::exit(77)
            }),
            Timestamp::ZERO,
        );
        unreachable!();
    }
    if stage == "confirmation" {
        l.request_retirement(&h, Timestamp::ZERO).unwrap();
        let id = l.cleanup_tasks().unwrap()[0].id;
        let worker = HostWitness::new(
            substrate::capture_process(std::process::id()).unwrap(),
            "crash-test".into(),
        )
        .unwrap();
        let work = l
            .transaction(|tx| {
                tx.ledger
                    .claim_cleanup(id, worker, Timestamp::ZERO)
                    .map_err(map_err)
            })
            .unwrap()
            .unwrap();
        assert!(matches!(
            substrate::execute_target(&work),
            CleanupOutcome::Confirmed
        ));
        let _: Result<(), KernelError> = l.transaction(|tx| {
            tx.ledger
                .finish_cleanup(&work, CleanupOutcome::Confirmed, Timestamp::ZERO)
                .map_err(map_err)?;
            persist(tx.sql, &tx.ledger.snapshot())?;
            std::process::exit(77)
        });
        unreachable!();
    }
    l.release_with_world(&h, &mut substrate::BootstrapWorld, Timestamp::ZERO)
        .unwrap();
    std::process::exit(77);
}

#[test]
fn process_interruptions_recover_committed_intent_and_unconfirmed_effects() {
    for stage in ["intent", "effect", "confirmation", "complete"] {
        let (db, root) = store(&format!("m2-crash-{stage}"));
        drop(db);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ledger::m2_tests::cleanup_crash_worker",
                "--nocapture",
            ])
            .env("PORTOS_M2_CRASH_ROOT", &root)
            .env("PORTOS_M2_CRASH_STAGE", stage)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(77));
        let db = Arc::new(Mutex::new(crate::db::open(&root).unwrap()));
        let before: String = db
            .lock()
            .unwrap()
            .query_row("SELECT record FROM resource_cleanup", [], |r| r.get(0))
            .unwrap();
        let before = cleanup_codec::task(&before).unwrap();
        assert_eq!(before.state.is_done(), stage == "complete");
        assert_eq!(root.join("lock").exists(), stage == "intent");
        let (l, report) = LedgerStore::open(db).unwrap();
        assert_eq!(report.cleanup_pending, 0);
        assert!(!root.join("lock").exists());
        let after = l.cleanup_tasks().unwrap();
        assert_eq!(after[0].key, before.key);
        assert!(after[0].state.is_done());
        assert_eq!(
            after[0].attempts,
            if stage == "effect" || stage == "confirmation" {
                2
            } else {
                1
            }
        );
        l.invariant().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
