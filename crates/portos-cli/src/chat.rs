//! `portos chat <root>` — the standalone runtime's front door.
//!
//! Spawns the trusted egress broker and the model driver (sibling binaries),
//! plus any extra drivers listed in `<root>/chat.json`, mints the configured
//! capability grants, then runs a REPL: each line goes to `model::send`
//! while deltas and tool activity stream live from the session's event
//! topic. The kernel stays in this process (library-linked, D17);
//! daemonization is still deferred.
//!
//! Layout under `<root>`:
//!   broker/config.json + broker/secrets.json   (templates written if absent)
//!   modeld/config.json                         (template written if absent)
//!   chat.json                                  (optional: extra plugins + grants)
//!
//! chat.json shape:
//!   { "plugins": [ {"bin": "node", "args": ["…/plugin.js"], "env": {"K": "V"}} ],
//!     "grants":  [ {"subject"?: "plugin:portos-modeld",
//!                   "resource": "driver:browser", "verbs": ["open", …]} ],
//!     "render":  "builtin" | "none" }
//! Relative paths in `args` resolve against `<root>` when they exist there.
//!
//! Rendering (decisions-v1.md D32): a renderer is an ordinary plugin
//! subscribed to the event plane (`model::session::*`); list one under
//! `plugins` and set `"render": "none"` to replace the builtin stdout
//! rendering — or leave both on and they compose. The model provider is
//! whatever the modeld backend's `base_url` points at; its host must be on
//! the broker allowlist (checked at startup) with the vendor's auth header
//! in the broker's inject rule — the key never leaves the broker.

use portos_kernel::Kernel;
use portos_kernel::host::Host;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn run(root: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(root);
    let kernel = Arc::new(Kernel::open(&root)?);
    let host = Host::new(kernel.clone(), &root.join("sock"))?;
    host.audit_topic("egress::log");

    write_templates(&root)?;

    let exe = std::env::current_exe()?;
    let sibling = |name: &str| -> Result<PathBuf, String> {
        let p = exe.with_file_name(name);
        if p.exists() {
            Ok(p)
        } else {
            Err(format!("missing sibling binary: {}", p.display()))
        }
    };

    let broker_dir = root.join("broker");
    host.spawn(
        &sibling("portos-broker")?,
        &[],
        &[("PORTOS_BROKER_DIR", broker_dir.to_str().unwrap())],
    )?;
    let modeld_dir = root.join("modeld");
    let modeld = host.spawn(
        &sibling("portos-modeld")?,
        &[],
        &[("PORTOS_MODELD_DIR", modeld_dir.to_str().unwrap())],
    )?;
    println!("[chat] plugins: portos-broker, {modeld}");

    // The model driver always gets egress (its LLM calls go through the
    // broker; it holds no key and no network of its own).
    kernel.caps.mint(
        &format!("plugin:{modeld}"),
        "driver:egress",
        BTreeSet::from(["http".to_string(), "http_stream".to_string()]),
        Default::default(),
        None,
    )?;

    warn_if_provider_host_unlisted(&root);

    // Extra drivers + grants + render mode from chat.json.
    let mut render_builtin = true;
    if let Ok(text) = std::fs::read_to_string(root.join("chat.json")) {
        let cfg: Value = serde_json::from_str(&text)?;
        if cfg["render"].as_str() == Some("none") {
            render_builtin = false;
            println!("[chat] builtin rendering off — renderer plugins own the output");
        }
        if let Some(plugins) = cfg["plugins"].as_array() {
            for p in plugins {
                let bin = p["bin"].as_str().ok_or("chat.json plugin missing bin")?;
                let args: Vec<String> = p["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| resolve_arg(&root, s))
                            .collect()
                    })
                    .unwrap_or_default();
                let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                let envs: Vec<(String, String)> = p["env"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                let env_refs: Vec<(&str, &str)> =
                    envs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let name = host.spawn(Path::new(bin), &arg_refs, &env_refs)?;
                println!("[chat] plugin: {name}");
            }
        }
        if let Some(grants) = cfg["grants"].as_array() {
            for g in grants {
                let subject = g["subject"]
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("plugin:{modeld}"));
                let resource = g["resource"].as_str().ok_or("grant missing resource")?;
                let verbs: BTreeSet<String> = g["verbs"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                kernel.caps.mint(&subject, resource, verbs, Default::default(), None)?;
                println!("[chat] grant: {subject} → {resource}");
            }
        }
    } else {
        println!("[chat] no chat.json — model-only chat (add one to wire in drivers)");
    }

    // One session; deltas render live from its event topic.
    let started = host.call(&modeld, "model::start", json!({}))?;
    let sid = started["session"]
        .as_str()
        .ok_or("model driver returned no session id")?
        .to_string();
    let (_sub, rx) = host.subscribe_local(&format!("model::session::{sid}"));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        // The builtin renderer — one subscriber among possibly several
        // (renderer plugins subscribe to the same topics). Even with
        // rendering off it keeps consuming for the turn-done signal.
        for ev in rx {
            let d = &ev["data"];
            match d["kind"].as_str() {
                Some("delta") if render_builtin => {
                    print!("{}", d["text"].as_str().unwrap_or(""));
                    let _ = std::io::stdout().flush();
                }
                Some("tool_call") if render_builtin => {
                    println!("\n[tool→] {}", d["verb"].as_str().unwrap_or("?"));
                    let _ = std::io::stdout().flush();
                }
                Some("tool_result") if render_builtin => {
                    let ok = d["ok"].as_bool().unwrap_or(false);
                    println!("[tool{}] {}", if ok { "✓" } else { "✗" }, d["verb"].as_str().unwrap_or("?"));
                    let _ = std::io::stdout().flush();
                }
                Some("done") => {
                    if render_builtin {
                        println!();
                    }
                    let _ = done_tx.send(());
                }
                _ => {}
            }
        }
    });

    println!("[chat] session {sid} — type a message, /exit to quit\n");
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "/exit" || text == "/quit" {
            break;
        }
        match host.call(&modeld, "model::send", json!({"session": sid, "text": text})) {
            Ok(_) => {
                // Let the render thread finish printing this turn's events.
                let _ = done_rx.recv_timeout(std::time::Duration::from_secs(2));
            }
            Err(e) => println!("[chat] error: {e}"),
        }
    }
    let _ = host.call(&modeld, "model::end", json!({"session": sid}));
    host.shutdown_all();
    Ok(())
}

/// The provider is vendor-neutral: modeld's backend `base_url` decides where
/// LLM traffic goes, and the broker allowlist must cover that host (with the
/// vendor's auth header in its inject rule). Catch the mismatch at startup
/// instead of at the first opaque egress denial.
fn warn_if_provider_host_unlisted(root: &Path) {
    let base_url = std::fs::read_to_string(root.join("modeld/config.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|c| c["base_url"].as_str().map(String::from))
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
    let host = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&base_url)
        .split(['/', ':'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let listed = std::fs::read_to_string(root.join("broker/config.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|c| c["allow"].as_array().cloned())
        .map(|rules| {
            rules
                .iter()
                .any(|r| r["host"].as_str().map(str::to_ascii_lowercase) == Some(host.clone()))
        })
        .unwrap_or(false);
    if !listed {
        println!(
            "[chat] warning: model provider host {host} (modeld base_url) is not on the \
             broker allowlist — LLM calls will be denied; add it to broker/config.json"
        );
    }
}

fn resolve_arg(root: &Path, arg: &str) -> String {
    let candidate = root.join(arg);
    if !Path::new(arg).is_absolute() && candidate.exists() {
        candidate.to_string_lossy().into_owned()
    } else {
        arg.to_string()
    }
}

fn write_templates(root: &Path) -> std::io::Result<()> {
    let broker = root.join("broker");
    std::fs::create_dir_all(&broker)?;
    let cfg = broker.join("config.json");
    if !cfg.exists() {
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "allow": [{
                    "host": "api.anthropic.com",
                    "inject": {"x-api-key": "anthropic_api_key"},
                }],
            }))
            .unwrap(),
        )?;
        println!("[chat] wrote {}", cfg.display());
    }
    let secrets = broker.join("secrets.json");
    if !secrets.exists() {
        std::fs::write(
            &secrets,
            serde_json::to_string_pretty(&json!({"anthropic_api_key": ""})).unwrap(),
        )?;
        println!(
            "[chat] wrote {} — put your API key there (it stays in the broker; the model driver never sees it)",
            secrets.display()
        );
    }
    let modeld = root.join("modeld");
    std::fs::create_dir_all(&modeld)?;
    let mcfg = modeld.join("config.json");
    if !mcfg.exists() {
        std::fs::write(
            &mcfg,
            serde_json::to_string_pretty(&json!({
                "backend": "anthropic",
                "base_url": "https://api.anthropic.com",
                "model": "claude-opus-5",
                "max_tokens": 64000,
                "system": "You are the PortOS assistant. Use the available tools when they help.",
                "tools": [],
            }))
            .unwrap(),
        )?;
        println!("[chat] wrote {}", mcfg.display());
    }
    Ok(())
}
