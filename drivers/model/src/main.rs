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

/// The built-in data-plane tool: oversized tool results arrive as
/// `{handle, preview}`; this reads the full content back by handle. It is
/// model-driver plumbing (provider- and driver-neutral), handled via the
/// kernel `read` op rather than an invoke — reads are free but audited.
const ARTIFACT_READ: &str = "artifact::read";

fn artifact_read_tool() -> ToolDef {
    ToolDef {
        verb: ARTIFACT_READ.to_string(),
        description: "Read (a range of) a stored artifact by id. Large tool results \
                      arrive as {handle, preview}: pass the handle here to read the \
                      full content as UTF-8 text. Page through big artifacts with \
                      offset/len; the result says whether it was truncated."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "the artifact handle"},
                "offset": {"type": "integer"},
                "len": {"type": "integer"},
            },
            "required": ["id"],
        }),
    }
}

/// The per-turn tool surface: grants introspection (each granted verb joined
/// with the metadata its driver advertised) + config-declared tools (which
/// win per verb) + the artifact::read built-in. Families in `exclude` never
/// surface — egress by default: it is this driver's own plumbing, not a
/// model tool, even though the capability exists.
fn assemble_tools(
    client: &std::sync::Arc<portos_sdk::KernelClient>,
    introspect: bool,
    exclude: &[String],
    config_tools: &[ToolDef],
) -> Vec<ToolDef> {
    let mut map: BTreeMap<String, ToolDef> = BTreeMap::new();
    if introspect {
        if let Ok(grants) = client.grants() {
            for g in grants {
                let Some(verb) = g["verb"].as_str() else { continue };
                let family = verb.split("::").next().unwrap_or(verb);
                if exclude.iter().any(|e| e == family) {
                    continue;
                }
                map.insert(
                    verb.to_string(),
                    ToolDef {
                        verb: verb.to_string(),
                        description: g["description"].as_str().unwrap_or("").to_string(),
                        schema: if g["schema"].is_object() {
                            g["schema"].clone()
                        } else {
                            json!({"type": "object"})
                        },
                    },
                );
            }
        }
    }
    for t in config_tools {
        map.insert(t.verb.clone(), t.clone());
    }
    map.entry(ARTIFACT_READ.to_string())
        .or_insert_with(artifact_read_tool);
    map.into_values().collect()
}

fn main() -> std::io::Result<()> {
    let cfg = load_config();
    let backend = backend::make_backend(&cfg).map_err(std::io::Error::other)?;
    let config_tools = load_tools(&cfg).map_err(std::io::Error::other)?;
    let introspect = cfg["introspect_tools"].as_bool().unwrap_or(true);
    let exclude: Vec<String> = cfg["tool_families_exclude"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| vec!["egress".to_string()]);
    let read_max = cfg["read_max"].as_u64().unwrap_or(32 * 1024);
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
                // Fresh per send: grants can change between turns.
                let tools = assemble_tools(client, introspect, &exclude, &config_tools);
                let topic = format!("model::session::{sid}");
                let emit = |v: Value| {
                    let _ = client.emit(&topic, v);
                };
                let invoke = |verb: &str, a: Value| -> Result<Value, String> {
                    if verb == ARTIFACT_READ {
                        let id = a["id"].as_str().ok_or("artifact::read: missing id")?;
                        let offset = a["offset"].as_u64().unwrap_or(0);
                        let len = a["len"].as_u64().map(|l| l.min(read_max)).unwrap_or(read_max);
                        let mut buf = Vec::new();
                        let n = client.read_to(id, offset, Some(len), &mut buf)?;
                        return Ok(json!({
                            "text": String::from_utf8_lossy(&buf),
                            "offset": offset,
                            "len_read": n,
                            "truncated": n == len,
                        }));
                    }
                    client.invoke(verb, a)
                };
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
