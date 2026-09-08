//! ABI v2 end-to-end tests: real child processes over the two-channel wire —
//! chunked artifact streaming (D25), capability-gated invoke (D23/D26),
//! the event bus, ephemeral refs, and the JS protocol client.

use portos_kernel::Kernel;
use portos_kernel::host::{Host, Slot};
use portos_kernel::ledger::ExclusiveRequest;
use portos_kernel::ledger::{CLASS_PLUGIN, CLASS_PROCESS, CLASS_SUBSCRIPTION};
use portos_proto::cap::Constraints;
use portos_rm::identity::{AccountId, ClassId, Generation, InstanceId, ResourceKey, SubjectId};
use portos_rm::time::{LeaseRequest, Timestamp};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const ECHO_BIN: &str = env!("CARGO_BIN_EXE_portos-echo");

fn setup(tag: &str) -> (Arc<Kernel>, Host, PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-abi2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
    (kernel, host, root)
}

fn spawn_echo(host: &Host, family: &str) -> String {
    host.spawn(Path::new(ECHO_BIN), &[], &[("PORTOS_ECHO_FAMILY", family)])
        .unwrap()
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

#[test]
fn call_stream_digest_and_ephemeral_refs() {
    let (kernel, host, root) = setup("digest");
    let name = spawn_echo(&host, "echo");
    assert_eq!(name, "portos-echo");

    // 4 MiB in through the kernel, digested by the plugin over the chunked
    // read path — the payload never rides in a JSON frame.
    let payload = pattern(4 * 1024 * 1024);
    let meta = kernel
        .cas
        .put_stream(
            payload.as_slice(),
            "test/blob",
            portos_proto::Label::public_trusted(),
            "test",
        )
        .unwrap();
    let out = host.call(&name, "echo::digest", json!([meta.id])).unwrap();
    assert_eq!(out["bytes"].as_u64(), Some(payload.len() as u64));
    let head_hex: String = payload[..32].iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(out["head_hex"].as_str(), Some(head_hex.as_str()));
    assert!(
        serde_json::to_string(&out).unwrap().len() < 512,
        "digest response is a bounded preview, not a payload copy"
    );

    // Two-layer naming: ephemeral refs are plugin-local and go stale there.
    let r = host.call(&name, "echo::make_ref", json!([])).unwrap();
    let rid = r["ref"].as_str().unwrap().to_string();
    let used = host.call(&name, "echo::use_ref", json!([rid])).unwrap();
    assert!(used["used"].is_string());
    let stale = host.call(&name, "echo::use_ref", json!(["e999"]));
    assert!(stale.is_err(), "stale ephemeral ref must be rejected");

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Acceptance: a large payload crosses the data plane in both directions
/// (plugin put + plugin read) while the control plane stays tiny.
/// PORTOS_ACCEPT_MB overrides the size (default 8 MiB).
#[test]
fn accept_zero_context_data_plane() {
    let (_kernel, host, root) = setup("accept");
    let name = spawn_echo(&host, "echo");

    let mb: u64 = std::env::var("PORTOS_ACCEPT_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let n = mb * 1024 * 1024;

    // Plugin streams n bytes INTO the CAS through its client channel…
    let stored = host.call(&name, "echo::put_pattern", json!([n])).unwrap();
    let id = stored["meta"]["id"].as_str().unwrap().to_string();
    assert_eq!(stored["meta"]["size"].as_u64(), Some(n));
    // …and reads them back out for the digest.
    let out = host.call(&name, "echo::digest", json!([id])).unwrap();
    assert_eq!(out["bytes"].as_u64(), Some(n));

    let (context, data) = host.meter();
    eprintln!(
        "health metric: context={context}B data={data}B ratio={:.2e}",
        context as f64 / data as f64
    );
    assert!(
        data >= 2 * n,
        "both directions count as data: {data} < {}",
        2 * n
    );
    assert!(
        context < 8 * 1024,
        "control-plane bytes stay tiny: {context}"
    );
    let ratio = context as f64 / data as f64;
    assert!(ratio < 1e-3, "context/data ratio too high: {ratio}");

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invoke_is_capability_gated_routed_and_audited() {
    let (kernel, host, root) = setup("invoke");
    let a = spawn_echo(&host, "echoa");
    let b = spawn_echo(&host, "echob");
    assert_eq!((a.as_str(), b.as_str()), ("portos-echoa", "portos-echob"));

    // Grant A two emits on the echob driver — and nothing else.
    let mut counts = BTreeMap::new();
    counts.insert("emit".to_string(), 2u64);
    kernel
        .caps
        .mint(
            "plugin:portos-echoa",
            "driver:echob",
            BTreeSet::from(["emit".to_string()]),
            Constraints {
                expires_at: None,
                counts,
            },
            None,
        )
        .unwrap();

    let relay = |args: Value| host.call(&a, "echoa::relay", args);

    // Two invokes pass, the third exhausts the counting budget.
    assert!(relay(json!(["echob::emit", ["one"]])).is_ok());
    assert!(relay(json!(["echob::emit", ["two"]])).is_ok());
    let third = relay(json!(["echob::emit", ["three"]]));
    assert!(third.is_err(), "counting budget must not overdraw");

    // A verb the grant does not cover is denied outright.
    let denied = relay(json!(["echob::make_ref", []]));
    assert!(denied.is_err(), "ungranted verb must be denied");

    // Both outcomes are on the audit chain.
    drop(host);
    let entries = portos_kernel::audit::AuditLog::verify(&root.join("audit.log")).unwrap();
    let events: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["body"]["event"].as_str())
        .collect();
    assert!(events.iter().filter(|e| **e == "invoke.allowed").count() >= 2);
    assert!(events.iter().filter(|e| **e == "invoke.denied").count() >= 2);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn events_flow_to_local_and_plugin_subscribers() {
    let (_kernel, host, root) = setup("events");
    let a = spawn_echo(&host, "echoa");
    let b = spawn_echo(&host, "echob");

    // A local (in-process) subscriber and a plugin subscriber on one topic.
    let (_sub, rx) = host.subscribe_local("echoa::ping").unwrap();
    host.call(&b, "echob::subscribe", json!(["echoa::ping"]))
        .unwrap();

    let out = host
        .call(&a, "echoa::publish", json!(["echoa::ping", {"k": 1}]))
        .unwrap();
    assert_eq!(out["delivered"].as_u64(), Some(2));

    let ev = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("local subscriber receives the event");
    assert_eq!(ev["topic"].as_str(), Some("echoa::ping"));
    assert_eq!(ev["data"]["k"].as_u64(), Some(1));

    // The plugin subscriber sees it on its serve channel (poll: delivery is
    // asynchronous through the bounded queue).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let evs = host.call(&b, "echob::events", json!([])).unwrap();
        if let Some(list) = evs.as_array() {
            if list
                .iter()
                .any(|e| e["topic"] == "echoa::ping" && e["data"]["k"] == 1)
            {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "plugin subscriber never saw the event"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn slow_local_subscriber_is_dropped_not_wedged() {
    let (_kernel, host, root) = setup("overflow");
    // Subscribe and never drain: the bounded queue fills, the subscriber is
    // dropped, and later emits simply deliver to nobody.
    let (_sub, rx) = host.subscribe_local("noisy::topic").unwrap();
    let mut dropped = false;
    for i in 0..(portos_kernel::host::EVENT_QUEUE + 16) {
        let delivered = host.emit("noisy::topic", json!({"i": i}));
        if delivered == 0 {
            dropped = true;
            break;
        }
    }
    assert!(dropped, "overflowing subscriber must be dropped");
    assert_eq!(host.emit("noisy::topic", json!({"late": true})), 0);
    // Whatever was queued before the drop is still readable.
    assert!(rx.try_recv().is_ok());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn wildcard_topics_and_grants_introspection() {
    let (kernel, host, root) = setup("grants");
    let a = spawn_echo(&host, "echoa");
    let b = spawn_echo(&host, "echob");

    // Wildcard subscription: a prefix pattern sees every matching topic.
    let (_sub, rx) = host.subscribe_local("echoa::*").unwrap();
    host.call(&a, "echoa::publish", json!(["echoa::anything", {"n": 9}]))
        .unwrap();
    let ev = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("wildcard subscriber receives the event");
    assert_eq!(ev["topic"].as_str(), Some("echoa::anything"));

    // Grants introspection: A's live caps joined with B's advertised verb
    // metadata — a ready-made tool definition, no config duplication.
    let mut counts = BTreeMap::new();
    counts.insert("emit".to_string(), 5u64);
    kernel
        .caps
        .mint(
            "plugin:portos-echoa",
            "driver:echob",
            BTreeSet::from(["emit".to_string(), "digest".to_string()]),
            Constraints {
                expires_at: None,
                counts,
            },
            None,
        )
        .unwrap();
    let grants = host.call(&a, "echoa::grants", json!([])).unwrap();
    let list = grants.as_array().unwrap();
    let emit = list
        .iter()
        .find(|g| g["verb"] == "echob::emit")
        .expect("granted verb introspected");
    assert!(
        emit["description"]
            .as_str()
            .unwrap()
            .contains("Print a line"),
        "driver-advertised description joined in"
    );
    assert!(emit["schema"]["properties"]["text"].is_object());
    assert_eq!(emit["counts_left"].as_u64(), Some(5));
    let digest = list
        .iter()
        .find(|g| g["verb"] == "echob::digest")
        .expect("verb without advertised metadata still listed");
    assert_eq!(digest["schema"], json!({"type": "object"}));
    assert!(
        digest.get("counts_left").is_none(),
        "uncounted grant is unlimited"
    );
    assert_eq!(list.len(), 2, "only granted verbs appear");
    drop(b);

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

fn audit_events(root: &Path) -> Vec<Value> {
    portos_kernel::audit::AuditLog::verify(&root.join("audit.log"))
        .unwrap()
        .into_iter()
        .map(|e| e["body"].clone())
        .collect()
}

/// F1 wired: a counting budget is a pool of rows. The declared capacity is
/// never mutated, each exercise is a spend row through the issuer gate, the
/// balance is a fold — and the rows are durable: reopening the kernel picks
/// the budget up where it was.
///
/// WP-03 sharpens whose budget that is: a grant is a `kernel/cap` holding of
/// its grantee, so a plugin-held grant dies with the plugin's teardown (the
/// issuer — chat.json, a consent — re-mints on the next start), while a grant
/// anchored to a stable subject (here `session:cli`) survives with its budget
/// intact.
#[test]
fn counting_budget_is_ledger_rows_and_survives_kernel_reopen() {
    let (kernel, host, root) = setup("rows");
    let a = spawn_echo(&host, "echoa");
    let _b = spawn_echo(&host, "echob");
    let counts = BTreeMap::from([("emit".to_string(), 2u64)]);
    let cap = kernel
        .caps
        .mint(
            "plugin:portos-echoa",
            "driver:echob",
            BTreeSet::from(["emit".to_string()]),
            Constraints {
                expires_at: None,
                counts: counts.clone(),
            },
            None,
        )
        .unwrap();
    // A standing grant anchored to a non-plugin subject: no teardown takes it.
    let standing = kernel
        .caps
        .mint(
            "session:cli",
            "driver:echob",
            BTreeSet::from(["emit".to_string()]),
            Constraints {
                expires_at: None,
                counts,
            },
            None,
        )
        .unwrap();
    host.call(&a, "echoa::relay", json!(["echob::emit", ["one"]]))
        .unwrap();
    kernel.caps.exercise(&standing.cap_id, "emit", 1).unwrap();
    let stored = kernel.caps.get(&cap.cap_id).unwrap();
    assert_eq!(
        stored.constraints.counts["emit"], 2,
        "capacity is immutable"
    );
    assert_eq!(
        kernel.caps.counts_left(&stored, "emit").unwrap(),
        Some(1),
        "balance = capacity − spend rows"
    );
    kernel.ledger.invariant().unwrap();
    host.shutdown_all();
    // WP-03: a grant is exactly as alive as its holder — the plugin's teardown
    // tombstoned its holding; the standing grant's holding survives.
    assert!(
        kernel
            .ledger
            .cap_holding(&AccountId::new(&cap.cap_id))
            .unwrap()
            .is_none(),
        "grant died with its holder"
    );
    assert!(
        kernel
            .ledger
            .cap_holding(&AccountId::new(&standing.cap_id))
            .unwrap()
            .is_some()
    );
    drop(host);
    drop(kernel);
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Reopen the same root: spend rows reload, plugin rows of the previous
    // process are tombstoned. The plugin-held grant stays dead (the gate
    // refuses it); the standing grant's budget continues where it was.
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
    assert!(
        kernel.caps.exercise(&cap.cap_id, "emit", 2).is_err(),
        "dead grant stays dead"
    );
    let stored = kernel.caps.get(&standing.cap_id).unwrap();
    assert_eq!(
        kernel.caps.counts_left(&stored, "emit").unwrap(),
        Some(1),
        "budget continues across restart"
    );
    kernel.caps.exercise(&standing.cap_id, "emit", 2).unwrap();
    assert!(
        kernel.caps.exercise(&standing.cap_id, "emit", 2).is_err(),
        "gate refuses at capacity"
    );
    assert_eq!(kernel.caps.counts_left(&stored, "emit").unwrap(), Some(0));
    kernel.ledger.invariant().unwrap();
    host.shutdown_all();
    drop(host);
    let events = audit_events(&root);
    assert!(
        !events.iter().any(|e| e["event"] == "ledger.reconciled"),
        "a graceful shutdown leaves no stale rows for the reopen to reconcile"
    );
    assert_eq!(
        kernel.ledger.counts(&ClassId::new(CLASS_PLUGIN)).unwrap(),
        (0, 2),
        "both plugins tombstoned"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// F2 wired: a plugin and its subscriptions are holdings; when the process
/// dies, the one crash-only teardown path releases them children first,
/// drops its routes and subscriptions, and audits. Graceful shutdown ends in
/// the same ledger shape.
#[test]
fn plugin_death_is_reclaimed_crash_only_children_first() {
    let (kernel, host, root) = setup("death");
    let a = spawn_echo(&host, "echoa");
    let b = spawn_echo(&host, "echob");
    host.call(&a, "echoa::subscribe", json!(["echob::ping"]))
        .unwrap();
    let subject = "plugin:portos-echoa";
    assert_eq!(
        kernel
            .ledger
            .live_closure(&SubjectId::new(subject))
            .unwrap()
            .len(),
        2,
        "plugin holding + subscription holding"
    );
    assert_eq!(
        host.call(&b, "echob::publish", json!(["echob::ping", {"n": 1}]))
            .unwrap()["delivered"],
        1
    );

    // The plugin dies without notice.
    let pid = host.pid(&a).expect("pid");
    std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !kernel
        .ledger
        .live_snapshot(&SubjectId::new(subject))
        .unwrap()
        .is_empty()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "death never reclaimed"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        host.call(&a, "echoa::events", json!([])).is_err(),
        "handle gone"
    );
    assert!(
        host.call_verb("echoa::emit", json!(["x"])).is_err(),
        "route gone"
    );
    assert_eq!(
        host.call(&b, "echob::publish", json!(["echob::ping", {"n": 2}]))
            .unwrap()["delivered"],
        0,
        "subscription gone with its holder"
    );
    assert_eq!(
        kernel.ledger.counts(&ClassId::new(CLASS_PLUGIN)).unwrap(),
        (1, 1),
        "echob live, echoa tombstoned"
    );
    assert_eq!(
        kernel
            .ledger
            .counts(&ClassId::new(CLASS_SUBSCRIPTION))
            .unwrap(),
        (0, 1)
    );
    kernel.ledger.invariant().unwrap();

    // Graceful shutdown is the same path, triggered early.
    host.shutdown(&b);
    assert_eq!(
        kernel.ledger.counts(&ClassId::new(CLASS_PLUGIN)).unwrap(),
        (0, 2)
    );
    drop(host);
    let events = audit_events(&root);
    let reclaimed: Vec<&Value> = events
        .iter()
        .filter(|e| e["event"] == "plugin.reclaimed")
        .collect();
    assert!(
        reclaimed.iter().any(|e| e["plugin"] == "portos-echoa"
            && e["reason"] == "exited"
            && e["released"] == 2),
        "death reclaimed both rows on the exited path: {reclaimed:?}"
    );
    assert!(
        reclaimed
            .iter()
            .any(|e| e["plugin"] == "portos-echob" && e["reason"] == "shutdown")
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// F4 wired: the verb character a plugin declares in its hello is checked by
/// the truth table at the door (an incoherent declaration refuses the spawn
/// and leaves nothing behind) and surfaces in grants introspection.
#[test]
fn verb_kind_metadata_is_checked_at_spawn_and_exposed_in_grants() {
    let (kernel, host, root) = setup("kinds");
    let bad = host.spawn(
        Path::new(ECHO_BIN),
        &[],
        &[
            ("PORTOS_ECHO_FAMILY", "echobad"),
            ("PORTOS_ECHO_BAD_KIND", "1"),
        ],
    );
    let err = bad
        .err()
        .expect("incoherent verb metadata must refuse the spawn")
        .to_string();
    assert!(err.contains("verb metadata rejected"), "{err}");
    assert!(
        kernel
            .ledger
            .live_snapshot(&SubjectId::new("plugin:portos-echobad"))
            .unwrap()
            .is_empty(),
        "nothing held"
    );
    assert!(
        host.call_verb("echobad::emit", json!(["x"])).is_err(),
        "nothing routed"
    );

    let a = spawn_echo(&host, "echoa");
    let _b = spawn_echo(&host, "echob");
    let mut counts = BTreeMap::new();
    counts.insert("emit".to_string(), 3u64);
    kernel
        .caps
        .mint(
            "plugin:portos-echoa",
            "driver:echob",
            BTreeSet::from([
                "emit".to_string(),
                "digest".to_string(),
                "relay".to_string(),
            ]),
            Constraints {
                expires_at: None,
                counts,
            },
            None,
        )
        .unwrap();
    let grants = host.call(&a, "echoa::grants", json!([])).unwrap();
    let list = grants.as_array().unwrap();
    let find = |v: &str| list.iter().find(|g| g["verb"] == v).cloned().unwrap();
    assert_eq!(find("echob::emit")["kind"], "emitting");
    assert_eq!(find("echob::emit")["budgeted"], true);
    assert_eq!(find("echob::digest")["kind"], "repeatable");
    assert_eq!(find("echob::digest")["budgeted"], false);
    assert!(
        find("echob::relay").get("kind").is_none(),
        "verb without a declared kind carries none"
    );

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// F5 wired: a slot's row is the position ceiling. A plugin whose declared
/// requires do not fit the row is refused at the door; a plugin inside a
/// row cannot invoke outside it even when it holds the capability.
#[test]
fn slot_row_bounds_invoke_and_admits_requires() {
    let (kernel, host, root) = setup("slot");
    let _b = spawn_echo(&host, "echob");

    let narrow = Slot {
        offers: vec!["echob::digest".into()],
        provides: vec![],
    };
    let refused = host.spawn_in(
        Path::new(ECHO_BIN),
        &[],
        &[
            ("PORTOS_ECHO_FAMILY", "echoa"),
            ("PORTOS_ECHO_RELAY_REQUIRES", "echob::emit"),
        ],
        Some(&narrow),
    );
    let err = refused
        .err()
        .expect("requires outside the row must refuse the spawn")
        .to_string();
    assert!(
        err.contains("slot admission failed") && err.contains("VerbExceedsRow"),
        "{err}"
    );

    let unmet = host.spawn_in(
        Path::new(ECHO_BIN),
        &[],
        &[
            ("PORTOS_ECHO_FAMILY", "echoa"),
            ("PORTOS_ECHO_RELAY_DEPS", "nosuch"),
        ],
        Some(&Slot {
            offers: vec![],
            provides: vec![],
        }),
    );
    let err = unmet
        .err()
        .expect("unmet dependency must refuse the spawn")
        .to_string();
    assert!(err.contains("MissingDependency"), "{err}");

    // Fits: relay requires echob::emit and depends on the echob family
    // (already routed). Then the row bounds invoke regardless of grants.
    let slot = Slot {
        offers: vec!["echob::emit".into()],
        provides: vec![],
    };
    let a = host
        .spawn_in(
            Path::new(ECHO_BIN),
            &[],
            &[
                ("PORTOS_ECHO_FAMILY", "echoa"),
                ("PORTOS_ECHO_RELAY_REQUIRES", "echob::emit"),
                ("PORTOS_ECHO_RELAY_DEPS", "echob"),
            ],
            Some(&slot),
        )
        .unwrap();
    kernel
        .caps
        .mint(
            "plugin:portos-echoa",
            "driver:echob",
            BTreeSet::from(["emit".to_string(), "make_ref".to_string()]),
            Constraints::default(),
            None,
        )
        .unwrap();
    assert!(
        host.call(&a, "echoa::relay", json!(["echob::emit", ["in row"]]))
            .is_ok()
    );
    let out = host.call(&a, "echoa::relay", json!(["echob::make_ref", []]));
    assert!(out.is_err(), "granted but outside the row: refused");
    host.shutdown_all();
    drop(host);
    let events = audit_events(&root);
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "invoke.denied" && e["reason"] == "verb outside the slot row")
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// F6 wired: a declared verb-order protocol is a safety property the kernel
/// enforces precisely — the offending call is refused, state advances only
/// on success.
#[test]
fn protocol_order_is_enforced_at_call() {
    let (_kernel, host, root) = setup("proto");
    let name = host
        .spawn(
            Path::new(ECHO_BIN),
            &[],
            &[
                ("PORTOS_ECHO_FAMILY", "echo"),
                ("PORTOS_ECHO_PROTOCOL", "1"),
            ],
        )
        .unwrap();
    let early = host.call(&name, "echo::use_ref", json!(["e1"]));
    let err = early
        .err()
        .expect("use_ref before make_ref violates the protocol")
        .to_string();
    assert!(err.contains("protocol violation"), "{err}");
    let r = host.call(&name, "echo::make_ref", json!([])).unwrap();
    let rid = r["ref"].as_str().unwrap().to_string();
    assert!(host.call(&name, "echo::use_ref", json!([rid])).is_ok());
    // A plugin-side error does not move the automaton: still open afterwards.
    assert!(host.call(&name, "echo::use_ref", json!(["e999"])).is_err());
    assert!(host.call(&name, "echo::make_ref", json!([])).is_ok());
    // Verbs outside the protocol's scope are unaffected.
    assert!(host.call(&name, "echo::events", json!([])).is_ok());
    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Cross-language: the JS protocol client speaks the same wire. Skips when
/// node is not installed.
#[test]
fn js_plugin_speaks_abi_v2() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping js_plugin_speaks_abi_v2: node not found");
        return;
    }
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/js-plugin.mjs");
    let (kernel, host, root) = setup("js");
    let name = host.spawn(Path::new("node"), &[fixture], &[]).unwrap();
    assert_eq!(name, "portos-jse");

    // call
    let out = host.call(&name, "jse::ping", json!(["hi", 2])).unwrap();
    assert_eq!(out["pong"], json!(["hi", 2]));

    // plugin put → kernel-side readback
    let stored = host
        .call(&name, "jse::store", json!(["chunked hello from js"]))
        .unwrap();
    let id = stored["meta"]["id"].as_str().unwrap().to_string();
    let mut f = kernel.cas.open_read(&id).unwrap();
    let mut s = String::new();
    use std::io::Read;
    f.read_to_string(&mut s).unwrap();
    assert_eq!(s, "chunked hello from js");

    // kernel put → plugin chunked read
    let meta = kernel
        .cas
        .put_bytes(
            b"kernel says hi",
            "text/plain",
            portos_proto::Label::public_trusted(),
            "test",
        )
        .unwrap();
    let fetched = host.call(&name, "jse::fetch", json!([meta.id])).unwrap();
    assert_eq!(fetched["text"].as_str(), Some("kernel says hi"));

    // plugin emit → local subscriber
    let (_sub, rx) = host.subscribe_local("jse::tick").unwrap();
    let pub_out = host
        .call(&name, "jse::publish", json!(["jse::tick", {"n": 7}]))
        .unwrap();
    assert_eq!(pub_out["delivered"].as_u64(), Some(1));
    let ev = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("event from js plugin");
    assert_eq!(ev["data"]["n"].as_u64(), Some(7));

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// WP-04: a plugin spawns a child only when it holds the `kernel:spawn`
/// capability; the child's `kernel/plugin` holding is parented under the
/// caller's holding (decision 2: instantiation edge, declared by the kernel
/// at spawn), and the parent's death reclaims the whole closure — the child
/// dies with it, children first.
#[test]
fn spawn_child_is_capability_gated_and_parent_death_reclaims_children_first() {
    let (kernel, host, root) = setup("child");
    let parent = spawn_echo(&host, "echop");

    // No capability: the kernel refuses and audits the denial.
    let denied = host.call(&parent, "echop::spawn_child", json!(["echoc"]));
    assert!(
        denied.is_err(),
        "spawn_child without kernel:spawn must be refused"
    );
    assert!(
        kernel
            .ledger
            .live_snapshot(&SubjectId::new("plugin:portos-echoc"))
            .unwrap()
            .is_empty(),
        "nothing spawned"
    );

    // With the capability the child spawns, parented under the caller.
    kernel
        .caps
        .mint(
            "plugin:portos-echop",
            "kernel:spawn",
            BTreeSet::from(["spawn_child".to_string()]),
            Constraints::default(),
            None,
        )
        .unwrap();
    let out = host
        .call(&parent, "echop::spawn_child", json!(["echoc"]))
        .unwrap();
    assert_eq!(out["name"].as_str(), Some("portos-echoc"));
    let p_row = kernel
        .ledger
        .live_snapshot(&SubjectId::new("plugin:portos-echop"))
        .unwrap()
        .into_iter()
        .find(|it| it.class_id.as_str() == CLASS_PLUGIN)
        .expect("parent row");
    let c_row = kernel
        .ledger
        .live_snapshot(&SubjectId::new("plugin:portos-echoc"))
        .unwrap()
        .into_iter()
        .find(|it| it.class_id.as_str() == CLASS_PLUGIN)
        .expect("child row");
    assert_eq!(
        c_row.parent,
        Some(p_row.id),
        "child holding parented under the parent's"
    );
    assert_eq!(
        kernel
            .ledger
            .live_closure(&SubjectId::new("plugin:portos-echop"))
            .unwrap()
            .len(),
        3,
        "the parent's ownership closure is parent + its spawn cap (WP-03) + child"
    );
    assert_eq!(
        host.call("portos-echoc", "echoc::events", json!([]))
            .unwrap(),
        json!([]),
        "the child answers calls"
    );

    // The parent dies without notice: the one teardown path takes the whole
    // closure, children first — the child process is reaped with it.
    let cpid = host.pid("portos-echoc").expect("child pid");
    let ppid = host.pid(&parent).expect("parent pid");
    std::process::Command::new("kill")
        .args(["-9", &ppid.to_string()])
        .status()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while kernel.ledger.counts(&ClassId::new(CLASS_PLUGIN)).unwrap().0 > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "parent death never reclaimed the child"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let alive = |pid: u32| {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    assert!(!alive(cpid), "child process reaped with its parent");
    assert_eq!(
        kernel.ledger.counts(&ClassId::new(CLASS_PLUGIN)).unwrap(),
        (0, 2),
        "both tombstoned"
    );
    kernel.ledger.invariant().unwrap();

    host.shutdown_all();
    drop(host);
    let events = audit_events(&root);
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "spawn_child.denied" && e["from"] == "portos-echop"),
        "the denial is audited"
    );
    assert!(
        events.iter().any(|e| e["event"] == "plugin.spawned"
            && e["plugin"] == "portos-echoc"
            && e["parent"] == "portos-echop"),
        "the spawn names its parent"
    );
    assert!(
        events.iter().any(|e| e["event"] == "plugin.reclaimed"
            && e["plugin"] == "portos-echop"
            && e["released"] == 3),
        "one teardown released the child, the parent's spawn cap and the parent"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// WP-02: a plugin registers its child process as a `kernel/process` holding
/// (parented under its own plugin holding); when the plugin is kill -9'd, the
/// one crash-only teardown path kills the exact witnessed incarnation —
/// nothing leaks.
#[test]
fn plugin_registers_a_child_process_holding_and_kill_minus_nine_reaps_it() {
    let (kernel, host, root) = setup("holdproc");
    let a = spawn_echo(&host, "echoa");
    let out = host.call(&a, "echoa::hold_process", json!([300])).unwrap();
    let sleeper = out["pid"].as_u64().unwrap() as u32;

    // The sleeper is alive, on the ledger, a child of the plugin holding.
    let subject = "plugin:portos-echoa";
    assert_eq!(
        kernel.ledger.counts(&ClassId::new(CLASS_PROCESS)).unwrap(),
        (1, 0)
    );
    assert_eq!(
        kernel
            .ledger
            .live_closure(&SubjectId::new(subject))
            .unwrap()
            .len(),
        2,
        "plugin row + process row"
    );
    let alive = |pid: u32| {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    assert!(alive(sleeper));

    // kill -9 the plugin: reclaim must kill the witnessed incarnation too.
    let ppid = host.pid(&a).expect("plugin pid");
    std::process::Command::new("kill")
        .args(["-9", &ppid.to_string()])
        .status()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while kernel
        .ledger
        .counts(&ClassId::new(CLASS_PROCESS))
        .unwrap()
        .0
        > 0
    {
        assert!(
            std::time::Instant::now() < deadline,
            "process holding never reclaimed"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    while alive(sleeper) {
        assert!(std::time::Instant::now() < deadline, "sleeper never died");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        kernel.ledger.counts(&ClassId::new(CLASS_PLUGIN)).unwrap(),
        (0, 1)
    );
    kernel.ledger.invariant().unwrap();

    host.shutdown_all();
    drop(host);
    let events = audit_events(&root);
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "substrate.held" && e["class"] == "kernel/process"),
        "the hold is audited"
    );
    assert!(
        events.iter().any(|e| e["event"] == "plugin.reclaimed"
            && e["plugin"] == "portos-echoa"
            && e["released"] == 2),
        "one teardown released plugin row and process row together"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// WP-03: revocation cascades along the derivation tree — an attenuated
/// child capability dies with its parent grant — and the teardown is scoped
/// to the grant's ownership subtree: holdings that exist because of the
/// grant go with it, while the plugin holding the grant keeps serving.
#[test]
fn attenuated_child_capability_dies_with_its_parent_grant() {
    let (kernel, host, root) = setup("casc");
    let a = spawn_echo(&host, "echoa");
    let _b = spawn_echo(&host, "echob");
    let subject = format!("plugin:{a}");
    let parent = kernel
        .caps
        .mint(
            &subject,
            "driver:echob",
            BTreeSet::from(["emit".to_string()]),
            Constraints {
                expires_at: None,
                counts: BTreeMap::from([("emit".to_string(), 4u64)]),
            },
            None,
        )
        .unwrap();
    // A child narrowed in budget, held by another subject.
    let kid = kernel
        .caps
        .attenuate(
            &parent.cap_id,
            "plugin:kid",
            BTreeSet::from(["emit".to_string()]),
            Constraints {
                expires_at: None,
                counts: BTreeMap::from([("emit".to_string(), 2u64)]),
            },
        )
        .unwrap();
    kernel.caps.exercise(&kid.cap_id, "emit", 1).unwrap();
    let parent_holding = kernel
        .ledger
        .cap_holding(&AccountId::new(&parent.cap_id))
        .unwrap()
        .unwrap()
        .id;
    let kid_holding = kernel
        .ledger
        .cap_holding(&AccountId::new(&kid.cap_id))
        .unwrap()
        .unwrap()
        .id;
    // A dependent under the parent grant's holding (WP-08's pools and routes
    // will hang exactly here).
    let dep = kernel
        .ledger
        .hold_exclusive(
            ExclusiveRequest {
                owner: SubjectId::new(&subject),
                resource: ResourceKey::new(
                    ClassId::new(CLASS_SUBSCRIPTION),
                    InstanceId::new("route-1"),
                ),
                generation: Generation::new("dep"),
                parent: Some(parent_holding)
                    .map(|id| kernel.ledger.holding(id).unwrap().unwrap().handle()),
                lease: LeaseRequest::UseClassDefault,
            },
            Timestamp::try_from(1u64).unwrap(),
        )
        .map(|h| h.id())
        .unwrap();

    let n = host.revoke_capability(&parent.cap_id).unwrap();
    assert_eq!(n, 2, "parent and child both revoked");
    for id in [parent_holding, kid_holding, dep] {
        assert!(
            kernel
                .ledger
                .holding(id)
                .unwrap()
                .unwrap()
                .released_at
                .is_some(),
            "holding {id} tombstoned by the cascade"
        );
    }
    assert!(
        kernel.caps.exercise(&kid.cap_id, "emit", 2).is_err(),
        "dead child cap refused"
    );
    // The revocation is subtree-scoped: the plugin holding the grant keeps
    // serving (its own holdings were never in the grant's tree).
    assert_eq!(
        host.call(&a, "echoa::events", json!([])).unwrap(),
        json!([])
    );
    kernel.ledger.invariant().unwrap();

    host.shutdown_all();
    drop(host);
    let events = audit_events(&root);
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "cap.revoked" && e["cap"] == parent.cap_id && e["cascade"] == 2),
        "the revocation is audited"
    );
    let _ = std::fs::remove_dir_all(&root);
}
