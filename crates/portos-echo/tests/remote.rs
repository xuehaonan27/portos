//! Multi-node, route A: another node's capabilities arriving as ordinary
//! local verbs, with no kernel change on either side.
//!
//! Both nodes run inside this test process — two roots, two `Kernel`s, two
//! `Host`s, sharing nothing but a loopback socket. That is the honest form
//! of the claim: the word "node" appears nowhere in `portos-kernel`, and
//! neither end is privileged. The far node publishes its surface with the
//! same `portos-bridge-http` that was written for a browser; the near node
//! dials it with `portos-remote`, which is a plugin like any other.
//!
//! What the three tests pin down is the division of authority, because that
//! is the part a multi-node design usually gets wrong: **the far node
//! decides what is exposed, the near node decides who may use it**, and
//! neither can overrule the other.
//!
//! Hermetic: loopback only, no model provider. Skips when node or the remote
//! driver binary is absent.

use portos_kernel::Kernel;
use portos_kernel::host::Host;
use portos_proto::cap::Constraints;
use portos_proto::ids::{PluginName, Topic, Verb};
use portos_proto::wire::Payload;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ECHO_BIN: &str = env!("CARGO_BIN_EXE_portos-echo");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Both ends of the link, or a printed reason for skipping. The remote
/// driver lives in another package, so it is found beside this one's binary
/// rather than through `CARGO_BIN_EXE_` — `cargo test --workspace` builds it.
fn link_ready() -> Option<(PathBuf, PathBuf)> {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: node not found");
        return None;
    }
    let remote = Path::new(ECHO_BIN).with_file_name("portos-remote");
    if !remote.exists() {
        eprintln!("skipping: portos-remote not built (use `cargo test --workspace`)");
        return None;
    }
    Some((repo_root().join("drivers/bridge-http/bridge.js"), remote))
}

struct Node {
    kernel: Arc<Kernel>,
    host: Host,
    root: PathBuf,
}

impl Node {
    fn open(tag: &str) -> Node {
        let root = std::env::temp_dir().join(format!("portos-remote-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let kernel = Arc::new(Kernel::open(&root).unwrap());
        let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
        Node { kernel, host, root }
    }

    fn grant(&self, subject: &PluginName, resource: &str, verbs: &[&str]) {
        self.kernel
            .caps
            .mint(
                &subject.subject(),
                resource,
                verbs.iter().map(|v| v.to_string()).collect::<BTreeSet<_>>(),
                Constraints::default(),
                None,
            )
            .unwrap();
    }

    fn echo(&self, family: &str) -> PluginName {
        self.host
            .spawn(Path::new(ECHO_BIN), &[], &[("PORTOS_ECHO_FAMILY", family)])
            .unwrap()
    }

    fn close(self) {
        self.host.shutdown_all();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn vb(s: &str) -> Verb {
    Verb::parse(s).expect("test verb")
}

fn tp(s: &str) -> Topic {
    Topic::parse(s).expect("test topic")
}

/// A kernel-side call: root authority, so it tests routing alone.
fn call_verb(node: &Node, verb: &str, args: Value) -> Result<Value, String> {
    node.host
        .call_verb(&vb(verb), Payload::of(&args).expect("test payload"))
        .map(|p| p.parse().expect("json"))
        .map_err(|e| e.to_string())
}

/// Call one of a plugin's own verbs. Kernel-side, so it carries root
/// authority and no grant is involved.
fn call(node: &Node, plugin: &PluginName, verb: &str, args: Value) -> Result<Value, String> {
    node.host
        .call(plugin, &vb(verb), Payload::of(&args).expect("test payload"))
        .map(|p| p.parse().expect("json"))
        .map_err(|e| e.to_string())
}

/// Reach a verb the way a plugin does — through the kernel, past the
/// capability gate — by having an echo driver relay it. Never to a verb of
/// the relaying plugin itself: one serve loop, so that deadlocks by design.
fn relay(node: &Node, from: &PluginName, verb: &str, args: Value) -> Result<Value, String> {
    let family = from.as_str().trim_start_matches("portos-").to_string();
    call(node, from, &format!("{family}::relay"), json!([verb, args]))
}

fn await_port(path: &Path) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(port) = text.trim().parse::<u16>() {
                return port;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("bridge never reported a port");
}

/// The far node: an echo driver, and a bridge granted `exposed` of it. That
/// grant list is the far node's entire statement about what it is willing to
/// share.
fn far_node(tag: &str, bridge_js: &Path, exposed: &[&str], topics: &str) -> (Node, u16) {
    let node = Node::open(tag);
    node.echo("echo");
    let port_file = node.root.join("port");
    let bridge = node
        .host
        .spawn(
            Path::new("node"),
            &[bridge_js.to_str().unwrap()],
            &[
                ("PORTOS_BRIDGE_ADDR", "127.0.0.1:0"),
                ("PORTOS_BRIDGE_TOPICS", topics),
                ("PORTOS_BRIDGE_PORT_FILE", port_file.to_str().unwrap()),
            ],
        )
        .unwrap();
    node.grant(&bridge, "driver:echo", exposed);
    let port = await_port(&port_file);
    (node, port)
}

/// The near node, with a remote driver pointed at the far one.
fn near_node(tag: &str, remote_bin: &Path, port: u16) -> Node {
    let node = Node::open(tag);
    let url = format!("http://127.0.0.1:{port}");
    let name = node
        .host
        .spawn(
            remote_bin,
            &[],
            &[("PORTOS_REMOTE_URL", &url), ("PORTOS_REMOTE_NODE", "b")],
        )
        .unwrap();
    assert_eq!(name.as_str(), "portos-remote-b");
    node
}

/// The claim itself: a verb served on another machine is called here the way
/// any local verb is, and it arrives carrying the description and schema its
/// own driver wrote — which is what makes it usable by a model rather than
/// merely reachable.
#[test]
fn another_nodes_verbs_arrive_as_ordinary_local_verbs() {
    let Some((bridge_js, remote_bin)) = link_ready() else {
        return;
    };
    let (far, port) = far_node("far-verbs", &bridge_js, &["make_ref", "emit"], "echo::*");
    let near = near_node("near-verbs", &remote_bin, port);

    // Routed, under a family that says whose it is.
    let made = call_verb(&near, "b_echo::make_ref", json!([])).expect("the far node answered");
    assert!(made["ref"].is_string(), "a real reply crossed: {made}");

    // And the far node's own name for it is gone from this side: two nodes
    // with a browser each would collide in a flat route table.
    assert!(
        call_verb(&near, "echo::make_ref", json!([])).is_err(),
        "the far node's verbs are not installed under their own family"
    );

    // The tool surface: what a plugin here sees when it asks what it may do.
    let near_echo = near.echo("echoa");
    near.grant(&near_echo, "driver:b_echo", &["emit"]);
    let grants = call(&near, &near_echo, "echoa::grants", json!([])).expect("grants");

    let emit = grants
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["verb"] == "b_echo::emit")
        .expect("the remote verb is a grantable capability like any other")
        .clone();
    let description = emit["description"].as_str().unwrap_or_default();
    assert!(
        description.contains("echo driver's stdout"),
        "the far driver's own words survived the trip: {description:?}"
    );
    assert!(
        description.contains("node b"),
        "and the model is told where it runs: {description:?}"
    );
    assert_eq!(
        emit["schema"]["properties"]["text"]["type"], "string",
        "the schema crossed too, so the call can be formed: {emit}"
    );

    near.close();
    far.close();
}

/// The other half of the two-layer naming rule, and the half that needs no
/// transport at all.
///
/// An ephemeral ref belongs to the driver session that minted it and means
/// nothing outside it — which is exactly why it crosses a node boundary for
/// free. `e1` travels as the opaque string it always was, and is interpreted
/// on the machine that holds the thing it names. A kernel *handle* is the
/// opposite: it names bytes, and bytes have to actually be somewhere.
#[test]
fn a_drivers_own_refs_cross_without_being_translated() {
    let Some((bridge_js, remote_bin)) = link_ready() else {
        return;
    };
    let (far, port) = far_node("far-refs", &bridge_js, &["make_ref", "use_ref"], "echo::*");
    let near = near_node("near-refs", &remote_bin, port);

    let made = call_verb(&near, "b_echo::make_ref", json!([])).expect("minted on the far node");
    let reference = made["ref"].as_str().expect("a ref").to_string();

    let used = call_verb(&near, "b_echo::use_ref", json!([reference.clone()]))
        .expect("the far driver still knows its own ref");
    assert_eq!(
        used["used"].as_str(),
        Some(reference.as_str()),
        "the ref went out and came back untouched: {used}"
    );

    // It is meaningful only there: this node never held it, and nothing
    // here would know what to do with it.
    assert!(
        call_verb(&near, "echo::use_ref", json!([reference])).is_err(),
        "a ref is not a portable name"
    );

    near.close();
    far.close();
}

/// Two grant tables, each answering a different question, and a verb needs
/// both to say yes. The failures are distinguishable on purpose: a verb the
/// far node never exposed is not in this node's route table at all, while a
/// verb it did expose but this node did not grant is refused by the local
/// capability gate.
#[test]
fn both_nodes_get_a_say_in_what_crosses() {
    let Some((bridge_js, remote_bin)) = link_ready() else {
        return;
    };
    let (far, port) = far_node("far-say", &bridge_js, &["make_ref"], "echo::*");
    let near = near_node("near-say", &remote_bin, port);

    let near_echo = near.echo("echoa");
    // Granted here for both — the near node cannot tell which of them the
    // far node is willing to serve, and does not need to.
    near.grant(&near_echo, "driver:b_echo", &["make_ref", "use_ref"]);

    let allowed = relay(&near, &near_echo, "b_echo::make_ref", json!([]));
    assert!(
        allowed.is_ok(),
        "exposed there and granted here: {allowed:?}"
    );

    let not_exposed = relay(&near, &near_echo, "b_echo::use_ref", json!(["e1"])).unwrap_err();
    assert!(
        not_exposed.contains("no route"),
        "what the far node did not expose was never declared here: {not_exposed}"
    );

    // The other direction: revoke locally and the link is irrelevant.
    let near_echo2 = near.echo("echob");
    let not_granted = relay(&near, &near_echo2, "b_echo::make_ref", json!([])).unwrap_err();
    assert!(
        not_granted.contains("no capability"),
        "the route exists and the far node would serve it; this node's own \
         gate is what refused: {not_granted}"
    );

    near.close();
    far.close();
}

/// The other interaction shape. A transport that carried only verbs would be
/// half a transport: a verb that reports through events — every long
/// operation PortOS has — would arrive unusable. Topics are renamed by the
/// same rule the verbs are.
#[test]
fn events_cross_the_link_renamed_like_the_verbs() {
    let Some((bridge_js, remote_bin)) = link_ready() else {
        return;
    };
    let (far, port) = far_node("far-events", &bridge_js, &["publish"], "echo::*");
    let near = near_node("near-events", &remote_bin, port);

    let (_sub, rx) = near.host.subscribe_local(&tp("b_echo::*"));

    // The far node publishes until it lands: nothing here knows when the
    // driver's event stream finished connecting, and inventing a readiness
    // handshake for a test would be worse than retrying.
    let deadline = Instant::now() + Duration::from_secs(20);
    let received = loop {
        assert!(Instant::now() < deadline, "no event crossed the link");
        call_verb(&far, "echo::publish", json!(["echo::news", {"n": 7}])).expect("published");
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(250)) {
            break ev;
        }
    };

    assert_eq!(
        received.topic.as_str(),
        "b_echo::news",
        "the leading segment gained the node's name, like a family does"
    );
    let data: Value = received.data.parse().unwrap();
    assert_eq!(data["n"], 7, "the payload crossed untouched: {data}");

    near.close();
    far.close();
}
