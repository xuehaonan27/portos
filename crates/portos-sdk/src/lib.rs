//! portos-sdk: the plugin side of the kernel IPC — ABI v2.
//!
//! A plugin is a plain process that connects to `$PORTOS_PLUGIN_SOCK`
//! **twice**, authenticating each connection with `$PORTOS_PLUGIN_TOKEN`:
//! a `serve` connection on which it declares its verbs and answers kernel
//! calls (and receives event deliveries), and a `client` connection through
//! which it reaches the kernel — `invoke` (call another plugin's verb,
//! capability-checked kernel-side), `emit`/`subscribe` (event bus),
//! `spawn_child` (parent/child instantiation, gated on `kernel:spawn`), and
//! `put`/`read` (artifact dereference as chunked byte streams; payloads
//! never ride inside JSON frames — decisions-v1.md D25).
//!
//! Plugins start with ZERO capabilities; `invoke` succeeds only for verbs
//! the kernel has been told to grant this plugin.

use portos_proto::{ABI_VERSION, chunk, frame};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

/// The plugin's connection to the kernel (the client channel). Safe to share
/// across threads; each operation holds the channel for one request/response
/// (chunk streams included), so requests never interleave.
pub struct KernelClient {
    stream: Mutex<UnixStream>,
}

impl KernelClient {
    fn request(&self, req: &Value) -> Result<Value, String> {
        let mut s = self.stream.lock().unwrap();
        frame::write_frame(&mut *s, req).map_err(|e| e.to_string())?;
        expect_ok(frame::read_frame(&mut *s).map_err(|e| e.to_string())?)
    }

    /// Call another plugin's verb through the kernel. The kernel checks this
    /// plugin's capabilities, audits, and routes.
    pub fn invoke(&self, verb: &str, args: Value) -> Result<Value, String> {
        self.request(&json!({"op": "invoke", "verb": verb, "args": args}))
    }

    /// Publish an event. Returns the number of subscribers it reached.
    pub fn emit(&self, topic: &str, data: Value) -> Result<u64, String> {
        let ok = self.request(&json!({"op": "emit", "topic": topic, "data": data}))?;
        Ok(ok["delivered"].as_u64().unwrap_or(0))
    }

    /// Subscribe to a topic. Matching events later arrive on the events
    /// channel and are handed to the plugin's event handler.
    pub fn subscribe(&self, topic: &str) -> Result<u64, String> {
        let ok = self.request(&json!({"op": "subscribe", "topic": topic}))?;
        ok["sub"].as_u64().ok_or_else(|| "no sub id".into())
    }

    /// Drop one of this plugin's subscriptions.
    pub fn unsubscribe(&self, sub: u64) -> Result<bool, String> {
        let ok = self.request(&json!({"op": "unsubscribe", "sub": sub}))?;
        Ok(ok["removed"].as_bool().unwrap_or(false))
    }

    /// Spawn a child plugin process (WP-04). The kernel gates this on the
    /// caller's `kernel:spawn` capability; the child's holding becomes a
    /// child of this plugin's own holding, so reclaiming this plugin tears
    /// the child down first. Returns the child's plugin name.
    pub fn spawn_child(&self, bin: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
        let env: serde_json::Map<String, Value> =
            env.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        let ok = self.request(&json!({
            "op": "spawn_child", "bin": bin, "args": args, "env": env,
        }))?;
        ok["name"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "no child name".into())
    }

    /// Submit an effect plan for admission (WP-06). The kernel admits and
    /// renders it; consent is given by the person out of band (the model
    /// never approves). Returns `{run_id, plan_hash, rendering, budget,
    /// needs, plan_path}`.
    pub fn plan_submit(&self, plan: Value, intent: &str) -> Result<Value, String> {
        self.request(&json!({"op": "plan.submit", "plan": plan, "intent": intent}))
    }

    /// Register a substrate holding (WP-02): a child process, port or lock
    /// file this plugin is responsible for. Restricted by the kernel to the
    /// built-in classes; the row is parented under this plugin's holding, so
    /// this plugin's death reclaims it children-first. `lease_secs` sets a
    /// per-holding lease (sweeper expiry). Returns `{id, generation}`.
    pub fn hold(
        &self,
        class: &str,
        instance: &str,
        substrate: Value,
        lease_secs: Option<u64>,
    ) -> Result<(u64, String), String> {
        let mut req = json!({"op": "hold", "class": class, "instance": instance, "substrate": substrate});
        if let Some(s) = lease_secs {
            req["lease_secs"] = json!(s);
        }
        let ok = self.request(&req)?;
        let id = ok["id"].as_u64().ok_or_else(|| "no holding id".to_string())?;
        let generation = ok["generation"].as_str().unwrap_or("").to_string();
        Ok((id, generation))
    }

    /// Give a holding back: the world side runs kernel-side (kill the
    /// witnessed process, remove the lock file), then the row tombstones.
    pub fn release(&self, id: u64, generation: &str) -> Result<bool, String> {
        let ok = self.request(&json!({"op": "release", "id": id, "generation": generation}))?;
        Ok(ok["released"].as_bool().unwrap_or(false))
    }

    /// Heartbeat a leased holding. With `lease_secs`, extend from now.
    pub fn renew(&self, id: u64, generation: &str, lease_secs: Option<u64>) -> Result<(), String> {
        let mut req = json!({"op": "renew", "id": id, "generation": generation});
        if let Some(s) = lease_secs {
            req["lease_secs"] = json!(s);
        }
        self.request(&req)?;
        Ok(())
    }

    /// What this plugin may invoke right now: live grants joined with the
    /// target verbs' advertised metadata — each entry
    /// `{verb, description, schema, counts_left?}`, ready to become a tool
    /// definition.
    pub fn grants(&self) -> Result<Vec<Value>, String> {
        let ok = self.request(&json!({"op": "grants"}))?;
        Ok(ok["grants"].as_array().cloned().unwrap_or_default())
    }

    /// Ingest a payload into the kernel CAS, streaming (never buffered whole,
    /// never inside a JSON frame). Returns the ArtifactMeta as JSON.
    pub fn put<R: Read>(&self, mut r: R, r#type: &str, labels: Value) -> Result<Value, String> {
        let mut s = self.stream.lock().unwrap();
        frame::write_frame(
            &mut *s,
            &json!({"op": "put", "type": r#type, "labels": labels}),
        )
        .map_err(|e| e.to_string())?;
        chunk::copy_into_chunks(&mut r, &mut *s).map_err(|e| e.to_string())?;
        let ok = expect_ok(frame::read_frame(&mut *s).map_err(|e| e.to_string())?)?;
        Ok(ok["meta"].clone())
    }

    /// Dereference (a range of) an artifact into `w`. Returns bytes moved.
    pub fn read_to<W: Write>(
        &self,
        id: &str,
        offset: u64,
        len: Option<u64>,
        w: &mut W,
    ) -> Result<u64, String> {
        let mut s = self.stream.lock().unwrap();
        let mut req = json!({"op": "read", "id": id, "offset": offset});
        if let Some(l) = len {
            req["len"] = json!(l);
        }
        frame::write_frame(&mut *s, &req).map_err(|e| e.to_string())?;
        expect_ok(frame::read_frame(&mut *s).map_err(|e| e.to_string())?)?;
        chunk::copy_from_chunks(&mut *s, w).map_err(|e| e.to_string())
    }

    /// Convenience: dereference a whole artifact into memory. Only for
    /// payloads the caller knows are small; streaming is the norm.
    pub fn read_bytes(&self, id: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        self.read_to(id, 0, None, &mut out)?;
        Ok(out)
    }
}

fn expect_ok(resp: Value) -> Result<Value, String> {
    if let Some(err) = resp.get("err").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    Ok(resp.get("ok").cloned().unwrap_or(Value::Null))
}

/// Connect all channels, declare `verbs`, and serve until the kernel says
/// shutdown (or goes away). `on_call` answers kernel calls and may use the
/// [`KernelClient`] it is handed — shared as an `Arc` so a handler can move a
/// clone into a background thread (e.g. to stream events after returning).
/// `on_event` receives subscribed events **on a dedicated thread** fed by the
/// events channel, so events keep flowing while a call handler is blocked —
/// which is what lets a handler await an event stream mid-call.
pub fn serve<F, G>(
    name: &str,
    verbs: &[&str],
    on_call: F,
    on_event: G,
) -> std::io::Result<()>
where
    F: FnMut(&str, &Value, &std::sync::Arc<KernelClient>) -> Result<Value, String>,
    G: FnMut(&str, &Value) + Send + 'static,
{
    serve_full(name, verbs, Value::Null, on_call, on_event)
}

/// Like [`serve`], additionally advertising per-verb tool metadata —
/// `{"family::verb": {"description": …, "schema": {…}}}` — which the kernel
/// stores with the routes and joins into `grants` introspection, so callers
/// holding a capability get ready-made tool definitions.
pub fn serve_full<F, G>(
    name: &str,
    verbs: &[&str],
    tools_meta: Value,
    on_call: F,
    on_event: G,
) -> std::io::Result<()>
where
    F: FnMut(&str, &Value, &std::sync::Arc<KernelClient>) -> Result<Value, String>,
    G: FnMut(&str, &Value) + Send + 'static,
{
    let extra = if tools_meta.is_null() { Value::Null } else { json!({"tools": tools_meta}) };
    serve_hello(name, verbs, extra, on_call, on_event)
}

/// Like [`serve_full`], with the whole declaration bundle: every key of
/// `hello_extra` is merged into the serve hello. Known keys:
///   - `tools`: per-verb `{description, schema, kind, world, compensate_with,
///     amortizable, idempotent, commutes, degrade, requires: {caps, deps}}` —
///     the verb character is checked by the kernel's F4 truth table at spawn,
///     `requires` by the F5 slot row;
///   - `holding_rho`: `inverse` | `compensable` | `external` — how this
///     plugin's holdings are given back (needed for held/transforming verbs);
///   - `protocol`: `{initial, transitions: [[from, "family::verb", to], …]}` —
///     a verb-order safety automaton the kernel enforces on calls (F6).
/// The wire identity keys (name, abi, role, token, verbs, channels) cannot be
/// overridden.
pub fn serve_hello<F, G>(
    name: &str,
    verbs: &[&str],
    hello_extra: Value,
    mut on_call: F,
    mut on_event: G,
) -> std::io::Result<()>
where
    F: FnMut(&str, &Value, &std::sync::Arc<KernelClient>) -> Result<Value, String>,
    G: FnMut(&str, &Value) + Send + 'static,
{
    let sock = std::env::var("PORTOS_PLUGIN_SOCK")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "PORTOS_PLUGIN_SOCK unset"))?;
    let token = std::env::var("PORTOS_PLUGIN_TOKEN").unwrap_or_default();

    let serve_stream = UnixStream::connect(&sock)?;
    let mut rd = serve_stream.try_clone()?;
    let mut wr = serve_stream.try_clone()?;
    let mut h = json!({"hello": {
        "name": name, "abi": ABI_VERSION, "role": "serve",
        "token": token, "verbs": verbs,
        "channels": ["client", "events"],
    }});
    if let Some(extra) = hello_extra.as_object() {
        const RESERVED: [&str; 6] = ["name", "abi", "role", "token", "verbs", "channels"];
        for (k, v) in extra {
            if !RESERVED.contains(&k.as_str()) {
                h["hello"][k] = v.clone();
            }
        }
    }
    hello(&mut wr, &mut rd, &h)?;

    let client_stream = UnixStream::connect(&sock)?;
    {
        let mut crd = client_stream.try_clone()?;
        let mut cwr = client_stream.try_clone()?;
        hello(
            &mut cwr,
            &mut crd,
            &json!({"hello": {
                "name": name, "abi": ABI_VERSION, "role": "client", "token": token,
            }}),
        )?;
    }
    let client = std::sync::Arc::new(KernelClient {
        stream: Mutex::new(client_stream),
    });

    let events_stream = UnixStream::connect(&sock)?;
    {
        let mut erd = events_stream.try_clone()?;
        let mut ewr = events_stream.try_clone()?;
        hello(
            &mut ewr,
            &mut erd,
            &json!({"hello": {
                "name": name, "abi": ABI_VERSION, "role": "events", "token": token,
            }}),
        )?;
    }
    std::thread::spawn(move || {
        let mut erd = events_stream;
        loop {
            let msg = match frame::read_frame(&mut erd) {
                Ok(m) => m,
                Err(_) => return, // kernel went away
            };
            if msg["op"] == "event" {
                on_event(msg["topic"].as_str().unwrap_or(""), &msg["data"]);
            }
        }
    });

    loop {
        let msg = match frame::read_frame(&mut rd) {
            Ok(m) => m,
            Err(_) => return Ok(()), // kernel went away; exit quietly
        };
        match msg["op"].as_str() {
            Some("shutdown") | None => return Ok(()),
            Some("call") => {
                let verb = msg["verb"].as_str().unwrap_or("");
                let args = msg.get("args").cloned().unwrap_or(Value::Null);
                let resp = match on_call(verb, &args, &client) {
                    Ok(v) => json!({"ok": v}),
                    Err(e) => json!({"err": e}),
                };
                frame::write_frame(&mut wr, &resp).map_err(io_err)?;
            }
            Some("event") => {} // events ride their own channel; tolerate strays
            Some(other) => {
                frame::write_frame(&mut wr, &json!({"err": format!("unknown op {other}")}))
                    .map_err(io_err)?;
            }
        }
    }
}

fn hello<W: Write, R: Read>(wr: &mut W, rd: &mut R, h: &Value) -> std::io::Result<()> {
    frame::write_frame(wr, h).map_err(io_err)?;
    let ack = frame::read_frame(rd).map_err(io_err)?;
    if let Some(err) = ack.get("err").and_then(|e| e.as_str()) {
        return Err(std::io::Error::other(format!("hello rejected: {err}")));
    }
    Ok(())
}

fn io_err(e: frame::FrameError) -> std::io::Error {
    std::io::Error::other(e.to_string())
}
