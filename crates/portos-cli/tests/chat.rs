//! The walking skeleton, closed: the browser adapter over the plugin ABI,
//! and `portos chat` driven end to end through the real CLI binary —
//! user line → modeld → broker (key injection) → scripted provider →
//! tool_use → capability-gated invoke → headless Chromium via the browser
//! driver → tool_result → final streamed text. Hermetic; skips when node or
//! the browser driver's node_modules are absent.

use portos_kernel::Kernel;
use portos_kernel::host::Host;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const CLI_BIN: &str = env!("CARGO_BIN_EXE_portos");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn browser_ready() -> Option<(PathBuf, PathBuf)> {
    if std::process::Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: node not found");
        return None;
    }
    let plugin = repo_root().join("drivers/browser/src/plugin.js");
    let modules = repo_root().join("drivers/browser/node_modules/playwright");
    if !plugin.exists() || !modules.exists() {
        eprintln!("skipping: browser driver not installed (npm install in drivers/browser)");
        return None;
    }
    Some((plugin, repo_root().join("drivers/browser/test/fixture.html")))
}

fn setup(tag: &str) -> (Arc<Kernel>, Host, PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-chat-{tag}-{}", std::process::id()));
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

/// Serve one fixed HTTP document `n` times (content-type matters to Chromium).
fn serve_html(n: usize, html: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for _ in 0..n {
            let Ok((mut conn, _)) = listener.accept() else { return };
            let _ = read_request(&mut conn);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                html.len()
            );
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(html.as_bytes());
        }
    });
    port
}

fn sse(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(e, d)| format!("event: {e}\ndata: {d}\n\n"))
        .collect()
}

fn mock_provider(bodies: Vec<String>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    std::thread::spawn(move || {
        for body in bodies {
            let Ok((mut conn, _)) = listener.accept() else { return };
            let req = read_request(&mut conn);
            cap.lock().unwrap().push(req);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(body.as_bytes());
        }
    });
    (port, captured)
}

fn write_json(path: &Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
}

/// The adapter alone: browser::* verbs over the plugin ABI, the sink's
/// kernel mode, screenshot-as-artifact, and origin taint labels.
#[test]
fn browser_adapter_serves_verbs_and_data_plane() {
    let Some((plugin, fixture)) = browser_ready() else { return };
    let (kernel, host, root) = setup("adapter");
    let profile = root.join("profile");

    // Phase A: generous inline cap — the fixture snapshot stays inline, and
    // a screenshot becomes a CAS artifact instead of a loose file.
    let name = host
        .spawn(
            Path::new("node"),
            &[plugin.to_str().unwrap()],
            &[
                ("WORKSHOP_HEADLESS", "1"),
                ("WORKSHOP_PROFILE_DIR", profile.to_str().unwrap()),
                ("WORKSHOP_SINK_INLINE_MAX", "200000"),
            ],
        )
        .unwrap();
    assert_eq!(name, "portos-browser");

    let url = format!("file://{}", fixture.display());
    let snap = host.call(&name, "browser::open", json!({"url": url})).unwrap();
    assert!(snap["title"].as_str().unwrap().contains("Workshop Fixture"));
    assert!(snap["elements"].as_array().unwrap().len() >= 3, "inline snapshot keeps the element table");

    let shot = host.call(&name, "browser::screenshot", json!({})).unwrap();
    let handle = shot["handle"].as_str().unwrap().to_string();
    let meta = kernel.cas.meta(&handle).unwrap();
    assert_eq!(meta.r#type, "image/png");
    assert_eq!(Some(meta.size), shot["size"].as_u64());
    host.shutdown(&name);

    // Phase B: tiny inline cap + a real http origin — the snapshot goes to
    // the CAS with a web:<origin> taint label; the model gets handle+preview.
    let html = std::fs::read_to_string(&fixture).unwrap();
    let port = serve_html(2, html);
    let origin = format!("http://127.0.0.1:{port}");
    let name = host
        .spawn(
            Path::new("node"),
            &[plugin.to_str().unwrap()],
            &[
                ("WORKSHOP_HEADLESS", "1"),
                ("WORKSHOP_PROFILE_DIR", profile.to_str().unwrap()),
                ("WORKSHOP_SINK_INLINE_MAX", "64"),
            ],
        )
        .unwrap();
    let out = host
        .call(&name, "browser::open", json!({"url": format!("{origin}/")}))
        .unwrap();
    let handle = out["handle"].as_str().expect("oversized snapshot becomes a handle");
    assert!(!out["preview"].as_str().unwrap().is_empty());
    let meta = kernel.cas.meta(&handle.to_string()).unwrap();
    assert_eq!(meta.r#type, "web/page-snapshot");
    assert!(
        meta.labels.integ.contains(&format!("web:{origin}")),
        "page-derived artifact carries its origin taint: {:?}",
        meta.labels
    );

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Rendering as a plugin (D32): builtin off, a renderer driver subscribed to
/// `model::session::*` owns the terminal output.
#[test]
fn portos_chat_with_renderer_plugin() {
    if std::process::Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: node not found");
        return;
    }
    let renderer = repo_root().join("drivers/render-tty/render.mjs");
    assert!(renderer.exists());
    let cli = Path::new(CLI_BIN);
    if !cli.with_file_name("portos-broker").exists() || !cli.with_file_name("portos-modeld").exists() {
        eprintln!("skipping: sibling binaries not built");
        return;
    }

    let root = std::env::temp_dir().join(format!("portos-chat-render-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let turn = sse(&[
        ("message_start", json!({"type": "message_start"})),
        ("content_block_start", json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Hello from the renderer"}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let (port, _captured) = mock_provider(vec![turn]);

    write_json(
        &root.join("broker/config.json"),
        &json!({"allow": [{"host": "127.0.0.1", "insecure_http": true}]}),
    );
    write_json(&root.join("broker/secrets.json"), &json!({}));
    write_json(
        &root.join("modeld/config.json"),
        &json!({
            "backend": "anthropic",
            "base_url": format!("http://127.0.0.1:{port}"),
            "model": "test-model",
            "max_tokens": 128,
        }),
    );
    write_json(
        &root.join("chat.json"),
        &json!({
            "render": "none",
            "plugins": [{"bin": "node", "args": [renderer.to_str().unwrap()]}],
        }),
    );

    let mut child = std::process::Command::new(cli)
        .args(["chat", root.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(120));
        let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).output();
    });
    child.stdin.take().unwrap().write_all(b"hi\n/exit\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "chat exited badly.\nstdout:\n{stdout}\nstderr:\n{stderr}");

    assert!(
        stdout.contains("Hello from the renderer"),
        "renderer printed the deltas:\n{stdout}"
    );
    assert!(
        stdout.contains("turn done"),
        "renderer's own turn marker present:\n{stdout}"
    );
    assert!(
        stdout.contains("builtin rendering off"),
        "builtin renderer was disabled:\n{stdout}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The whole runtime through the real CLI binary and a scripted provider.
#[test]
fn portos_chat_end_to_end() {
    let Some((plugin, fixture)) = browser_ready() else { return };
    let cli = Path::new(CLI_BIN);
    let have = |n: &str| cli.with_file_name(n).exists();
    if !have("portos-broker") || !have("portos-modeld") {
        eprintln!("skipping: sibling binaries not built (run under cargo test --workspace)");
        return;
    }

    let root = std::env::temp_dir().join(format!("portos-chat-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let fixture_url = format!("file://{}", fixture.display());

    let turn1 = sse(&[
        ("message_start", json!({"type": "message_start"})),
        ("content_block_start", json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Opening the page."}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ("content_block_start", json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "browser__open",
                               "input": {}}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta",
                       "partial_json": json!({"url": fixture_url}).to_string()}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 1})),
        ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}})),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let turn2 = sse(&[
        ("message_start", json!({"type": "message_start"})),
        ("content_block_start", json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}})),
        ("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Opened: done"}})),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let (port, captured) = mock_provider(vec![turn1, turn2]);

    // Configs the chat command will pick up (templates only write if absent).
    write_json(
        &root.join("broker/config.json"),
        &json!({"allow": [{"host": "127.0.0.1", "insecure_http": true,
                            "inject": {"x-api-key": "k1"}}]}),
    );
    write_json(&root.join("broker/secrets.json"), &json!({"k1": "fake-test-key"}));
    write_json(
        &root.join("modeld/config.json"),
        &json!({
            "backend": "anthropic",
            "base_url": format!("http://127.0.0.1:{port}"),
            "model": "test-model",
            "max_tokens": 512,
            "system": "You are the PortOS assistant.",
            "tools": [{
                "verb": "browser::open",
                "description": "Open a page in the watchable browser.",
                "schema": {"type": "object", "properties": {"url": {"type": "string"}},
                            "required": ["url"]},
            }],
        }),
    );
    write_json(
        &root.join("chat.json"),
        &json!({
            "plugins": [{
                "bin": "node",
                "args": [plugin.to_str().unwrap()],
                "env": {"WORKSHOP_HEADLESS": "1",
                         "WORKSHOP_PROFILE_DIR": root.join("profile").to_str().unwrap()},
            }],
            "grants": [{"resource": "driver:browser", "verbs": ["open"]}],
        }),
    );

    let mut child = std::process::Command::new(cli)
        .args(["chat", root.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(120));
        let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).output();
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"please open the fixture page\n/exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "chat exited badly.\nstdout:\n{stdout}\nstderr:\n{stderr}");

    assert!(stdout.contains("Opening the page."), "first-turn deltas streamed:\n{stdout}");
    assert!(stdout.contains("[tool→] browser::open"), "tool call surfaced:\n{stdout}");
    assert!(stdout.contains("[tool✓] browser::open"), "tool result surfaced:\n{stdout}");
    assert!(stdout.contains("Opened: done"), "final turn streamed:\n{stdout}");

    // The provider saw the injected key, the tool definition, and — in turn
    // two — the browser's actual snapshot riding in the tool result.
    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].contains("x-api-key: fake-test-key"));
    assert!(reqs[0].contains("browser__open"));
    assert!(reqs[1].contains("tool_result"));
    assert!(
        reqs[1].contains("Workshop Fixture"),
        "the page title flowed browser → modeld → provider"
    );

    let _ = std::fs::remove_dir_all(&root);
}
