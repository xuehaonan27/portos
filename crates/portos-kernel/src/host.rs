//! Plugin host — ABI v2.
//!
//! A plugin is a plain child process (no sandbox yet) that connects back to a
//! per-spawn UDS **twice**, authenticating both connections with a spawn
//! token from the environment:
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
//!     in `*` for prefix matching), and `put`/`read` (artifact dereference
//!     as chunked byte streams; see `portos_proto::chunk`).
//!   - an optional **events** channel: one-way kernel→plugin event
//!     deliveries. A plugin that declares it can receive subscribed events
//!     *while one of its own verbs is mid-call* — the serve channel is busy
//!     then, and without a separate channel a plugin awaiting an event
//!     stream inside a call handler would deadlock (the model driver's SSE
//!     consumption is exactly that shape). Plugins that don't declare it get
//!     events interleaved on the serve channel as before.
//!
//! Each channel is strict in a single direction, which keeps the sync-thread
//! model trivial: no frame multiplexing anywhere.
//!
//! Every frame crossing these channels is one of the types in
//! `portos_proto::wire`, so a malformed frame is refused at the boundary
//! instead of defaulting into a plausible-looking request. Verb *payloads*
//! stay opaque: this module moves [`Payload`] bytes and has no way to read a
//! field out of them, which makes domain ignorance structural rather than a
//! rule to remember. The capability convention for invoke is subject
//! `plugin:<name>`, resource `driver:<family>`, verb the short name.
//!
//! Known limitation: the invoke graph must be acyclic. A cycle (A invokes B
//! while B's serve loop is blocked invoking A) deadlocks; current flows
//! (cli → model driver → {broker, browser}) are acyclic by construction.

use crate::{Kernel, KernelError};
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use portos_proto::ids::{PluginName, SubId, Topic, Verb};
use portos_proto::wire::{
    Call, ChannelRole, ClientOp, EmitReply, Event, GrantsReply, Hello, HelloFrame, LocalEvent,
    Payload, PutReply, ReadReply, Reply, ServeMsg, SubscribeReply, ToolMeta, UnsubscribeReply,
};
use portos_proto::{ABI_VERSION, chunk, frame};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

/// Bounded event queue per subscriber. A subscriber that falls this far
/// behind is cut off, because a slow consumer must never stall the kernel:
/// plugin subscribers are disconnected, local subscribers dropped.
pub const EVENT_QUEUE: usize = 256;

const SPAWN_DEADLINE_MS: u64 = 10_000;

/// How long each teardown step waits before escalating.
const GRACE_MS: u64 = 1_000;

struct PluginHandle {
    child: Mutex<std::process::Child>,
    serve: Mutex<UnixStream>,
    /// Dedicated event-delivery stream, when the plugin declared one.
    /// Without it, events interleave on the serve channel.
    events: Option<Mutex<UnixStream>>,
    events_tx: SyncSender<ServeMsg>,
    sock_path: PathBuf,
}

enum SubTarget {
    Local(SyncSender<LocalEvent>),
    Plugin(PluginName),
}

struct Sub {
    id: SubId,
    pattern: Topic,
    target: SubTarget,
}

/// A routed verb: which plugin serves it, plus the model-facing metadata the
/// plugin advertised in its hello (opaque to the kernel — stored and joined,
/// never interpreted).
struct RouteEntry {
    plugin: PluginName,
    meta: ToolMeta,
}

struct HostInner {
    plugins: Mutex<BTreeMap<PluginName, Arc<PluginHandle>>>,
    routes: Mutex<BTreeMap<Verb, RouteEntry>>,
    subs: Mutex<Vec<Sub>>,
    next_sub: AtomicU64,
    next_spawn: AtomicU64,
    sock_dir: PathBuf,
    meter: Mutex<crate::metrics::ContextMeter>,
}

/// The plugin host: spawn, route, event bus, artifact channel.
pub struct Host {
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
}

impl Host {
    pub fn new(kernel: Arc<Kernel>, sock_dir: &Path) -> Result<Host, KernelError> {
        std::fs::create_dir_all(sock_dir)?;
        Ok(Host {
            kernel,
            inner: Arc::new(HostInner {
                plugins: Mutex::new(BTreeMap::new()),
                routes: Mutex::new(BTreeMap::new()),
                subs: Mutex::new(Vec::new()),
                next_sub: AtomicU64::new(1),
                next_spawn: AtomicU64::new(1),
                sock_dir: sock_dir.to_path_buf(),
                meter: Mutex::new(crate::metrics::ContextMeter::default()),
            }),
        })
    }

    /// Spawn `bin args…` with `envs` added, wait for every declared hello,
    /// register the plugin's verbs, and start its service threads. Returns
    /// the name the plugin declared.
    pub fn spawn(
        &self,
        bin: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<PluginName, KernelError> {
        let idx = self.inner.next_spawn.fetch_add(1, Ordering::SeqCst);
        let sock_path = self
            .inner
            .sock_dir
            .join(format!("plugin-{}-{idx}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path)?;
        listener.set_nonblocking(true)?;
        let token = rand_token();

        let mut cmd = std::process::Command::new(bin);
        cmd.args(args)
            .env("PORTOS_PLUGIN_SOCK", &sock_path)
            .env("PORTOS_PLUGIN_TOKEN", &token)
            // Each plugin leads its own process group, so teardown can reach
            // whatever it started. A driver's real cost is usually its
            // grandchildren — the browser driver's chromium, a shell driver's
            // pipeline — and `Child::kill` never sees those.
            .process_group(0);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;

        // The serve connection comes first and declares which extra channels
        // follow ("client" always; "events" optionally). A frame that is not
        // a well-formed hello, or a bad token, is fatal for the spawn.
        let accept_hello =
            |child: &mut std::process::Child| -> Result<(UnixStream, Hello), KernelError> {
                let mut stream = accept_with_deadline(&listener, child, SPAWN_DEADLINE_MS)?;
                let frame: HelloFrame = frame::read_frame(&mut stream)
                    .map_err(|e| KernelError::Corrupt(format!("hello: {e}")))?;
                if frame.hello.token != token {
                    let _ = frame::write_frame(&mut stream, &Reply::<()>::Err("bad token".into()));
                    return Err(KernelError::Denied("plugin hello: bad token".into()));
                }
                frame::write_frame(&mut stream, &Reply::Ok(Payload::null()))
                    .map_err(|e| KernelError::Corrupt(format!("hello ack: {e}")))?;
                Ok((stream, frame.hello))
            };

        let (serve_stream, hello) = match accept_hello(&mut child) {
            Ok((stream, h)) if h.role == ChannelRole::Serve => (stream, h),
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
        if hello.abi != ABI_VERSION {
            let _ = child.kill();
            return Err(KernelError::Denied(format!(
                "plugin abi {} != kernel abi {ABI_VERSION}",
                hello.abi
            )));
        }
        let name = hello.name.clone();
        let verbs = hello.verbs.clone();
        let mut tools_meta = hello.tools.clone().unwrap_or_default();
        let mut expected = hello.declared_channels();

        if !expected.contains(&ChannelRole::Client) {
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
            match expected.iter().position(|c| *c == h.role) {
                Some(i) => {
                    expected.remove(i);
                    match h.role {
                        ChannelRole::Client => client = Some(stream),
                        ChannelRole::Events => events = Some(stream),
                        ChannelRole::Serve => {
                            let _ = child.kill();
                            return Err(KernelError::Corrupt(
                                "plugin hello: duplicate serve channel".into(),
                            ));
                        }
                    }
                }
                None => {
                    let _ = child.kill();
                    return Err(KernelError::Corrupt(format!(
                        "plugin hello: undeclared or duplicate role {:?}",
                        h.role
                    )));
                }
            }
        }
        let client_stream = client.expect("client channel present");

        // Register verbs; a route conflict aborts the spawn.
        {
            let mut plugins = self.inner.plugins.lock().unwrap();
            let mut routes = self.inner.routes.lock().unwrap();
            if plugins.contains_key(&name) {
                let _ = child.kill();
                return Err(KernelError::Denied(format!("plugin name taken: {name}")));
            }
            if let Some(v) = verbs.iter().find(|v| routes.contains_key(*v)) {
                let _ = child.kill();
                return Err(KernelError::Denied(format!("verb already routed: {v}")));
            }
            let (events_tx, events_rx) = sync_channel::<ServeMsg>(EVENT_QUEUE);
            let handle = Arc::new(PluginHandle {
                child: Mutex::new(child),
                serve: Mutex::new(serve_stream),
                events: events.map(Mutex::new),
                events_tx,
                sock_path: sock_path.clone(),
            });
            for v in &verbs {
                let meta = tools_meta.remove(v).unwrap_or_default();
                routes.insert(
                    v.clone(),
                    RouteEntry {
                        plugin: name.clone(),
                        meta,
                    },
                );
            }
            plugins.insert(name.clone(), handle.clone());
            spawn_event_pump(handle.clone(), events_rx);
            spawn_client_loop(
                self.kernel.clone(),
                self.inner.clone(),
                name.clone(),
                client_stream,
            );
        }

        self.audit(json!({
            "event": "plugin.spawned",
            "plugin": name.as_str(),
            "verbs": verbs.iter().map(Verb::as_str).collect::<Vec<_>>(),
        }));
        Ok(name)
    }

    /// Kernel-initiated verb call on a named plugin. No capability check:
    /// kernel-side callers act with root authority.
    pub fn call(
        &self,
        plugin: &PluginName,
        verb: &Verb,
        args: Payload,
    ) -> Result<Payload, KernelError> {
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
    pub fn call_verb(&self, verb: &Verb, args: Payload) -> Result<Payload, KernelError> {
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

    /// Subscribe an in-process consumer (the chat loop) to a topic pattern.
    pub fn subscribe_local(&self, pattern: &Topic) -> (SubId, Receiver<LocalEvent>) {
        let (tx, rx) = sync_channel::<LocalEvent>(EVENT_QUEUE);
        let id = SubId::new(self.inner.next_sub.fetch_add(1, Ordering::SeqCst));
        self.inner.subs.lock().unwrap().push(Sub {
            id,
            pattern: pattern.clone(),
            target: SubTarget::Local(tx),
        });
        (id, rx)
    }

    /// Kernel-side event publish. Returns how many subscribers it reached.
    pub fn emit(&self, topic: &Topic, data: Payload) -> u64 {
        dispatch_event(&self.kernel, &self.inner, topic, &data)
    }

    /// Persist every event matching `pattern` to the audit chain. This is how
    /// a trusted plugin's self-reported log (e.g. the egress broker's
    /// `egress::log`) becomes tamper-evident: the plugin emits, the kernel
    /// subscribes and appends.
    pub fn audit_topic(&self, pattern: &Topic) {
        let (_id, rx) = self.subscribe_local(pattern);
        let kernel = self.kernel.clone();
        std::thread::spawn(move || {
            for ev in rx {
                let data: serde_json::Value = ev.data.parse().unwrap_or(serde_json::Value::Null);
                let _ = kernel.audit.lock().unwrap().append(json!({
                    "event": "topic.audit",
                    "topic": ev.topic.as_str(),
                    "data": data,
                }));
            }
        });
    }

    /// (context_bytes, data_bytes) moved through plugin channels so far.
    pub fn meter(&self) -> (u64, u64) {
        let m = self.inner.meter.lock().unwrap();
        (m.context_bytes, m.data_bytes)
    }

    /// Shut a plugin down, escalating until it is actually gone.
    ///
    /// Asking politely is the first step, not the mechanism: a plugin that
    /// ignores `shutdown`, or that leaves a listening socket holding its
    /// runtime alive, still goes — and so does everything it started, because
    /// the signals go to its process group rather than to it alone.
    pub fn shutdown(&self, plugin: &PluginName) {
        let handle = { self.inner.plugins.lock().unwrap().remove(plugin) };
        let Some(h) = handle else { return };
        cleanup_plugin(&self.inner, plugin);
        if let Ok(mut s) = h.serve.lock() {
            let _ = frame::write_frame(&mut *s, &ServeMsg::Shutdown);
        }
        let mut child = h.child.lock().unwrap();
        let pgid = child.id();
        if !wait_for_exit(&mut child, GRACE_MS) {
            signal_group(pgid, Signal::SIGTERM);
            if !wait_for_exit(&mut child, GRACE_MS) {
                signal_group(pgid, Signal::SIGKILL);
                let _ = wait_for_exit(&mut child, GRACE_MS);
            }
        }
        // The leader is reaped; anything left in its group is not a child of
        // ours and cannot be waited on, so the final KILL is unconditional.
        let _ = child.kill();
        let _ = child.wait();
        signal_group(pgid, Signal::SIGKILL);
        let _ = std::fs::remove_file(&h.sock_path);
    }

    pub fn shutdown_all(&self) {
        let names: Vec<PluginName> = self.inner.plugins.lock().unwrap().keys().cloned().collect();
        for n in &names {
            self.shutdown(n);
        }
    }

    fn audit(&self, body: serde_json::Value) {
        let _ = self.kernel.audit.lock().unwrap().append(body);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

/// One serve-channel request/response under the channel lock. Holding the
/// lock across write+read is what keeps the channel unmultiplexed.
fn call_on(handle: &PluginHandle, verb: &Verb, args: Payload) -> Result<Payload, KernelError> {
    let mut s = handle.serve.lock().unwrap();
    let msg = ServeMsg::Call(Call {
        verb: verb.clone(),
        args,
    });
    frame::write_frame(&mut *s, &msg)
        .map_err(|e| KernelError::Corrupt(format!("call write: {e}")))?;
    let reply: Reply<Payload> =
        frame::read_frame(&mut *s).map_err(|e| KernelError::Corrupt(format!("call read: {e}")))?;
    reply
        .into_result()
        .map_err(|e| KernelError::Denied(format!("plugin error: {e}")))
}

/// Per-plugin thread draining the bounded event queue onto the plugin's
/// events channel (or the serve channel, when it declared none).
fn spawn_event_pump(handle: Arc<PluginHandle>, rx: Receiver<ServeMsg>) {
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

/// Why a delivery failed: the queue is full (and, for a plugin, whose), or
/// the subscriber is gone.
enum Overflow {
    Full(Option<PluginName>),
    Gone,
}

/// Deliver an event to every matching subscriber. Overflow policy: a local
/// subscriber is dropped; a plugin subscriber's whole connection is cut —
/// never let a slow consumer wedge the kernel.
fn dispatch_event(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    topic: &Topic,
    data: &Payload,
) -> u64 {
    enum Target {
        Local(SubId, SyncSender<LocalEvent>),
        Plugin(SubId, PluginName),
    }
    let targets: Vec<Target> = {
        let subs = inner.subs.lock().unwrap();
        subs.iter()
            .filter(|s| s.pattern.matches(topic))
            .map(|s| match &s.target {
                SubTarget::Local(tx) => Target::Local(s.id, tx.clone()),
                SubTarget::Plugin(name) => Target::Plugin(s.id, name.clone()),
            })
            .collect()
    };
    let mut delivered = 0u64;
    let mut drop_subs: Vec<SubId> = Vec::new();
    let mut kill_plugins: Vec<PluginName> = Vec::new();
    for t in targets {
        let (id, sent) = match t {
            Target::Local(id, tx) => {
                let ev = LocalEvent {
                    sub: id,
                    topic: topic.clone(),
                    data: data.clone(),
                };
                let sent = tx.try_send(ev).map_err(|e| match e {
                    TrySendError::Full(_) => Overflow::Full(None),
                    TrySendError::Disconnected(_) => Overflow::Gone,
                });
                (id, sent)
            }
            Target::Plugin(id, name) => {
                let tx = inner
                    .plugins
                    .lock()
                    .unwrap()
                    .get(&name)
                    .map(|h| h.events_tx.clone());
                let Some(tx) = tx else {
                    drop_subs.push(id);
                    continue;
                };
                let ev = ServeMsg::Event(Event {
                    sub: id,
                    topic: topic.clone(),
                    data: data.clone(),
                });
                let sent = tx.try_send(ev).map_err(|e| match e {
                    TrySendError::Full(_) => Overflow::Full(Some(name)),
                    TrySendError::Disconnected(_) => Overflow::Gone,
                });
                (id, sent)
            }
        };
        match sent {
            Ok(()) => delivered += 1,
            Err(Overflow::Gone) => drop_subs.push(id),
            Err(Overflow::Full(plugin)) => {
                drop_subs.push(id);
                let _ = kernel.audit.lock().unwrap().append(json!({
                    "event": "events.overflow",
                    "sub": id.get(),
                    "topic": topic.as_str(),
                    "plugin": plugin.as_ref().map(PluginName::as_str),
                }));
                if let Some(name) = plugin {
                    kill_plugins.push(name);
                }
            }
        }
    }
    if !drop_subs.is_empty() {
        inner
            .subs
            .lock()
            .unwrap()
            .retain(|s| !drop_subs.contains(&s.id));
    }
    for name in kill_plugins {
        if let Some(h) = inner.plugins.lock().unwrap().remove(&name) {
            cleanup_plugin(inner, &name);
            let _ = h.child.lock().unwrap().kill();
        }
    }
    delivered
}

fn cleanup_plugin(inner: &Arc<HostInner>, name: &PluginName) {
    inner
        .routes
        .lock()
        .unwrap()
        .retain(|_, entry| &entry.plugin != name);
    inner.subs.lock().unwrap().retain(|s| match &s.target {
        SubTarget::Plugin(n) => n != name,
        _ => true,
    });
}

/// The client-channel loop: serve one plugin's kernel requests until EOF.
fn spawn_client_loop(
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
    name: PluginName,
    mut stream: UnixStream,
) {
    std::thread::spawn(move || {
        loop {
            let bytes = match frame::read_bytes(&mut stream) {
                Ok(b) => b,
                Err(_) => return, // plugin went away; spawn/shutdown owns cleanup
            };
            inner
                .meter
                .lock()
                .unwrap()
                .count_context(bytes.len() as u64);
            let outcome = match ClientOp::from_slice(&bytes) {
                Ok(op) => handle_client_op(&kernel, &inner, &name, op, &mut stream),
                Err(e) => Err(KernelError::Corrupt(e.to_string())),
            };
            let resp = match outcome {
                Ok(Some(body)) => body,
                Ok(None) => continue, // the op wrote its own response
                Err(e) => serde_json::to_vec(&Reply::<()>::Err(e.to_string()))
                    .expect("error reply serializes"),
            };
            inner.meter.lock().unwrap().count_context(resp.len() as u64);
            if frame::write_bytes(&mut stream, &resp).is_err() {
                return;
            }
        }
    });
}

/// Encode a successful reply body. `Ok(None)` from an op means it already
/// wrote its own response frame, as the streaming read does.
fn ok<T: Serialize>(body: &T) -> Result<Option<Vec<u8>>, KernelError> {
    serde_json::to_vec(&Reply::Ok(body))
        .map(Some)
        .map_err(|e| KernelError::Corrupt(format!("reply encode: {e}")))
}

fn handle_client_op(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    name: &PluginName,
    op: ClientOp,
    stream: &mut UnixStream,
) -> Result<Option<Vec<u8>>, KernelError> {
    let now = crate::db::now_unix();
    let subject = name.subject();
    match op {
        // ---- invoke: the capability-gated plugin→plugin path ----
        ClientOp::Invoke(req) => {
            let verb = req.verb;
            let cap =
                match kernel
                    .caps
                    .find_and_exercise(&subject, &verb.resource(), verb.short(), now)
                {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "invoke.denied", "from": name.as_str(),
                            "verb": verb.as_str(), "reason": e.to_string(),
                        }));
                        return Err(e);
                    }
                };
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "invoke.allowed", "from": name.as_str(),
                "verb": verb.as_str(), "cap": cap,
            }));
            let handle = {
                let routes = inner.routes.lock().unwrap();
                let target = routes
                    .get(&verb)
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
            ok(&call_on(&handle, &verb, req.args)?)
        }

        // ---- grants introspection: what may *this* plugin invoke? ----
        // The capability table knows the verbs and budgets; the route table
        // knows what each driver said about them. Joining the two hands the
        // caller a ready-made tool definition — the driver owns descriptions
        // and schemas, the user owns grants, the kernel owns neither.
        ClientOp::Grants => {
            let grants = kernel.caps.grants_for(&subject, now, |verb| {
                inner
                    .routes
                    .lock()
                    .unwrap()
                    .get(verb)
                    .map(|e| e.meta.clone())
            })?;
            ok(&GrantsReply { grants })
        }

        // ---- event bus ----
        ClientOp::Emit(req) => {
            let delivered = dispatch_event(kernel, inner, &req.topic, &req.data);
            ok(&EmitReply { delivered })
        }
        ClientOp::Subscribe(req) => {
            let id = SubId::new(inner.next_sub.fetch_add(1, Ordering::SeqCst));
            inner.subs.lock().unwrap().push(Sub {
                id,
                pattern: req.topic.clone(),
                target: SubTarget::Plugin(name.clone()),
            });
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "events.subscribed", "plugin": name.as_str(),
                "topic": req.topic.as_str(), "sub": id.get(),
            }));
            ok(&SubscribeReply { sub: id })
        }
        ClientOp::Unsubscribe(req) => {
            // A plugin can only drop its own subscriptions.
            let removed = {
                let mut subs = inner.subs.lock().unwrap();
                let before = subs.len();
                subs.retain(|s| {
                    !(s.id == req.sub && matches!(&s.target, SubTarget::Plugin(n) if n == name))
                });
                before != subs.len()
            };
            ok(&UnsubscribeReply { removed })
        }

        // ---- artifact ingest: frame, then chunk stream ----
        ClientOp::Put(req) => {
            let labels = req.labels.unwrap_or_default();
            let mut reader = chunk::ChunkReader::new(&mut *stream);
            let result = kernel
                .cas
                .put_stream(&mut reader, &req.content_type, labels, &subject);
            // Resync the stream even on a CAS error so the channel survives.
            reader
                .drain()
                .map_err(|e| KernelError::Corrupt(format!("put drain: {e}")))?;
            let meta = result?;
            inner.meter.lock().unwrap().count_data(meta.size);
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "artifact.put", "plugin": name.as_str(),
                "id": meta.id, "size": meta.size,
            }));
            ok(&PutReply { meta })
        }

        // ---- artifact dereference: response frame, then chunk stream ----
        ClientOp::Read(req) => {
            // Reads are free but accounted: audit before bytes move.
            let meta = kernel.cas.meta(&req.id)?;
            let avail = meta.size.saturating_sub(req.offset);
            let n = req.len.map(|w| w.min(avail)).unwrap_or(avail);
            let mut f = kernel.cas.open_read(&req.id)?;
            f.seek(SeekFrom::Start(req.offset))?;
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "artifact.read", "plugin": name.as_str(), "id": req.id,
                "offset": req.offset, "len": n,
            }));
            let written = frame::write_frame(stream, &Reply::Ok(ReadReply { len: n }))
                .map_err(|e| KernelError::Corrupt(format!("read resp: {e}")))?;
            inner.meter.lock().unwrap().count_context(written);
            let mut taken = f.take(n);
            let moved = chunk::copy_into_chunks(&mut taken, stream)
                .map_err(|e| KernelError::Corrupt(format!("read stream: {e}")))?;
            inner.meter.lock().unwrap().count_data(moved);
            Ok(None) // response already sent
        }
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

/// Signal a plugin's whole process group. The group id equals the plugin's
/// own pid because it was spawned as a group leader; an error here means the
/// group is already gone, which is the outcome we wanted.
fn signal_group(pgid: u32, sig: Signal) {
    let _ = nix::sys::signal::killpg(Pid::from_raw(pgid as i32), sig);
}

/// Poll for exit up to `ms`. Returns whether the child is gone.
fn wait_for_exit(child: &mut std::process::Child, ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn rand_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}
