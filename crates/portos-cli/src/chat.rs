//! `portos chat <root>` — the standalone runtime's front door.
//!
//! Spawns the trusted egress broker and the model driver (sibling binaries),
//! plus any extra drivers listed in `<root>/chat.json`, mints the configured
//! capability grants, then runs a REPL: each line goes to `model::send`
//! while deltas and tool activity stream live from the session's event
//! topic. The kernel stays in this process (library-linked); daemonization
//! is still deferred.
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
//! Rendering is not a special mechanism: a renderer is an ordinary plugin
//! subscribed to the event plane; list one under `plugins` and set
//! `"render": "none"` to replace the builtin stdout rendering — or leave
//! both on and they compose. The model provider is whatever the modeld
//! backend's `base_url` points at — any endpoint speaking the configured
//! wire protocol, not a fixed vendor. Its host must be on the broker
//! allowlist (checked at startup), with whatever auth header that endpoint
//! wants in the broker's inject rule — the key never leaves the broker.

use nix::sys::signal::{SigSet, Signal};
use portos_egress_api as egress;
use portos_kernel::Kernel;
use portos_kernel::host::Host;
use portos_model_api as model;
use portos_proto::ids::PluginName;
use portos_proto::wire::Payload;
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// `<root>/chat.json`: which extra drivers to start and what they may do.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ChatConfig {
    plugins: Vec<PluginSpec>,
    grants: Vec<GrantSpec>,
    render: RenderMode,
}

#[derive(Debug, Deserialize)]
struct PluginSpec {
    bin: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// A grant is declared, not negotiated: whatever is listed here is what the
/// subject may invoke.
#[derive(Debug, Deserialize)]
struct GrantSpec {
    /// Defaults to the model driver, which is what almost every grant is for.
    #[serde(default)]
    subject: Option<String>,
    resource: String,
    #[serde(default)]
    verbs: BTreeSet<String>,
}

#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RenderMode {
    #[default]
    Builtin,
    None,
}

pub fn run(root: &str, repl: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Block the interrupt signals before anything else starts a thread, so
    // every thread inherits the mask and only the handler thread below ever
    // sees them. Without this, Ctrl-C tears the process down without running
    // teardown, and every plugin — and everything it started — is orphaned.
    let signals = block_interrupts()?;

    let root = PathBuf::from(root);
    let kernel = Arc::new(Kernel::open(&root)?);
    let host = Arc::new(Host::new(kernel.clone(), &root.join("sock"))?);
    host.audit_topic(&egress::LOG);

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
        &modeld.subject(),
        "driver:egress",
        BTreeSet::from([
            egress::HTTP.short().to_string(),
            egress::HTTP_STREAM.short().to_string(),
        ]),
        Default::default(),
        None,
    )?;

    warn_if_provider_host_unlisted(&root);

    let cfg: ChatConfig = match std::fs::read_to_string(root.join("chat.json")) {
        Ok(text) => serde_json::from_str(&text)?,
        Err(_) => {
            println!("[chat] no chat.json — model-only chat (add one to wire in drivers)");
            ChatConfig::default()
        }
    };
    let render_builtin = cfg.render == RenderMode::Builtin;
    if !render_builtin {
        println!("[chat] builtin rendering off — renderer plugins own the output");
    }
    for p in &cfg.plugins {
        let args: Vec<String> = p.args.iter().map(|a| resolve_arg(&root, a)).collect();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let env_refs: Vec<(&str, &str)> = p
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let name = host.spawn(Path::new(&p.bin), &arg_refs, &env_refs)?;
        println!("[chat] plugin: {name}");
    }
    for g in &cfg.grants {
        let subject = g.subject.clone().unwrap_or_else(|| modeld.subject());
        kernel.caps.mint(
            &subject,
            &g.resource,
            g.verbs.clone(),
            Default::default(),
            None,
        )?;
        println!("[chat] grant: {subject} → {}", g.resource);
    }

    // Whether a turn is in flight, and which session it belongs to, so an
    // interrupt knows whether to cancel the turn or bring the runtime down.
    let busy = Arc::new(AtomicBool::new(false));
    let current: Arc<Mutex<Option<model::SessionId>>> = Arc::new(Mutex::new(None));
    spawn_signal_handler(
        signals,
        host.clone(),
        modeld.clone(),
        busy.clone(),
        current.clone(),
    );

    if !repl {
        // No REPL: the runtime is up and a front end owns its own sessions
        // (a bridge plugin's presenter calls `model::start` itself). Parking
        // here keeps the plugins alive.
        //
        // Known gap: a signal kills this process without running `Drop for
        // Host`, so plugins are orphaned rather than shut down. The fix is
        // process-group teardown, not a handler here.
        println!("[chat] runtime up, no REPL — drive it through a front end; Ctrl-C to stop");
        let (_keep, never) = std::sync::mpsc::channel::<()>();
        let _ = never.recv();
        return Ok(());
    }

    // One session; deltas render live from its event topic.
    let started = host.call(
        &modeld,
        &model::START,
        Payload::of(&model::StartArgs::default())?,
    )?;
    let sid = started.parse::<model::StartReply>()?.session;
    *current.lock().unwrap() = Some(sid.clone());
    let (_sub, rx) = host.subscribe_local(&sid.topic());
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        // The builtin renderer — one subscriber among possibly several
        // (renderer plugins subscribe to the same topics). Even with
        // rendering off it keeps consuming for the turn-done signal.
        for ev in rx {
            let Ok(event) = ev.data.parse::<model::SessionEvent>() else {
                continue;
            };
            match event {
                model::SessionEvent::Delta { text } if render_builtin => {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                model::SessionEvent::ToolCall { verb } if render_builtin => {
                    println!("\n[tool→] {verb}");
                    let _ = std::io::stdout().flush();
                }
                model::SessionEvent::ToolResult { verb, ok } if render_builtin => {
                    println!("[tool{}] {verb}", if ok { "✓" } else { "✗" });
                    let _ = std::io::stdout().flush();
                }
                // The three terminal events. Exactly one arrives per turn,
                // and the prompt comes back on any of them.
                model::SessionEvent::Done { .. } => {
                    if render_builtin {
                        println!();
                    }
                    let _ = done_tx.send(());
                }
                model::SessionEvent::Cancelled => {
                    if render_builtin {
                        println!("\n[chat] cancelled");
                    }
                    let _ = done_tx.send(());
                }
                model::SessionEvent::Failed { error } => {
                    if render_builtin {
                        println!("\n[chat] failed: {error}");
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
        let args = Payload::of(&model::SendArgs {
            session: sid.clone(),
            text: text.to_string(),
        })?;
        // `send` returns as soon as the turn is accepted; the turn itself is
        // over when a terminal event arrives, however long that takes.
        match host.call(&modeld, &model::SEND, args) {
            Ok(_) => {
                busy.store(true, Ordering::SeqCst);
                let _ = done_rx.recv();
                busy.store(false, Ordering::SeqCst);
            }
            Err(e) => println!("[chat] error: {e}"),
        }
    }
    if let Ok(end) = Payload::of(&model::EndArgs {
        session: sid.clone(),
    }) {
        let _ = host.call(&modeld, &model::END, end);
    }
    host.shutdown_all();
    Ok(())
}

/// Take the interrupt signals away from the default disposition, which is to
/// kill the process without running any teardown. Called before any thread
/// exists so the mask is inherited everywhere.
fn block_interrupts() -> Result<SigSet, Box<dyn std::error::Error>> {
    let mut set = SigSet::empty();
    set.add(Signal::SIGINT);
    set.add(Signal::SIGTERM);
    set.thread_block()?;
    Ok(set)
}

/// One thread that does nothing but wait for an interrupt. Waiting rather
/// than handling means it may do real work — call a verb, tear plugins down —
/// none of which is safe inside an actual signal handler.
fn spawn_signal_handler(
    signals: SigSet,
    host: Arc<Host>,
    modeld: PluginName,
    busy: Arc<AtomicBool>,
    current: Arc<Mutex<Option<model::SessionId>>>,
) {
    std::thread::spawn(move || {
        loop {
            if signals.wait().is_err() {
                return;
            }
            // A turn in flight is cancelled, not killed: an interrupt almost
            // always means "stop this", not "lose the session". Interrupting
            // again, or when idle, brings the runtime down.
            let session = current.lock().unwrap().clone();
            if busy.swap(false, Ordering::SeqCst) {
                if let Some(session) = session {
                    eprintln!("\n[chat] cancelling — interrupt again to quit");
                    if let Ok(p) = Payload::of(&model::CancelArgs { session }) {
                        let _ = host.call(&modeld, &model::CANCEL, p);
                    }
                    continue;
                }
            }
            eprintln!("\n[chat] shutting down");
            host.shutdown_all();
            std::process::exit(130);
        }
    });
}

/// The provider is vendor-neutral: modeld's backend `base_url` decides where
/// LLM traffic goes, and the broker allowlist must cover that host (with
/// whatever auth header that endpoint wants, in the broker's inject rule).
/// Catch the mismatch at startup instead of at the first opaque egress
/// denial.
///
/// No fallback host: modeld refuses to start without a `base_url`, and this
/// check guessing one would only make the eventual error less clear.
fn warn_if_provider_host_unlisted(root: &Path) {
    let Some(base_url) = std::fs::read_to_string(root.join("modeld/config.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|c| c["base_url"].as_str().map(String::from))
    else {
        return;
    };
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
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
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
                // A protocol, not a vendor: point base_url at any endpoint
                // that speaks the Anthropic Messages API. Whichever one it
                // is, its host and auth header belong in broker/config.json.
                "backend": "anthropic-compatible",
                "base_url": "https://api.anthropic.com",
                "model": "claude-opus-5",
                "max_tokens": 64000,
                "system": "You are the PortOS assistant. Use the available tools when they help.",
                "tools": [],
            }))
            .unwrap(),
        )?;
        println!(
            "[chat] wrote {} — base_url and model are required; change them and \
             broker/config.json together to use another endpoint",
            mcfg.display()
        );
    }
    Ok(())
}
