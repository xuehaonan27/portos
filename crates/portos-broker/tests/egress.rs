//! Egress broker end-to-end tests: allowlist, credential injection, response
//! sanitization, streaming-as-events, audit accounting, and the
//! capability-gated invoke path from another plugin. The upstream is a
//! minimal in-test HTTP/1.1 server on loopback (rules opt into
//! `insecure_http` for it — production rules are https-only by default).

use portos_kernel::Kernel;
use portos_kernel::host::Host;
use portos_proto::cap::Constraints;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const BROKER_BIN: &str = env!("CARGO_BIN_EXE_portos-broker");

fn setup(tag: &str) -> (Arc<Kernel>, Host, PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-egress-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
    (kernel, host, root)
}

/// Write broker config + secrets under `root/broker` and spawn the broker.
fn spawn_broker(host: &Host, root: &Path, allow: Value, secrets: Value) -> String {
    let dir = root.join("broker");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_string_pretty(&json!({"allow": allow})).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("secrets.json"),
        serde_json::to_string_pretty(&secrets).unwrap(),
    )
    .unwrap();
    host.spawn(
        Path::new(BROKER_BIN),
        &[],
        &[("PORTOS_BROKER_DIR", dir.to_str().unwrap())],
    )
    .unwrap()
}

/// Minimal HTTP/1.1 upstream: accepts `n` connections, hands each request's
/// head+body to `respond`, writes whatever it returns.
fn upstream<F>(n: usize, respond: F) -> u16
where
    F: Fn(String) -> Vec<u8> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for _ in 0..n {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            let req = read_request(&mut conn);
            let out = respond(req);
            let _ = conn.write_all(&out);
            let _ = conn.flush();
        }
    });
    port
}

/// Read request head (and content-length body, if any) as one string.
fn read_request(conn: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if conn.read_exact(&mut byte).is_err() {
            break;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let clen: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; clen];
    if clen > 0 {
        let _ = conn.read_exact(&mut body);
    }
    format!("{head}{}", String::from_utf8_lossy(&body))
}

fn http_response(extra_headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

#[test]
fn denies_unlisted_host_and_plain_http() {
    let (_k, host, root) = setup("deny");
    // Allowlist has one https-only host; loopback is not on it at all.
    let name = spawn_broker(
        &host,
        &root,
        json!([{"host": "allowed.example"}]),
        json!({}),
    );

    let unlisted = host.call(
        &name,
        "egress::http",
        json!({"url": "http://127.0.0.1:9/x"}),
    );
    assert!(
        unlisted.unwrap_err().to_string().contains("not allowed"),
        "unlisted host must be denied before any connection is attempted"
    );

    let plain = host.call(
        &name,
        "egress::http",
        json!({"url": "http://allowed.example/x"}),
    );
    assert!(
        plain.unwrap_err().to_string().contains("scheme"),
        "http to an https-only rule must be denied"
    );

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn injects_secret_and_sanitizes_response() {
    let (_k, host, root) = setup("inject");
    // Upstream echoes the request (head+body) back so the test can see what
    // actually went over the wire, and sets a cookie that must not survive.
    let port = upstream(1, |req| {
        http_response("Set-Cookie: sid=1\r\nX-Keep: yes\r\n", req.as_bytes())
    });
    let name = spawn_broker(
        &host,
        &root,
        json!([{
            "host": "127.0.0.1", "insecure_http": true,
            "inject": {"x-test-key": "k1"},
            "strip_response": ["x-secret-echo"],
        }]),
        json!({"k1": "sekrit-value-123"}),
    );

    let out = host
        .call(
            &name,
            "egress::http",
            json!({
                "method": "POST",
                "url": format!("http://127.0.0.1:{port}/p"),
                // The caller tries to smuggle its own value into the injected
                // header — the injection point must own it.
                "headers": {"x-test-key": "attacker-value", "x-custom": "ok"},
                "body": "hello upstream",
            }),
        )
        .unwrap();

    assert_eq!(out["status"].as_u64(), Some(200));
    let wire = out["body"].as_str().unwrap();
    assert!(wire.contains("x-test-key: sekrit-value-123"), "secret injected");
    assert!(
        !wire.contains("attacker-value"),
        "caller cannot override an injected header"
    );
    assert!(wire.contains("x-custom: ok"), "ordinary caller headers pass");
    assert!(wire.ends_with("hello upstream"), "body forwarded");
    assert!(
        out["headers"].get("set-cookie").is_none(),
        "set-cookie never reaches the caller"
    );
    assert_eq!(out["headers"]["x-keep"].as_str(), Some("yes"));

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn streams_body_as_events() {
    let (_k, host, root) = setup("stream");
    // Upstream dribbles a 3-part body so the broker actually streams.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let _ = read_request(&mut conn);
        let parts = ["data: one\n\n", "data: two\n\n", "data: done\n\n"];
        let total: usize = parts.iter().map(|p| p.len()).sum();
        conn.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
        for p in parts {
            conn.write_all(p.as_bytes()).unwrap();
            conn.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    });
    let name = spawn_broker(
        &host,
        &root,
        json!([{"host": "127.0.0.1", "insecure_http": true}]),
        json!({}),
    );

    let (_sub, rx) = host.subscribe_local("sse::t1");
    let head = host
        .call(
            &name,
            "egress::http_stream",
            json!({"url": format!("http://127.0.0.1:{port}/sse"), "topic": "sse::t1"}),
        )
        .unwrap();
    assert_eq!(head["status"].as_u64(), Some(200));

    let mut collected = String::new();
    loop {
        let ev = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("stream event");
        let data = &ev["data"];
        if let Some(c) = data["chunk"].as_str() {
            collected.push_str(c);
        } else if data["done"].as_bool() == Some(true) {
            assert_eq!(data["bytes"].as_u64(), Some(collected.len() as u64));
            break;
        } else {
            panic!("unexpected stream event: {ev}");
        }
    }
    assert_eq!(collected, "data: one\n\ndata: two\n\ndata: done\n\n");

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn secret_stays_out_of_responses_and_audit() {
    let (kernel, host, root) = setup("noleak");
    host.audit_topic("egress::log");
    let port = upstream(1, |_req| http_response("", b"ok"));
    let name = spawn_broker(
        &host,
        &root,
        json!([{"host": "127.0.0.1", "insecure_http": true, "inject": {"x-api-key": "k1"}}]),
        json!({"k1": "sekrit-value-123"}),
    );

    let out = host
        .call(
            &name,
            "egress::http",
            json!({"url": format!("http://127.0.0.1:{port}/q")}),
        )
        .unwrap();
    assert_eq!(out["body"].as_str(), Some("ok"));
    assert!(
        !serde_json::to_string(&out).unwrap().contains("sekrit"),
        "secret value must never appear in a response"
    );

    // The egress::log event lands on the audit chain (async — poll), with
    // injected header *names* only.
    let audit_path = root.join("audit.log");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let entry = loop {
        let entries = portos_kernel::audit::AuditLog::verify(&audit_path).unwrap();
        if let Some(e) = entries.iter().find(|e| {
            e["body"]["event"] == "topic.audit" && e["body"]["topic"] == "egress::log"
        }) {
            break e.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "egress::log never reached the audit chain"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(entry["body"]["data"]["host"].as_str(), Some("127.0.0.1"));
    assert_eq!(entry["body"]["data"]["injected"][0].as_str(), Some("x-api-key"));
    let audit_text = std::fs::read_to_string(&audit_path).unwrap();
    assert!(
        !audit_text.contains("sekrit"),
        "secret value must never appear in the audit log"
    );
    drop(kernel);

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// The real calling shape: another plugin invokes the broker through the
/// kernel, gated by its capability grant. Uses the echo toy as the caller;
/// skips when the echo binary isn't built (it is under `cargo test
/// --workspace`).
#[test]
fn plugin_invoke_reaches_broker_capability_gated() {
    let echo_bin = Path::new(BROKER_BIN).with_file_name("portos-echo");
    if !echo_bin.exists() {
        eprintln!("skipping: portos-echo binary not built ({})", echo_bin.display());
        return;
    }
    let (kernel, host, root) = setup("invoke");
    let port = upstream(1, |_req| http_response("", b"pong"));
    let broker = spawn_broker(
        &host,
        &root,
        json!([{"host": "127.0.0.1", "insecure_http": true}]),
        json!({}),
    );
    assert_eq!(broker, "portos-broker");
    let caller = host
        .spawn(&echo_bin, &[], &[("PORTOS_ECHO_FAMILY", "echoa")])
        .unwrap();

    // Granted: egress::http. Not granted: egress::http_stream.
    kernel
        .caps
        .mint(
            "plugin:portos-echoa",
            "driver:egress",
            BTreeSet::from(["http".to_string()]),
            Constraints::default(),
            None,
        )
        .unwrap();

    let url = format!("http://127.0.0.1:{port}/via-invoke");
    let out = host
        .call(
            &caller,
            "echoa::relay",
            json!(["egress::http", {"url": url}]),
        )
        .unwrap();
    assert_eq!(out["status"].as_u64(), Some(200));
    assert_eq!(out["body"].as_str(), Some("pong"));

    let denied = host.call(
        &caller,
        "echoa::relay",
        json!(["egress::http_stream", {"url": "http://127.0.0.1:1/", "topic": "t"}]),
    );
    assert!(denied.is_err(), "ungranted egress verb must be denied");

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}
