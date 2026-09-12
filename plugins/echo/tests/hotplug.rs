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

use portos_abi::cap::Constraints;
use portos_abi::ids::{PluginName, Topic, Verb};
use portos_abi::wire::Payload;
use portos_kernel::Kernel;
use portos_kernel::host::Host;
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

fn spawn_echo(host: &Host, driver: &str) -> PluginName {
    host.spawn(Path::new(ECHO_BIN), &[], &[("PORTOS_ECHO_DRIVER", driver)])
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
    let driver = from.as_str().trim_start_matches("portos-").to_string();
    call(host, from, &format!("{driver}::relay"), json!([verb, args]))
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

/// A spec that starts another echo playing `driver`, granting it a capability
/// of its own and granting `caller` the right to use it.
fn echo_spec(driver: &str, caller: &PluginName) -> Value {
    json!({
        "bin": ECHO_BIN,
        "env": {"PORTOS_ECHO_DRIVER": driver},
        "grants": [
            // subject defaults to the plugin being started
            {"resource": "driver:noop", "verbs": ["nothing"]},
            {"subject": caller.subject(), "resource": format!("driver:{driver}"),
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
/// were granted about its driver is left alone: it goes inert with the route
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

/// Two instances of one driver — two browsers — are told apart by name,
/// and only by name: the verb is the driver's and says nothing about who
/// answers it.
///
/// This replaced a law that said a family has one answerer. That law made
/// the name do two jobs, and the second one leaked out as encoded names
/// (`mac_browser::open`) that nothing could parse back. Which instance
/// answers is a routing decision, so it lives in the call, not in the name.
#[test]
fn two_instances_of_one_driver_are_told_apart_by_name() {
    let (kernel, host, root) = setup("instances");
    let instance = |name: &str| portos_kernel::host::LaunchSpec {
        name: Some(name.to_string()),
        env: [("PORTOS_ECHO_DRIVER".to_string(), "shared".to_string())]
            .into_iter()
            .collect(),
        ..portos_kernel::host::LaunchSpec::from_path(ECHO_BIN)
    };
    let a = host
        .spawn_spec(&instance("portos-shared-a"))
        .expect("first");
    let b = host
        .spawn_spec(&instance("portos-shared-b"))
        .expect("a second instance of the same driver is not a conflict");
    assert_eq!(
        (a.as_str(), b.as_str()),
        ("portos-shared-a", "portos-shared-b"),
        "the launcher's names, not the plugin's own"
    );

    // Unnamed, the call is ambiguous: an error, not a choice made for the
    // caller — and the error says who could have been named.
    let err = routed(&host, "shared::make_ref").unwrap_err();
    assert!(
        err.contains("ambiguous")
            && err.contains("portos-shared-a")
            && err.contains("portos-shared-b"),
        "{err}"
    );

    // Named, each answers for itself: a ref minted by one is unknown to the
    // other, which is what "two instances" means.
    let minted = routed_at(&host, "shared::make_ref", &a, json!([])).expect("named");
    let r = minted["ref"].as_str().expect("a ref").to_string();
    assert!(routed_at(&host, "shared::use_ref", &a, json!([r.clone()])).is_ok());
    assert!(
        routed_at(&host, "shared::use_ref", &b, json!([r.clone()])).is_err(),
        "b never minted it"
    );

    // A plugin names an instance the same way — through the kernel, past
    // the gate — and the grant it holds tells it there is a choice.
    let caller = spawn_echo(&host, "echoa");
    grant(&kernel, &caller.subject(), "driver:shared", &["make_ref"]);
    let via = call(
        &host,
        &caller,
        "echoa::relay_at",
        json!(["portos-shared-b", "shared::make_ref", []]),
    )
    .expect("named through the kernel");
    assert!(via["ref"].is_string(), "{via}");
    let grants = call(&host, &caller, "echoa::grants", json!([])).expect("grants");
    let g = grants
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["verb"] == "shared::make_ref")
        .expect("granted")
        .clone();
    assert_eq!(
        g["instances"],
        json!(["portos-shared-a", "portos-shared-b"])
    );

    // One leaves, and the verb needs no name again: a choice with one
    // option is not a choice.
    host.shutdown(&a);
    assert!(
        routed(&host, "shared::make_ref").is_ok(),
        "one answerer left, so nothing to name"
    );

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// A plugin says what it cannot work without, and the order it is started in
/// stops mattering.
///
/// Nothing here declares that one plugin comes before another. The waiting
/// one is started *first*, on purpose: it runs, keeps its names, and simply
/// is not answered until what it needs turns up. What a caller sees while it
/// waits is exactly what it sees for a plugin that was stopped — "no route" —
/// because "not there yet" and "not there any more" are the same thing to a
/// caller, and two ways to be absent would be two rules to learn.
#[test]
fn a_plugin_waits_for_what_it_needs_and_nobody_declares_an_order() {
    let (kernel, host, root) = setup("needs");

    // Started first, and it needs something nothing answers yet.
    let waiting = host
        .spawn_spec(&portos_kernel::host::LaunchSpec {
            env: [
                ("PORTOS_ECHO_DRIVER".to_string(), "waiter".to_string()),
                (
                    "PORTOS_ECHO_NEEDS".to_string(),
                    "provider::make_ref".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            ..portos_kernel::host::LaunchSpec::from_path(ECHO_BIN)
        })
        .expect("it starts — a missing dependency is not a failure to start");

    assert!(
        routed(&host, "waiter::make_ref").is_err(),
        "running, but not answered"
    );
    let (_, verbs, unmet) = host
        .plugins()
        .into_iter()
        .find(|(n, _, _)| n == &waiting)
        .expect("it is up");
    assert!(verbs.is_empty(), "nothing routed to it: {verbs:?}");
    assert_eq!(
        unmet.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
        vec!["provider::make_ref"],
        "and it says what it is waiting for"
    );

    // Being allowed to call it is the other half of needing it: a plugin
    // that may not ask is no better off than one with nobody to ask.
    grant(
        &kernel,
        &waiting.subject(),
        "driver:provider",
        &["make_ref"],
    );

    // The thing it needed turns up. Nothing tells anyone to re-check.
    let provider = spawn_echo(&host, "provider");
    assert!(
        routed(&host, "waiter::make_ref").is_ok(),
        "satisfying the need is what makes it usable"
    );

    // And it goes away again when the dependency does.
    host.shutdown(&provider);
    assert!(
        routed(&host, "waiter::make_ref").is_err(),
        "a dependency leaving is the same as never having arrived"
    );

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Reach a verb through the route table, which is what tells whether a
/// plugin is being answered at all.
fn routed(host: &Host, verb: &str) -> Result<Value, String> {
    host.call_verb(
        &Verb::parse(verb).expect("test verb"),
        None,
        Payload::of(&serde_json::json!([])).expect("test payload"),
    )
    .map(|p| p.parse().expect("json"))
    .map_err(|e| e.to_string())
}

/// The same, naming the instance.
fn routed_at(host: &Host, verb: &str, at: &PluginName, args: Value) -> Result<Value, String> {
    host.call_verb(
        &Verb::parse(verb).expect("test verb"),
        Some(at),
        Payload::of(&args).expect("test payload"),
    )
    .map(|p| p.parse().expect("json"))
    .map_err(|e| e.to_string())
}
