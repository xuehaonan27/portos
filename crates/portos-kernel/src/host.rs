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
//!     all submit durable retirement of the plugin's ownership closure.
//!     Children are confirmed before parents; cleanup runs outside storage
//!     transactions. A polite shutdown frame is sent only when the plugin's
//!     own cleanup attempt is ready.
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
//! Storage transactions lock the ledger then SQLite and never call the host.
//! Cleanup takes host locks only after its claim commits. Registration can
//! hold its host registry lock across admission to serialize publication with
//! cleanup. Nested plugin/route access takes the plugin registry first.

use crate::ledger::ExclusiveRequest;
use crate::ledger::{
    CLASS_FILE_LOCK, CLASS_PLUGIN, CLASS_PROCESS, CLASS_SUBSCRIPTION, capture_process,
    cleanup_process, execute_target, proc_start_time,
};
use crate::{Kernel, KernelError};
use portos_proto::resource::{
    HoldRequest, HoldingRef, ReleaseRequest, ReleaseResponse, RenewRequest, RenewResponse,
};
use portos_proto::{Label, chunk, frame};
use portos_rm::cleanup::*;
use portos_rm::coeffect::{Flat, Manifest, Mount, Requires, admit_mount};
use portos_rm::identity::{
    AccountId, ClassId, Generation, HoldingHandle, HoldingId, InstanceId, ResourceKey, SubjectId,
    VerbId,
};
use portos_rm::ledger::RevertGrade;
use portos_rm::protocol::{Protocol, ProtocolDraft};
use portos_rm::time::{LeaseDuration, LeaseRequest, Timestamp};
use portos_rm::verbs::{
    CheckedClass, CheckedVerb, ClassDeclarationDraft, ConsumeGrade, EmitGrade, Kind, VerbEntry,
};
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
    character: Option<CheckedVerb>,
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
    witness: HostWitness,
    next_spawn: AtomicU64,
    sock_dir: PathBuf,
    meter: Mutex<crate::metrics::ContextMeter>,
    /// Lease sweeper (WP-02): stop flag and thread handle; stopped on drop.
    sweeper_stop: Arc<AtomicBool>,
    sweeper_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

// Weak registration lets recovery find an existing host by its exact session,
// including during retry from another Host in the same kernel process.
static HOSTS: std::sync::OnceLock<Mutex<BTreeMap<String, std::sync::Weak<HostInner>>>> =
    std::sync::OnceLock::new();
fn host_registry() -> &'static Mutex<BTreeMap<String, std::sync::Weak<HostInner>>> {
    HOSTS.get_or_init(Mutex::default)
}
fn known_host(w: &HostWitness) -> Option<Arc<HostInner>> {
    host_registry()
        .lock()
        .unwrap()
        .get(w.session().as_str())
        .and_then(std::sync::Weak::upgrade)
        .filter(|h| &h.witness == w)
}
pub(crate) fn local_host_gone(w: &HostWitness) -> bool {
    capture_process(std::process::id()).is_ok_and(|p| &p == w.process()) && known_host(w).is_none()
}
pub(crate) fn cleanup_known_host(work: &CleanupWork) -> Option<CleanupOutcome> {
    let host = match work.target() {
        CleanupTarget::Subscription { host, .. } | CleanupTarget::Plugin { host, .. } => host,
        _ => return None,
    };
    known_host(host).map(|inner| HostWorld { inner }.execute(work))
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
            witness: HostWitness::new(
                capture_process(std::process::id())?,
                Generation::new(rand_token()),
            )
            .map_err(crate::ledger::map_err)?,
            next_spawn: AtomicU64::new(1),
            sock_dir: sock_dir.to_path_buf(),
            meter: Mutex::new(crate::metrics::ContextMeter::default()),
            sweeper_stop: Arc::new(AtomicBool::new(false)),
            sweeper_handle: Mutex::new(None),
        });
        host_registry()
            .lock()
            .unwrap()
            .insert(inner.witness.session().to_string(), Arc::downgrade(&inner));
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
    let (name, verbs) = match declaration_identity(&hello) {
        Ok(identity) => identity,
        Err(e) => {
            let _ = child.kill();
            return Err(KernelError::Denied(format!("plugin hello: {e}")));
        }
    };
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
    let class = match check_class_declaration(&name, &verbs, &tools_meta, &hello) {
        Ok(class) => class,
        Err(e) => {
            let _ = child.kill();
            return Err(KernelError::Denied(format!(
                "plugin {name}: verb metadata rejected: {e}"
            )));
        }
    };
    let protocol = class.protocol().cloned();

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

    // Serialize publication with cleanup of this host registry. The transaction
    // performs no world actions, so holding the host lock cannot form a cycle.
    let now = crate::db::now_unix();
    let subject = format!("plugin:{name}");
    let process = capture_process(child.id()).map_err(|e| {
        let _ = child.kill();
        e
    })?;
    let mut plugins = inner.plugins.lock().unwrap();
    let holding = kernel
        .ledger
        .hold_managed(
            ExclusiveRequest {
                owner: SubjectId::new(&subject),
                resource: ResourceKey::new(ClassId::new(CLASS_PLUGIN), InstanceId::new(&name)),
                generation: Generation::new(&token),
                parent: parent.as_ref().map(|(_, h)| h.clone()),
                lease: LeaseRequest::UseClassDefault,
            },
            CleanupTarget::Plugin {
                host: inner.witness.clone(),
                process,
            },
            parent
                .as_ref()
                .map(|(name, _)| SubjectId::new(format!("plugin:{name}"))),
            Timestamp::try_from(now).map_err(crate::ledger::map_err)?,
        )
        .map_err(|e| {
            let _ = child.kill();
            e
        })?;
    let pid = child.id();

    // Register verbs; a route conflict aborts the spawn (and releases the
    // holding again — nothing in the ledger outlives a failed spawn).
    {
        let mut routes = inner.routes.lock().unwrap();
        if let Some(v) = verbs.iter().find(|v| routes.contains_key(*v)) {
            let conflict = v.clone();
            drop(routes);
            drop(plugins);
            let _ = kernel.ledger.release_with_world(
                &holding,
                &mut HostWorld {
                    inner: inner.clone(),
                },
                Timestamp::try_from(now).expect("system timestamp in range"),
            );
            let _ = child.try_wait();
            return Err(KernelError::Denied(format!(
                "verb already routed: {conflict}"
            )));
        }
        let (events_tx, events_rx) = sync_channel::<Value>(EVENT_QUEUE);
        let handle = Arc::new(PluginHandle {
            child: Mutex::new(child),
            serve: Mutex::new(serve_stream),
            events: events.map(Mutex::new),
            events_tx,
            sock_path: sock_path.clone(),
            holding: holding.clone(),
            pid,
            offers,
            protocol,
            proto_state: Mutex::new(None),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
        });
        for v in &verbs {
            let meta = &tools_meta[v.as_str()];
            let entry = class.lookup(&VerbId::new(v)).ok();
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
                    character: entry.cloned(),
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
            holding.clone(),
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
        if !self
            .kernel
            .ledger
            .holding(handle.holding.id())?
            .is_some_and(|h| h.state.is_active() && h.handle() == handle.holding)
        {
            return Err(KernelError::Denied("plugin is retiring".into()));
        }
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
    /// ledger without it, then let the cleanup executor remove the subscription.
    pub fn unsubscribe_local(&self, sub_id: u64) -> bool {
        let holding = {
            let subs = self.inner.subs.lock().unwrap();
            subs.iter()
                .find(|s| s.id == sub_id && matches!(s.target, SubTarget::Local(_)))
                .map(|s| s.holding.clone())
        };
        match holding {
            Some(h) => {
                let result = self.kernel.ledger.release_with_world(
                    &h,
                    &mut HostWorld {
                        inner: self.inner.clone(),
                    },
                    Timestamp::try_from(crate::db::now_unix()).expect("system timestamp in range"),
                );
                result.is_ok_and(|report| report.pending.is_empty())
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

    /// Request the same durable, children-first path used on process exit.
    /// The executor may send a polite frame once all children are confirmed.
    pub fn shutdown(&self, plugin: &str) {
        let handle = self.inner.plugins.lock().unwrap().get(plugin).cloned();
        if let Some(h) = handle {
            h.shutting_down.store(true, Ordering::SeqCst);
            reclaim_holding(&self.kernel, &self.inner, plugin, &h.holding, "shutdown");
        }
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
                    Ok(report) if !report.released.is_empty() || !report.pending.is_empty() => {
                        let mut classes: BTreeMap<String, usize> = BTreeMap::new();
                        for (_, class, _) in &report.released {
                            *classes.entry(class.clone()).or_insert(0) += 1;
                        }
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "ledger.swept",
                            "released": report.released.len(),
                            "classes": classes,
                            "pending": report.pending.iter().map(|t|json!({"cleanup_id":t.id.get(),"holding":t.holding.id().get(),"state":format!("{:?}",t.state)})).collect::<Vec<_>>(),
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
        let subscriptions = self
            .inner
            .subs
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.holding.clone())
            .collect::<Vec<_>>();
        let now = Timestamp::try_from(crate::db::now_unix()).expect("system timestamp in range");
        for holding in subscriptions {
            let _ = self.kernel.ledger.release_with_world(
                &holding,
                &mut HostWorld {
                    inner: self.inner.clone(),
                },
                now,
            );
        }
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
                .unwrap_or_else(|| p.initial().to_string());
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

/// Decode declaration names before constructing domain values.
fn declaration_identity(hello: &Value) -> Result<(String, Vec<String>), String> {
    let name = hello
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("name must be a nonempty string")?;
    let verbs = hello
        .get("verbs")
        .and_then(Value::as_array)
        .ok_or("verbs must be an array")?;
    let mut names = std::collections::BTreeSet::new();
    let verbs = verbs
        .iter()
        .map(|v| {
            let v = v
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("verbs must contain nonempty strings")?;
            if !names.insert(v) {
                return Err("duplicate advertised verb");
            }
            Ok(v.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((name.to_string(), verbs))
}

/// Check the plugin's F4 class using the names decoded by `declaration_identity`.
/// Verbs use their full `family::verb` name. Verbs without a declared `kind`
/// are absent from the class and retain their legacy routing behavior.
fn check_class_declaration(
    name: &str,
    verbs: &[String],
    tools_meta: &Value,
    hello: &Value,
) -> Result<CheckedClass, String> {
    if !tools_meta.is_null() && !tools_meta.is_object() {
        return Err("tools must be an object".into());
    }
    let mut draft = ClassDeclarationDraft::new(ClassId::new(name));
    if let Some(rho) = optional_str(hello, "holding_rho")? {
        draft.holding_grade = Some(match rho {
            "inverse" => RevertGrade::Inverse,
            "compensable" => RevertGrade::Compensable,
            "external" => RevertGrade::External,
            _ => return Err(format!("unknown holding_rho: {rho}")),
        });
    }
    for v in verbs {
        if let Some(entry) = kind_from_meta(&tools_meta[v.as_str()])? {
            draft.verbs.push((VerbId::new(v), entry));
        }
    }
    draft.protocol = protocol_from_json(hello.get("protocol"))?;
    draft.check().map_err(|e| format!("{e:?}"))
}

fn optional_str<'a>(v: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    v.get(key)
        .map(|x| x.as_str().ok_or_else(|| format!("{key} must be a string")))
        .transpose()
}
fn optional_bool(v: &Value, key: &str) -> Result<Option<bool>, String> {
    v.get(key)
        .map(|x| {
            x.as_bool()
                .ok_or_else(|| format!("{key} must be a boolean"))
        })
        .transpose()
}

fn kind_from_meta(meta: &Value) -> Result<Option<VerbEntry>, String> {
    if !meta.is_null() && !meta.is_object() {
        return Err("verb metadata must be an object".into());
    }
    let Some(kind) = optional_str(meta, "kind")? else {
        if [
            "world",
            "compensate_with",
            "amortizable",
            "idempotent",
            "commutes",
            "degrade",
        ]
        .iter()
        .any(|k| meta.get(k).is_some())
        {
            return Err("verb character fields require kind".into());
        }
        return Ok(None);
    };
    let compensate = optional_str(meta, "compensate_with")?.map(VerbId::new);
    let world = optional_str(meta, "world")?;
    let amortizable = optional_bool(meta, "amortizable")?.unwrap_or(true);
    let mut entry = match kind {
        "repeatable" => VerbEntry::repeatable(),
        "repeatable_shared" => VerbEntry::repeatable_shared(),
        "transforming" => VerbEntry::transforming(),
        "consuming" => {
            let world = match world {
                None | Some("held") => ConsumeGrade::Held,
                Some("compensable") => ConsumeGrade::Compensable {
                    compensate_with: compensate
                        .ok_or("consuming/compensable needs compensate_with")?,
                },
                Some("external") => ConsumeGrade::External,
                Some(other) => return Err(format!("unknown consuming world {other}")),
            };
            VerbEntry::consuming(world)
        }
        "emitting" => {
            let world = match world {
                None | Some("external") => EmitGrade::External,
                Some("compensable") => EmitGrade::Compensable {
                    compensate_with: compensate
                        .ok_or("emitting/compensable needs compensate_with")?,
                },
                Some(other) => return Err(format!("unknown emitting world {other}")),
            };
            VerbEntry::emitting(world, amortizable)
        }
        other => return Err(format!("unknown verb kind {other}")),
    };
    if let Some(b) = optional_bool(meta, "idempotent")? {
        entry.idempotent = b;
    }
    if let Some(b) = optional_bool(meta, "commutes")? {
        entry.commutes = b;
    }
    if let Some(d) = optional_str(meta, "degrade")? {
        entry = entry.degrades_to(d);
    }
    Ok(Some(entry))
}

fn kind_label(e: &CheckedVerb) -> &'static str {
    match e.kind() {
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
    let mut p = ProtocolDraft::new(initial);
    for t in v
        .get("transitions")
        .and_then(|t| t.as_array())
        .ok_or("protocol transitions must be an array")?
    {
        if t.as_array().is_none_or(|a| a.len() != 3) {
            return Err("protocol transition must have exactly three names".into());
        }
        let (Some(from), Some(verb), Some(to)) = (
            t.get(0).and_then(|x| x.as_str()),
            t.get(1).and_then(|x| x.as_str()),
            t.get(2).and_then(|x| x.as_str()),
        ) else {
            return Err("protocol transition must be [from, verb, to]".into());
        };
        p = p.transition(from, verb, to);
    }
    Ok(Some(p.check().map_err(|e| format!("protocol: {e:?}"))?))
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

/// The host cleanup executor: physical cleanup of the kernel's
/// built-in holdings. Called after a durable claim, without storage locks.
pub(crate) struct HostWorld {
    pub(crate) inner: Arc<HostInner>,
}

impl CleanupExecutor for HostWorld {
    fn execute(&mut self, work: &CleanupWork) -> CleanupOutcome {
        match work.target() {
            CleanupTarget::Subscription { host, subscription } if host == &self.inner.witness => {
                let mut subs = self.inner.subs.lock().unwrap();
                let before = subs.len();
                subs.retain(|s| !(s.id == *subscription && s.holding == work.task().holding));
                if subs.len() != before {
                    CleanupOutcome::Confirmed
                } else {
                    CleanupOutcome::AlreadyAbsent
                }
            }
            CleanupTarget::Plugin { host, process } if host == &self.inner.witness => {
                let name = work.resource().instance().as_str();
                let mut plugins = self.inner.plugins.lock().unwrap();
                let handle = plugins
                    .get(name)
                    .filter(|h| h.holding == work.task().holding)
                    .cloned();
                if let Some(h) = &handle {
                    if h.shutting_down.load(Ordering::SeqCst) {
                        if let Ok(mut stream) = h.serve.try_lock() {
                            let previous = stream.write_timeout().ok().flatten();
                            if stream
                                .set_write_timeout(Some(std::time::Duration::from_millis(50)))
                                .is_ok()
                            {
                                let _ = frame::write_frame(&mut *stream, &json!({"op":"shutdown"}));
                                let _ = stream.set_write_timeout(previous);
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
                let result = cleanup_process(process);
                if matches!(
                    result,
                    CleanupOutcome::Confirmed | CleanupOutcome::AlreadyAbsent
                ) {
                    if let Some(h) = handle {
                        if let Err(e) = h.child.lock().unwrap().try_wait() {
                            return CleanupOutcome::Retryable(e.to_string());
                        }
                        if let Err(e) = std::fs::remove_file(&h.sock_path) {
                            if e.kind() != std::io::ErrorKind::NotFound {
                                return CleanupOutcome::Retryable(e.to_string());
                            }
                        }
                        // Keep the name locked until its old routes are removed.
                        cleanup_plugin(&self.inner, name);
                        plugins.remove(name);
                    }
                }
                result
            }
            _ => execute_target(work),
        }
    }
}

/// The one teardown path (crash-only): release the plugin's ownership
/// closure children first through `HostWorld`, then audit. Idempotent — a
/// second call for the same plugin finds nothing to release.
fn reclaim(kernel: &Arc<Kernel>, inner: &Arc<HostInner>, name: &str, reason: &str) -> usize {
    let holding = inner
        .plugins
        .lock()
        .unwrap()
        .get(name)
        .map(|h| h.holding.clone());
    holding
        .map(|h| reclaim_holding(kernel, inner, name, &h, reason))
        .unwrap_or(0)
}
fn reclaim_holding(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    name: &str,
    holding: &HoldingHandle,
    reason: &str,
) -> usize {
    let now = crate::db::now_unix();
    let mut world = HostWorld {
        inner: inner.clone(),
    };
    let report = match kernel.ledger.teardown_owner_incarnation(
        holding,
        &mut world,
        Timestamp::try_from(now).expect("system timestamp in range"),
    ) {
        Ok(x) => x,
        Err(e) => {
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "plugin.reclaim_error", "plugin": name, "reason": reason,
                "error": e.to_string(),
            }));
            crate::ledger::CleanupReport::default()
        }
    };
    let released = report.completed.len();
    let pending = report
        .pending
        .iter()
        .map(|t| t.holding.id().get())
        .collect::<Vec<_>>();
    if released > 0 || !pending.is_empty() {
        let _ = kernel.audit.lock().unwrap().append(json!({
            "event": "plugin.reclaimed", "plugin": name, "reason": reason,
            "released": released, "pending": pending,
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
        let target = inner
            .routes
            .lock()
            .unwrap()
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
    if !kernel
        .ledger
        .holding(handle.holding.id())?
        .is_some_and(|h| h.state.is_active() && h.handle() == handle.holding)
    {
        return Err(KernelError::Denied("plugin is retiring".into()));
    }
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
        match e.character.as_ref().map(CheckedVerb::kind) {
            Some(Kind::Repeatable) => {
                s.observe.insert(verb.clone(), Label::public_trusted());
            }
            Some(Kind::Emitting { .. }) => {
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
        .map(|e| e.character.as_ref().is_some_and(CheckedVerb::withhold))
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
    let id = inner
        .next_sub
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
        .map_err(|_| KernelError::Denied("subscription IDs exhausted".into()))?;
    let mut subs = inner.subs.lock().unwrap();
    let holding = kernel.ledger.hold_managed(
        ExclusiveRequest {
            owner: SubjectId::new(subject),
            resource: ResourceKey::new(
                ClassId::new(CLASS_SUBSCRIPTION),
                InstanceId::new(format!("{}/{}", inner.witness.session(), id)),
            ),
            generation: inner.witness.session().clone(),
            parent: None,
            lease: LeaseRequest::UseClassDefault,
        },
        CleanupTarget::Subscription {
            host: inner.witness.clone(),
            subscription: id,
        },
        None,
        Timestamp::try_from(crate::db::now_unix()).map_err(crate::ledger::map_err)?,
    )?;
    subs.push(Sub {
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
        let holding = match &t {
            Target::Local(_, h, _) | Target::Plugin(_, h, _) => h,
        };
        let Some(current) = kernel
            .ledger
            .holding(holding.id())
            .ok()
            .flatten()
            .filter(|h| h.state.is_active() && h.handle() == *holding)
        else {
            continue;
        };
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
                    .filter(|h| Some(h.holding.id()) == current.parent)
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
        let now = crate::db::now_unix();
        for (_, holding) in drop_subs {
            let _ = kernel.ledger.release_with_world(
                &holding,
                &mut HostWorld {
                    inner: inner.clone(),
                },
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
    holding: HoldingHandle,
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
                    reclaim_holding(&kernel, &inner, &name, &holding, reason);
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
    kernel.ledger.transaction(|tx| {
        let h = tx
            .holding(handle.id())
            .ok_or_else(|| KernelError::Denied("release: unknown holding".into()))?;
        if h.subject != subject {
            return Err(KernelError::Denied("release: not your holding".into()));
        }
        tx.request_retirement(&handle, now)
    })?;
    let report = kernel.ledger.release_with_world(
        &handle,
        &mut HostWorld {
            inner: inner.clone(),
        },
        now,
    )?;
    let response = match report.pending.first() {
        None => ReleaseResponse::Released,
        Some(t) => {
            let id = t.id.get();
            match &t.state {
                CleanupState::Retryable(reason) => ReleaseResponse::Retryable {
                    cleanup_id: id,
                    reason: reason.clone(),
                },
                CleanupState::Unknown(reason) => ReleaseResponse::Unknown {
                    cleanup_id: id,
                    reason: reason.clone(),
                },
                CleanupState::Blocked(reason) => ReleaseResponse::Blocked {
                    cleanup_id: id,
                    reason: reason.clone(),
                },
                _ => ReleaseResponse::Pending { cleanup_id: id },
            }
        }
    };
    let _=kernel.audit.lock().unwrap().append(json!({"event":"substrate.release","plugin":name,"holding":handle.id().get(),"result":response}));
    Ok(json!({"ok":response}))
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
    let caller = inner
        .plugins
        .lock()
        .unwrap()
        .get(name)
        .map(|p| p.holding.clone());
    if let Some(caller) = caller {
        if !matches!(req["op"].as_str(), Some("release" | "unsubscribe"))
            && !kernel
                .ledger
                .holding(caller.id())?
                .is_some_and(|h| h.state.is_active() && h.handle() == caller)
        {
            return Err(KernelError::Denied("plugin is retiring".into()));
        }
    }
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
                let target = inner
                    .routes
                    .lock()
                    .unwrap()
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
            if !kernel
                .ledger
                .holding(handle.holding.id())?
                .is_some_and(|h| h.state.is_active() && h.handle() == handle.holding)
            {
                return Err(KernelError::Denied("target plugin is retiring".into()));
            }
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
                        .map(|e| {
                            (
                                e.description.clone(),
                                e.schema.clone(),
                                e.character.as_ref().map(kind_label),
                                e.character.as_ref().map(CheckedVerb::bears_budget),
                            )
                        })
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
            let id = inner
                .next_sub
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
                .map_err(|_| KernelError::Denied("subscription IDs exhausted".into()))?;
            // A standing inbound channel is a holding, child of the plugin's
            // own holding: teardown unsubscribes before it kills.
            let parent = inner
                .plugins
                .lock()
                .unwrap()
                .get(name)
                .map(|h| h.holding.clone());
            let subject = format!("plugin:{name}");
            let mut subs = inner.subs.lock().unwrap();
            let holding = kernel.ledger.hold_managed(
                ExclusiveRequest {
                    owner: SubjectId::new(&subject),
                    resource: ResourceKey::new(
                        ClassId::new(CLASS_SUBSCRIPTION),
                        InstanceId::new(format!("{}/{}", inner.witness.session(), id)),
                    ),
                    generation: inner.witness.session().clone(),
                    parent: parent,
                    lease: LeaseRequest::UseClassDefault,
                },
                CleanupTarget::Subscription {
                    host: inner.witness.clone(),
                    subscription: id,
                },
                None,
                Timestamp::try_from(now).map_err(crate::ledger::map_err)?,
            )?;
            subs.push(Sub {
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
                    let report = kernel.ledger.release_with_world(
                        &h,
                        &mut HostWorld {
                            inner: inner.clone(),
                        },
                        Timestamp::try_from(now).expect("system timestamp in range"),
                    )?;
                    report.pending.is_empty()
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
    use crate::ledger::CLASS_PORT;
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
                .released_at()
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
        assert_eq!(released, json!({"ok":{"released":true,"state":"retired"}}));
        assert!(
            host.kernel
                .ledger
                .holding(id)
                .unwrap()
                .unwrap()
                .released_at()
                .is_some()
        );
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn release_response_stays_pending_until_port_absence_is_confirmed() {
        let (host, root) = host("m2-port-response");
        let listener = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let held = op_hold(
            &host.kernel,
            &host.inner,
            "test",
            &json!({"class":CLASS_PORT,"instance":format!("tcp:{port}"),"substrate":{}}),
            100,
        )
        .unwrap();
        let request = held["ok"].clone();
        let first = op_release(&host.kernel, &host.inner, "test", &request, 101).unwrap();
        assert_eq!(first["ok"]["released"], false);
        assert_eq!(first["ok"]["state"], "retryable");
        let id = first["ok"]["cleanup_id"].clone();
        let key = host.kernel.ledger.cleanup_tasks().unwrap()[0].key.clone();
        assert!(op_renew(&host.kernel, "test", &request, 102).is_err());
        let repeated = op_release(&host.kernel, &host.inner, "test", &request, 103).unwrap();
        assert_eq!(repeated["ok"]["cleanup_id"], id);
        assert_eq!(host.kernel.ledger.cleanup_tasks().unwrap()[0].key, key);
        drop(listener);
        let completed = op_release(&host.kernel, &host.inner, "test", &request, 104).unwrap();
        assert_eq!(completed["ok"]["released"], true);
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_uses_the_subscription_session_not_just_its_numeric_id() {
        let (first, root) = host("m2-subscription-session");
        let (id, old_rx) = first.subscribe_local("topic").unwrap();
        let old = first
            .inner
            .subs
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.id == id)
            .unwrap()
            .holding
            .clone();
        first
            .kernel
            .ledger
            .request_retirement(&old, Timestamp::ZERO)
            .unwrap();
        let second = Host::new(first.kernel.clone(), &root.join("second-sockets")).unwrap();
        let (new_id, new_rx) = second.subscribe_local("topic").unwrap();
        assert_eq!(id, new_id);
        let report = first
            .kernel
            .ledger
            .release_with_world(
                &old,
                &mut HostWorld {
                    inner: second.inner.clone(),
                },
                Timestamp::ZERO,
            )
            .unwrap();
        assert!(report.pending.is_empty());
        assert_eq!(second.emit("topic", json!("new")), 1);
        assert_eq!(new_rx.recv().unwrap()["data"], "new");
        assert!(old_rx.try_recv().is_err());
        assert_eq!(first.emit("topic", json!("old")), 0);
        drop(first);
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod m3_metadata_tests {
    use super::*;

    #[test]
    fn malformed_character_fields_are_not_defaulted() {
        for meta in [
            json!({"kind": 1}),
            json!({"kind": "emitting", "amortizable": "false"}),
            json!({"kind": "consuming", "world": false}),
            json!({"kind": "repeatable", "idempotent": "true"}),
            json!({"kind": "repeatable", "commutes": 1}),
            json!({"kind": "repeatable", "degrade": 0}),
            json!({"kind": "emitting", "compensate_with": false}),
            json!({"commutes": true}),
        ] {
            assert!(kind_from_meta(&meta).is_err(), "{meta}");
        }
        for hello in [
            json!({"name": "p", "verbs": ["v", 1]}),
            json!({"name": "p", "verbs": ["v", "v"]}),
            json!({"name": "p"}),
        ] {
            assert!(declaration_identity(&hello).is_err());
        }
    }

    #[test]
    fn checked_character_preserves_legacy_defaults_and_hard_list() {
        let verbs = vec!["emit".into(), "send".into(), "legacy".into()];
        let class = check_class_declaration(
            "p",
            &verbs,
            &json!({
                "emit": {"kind": "emitting"}, "send": {"kind": "emitting", "amortizable": false},
            }),
            &json!({}),
        )
        .unwrap();
        assert!(!class.lookup(&VerbId::new("emit")).unwrap().withhold());
        assert!(class.lookup(&VerbId::new("send")).unwrap().withhold());
        assert!(class.lookup(&VerbId::new("legacy")).is_err());
        assert!(
            check_class_declaration("p", &verbs, &json!({}), &json!({"holding_rho": 1})).is_err()
        );
    }

    #[test]
    fn invalid_protocols_and_dangling_relations_cannot_be_published() {
        for protocol in [
            json!({"initial": "s", "transitions": false}),
            json!({"initial": "s", "transitions": [["s", "read", "s", "extra"]]}),
            json!({"initial": "s", "transitions": [["s", "read", "s"], ["s", "read", "other"]]}),
        ] {
            assert!(protocol_from_json(Some(&protocol)).is_err());
        }
        assert!(
            check_class_declaration(
                "p",
                &["read".into()],
                &json!({"read": {"kind": "repeatable", "degrade": "unknown"}}),
                &json!({})
            )
            .is_err()
        );
    }
}
