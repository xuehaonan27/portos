//! ABI v2 end-to-end tests: real child processes over the two-channel wire —
//! chunked artifact streaming (D25), capability-gated invoke (D23/D26),
//! the event bus, ephemeral refs, and the JS protocol client.

use portos_kernel::Kernel;
use portos_kernel::host::Host;
use portos_proto::cap::Constraints;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// --- test boundary ------------------------------------------------------
// A test owns the domain meaning of what it sends and asserts, so it works
// in plain JSON. These wrappers convert at that boundary, which is exactly
// where the typed kernel API expects it to happen.

fn vb(s: &str) -> portos_proto::ids::Verb {
    portos_proto::ids::Verb::parse(s).expect("test verb")
}

fn tp(s: &str) -> portos_proto::ids::Topic {
    portos_proto::ids::Topic::parse(s).expect("test topic")
}

fn pl(v: &serde_json::Value) -> portos_proto::wire::Payload {
    portos_proto::wire::Payload::of(v).expect("test payload")
}

fn js(p: &portos_proto::wire::Payload) -> serde_json::Value {
    p.parse().expect("test payload is json")
}

fn call(
    host: &Host,
    plugin: &portos_proto::ids::PluginName,
    verb: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, portos_kernel::KernelError> {
    host.call(plugin, &vb(verb), pl(&args)).map(|p| js(&p))
}

const ECHO_BIN: &str = env!("CARGO_BIN_EXE_portos-echo");

fn setup(tag: &str) -> (Arc<Kernel>, Host, PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-abi2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
    (kernel, host, root)
}

fn spawn_echo(host: &Host, family: &str) -> portos_proto::ids::PluginName {
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
    assert_eq!(name.as_str(), "portos-echo");

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
    let out = call(&host, &name, "echo::digest", json!([meta.id])).unwrap();
    assert_eq!(out["bytes"].as_u64(), Some(payload.len() as u64));
    let head_hex: String = payload[..32].iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(out["head_hex"].as_str(), Some(head_hex.as_str()));
    assert!(
        serde_json::to_string(&out).unwrap().len() < 512,
        "digest response is a bounded preview, not a payload copy"
    );

    // Two-layer naming: ephemeral refs are plugin-local and go stale there.
    let r = call(&host, &name, "echo::make_ref", json!([])).unwrap();
    let rid = r["ref"].as_str().unwrap().to_string();
    let used = call(&host, &name, "echo::use_ref", json!([rid])).unwrap();
    assert!(used["used"].is_string());
    let stale = call(&host, &name, "echo::use_ref", json!(["e999"]));
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
    let stored = call(&host, &name, "echo::put_pattern", json!([n])).unwrap();
    let id = stored["meta"]["id"].as_str().unwrap().to_string();
    assert_eq!(stored["meta"]["size"].as_u64(), Some(n));
    // …and reads them back out for the digest.
    let out = call(&host, &name, "echo::digest", json!([id])).unwrap();
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

    let relay = |args: Value| call(&host, &a, "echoa::relay", args);

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
    let (_sub, rx) = host.subscribe_local(&tp("echoa::ping"));
    call(&host, &b, "echob::subscribe", json!(["echoa::ping"])).unwrap();

    let out = call(
        &host,
        &a,
        "echoa::publish",
        json!(["echoa::ping", {"k": 1}]),
    )
    .unwrap();
    assert_eq!(out["delivered"].as_u64(), Some(2));

    let ev = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("local subscriber receives the event");
    assert_eq!(ev.topic.as_str(), "echoa::ping");
    assert_eq!(js(&ev.data)["k"].as_u64(), Some(1));

    // The plugin subscriber sees it on its serve channel (poll: delivery is
    // asynchronous through the bounded queue).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let evs = call(&host, &b, "echob::events", json!([])).unwrap();
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
    let (_sub, rx) = host.subscribe_local(&tp("noisy::topic"));
    let mut dropped = false;
    for i in 0..(portos_kernel::host::EVENT_QUEUE + 16) {
        let delivered = host.emit(&tp("noisy::topic"), pl(&json!({"i": i})));
        if delivered == 0 {
            dropped = true;
            break;
        }
    }
    assert!(dropped, "overflowing subscriber must be dropped");
    assert_eq!(
        host.emit(&tp("noisy::topic"), pl(&json!({"late": true}))),
        0
    );
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
    let (_sub, rx) = host.subscribe_local(&tp("echoa::*"));
    call(
        &host,
        &a,
        "echoa::publish",
        json!(["echoa::anything", {"n": 9}]),
    )
    .unwrap();
    let ev = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("wildcard subscriber receives the event");
    assert_eq!(ev.topic.as_str(), "echoa::anything");

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
    let grants = call(&host, &a, "echoa::grants", json!([])).unwrap();
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
    assert_eq!(name.as_str(), "portos-jse");

    // call
    let out = call(&host, &name, "jse::ping", json!(["hi", 2])).unwrap();
    assert_eq!(out["pong"], json!(["hi", 2]));

    // plugin put → kernel-side readback
    let stored = call(&host, &name, "jse::store", json!(["chunked hello from js"])).unwrap();
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
    let fetched = call(&host, &name, "jse::fetch", json!([meta.id])).unwrap();
    assert_eq!(fetched["text"].as_str(), Some("kernel says hi"));

    // plugin emit → local subscriber
    let (_sub, rx) = host.subscribe_local(&tp("jse::tick"));
    let pub_out = call(&host, &name, "jse::publish", json!(["jse::tick", {"n": 7}])).unwrap();
    assert_eq!(pub_out["delivered"].as_u64(), Some(1));
    let ev = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("event from js plugin");
    assert_eq!(js(&ev.data)["n"].as_u64(), Some(7));

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Teardown must not depend on a plugin's goodwill, and must reach past it.
///
/// A driver's real cost is usually its grandchildren — chromium under the
/// browser driver, a pipeline under a shell driver — and `Child::kill` never
/// sees those. Each plugin therefore leads its own process group and
/// teardown signals the group, so a plugin that ignores `shutdown`, or that
/// leaks a process and exits anyway, still leaves nothing behind.
#[test]
fn teardown_collects_a_plugin_s_grandchildren() {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let (_kernel, host, root) = setup("grandchild");
    let pid_file = root.join("grandchild.pid");
    host.spawn(
        Path::new(ECHO_BIN),
        &[],
        &[
            ("PORTOS_ECHO_FAMILY", "echo"),
            ("PORTOS_ECHO_GRANDCHILD", pid_file.to_str().unwrap()),
        ],
    )
    .unwrap();

    let grandchild: i32 = std::fs::read_to_string(&pid_file)
        .expect("the plugin recorded its grandchild")
        .trim()
        .parse()
        .unwrap();
    let alive = || kill(Pid::from_raw(grandchild), None).is_ok();
    assert!(alive(), "grandchild should be running before teardown");

    host.shutdown_all();

    // The group signal is delivered synchronously; give the scheduler a beat
    // to reap before asking.
    for _ in 0..50 {
        if !alive() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !alive(),
        "grandchild {grandchild} survived teardown — the process group was not collected"
    );

    let _ = std::fs::remove_dir_all(&root);
}
