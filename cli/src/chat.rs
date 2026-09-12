//! `portos chat <root>` — the standalone runtime's front door.
//!
//! Starts the plugins listed in `<root>/chat.json`, then the standard egress
//! broker and model driver (sibling binaries) for whichever of those two
//! drivers nothing listed answers, mints the configured capability grants,
//! then runs a REPL: each line goes to `model::send`
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
//! A `bin` with no directory is looked for beside this binary, then on PATH.
//! Listing a plugin that answers `model::*` or `egress::*` replaces the
//! standard one: this front end addresses both by verb and never by name.
//! `"name"` gives an entry its instance identifier — two browsers are two
//! entries with two names, and a caller with a choice names one.
//!
//! **Reload is re-plug.** `SIGHUP` re-reads `chat.json` (and the two
//! built-in drivers' config directories) and brings the running set back in
//! line: whatever changed is stopped and started again, whatever did not is
//! left alone. There is no second mechanism for it — `kernel::stop` plus
//! `spawn` already collect a plugin's whole residue and put it back, so a
//! reload is a policy over the plugin lifecycle rather than a per-field
//! "which settings are live" matrix, which is the kind of thing that grows
//! without end. A plugin's config files count as part of what it *is*:
//! filling in an API key changes the broker without changing its command
//! line, and that is exactly the case reload exists for.
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
use portos_abi::ids::{PluginName, Verb};
use portos_abi::wire::Payload;
use portos_egress_api as egress;
use portos_kernel::Kernel;
use portos_kernel::host::{Form, Host, LaunchSpec};
use portos_model_api as model;
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
    /// An executable stored in the CAS — `portos put <root> <file>` gives
    /// you the id. A plugin named this way is a thing rather than a
    /// location.
    #[serde(default)]
    artifact: Option<String>,
    /// A path on this host. Fine for `node` and other things already
    /// installed; not reproducible and not portable.
    #[serde(default)]
    bin: Option<String>,
    /// A tar archive in the CAS holding everything else the plugin needs —
    /// `portos bundle <root> <base> [paths…]` builds one. It becomes the
    /// working directory, so `args` are relative to it.
    #[serde(default)]
    bundle: Option<String>,
    /// What this plugin should be, in its own vocabulary. Handed down rather
    /// than left for the plugin to find, so editing it re-plugs that driver
    /// on the next reload without anyone declaring which files matter.
    #[serde(default)]
    config: Option<serde_json::Value>,
    /// The instance identifier. Needed only to run two instances of one
    /// driver; otherwise the plugin's own name is used.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Files this plugin reads when it starts. Listing one here makes its
    /// contents part of what the plugin *is*, so editing it and reloading
    /// re-plugs this driver and nothing else.
    #[serde(default)]
    watch: Vec<String>,
    /// How it runs: `cgroup` (the default) or `bare`. Part of the spec, so a
    /// change to it re-plugs the driver like any other.
    #[serde(default)]
    form: Form,
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

/// Which conversation a run should open.
pub enum Resume {
    /// A new one.
    New,
    /// The most recently touched stored one.
    Latest,
    Named(String),
}

/// `portos sessions <root>` — what has been stored, newest first.
///
/// Reads the driver's index rather than asking it, so it works with nothing
/// running. The format is `portos_model_api::SessionIndex`, which is where
/// the writer and this reader agree; the transcripts themselves stay in the
/// CAS and are not touched to produce this list.
pub fn sessions(root: &str) -> Result<(), Box<dyn std::error::Error>> {
    let index = model::SessionIndex::read(&PathBuf::from(root).join(model::DIR));
    let listed = index.by_recency();
    if listed.is_empty() {
        println!("no stored sessions");
        return Ok(());
    }
    println!(
        "{:<8} {:>6}  {:<20} {}",
        "SESSION", "TURNS", "UPDATED", "OPENING"
    );
    for (id, rec) in listed {
        println!(
            "{:<8} {:>6}  {:<20} {}",
            id,
            rec.turns,
            stamp(rec.updated_at),
            rec.title
        );
    }
    Ok(())
}

/// Unix seconds as something a person can read, without pulling in a date
/// library for one column.
fn stamp(secs: u64) -> String {
    let days = secs / 86_400;
    let (h, m) = ((secs % 86_400) / 3600, (secs % 3600) / 60);
    // 1970-01-01 plus `days`, by the civil-from-days algorithm.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

/// What to ask the driver for. Resuming and starting are the same verb; this
/// only decides which id, if any, to name.
fn start_args(
    root: &Path,
    resume: &Resume,
) -> Result<model::StartArgs, Box<dyn std::error::Error>> {
    let index = || model::SessionIndex::read(&root.join(model::DIR));
    let id = match resume {
        Resume::New => None,
        Resume::Named(name) => Some(model::SessionId::parse(name)?),
        Resume::Latest => match index().by_recency().first() {
            Some((id, _)) => Some(model::SessionId::parse(id)?),
            None => {
                println!("[chat] nothing stored to resume — starting a new session");
                None
            }
        },
    };
    if let Some(id) = &id {
        let idx = index();
        let turns = idx.sessions.get(id.as_str()).map(|r| r.turns).unwrap_or(0);
        println!("[chat] resuming {id} ({turns} turns)");
    }
    Ok(model::StartArgs {
        resume: id,
        ..Default::default()
    })
}

pub fn run(root: &str, repl: bool, resume: Resume) -> Result<(), Box<dyn std::error::Error>> {
    // Block the interrupt signals before anything else starts a thread, so
    // every thread inherits the mask and only the handler thread below ever
    // sees them. Without this, Ctrl-C tears the process down without running
    // teardown, and every plugin — and everything it started — is orphaned.
    let signals = block_interrupts()?;

    // Canonical for the same reason the kernel canonicalises its socket
    // directory: every path handed to a plugin has to mean the same thing
    // whatever working directory that plugin ends up with.
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
    let kernel = Arc::new(Kernel::open(&root)?);
    let host = Arc::new(Host::new(kernel.clone(), &root.join("sock"))?);
    host.audit_topic(&egress::LOG);

    write_templates(&root)?;

    let exe = std::env::current_exe()?;
    warn_if_provider_host_unlisted(&root);

    // What this process started, and what it has already granted. Startup and
    // reload run the same code over it; at startup everything is new.
    let state = Arc::new(Mutex::new(RuntimeState::default()));
    let failures = converge(&kernel, &host, &root, &exe, &mut state.lock().unwrap());
    if let Some(first) = failures.first() {
        return Err(first.clone().into());
    }
    match host.answerers_of(&model::START).as_slice() {
        [] => return Err("no model driver: nothing answers model::start".into()),
        [_] => {}
        several => {
            let names: Vec<String> = several.iter().map(|n| n.to_string()).collect();
            return Err(format!(
                "several model drivers are running ({}); this REPL drives one — list one in chat.json",
                names.join(", ")
            )
            .into());
        }
    }

    let render_builtin = load_chat_config(&root).render == RenderMode::Builtin;
    if !render_builtin {
        println!("[chat] builtin rendering off — renderer plugins own the output");
    }

    // Whether a turn is in flight, and which session it belongs to, so an
    // interrupt knows whether to cancel the turn or bring the runtime down.
    let busy = Arc::new(AtomicBool::new(false));
    let current: Arc<Mutex<Option<model::SessionId>>> = Arc::new(Mutex::new(None));
    spawn_signal_handler(
        signals,
        Reloadable {
            kernel: kernel.clone(),
            host: host.clone(),
            root: root.clone(),
            exe,
            state,
        },
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
        if !matches!(resume, Resume::New) {
            println!("[chat] --resume has no effect without a REPL: a front end opens its own");
        }
        println!(
            "[chat] runtime up, no REPL — drive it through a front end; \
             SIGHUP to reload config, Ctrl-C to stop"
        );
        let (_keep, never) = std::sync::mpsc::channel::<()>();
        let _ = never.recv();
        return Ok(());
    }

    // One session; deltas render live from its event topic.
    let started = host.call_verb(
        &model::START,
        None,
        Payload::of(&start_args(&root, &resume)?)?,
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
        match host.call_verb(&model::SEND, None, args) {
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
        let _ = host.call_verb(&model::END, None, end);
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
    // SIGHUP joins them because it is handled on the same thread, for the
    // same reason: reloading means stopping and starting plugins, which is
    // real work and must not happen inside a signal handler.
    set.add(Signal::SIGHUP);
    set.thread_block()?;
    Ok(set)
}

/// One thread that does nothing but wait for an interrupt. Waiting rather
/// than handling means it may do real work — call a verb, tear plugins down —
/// none of which is safe inside an actual signal handler.
fn spawn_signal_handler(
    signals: SigSet,
    rt: Reloadable,
    busy: Arc<AtomicBool>,
    current: Arc<Mutex<Option<model::SessionId>>>,
) {
    std::thread::spawn(move || {
        loop {
            let Ok(sig) = signals.wait() else {
                return;
            };
            if sig == Signal::SIGHUP {
                rt.reload();
                continue;
            }
            // A turn in flight is cancelled, not killed: an interrupt almost
            // always means "stop this", not "lose the session". Interrupting
            // again, or when idle, brings the runtime down.
            let session = current.lock().unwrap().clone();
            if busy.swap(false, Ordering::SeqCst) {
                if let Some(session) = session {
                    eprintln!("\n[chat] cancelling — interrupt again to quit");
                    if let Ok(p) = Payload::of(&model::CancelArgs { session }) {
                        let _ = rt.host.call_verb(&model::CANCEL, None, p);
                    }
                    continue;
                }
            }
            eprintln!("\n[chat] shutting down");
            rt.host.shutdown_all();
            std::process::exit(130);
        }
    });
}

/// What the signal thread needs in order to reload.
struct Reloadable {
    kernel: Arc<Kernel>,
    host: Arc<Host>,
    root: PathBuf,
    exe: PathBuf,
    state: Arc<Mutex<RuntimeState>>,
}

impl Reloadable {
    fn reload(&self) {
        let mut state = self.state.lock().unwrap();
        eprintln!("[chat] reloading");
        // A bad edit must not leave the runtime emptier than it found it,
        // so a failed start is reported and the rest still converges.
        for f in converge(&self.kernel, &self.host, &self.root, &self.exe, &mut state) {
            eprintln!("[chat] reload: {f}");
        }
    }
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
    let Some(base_url) = std::fs::read_to_string(root.join(model::DIR).join("config.json"))
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
    let listed = std::fs::read_to_string(root.join(egress::DIR).join("config.json"))
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

/// What this process started and granted. Nothing else: a plugin an agent
/// started with `kernel::spawn` is not in here, and a reload leaves it alone.
#[derive(Default)]
struct RuntimeState {
    /// fingerprint → the name the plugin declared. Keyed by fingerprint
    /// rather than by name because a name is something a plugin declares
    /// *after* it starts, while the decision to start it is made before.
    running: BTreeMap<String, PluginName>,
    /// Grants already minted, so a reload does not mint a second copy of
    /// every capability and hand the model a duplicated tool list.
    minted: BTreeSet<String>,
}

/// One plugin the runtime wants running.
struct Desired {
    spec: LaunchSpec,
    /// Files it reads when it starts. Their contents are part of what it
    /// *is* — filling in an API key changes the broker without changing its
    /// command line, and a reload that compared only command lines would
    /// miss the one case it exists for.
    watch: Vec<PathBuf>,
}

impl Desired {
    /// How to name it in a message: by what the spec actually said.
    fn label(&self) -> String {
        match (&self.spec.artifact, &self.spec.bin) {
            (Some(id), _) => id.clone(),
            (_, Some(bin)) => bin.clone(),
            _ => "<no bin or artifact>".to_string(),
        }
    }

    /// What "the same plugin" means here.
    ///
    /// Hashed rather than kept: the secrets file goes through this, and the
    /// CLI has no business holding an API key in memory to compare it later.
    fn fingerprint(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(self.spec.artifact.as_deref().unwrap_or("").as_bytes());
        h.update(b"\0b");
        h.update(self.spec.bin.as_deref().unwrap_or("").as_bytes());
        h.update(b"\0u");
        h.update(self.spec.bundle.as_deref().unwrap_or("").as_bytes());
        // Config is part of what the plugin *is*: change it and the next
        // reload re-plugs this one and nothing else.
        h.update(b"\0n");
        h.update(self.spec.name.as_deref().unwrap_or("").as_bytes());
        h.update(b"\0c");
        h.update(
            self.spec
                .config
                .as_ref()
                .map(|c| c.as_raw())
                .unwrap_or("")
                .as_bytes(),
        );
        for a in &self.spec.args {
            h.update(b"\0a");
            h.update(a.as_bytes());
        }
        for (k, v) in &self.spec.env {
            h.update(b"\0e");
            h.update(k.as_bytes());
            h.update(b"=");
            h.update(v.as_bytes());
        }
        h.update(b"\0f");
        h.update(format!("{:?}", self.spec.form).as_bytes());
        for p in &self.watch {
            h.update(b"\0w");
            h.update(p.as_os_str().as_encoded_bytes());
            // A file that is not there yet is a state like any other: create
            // it later and the fingerprint changes, which is the point.
            if let Ok(bytes) = std::fs::read(p) {
                h.update(&bytes);
            }
        }
        h.finalize().to_hex().to_string()
    }
}

fn load_chat_config(root: &Path) -> ChatConfig {
    match std::fs::read_to_string(root.join("chat.json")) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            eprintln!("[chat] chat.json is not readable, ignoring it: {e}");
            ChatConfig::default()
        }),
        Err(_) => ChatConfig::default(),
    }
}

/// What `chat.json` lists.
///
/// Not in any order: a plugin declares what it needs and the kernel answers
/// it when that arrives, so the order these are started in stops mattering.
/// It used to be a hand-written list with the broker first — and that order
/// was silently thrown away when this began keying by fingerprint, which
/// nothing noticed, because nothing was checking. An ordering nobody can
/// check is an ordering nobody has.
fn configured(root: &Path, exe: &Path) -> Vec<Desired> {
    load_chat_config(root)
        .plugins
        .iter()
        .map(|p| Desired {
            spec: LaunchSpec {
                artifact: p.artifact.clone(),
                bin: p.bin.as_deref().map(|b| resolve_bin(exe, b)),
                name: p.name.clone(),
                bundle: p.bundle.clone(),
                config: p
                    .config
                    .as_ref()
                    .and_then(|c| portos_abi::wire::Payload::of(c).ok()),
                // A bundle brings its own working directory, so its args are
                // already relative to something real and must not be turned
                // into paths on this host.
                args: match p.bundle {
                    Some(_) => p.args.clone(),
                    None => p.args.iter().map(|a| resolve_arg(root, a)).collect(),
                },
                env: p.env.clone(),
                grants: Vec::new(),
                form: p.form,
            },
            watch: p.watch.iter().map(|w| root.join(w)).collect(),
        })
        .collect()
}

/// The two plugins `portos chat` cannot run without, each with the verb that
/// says whether something already provides it.
///
/// Defaults rather than fixtures: one is started only if nothing listed
/// answers for its driver. So replacing the model driver is listing another
/// in `chat.json` — this front end addresses it by verb and has no way to
/// tell whose implementation it got, which is the property that makes any
/// plugin replaceable at all.
fn defaults(root: &Path, exe: &Path) -> Vec<(Verb, Desired)> {
    let env1 =
        |k: &str, v: &Path| BTreeMap::from([(k.to_string(), v.to_string_lossy().into_owned())]);
    let broker_dir = root.join(egress::DIR);
    let model_dir = root.join(model::DIR);
    vec![
        (
            egress::HTTP.clone(),
            Desired {
                spec: LaunchSpec {
                    env: env1(egress::DIR_ENV, &broker_dir),
                    ..LaunchSpec::from_path(resolve_bin(exe, "portos-broker"))
                },
                // The reason reload exists: the API key lands in secrets.json,
                // and only the broker ever reads it.
                watch: vec![
                    broker_dir.join("config.json"),
                    broker_dir.join("secrets.json"),
                ],
            },
        ),
        (
            model::START.clone(),
            Desired {
                spec: LaunchSpec {
                    env: env1(model::DIR_ENV, &model_dir),
                    ..LaunchSpec::from_path(resolve_bin(exe, "portos-modeld"))
                },
                watch: vec![model_dir.join("config.json")],
            },
        ),
    ]
}

/// A `bin` with no directory in it is looked for beside this binary first,
/// which is where the standard plugins are installed, and otherwise left to
/// PATH the way `node` is. One rule for the defaults and for `chat.json`.
fn resolve_bin(exe: &Path, bin: &str) -> String {
    if bin.contains('/') {
        return bin.to_string();
    }
    let sibling = exe.with_file_name(bin);
    if sibling.exists() {
        sibling.to_string_lossy().into_owned()
    } else {
        bin.to_string()
    }
}

/// Start one plugin and remember it under its fingerprint.
fn start(host: &Host, state: &mut RuntimeState, fp: &str, d: &Desired, failures: &mut Vec<String>) {
    match host.spawn_spec(&d.spec) {
        Ok(name) => {
            println!("[chat] started {name}");
            state.running.insert(fp.to_string(), name);
        }
        Err(e) => failures.push(format!("{}: {e}", d.label())),
    }
}

/// Bring the running set in line with the config, and mint anything newly
/// granted. Returns the failures rather than stopping at the first, so one
/// bad entry does not leave the runtime emptier than it found it.
///
/// Reload is nothing more than this function running a second time. There is
/// no per-setting "is this live?" question to answer, because the unit of
/// change is a plugin: `kernel::stop` already collects its whole residue and
/// starting it again puts it back.
fn converge(
    kernel: &Kernel,
    host: &Host,
    root: &Path,
    exe: &Path,
    state: &mut RuntimeState,
) -> Vec<String> {
    let listed: BTreeMap<String, Desired> = configured(root, exe)
        .into_iter()
        .map(|d| (d.fingerprint(), d))
        .collect();
    let standard: Vec<(Verb, String, Desired)> = defaults(root, exe)
        .into_iter()
        .map(|(probe, d)| (probe, d.fingerprint(), d))
        .collect();
    let mut failures = Vec::new();

    // Stopping comes first: a plugin whose config changed comes back under
    // the same name, and the route table refuses a duplicate. A standard
    // plugin keeps its fingerprint across reloads, so it is left alone —
    // which is also the limit: listing a replacement for one that is already
    // running takes a restart, because the standard one keeps running beside
    // it as a second instance, and this REPL refuses to choose between two.
    let stale: Vec<String> = state
        .running
        .keys()
        .filter(|fp| !listed.contains_key(*fp) && !standard.iter().any(|(_, f, _)| f == *fp))
        .cloned()
        .collect();
    for fp in stale {
        if let Some(name) = state.running.remove(&fp) {
            host.shutdown(&name);
            // Stopping revokes what that plugin *held* — a capability belongs
            // to a running plugin, not to a name — so forget having granted
            // it and the loop below will grant it again to the one that comes
            // back. What *others* hold about its driver is untouched and
            // needs no re-minting: it goes inert with the route and means
            // something again when the route returns.
            let subject = name.subject();
            state
                .minted
                .retain(|k| !k.starts_with(&format!("{subject}|")));
            println!("[chat] stopped {name}");
        }
    }
    // What is listed comes first and decides; the standard pair fills in
    // whichever driver is still unanswered. Judged by what is declared rather
    // than by what is routed, because a listed driver waiting on the grant
    // minted below has declared its verbs all the same.
    for (fp, d) in &listed {
        if !state.running.contains_key(fp) {
            start(host, state, fp, d, &mut failures);
        }
    }
    for (probe, fp, d) in &standard {
        if !state.running.contains_key(fp) && host.answerers_of(probe).is_empty() {
            start(host, state, fp, d, &mut failures);
        }
    }

    // Grants. Every model driver — whichever, and however many — gets
    // egress: its LLM calls go through the broker, and it holds no key and no
    // network of its own.
    let model_drivers = host.answerers_of(&model::START);
    if model_drivers.is_empty() {
        return failures;
    }
    let mut mint = |subject: String, resource: &str, verbs: BTreeSet<String>| {
        let key = format!(
            "{subject}|{resource}|{}",
            verbs.iter().cloned().collect::<Vec<_>>().join(",")
        );
        if !state.minted.insert(key) {
            return;
        }
        match kernel
            .caps
            .mint(&subject, resource, verbs, Default::default(), None)
        {
            Ok(_) => println!("[chat] grant: {subject} → {resource}"),
            Err(e) => eprintln!("[chat] grant {subject} → {resource} failed: {e}"),
        }
    };
    for driver in &model_drivers {
        mint(
            driver.subject(),
            "driver:egress",
            BTreeSet::from([
                egress::HTTP.short().to_string(),
                egress::HTTP_STREAM.short().to_string(),
            ]),
        );
    }
    for g in &load_chat_config(root).grants {
        let subjects: Vec<String> = match &g.subject {
            Some(s) => vec![s.clone()],
            None => model_drivers.iter().map(|d| d.subject()).collect(),
        };
        for subject in subjects {
            mint(subject, &g.resource, g.verbs.clone());
        }
    }
    // Grants are half of whether a plugin can work, and they land after the
    // plugins do; without this a driver that was only ever waiting on a
    // permission would wait forever.
    host.refresh();

    // Reported once everything is up and granted, because until then
    // "waiting" is just "not yet" and saying so would be noise.
    for (name, _, unmet) in host.plugins() {
        if !unmet.is_empty() {
            let what: Vec<String> = unmet.iter().map(|v| v.to_string()).collect();
            println!(
                "[chat] {name} is not answering — it needs {}",
                what.join(", ")
            );
        }
    }
    failures
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
    let broker = root.join(egress::DIR);
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
    let modeld = root.join(model::DIR);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(watch: Vec<PathBuf>) -> Desired {
        Desired {
            spec: LaunchSpec::from_path("/bin/true"),
            watch,
        }
    }

    /// The whole reason reload keys on a fingerprint rather than a command
    /// line: filling in an API key changes nothing a process listing would
    /// show, and it is the change that matters most.
    #[test]
    fn a_watched_files_contents_are_part_of_what_the_plugin_is() {
        let dir = std::env::temp_dir().join(format!("portos-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let secrets = dir.join("secrets.json");

        // Absent is a state like any other.
        let missing = desired(vec![secrets.clone()]).fingerprint();

        std::fs::write(&secrets, r#"{"key":""}"#).unwrap();
        let empty = desired(vec![secrets.clone()]).fingerprint();
        assert_ne!(missing, empty, "the file appearing is a change");

        std::fs::write(&secrets, r#"{"key":"filled-in"}"#).unwrap();
        let filled = desired(vec![secrets.clone()]).fingerprint();
        assert_ne!(empty, filled, "filling in the key must re-plug the broker");

        assert_eq!(
            filled,
            desired(vec![secrets.clone()]).fingerprint(),
            "and an unchanged plugin must be left alone"
        );

        // A plugin that watches nothing is decided by its spec alone.
        assert_eq!(desired(vec![]).fingerprint(), desired(vec![]).fingerprint());
        assert_ne!(desired(vec![]).fingerprint(), filled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two plugins differing only in one environment variable are two
    /// plugins — which is what makes editing `chat.json` re-plug just the
    /// driver that changed.
    #[test]
    fn the_spec_decides_identity_when_nothing_is_watched() {
        let base = desired(vec![]);
        let mut other = desired(vec![]);
        other
            .spec
            .env
            .insert("PORTOS_FS_ROOT".into(), "/srv".into());
        assert_ne!(base.fingerprint(), other.fingerprint());
    }
}
