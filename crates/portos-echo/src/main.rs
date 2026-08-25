//! portos-echo: the toy driver. Exists to exercise kernel mechanisms end to
//! end over ABI v2 — verb calls, chunked artifact streaming (put/read),
//! capability-gated invoke, the event bus, and the two-layer naming rule
//! (ephemeral refs live here, NOT in the kernel handle table;
//! browser-driver-v0.md §14-7).
//!
//! The verb family is `PORTOS_ECHO_FAMILY` (default "echo"), so one binary
//! can be spawned as several distinct plugins — which is exactly what the
//! invoke tests need (a plugin cannot invoke itself: single-threaded serve
//! loop, and invoke cycles deadlock by design in M0).

use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

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
    ]
    .iter()
    .map(|v| format!("{family}::{v}"))
    .collect();
    let verb_refs: Vec<&str> = verbs.iter().map(|s| s.as_str()).collect();

    let mut ephemeral: HashSet<String> = HashSet::new();
    let mut next_ref = 0u32;
    let received: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let received_by_call = received.clone();

    let prefix = format!("{family}::");
    portos_sdk::serve(
        &name,
        &verb_refs,
        move |verb, args, client| {
            let short = verb.strip_prefix(&prefix).unwrap_or(verb);
            let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
            match short {
                // toy effect: visible side channel for tests (stdout)
                "emit" => {
                    let s = arg(0).as_str().unwrap_or("").to_string();
                    println!("emit: {s}");
                    Ok(Value::Null)
                }
                // observation over the data plane: the payload streams in as
                // chunks through the client channel, never inside a JSON
                // frame. Returns a bounded digest (control-plane preview
                // discipline, not a payload copy).
                "digest" => {
                    let id = arg(0).as_str().unwrap_or("").to_string();
                    let mut sink = DigestSink::default();
                    let n = client.read_to(&id, 0, None, &mut sink)?;
                    Ok(json!({ "bytes": n, "head_hex": hex_of(&sink.head) }))
                }
                // data-plane ingest from the plugin side: generate n pattern
                // bytes and stream them into the CAS.
                "put_pattern" => {
                    let n = arg(0).as_u64().unwrap_or(0);
                    let meta = client.put(PatternReader { left: n, pos: 0 }, "test/pattern", Value::Null)?;
                    Ok(json!({ "meta": meta }))
                }
                // invoke another plugin's verb through the kernel (cap-gated
                // there; this plugin holds no authority of its own).
                "relay" => {
                    let target = arg(0).as_str().unwrap_or("").to_string();
                    client.invoke(&target, arg(1))
                }
                // event bus, both directions
                "publish" => {
                    let topic = arg(0).as_str().unwrap_or("").to_string();
                    let delivered = client.emit(&topic, arg(1))?;
                    Ok(json!({ "delivered": delivered }))
                }
                "subscribe" => {
                    let topic = arg(0).as_str().unwrap_or("").to_string();
                    let sub = client.subscribe(&topic)?;
                    Ok(json!({ "sub": sub }))
                }
                "events" => {
                    let evs = received_by_call.lock().unwrap();
                    Ok(json!(evs.clone()))
                }
                // two-layer naming demo: refs are driver-session-local,
                // volatile, and never enter the kernel handle table.
                "make_ref" => {
                    next_ref += 1;
                    let r = format!("e{next_ref}");
                    ephemeral.insert(r.clone());
                    Ok(json!({ "ref": r }))
                }
                "use_ref" => {
                    let r = arg(0).as_str().unwrap_or("").to_string();
                    if ephemeral.contains(&r) {
                        Ok(json!({ "used": r }))
                    } else {
                        Err(format!("stale ephemeral ref: {r}"))
                    }
                }
                other => Err(format!("unknown verb: {other}")),
            }
        },
        move |topic, data| {
            received
                .lock()
                .unwrap()
                .push(json!({ "topic": topic, "data": data }));
        },
    )
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
