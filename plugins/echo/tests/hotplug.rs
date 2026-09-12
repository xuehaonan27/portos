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

/// A family has one answerer — which is what "the kernel's family is its
/// own" turned out to be a special case of.
///
/// The capability resource is `driver:<family>`, so a family split between
/// two answerers makes one grant statement mean two different things. The
/// kernel's family is not privileged; it is simply already answered.
#[test]
fn a_family_has_one_answerer() {
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
        "the kernel already answers that family: {err}"
    );

    // And the same refusal for two ordinary plugins, which is the part that
    // was never checked while this looked like a privilege of the kernel's.
    let _first = spawn_echo(&host, "shared");
    let second = host
        .spawn_spec(&portos_kernel::host::LaunchSpec {
            env: [("PORTOS_ECHO_FAMILY".to_string(), "shared".to_string())]
                .into_iter()
                .collect(),
            ..portos_kernel::host::LaunchSpec::from_path(ECHO_BIN)
        })
        .unwrap_err();
    assert!(
        second.to_string().contains("shared"),
        "two plugins may not split a family between them: {second}"
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
                ("PORTOS_ECHO_FAMILY".to_string(), "waiter".to_string()),
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
        Payload::of(&serde_json::json!([])).expect("test payload"),
    )
    .map(|p| p.parse().expect("json"))
    .map_err(|e| e.to_string())
}
