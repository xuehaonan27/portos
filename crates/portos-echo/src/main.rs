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

use portos_proto::ids::{Topic, Verb};
use portos_proto::wire::{Payload, ToolMeta};
use portos_sdk::{CallError, CallResult, Plugin};
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

/// One event this plugin received, kept so a test can ask for them back.
#[derive(Clone, Serialize)]
struct Received {
    topic: String,
    data: Payload,
}

fn main() -> std::io::Result<()> {
    let family = std::env::var("PORTOS_ECHO_FAMILY").unwrap_or_else(|_| "echo".into());
    let name = format!("portos-{family}");
    let verbs: Vec<String> = [
        "emit",
        "digest",
        "make_ref",
        "use_ref",
        "relay",
        "publish",
        "subscribe",
        "events",
        "put_pattern",
        "grants",
    ]
    .iter()
    .map(|v| format!("{family}::{v}"))
    .collect();
    let verb_refs: Vec<&str> = verbs.iter().map(|s| s.as_str()).collect();

    // Advertise metadata for one verb so grants introspection has something
    // to join against in tests.
    let emit_verb = format!("{family}::emit");
    let mut tools = BTreeMap::new();
    tools.insert(
        emit_verb.as_str(),
        ToolMeta {
            description: "Print a line to the echo driver's stdout.".to_string(),
            schema: Payload::of(&json!({
                "type": "object", "properties": {"text": {"type": "string"}},
            }))
            .ok(),
        },
    );

    let mut ephemeral: HashSet<String> = HashSet::new();
    let mut next_ref = 0u32;
    let received: Arc<Mutex<Vec<Received>>> = Arc::new(Mutex::new(Vec::new()));
    let received_by_call = received.clone();

    portos_sdk::serve(
        Plugin::new(&name, &verb_refs).with_tools(tools),
        move |verb, args, client| -> CallResult {
            match verb.short() {
                // toy effect: visible side channel for tests (stdout)
                "emit" => {
                    let (text,): (String,) = args.parse()?;
                    println!("emit: {text}");
                    Ok(Payload::null())
                }
                // observation over the data plane: the payload streams in as
                // chunks through the client channel, never inside a JSON
                // frame. Returns a bounded digest (control-plane preview
                // discipline, not a payload copy).
                "digest" => {
                    let (id,): (String,) = args.parse()?;
                    let mut sink = DigestSink::default();
                    let n = client.read_to(&id, 0, None, &mut sink)?;
                    payload(&json!({ "bytes": n, "head_hex": hex_of(&sink.head) }))
                }
                // data-plane ingest from the plugin side: generate n pattern
                // bytes and stream them into the CAS.
                "put_pattern" => {
                    let (n,): (u64,) = args.parse()?;
                    let meta =
                        client.put(PatternReader { left: n, pos: 0 }, "test/pattern", None)?;
                    payload(&json!({ "meta": meta }))
                }
                // invoke another plugin's verb through the kernel (cap-gated
                // there; this plugin holds no authority of its own).
                "relay" => {
                    let (target, inner): (String, Payload) = args.parse()?;
                    Ok(client.invoke(&Verb::parse(&target)?, inner)?)
                }
                // event bus, both directions
                "publish" => {
                    let (topic, data): (String, Payload) = args.parse()?;
                    let delivered = client.emit(&Topic::parse(&topic)?, data)?;
                    payload(&json!({ "delivered": delivered }))
                }
                "subscribe" => {
                    let (topic,): (String,) = args.parse()?;
                    let sub = client.subscribe(&Topic::parse(&topic)?)?;
                    payload(&json!({ "sub": sub }))
                }
                "events" => {
                    let evs = received_by_call.lock().unwrap().clone();
                    payload(&evs)
                }
                // expose grants introspection for tests
                "grants" => payload(&client.grants()?),
                // two-layer naming demo: refs are driver-session-local,
                // volatile, and never enter the kernel handle table.
                "make_ref" => {
                    next_ref += 1;
                    let r = format!("e{next_ref}");
                    ephemeral.insert(r.clone());
                    payload(&json!({ "ref": r }))
                }
                "use_ref" => {
                    let (r,): (String,) = args.parse()?;
                    if ephemeral.contains(&r) {
                        payload(&json!({ "used": r }))
                    } else {
                        Err(CallError::from(format!("stale ephemeral ref: {r}")))
                    }
                }
                other => Err(CallError::from(format!("unknown verb: {other}"))),
            }
        },
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
