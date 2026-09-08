//! Plugin host — ABI v2 (decisions-v1.md D23–D26, D29).
//!
//! A plugin is a plain child process (M0: no sandbox yet) that connects back
//! to a per-spawn UDS **twice**, authenticating both connections with a
//! spawn token from the environment:
//!
//!   - the **serve** channel: kernel→plugin `call` requests and `shutdown`.
//!     The plugin declares its verb list (and which extra channels it will
//!     open) in the serve hello; the kernel registers those verbs in its
//!     route table.
//!   - the **client** channel: plugin→kernel requests — `invoke` (call
//!     another plugin's verb through the kernel: capability-checked against
//!     the calling plugin, audited, routed), `grants` (introspect what this
//!     plugin may invoke, joined with the verbs' advertised metadata),
//!     `emit`/`subscribe`/`unsubscribe` (event bus; topic patterns may end
//!     in `*` for prefix matching), `spawn_child` (parent/child plugin
//!     instantiation, gated on the caller's `kernel:spawn` capability; the
//!     child's holding is parented under the caller's), and `put`/`read`
//!     (artifact dereference as chunked byte streams; see
//!     `portos_proto::chunk`). fd passing is gone (D25).
//!   - an optional **events** channel: one-way kernel→plugin event
//!     deliveries. A plugin that declares it can receive subscribed events
//!     *while one of its own verbs is mid-call* — the serve channel is busy
//!     then, and without a separate channel a plugin awaiting an event
//!     stream inside a call handler would deadlock (the model driver's SSE
//!     consumption is exactly that shape). Plugins that don't declare it get
//!     events interleaved on the serve channel as before.
//!
//! Each channel is strict in a single direction, which keeps the M0
//! sync-thread model trivial: no frame multiplexing anywhere.
//!
//! Verbs are `family::verb` strings (D29). They are opaque to this module —
//! the kernel routes text, it never interprets domain meaning (the
//! domain-ignorance invariant). The capability convention for invoke is
//! subject `plugin:<name>`, resource `driver:<family>`, verbs = short names.
//!
//! Known M0 limitation: the invoke graph must be acyclic. A cycle (A invokes
//! B while B's serve loop is blocked invoking A) deadlocks; v0 flows
//! (cli → model driver → {broker, browser}) are acyclic by construction, and
//! the effect-plan world later makes call structure explicit.
//!
//! ## Resource-management laws wired in (spec F1–F6 via `portos-rm`)
//!
//!   - **Holdings (F1/F2).** A spawned plugin is an exclusive holding of class
//!     `kernel/plugin` (generation = spawn token); each of its subscriptions
//!     is a `kernel/subscription` holding under it. Reclamation is
//!     crash-only: plugin death, event-queue overflow and graceful shutdown
//!     all run the same F2 teardown of the plugin's ownership closure,
//!     children first, through a `World` that drops subscriptions, routes and
//!     the process. Graceful shutdown only adds a polite `shutdown` frame in
//!     front of it.
//!   - **Verb character (F4).** A hello's `tools[verb]` may declare `kind`
//!     (`repeatable` | `repeatable_shared` | `transforming` | `consuming` |
//!     `emitting`, with `world`, `compensate_with`, `amortizable`,
//!     `idempotent`, `commutes`, `degrade`), and the hello may declare the
//!     plugin's `holding_rho`. The kernel builds the plugin's truth table and
//!     refuses the spawn if it is incoherent. `grants` exposes `kind` and
//!     `budgeted` so callers (the model driver) can tell reads from effects.
//!   - **Slot row (F5).** `spawn_in` takes a slot: its `offers` are the
//!     position ceiling; every verb's declared `requires.caps` must fit at
//!     spawn (`admit_mount`), and an `invoke` outside the row is refused
//!     regardless of capabilities (actual = row ∩ grant).
//!   - **Protocol (F6).** A hello may declare a deterministic safety
//!     automaton over its verbs; the kernel enforces it precisely by refusing
//!     the offending call (truncation tier), state advancing only on success.
//!
//! Lock order: ledger → plugins/routes/subs. Teardown holds the ledger while
//! its `World` takes host locks; no host path takes a host lock and then the
//! ledger.

use crate::ledger::ExclusiveRequest;
use crate::ledger::{
    CLASS_FILE_LOCK, CLASS_PLUGIN, CLASS_PORT, CLASS_PROCESS, CLASS_SUBSCRIPTION, kill_pid,
    proc_alive, proc_start_time,
};
use crate::{Kernel, KernelError};
use portos_proto::resource::{
    HoldRequest, HoldingRef, ReleaseRequest, ReleaseResponse, RenewRequest, RenewResponse,
};
use portos_proto::{Label, chunk, frame};
use portos_rm::coeffect::{Flat, Manifest, Mount, Requires, admit_mount};
use portos_rm::identity::{
    AccountId, ClassId, Generation, HoldingHandle, HoldingId, InstanceId, ResourceKey, SubjectId,
};
use portos_rm::ledger::{LiveItem, RevertGrade};
use portos_rm::protocol::Protocol;
use portos_rm::teardown::{RunOutcome, World};
use portos_rm::time::{LeaseDuration, LeaseRequest, Timestamp};
use portos_rm::verbs::{ConsumeGrade, EmitGrade, Kind, VerbEntry, VerbTable};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

/// Bounded event queue per subscriber. A subscriber that falls this far
/// behind is cut off (m0-kernel-v0.md §3: slow consumers must not stall the
/// kernel): plugin subscribers are disconnected, local subscribers dropped.
pub const EVENT_QUEUE: usize = 256;

const SPAWN_DEADLINE_MS: u64 = 10_000;

struct PluginHandle {
    child: Mutex<std::process::Child>,
    serve: Mutex<UnixStream>,
    /// Dedicated event-delivery stream, when the plugin declared one.
    /// Without it, events interleave on the serve channel.
    events: Option<Mutex<UnixStream>>,
    events_tx: SyncSender<Value>,
    sock_path: PathBuf,
    /// The plugin's `kernel/plugin` holding (F1) and its generation (the
    /// spawn token): the stable denotation of this incarnation.
    holding: HoldingHandle,
    generation: String,
    pid: u32,
    /// Position ceiling (F5 effect row) when spawned into a slot: the verbs
    /// this plugin may invoke, before any capability is consulted.
    offers: Option<Flat>,
    /// Declared verb-order protocol (F6) and its current state.
    protocol: Option<Protocol>,
    proto_state: Mutex<Option<String>>,
    /// Set by a graceful shutdown before the polite frame, so the exit path
    /// that races it reports the true reason.
    shutting_down: std::sync::atomic::AtomicBool,
}

/// A slot a plugin is spawned into (F5 `Mount`): `offers` is the row — the
/// verbs a plugin here may invoke at most; `provides` names services (verb
/// families) the slot guarantees present besides those already routed.
#[derive(Clone, Debug, Default)]
pub struct Slot {
    pub offers: Vec<String>,
    pub provides: Vec<String>,
}

enum SubTarget {
    Local(SyncSender<Value>),
    Plugin(String),
}

struct Sub {
    id: u64,
    topic: String,
    target: SubTarget,
    /// The `kernel/subscription` holding backing this subscription.
    holding: HoldingHandle,
}

/// A routed verb: which plugin serves it, plus the model-facing metadata the
/// plugin advertised in its hello (opaque to the kernel — stored and joined,
/// never interpreted), and the verb character it declared (checked by the
/// F4 truth table at spawn).
struct RouteEntry {
    plugin: String,
    description: String,
    schema: Value,
    kind: Option<&'static str>,
    budgeted: Option<bool>,
    /// F3 hard list: emitting ∧ non-amortizable — plan runs withhold it.
    withhold: bool,
    /// Sink-target extraction (WP-06): which arg is the target, and how to
    /// read it ("origin" normalizes to scheme://host[:port]).
    target: Option<TargetSpec>,
}

struct TargetSpec {
    arg: String,
    kind: String,
}

pub(crate) struct HostInner {
    plugins: Mutex<BTreeMap<String, Arc<PluginHandle>>>,
    routes: Mutex<BTreeMap<String, RouteEntry>>, // verb -> route
    subs: Mutex<Vec<Sub>>,
    next_sub: AtomicU64,
    next_spawn: AtomicU64,
    sock_dir: PathBuf,
    meter: Mutex<crate::metrics::ContextMeter>,
    /// Lease sweeper (WP-02): stop flag and thread handle; stopped on drop.
    sweeper_stop: Arc<AtomicBool>,
    sweeper_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// The plugin host: spawn, route, event bus, artifact channel.
pub struct Host {
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
    /// Plan runs (WP-06): admission → consent → run under the F3 monitor.
    pub plans: Arc<crate::plans::PlanService>,
}

impl Host {
    pub fn new(kernel: Arc<Kernel>, sock_dir: &Path) -> Result<Host, KernelError> {
        std::fs::create_dir_all(sock_dir)?;
        let inner = Arc::new(HostInner {
            plugins: Mutex::new(BTreeMap::new()),
            routes: Mutex::new(BTreeMap::new()),
            subs: Mutex::new(Vec::new()),
            next_sub: AtomicU64::new(1),
            next_spawn: AtomicU64::new(1),
            sock_dir: sock_dir.to_path_buf(),
            meter: Mutex::new(crate::metrics::ContextMeter::default()),
            sweeper_stop: Arc::new(AtomicBool::new(false)),
            sweeper_handle: Mutex::new(None),
        });
        let plans = crate::plans::PlanService::new(kernel.clone(), inner.clone());
        Ok(Host {
            kernel,
            inner,
            plans,
        })
    }

    /// Spawn `bin args…` with `envs` added, wait for both hellos, register
    /// the plugin's verbs, and start its service threads. Returns the plugin
    /// name (from its hello). No slot: no position ceiling.
    pub fn spawn(
        &self,
        bin: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<String, KernelError> {
        self.spawn_in(bin, args, envs, None)
    }

    /// [`spawn`](Self::spawn) into a slot (F5): the plugin's declared
    /// `requires` must fit the slot's row at the door, and later invokes are
    /// bounded by it.
    pub fn spawn_in(
        &self,
        bin: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
        slot: Option<&Slot>,
    ) -> Result<String, KernelError> {
        spawn_plugin(
            &self.kernel,
            &self.inner,
            &self.plans,
            bin,
            args,
            envs,
            slot,
            None,
        )
    }
}

/// Spawn a plugin process: `bin args…` with `envs` added, wait for the
/// hellos, register the plugin's verbs, and start its service threads.
/// Returns the plugin name (from its hello).
///
/// With `parent = Some((parent_name, parent_holding))` the new plugin's
/// `kernel/plugin` holding becomes a child of the parent's holding (decision
/// 2: the instantiation edge is declared here, at spawn), so reclaiming the
/// parent tears the child down first (F2 closure).
#[allow(clippy::too_many_arguments)]
fn spawn_plugin(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    plans: &Arc<crate::plans::PlanService>,
    bin: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    slot: Option<&Slot>,
    parent: Option<(&str, HoldingHandle)>,
) -> Result<String, KernelError> {
    let idx = inner.next_spawn.fetch_add(1, Ordering::SeqCst);
    let sock_path = inner
        .sock_dir
        .join(format!("plugin-{}-{idx}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;
    listener.set_nonblocking(true)?;
    let token = rand_token();

    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .env("PORTOS_PLUGIN_SOCK", &sock_path)
        .env("PORTOS_PLUGIN_TOKEN", &token);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;

    // The serve connection comes first and declares which extra channels
    // follow ("client" always; "events" optionally). Bad token or an
    // undeclared/duplicate role is fatal for the spawn.
    let accept_hello =
        |child: &mut std::process::Child| -> Result<(UnixStream, Value), KernelError> {
            let mut stream = accept_with_deadline(&listener, child, SPAWN_DEADLINE_MS)?;
            let hello = frame::read_frame(&mut stream)
                .map_err(|e| KernelError::Corrupt(format!("hello: {e}")))?;
            if hello["hello"]["token"].as_str() != Some(token.as_str()) {
                let _ = frame::write_frame(&mut stream, &json!({"err": "bad token"}));
                return Err(KernelError::Denied("plugin hello: bad token".into()));
            }
            frame::write_frame(&mut stream, &json!({"ok": {}}))
                .map_err(|e| KernelError::Corrupt(format!("hello ack: {e}")))?;
            Ok((stream, hello["hello"].clone()))
        };

    let (serve_stream, hello) = match accept_hello(&mut child) {
        Ok((stream, h)) if h["role"] == "serve" => (stream, h),
        Ok(_) => {
            let _ = child.kill();
            return Err(KernelError::Corrupt(
                "plugin hello: first connection must be role serve".into(),
            ));
        }
        Err(e) => {
            let _ = child.kill();
            return Err(e);
        }
    };
    let name = hello["name"].as_str().unwrap_or("?").to_string();
    let verbs: Vec<String> = str_array(&hello["verbs"]);
    // Optional per-verb tool metadata (description + schema + kind +
    // requires): joined into `grants`, checked by the F4/F5 laws below.
    let tools_meta = hello["tools"].clone();
    let mut expected: Vec<String> = hello["channels"]
        .as_array()
        .map(|_| str_array(&hello["channels"]))
        .unwrap_or_else(|| vec!["client".to_string()]);
    if !expected.iter().any(|c| c == "client") {
        let _ = child.kill();
        return Err(KernelError::Corrupt(
            "plugin hello: a client channel is required".into(),
        ));
    }
    let mut client: Option<UnixStream> = None;
    let mut events: Option<UnixStream> = None;
    while !expected.is_empty() {
        let (stream, h) = match accept_hello(&mut child) {
            Ok(x) => x,
            Err(e) => {
                let _ = child.kill();
                return Err(e);
            }
        };
        let role = h["role"].as_str().unwrap_or("?").to_string();
        match expected.iter().position(|c| *c == role) {
            Some(i) => {
                expected.remove(i);
                match role.as_str() {
                    "client" => client = Some(stream),
                    "events" => events = Some(stream),
                    _ => {
                        let _ = child.kill();
                        return Err(KernelError::Corrupt(format!(
                            "plugin hello: unknown channel role {role}"
                        )));
                    }
                }
            }
            None => {
                let _ = child.kill();
                return Err(KernelError::Corrupt(format!(
                    "plugin hello: undeclared or duplicate role {role}"
                )));
            }
        }
    }
    let client_stream = client.expect("client channel present");

    // ---- F4: the verb character the plugin declared, checked at the door.
    let table = match build_verb_table(&name, &verbs, &tools_meta, &hello) {
        Ok(t) => t,
        Err(e) => {
            let _ = child.kill();
            return Err(KernelError::Denied(format!(
                "plugin {name}: verb metadata rejected: {e}"
            )));
        }
    };
    let protocol = table.derive_handler_policy(&name).protocol;

    // ---- F5: slot admission — every verb's requires must fit the row.
    if let Some(slot) = slot {
        let manifest = manifest_from_meta(&name, &verbs, &tools_meta);
        let mut provides: Vec<String> = slot.provides.clone();
        provides.extend(routed_families(inner));
        let mount = Mount {
            name: "slot".into(),
            offers: Flat(slot.offers.iter().cloned().collect()),
            provides: Flat(provides.into_iter().collect()),
        };
        if let Err(e) = admit_mount(&manifest, &mount) {
            let _ = child.kill();
            return Err(KernelError::Denied(format!(
                "plugin {name}: slot admission failed: {e:?}"
            )));
        }
    }
    let offers: Option<Flat> = slot.map(|s| Flat(s.offers.iter().cloned().collect()));

    // ---- F1: the instance is a holding. Ledger first, host locks after.
    let now = crate::db::now_unix();
    let subject = format!("plugin:{name}");
    let holding = match &parent {
        Some((parent_name, parent_holding)) => kernel
            .ledger
            .hold_exclusive_child(
                ExclusiveRequest {
                    owner: SubjectId::new(&subject),
                    resource: ResourceKey::new(ClassId::new(CLASS_PLUGIN), InstanceId::new(&name)),
                    generation: Generation::new(&token),
                    parent: Some(parent_holding.clone()),
                    lease: LeaseRequest::UseClassDefault,
                },
                SubjectId::new(&format!("plugin:{parent_name}")),
                Timestamp::try_from(now).map_err(crate::ledger::map_err)?,
            )
            .map_err(|e| {
                let _ = child.kill();
                KernelError::Denied(format!("plugin {name}: spawn under parent refused: {e}"))
            })?,
        None => match kernel.ledger.hold_exclusive(
            ExclusiveRequest {
                owner: SubjectId::new(&subject),
                resource: ResourceKey::new(ClassId::new(CLASS_PLUGIN), InstanceId::new(&name)),
                generation: Generation::new(&token),
                parent: None,
                lease: LeaseRequest::UseClassDefault,
            },
            Timestamp::try_from(now).map_err(crate::ledger::map_err)?,
        ) {
            Ok(id) => id,
            Err(_) => {
                let _ = child.kill();
                return Err(KernelError::Denied(format!("plugin name taken: {name}")));
            }
        },
    };
    let pid = child.id();

    // Register verbs; a route conflict aborts the spawn (and releases the
    // holding again — nothing in the ledger outlives a failed spawn).
    {
        let mut plugins = inner.plugins.lock().unwrap();
        let mut routes = inner.routes.lock().unwrap();
        if let Some(v) = verbs.iter().find(|v| routes.contains_key(*v)) {
            let _ = child.kill();
            let _ = kernel.ledger.release(
                &holding,
                Timestamp::try_from(now).expect("system timestamp in range"),
            );
            return Err(KernelError::Denied(format!("verb already routed: {v}")));
        }
        let (events_tx, events_rx) = sync_channel::<Value>(EVENT_QUEUE);
        let handle = Arc::new(PluginHandle {
            child: Mutex::new(child),
            serve: Mutex::new(serve_stream),
            events: events.map(Mutex::new),
            events_tx,
            sock_path: sock_path.clone(),
            holding: holding.clone(),
            generation: token.clone(),
            pid,
            offers,
            protocol,
            proto_state: Mutex::new(None),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
        });
        for v in &verbs {
            let meta = &tools_meta[v.as_str()];
            let entry = table.lookup(&name, v).ok();
            let target = meta.get("target").and_then(|t| {
                Some(TargetSpec {
                    arg: t["arg"].as_str()?.to_string(),
                    kind: t["kind"].as_str().unwrap_or("literal").to_string(),
                })
            });
            routes.insert(
                v.clone(),
                RouteEntry {
                    plugin: name.clone(),
                    description: meta["description"].as_str().unwrap_or("").to_string(),
                    schema: if meta["schema"].is_object() {
                        meta["schema"].clone()
                    } else {
                        json!({"type": "object"})
                    },
                    kind: entry.map(kind_label),
                    budgeted: entry.map(|e| e.bears_budget()),
                    withhold: entry.map(kind_label) == Some("emitting")
                        && meta.get("amortizable").and_then(|a| a.as_bool()) == Some(false),
                    target,
                },
            );
        }
        plugins.insert(name.clone(), handle.clone());
        spawn_event_pump(handle.clone(), events_rx);
        spawn_client_loop(
            kernel.clone(),
            inner.clone(),
            plans.clone(),
            name.clone(),
            client_stream,
        );
    }

    let _ = kernel.audit.lock().unwrap().append(json!({
        "event": "plugin.spawned", "plugin": name, "verbs": verbs,
        "holding": holding.id().get(), "pid": pid, "parent": parent.as_ref().map(|(p, _)| p),
    }));
    Ok(name)
}

/// Verb families currently routed (the services present for F5 `deps`).
fn routed_families(inner: &HostInner) -> Vec<String> {
    let routes = inner.routes.lock().unwrap();
    let mut fams: Vec<String> = routes
        .keys()
        .map(|v| v.split("::").next().unwrap_or(v).to_string())
        .collect();
    fams.sort();
    fams.dedup();
    fams
}

impl Host {
    /// OS pid of a running plugin (tests kill plugins with it).
    pub fn pid(&self, plugin: &str) -> Option<u32> {
        self.inner
            .plugins
            .lock()
            .unwrap()
            .get(plugin)
            .map(|h| h.pid)
    }

    /// The plugin serving a verb, if any — used by the CLI to find a driver
    /// by its family rather than by name (stack entries are data).
    pub fn verb_provider(&self, verb: &str) -> Option<String> {
        self.inner
            .routes
            .lock()
            .unwrap()
            .get(verb)
            .map(|e| e.plugin.clone())
    }

    /// Crash-only reclamation of a plugin's holdings (children first) with
    /// the physical inverses — the one teardown path. Returns rows released.
    pub fn reclaim(&self, plugin: &str, reason: &str) -> usize {
        reclaim(&self.kernel, &self.inner, plugin, reason)
    }

    /// Revoke a capability (WP-03): the derivation-tree cascade, each revoked
    /// cap's holding subtree torn down through the one teardown path. Returns
    /// how many caps the cascade newly revoked.
    pub fn revoke_capability(&self, cap_id: &str) -> Result<u64, KernelError> {
        let now = crate::db::now_unix();
        let mut world = HostWorld {
            inner: self.inner.clone(),
        };
        let n = self.kernel.caps.revoke(cap_id, &mut world, now)?;
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "cap.revoked", "cap": cap_id, "cascade": n,
        }));
        Ok(n)
    }

    /// Kernel-initiated verb call on a named plugin (no capability check:
    /// kernel-side callers act with root authority; user-session grants come
    /// later via consent).
    pub fn call(&self, plugin: &str, verb: &str, args: Value) -> Result<Value, KernelError> {
        let handle = self
            .inner
            .plugins
            .lock()
            .unwrap()
            .get(plugin)
            .cloned()
            .ok_or_else(|| KernelError::NotFound(format!("plugin: {plugin}")))?;
        call_on(&handle, verb, args)
    }

    /// Kernel-initiated call routed by verb.
    pub fn call_verb(&self, verb: &str, args: Value) -> Result<Value, KernelError> {
        let target = self
            .inner
            .routes
            .lock()
            .unwrap()
            .get(verb)
            .map(|e| e.plugin.clone())
            .ok_or_else(|| KernelError::NotFound(format!("no route for verb: {verb}")))?;
        self.call(&target, verb, args)
    }

    /// Subscribe an in-process consumer (the CLI / model-facing loop) to a
    /// topic (exact or trailing-`*` prefix pattern). The subscription is a
    /// `kernel/subscription` holding of the kernel itself.
    pub fn subscribe_local(&self, topic: &str) -> Result<(u64, Receiver<Value>), KernelError> {
        subscribe_with_subject(&self.kernel, &self.inner, "kernel", topic)
    }

    /// Subscribe an arbitrary kernel-side subject (e.g. a plan run's
    /// segment) to a topic: the holding lands under that subject.
    pub fn subscribe_for(
        &self,
        subject: &str,
        topic: &str,
    ) -> Result<(u64, Receiver<Value>), KernelError> {
        subscribe_with_subject(&self.kernel, &self.inner, subject, topic)
    }

    /// Drop one of the kernel's own local subscriptions. Mirror of the client
    /// `unsubscribe` op: find under the subs lock, release its holding in the
    /// ledger without it (lock order: ledger → host), then remove.
    pub fn unsubscribe_local(&self, sub_id: u64) -> bool {
        let holding = {
            let subs = self.inner.subs.lock().unwrap();
            subs.iter()
                .find(|s| s.id == sub_id && matches!(s.target, SubTarget::Local(_)))
                .map(|s| s.holding.clone())
        };
        match holding {
            Some(h) => {
                let _ = self.kernel.ledger.release(
                    &h,
                    Timestamp::try_from(crate::db::now_unix()).expect("system timestamp in range"),
                );
                self.inner.subs.lock().unwrap().retain(|s| s.id != sub_id);
                true
            }
            None => false,
        }
    }

    /// Kernel-side event publish. Returns the number of subscribers the event
    /// was delivered to.
    pub fn emit(&self, topic: &str, data: Value) -> usize {
        dispatch_event(&self.kernel, &self.inner, topic, data)
    }

    /// Persist every event on `topic` to the audit chain. This is how a
    /// trusted plugin's self-reported log (e.g. the egress broker's
    /// `egress::log`) becomes tamper-evident: the plugin emits, the kernel
    /// subscribes and appends.
    pub fn audit_topic(&self, topic: &str) -> Result<(), KernelError> {
        let (_id, rx) = self.subscribe_local(topic)?;
        let kernel = self.kernel.clone();
        std::thread::spawn(move || {
            for ev in rx {
                let _ = kernel.audit.lock().unwrap().append(json!({
                    "event": "topic.audit",
                    "topic": ev["topic"],
                    "data": ev["data"],
                }));
            }
        });
        Ok(())
    }

    /// (context_bytes, data_bytes) moved through plugin channels so far.
    pub fn meter(&self) -> (u64, u64) {
        let m = self.inner.meter.lock().unwrap();
        (m.context_bytes, m.data_bytes)
    }

    /// Graceful shutdown = the crash-only path triggered early (endstate
    /// §5.3): a polite `shutdown` frame and a moment to exit, then the same
    /// reclamation any death gets.
    pub fn shutdown(&self, plugin: &str) {
        let handle = { self.inner.plugins.lock().unwrap().get(plugin).cloned() };
        if let Some(h) = handle {
            h.shutting_down.store(true, Ordering::SeqCst);
            if let Ok(mut s) = h.serve.lock() {
                let _ = frame::write_frame(&mut *s, &json!({"op": "shutdown"}));
            }
            for _ in 0..20 {
                if matches!(h.child.lock().unwrap().try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        reclaim(&self.kernel, &self.inner, plugin, "shutdown");
    }

    pub fn shutdown_all(&self) {
        let names: Vec<String> = self.inner.plugins.lock().unwrap().keys().cloned().collect();
        for n in names {
            self.shutdown(&n);
        }
    }

    /// Start the lease sweeper (WP-02): every `interval`, due holdings expire
    /// through `Ledger::sweep` (children first; a parent waits on a child
    /// holding its own live lease) and the world side of each release runs
    /// through `HostWorld`. Quiet ticks audit nothing; a tick that released
    /// or failed anything audits `ledger.swept`. The thread stops on drop.
    pub fn start_sweeper(&self, interval: std::time::Duration) {
        let kernel = self.kernel.clone();
        let inner = self.inner.clone();
        let plans = self.plans.clone();
        let stop = inner.sweeper_stop.clone();
        let handle = std::thread::spawn(move || {
            loop {
                // Sleep in slices so Host::drop stops the thread promptly.
                let deadline = std::time::Instant::now() + interval;
                while std::time::Instant::now() < deadline {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let now = crate::db::now_unix();
                let mut world = HostWorld {
                    inner: inner.clone(),
                };
                match kernel.ledger.sweep_with_world(
                    &mut world,
                    Timestamp::try_from(now).expect("system timestamp in range"),
                ) {
                    Ok(report) if !report.released.is_empty() || !report.failed.is_empty() => {
                        let mut classes: BTreeMap<String, usize> = BTreeMap::new();
                        for (_, class, _) in &report.released {
                            *classes.entry(class.clone()).or_insert(0) += 1;
                        }
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "ledger.swept",
                            "released": report.released.len(),
                            "classes": classes,
                            "failed": report.failed,
                        }));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "ledger.sweep_error", "error": e.to_string(),
                        }));
                    }
                }
                // [TTL] suspended plan runs expire on the same tick (WP-06).
                plans.expire(now);
            }
        });
        *self.inner.sweeper_handle.lock().unwrap() = Some(handle);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.inner.sweeper_stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.inner.sweeper_handle.lock().unwrap().take() {
            let _ = h.join();
        }
        self.plans.shutdown();
        self.shutdown_all();
    }
}

/// One serve-channel request/response under the channel lock. Holding the
/// lock across write+read is what keeps the channel unmultiplexed — and it
/// is also what makes the F6 protocol check below precise: calls to one
/// plugin are serialized, so the automaton steps in call order, advancing
/// only when the call succeeded.
fn call_on(handle: &PluginHandle, verb: &str, args: Value) -> Result<Value, KernelError> {
    let mut s = handle.serve.lock().unwrap();
    let next_state = match &handle.protocol {
        Some(p) => {
            let cur = handle
                .proto_state
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| p.initial.clone());
            match p.step(&cur, verb) {
                Ok(n) => Some(n),
                Err(v) => {
                    return Err(KernelError::Denied(format!(
                        "protocol violation: {} in state {}",
                        v.verb, v.state
                    )));
                }
            }
        }
        None => None,
    };
    frame::write_frame(&mut *s, &json!({"op": "call", "verb": verb, "args": args}))
        .map_err(|e| KernelError::Corrupt(format!("call write: {e}")))?;
    let resp =
        frame::read_frame(&mut *s).map_err(|e| KernelError::Corrupt(format!("call read: {e}")))?;
    if let Some(err) = resp.get("err").and_then(|e| e.as_str()) {
        return Err(KernelError::Denied(format!("plugin error: {err}")));
    }
    if let Some(n) = next_state {
        *handle.proto_state.lock().unwrap() = Some(n);
    }
    Ok(resp.get("ok").cloned().unwrap_or(Value::Null))
}

fn str_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The plugin's F4 truth table from its hello: class = the plugin (one
/// handler, one table — D1), verbs keyed by their full `family::verb` name.
/// Verbs without a declared `kind` are simply not in the table (no
/// character known; they route as before).
fn build_verb_table(
    name: &str,
    verbs: &[String],
    tools_meta: &Value,
    hello: &Value,
) -> Result<VerbTable, String> {
    let mut table = VerbTable::new();
    if let Some(rho) = hello.get("holding_rho").and_then(|r| r.as_str()) {
        let rho = match rho {
            "inverse" => RevertGrade::Inverse,
            "compensable" => RevertGrade::Compensable,
            "external" => RevertGrade::External,
            other => return Err(format!("unknown holding_rho {other}")),
        };
        table
            .declare_class(name, rho)
            .map_err(|e| format!("{e:?}"))?;
    }
    for v in verbs {
        let meta = &tools_meta[v.as_str()];
        if let Some(entry) = kind_from_meta(meta)? {
            table
                .register(name, v, entry)
                .map_err(|e| format!("{v}: {e:?}"))?;
        }
    }
    if let Some(proto) = protocol_from_json(hello.get("protocol"))? {
        table
            .declare_protocol(name, proto)
            .map_err(|e| format!("protocol: {e:?}"))?;
    }
    table.check_all().map_err(|e| format!("{e:?}"))?;
    Ok(table)
}

fn kind_from_meta(meta: &Value) -> Result<Option<VerbEntry>, String> {
    let Some(kind) = meta.get("kind").and_then(|k| k.as_str()) else {
        return Ok(None);
    };
    let compensate = meta
        .get("compensate_with")
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let mut entry = match kind {
        "repeatable" => VerbEntry::repeatable(),
        "repeatable_shared" => VerbEntry::repeatable_shared(),
        "transforming" => VerbEntry::transforming(),
        "consuming" => {
            let world = match meta.get("world").and_then(|w| w.as_str()) {
                None | Some("held") => ConsumeGrade::Held,
                Some("compensable") => ConsumeGrade::Compensable {
                    compensate_with: compensate
                        .clone()
                        .ok_or("consuming/compensable needs compensate_with")?,
                },
                Some("external") => ConsumeGrade::External,
                Some(other) => return Err(format!("unknown consuming world {other}")),
            };
            VerbEntry::consuming(world)
        }
        "emitting" => {
            let world = match meta.get("world").and_then(|w| w.as_str()) {
                None | Some("external") => EmitGrade::External,
                Some("compensable") => EmitGrade::Compensable {
                    compensate_with: compensate
                        .clone()
                        .ok_or("emitting/compensable needs compensate_with")?,
                },
                Some(other) => return Err(format!("unknown emitting world {other}")),
            };
            let amortizable = meta
                .get("amortizable")
                .and_then(|a| a.as_bool())
                .unwrap_or(true);
            VerbEntry::emitting(world, amortizable)
        }
        other => return Err(format!("unknown verb kind {other}")),
    };
    if let Some(b) = meta.get("idempotent").and_then(|b| b.as_bool()) {
        entry.idempotent = b;
    }
    if let Some(b) = meta.get("commutes").and_then(|b| b.as_bool()) {
        entry.commutes = b;
    }
    if let Some(d) = meta.get("degrade").and_then(|d| d.as_str()) {
        entry = entry.degrades_to(d);
    }
    Ok(Some(entry))
}

fn kind_label(e: &VerbEntry) -> &'static str {
    match e.kind {
        Kind::Repeatable => "repeatable",
        Kind::Transforming => "transforming",
        Kind::Consuming { .. } => "consuming",
        Kind::Emitting { .. } => "emitting",
    }
}

/// `{"initial": "s0", "transitions": [["s0", "family::verb", "s1"], …]}`.
fn protocol_from_json(v: Option<&Value>) -> Result<Option<Protocol>, String> {
    let Some(v) = v else { return Ok(None) };
    if v.is_null() {
        return Ok(None);
    }
    let initial = v
        .get("initial")
        .and_then(|i| i.as_str())
        .ok_or("protocol needs an initial state")?;
    let mut p = Protocol::new(initial);
    for t in v
        .get("transitions")
        .and_then(|t| t.as_array())
        .unwrap_or(&Vec::new())
    {
        let (Some(from), Some(verb), Some(to)) = (
            t.get(0).and_then(|x| x.as_str()),
            t.get(1).and_then(|x| x.as_str()),
            t.get(2).and_then(|x| x.as_str()),
        ) else {
            return Err("protocol transition must be [from, verb, to]".into());
        };
        p = p.transition(from, verb, to);
    }
    Ok(Some(p))
}

/// The plugin's F5 manifest from its hello: per verb, the caps it needs to
/// invoke and the services it depends on (`tools[verb].requires`).
fn manifest_from_meta(name: &str, verbs: &[String], tools_meta: &Value) -> Manifest {
    let mut m = Manifest {
        driver: name.to_string(),
        verbs: BTreeMap::new(),
    };
    for v in verbs {
        let req = &tools_meta[v.as_str()]["requires"];
        m.verbs.insert(
            v.clone(),
            Requires {
                caps: Flat(str_array(&req["caps"]).into_iter().collect()),
                deps: Flat(str_array(&req["deps"]).into_iter().collect()),
                uses: Default::default(),
            },
        );
    }
    m
}

/// The host as the F2 `World`: the physical inverses of the kernel's
/// built-in holdings. Called with the ledger lock held; takes host locks only.
pub(crate) struct HostWorld {
    pub(crate) inner: Arc<HostInner>,
}

impl World for HostWorld {
    fn release(&mut self, item: &LiveItem) -> Result<(), ()> {
        match item.class_id.as_str() {
            CLASS_SUBSCRIPTION => {
                let id: u64 = item.instance.as_str().parse().unwrap_or(0);
                self.inner.subs.lock().unwrap().retain(|s| s.id != id);
                Ok(())
            }
            CLASS_PLUGIN => {
                // Only this incarnation (generation = spawn token): a row of an
                // older incarnation must never take down a newer plugin of the
                // same name.
                let handle = {
                    let mut plugins = self.inner.plugins.lock().unwrap();
                    let same = plugins
                        .get(item.instance.as_str())
                        .map(|h| h.generation == item.generation.as_str())
                        .unwrap_or(false);
                    if same {
                        plugins.remove(item.instance.as_str())
                    } else {
                        None
                    }
                };
                if let Some(h) = handle {
                    kill_and_reap(&h);
                    cleanup_plugin(&self.inner, item.instance.as_str());
                }
                Ok(())
            }
            CLASS_PROCESS => {
                // Kill only the exact witnessed incarnation — generation is
                // "<pid>:<start time>", so a recycled pid is never hit.
                let mut parts = item.generation.as_str().split(':');
                let pid: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
                let start: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                if proc_alive(pid, start) {
                    kill_pid(pid);
                }
                Ok(())
            }
            CLASS_PORT => Ok(()), // nothing physical to release; reconcile bind-probes
            CLASS_FILE_LOCK => match std::fs::remove_file(item.instance.as_str()) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(()),
            },
            _ => Ok(()), // nothing physical behind other rows
        }
    }
    fn compensate(&mut self, _item: &LiveItem, _key: &str) -> Result<bool, ()> {
        Ok(true) // no compensable built-in class
    }
}

fn kill_and_reap(h: &PluginHandle) {
    let mut child = h.child.lock().unwrap();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&h.sock_path);
}

/// The one teardown path (crash-only): release the plugin's ownership
/// closure children first through `HostWorld`, then audit. Idempotent — a
/// second call for the same plugin finds nothing to release.
fn reclaim(kernel: &Arc<Kernel>, inner: &Arc<HostInner>, name: &str, reason: &str) -> usize {
    let subject = format!("plugin:{name}");
    let now = crate::db::now_unix();
    let mut world = HostWorld {
        inner: inner.clone(),
    };
    let (outcome, released) = match kernel.ledger.teardown(
        &SubjectId::new(&subject),
        &mut world,
        Timestamp::try_from(now).expect("system timestamp in range"),
    ) {
        Ok(x) => x,
        Err(e) => {
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "plugin.reclaim_error", "plugin": name, "reason": reason,
                "error": e.to_string(),
            }));
            (RunOutcome::Completed { failed: Vec::new() }, 0)
        }
    };
    // Defensive: a handle the ledger did not know about still gets cleaned.
    let stray = inner.plugins.lock().unwrap().remove(name);
    if let Some(h) = stray {
        kill_and_reap(&h);
        cleanup_plugin(inner, name);
    }
    let failed = match &outcome {
        RunOutcome::Completed { failed } => failed.clone(),
        RunOutcome::Crashed => Vec::new(),
    };
    if released > 0 || !failed.is_empty() {
        let _ = kernel.audit.lock().unwrap().append(json!({
            "event": "plugin.reclaimed", "plugin": name, "reason": reason,
            "released": released, "failed": failed.iter().map(|id| id.get()).collect::<Vec<_>>(),
        }));
    }
    released
}

/// Per-plugin thread draining the bounded event queue onto the plugin's
/// events channel (or the serve channel, when it declared none).
fn spawn_event_pump(handle: Arc<PluginHandle>, rx: Receiver<Value>) {
    std::thread::spawn(move || {
        for ev in rx {
            let target = handle.events.as_ref().unwrap_or(&handle.serve);
            let mut s = target.lock().unwrap();
            if frame::write_frame(&mut *s, &ev).is_err() {
                return;
            }
        }
    });
}

/// The fiber-side invoke (WP-06): a plan run (or another kernel-side
/// subject) calls a verb through the same capability gate, budget spend and
/// protocol step the plugin-facing `invoke` op uses.
pub(crate) fn invoke_as(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    subject: &str,
    verb: &str,
    args: Value,
) -> Result<Value, KernelError> {
    let now = crate::db::now_unix();
    let family = verb.split("::").next().unwrap_or(verb);
    let short = verb.rsplit("::").next().unwrap_or(verb);
    let resource = format!("driver:{family}");
    if let Err(e) = kernel
        .caps
        .find_and_exercise(subject, &resource, short, now)
    {
        let _ = kernel.audit.lock().unwrap().append(json!({
            "event": "invoke.denied", "from": subject, "verb": verb,
            "reason": e.to_string(),
        }));
        return Err(e);
    }
    let handle = {
        let routes = inner.routes.lock().unwrap();
        let target = routes
            .get(verb)
            .map(|e| e.plugin.clone())
            .ok_or_else(|| KernelError::NotFound(format!("no route for verb: {verb}")))?;
        inner
            .plugins
            .lock()
            .unwrap()
            .get(&target)
            .cloned()
            .ok_or_else(|| KernelError::NotFound(format!("plugin gone: {target}")))?
    };
    call_on(&handle, verb, args)
}

/// The kernel's verb-label schemas for plan admission and the sink rule,
/// derived from the route table's declared verb characters. v0: reads carry
/// no confidentiality (the taint plane lands with WP-09); emitting verbs are
/// external sinks.
pub(crate) fn kernel_schemas(inner: &HostInner) -> crate::plancheck::VerbSchemas {
    let routes = inner.routes.lock().unwrap();
    let mut s = crate::plancheck::VerbSchemas::default();
    for (verb, e) in routes.iter() {
        match e.kind {
            Some("repeatable") | Some("repeatable_shared") => {
                s.observe.insert(verb.clone(), Label::public_trusted());
            }
            Some("emitting") => {
                s.external_effects.insert(verb.clone(), true);
            }
            _ => {}
        }
    }
    s
}

/// The sink target of one effect (WP-06): the route's declared `target` arg,
/// normalized by kind ("origin" → scheme://host[:port]); `*` when undeclared.
pub(crate) fn target_of(inner: &HostInner, verb: &str, args: &Value) -> String {
    let spec = {
        let routes = inner.routes.lock().unwrap();
        routes
            .get(verb)
            .and_then(|e| e.target.as_ref().map(|t| (t.arg.clone(), t.kind.clone())))
    };
    let Some((arg, kind)) = spec else {
        return "*".into();
    };
    let raw = args[&arg].as_str().unwrap_or("*");
    match kind.as_str() {
        "origin" => origin_of(raw).unwrap_or_else(|| raw.to_string()),
        _ => raw.to_string(),
    }
}

fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let hostport = rest.split('/').next().unwrap_or(rest);
    if hostport.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{hostport}"))
}

/// Is this verb on the F3 hard list (emitting ∧ non-amortizable)?
pub(crate) fn withholds(inner: &HostInner, verb: &str) -> bool {
    inner
        .routes
        .lock()
        .unwrap()
        .get(verb)
        .map(|e| e.withhold)
        .unwrap_or(false)
}

/// Subscribe an arbitrary kernel-side subject (e.g. a plan run's segment) to
/// a topic. `Host::subscribe_local` is this with the `kernel` subject.
pub(crate) fn subscribe_with_subject(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    subject: &str,
    topic: &str,
) -> Result<(u64, Receiver<Value>), KernelError> {
    let (tx, rx) = sync_channel::<Value>(EVENT_QUEUE);
    let id = inner.next_sub.fetch_add(1, Ordering::SeqCst);
    let holding = kernel.ledger.hold_exclusive(
        ExclusiveRequest {
            owner: SubjectId::new(subject),
            resource: ResourceKey::new(
                ClassId::new(CLASS_SUBSCRIPTION),
                InstanceId::new(&id.to_string()),
            ),
            generation: Generation::new("sub"),
            parent: None,
            lease: LeaseRequest::UseClassDefault,
        },
        Timestamp::try_from(crate::db::now_unix()).map_err(crate::ledger::map_err)?,
    )?;
    inner.subs.lock().unwrap().push(Sub {
        id,
        topic: topic.to_string(),
        target: SubTarget::Local(tx),
        holding,
    });
    Ok((id, rx))
}

/// Deliver an event to every matching subscriber. Overflow policy: a local
/// subscriber is dropped; a plugin subscriber's whole connection is cut
/// (m0 §3 — never let a slow consumer wedge the kernel).
pub(crate) fn dispatch_event(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    topic: &str,
    data: Value,
) -> usize {
    enum Target {
        Local(u64, HoldingHandle, SyncSender<Value>),
        Plugin(u64, HoldingHandle, String),
    }
    let targets: Vec<Target> = {
        let subs = inner.subs.lock().unwrap();
        subs.iter()
            .filter(|s| topic_matches(&s.topic, topic))
            .map(|s| match &s.target {
                SubTarget::Local(tx) => Target::Local(s.id, s.holding.clone(), tx.clone()),
                SubTarget::Plugin(name) => Target::Plugin(s.id, s.holding.clone(), name.clone()),
            })
            .collect()
    };
    let mut delivered = 0usize;
    let mut drop_subs: Vec<(u64, HoldingHandle)> = Vec::new();
    let mut kill_plugins: Vec<String> = Vec::new();
    for t in targets {
        match t {
            Target::Local(id, holding, tx) => {
                let ev = json!({"topic": topic, "data": data, "sub": id});
                match tx.try_send(ev) {
                    Ok(()) => delivered += 1,
                    Err(TrySendError::Full(_)) => {
                        drop_subs.push((id, holding));
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "events.overflow", "sub": id, "topic": topic,
                        }));
                    }
                    Err(TrySendError::Disconnected(_)) => drop_subs.push((id, holding)),
                }
            }
            Target::Plugin(id, holding, name) => {
                let tx = inner
                    .plugins
                    .lock()
                    .unwrap()
                    .get(&name)
                    .map(|h| h.events_tx.clone());
                let Some(tx) = tx else {
                    drop_subs.push((id, holding));
                    continue;
                };
                let ev = json!({"op": "event", "sub": id, "topic": topic, "data": data});
                match tx.try_send(ev) {
                    Ok(()) => delivered += 1,
                    Err(TrySendError::Full(_)) => {
                        // A plugin that cannot keep up is reclaimed on the
                        // one teardown path (its subscriptions go with it).
                        kill_plugins.push(name.clone());
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "events.overflow", "sub": id, "topic": topic,
                            "plugin": name,
                        }));
                    }
                    Err(TrySendError::Disconnected(_)) => drop_subs.push((id, holding)),
                }
            }
        }
    }
    if !drop_subs.is_empty() {
        let ids: Vec<u64> = drop_subs.iter().map(|(id, _)| *id).collect();
        inner.subs.lock().unwrap().retain(|s| !ids.contains(&s.id));
        let now = crate::db::now_unix();
        for (_, holding) in drop_subs {
            let _ = kernel.ledger.release(
                &holding,
                Timestamp::try_from(now).expect("system timestamp in range"),
            );
        }
    }
    for name in kill_plugins {
        reclaim(kernel, inner, &name, "events.overflow");
    }
    delivered
}

fn cleanup_plugin(inner: &Arc<HostInner>, name: &str) {
    inner
        .routes
        .lock()
        .unwrap()
        .retain(|_, entry| entry.plugin != name);
    inner.subs.lock().unwrap().retain(|s| match &s.target {
        SubTarget::Plugin(n) => n != name,
        _ => true,
    });
}

/// Topic patterns: a trailing `*` matches any topic with that prefix
/// (`model::session::*`); otherwise exact match.
fn topic_matches(pattern: &str, topic: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => topic.starts_with(prefix),
        None => pattern == topic,
    }
}

/// The client-channel loop: serve one plugin's kernel requests until EOF.
fn spawn_client_loop(
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
    plans: Arc<crate::plans::PlanService>,
    name: String,
    mut stream: UnixStream,
) {
    std::thread::spawn(move || {
        loop {
            let req = match frame::read_frame(&mut stream) {
                Ok(r) => r,
                Err(_) => {
                    // The plugin went away (exit or crash): crash-only
                    // reclamation of everything it held. A graceful
                    // shutdown that raced us keeps its reason.
                    let reason = inner
                        .plugins
                        .lock()
                        .unwrap()
                        .get(&name)
                        .map(|h| {
                            if h.shutting_down.load(Ordering::SeqCst) {
                                "shutdown"
                            } else {
                                "exited"
                            }
                        })
                        .unwrap_or("exited");
                    reclaim(&kernel, &inner, &name, reason);
                    return;
                }
            };
            count_context(&inner, &req);
            let resp = handle_client_op(&kernel, &inner, &plans, &name, &req, &mut stream);
            let resp = match resp {
                Ok(v) => v,
                Err(e) => json!({"err": e.to_string()}),
            };
            count_context(&inner, &resp);
            // `read` writes its own response before streaming chunks.
            if !resp.is_null() && frame::write_frame(&mut stream, &resp).is_err() {
                return;
            }
        }
    });
}

fn count_context(inner: &Arc<HostInner>, v: &Value) {
    if v.is_null() {
        return;
    }
    let n = serde_json::to_vec(v).map(|b| b.len() as u64).unwrap_or(0);
    inner.meter.lock().unwrap().count_context(n);
}

// ---- substrate holdings (WP-02) ----

/// `hold {class, instance, substrate, lease_secs?}` → `{id, generation}`.
/// Restricted to the holdable built-in classes; the holding's parent is the
/// caller's `kernel/plugin` holding, so plugin death reclaims it children
/// first. For `kernel/process` the kernel derives the generation itself —
/// `<pid>:<start time>` witnesses the exact incarnation.
fn op_hold(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    name: &str,
    req: &Value,
    now: u64,
) -> Result<Value, KernelError> {
    let request: HoldRequest = serde_json::from_value(req.clone())
        .map_err(|e| KernelError::Denied(format!("hold request: {e}")))?;
    let class = request.class.as_str();
    let instance = request.instance;
    let mut substrate = request.substrate;
    let lease = request
        .lease_secs
        .map(LeaseDuration::try_from)
        .transpose()
        .map_err(crate::ledger::map_err)?
        .map(LeaseRequest::For)
        .unwrap_or_default();
    let now = Timestamp::try_from(now).map_err(crate::ledger::map_err)?;
    if instance.is_empty() {
        return Err(KernelError::Denied("hold: instance required".into()));
    }
    let positive_pid = |v: &Value| {
        v.as_u64()
            .and_then(|n| i32::try_from(n).ok())
            .filter(|p| *p > 0)
            .map(|p| p as u32)
            .ok_or_else(|| {
                KernelError::Denied("substrate pid must be a positive process ID".into())
            })
    };
    let generation = match class {
        CLASS_PROCESS => {
            let pid = positive_pid(&substrate["pid"])?;
            let start = proc_start_time(pid)
                .ok_or_else(|| KernelError::Denied("hold: process not found".into()))?;
            substrate = json!({"pid":pid,"start":start});
            Generation::new(format!("{pid}:{start}"))
        }
        CLASS_FILE_LOCK => {
            if let Some(raw) = substrate.get("owner_pid") {
                let pid = positive_pid(raw)?;
                if let Some(start) = proc_start_time(pid) {
                    substrate["owner_start"] = json!(start);
                }
            }
            Generation::new(rand_token())
        }
        _ => Generation::new(rand_token()),
    };
    let parent = inner
        .plugins
        .lock()
        .unwrap()
        .get(name)
        .map(|h| h.holding.clone());
    let handle = kernel.ledger.hold_substrate(ExclusiveRequest {
        owner: SubjectId::new(format!("plugin:{name}")), resource: ResourceKey::new(ClassId::new(class), InstanceId::new(&instance)),
        generation, parent, lease,
    }, &substrate, now).map_err(|e| {
        let _ = kernel.audit.lock().unwrap().append(json!({"event":"hold.denied","from":name,"class":class,"instance":instance,"reason":e.to_string()}));
        e
    })?;
    let reference = HoldingRef {
        id: handle.id().get(),
        generation: handle.generation().to_string(),
    };
    let _ = kernel.audit.lock().unwrap().append(json!({"event":"substrate.held","plugin":name,"class":class,"instance":instance,"holding":reference.id,"generation":reference.generation,"lease_secs":request.lease_secs}));
    Ok(json!({"ok":reference}))
}

fn domain_handle(reference: &HoldingRef) -> Result<HoldingHandle, KernelError> {
    if reference.generation.is_empty() {
        return Err(KernelError::Denied("empty generation".into()));
    }
    Ok(HoldingHandle::new(
        HoldingId::try_from(reference.id).map_err(crate::ledger::map_err)?,
        Generation::new(&reference.generation),
    ))
}

// M2 will replace the synchronous world call with a committed cleanup obligation.
fn op_release(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    name: &str,
    req: &Value,
    now: u64,
) -> Result<Value, KernelError> {
    let request: ReleaseRequest = serde_json::from_value(req.clone())
        .map_err(|e| KernelError::Denied(format!("release request: {e}")))?;
    let handle = domain_handle(&request.holding)?;
    let now = Timestamp::try_from(now).map_err(crate::ledger::map_err)?;
    let subject = SubjectId::new(format!("plugin:{name}"));
    let h = kernel
        .ledger
        .holding(handle.id())?
        .ok_or_else(|| KernelError::Denied("release: unknown holding".into()))?;
    if h.subject != subject || h.released_at.is_some() {
        return Err(KernelError::Denied("release: not your holding".into()));
    }
    if &h.generation != handle.generation() {
        return Err(KernelError::Denied("release: stale generation".into()));
    }
    let mut world = HostWorld {
        inner: inner.clone(),
    };
    let item = LiveItem {
        id: h.id,
        parent: h.parent,
        class_id: h.class_id.clone(),
        instance: h.instance.clone(),
        generation: h.generation.clone(),
        grade: RevertGrade::Inverse,
    };
    let _ = world.release(&item);
    kernel.ledger.transaction(|tx| {
        let current = tx
            .holding(handle.id())
            .ok_or_else(|| KernelError::Denied("release: unknown holding".into()))?;
        if current.subject != subject {
            return Err(KernelError::Denied("release: owner changed".into()));
        }
        tx.release(&handle, now)
    })?;
    let _ = kernel.audit.lock().unwrap().append(json!({"event":"substrate.released","plugin":name,"holding":handle.id().get(),"class":h.class_id.as_str(),"instance":h.instance.as_str()}));
    Ok(json!({"ok":ReleaseResponse { released:true }}))
}

fn op_renew(kernel: &Arc<Kernel>, name: &str, req: &Value, now: u64) -> Result<Value, KernelError> {
    let request: RenewRequest = serde_json::from_value(req.clone())
        .map_err(|e| KernelError::Denied(format!("renew request: {e}")))?;
    let handle = domain_handle(&request.holding)?;
    let now = Timestamp::try_from(now).map_err(crate::ledger::map_err)?;
    let lease = request
        .lease_secs
        .map(LeaseDuration::try_from)
        .transpose()
        .map_err(crate::ledger::map_err)?
        .map(LeaseRequest::For)
        .unwrap_or_default();
    let expires = kernel
        .ledger
        .transaction(|tx| {
            let h = tx
                .holding(handle.id())
                .ok_or_else(|| KernelError::Denied("renew: unknown holding".into()))?;
            if h.subject.as_str() != format!("plugin:{name}") {
                return Err(KernelError::Denied("renew: not your holding".into()));
            }
            tx.renew(&handle, lease, now)
        })?
        .expires_at()
        .map(Timestamp::get);
    let _ = kernel.audit.lock().unwrap().append(json!({"event":"substrate.renewed","plugin":name,"holding":handle.id().get(),"lease_expires_at":expires}));
    Ok(json!({"ok":RenewResponse { lease_expires_at:expires }}))
}

fn handle_client_op(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    plans: &Arc<crate::plans::PlanService>,
    name: &str,
    req: &Value,
    stream: &mut UnixStream,
) -> Result<Value, KernelError> {
    let now = crate::db::now_unix();
    match req["op"].as_str() {
        // ---- invoke: the capability-gated plugin→plugin path ----
        Some("invoke") => {
            let verb = req["verb"].as_str().unwrap_or("");
            let args = req.get("args").cloned().unwrap_or(Value::Null);
            let family = verb.split("::").next().unwrap_or(verb);
            let short = verb.rsplit("::").next().unwrap_or(verb);
            let subject = format!("plugin:{name}");
            let resource = format!("driver:{family}");
            // F5 effect row: the slot's position ceiling comes before the
            // subject's grants (actual = row ∩ grant) — and before any
            // budget is spent.
            let in_row = inner
                .plugins
                .lock()
                .unwrap()
                .get(name)
                .map(|h| {
                    h.offers
                        .as_ref()
                        .map(|o| o.0.contains(verb))
                        .unwrap_or(true)
                })
                .unwrap_or(true);
            if !in_row {
                let _ = kernel.audit.lock().unwrap().append(json!({
                    "event": "invoke.denied", "from": name, "verb": verb,
                    "reason": "verb outside the slot row",
                }));
                return Err(KernelError::Denied(format!(
                    "verb outside the slot row: {verb}"
                )));
            }
            let cap = match kernel
                .caps
                .find_and_exercise(&subject, &resource, short, now)
            {
                Ok(id) => id,
                Err(e) => {
                    let _ = kernel.audit.lock().unwrap().append(json!({
                        "event": "invoke.denied", "from": name, "verb": verb,
                        "reason": e.to_string(),
                    }));
                    return Err(e);
                }
            };
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "invoke.allowed", "from": name, "verb": verb, "cap": cap,
            }));
            let handle = {
                let routes = inner.routes.lock().unwrap();
                let target = routes
                    .get(verb)
                    .map(|e| e.plugin.clone())
                    .ok_or_else(|| KernelError::NotFound(format!("no route for verb: {verb}")))?;
                inner
                    .plugins
                    .lock()
                    .unwrap()
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| KernelError::NotFound(format!("plugin gone: {target}")))?
            };
            let out = call_on(&handle, verb, args)?;
            Ok(json!({"ok": out}))
        }

        // ---- grants introspection: what may *this* plugin invoke? ----
        // Joins the caller's live capabilities with the route table's
        // advertised verb metadata: the driver owns descriptions/schemas,
        // the user owns grants, the caller (e.g. the model driver) gets
        // ready-made tool definitions. The kernel joins strings; it never
        // interprets them.
        Some("grants") => {
            let subject = format!("plugin:{name}");
            let caps = kernel.caps.list_live(&subject, now)?;
            // Pass 1: merge per verb. Each entry tracks the strongest single
            // contributing grant: unlimited counts beat counted (then the
            // larger balance wins); no-expiry beats expiring (then the later
            // one wins). The winner's holding id is reported (WP-03).
            struct Best {
                counts: Option<u64>,
                expires_at: Option<u64>,
                holding: u64,
            }
            let key = |counts: Option<u64>, expires: Option<u64>| {
                (counts.unwrap_or(u64::MAX), expires.unwrap_or(u64::MAX))
            };
            let mut merged: BTreeMap<String, Best> = BTreeMap::new();
            for cap in &caps {
                let Some(family) = cap.resource.strip_prefix("driver:") else {
                    continue;
                };
                let Some(holding) = kernel
                    .ledger
                    .cap_holding(&AccountId::new(&cap.cap_id))?
                    .map(|h| h.id.get())
                else {
                    continue;
                };
                for short in &cap.verbs {
                    let verb = format!("{family}::{short}");
                    // Balance = capacity − fold of spend rows (F1), a snapshot.
                    let this = kernel.caps.counts_left(cap, short)?;
                    merged
                        .entry(verb)
                        .and_modify(|acc| {
                            if key(this, cap.constraints.expires_at)
                                > key(acc.counts, acc.expires_at)
                            {
                                *acc = Best {
                                    counts: this,
                                    expires_at: cap.constraints.expires_at,
                                    holding,
                                };
                            }
                        })
                        .or_insert(Best {
                            counts: this,
                            expires_at: cap.constraints.expires_at,
                            holding,
                        });
                }
            }
            // Pass 2: join with the route table's advertised metadata.
            let routes = inner.routes.lock().unwrap();
            let grants: Vec<Value> = merged
                .into_iter()
                .map(|(verb, best)| {
                    let (description, schema, kind, budgeted) = routes
                        .get(&verb)
                        .map(|e| (e.description.clone(), e.schema.clone(), e.kind, e.budgeted))
                        .unwrap_or_else(|| (String::new(), json!({"type": "object"}), None, None));
                    let mut g = json!({
                        "verb": verb, "description": description, "schema": schema,
                        "holding": best.holding,
                    });
                    if let Some(n) = best.counts {
                        g["counts_left"] = json!(n);
                    }
                    if let Some(t) = best.expires_at {
                        g["expires_at"] = json!(t);
                    }
                    // F4: the verb character the serving plugin declared.
                    if let Some(k) = kind {
                        g["kind"] = json!(k);
                    }
                    if let Some(b) = budgeted {
                        g["budgeted"] = json!(b);
                    }
                    g
                })
                .collect();
            Ok(json!({"ok": {"grants": grants}}))
        }

        // ---- event bus ----
        Some("emit") => {
            let topic = req["topic"].as_str().unwrap_or("");
            let data = req.get("data").cloned().unwrap_or(Value::Null);
            let n = dispatch_event(kernel, inner, topic, data);
            Ok(json!({"ok": {"delivered": n}}))
        }
        Some("subscribe") => {
            let topic = req["topic"].as_str().unwrap_or("").to_string();
            let id = inner.next_sub.fetch_add(1, Ordering::SeqCst);
            // A standing inbound channel is a holding, child of the plugin's
            // own holding: teardown unsubscribes before it kills.
            let parent = inner
                .plugins
                .lock()
                .unwrap()
                .get(name)
                .map(|h| h.holding.clone());
            let subject = format!("plugin:{name}");
            let holding = kernel.ledger.hold_exclusive(
                ExclusiveRequest {
                    owner: SubjectId::new(&subject),
                    resource: ResourceKey::new(
                        ClassId::new(CLASS_SUBSCRIPTION),
                        InstanceId::new(&id.to_string()),
                    ),
                    generation: Generation::new("sub"),
                    parent: parent,
                    lease: LeaseRequest::UseClassDefault,
                },
                Timestamp::try_from(now).map_err(crate::ledger::map_err)?,
            )?;
            inner.subs.lock().unwrap().push(Sub {
                id,
                topic: topic.clone(),
                target: SubTarget::Plugin(name.to_string()),
                holding: holding.clone(),
            });
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "events.subscribed", "plugin": name, "topic": topic, "sub": id,
                "holding": holding.id().get(),
            }));
            Ok(json!({"ok": {"sub": id}}))
        }
        Some("unsubscribe") => {
            // A plugin can only drop its own subscriptions. Find under the
            // subs lock, release in the ledger without it (lock order), then
            // remove.
            let id = req["sub"].as_u64().unwrap_or(0);
            let holding = {
                let subs = inner.subs.lock().unwrap();
                subs.iter()
                    .find(|s| s.id == id && matches!(&s.target, SubTarget::Plugin(n) if n == name))
                    .map(|s| s.holding.clone())
            };
            let removed = match holding {
                Some(h) => {
                    let _ = kernel.ledger.release(
                        &h,
                        Timestamp::try_from(now).expect("system timestamp in range"),
                    );
                    inner.subs.lock().unwrap().retain(|s| s.id != id);
                    true
                }
                None => false,
            };
            Ok(json!({"ok": {"removed": removed}}))
        }

        // ---- parent/child instantiation (WP-04) ----
        Some("spawn_child") => {
            // Capability gate: the caller must hold `kernel:spawn` /
            // `spawn_child`. The child's `kernel/plugin` holding becomes a
            // child of the caller's holding, so reclaiming the caller tears
            // the child down first (F2 closure).
            let subject = format!("plugin:{name}");
            let bin = req["bin"].as_str().unwrap_or("").to_string();
            if let Err(e) =
                kernel
                    .caps
                    .find_and_exercise(&subject, "kernel:spawn", "spawn_child", now)
            {
                let _ = kernel.audit.lock().unwrap().append(json!({
                    "event": "spawn_child.denied", "from": name, "bin": bin,
                    "reason": e.to_string(),
                }));
                return Err(e);
            }
            let parent_holding = inner
                .plugins
                .lock()
                .unwrap()
                .get(name)
                .map(|h| h.holding.clone())
                .ok_or_else(|| KernelError::NotFound(format!("parent plugin gone: {name}")))?;
            let args: Vec<String> = str_array(&req["args"]);
            let envs: Vec<(String, String)> = req["env"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let env_refs: Vec<(&str, &str)> =
                envs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            if bin.is_empty() {
                return Err(KernelError::Corrupt("spawn_child: missing bin".into()));
            }
            let child = spawn_plugin(
                kernel,
                inner,
                plans,
                Path::new(&bin),
                &arg_refs,
                &env_refs,
                None,
                Some((name, parent_holding)),
            )?;
            Ok(json!({"ok": {"name": child}}))
        }

        // ---- substrate holdings (WP-02): plugin-registered child processes,
        // ports, lock files — reclaimed with the plugin, children first;
        // leased ones expire via the sweeper.
        Some("hold") => op_hold(kernel, inner, name, req, now),
        Some("release") => op_release(kernel, inner, name, req, now),
        Some("renew") => op_renew(kernel, name, req, now),

        // ---- plan submission (WP-06): admit only — the model proposes,
        // never executes and never approves. Consent is the person's, given
        // through the CLI.
        Some("plan.submit") => {
            let subject = format!("plugin:{name}");
            let plan_val = req.get("plan").cloned().unwrap_or(Value::Null);
            let bytes = serde_json::to_vec(&plan_val)
                .map_err(|e| KernelError::Corrupt(format!("plan.submit encode: {e}")))?;
            match plans.submit(&subject, &bytes) {
                Ok(out) => {
                    // Put the plan where the CLI can sign it.
                    let dir = kernel.root.join("submitted-plans");
                    let fname = out.plan_hash.replace(':', "_");
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(dir.join(format!("{fname}.json")), &bytes);
                    Ok(json!({"ok": {
                        "run_id": out.run_id,
                        "plan_hash": out.plan_hash,
                        "rendering": out.rendering,
                        "budget": out.budget,
                        "needs": "consent",
                        "plan_path": dir.join(format!("{fname}.json")),
                    }}))
                }
                Err(e) => Err(e),
            }
        }

        // ---- artifact dereference: put (frame, then chunk stream) ----
        Some("put") => {
            let ty = req["type"].as_str().unwrap_or("application/octet-stream");
            let labels: Label = req
                .get("labels")
                .cloned()
                .map(|v| serde_json::from_value(v).unwrap_or_default())
                .unwrap_or_default();
            let origin = format!("plugin:{name}");
            let mut reader = chunk::ChunkReader::new(&mut *stream);
            let result = kernel.cas.put_stream(&mut reader, ty, labels, &origin);
            // Resync the stream even on a CAS error so the channel survives.
            reader
                .drain()
                .map_err(|e| KernelError::Corrupt(format!("put drain: {e}")))?;
            let meta = result?;
            inner.meter.lock().unwrap().count_data(meta.size);
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "artifact.put", "plugin": name, "id": meta.id, "size": meta.size,
            }));
            Ok(json!({"ok": {"meta": meta}}))
        }

        // ---- artifact dereference: read (response frame, then chunks) ----
        Some("read") => {
            let id = req["id"].as_str().unwrap_or("").to_string();
            let offset = req["offset"].as_u64().unwrap_or(0);
            let want = req["len"].as_u64();
            // Reads are free but accounted (读取记账): audit before bytes move.
            let meta = kernel.cas.meta(&id)?;
            let avail = meta.size.saturating_sub(offset);
            let n = want.map(|w| w.min(avail)).unwrap_or(avail);
            let mut f = kernel.cas.open_read(&id)?;
            f.seek(SeekFrom::Start(offset))?;
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "artifact.read", "plugin": name, "id": id,
                "offset": offset, "len": n,
            }));
            let head = json!({"ok": {"len": n}});
            count_context(inner, &head);
            frame::write_frame(stream, &head)
                .map_err(|e| KernelError::Corrupt(format!("read resp: {e}")))?;
            let mut taken = f.take(n);
            let moved = chunk::copy_into_chunks(&mut taken, stream)
                .map_err(|e| KernelError::Corrupt(format!("read stream: {e}")))?;
            inner.meter.lock().unwrap().count_data(moved);
            Ok(Value::Null) // response already sent
        }

        other => Err(KernelError::Corrupt(format!(
            "unknown client op: {other:?}"
        ))),
    }
}

fn accept_with_deadline(
    listener: &UnixListener,
    child: &mut std::process::Child,
    ms: u64,
) -> Result<UnixStream, KernelError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false)?;
                return Ok(s);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(KernelError::Corrupt(format!(
                        "plugin exited before connecting: {status}"
                    )));
                }
                if std::time::Instant::now() > deadline {
                    return Err(KernelError::Corrupt("plugin connect timeout".into()));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn rand_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::ExclusiveRequest;
    use portos_rm::identity::{ClassId, Generation, InstanceId, ResourceKey, SubjectId};
    use portos_rm::time::{LeaseRequest, Timestamp};

    fn host(tag: &str) -> (Host, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("portos-host-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let kernel = Arc::new(Kernel::open(&root).unwrap());
        let host = Host::new(kernel, &root.join("sock")).unwrap();
        (host, root)
    }

    /// A local subscription is a `kernel/subscription` holding: dropping it
    /// releases the holding (tombstone, never a delete), and the bus no longer
    /// delivers to it.
    #[test]
    fn unsubscribe_local_releases_the_subscription_holding() {
        let (host, root) = host("unsub");
        let (id, _rx) = host.subscribe_local("t::*").unwrap();
        assert_eq!(
            host.kernel
                .ledger
                .counts(&ClassId::new(CLASS_SUBSCRIPTION))
                .unwrap(),
            (1, 0)
        );
        assert!(host.unsubscribe_local(id));
        assert_eq!(
            host.kernel
                .ledger
                .counts(&ClassId::new(CLASS_SUBSCRIPTION))
                .unwrap(),
            (0, 1),
            "holding tombstoned"
        );
        assert_eq!(
            host.emit("t::x", json!({})),
            0,
            "no delivery after unsubscribe"
        );
        assert!(!host.unsubscribe_local(id), "second drop is a no-op");
        host.kernel.ledger.invariant().unwrap();
        drop(host);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The sweeper thread (WP-02) expires a leased holding and runs its world
    /// action — the lock file is removed — and audits the sweep. Dropping the
    /// host stops the thread (the test returning proves the join).
    #[test]
    fn sweeper_thread_expires_leased_holdings_and_removes_the_lock_file() {
        let (host, root) = host("sweeper");
        let lock = root.join("test.lock");
        std::fs::write(&lock, b"x").unwrap();
        let now = crate::db::now_unix();
        let id = host
            .kernel
            .ledger
            .hold_substrate(
                ExclusiveRequest {
                    owner: SubjectId::new("kernel"),
                    resource: ResourceKey::new(
                        ClassId::new(CLASS_FILE_LOCK),
                        InstanceId::new(lock.to_str().unwrap()),
                    ),
                    generation: Generation::new("g1"),
                    parent: None,
                    lease: Some(1)
                        .map(|s: u64| LeaseDuration::try_from(s).unwrap())
                        .map(LeaseRequest::For)
                        .unwrap_or_default(),
                },
                &json!({}),
                Timestamp::try_from(now).unwrap(),
            )
            .map(|h| h.id())
            .unwrap();
        host.start_sweeper(std::time::Duration::from_millis(100));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while lock.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "sweeper never expired the lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            host.kernel
                .ledger
                .holding(id)
                .unwrap()
                .unwrap()
                .released_at
                .is_some(),
            "holding tombstoned by the sweep"
        );
        host.kernel.ledger.invariant().unwrap();
        drop(host); // stops and joins the sweeper
        let events = crate::audit::AuditLog::verify(&root.join("audit.log")).unwrap();
        assert!(
            events.iter().any(|e| e["body"]["event"] == "ledger.swept"
                && e["body"]["classes"]["kernel/file-lock"] == 1),
            "the sweep is audited (never silent)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn resource_messages_reject_missing_identity_and_bad_numbers_without_mutation() {
        let (host, root) = host("resource-codec");
        let held=op_hold(&host.kernel,&host.inner,"test",&json!({"op":"hold","class":CLASS_PORT,"instance":"tcp:4567","substrate":{},"lease_secs":5}),100).unwrap();
        let reference: HoldingRef = serde_json::from_value(held["ok"].clone()).unwrap();
        assert_eq!(
            reference.id, 0,
            "missing id used to target this real holding"
        );
        let id = HoldingId::try_from(reference.id).unwrap();
        let before = host.kernel.ledger.holding(id).unwrap().unwrap();
        for bad in [
            json!({"generation":reference.generation}),
            json!({"id":reference.id}),
            json!({"id":reference.id,"generation":""}),
            json!({"id":u64::MAX,"generation":reference.generation}),
        ] {
            assert!(op_release(&host.kernel, &host.inner, "test", &bad, 101).is_err());
            assert_eq!(host.kernel.ledger.holding(id).unwrap().unwrap(), before);
        }
        for lease in [json!(-1), json!("5"), json!(u64::MAX)] {
            let bad =
                json!({"id":reference.id,"generation":reference.generation,"lease_secs":lease});
            assert!(op_renew(&host.kernel, "test", &bad, 101).is_err());
            assert_eq!(host.kernel.ledger.holding(id).unwrap().unwrap(), before);
        }
        for bad in [
            json!({"class":CLASS_PORT,"substrate":{}}),
            json!({"class":CLASS_PORT,"instance":"tcp:4568"}),
            json!({"class":CLASS_PROCESS,"instance":"x","substrate":{"pid":u64::MAX}}),
        ] {
            assert!(op_hold(&host.kernel, &host.inner, "test", &bad, 101).is_err());
            assert_eq!(
                host.kernel
                    .ledger
                    .live_snapshot(&SubjectId::new("plugin:test"))
                    .unwrap()
                    .len(),
                1
            );
        }
        let heartbeat = op_renew(
            &host.kernel,
            "test",
            &serde_json::to_value(&reference).unwrap(),
            101,
        )
        .unwrap();
        assert_eq!(heartbeat["ok"]["lease_expires_at"], 105);
        let released = op_release(
            &host.kernel,
            &host.inner,
            "test",
            &serde_json::to_value(reference).unwrap(),
            102,
        )
        .unwrap();
        assert_eq!(released, json!({"ok":{"released":true}}));
        assert!(
            host.kernel
                .ledger
                .holding(id)
                .unwrap()
                .unwrap()
                .released_at
                .is_some()
        );
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
}
