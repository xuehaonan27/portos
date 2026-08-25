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
//!     the calling plugin, audited, routed), `emit`/`subscribe`/`unsubscribe`
//!     (event bus), and `put`/`read` (artifact dereference as chunked byte
//!     streams; see `portos_proto::chunk`). fd passing is gone (D25).
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

use crate::{Kernel, KernelError};
use portos_proto::{Label, chunk, frame};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
}

enum SubTarget {
    Local(SyncSender<Value>),
    Plugin(String),
}

struct Sub {
    id: u64,
    topic: String,
    target: SubTarget,
}

struct HostInner {
    plugins: Mutex<BTreeMap<String, Arc<PluginHandle>>>,
    routes: Mutex<BTreeMap<String, String>>, // verb -> plugin name
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

    /// Spawn `bin args…` with `envs` added, wait for both hellos, register
    /// the plugin's verbs, and start its service threads. Returns the plugin
    /// name (from its hello).
    pub fn spawn(
        &self,
        bin: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<String, KernelError> {
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
            .env("PORTOS_PLUGIN_TOKEN", &token);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;

        // The serve connection comes first and declares which extra channels
        // follow ("client" always; "events" optionally). Bad token or an
        // undeclared/duplicate role is fatal for the spawn.
        let mut accept_hello = |child: &mut std::process::Child| -> Result<(UnixStream, Value), KernelError> {
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

        let (serve_stream, name, verbs, mut expected) =
            match accept_hello(&mut child) {
                Ok((stream, h)) if h["role"] == "serve" => {
                    let name = h["name"].as_str().unwrap_or("?").to_string();
                    let verbs: Vec<String> = h["verbs"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let channels: Vec<String> = h["channels"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_else(|| vec!["client".to_string()]);
                    (stream, name, verbs, channels)
                }
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
            let (events_tx, events_rx) = sync_channel::<Value>(EVENT_QUEUE);
            let handle = Arc::new(PluginHandle {
                child: Mutex::new(child),
                serve: Mutex::new(serve_stream),
                events: events.map(Mutex::new),
                events_tx,
                sock_path: sock_path.clone(),
            });
            for v in &verbs {
                routes.insert(v.clone(), name.clone());
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
            "event": "plugin.spawned", "plugin": name, "verbs": verbs,
        }));
        Ok(name)
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
            .cloned()
            .ok_or_else(|| KernelError::NotFound(format!("no route for verb: {verb}")))?;
        self.call(&target, verb, args)
    }

    /// Subscribe an in-process consumer (the CLI / model-facing loop) to a
    /// topic. Exact-match topics for now.
    pub fn subscribe_local(&self, topic: &str) -> (u64, Receiver<Value>) {
        let (tx, rx) = sync_channel::<Value>(EVENT_QUEUE);
        let id = self.inner.next_sub.fetch_add(1, Ordering::SeqCst);
        self.inner.subs.lock().unwrap().push(Sub {
            id,
            topic: topic.to_string(),
            target: SubTarget::Local(tx),
        });
        (id, rx)
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
    pub fn audit_topic(&self, topic: &str) {
        let (_id, rx) = self.subscribe_local(topic);
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
    }

    /// (context_bytes, data_bytes) moved through plugin channels so far.
    pub fn meter(&self) -> (u64, u64) {
        let m = self.inner.meter.lock().unwrap();
        (m.context_bytes, m.data_bytes)
    }

    /// Graceful-ish shutdown: send the shutdown op, give the plugin a moment,
    /// then make sure it is gone.
    pub fn shutdown(&self, plugin: &str) {
        let handle = { self.inner.plugins.lock().unwrap().remove(plugin) };
        if let Some(h) = handle {
            cleanup_plugin(&self.inner, plugin);
            if let Ok(mut s) = h.serve.lock() {
                let _ = frame::write_frame(&mut *s, &json!({"op": "shutdown"}));
            }
            let mut child = h.child.lock().unwrap();
            for _ in 0..20 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&h.sock_path);
        }
    }

    pub fn shutdown_all(&self) {
        let names: Vec<String> = self.inner.plugins.lock().unwrap().keys().cloned().collect();
        for n in names {
            self.shutdown(&n);
        }
    }

    fn audit(&self, body: Value) {
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
fn call_on(handle: &PluginHandle, verb: &str, args: Value) -> Result<Value, KernelError> {
    let mut s = handle.serve.lock().unwrap();
    frame::write_frame(&mut *s, &json!({"op": "call", "verb": verb, "args": args}))
        .map_err(|e| KernelError::Corrupt(format!("call write: {e}")))?;
    let resp = frame::read_frame(&mut *s)
        .map_err(|e| KernelError::Corrupt(format!("call read: {e}")))?;
    if let Some(err) = resp.get("err").and_then(|e| e.as_str()) {
        return Err(KernelError::Denied(format!("plugin error: {err}")));
    }
    Ok(resp.get("ok").cloned().unwrap_or(Value::Null))
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

/// Deliver an event to every matching subscriber. Overflow policy: a local
/// subscriber is dropped; a plugin subscriber's whole connection is cut
/// (m0 §3 — never let a slow consumer wedge the kernel).
fn dispatch_event(kernel: &Arc<Kernel>, inner: &Arc<HostInner>, topic: &str, data: Value) -> usize {
    enum Target {
        Local(u64, SyncSender<Value>),
        Plugin(u64, String),
    }
    let targets: Vec<Target> = {
        let subs = inner.subs.lock().unwrap();
        subs.iter()
            .filter(|s| s.topic == topic)
            .map(|s| match &s.target {
                SubTarget::Local(tx) => Target::Local(s.id, tx.clone()),
                SubTarget::Plugin(name) => Target::Plugin(s.id, name.clone()),
            })
            .collect()
    };
    let mut delivered = 0usize;
    let mut drop_subs: Vec<u64> = Vec::new();
    let mut kill_plugins: Vec<String> = Vec::new();
    for t in targets {
        match t {
            Target::Local(id, tx) => {
                let ev = json!({"topic": topic, "data": data, "sub": id});
                match tx.try_send(ev) {
                    Ok(()) => delivered += 1,
                    Err(TrySendError::Full(_)) => {
                        drop_subs.push(id);
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "events.overflow", "sub": id, "topic": topic,
                        }));
                    }
                    Err(TrySendError::Disconnected(_)) => drop_subs.push(id),
                }
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
                let ev = json!({"op": "event", "sub": id, "topic": topic, "data": data});
                match tx.try_send(ev) {
                    Ok(()) => delivered += 1,
                    Err(TrySendError::Full(_)) => {
                        drop_subs.push(id);
                        kill_plugins.push(name.clone());
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "events.overflow", "sub": id, "topic": topic,
                            "plugin": name,
                        }));
                    }
                    Err(TrySendError::Disconnected(_)) => drop_subs.push(id),
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

fn cleanup_plugin(inner: &Arc<HostInner>, name: &str) {
    inner
        .routes
        .lock()
        .unwrap()
        .retain(|_, target| target != name);
    inner.subs.lock().unwrap().retain(|s| match &s.target {
        SubTarget::Plugin(n) => n != name,
        _ => true,
    });
}

/// The client-channel loop: serve one plugin's kernel requests until EOF.
fn spawn_client_loop(
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
    name: String,
    mut stream: UnixStream,
) {
    std::thread::spawn(move || {
        loop {
            let req = match frame::read_frame(&mut stream) {
                Ok(r) => r,
                Err(_) => return, // plugin went away; spawn/shutdown owns cleanup
            };
            count_context(&inner, &req);
            let resp = handle_client_op(&kernel, &inner, &name, &req, &mut stream);
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

fn handle_client_op(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
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
            let cap = match kernel.caps.find_and_exercise(&subject, &resource, short, now) {
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
                    .ok_or_else(|| KernelError::NotFound(format!("no route for verb: {verb}")))?;
                inner
                    .plugins
                    .lock()
                    .unwrap()
                    .get(target)
                    .cloned()
                    .ok_or_else(|| KernelError::NotFound(format!("plugin gone: {target}")))?
            };
            let out = call_on(&handle, verb, args)?;
            Ok(json!({"ok": out}))
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
            inner.subs.lock().unwrap().push(Sub {
                id,
                topic: topic.clone(),
                target: SubTarget::Plugin(name.to_string()),
            });
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "events.subscribed", "plugin": name, "topic": topic, "sub": id,
            }));
            Ok(json!({"ok": {"sub": id}}))
        }
        Some("unsubscribe") => {
            // A plugin can only drop its own subscriptions.
            let id = req["sub"].as_u64().unwrap_or(0);
            let mut subs = inner.subs.lock().unwrap();
            let before = subs.len();
            subs.retain(|s| {
                !(s.id == id && matches!(&s.target, SubTarget::Plugin(n) if n == name))
            });
            let removed = before != subs.len();
            Ok(json!({"ok": {"removed": removed}}))
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
