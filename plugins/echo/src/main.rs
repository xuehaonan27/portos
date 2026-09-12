//! portos-echo: the toy driver. Exists to exercise kernel mechanisms end to
//! end over ABI v2 — verb calls, chunked artifact streaming (put/read),
//! capability-gated invoke, the event bus, and the two-layer naming rule
//! (ephemeral refs live here, NOT in the kernel handle table).
//!
//! The verb family is `PORTOS_ECHO_FAMILY` (default "echo"), so one binary
//! can be spawned as several distinct plugins — which is exactly what the
//! invoke tests need (a plugin cannot invoke itself: single-threaded serve
//! loop, and invoke cycles deadlock by design).
//!
//! Its verbs take positional arguments, so each one parses the opaque
//! payload into a tuple of exactly the types it wants. That is the shape
//! every driver takes: the kernel moved bytes it could not read, and the
//! plugin that owns the meaning is the one that names the types.

use portos_abi::ids::{Topic, Verb};
use portos_abi::wire::Payload;
use portos_sdk::{CallError, CallResult, Plugin};
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::sync::{Arc, Mutex};

/// One event this plugin received, kept so a test can ask for them back.
#[derive(Clone, Serialize)]
struct Received {
    topic: String,
    data: Payload,
}

fn main() -> std::io::Result<()> {
    // A grandchild the kernel never sees, deliberately never reaped: it is
    // how a real driver's cost shows up (the browser driver's chromium), and
    // it is what teardown has to reach past this process to collect.
    if let Some(path) = std::env::var_os("PORTOS_ECHO_GRANDCHILD") {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("300");
        // `PORTOS_ECHO_GRANDCHILD_ESCAPES` makes it leave the process group
        // outright, which is the case a process group provably cannot
        // collect and a cgroup can. Real drivers do this without meaning to:
        // anything that daemonises calls `setsid`.
        if std::env::var_os("PORTOS_ECHO_GRANDCHILD_ESCAPES").is_some() {
            cmd.process_group(0);
        }
        let child = cmd.spawn()?;
        std::fs::write(path, child.id().to_string())?;
    }

    let family = std::env::var("PORTOS_ECHO_FAMILY").unwrap_or_else(|_| "echo".into());
    let name = format!("portos-{family}");

    // Shared because the verbs are separate closures now. That is not a
    // cost of the new shape so much as the old shape hiding the fact: two
    // verbs really do touch the same table, and one `match` arm mutating a
    // local made it look otherwise.
    let refs: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let next_ref = Arc::new(Mutex::new(0u32));
    let received: Arc<Mutex<Vec<Received>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = received.clone();

    let (make, used) = (refs.clone(), refs);
    let v = |short: &str| format!("{family}::{short}");

    // A fixture for dependency readiness: whatever is listed here is
    // something this echo cannot work without, the way modeld cannot work
    // without a gateway.
    let needs: Vec<Verb> = std::env::var("PORTOS_ECHO_NEEDS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| Verb::parse(s.trim()).expect("test verb"))
        .collect();
    let mut plugin = Plugin::new(&name);
    for need in &needs {
        plugin = plugin.needs(need);
    }

    portos_sdk::serve(
        plugin
            // The one verb with anything to say for itself, so grants
            // introspection has something to join against in tests.
            .tool(
                &v("emit"),
                "Print a line to the echo driver's stdout.",
                json!({"type": "object", "properties": {"text": {"type": "string"}}}),
                |args, _client| {
                    let (text,): (String,) = args.parse()?;
                    println!("emit: {text}");
                    Ok(Payload::null())
                },
            )
            // Observation over the data plane: the payload streams in as
            // chunks through the client channel, never inside a JSON frame.
            // Returns a bounded digest — control-plane preview discipline,
            // not a payload copy.
            .verb(&v("digest"), |args, client| {
                let (id,): (String,) = args.parse()?;
                let mut sink = DigestSink::default();
                let n = client.read_to(&id, 0, None, &mut sink)?;
                payload(&json!({ "bytes": n, "head_hex": hex_of(&sink.head) }))
            })
            // Data-plane ingest from the plugin side: generate n pattern
            // bytes and stream them into the CAS.
            .verb(&v("put_pattern"), |args, client| {
                let (n,): (u64,) = args.parse()?;
                let meta = client.put(PatternReader { left: n, pos: 0 }, "test/pattern", None)?;
                payload(&json!({ "meta": meta }))
            })
            // Invoke another plugin's verb through the kernel (cap-gated
            // there; this plugin holds no authority of its own).
            .verb(&v("relay"), |args, client| {
                let (target, inner): (String, Payload) = args.parse()?;
                Ok(client.invoke(&Verb::parse(&target)?, inner)?)
            })
            .verb(&v("publish"), |args, client| {
                let (topic, data): (String, Payload) = args.parse()?;
                let delivered = client.emit(&Topic::parse(&topic)?, data)?;
                payload(&json!({ "delivered": delivered }))
            })
            .verb(&v("subscribe"), |args, client| {
                let (topic,): (String,) = args.parse()?;
                let sub = client.subscribe(&Topic::parse(&topic)?)?;
                payload(&json!({ "sub": sub }))
            })
            .verb(&v("events"), move |_args, _client| {
                payload(&seen.lock().unwrap().clone())
            })
            .verb(&v("grants"), |_args, client| payload(&client.grants()?))
            // Two-layer naming demo: refs are driver-session-local, volatile,
            // and never enter the kernel handle table.
            .verb(&v("make_ref"), move |_args, _client| {
                let mut n = next_ref.lock().unwrap();
                *n += 1;
                let r = format!("e{n}");
                make.lock().unwrap().insert(r.clone());
                payload(&json!({ "ref": r }))
            })
            .verb(&v("use_ref"), move |args, _client| {
                let (r,): (String,) = args.parse()?;
                if used.lock().unwrap().contains(&r) {
                    payload(&json!({ "used": r }))
                } else {
                    Err(CallError::from(format!("stale ephemeral ref: {r}")))
                }
            }),
        move |topic, data| {
            received.lock().unwrap().push(Received {
                topic: topic.to_string(),
                data: data.clone(),
            });
        },
    )
}

fn payload<T: Serialize>(v: &T) -> CallResult {
    Ok(Payload::of(v)?)
}

/// Counts bytes and keeps the first 32 — the digest is a bounded preview,
/// so the payload is never held in memory.
#[derive(Default)]
struct DigestSink {
    head: Vec<u8>,
}

impl Write for DigestSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.head.len() < 32 {
            let take = (32 - self.head.len()).min(buf.len());
            self.head.extend_from_slice(&buf[..take]);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Deterministic pattern source: byte i is i % 251.
struct PatternReader {
    left: u64,
    pos: u64,
}

impl Read for PatternReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (self.left as usize).min(buf.len());
        for b in buf.iter_mut().take(n) {
            *b = (self.pos % 251) as u8;
            self.pos += 1;
        }
        self.left -= n as u64;
        Ok(n)
    }
}

fn hex_of(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
