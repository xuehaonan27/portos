//! portos-modeld: the model driver (decisions-v1.md D27) — the LLM as a
//! peripheral (architecture-v0.md §3.1), provider-neutral by construction.
//!
//! Verb family `model::` — `start` / `send` / `end`. A `send` runs the
//! agentic loop: the configured [`backend`](crate::backend) produces turns,
//! tool calls route through kernel `invoke` (capability-gated there — this
//! plugin holds no authority of its own, not even network: LLM traffic goes
//! through the egress broker, which injects the API key this process never
//! sees), and progress streams as events on `model::session::<id>`
//! (`{"kind": "delta"|"tool_call"|"tool_result"|"done"}`).
//!
//! Config: `$PORTOS_MODELD_DIR/config.json` —
//! `{backend, model, max_tokens, system, max_turns,
//!   tools: [{verb, description, schema}]}`.
//! The tool surface is config-declared for now (the kernel still enforces
//! capabilities on every invoke; a grants-introspection op can replace the
//! config list later). Nothing here touches the plan language (D31).

mod backend;
mod backends;
mod core;

use crate::core::{EgressStream, Gateway, Session, ToolDef};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

/// The single egress-stream topic this plugin listens on. One turn is in
/// flight at a time (the serve loop is single-threaded), so one slot is
/// enough; concurrent sessions would need per-stream topics.
const STREAM_TOPIC: &str = "portos-modeld::egress";

struct Gw {
    client: Arc<portos_sdk::KernelClient>,
    slot: Arc<Mutex<Option<SyncSender<Value>>>>,
}

impl Gateway for Gw {
    fn http(&self, args: Value) -> Result<Value, String> {
        self.client.invoke("egress::http", args)
    }
    fn http_stream(&self, mut args: Value) -> Result<EgressStream, String> {
        let (tx, rx) = sync_channel::<Value>(1024);
        *self.slot.lock().unwrap() = Some(tx);
        args["topic"] = json!(STREAM_TOPIC);
        let head = self.client.invoke("egress::http_stream", args)?;
        Ok(EgressStream { head, rx })
    }
}

fn load_config() -> Value {
    std::env::var_os("PORTOS_MODELD_DIR")
        .map(std::path::PathBuf::from)
        .and_then(|d| std::fs::read_to_string(d.join("config.json")).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn load_tools(cfg: &Value) -> Result<Vec<ToolDef>, String> {
    let mut out = Vec::new();
    if let Some(list) = cfg["tools"].as_array() {
        for t in list {
            let verb = t["verb"]
                .as_str()
                .ok_or("tool entry missing verb")?
                .to_string();
            if verb.contains("__") {
                return Err(format!("tool verb may not contain '__': {verb}"));
            }
            out.push(ToolDef {
                verb,
                description: t["description"].as_str().unwrap_or("").to_string(),
                schema: if t["schema"].is_object() {
                    t["schema"].clone()
                } else {
                    json!({"type": "object", "properties": {}})
                },
            });
        }
    }
    Ok(out)
}

fn main() -> std::io::Result<()> {
    let cfg = load_config();
    let backend = backend::make_backend(&cfg).map_err(std::io::Error::other)?;
    let tools = load_tools(&cfg).map_err(std::io::Error::other)?;
    let default_system = cfg["system"].as_str().unwrap_or("").to_string();
    let max_turns = cfg["max_turns"].as_u64().unwrap_or(16) as u32;

    let slot: Arc<Mutex<Option<SyncSender<Value>>>> = Arc::new(Mutex::new(None));
    let slot_for_events = slot.clone();

    let mut sessions: BTreeMap<String, Session> = BTreeMap::new();
    let mut next_session = 0u64;
    let mut subscribed = false;

    portos_sdk::serve(
        "portos-modeld",
        &["model::start", "model::send", "model::end"],
        move |verb, args, client| match verb {
            "model::start" => {
                next_session += 1;
                let id = format!("s{next_session}");
                let system = args["system"]
                    .as_str()
                    .unwrap_or(&default_system)
                    .to_string();
                sessions.insert(
                    id.clone(),
                    Session {
                        system,
                        messages: Vec::new(),
                    },
                );
                Ok(json!({"session": id}))
            }
            "model::send" => {
                let sid = args["session"]
                    .as_str()
                    .ok_or("missing session")?
                    .to_string();
                let text = args["text"].as_str().ok_or("missing text")?.to_string();
                let mut session = sessions
                    .remove(&sid)
                    .ok_or_else(|| format!("unknown session: {sid}"))?;
                if !subscribed {
                    client.subscribe(STREAM_TOPIC)?;
                    subscribed = true;
                }
                let gw = Gw {
                    client: client.clone(),
                    slot: slot.clone(),
                };
                let topic = format!("model::session::{sid}");
                let emit = |v: Value| {
                    let _ = client.emit(&topic, v);
                };
                let invoke = |verb: &str, a: Value| client.invoke(verb, a);
                let result = core::run_send(
                    &*backend, &gw, &mut session, &tools, text, max_turns, &emit, &invoke,
                );
                sessions.insert(sid, session);
                Ok(json!({"text": result?}))
            }
            "model::end" => {
                let sid = args["session"].as_str().unwrap_or("");
                Ok(json!({"ended": sessions.remove(sid).is_some()}))
            }
            other => Err(format!("unknown verb: {other}")),
        },
        move |topic, data| {
            if topic == STREAM_TOPIC {
                if let Some(tx) = slot_for_events.lock().unwrap().as_ref() {
                    // A full or gone consumer just drops chunks; the event
                    // thread must never block.
                    let _ = tx.try_send(data.clone());
                }
            }
        },
    )
}
