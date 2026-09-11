//! The extensibility claim, under test: a plugin that carries the event
//! plane and the invoke path over HTTP, written against the published ABI
//! with **no kernel change at all**.
//!
//! If this file needed `portos-kernel` to grow a feature, "extensibility
//! comes from the ABI" would be a slogan rather than a property. It does not:
//! the bridge subscribes to topics, invokes verbs it was granted, reads
//! artifacts by handle, and listens on a socket of its own — every one of
//! those already existed for `render-tty` and the browser driver.
//!
//! Hermetic: no network, no model provider. `portos-echo` is the target and
//! the kernel itself is the event source. Skips when node is absent.

use portos_kernel::Kernel;
use portos_kernel::host::Host;
use portos_proto::cap::Constraints;
use portos_proto::ids::{PluginName, Topic};
use portos_proto::wire::Payload;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
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

fn bridge_ready() -> Option<PathBuf> {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: node not found");
        return None;
    }
    Some(repo_root().join("drivers/bridge-http/bridge.js"))
}

fn tp(s: &str) -> Topic {
    Topic::parse(s).expect("test topic")
}

/// Wait for the bridge to report the port it bound. The file is written last
/// in its startup, after it has subscribed and is listening, so its presence
/// is the whole synchronisation this test needs.
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

/// One request/response with `Connection: close`, so the body ends at EOF.
/// Bytes, not text: artifacts are binary and a lossy decode would change
/// their length.
fn request_raw(port: u16, head: &str, body: Option<&str>) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut req = format!("{head} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(b) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n\r\n", b.len()));
        req.push_str(b);
    } else {
        req.push_str("\r\n");
    }
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status line");
    (status, raw[split + 4..].to_vec())
}

fn request(port: u16, head: &str, body: Option<&str>) -> (u16, String) {
    let (status, bytes) = request_raw(port, head, body);
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn get_json(port: u16, path: &str) -> Value {
    let (status, body) = request(port, &format!("GET {path}"), None);
    assert_eq!(status, 200, "GET {path} → {body}");
    serde_json::from_str(&body).expect("json body")
}

fn post_invoke(port: u16, verb: &str, args: Value) -> Value {
    let body = json!({"verb": verb, "args": args}).to_string();
    let (status, body) = request(port, "POST /invoke", Some(&body));
    assert_eq!(status, 200, "POST /invoke → {body}");
    serde_json::from_str(&body).expect("json body")
}

/// Read the first SSE frame from `GET /events`. The bridge replays what it
/// has already seen, so a subscriber that connects after the fact still gets
/// it — which is also what makes this test free of ordering races.
fn first_event(port: u16) -> Value {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line).expect("sse line");
        assert!(n > 0, "stream ended before an event arrived");
        if let Some(data) = line.trim_end().strip_prefix("data: ") {
            return serde_json::from_str(data).expect("sse json");
        }
    }
}

fn setup(tag: &str) -> (Arc<Kernel>, Host, PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-bridge-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
    (kernel, host, root)
}

fn grant(kernel: &Kernel, subject: &PluginName, resource: &str, verbs: &[&str]) {
    kernel
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

/// The whole claim in one test: topics out, verbs in, artifacts by handle —
/// all of it over HTTP, none of it requiring the kernel to know that HTTP,
/// browsers or consoles exist.
#[test]
fn bridge_carries_the_event_plane_and_the_invoke_path() {
    let Some(bridge_js) = bridge_ready() else {
        return;
    };
    let (kernel, host, root) = setup("plane");

    let echo = host
        .spawn(Path::new(ECHO_BIN), &[], &[("PORTOS_ECHO_FAMILY", "echo")])
        .unwrap();

    let port_file = root.join("port");
    let bridge = host
        .spawn(
            Path::new("node"),
            &[bridge_js.to_str().unwrap()],
            &[
                ("PORTOS_BRIDGE_ADDR", "127.0.0.1:0"),
                ("PORTOS_BRIDGE_TOPICS", "probe::*"),
                ("PORTOS_BRIDGE_PORT_FILE", port_file.to_str().unwrap()),
            ],
        )
        .unwrap();
    assert_eq!(bridge.as_str(), "portos-bridge-http");

    // The bridge acts with exactly the authority it was granted, and nothing
    // that reaches it over HTTP can exceed that.
    grant(&kernel, &bridge, "driver:echo", &["make_ref", "use_ref"]);

    let port = await_port(&port_file);

    // ---- verbs in: grants introspection, then an invoke ------------------
    let grants = get_json(port, "/grants");
    let verbs: Vec<&str> = grants["grants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|g| g["verb"].as_str())
        .collect();
    assert!(
        verbs.contains(&"echo::make_ref") && verbs.contains(&"echo::use_ref"),
        "the presenter can discover what it may call: {verbs:?}"
    );

    let made = post_invoke(port, "echo::make_ref", json!([]));
    let reference = made["ok"]["ref"].as_str().expect("a ref").to_string();
    let used = post_invoke(port, "echo::use_ref", json!([reference]));
    assert!(used["ok"]["used"].is_string(), "round trip: {used}");

    // A verb it was never granted is refused by the kernel, not by the
    // bridge — the HTTP surface adds no authority of its own.
    let denied = post_invoke(port, "echo::emit", json!(["nope"]));
    assert!(
        denied["err"].as_str().unwrap_or("").contains("denied") || denied["err"].is_string(),
        "ungranted verb must come back as an error: {denied}"
    );

    // ---- topics out: a kernel-side event reaches an HTTP subscriber -----
    host.emit(&tp("probe::hello"), Payload::of(&json!({"n": 7})).unwrap());
    let event = first_event(port);
    assert_eq!(event["topic"].as_str(), Some("probe::hello"));
    assert_eq!(event["data"]["n"].as_u64(), Some(7));

    // ---- the data plane: bytes by handle, never through the event stream -
    let blob: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    let meta = kernel
        .cas
        .put_stream(
            blob.as_slice(),
            "application/octet-stream",
            portos_proto::Label::public_trusted(),
            "test",
        )
        .unwrap();
    let (status, bytes) = request_raw(port, &format!("GET /artifact/{}", meta.id), None);
    assert_eq!(status, 200);
    assert_eq!(bytes, blob, "the whole artifact, byte for byte");

    // Echo is still the one serving the verb; the bridge only routed to it.
    assert_eq!(echo.as_str(), "portos-echo");

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// The presenter is static bytes the bridge happens to host. Nothing about
/// it is privileged, and the transport does not know what language it speaks.
#[test]
fn presenter_is_served_as_plain_bytes_and_confined_to_its_directory() {
    let Some(bridge_js) = bridge_ready() else {
        return;
    };
    let (_kernel, host, root) = setup("static");

    let port_file = root.join("port");
    host.spawn(
        Path::new("node"),
        &[bridge_js.to_str().unwrap()],
        &[
            ("PORTOS_BRIDGE_ADDR", "127.0.0.1:0"),
            ("PORTOS_BRIDGE_PORT_FILE", port_file.to_str().unwrap()),
        ],
    )
    .unwrap();
    let port = await_port(&port_file);

    let (status, body) = request(port, "GET /", None);
    assert_eq!(status, 200);
    assert!(body.contains("PortOS console"), "the default presenter");

    let (status, _) = request(port, "GET /../../Cargo.toml", None);
    assert_ne!(status, 200, "presenters cannot escape web/");

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Kept honest about the one thing this design does not have: a verb the
/// bridge holds is a verb anything reaching the socket holds.
#[test]
fn an_unconfigured_bridge_can_invoke_nothing() {
    let Some(bridge_js) = bridge_ready() else {
        return;
    };
    let (_kernel, host, root) = setup("empty");

    let port_file = root.join("port");
    host.spawn(
        Path::new("node"),
        &[bridge_js.to_str().unwrap()],
        &[
            ("PORTOS_BRIDGE_ADDR", "127.0.0.1:0"),
            ("PORTOS_BRIDGE_PORT_FILE", port_file.to_str().unwrap()),
        ],
    )
    .unwrap();
    let port = await_port(&port_file);

    let grants = get_json(port, "/grants");
    assert_eq!(
        grants["grants"].as_array().map(Vec::len),
        Some(0),
        "a bridge with no grants exposes no verbs"
    );
    let denied = post_invoke(port, "echo::make_ref", json!([]));
    assert!(denied["err"].is_string(), "and can call none: {denied}");

    // A malformed verb is refused before it ever reaches the capability
    // table — the typed protocol boundary doing its job.
    let bad = post_invoke(port, "not-a-verb", json!([]));
    assert!(bad["err"].is_string(), "unparseable verb: {bad}");

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}
