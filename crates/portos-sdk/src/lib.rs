//! portos-sdk: the plugin side of the kernel IPC — ABI v2.
//!
//! A plugin is a plain process that connects to `$PORTOS_PLUGIN_SOCK`
//! **twice**, authenticating each connection with `$PORTOS_PLUGIN_TOKEN`:
//! a `serve` connection on which it declares its verbs and answers kernel
//! calls (and receives event deliveries), and a `client` connection through
//! which it reaches the kernel — `invoke` (call another plugin's verb,
//! capability-checked kernel-side), `emit`/`subscribe` (event bus), and
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
    hello(
        &mut wr,
        &mut rd,
        &json!({"hello": {
            "name": name, "abi": ABI_VERSION, "role": "serve",
            "token": token, "verbs": verbs,
            "channels": ["client", "events"],
        }}),
    )?;

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
