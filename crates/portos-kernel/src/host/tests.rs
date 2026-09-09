use super::resources::{op_hold, op_release, op_renew};
use super::*;
use crate::ledger::ExclusiveRequest;
use crate::ledger::{CLASS_FILE_LOCK, CLASS_PORT, CLASS_PROCESS, CLASS_SUBSCRIPTION};
use portos_proto::resource::HoldingRef;
use portos_rm::identity::{ClassId, Generation, HoldingId, InstanceId, ResourceKey, SubjectId};
use portos_rm::time::{LeaseDuration, LeaseRequest, Timestamp};

fn host(tag: &str) -> (Host, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-host-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = Host::new(kernel, &root.join("sock")).unwrap();
    (host, root)
}

/// A local subscription is a `kernel/subscription` holding: dropping it
/// releases the holding (tombstone, never a delete), and the bus no longer
/// delivers to it.
#[test]
fn unsubscribe_local_releases_the_subscription_holding() {
    let (host, root) = host("unsub");
    let (id, _rx) = host.subscribe_local("t::*").unwrap();
    assert_eq!(
        host.kernel
            .ledger
            .counts(&ClassId::new(CLASS_SUBSCRIPTION))
            .unwrap(),
        (1, 0)
    );
    assert!(host.unsubscribe_local(id));
    assert_eq!(
        host.kernel
            .ledger
            .counts(&ClassId::new(CLASS_SUBSCRIPTION))
            .unwrap(),
        (0, 1),
        "holding tombstoned"
    );
    assert_eq!(
        host.emit("t::x", json!({})),
        0,
        "no delivery after unsubscribe"
    );
    assert!(!host.unsubscribe_local(id), "second drop is a no-op");
    host.kernel.ledger.invariant().unwrap();
    drop(host);
    let _ = std::fs::remove_dir_all(&root);
}

/// The sweeper thread (WP-02) expires a leased holding and runs its world
/// action — the lock file is removed — and audits the sweep. Dropping the
/// host stops the thread (the test returning proves the join).
#[test]
fn sweeper_thread_expires_leased_holdings_and_removes_the_lock_file() {
    let (host, root) = host("sweeper");
    let lock = root.join("test.lock");
    std::fs::write(&lock, b"x").unwrap();
    let now = crate::db::now_unix();
    let id = host
        .kernel
        .ledger
        .hold_substrate(
            ExclusiveRequest {
                owner: SubjectId::new("kernel"),
                resource: ResourceKey::new(
                    ClassId::new(CLASS_FILE_LOCK),
                    InstanceId::new(lock.to_str().unwrap()),
                ),
                generation: Generation::new("g1"),
                parent: None,
                lease: Some(1)
                    .map(|s: u64| LeaseDuration::try_from(s).unwrap())
                    .map(LeaseRequest::For)
                    .unwrap_or_default(),
            },
            &json!({}),
            Timestamp::try_from(now).unwrap(),
        )
        .map(|h| h.id())
        .unwrap();
    host.start_sweeper(std::time::Duration::from_millis(100));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while lock.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "sweeper never expired the lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        host.kernel
            .ledger
            .holding(id)
            .unwrap()
            .unwrap()
            .released_at()
            .is_some(),
        "holding tombstoned by the sweep"
    );
    host.kernel.ledger.invariant().unwrap();
    drop(host); // stops and joins the sweeper
    let events = crate::audit::AuditLog::verify(&root.join("audit.log")).unwrap();
    assert!(
        events.iter().any(|e| e["body"]["event"] == "ledger.swept"
            && e["body"]["classes"]["kernel/file-lock"] == 1),
        "the sweep is audited (never silent)"
    );
    let _ = std::fs::remove_dir_all(&root);
}
#[test]
fn resource_messages_reject_missing_identity_and_bad_numbers_without_mutation() {
    let (host, root) = host("resource-codec");
    let held=op_hold(&host.kernel,&host.inner,"test",&json!({"op":"hold","class":CLASS_PORT,"instance":"tcp:4567","substrate":{},"lease_secs":5}),100).unwrap();
    let reference: HoldingRef = serde_json::from_value(held["ok"].clone()).unwrap();
    assert_eq!(
        reference.id, 0,
        "missing id used to target this real holding"
    );
    let id = HoldingId::try_from(reference.id).unwrap();
    let before = host.kernel.ledger.holding(id).unwrap().unwrap();
    for bad in [
        json!({"generation":reference.generation}),
        json!({"id":reference.id}),
        json!({"id":reference.id,"generation":""}),
        json!({"id":u64::MAX,"generation":reference.generation}),
    ] {
        assert!(op_release(&host.kernel, &host.inner, "test", &bad, 101).is_err());
        assert_eq!(host.kernel.ledger.holding(id).unwrap().unwrap(), before);
    }
    for lease in [json!(-1), json!("5"), json!(u64::MAX)] {
        let bad = json!({"id":reference.id,"generation":reference.generation,"lease_secs":lease});
        assert!(op_renew(&host.kernel, "test", &bad, 101).is_err());
        assert_eq!(host.kernel.ledger.holding(id).unwrap().unwrap(), before);
    }
    for bad in [
        json!({"class":CLASS_PORT,"substrate":{}}),
        json!({"class":CLASS_PORT,"instance":"tcp:4568"}),
        json!({"class":CLASS_PROCESS,"instance":"x","substrate":{"pid":u64::MAX}}),
    ] {
        assert!(op_hold(&host.kernel, &host.inner, "test", &bad, 101).is_err());
        assert_eq!(
            host.kernel
                .ledger
                .live_snapshot(&SubjectId::new("plugin:test"))
                .unwrap()
                .len(),
            1
        );
    }
    let heartbeat = op_renew(
        &host.kernel,
        "test",
        &serde_json::to_value(&reference).unwrap(),
        101,
    )
    .unwrap();
    assert_eq!(heartbeat["ok"]["lease_expires_at"], 105);
    let released = op_release(
        &host.kernel,
        &host.inner,
        "test",
        &serde_json::to_value(reference).unwrap(),
        102,
    )
    .unwrap();
    assert_eq!(released, json!({"ok":{"released":true,"state":"retired"}}));
    assert!(
        host.kernel
            .ledger
            .holding(id)
            .unwrap()
            .unwrap()
            .released_at()
            .is_some()
    );
    drop(host);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn release_response_stays_pending_until_port_absence_is_confirmed() {
    let (host, root) = host("m2-port-response");
    let listener = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let held = op_hold(
        &host.kernel,
        &host.inner,
        "test",
        &json!({"class":CLASS_PORT,"instance":format!("tcp:{port}"),"substrate":{}}),
        100,
    )
    .unwrap();
    let request = held["ok"].clone();
    let first = op_release(&host.kernel, &host.inner, "test", &request, 101).unwrap();
    assert_eq!(first["ok"]["released"], false);
    assert_eq!(first["ok"]["state"], "retryable");
    let id = first["ok"]["cleanup_id"].clone();
    let key = host.kernel.ledger.cleanup_tasks().unwrap()[0].key.clone();
    assert!(op_renew(&host.kernel, "test", &request, 102).is_err());
    let repeated = op_release(&host.kernel, &host.inner, "test", &request, 103).unwrap();
    assert_eq!(repeated["ok"]["cleanup_id"], id);
    assert_eq!(host.kernel.ledger.cleanup_tasks().unwrap()[0].key, key);
    drop(listener);
    let completed = op_release(&host.kernel, &host.inner, "test", &request, 104).unwrap();
    assert_eq!(completed["ok"]["released"], true);
    drop(host);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cleanup_uses_the_subscription_session_not_just_its_numeric_id() {
    let (first, root) = host("m2-subscription-session");
    let (id, old_rx) = first.subscribe_local("topic").unwrap();
    let old = first
        .inner
        .subs
        .lock()
        .unwrap()
        .iter()
        .find(|s| s.id == id)
        .unwrap()
        .holding
        .clone();
    first
        .kernel
        .ledger
        .request_retirement(&old, Timestamp::ZERO)
        .unwrap();
    let second = Host::new(first.kernel.clone(), &root.join("second-sockets")).unwrap();
    let (new_id, new_rx) = second.subscribe_local("topic").unwrap();
    assert_eq!(id, new_id);
    let report = first
        .kernel
        .ledger
        .release_with_world(
            &old,
            &mut HostWorld {
                inner: second.inner.clone(),
            },
            Timestamp::ZERO,
        )
        .unwrap();
    assert!(report.pending.is_empty());
    assert_eq!(second.emit("topic", json!("new")), 1);
    assert_eq!(new_rx.recv().unwrap()["data"], "new");
    assert!(old_rx.try_recv().is_err());
    assert_eq!(first.emit("topic", json!("old")), 0);
    drop(first);
    drop(second);
    std::fs::remove_dir_all(root).unwrap();
}
