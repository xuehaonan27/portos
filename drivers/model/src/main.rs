//! portos-modeld: the model driver — the LLM as a peripheral,
//! provider-neutral by construction.
//!
//! Verb family `model::` — `start` / `send` / `end`, defined by the
//! `portos-model-api` family interface. A `send` runs the agentic loop: the
//! configured [`backend`](crate::backend) produces turns, tool calls route
//! through kernel `invoke` (capability-gated there — this plugin holds no
//! authority of its own, not even network: LLM traffic goes through the
//! egress gateway, which injects an API key this process never sees), and
//! progress streams as `SessionEvent`s on the session's topic.
//!
//! Config: `$PORTOS_MODELD_DIR/config.json` —
//! `{backend, model, max_tokens, system, max_turns, tools: […]}`.
//! The tool surface comes from grants introspection by default — each verb
//! this plugin may invoke, joined with the metadata its driver advertised —
//! with config-declared `tools` overriding per verb.

mod backend;
mod backends;
mod core;

use crate::core::{EgressStream, Gateway, ModelError, Session, ToolDef};
use portos_egress_api::{self as egress, EgressRequest, StreamEvent};
use portos_model_api as model;
use portos_proto::ids::{Topic, Verb};
use portos_proto::wire::Payload;
use portos_sdk::{CallError, CallResult, KernelClient, Plugin};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, LazyLock, Mutex};

/// The single egress-stream topic this plugin listens on. One turn is in
/// flight at a time (the serve loop is single-threaded), so one slot is
/// enough; concurrent sessions would need per-stream topics.
static STREAM_TOPIC: LazyLock<Topic> =
    LazyLock::new(|| Topic::parse("portos-modeld::egress").expect("constant topic"));

/// The built-in data-plane tool: oversized tool results arrive as
/// `{handle, preview}`; this reads the full content back by handle. It is
/// model-driver plumbing (provider- and driver-neutral), served by the
/// kernel `read` op rather than an invoke — reads are free but audited.
static ARTIFACT_READ: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("artifact::read").expect("constant verb"));

struct Gw {
    client: Arc<KernelClient>,
    slot: Arc<Mutex<Option<SyncSender<StreamEvent>>>>,
}

impl Gateway for Gw {
    fn http_stream(&self, req: EgressRequest) -> Result<EgressStream, ModelError> {
        let (tx, rx) = sync_channel::<StreamEvent>(1024);
        *self.slot.lock().unwrap() = Some(tx);
        let req = req.streaming_to(STREAM_TOPIC.clone());
        let head = self
            .client
            .invoke(&egress::HTTP_STREAM, Payload::of(&req)?)
            .map_err(|e| ModelError::Gateway(e.to_string()))?;
        let head: egress::StreamHead = head.parse()?;
        Ok(EgressStream {
            status: head.status,
            rx,
        })
    }
}

fn load_config() -> Value {
    std::env::var_os("PORTOS_MODELD_DIR")
        .map(std::path::PathBuf::from)
        .and_then(|d| std::fs::read_to_string(d.join("config.json")).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

/// A tool the operator declared in config, overriding or adding to what
/// grants introspection turns up.
#[derive(Deserialize)]
struct ConfigTool {
    verb: Verb,
    #[serde(default)]
    description: String,
    #[serde(default)]
    schema: Option<Payload>,
}

fn load_tools(cfg: &Value) -> Result<Vec<ToolDef>, String> {
    let Some(list) = cfg.get("tools") else {
        return Ok(Vec::new());
    };
    let tools: Vec<ConfigTool> =
        serde_json::from_value(list.clone()).map_err(|e| format!("config tools: {e}"))?;
    Ok(tools
        .into_iter()
        .map(|t| ToolDef {
            verb: t.verb,
            description: t.description,
            schema: t.schema.unwrap_or_else(empty_object_schema),
        })
        .collect())
}

fn empty_object_schema() -> Payload {
    Payload::of(&json!({"type": "object", "properties": {}})).expect("constant schema")
}

fn artifact_read_tool() -> ToolDef {
    ToolDef {
        verb: ARTIFACT_READ.clone(),
        description: "Read (a range of) a stored artifact by id. Large tool results \
                      arrive as {handle, preview}: pass the handle here to read the \
                      full content as UTF-8 text. Page through big artifacts with \
                      offset/len; the result says whether it was truncated."
            .to_string(),
        schema: Payload::of(&json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "the artifact handle"},
                "offset": {"type": "integer"},
                "len": {"type": "integer"},
            },
            "required": ["id"],
        }))
        .expect("constant schema"),
    }
}

#[derive(Deserialize)]
struct ArtifactReadArgs {
    id: String,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    len: Option<u64>,
}

#[derive(Serialize)]
struct ArtifactReadReply {
    text: String,
    offset: u64,
    len_read: u64,
    truncated: bool,
}

/// The per-turn tool surface: grants introspection (each granted verb joined
/// with the metadata its driver advertised) + config-declared tools (which
/// win per verb) + the artifact::read built-in. Families in `exclude` never
/// surface — egress by default: it is this driver's own plumbing, not a
/// model tool, even though the capability exists.
fn assemble_tools(
    client: &Arc<KernelClient>,
    introspect: bool,
    exclude: &[String],
    config_tools: &[ToolDef],
) -> Vec<ToolDef> {
    let mut map: BTreeMap<Verb, ToolDef> = BTreeMap::new();
    if introspect {
        if let Ok(grants) = client.grants() {
            for g in grants {
                if exclude.iter().any(|e| e == g.verb.family()) {
                    continue;
                }
                map.insert(
                    g.verb.clone(),
                    ToolDef {
                        verb: g.verb,
                        description: g.description,
                        schema: g.schema,
                    },
                );
            }
        }
    }
    for t in config_tools {
        map.insert(t.verb.clone(), t.clone());
    }
    map.entry(ARTIFACT_READ.clone())
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

    let slot: Arc<Mutex<Option<SyncSender<StreamEvent>>>> = Arc::new(Mutex::new(None));
    let slot_for_events = slot.clone();

    let mut sessions: BTreeMap<String, Session> = BTreeMap::new();
    let mut next_session = 0u64;
    let mut subscribed = false;

    portos_sdk::serve(
        Plugin::new(
            "portos-modeld",
            &["model::start", "model::send", "model::end"],
        ),
        move |verb, args, client| -> CallResult {
            match verb.short() {
                "start" => {
                    let a: model::StartArgs = args.parse()?;
                    next_session += 1;
                    let id = model::SessionId::new(next_session);
                    sessions.insert(
                        id.as_str().to_string(),
                        Session {
                            system: a.system.unwrap_or_else(|| default_system.clone()),
                            messages: Vec::new(),
                        },
                    );
                    Ok(Payload::of(&model::StartReply { session: id })?)
                }
                "send" => {
                    let a: model::SendArgs = args.parse()?;
                    let sid = a.session;
                    let mut session = sessions
                        .remove(sid.as_str())
                        .ok_or_else(|| CallError::from(format!("unknown session: {sid}")))?;
                    if !subscribed {
                        client.subscribe(&STREAM_TOPIC)?;
                        subscribed = true;
                    }
                    let gw = Gw {
                        client: client.clone(),
                        slot: slot.clone(),
                    };
                    // Fresh per send: grants can change between turns.
                    let tools = assemble_tools(client, introspect, &exclude, &config_tools);
                    let topic = sid.topic();
                    let emit = |ev: model::SessionEvent| {
                        if let Ok(p) = Payload::of(&ev) {
                            let _ = client.emit(&topic, p);
                        }
                    };
                    let invoke = |verb: &Verb, a: Payload| -> Result<Payload, String> {
                        if verb == &*ARTIFACT_READ {
                            return read_artifact(client, a, read_max).map_err(|e| e.to_string());
                        }
                        client.invoke(verb, a).map_err(|e| e.to_string())
                    };
                    let result = core::run_send(
                        &*backend,
                        &gw,
                        &mut session,
                        &tools,
                        a.text,
                        max_turns,
                        &emit,
                        &invoke,
                    );
                    sessions.insert(sid.as_str().to_string(), session);
                    let text = result.map_err(|e| CallError::from(e.to_string()))?;
                    Ok(Payload::of(&model::SendReply { text })?)
                }
                "end" => {
                    let a: model::EndArgs = args.parse()?;
                    Ok(Payload::of(&model::EndReply {
                        ended: sessions.remove(a.session.as_str()).is_some(),
                    })?)
                }
                other => Err(CallError::from(format!("unknown verb: {other}"))),
            }
        },
        move |topic, data| {
            if topic == &*STREAM_TOPIC {
                if let Ok(ev) = data.parse::<StreamEvent>() {
                    if let Some(tx) = slot_for_events.lock().unwrap().as_ref() {
                        // A full or gone consumer just drops frames; the
                        // event thread must never block.
                        let _ = tx.try_send(ev);
                    }
                }
            }
        },
    )
}

/// Dereference an artifact for the model, capped at `read_max` bytes so one
/// tool result cannot flood the context.
fn read_artifact(
    client: &Arc<KernelClient>,
    args: Payload,
    read_max: u64,
) -> Result<Payload, CallError> {
    let a: ArtifactReadArgs = args.parse()?;
    let len = a.len.map(|l| l.min(read_max)).unwrap_or(read_max);
    let mut buf = Vec::new();
    let n = client.read_to(&a.id, a.offset, Some(len), &mut buf)?;
    Ok(Payload::of(&ArtifactReadReply {
        text: String::from_utf8_lossy(&buf).into_owned(),
        offset: a.offset,
        len_read: n,
        truncated: n == len,
    })?)
}
