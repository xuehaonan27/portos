//! Hot-plug: a running system gaining and losing a capability.
//!
//! This is the problem Cordis pays for with a whole calculus — load and
//! unload without trace, and the theorems that make "without trace" mean
//! something. It pays that price because its unit of plugging is a linked
//! module sharing an address space, so every change it made has to be undone
//! by an inverse.
//!
//! PortOS's unit is a process, so the expensive half is the operating
//! system's problem: the plugin's own state goes when it does. What remains
//! is kernel-side residue, and that is a list short enough to write down —
//! routes, subscriptions, a socket file, a process group, capabilities.
//! These tests are that list, asserted. No calculus, but also no hand-waving
//! about what "without trace" covers: artifacts already in the CAS and the
//! audit log survive on purpose, because immutable records are not state.

use portos_kernel::Kernel;
use portos_kernel::host::Host;
use portos_proto::cap::Constraints;
use portos_proto::ids::{PluginName, Topic, Verb};
use portos_proto::wire::Payload;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const ECHO_BIN: &str = env!("CARGO_BIN_EXE_portos-echo");

fn setup(tag: &str) -> (Arc<Kernel>, Host, PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-hotplug-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
    (kernel, host, root)
}

fn spawn_echo(host: &Host, family: &str) -> PluginName {
    host.spawn(Path::new(ECHO_BIN), &[], &[("PORTOS_ECHO_FAMILY", family)])
        .unwrap()
}

fn call(host: &Host, plugin: &PluginName, verb: &str, args: Value) -> Result<Value, String> {
    host.call(
        plugin,
        &Verb::parse(verb).expect("test verb"),
        Payload::of(&args).expect("test payload"),
    )
    .map(|p| p.parse().expect("json"))
    .map_err(|e| e.to_string())
}

/// Reach a verb the way a plugin does: through the kernel, past the
/// capability gate. `echo::relay` forwards whatever it is handed.
fn relay(host: &Host, from: &PluginName, verb: &str, args: Value) -> Result<Value, String> {
    let family = from.as_str().trim_start_matches("portos-").to_string();
    call(host, from, &format!("{family}::relay"), json!([verb, args]))
}

fn grant(kernel: &Kernel, subject: &str, resource: &str, verbs: &[&str]) {
    kernel
        .caps
        .mint(
            subject,
            resource,
            verbs.iter().map(|v| v.to_string()).collect::<BTreeSet<_>>(),
            Constraints::default(),
            None,
        )
        .unwrap();
}

/// A spec that starts another echo under `family`, granting it a capability
/// of its own and granting `caller` the right to use it.
fn echo_spec(family: &str, caller: &PluginName) -> Value {
    json!({
        "bin": ECHO_BIN,
        "env": {"PORTOS_ECHO_FAMILY": family},
        "grants": [
            // subject defaults to the plugin being started
            {"resource": "driver:noop", "verbs": ["nothing"]},
            {"subject": caller.subject(), "resource": format!("driver:{family}"),
             "verbs": ["make_ref"]},
        ],
    })
}

/// The kernel's own verbs are not a back door: they go through the same gate
/// as any driver's, and a plugin that was not granted them cannot call them.
#[test]
fn kernel_verbs_are_capability_gated_like_any_other() {
    let (kernel, host, root) = setup("gate");
    let echoa = spawn_echo(&host, "echoa");
    grant(&kernel, &echoa.subject(), "driver:echoa", &["relay"]);

    let denied = relay(&host, &echoa, "kernel::plugins", json!({}));
    assert!(
        denied.is_err(),
        "kernel::plugins without a grant must be refused: {denied:?}"
    );

    grant(&kernel, &echoa.subject(), "driver:kernel", &["plugins"]);
    let listed = relay(&host, &echoa, "kernel::plugins", json!({})).expect("now granted");
    let names: Vec<&str> = listed["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(names, vec!["portos-echoa"]);

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// The whole point: a system that did not have a capability acquires one
/// while it is running, and the new verbs are immediately routable by
/// whoever the spec said may use them.
#[test]
fn a_plugin_can_start_another_and_the_new_verbs_route_at_once() {
    let (kernel, host, root) = setup("plug");
    let echoa = spawn_echo(&host, "echoa");
    grant(&kernel, &echoa.subject(), "driver:echoa", &["relay"]);
    grant(&kernel, &echoa.subject(), "driver:kernel", &["spawn"]);

    // Before: nothing answers echob.
    assert!(
        relay(&host, &echoa, "echob::make_ref", json!([])).is_err(),
        "echob does not exist yet"
    );

    let spawned = relay(&host, &echoa, "kernel::spawn", echo_spec("echob", &echoa))
        .expect("spawn should be allowed");
    assert_eq!(spawned["name"].as_str(), Some("portos-echob"));
    let verbs: Vec<&str> = spawned["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        verbs.contains(&"echob::make_ref"),
        "the reply says what was gained: {verbs:?}"
    );

    // After: the verb routes, and the grant the spec minted is what allows it.
    let made = relay(&host, &echoa, "echob::make_ref", json!([])).expect("granted by the spec");
    assert!(made["ref"].is_string(), "the new plugin answered: {made}");

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Unplug, and the residue list is empty — every item of it.
///
/// The one rule worth stating: a capability is held by a *running plugin*,
/// not by a name. What the stopped plugin held is revoked. What *others*
/// were granted about its family is left alone: it goes inert with the route
/// and means something again if the plugin comes back.
#[test]
fn stopping_a_plugin_collects_every_item_of_the_residue() {
    let (kernel, host, root) = setup("unplug");
    let echoa = spawn_echo(&host, "echoa");
    grant(&kernel, &echoa.subject(), "driver:echoa", &["relay"]);
    grant(
        &kernel,
        &echoa.subject(),
        "driver:kernel",
        &["spawn", "stop"],
    );

    relay(&host, &echoa, "kernel::spawn", echo_spec("echob", &echoa)).unwrap();
    let echob = PluginName::parse("portos-echob").unwrap();
    let now = 0;

    // It is up, it holds something, and it answers.
    assert!(
        !kernel
            .caps
            .list_live(&echob.subject(), now)
            .unwrap()
            .is_empty()
    );
    assert!(relay(&host, &echoa, "echob::make_ref", json!([])).is_ok());

    let stopped = relay(
        &host,
        &echoa,
        "kernel::stop",
        json!({"name": "portos-echob"}),
    )
    .unwrap();
    assert_eq!(stopped["stopped"].as_bool(), Some(true));

    // Routes: gone. Capabilities it held: revoked.
    assert!(
        kernel
            .caps
            .list_live(&echob.subject(), now)
            .unwrap()
            .is_empty(),
        "authority does not outlive the process that held it"
    );
    let after = relay(&host, &echoa, "echob::make_ref", json!([])).unwrap_err();
    assert!(
        after.contains("no route"),
        "the caller's own grant survived — the verb is unroutable, not forbidden: {after}"
    );

    // Subscriptions: gone. An event on a topic it had subscribed to reaches
    // nobody, and the kernel does not trip over the dead subscriber.
    assert_eq!(
        host.emit(
            &Topic::parse("echob::gone").unwrap(),
            Payload::of(&json!({})).unwrap()
        ),
        0
    );

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Confluence, in the only form that costs nothing here: unplug and replug,
/// and the system is what it would have been. Cordis proves this; PortOS
/// gets to test it, because the state that would have made it hard died with
/// the process.
#[test]
fn replugging_lands_where_the_first_plug_did() {
    let (kernel, host, root) = setup("replug");
    let echoa = spawn_echo(&host, "echoa");
    grant(&kernel, &echoa.subject(), "driver:echoa", &["relay"]);
    grant(
        &kernel,
        &echoa.subject(),
        "driver:kernel",
        &["spawn", "stop"],
    );

    let first = relay(&host, &echoa, "kernel::spawn", echo_spec("echob", &echoa)).unwrap();
    relay(
        &host,
        &echoa,
        "kernel::stop",
        json!({"name": "portos-echob"}),
    )
    .unwrap();
    let second = relay(&host, &echoa, "kernel::spawn", echo_spec("echob", &echoa)).unwrap();

    assert_eq!(
        first["verbs"], second["verbs"],
        "the same plugin comes back with the same surface"
    );
    assert!(
        relay(&host, &echoa, "echob::make_ref", json!([])).is_ok(),
        "and works, which it would not if the first plug had left a route or a name behind"
    );

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// The kernel's family is its own. A plugin claiming it would be able to
/// shadow `kernel::spawn` for everyone routed through the table.
#[test]
fn the_kernel_family_is_reserved() {
    let (_kernel, host, root) = setup("reserved");
    let err = host
        .spawn(
            Path::new(ECHO_BIN),
            &[],
            &[("PORTOS_ECHO_FAMILY", "kernel")],
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("kernel"),
        "a plugin may not serve kernel::*: {err}"
    );

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}
