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
use portos_egress_api as egress;
use portos_kernel::Kernel;
use portos_kernel::host::{Form, Host, LaunchSpec};
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
    /// An executable stored in the CAS — `portos put <root> <file>` gives
    /// you the id. A plugin named this way is a thing rather than a
    /// location.
    #[serde(default)]
    artifact: Option<String>,
    /// A path on this host. Fine for `node` and other things already
    /// installed; not reproducible and not portable.
    #[serde(default)]
    bin: Option<String>,
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
    let index = model::SessionIndex::read(&PathBuf::from(root).join("modeld"));
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
    let index = || model::SessionIndex::read(&root.join("modeld"));
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

    let root = PathBuf::from(root);
    let kernel = Arc::new(Kernel::open(&root)?);
    let host = Arc::new(Host::new(kernel.clone(), &root.join("sock"))?);
    host.audit_topic(&egress::LOG);

    write_templates(&root)?;

    let exe = std::env::current_exe()?;
    warn_if_provider_host_unlisted(&root);

    // What this process started, and what it has already granted. Startup and
    // reload run the same code over it; at startup everything is new.
    let state = Arc::new(Mutex::new(RuntimeState::default()));
    let failures = converge(&kernel, &host, &root, &exe, &mut state.lock().unwrap())?;
    if let Some(first) = failures.first() {
        return Err(first.clone().into());
    }
    let modeld = state
        .lock()
        .unwrap()
        .modeld()
        .ok_or("the model driver did not start")?;

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
    let started = host.call(
        &modeld,
        &model::START,
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
    modeld: PluginName,
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
                        let _ = rt.host.call(&modeld, &model::CANCEL, p);
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
        match converge(&self.kernel, &self.host, &self.root, &self.exe, &mut state) {
            // A bad edit must not leave the runtime emptier than it found it,
            // so a failed start is reported and the rest still converges.
            Ok(failures) => {
                for f in failures {
                    eprintln!("[chat] reload: {f}");
                }
            }
            Err(e) => eprintln!("[chat] reload failed: {e}"),
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

impl RuntimeState {
    fn modeld(&self) -> Option<PluginName> {
        self.running
            .values()
            .find(|n| n.as_str() == "portos-modeld")
            .cloned()
    }
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

/// Everything that should be running, in start order: the two trusted
/// built-ins, then whatever `chat.json` lists.
fn desired_set(root: &Path, exe: &Path) -> Result<Vec<Desired>, String> {
    let sibling = |name: &str| -> Result<String, String> {
        let p = exe.with_file_name(name);
        if p.exists() {
            Ok(p.to_string_lossy().into_owned())
        } else {
            Err(format!("missing sibling binary: {}", p.display()))
        }
    };
    let env1 =
        |k: &str, v: &Path| BTreeMap::from([(k.to_string(), v.to_string_lossy().into_owned())]);

    let broker_dir = root.join("broker");
    let modeld_dir = root.join("modeld");
    let mut want = vec![
        Desired {
            spec: LaunchSpec {
                env: env1("PORTOS_BROKER_DIR", &broker_dir),
                ..LaunchSpec::from_path(sibling("portos-broker")?)
            },
            // The reason reload exists: the API key lands in secrets.json,
            // and only the broker ever reads it.
            watch: vec![
                broker_dir.join("config.json"),
                broker_dir.join("secrets.json"),
            ],
        },
        Desired {
            spec: LaunchSpec {
                env: env1("PORTOS_MODELD_DIR", &modeld_dir),
                ..LaunchSpec::from_path(sibling("portos-modeld")?)
            },
            watch: vec![modeld_dir.join("config.json")],
        },
    ];
    for p in &load_chat_config(root).plugins {
        want.push(Desired {
            spec: LaunchSpec {
                artifact: p.artifact.clone(),
                bin: p.bin.clone(),
                args: p.args.iter().map(|a| resolve_arg(root, a)).collect(),
                env: p.env.clone(),
                grants: Vec::new(),
                form: p.form,
            },
            watch: p.watch.iter().map(|w| root.join(w)).collect(),
        });
    }
    Ok(want)
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
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let wanted: BTreeMap<String, Desired> = desired_set(root, exe)?
        .into_iter()
        .map(|d| (d.fingerprint(), d))
        .collect();
    let mut failures = Vec::new();

    // Stopping comes first: a plugin whose config changed comes back under
    // the same name, and the route table refuses a duplicate.
    let stale: Vec<String> = state
        .running
        .keys()
        .filter(|fp| !wanted.contains_key(*fp))
        .cloned()
        .collect();
    for fp in stale {
        if let Some(name) = state.running.remove(&fp) {
            host.shutdown(&name);
            // Stopping revokes what that plugin *held* — a capability belongs
            // to a running plugin, not to a name — so forget having granted
            // it and the loop below will grant it again to the one that comes
            // back. What *others* hold about its family is untouched and
            // needs no re-minting: it goes inert with the route and means
            // something again when the route returns.
            let subject = name.subject();
            state
                .minted
                .retain(|k| !k.starts_with(&format!("{subject}|")));
            println!("[chat] stopped {name}");
        }
    }
    for (fp, d) in &wanted {
        if state.running.contains_key(fp) {
            continue;
        }
        match host.spawn_spec(&d.spec) {
            Ok(name) => {
                println!("[chat] started {name}");
                state.running.insert(fp.clone(), name);
            }
            Err(e) => failures.push(format!("{}: {e}", d.label())),
        }
    }

    // Grants. The model driver always gets egress: its LLM calls go through
    // the broker, and it holds no key and no network of its own.
    let Some(modeld) = state.modeld() else {
        return Ok(failures);
    };
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
    mint(
        modeld.subject(),
        "driver:egress",
        BTreeSet::from([
            egress::HTTP.short().to_string(),
            egress::HTTP_STREAM.short().to_string(),
        ]),
    );
    for g in &load_chat_config(root).grants {
        let subject = g.subject.clone().unwrap_or_else(|| modeld.subject());
        mint(subject, &g.resource, g.verbs.clone());
    }
    Ok(failures)
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
