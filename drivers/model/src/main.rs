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
//!   tools: [{verb, description, schema, kind?}]}`.
//! The tool surface comes from `grants` introspection (D33) with the config
//! list overriding per verb; the kernel still enforces capabilities on every
//! invoke. Each granted verb arrives with the character its driver declared
//! and the kernel checked (F4 `kind`/`budgeted`): it is shown to the model in
//! the tool description and rides on the session's tool events as
//! `verb_kind`, so reads and effects can be told apart end to end.
//!
//! This driver's own hello declares its verbs' characters too: `start`
//! materializes a session (consuming/held; the class gives sessions back
//! exactly, `holding_rho: inverse`), `send` is an external emission (the
//! provider observes the transcript and bills it; one consent covers a
//! conversation) that requires the egress stream verb, and `end` changes
//! held state only (transforming, idempotent). Nothing here touches the plan
//! language (D31).

mod backend;

use portos_model_core::{EgressStream, Gateway, Session, ToolDef};
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
            let mut def = ToolDef::new(
                verb,
                t["description"].as_str().unwrap_or(""),
                if t["schema"].is_object() {
                    t["schema"].clone()
                } else {
                    json!({"type": "object", "properties": {}})
                },
            );
            def.kind = t["kind"].as_str().map(String::from);
            def.budgeted = def.kind.as_deref().map(|k| k != "repeatable");
            out.push(def);
        }
    }
    Ok(out)
}

/// The built-in data-plane tool: oversized tool results arrive as
/// `{handle, preview}`; this reads the full content back by handle. It is
/// model-driver plumbing (provider- and driver-neutral), handled via the
/// kernel `read` op rather than an invoke — reads are free but audited.
const ARTIFACT_READ: &str = "artifact::read";

/// The built-in plan tool (WP-06): the model submits an effect plan for
/// admission; the kernel admits and renders it, and the person decides
/// consent out of band. The model can propose — it can never approve.
const PLAN_SUBMIT: &str = "plan::submit";

fn plan_submit_tool() -> ToolDef {
    ToolDef {
        verb: PLAN_SUBMIT.to_string(),
        description: "Submit an effect plan (JSON tree: stmts of let/effect/if/foreach) for \
                      admission. The kernel checks it and returns a deterministic rendering of \
                      its per-verb budget plus a run id. Tell the user the rendering and the \
                      plan path: only the person can sign consent (out of band, via the CLI). \
                      The run executes nothing before that."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "plan": {"type": "object", "description": "the plan AST"},
                "intent": {"type": "string", "description": "what the plan is for, in the model's words (rendered in the untrusted area)"},
            },
            "required": ["plan", "intent"],
        }),
        kind: Some("transforming".to_string()),
        budgeted: Some(false),
    }
}

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
        // A CAS read: immutable source, free, blind-replay safe.
        kind: Some("repeatable".to_string()),
        budgeted: Some(false),
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
                let Some(verb) = g["verb"].as_str() else {
                    continue;
                };
                let family = verb.split("::").next().unwrap_or(verb);
                if exclude.iter().any(|e| e == family) {
                    continue;
                }
                let mut def = ToolDef::new(
                    verb,
                    g["description"].as_str().unwrap_or(""),
                    if g["schema"].is_object() {
                        g["schema"].clone()
                    } else {
                        json!({"type": "object"})
                    },
                );
                def.kind = g["kind"].as_str().map(String::from);
                def.budgeted = g["budgeted"].as_bool();
                map.insert(verb.to_string(), def);
            }
        }
    }
    for t in config_tools {
        // Config wins on description and schema; the verb character stays
        // the kernel's (declared by the driver, checked at its spawn) unless
        // the config states one itself.
        let mut t = t.clone();
        if t.kind.is_none() {
            if let Some(seen) = map.get(&t.verb) {
                t.kind = seen.kind.clone();
                t.budgeted = seen.budgeted;
            }
        }
        map.insert(t.verb.clone(), t);
    }
    map.entry(ARTIFACT_READ.to_string())
        .or_insert_with(artifact_read_tool);
    map.entry(PLAN_SUBMIT.to_string())
        .or_insert_with(plan_submit_tool);
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

    // The declaration bundle: description/schema for grants introspection
    // and each verb's character for the kernel's truth table (F4), plus what
    // `send` needs from its slot row (F5): the egress stream verb.
    let hello_extra = json!({
        "tools": {
            "model::start": {
                "description": "Start a model session (optional system prompt); returns {session}.",
                "schema": {"type": "object", "properties": {"system": {"type": "string"}}},
                "kind": "consuming", "world": "held",
            },
            "model::send": {
                "description": "Send one user message to a session and run the agentic loop \
                                (provider turns, tool calls) to its final text.",
                "schema": {"type": "object", "required": ["session", "text"],
                           "properties": {"session": {"type": "string"}, "text": {"type": "string"}}},
                "kind": "emitting", "world": "external", "amortizable": true,
                "requires": {"caps": ["egress::http_stream"], "deps": ["egress"]},
            },
            "model::end": {
                "description": "End a session; idempotent.",
                "schema": {"type": "object", "required": ["session"],
                           "properties": {"session": {"type": "string"}}},
                "kind": "transforming", "idempotent": true,
            },
        },
        "holding_rho": "inverse",
    });

    portos_sdk::serve_hello(
        "portos-modeld",
        &["model::start", "model::send", "model::end"],
        hello_extra,
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
                        let len = a["len"]
                            .as_u64()
                            .map(|l| l.min(read_max))
                            .unwrap_or(read_max);
                        let mut buf = Vec::new();
                        let n = client.read_to(id, offset, Some(len), &mut buf)?;
                        return Ok(json!({
                            "text": String::from_utf8_lossy(&buf),
                            "offset": offset,
                            "len_read": n,
                            "truncated": n == len,
                        }));
                    }
                    if verb == PLAN_SUBMIT {
                        let plan = a.get("plan").cloned().ok_or("plan::submit: missing plan")?;
                        let intent = a["intent"].as_str().unwrap_or("");
                        return client.plan_submit(plan, intent);
                    }
                    client.invoke(verb, a)
                };
                let result = portos_model_core::run_send(
                    &*backend,
                    &gw,
                    &mut session,
                    &tools,
                    text,
                    max_turns,
                    &emit,
                    &invoke,
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
