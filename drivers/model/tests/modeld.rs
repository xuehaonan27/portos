//! Model-driver end-to-end: the full standalone-runtime chain, hermetic.
//!
//!   host.call(model::send)
//!     → modeld (neutral core, anthropic backend)
//!       → invoke egress::http_stream → broker (injects the API key modeld
//!         never holds) → mock provider (scripted SSE)
//!       → tool_use → invoke echoa::make_ref (capability-gated) → echo
//!       → tool_result → second provider turn → final text
//!   …with deltas / tool activity streaming on model::session::<id>.
//!
//! The mock provider dribbles SSE in tiny slices, so chunk reassembly across
//! broker-event boundaries is exercised for real.

use portos_kernel::Kernel;
use portos_kernel::host::Host;
use portos_proto::cap::Constraints;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MODELD_BIN: &str = env!("CARGO_BIN_EXE_portos-modeld");

fn sibling(name: &str) -> Option<PathBuf> {
    let p = Path::new(MODELD_BIN).with_file_name(name);
    p.exists().then_some(p)
}

fn setup(tag: &str) -> (Arc<Kernel>, Host, PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-modeld-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
    (kernel, host, root)
}

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

fn sse(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(e, d)| format!("event: {e}\ndata: {d}\n\n"))
        .collect()
}

/// Serve `bodies` to sequential connections, dribbling each response body in
/// tiny slices; captured requests land in the returned mutex.
fn mock_provider(bodies: Vec<String>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    std::thread::spawn(move || {
        for body in bodies {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            let req = read_request(&mut conn);
            cap.lock().unwrap().push(req);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = conn.write_all(head.as_bytes());
            for slice in body.as_bytes().chunks(41) {
                let _ = conn.write_all(slice);
                let _ = conn.flush();
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
        }
    });
    (port, captured)
}

fn write_json(path: &Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
}

#[test]
fn agentic_loop_end_to_end_with_tool_call() {
    let (Some(broker_bin), Some(echo_bin)) = (sibling("portos-broker"), sibling("portos-echo"))
    else {
        eprintln!("skipping: sibling binaries not built (run under cargo test --workspace)");
        return;
    };
    let (kernel, host, root) = setup("e2e");

    // Turn 1: some text, then a tool call. Turn 2: the wrap-up.
    let turn1 = sse(&[
        ("message_start", json!({"type": "message_start"})),
        ("content_block_start", json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Let me "}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "make a ref."}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ("content_block_start", json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "echoa__make_ref", "input": {}}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "{}"}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 1})),
        ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}})),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let turn2 = sse(&[
        ("message_start", json!({"type": "message_start"})),
        ("content_block_start", json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Ref created."}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let (port, captured) = mock_provider(vec![turn1, turn2]);

    // Broker: allow the mock host, inject the key modeld must never hold.
    let broker_dir = root.join("broker");
    write_json(
        &broker_dir.join("config.json"),
        &json!({"allow": [{"host": "127.0.0.1", "insecure_http": true,
                            "inject": {"x-api-key": "k1"}}]}),
    );
    write_json(&broker_dir.join("secrets.json"), &json!({"k1": "fake-test-key"}));
    host.spawn(
        &broker_bin,
        &[],
        &[("PORTOS_BROKER_DIR", broker_dir.to_str().unwrap())],
    )
    .unwrap();
    host.spawn(&echo_bin, &[], &[("PORTOS_ECHO_FAMILY", "echoa")])
        .unwrap();

    // Model driver: anthropic backend pointed at the mock, one tool.
    let modeld_dir = root.join("modeld");
    write_json(
        &modeld_dir.join("config.json"),
        &json!({
            "backend": "anthropic",
            "base_url": format!("http://127.0.0.1:{port}"),
            "model": "test-model",
            "max_tokens": 512,
            "system": "You are a test assistant.",
            "tools": [{
                "verb": "echoa::make_ref",
                "description": "Make an ephemeral ref in the echo driver.",
                "schema": {"type": "object", "properties": {}},
            }],
        }),
    );
    let modeld = host
        .spawn(
            Path::new(MODELD_BIN),
            &[],
            &[("PORTOS_MODELD_DIR", modeld_dir.to_str().unwrap())],
        )
        .unwrap();
    assert_eq!(modeld, "portos-modeld");

    // Grants: egress for the LLM call, one echo verb for the tool.
    for (resource, verbs) in [
        ("driver:egress", vec!["http", "http_stream"]),
        ("driver:echoa", vec!["make_ref"]),
    ] {
        kernel
            .caps
            .mint(
                "plugin:portos-modeld",
                resource,
                verbs.into_iter().map(String::from).collect::<BTreeSet<_>>(),
                Constraints::default(),
                None,
            )
            .unwrap();
    }

    let started = host.call(&modeld, "model::start", json!({})).unwrap();
    let sid = started["session"].as_str().unwrap().to_string();
    let (_sub, rx) = host.subscribe_local(&format!("model::session::{sid}"));

    let out = host
        .call(
            &modeld,
            "model::send",
            json!({"session": sid, "text": "please make a ref"}),
        )
        .unwrap();
    assert_eq!(out["text"].as_str(), Some("Ref created."));

    // The event stream told the story live: deltas, the tool call, its
    // result, and the final text.
    let mut deltas = String::new();
    let mut kinds = Vec::new();
    loop {
        let ev = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("session event");
        let d = &ev["data"];
        let kind = d["kind"].as_str().unwrap_or("?").to_string();
        if kind == "delta" {
            deltas.push_str(d["text"].as_str().unwrap_or(""));
        }
        kinds.push(kind.clone());
        if kind == "done" {
            assert_eq!(d["text"].as_str(), Some("Ref created."));
            break;
        }
    }
    assert_eq!(deltas, "Let me make a ref.Ref created.");
    assert!(kinds.contains(&"tool_call".to_string()));
    assert!(kinds.contains(&"tool_result".to_string()));

    // What the provider actually received: the injected key (proof modeld
    // ran with zero credentials), the mangled tool name, the system prompt —
    // and on turn two, the tool result carrying echo's ref.
    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].contains("x-api-key: fake-test-key"));
    assert!(reqs[0].contains("echoa__make_ref"));
    assert!(reqs[0].contains("You are a test assistant."));
    assert!(reqs[0].contains("please make a ref"));
    assert!(reqs[1].contains("tool_result"));
    assert!(reqs[1].contains("toolu_1"));
    assert!(reqs[1].contains("e1"), "echo's ref value reached the provider");

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Zero tool config: the tool surface comes from grants introspection (with
/// driver-advertised metadata), egress never surfaces as a model tool, and
/// the built-in artifact::read pages a stored payload back to the provider.
#[test]
fn introspected_tools_and_artifact_read() {
    let (Some(broker_bin), Some(echo_bin)) = (sibling("portos-broker"), sibling("portos-echo"))
    else {
        eprintln!("skipping: sibling binaries not built (run under cargo test --workspace)");
        return;
    };
    let (kernel, host, root) = setup("introspect");

    let content = "the quick brown artifact jumped over the lazy preview";
    let meta = kernel
        .cas
        .put_bytes(
            content.as_bytes(),
            "text/plain",
            portos_proto::Label::public_trusted(),
            "test",
        )
        .unwrap();

    let turn1 = sse(&[
        ("message_start", json!({"type": "message_start"})),
        ("content_block_start", json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_r", "name": "artifact__read",
                               "input": {}}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta",
                       "partial_json": json!({"id": meta.id}).to_string()}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}})),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let turn2 = sse(&[
        ("message_start", json!({"type": "message_start"})),
        ("content_block_start", json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Read it."}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let (port, captured) = mock_provider(vec![turn1, turn2]);

    let broker_dir = root.join("broker");
    write_json(
        &broker_dir.join("config.json"),
        &json!({"allow": [{"host": "127.0.0.1", "insecure_http": true}]}),
    );
    write_json(&broker_dir.join("secrets.json"), &json!({}));
    host.spawn(&broker_bin, &[], &[("PORTOS_BROKER_DIR", broker_dir.to_str().unwrap())])
        .unwrap();
    host.spawn(&echo_bin, &[], &[("PORTOS_ECHO_FAMILY", "echoa")])
        .unwrap();

    // NOTE: no tools in the config — introspection provides the surface.
    let modeld_dir = root.join("modeld");
    write_json(
        &modeld_dir.join("config.json"),
        &json!({
            "backend": "anthropic",
            "base_url": format!("http://127.0.0.1:{port}"),
            "model": "test-model",
            "max_tokens": 512,
        }),
    );
    let modeld = host
        .spawn(
            Path::new(MODELD_BIN),
            &[],
            &[("PORTOS_MODELD_DIR", modeld_dir.to_str().unwrap())],
        )
        .unwrap();

    for (resource, verbs) in [
        ("driver:egress", vec!["http", "http_stream"]),
        ("driver:echoa", vec!["emit"]),
    ] {
        kernel
            .caps
            .mint(
                "plugin:portos-modeld",
                resource,
                verbs.into_iter().map(String::from).collect::<BTreeSet<_>>(),
                Constraints::default(),
                None,
            )
            .unwrap();
    }

    let started = host.call(&modeld, "model::start", json!({})).unwrap();
    let sid = started["session"].as_str().unwrap().to_string();
    let out = host
        .call(&modeld, "model::send", json!({"session": sid, "text": "read the artifact"}))
        .unwrap();
    assert_eq!(out["text"].as_str(), Some("Read it."));

    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    // Tool surface: introspected echo verb with the driver's own description,
    // plus the built-in reader — and egress stays plumbing, never a tool.
    assert!(reqs[0].contains("echoa__emit"), "introspected tool present");
    assert!(reqs[0].contains("Print a line"), "driver-advertised description flowed through");
    assert!(reqs[0].contains("artifact__read"), "built-in reader present");
    assert!(!reqs[0].contains("egress__"), "egress must not surface as a model tool");
    // The artifact's full content reached the provider via artifact::read.
    assert!(reqs[1].contains("tool_result"));
    assert!(reqs[1].contains("quick brown artifact"));

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unknown_session_and_lifecycle() {
    let (_k, host, root) = setup("life");
    let modeld_dir = root.join("modeld");
    write_json(&modeld_dir.join("config.json"), &json!({"backend": "anthropic"}));
    let modeld = host
        .spawn(
            Path::new(MODELD_BIN),
            &[],
            &[("PORTOS_MODELD_DIR", modeld_dir.to_str().unwrap())],
        )
        .unwrap();

    let err = host.call(&modeld, "model::send", json!({"session": "nope", "text": "x"}));
    assert!(err.unwrap_err().to_string().contains("unknown session"));

    let s = host.call(&modeld, "model::start", json!({})).unwrap();
    let sid = s["session"].as_str().unwrap().to_string();
    let ended = host.call(&modeld, "model::end", json!({"session": sid})).unwrap();
    assert_eq!(ended["ended"].as_bool(), Some(true));
    let again = host.call(&modeld, "model::end", json!({"session": "s1"})).unwrap();
    assert_eq!(again["ended"].as_bool(), Some(false));

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}
