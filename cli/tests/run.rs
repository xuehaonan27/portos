//! The walking skeleton, closed: the browser adapter over the plugin ABI,
//! and `portos run` driven end to end through the real CLI binary — the
//! launcher starts what `portos.json` lists and nothing else; the terminal
//! front end (a plugin, its stdin this test's pipe) sends the line →
//! modeld → broker (key injection) → scripted provider → tool_use →
//! capability-gated invoke → headless Chromium via the browser driver →
//! tool_result → streamed text back out through the same front end.
//! Hermetic; skips when node or the browser driver's node_modules are
//! absent.

use portos_kernel::Kernel;
use portos_kernel::host::Host;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// --- test boundary ------------------------------------------------------
// A test owns the domain meaning of what it sends and asserts, so it works
// in plain JSON. These wrappers convert at that boundary, which is exactly
// where the typed kernel API expects it to happen.

fn vb(s: &str) -> portos_abi::ids::Verb {
    portos_abi::ids::Verb::parse(s).expect("test verb")
}

fn pl(v: &serde_json::Value) -> portos_abi::wire::Payload {
    portos_abi::wire::Payload::of(v).expect("test payload")
}

fn js(p: &portos_abi::wire::Payload) -> serde_json::Value {
    p.parse().expect("test payload is json")
}

fn call(
    host: &Host,
    plugin: &portos_abi::ids::PluginName,
    verb: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, portos_kernel::KernelError> {
    host.call(plugin, &vb(verb), pl(&args)).map(|p| js(&p))
}

const CLI_BIN: &str = env!("CARGO_BIN_EXE_portos");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .unwrap()
}

fn browser_ready() -> Option<(PathBuf, PathBuf)> {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: node not found");
        return None;
    }
    let plugin = repo_root().join("plugins/browser/src/plugin.js");
    let modules = repo_root().join("plugins/browser/node_modules/playwright");
    if !plugin.exists() || !modules.exists() {
        eprintln!("skipping: browser driver not installed (npm install in plugins/browser)");
        return None;
    }
    Some((
        plugin,
        repo_root().join("plugins/browser/test/fixture.html"),
    ))
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
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
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
            let _ = conn.write_all(body.as_bytes());
        }
    });
    (port, captured)
}

fn write_json(path: &Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
}

/// The terminal front end, granted the model driver. Its stdin is whatever
/// the launcher's is — here, the test's pipe.
fn tty_plugin() -> Value {
    json!({"bin": "portos-tty", "grants": [
        {"resource": "driver:model", "verbs": ["start", "send", "cancel", "end", "sessions"]},
        {"resource": "driver:kernel", "verbs": ["plugins"]},
    ]})
}

/// The set `portos init` writes, for a root under test: broker, model driver
/// with the grants a test adds, and the front end.
fn standard_plugins(root: &Path, modeld_grants: Vec<Value>) -> Vec<Value> {
    let mut grants = vec![json!({"resource": "driver:egress", "verbs": ["http", "http_stream"]})];
    grants.extend(modeld_grants);
    vec![
        json!({"bin": "portos-broker", "env": {"PORTOS_BROKER_DIR": root.join("broker")}}),
        json!({"bin": "portos-modeld", "env": {"PORTOS_MODELD_DIR": root.join("modeld")},
               "grants": grants}),
        tty_plugin(),
    ]
}

/// The browser plugin alone: `browser::*` over the plugin ABI, what it
/// advertises being what `drivers/browser` says, a screenshot as an
/// artifact, and an element table too big for the context arriving as a
/// handle with a preview.
#[test]
fn browser_plugin_implements_the_interface() {
    let Some((plugin, fixture)) = browser_ready() else {
        return;
    };
    let echo = Path::new(CLI_BIN).with_file_name("portos-echo");
    if !echo.exists() {
        eprintln!("skipping: portos-echo not built (use `cargo test --workspace`)");
        return;
    }
    let (kernel, host, root) = setup("browser");
    let profile = root.join("profile");
    let config = json!({"headless": true, "profile_dir": profile}).to_string();
    let spawn = || {
        host.spawn(
            Path::new("node"),
            &[plugin.to_str().unwrap()],
            &[("PORTOS_PLUGIN_CONFIG", config.as_str())],
        )
        .unwrap()
    };

    let name = spawn();
    assert_eq!(name.as_str(), "portos-browser");

    // Conformance: what the JS advertised, seen through grants introspection
    // by a plugin granted all of it, is the Rust interface word for word.
    let probe = host
        .spawn(Path::new(&echo), &[], &[("PORTOS_ECHO_DRIVER", "probe")])
        .unwrap();
    let interface = portos_browser_api::tools();
    kernel
        .caps
        .mint(
            &probe.subject(),
            "driver:browser",
            interface.keys().map(|v| v.short().to_string()).collect(),
            Default::default(),
            None,
        )
        .unwrap();
    let advertised = call(&host, &probe, "probe::grants", json!([])).unwrap();
    let advertised = advertised.as_array().unwrap();
    for (verb, meta) in &interface {
        let g = advertised
            .iter()
            .find(|g| g["verb"] == verb.as_str())
            .unwrap_or_else(|| panic!("{verb} is advertised"));
        assert_eq!(g["description"], meta.description, "{verb}");
        let schema: Value = meta.schema.as_ref().unwrap().parse().unwrap();
        assert_eq!(g["schema"], schema, "{verb}");
    }
    assert_eq!(advertised.len(), interface.len(), "and nothing beyond it");

    // The fixture's table fits in context; a screenshot never does.
    let url = format!("file://{}", fixture.display());
    let snap = call(&host, &name, "browser::open", json!({"url": url})).unwrap();
    assert!(snap["title"].as_str().unwrap().contains("Workshop Fixture"));
    assert!(
        snap["elements"].as_array().unwrap().len() >= 3,
        "inline table: {snap}"
    );
    let shot = call(&host, &name, "browser::screenshot", json!({})).unwrap();
    let handle = shot["handle"].as_str().unwrap().to_string();
    let meta = kernel.cas.meta(&handle).unwrap();
    assert_eq!(meta.r#type, "image/png");
    assert_eq!(Some(meta.size), shot["size"].as_u64());
    host.shutdown(&name);

    // A page whose table does not fit: the model gets a handle and a
    // preview, the artifact says which origin it came from. The line is a
    // constant, so the test makes a bigger page rather than turning a knob.
    let inputs: String = (0..150)
        .map(|i| {
            format!(
                r#"<input name="f{i}" aria-label="{} {i}">"#,
                "x".repeat(120)
            )
        })
        .collect();
    let port = serve_html(
        2,
        format!("<!doctype html><title>Big</title><form>{inputs}</form>"),
    );
    let origin = format!("http://127.0.0.1:{port}");
    let name = spawn();
    let out = call(
        &host,
        &name,
        "browser::open",
        json!({"url": format!("{origin}/")}),
    )
    .unwrap();
    let handle = out["handle"]
        .as_str()
        .unwrap_or_else(|| panic!("an oversized table becomes a handle: {out}"));
    assert!(!out["preview"].as_str().unwrap().is_empty());
    let meta = kernel.cas.meta(&handle.to_string()).unwrap();
    assert_eq!(
        meta.r#type, "web/page-snapshot",
        "the type the driver declared"
    );
    // Stored by the SDK, not the plugin, so the provenance is the SDK's
    // convention: the verb and the arguments it was given.
    assert!(
        meta.labels.integ.contains("browser::open")
            && meta
                .labels
                .integ
                .iter()
                .any(|l| l.starts_with("args:") && l.contains(&origin)),
        "a page-derived artifact says where it came from: {:?}",
        meta.labels
    );

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Rendering as a plugin (D32): builtin off, a renderer driver subscribed to
/// `model::session::*` owns the terminal output.
#[test]
fn portos_run_with_renderer_plugin() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: node not found");
        return;
    }
    let renderer = repo_root().join("plugins/render-tty/render.mjs");
    assert!(renderer.exists());
    let cli = Path::new(CLI_BIN);
    let have = |n: &str| cli.with_file_name(n).exists();
    if !have("portos-broker") || !have("portos-modeld") || !have("portos-tty") {
        eprintln!("skipping: sibling binaries not built");
        return;
    }

    let root = std::env::temp_dir().join(format!("portos-run-render-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let turn = sse(&[
        ("message_start", json!({"type": "message_start"})),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Hello from the renderer"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ),
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
    // Two renderers on one session: the front end and the renderer plugin
    // both subscribe, and both print.
    let mut plugins = standard_plugins(&root, vec![]);
    plugins.push(json!({"bin": "node", "args": [renderer.to_str().unwrap()]}));
    write_json(&root.join("portos.json"), &json!({"plugins": plugins}));

    let mut child = std::process::Command::new(cli)
        .args(["run", root.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(120));
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hi\n/exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "run exited badly.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    assert!(
        stdout.contains("Hello from the renderer"),
        "renderer printed the deltas:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("turn done"),
        "renderer's own turn marker present:\n{stdout}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The guarantee a plugin author relies on, exercised on the most entangled
/// plugin there is: a *different* model driver listed in `portos.json`, and
/// nothing else changed. The front end addresses the driver by verb, so it
/// cannot tell; the launcher starts the list and nothing else, so no
/// standard driver appears beside it.
#[test]
fn portos_run_with_another_model_driver() {
    let cli = Path::new(CLI_BIN);
    let have = |n: &str| cli.with_file_name(n).exists();
    if !have("portos-model-echo") || !have("portos-tty") {
        eprintln!("skipping: sibling binaries not built");
        return;
    }

    let root = std::env::temp_dir().join(format!("portos-run-other-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // Bare names: found beside the CLI, the way the standard plugins are.
    // No broker: this driver needs no network, so nothing lists one.
    write_json(
        &root.join("portos.json"),
        &json!({"plugins": [{"bin": "portos-model-echo"}, tty_plugin()]}),
    );

    let mut child = std::process::Command::new(cli)
        .args(["run", root.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(120));
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/ps\nsay this back\n/exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "run exited badly.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    assert!(
        stdout.contains("started portos-model-echo"),
        "the listed driver was started:\n{stdout}"
    );
    assert!(
        !stdout.contains("started portos-modeld") && !stdout.contains("started portos-broker"),
        "and nothing beyond the list:\n{stdout}"
    );
    assert!(
        stdout.contains("say this back"),
        "the turn went through the other driver and came back:\n{stdout}"
    );
    // The overview: the driver, the instance under it, what it is by
    // content, what it was started from, and what it answers — and the
    // front end itself, answering nothing.
    assert!(
        stdout.contains("[tty] model\n  portos-model-echo  blake3:")
            && stdout.contains("  bin ")
            && stdout.contains("  cancel end send sessions start"),
        "/ps lists the driver's instance with its content, source and verbs:\n{stdout}"
    );
    assert!(
        stdout.contains("[tty] (no verbs)\n  portos-tty  blake3:"),
        "/ps lists a plugin with no verbs too:\n{stdout}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The whole runtime through the real CLI binary and a scripted provider.
/// A plugin as data, through the CLI: the driver's executable goes into the
/// CAS, `portos plugin` runs it once and stores what it declared as a
/// manifest, and `portos.json` lists the plugin by that one id — nothing
/// about what runs is in the file. The overview then shows what it is by
/// content.
#[test]
fn portos_run_with_a_plugin_named_by_one_id() {
    let cli = Path::new(CLI_BIN);
    let driver = cli.with_file_name("portos-model-echo");
    if !driver.exists() || !cli.with_file_name("portos-tty").exists() {
        eprintln!("skipping: sibling binaries not built");
        return;
    }
    let root = std::env::temp_dir().join(format!("portos-run-manifest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let cli_json = |args: &[&str]| -> Value {
        let out = std::process::Command::new(cli).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "portos {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let put = cli_json(&["put", root.to_str().unwrap(), driver.to_str().unwrap()]);
    let executable = put["id"].as_str().unwrap().to_string();
    let spec = root.join("model-echo.json");
    write_json(&spec, &json!({"artifact": executable}));
    let plugin_fails = |why: &str| -> String {
        let out = std::process::Command::new(cli)
            .args(["plugin", root.to_str().unwrap(), spec.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(!out.status.success(), "no manifest {why}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    // A declaration is held to a document, so without one there is no
    // manifest — and the message says what to run.
    let err = plugin_fails("without a driver document");
    assert!(err.contains("portos driver"), "{err}");

    // A document that says something else: the plugin does not conform,
    // the departure is named, and there is no manifest.
    let model_doc = repo_root().join("drivers/model/driver.json");
    let mut wrong: Value =
        serde_json::from_str(&std::fs::read_to_string(&model_doc).unwrap()).unwrap();
    wrong["verbs"]["send"]["description"] = json!("Something else entirely.");
    write_json(&root.join("wrong.json"), &wrong);
    cli_json(&[
        "driver",
        root.to_str().unwrap(),
        root.join("wrong.json").to_str().unwrap(),
    ]);
    let err = plugin_fails("against a document it departs from");
    assert!(
        err.contains("does not conform to driver model")
            && err.contains("model::send: description"),
        "{err}"
    );

    // The document itself: registered under its name, the manifest names it.
    let registered = cli_json(&[
        "driver",
        root.to_str().unwrap(),
        model_doc.to_str().unwrap(),
    ]);
    assert_eq!(registered["type"], "portos/driver");
    let stored = cli_json(&["plugin", root.to_str().unwrap(), spec.to_str().unwrap()]);
    assert_eq!(stored["type"], "portos/plugin");
    let manifest = stored["id"].as_str().unwrap().to_string();
    let out = root.join("manifest.json");
    let got = std::process::Command::new(cli)
        .args([
            "get",
            root.to_str().unwrap(),
            &manifest,
            out.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(got.status.success());
    let manifest_doc: Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(
        manifest_doc["conforms"]["model"], registered["id"],
        "the manifest names the document it was held to, by content"
    );

    write_json(
        &root.join("portos.json"),
        &json!({"plugins": [{"plugin": manifest}, tty_plugin()]}),
    );
    let mut child = std::process::Command::new(cli)
        .args(["run", root.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(120));
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/ps\nsay this back\n/exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "run exited badly.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("say this back"),
        "the turn went through a driver the file names by one id:\n{stdout}"
    );
    // By content (the front end shows `blake3:` and twelve hex digits), and
    // by the manifest the file named.
    let short = &executable[..executable.find(':').unwrap() + 13];
    assert!(
        stdout.contains(&format!("  portos-model-echo  {short}  plugin {manifest}")),
        "/ps shows what it is by content and what named it:\n{stdout}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn portos_run_end_to_end() {
    let Some((plugin, fixture)) = browser_ready() else {
        return;
    };
    let cli = Path::new(CLI_BIN);
    let have = |n: &str| cli.with_file_name(n).exists();
    if !have("portos-broker") || !have("portos-modeld") || !have("portos-tty") {
        eprintln!("skipping: sibling binaries not built (run under cargo test --workspace)");
        return;
    }

    let root = std::env::temp_dir().join(format!("portos-run-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let fixture_url = format!("file://{}", fixture.display());

    let turn1 = sse(&[
        ("message_start", json!({"type": "message_start"})),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Opening the page."}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "browser__open",
                               "input": {}}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta",
                       "partial_json": json!({"url": fixture_url}).to_string()}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 1}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let turn2 = sse(&[
        ("message_start", json!({"type": "message_start"})),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Opened: done"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    let (port, captured) = mock_provider(vec![turn1, turn2]);

    // What the plugins read.
    write_json(
        &root.join("broker/config.json"),
        &json!({"allow": [{"host": "127.0.0.1", "insecure_http": true,
                            "inject": {"x-api-key": "k1"}}]}),
    );
    write_json(
        &root.join("broker/secrets.json"),
        &json!({"k1": "fake-test-key"}),
    );
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
    let mut plugins = standard_plugins(
        &root,
        vec![json!({"resource": "driver:browser", "verbs": ["open"]})],
    );
    plugins.push(json!({
        "bin": "node",
        "args": [plugin.to_str().unwrap()],
        "config": {"headless": true, "profile_dir": root.join("profile")},
    }));
    write_json(&root.join("portos.json"), &json!({"plugins": plugins}));

    let mut child = std::process::Command::new(cli)
        .args(["run", root.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(120));
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
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
    assert!(
        out.status.success(),
        "run exited badly.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    assert!(
        stdout.contains("Opening the page."),
        "first-turn deltas streamed:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("[tool→] browser::open"),
        "tool call surfaced:\n{stdout}"
    );
    assert!(
        stdout.contains("[tool✓] browser::open"),
        "tool result surfaced:\n{stdout}"
    );
    assert!(
        stdout.contains("Opened: done"),
        "final turn streamed:\n{stdout}"
    );

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
