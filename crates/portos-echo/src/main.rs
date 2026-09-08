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
        "grants",
        "spawn_child",
        "hold_process",
    ]
    .iter()
    .map(|v| format!("{family}::{v}"))
    .collect();
    let verb_refs: Vec<&str> = verbs.iter().map(|s| s.as_str()).collect();
    // The declaration bundle: description/schema for grants introspection,
    // and each verb's character for the kernel's F4 truth table. Env toggles
    // let the tests exercise the doors:
    //   PORTOS_ECHO_BAD_KIND=1        declare emit repeatable-but-not-idempotent (incoherent)
    //   PORTOS_ECHO_RELAY_REQUIRES=a,b  relay requires these caps (F5 slot admission)
    //   PORTOS_ECHO_RELAY_DEPS=x,y      relay depends on these families
    //   PORTOS_ECHO_PROTOCOL=1        declare make_ref-before-use_ref as a protocol (F6)
    let v = |s: &str| format!("{family}::{s}");
    let list = |env: &str| -> Vec<String> {
        std::env::var(env)
            .ok()
            .map(|s| s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let emit_kind = if std::env::var("PORTOS_ECHO_BAD_KIND").is_ok() {
        json!({"kind": "repeatable", "idempotent": false})
    } else if std::env::var("PORTOS_ECHO_HARD_EMIT").is_ok() {
        // F3 hard-list shape for plan-run tests: emitting ∧ non-amortizable.
        json!({"kind": "emitting", "world": "external", "amortizable": false})
    } else {
        json!({"kind": "emitting", "world": "external", "amortizable": true})
    };
    let mut tools_meta = json!({
        v("emit"): {
            "description": "Print a line to the echo driver's stdout.",
            "schema": {"type": "object", "properties": {"text": {"type": "string"}}},
        },
        v("digest"): {"kind": "repeatable"},
        v("events"): {"kind": "repeatable"},
        v("grants"): {"kind": "repeatable"},
        v("use_ref"): {"kind": "repeatable"},
        v("publish"): {"kind": "emitting", "world": "external", "amortizable": true},
        v("make_ref"): {"kind": "transforming"},
        v("put_pattern"): {"kind": "transforming"},
        v("subscribe"): {"kind": "consuming", "world": "held"},
        v("spawn_child"): {"kind": "transforming"},
        v("hold_process"): {"kind": "transforming"},
        v("relay"): {"requires": {"caps": list("PORTOS_ECHO_RELAY_REQUIRES"), "deps": list("PORTOS_ECHO_RELAY_DEPS")}},
    });
    for (k, val) in emit_kind.as_object().unwrap() {
        tools_meta[v("emit")][k] = val.clone();
    }
    let mut hello_extra = json!({"tools": tools_meta, "holding_rho": "inverse"});
    if std::env::var("PORTOS_ECHO_PROTOCOL").is_ok() {
        hello_extra["protocol"] = json!({
            "initial": "idle",
            "transitions": [
                ["idle", v("make_ref"), "open"],
                ["open", v("make_ref"), "open"],
                ["open", v("use_ref"), "open"],
            ],
        });
    }

    let mut ephemeral: HashSet<String> = HashSet::new();
    let mut next_ref = 0u32;
    let received: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let received_by_call = received.clone();
    // Child processes this plugin registered as `kernel/process` holdings
    // (kept so their handles outlive the call handler).
    let children: Arc<Mutex<Vec<std::process::Child>>> = Arc::new(Mutex::new(Vec::new()));
    let children_by_call = children.clone();
    let plugin_name = name.clone();

    let prefix = format!("{family}::");
    portos_sdk::serve_hello(
        &name,
        &verb_refs,
        hello_extra,
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
                // expose grants introspection for tests
                "grants" => Ok(json!(client.grants()?)),
                // parent/child instantiation (WP-04): spawn a copy of this
                // binary as a child plugin. The kernel gates the op on the
                // caller's `kernel:spawn` capability; the child's holding is
                // parented under this plugin's own (F2 closure).
                "spawn_child" => {
                    let family = arg(0).as_str().unwrap_or("echoc").to_string();
                    let bin = std::env::current_exe().map_err(|e| e.to_string())?;
                    let bin = bin.to_str().ok_or_else(|| "exe path not utf-8".to_string())?;
                    client
                        .spawn_child(bin, &[], &[("PORTOS_ECHO_FAMILY", family.as_str())])
                        .map(|name| json!({"name": name}))
                }
                // substrate holding (WP-02): spawn `sleep <n>` and register it
                // as a `kernel/process` holding under this plugin — the kernel
                // kills the exact incarnation when this plugin is reclaimed.
                "hold_process" => {
                    let secs = arg(0).as_u64().unwrap_or(300);
                    let child = std::process::Command::new("sleep")
                        .arg(secs.to_string())
                        .spawn()
                        .map_err(|e| e.to_string())?;
                    let pid = child.id();
                    children_by_call.lock().unwrap().push(child);
                    let instance = format!("{plugin_name}/{pid}");
                    let holding =
                        client.hold("kernel/process", &instance, json!({"pid": pid}), None)?;
                    Ok(json!({"pid": pid, "holding": holding.id, "generation": holding.generation}))
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
