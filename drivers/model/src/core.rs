//! The neutral core of the model driver: session transcripts, tool
//! definitions, and the agentic loop. **Nothing in this module knows any
//! provider** — a provider is a [`Backend`] implementation chosen by config
//! (the same seam discipline as the browser driver's `driver.js`:
//! interface here, `backends/*` behind it). Nothing here knows the plan
//! language either (decisions-v1.md D31): the loop faces the Host ABI only,
//! through the `emit`/`invoke` closures its caller wires up.

use serde_json::{Value, json};
use std::sync::mpsc::Receiver;

/// A tool the model may call: a kernel verb plus what the model needs to
/// understand it. The verb is the identity; provider wire names are derived
/// by [`mangle`] at the provider boundary (D29) and never stored.
#[derive(Clone, Debug)]
pub struct ToolDef {
    pub verb: String,
    pub description: String,
    pub schema: Value,
}

/// `family::verb` → provider-safe tool name and back. Providers commonly
/// restrict tool names to `[A-Za-z0-9_-]`, so `::` maps to `__`; tool verbs
/// therefore must not contain `__` themselves (validated at config load).
pub fn mangle(verb: &str) -> String {
    verb.replace("::", "__")
}
pub fn unmangle(name: &str) -> String {
    name.replace("__", "::")
}

#[derive(Clone, Debug)]
pub enum Part {
    Text(String),
    ToolCall(ToolCall),
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    /// Provider-issued call id, echoed back with the result.
    pub id: String,
    pub verb: String,
    pub args: Value,
}

#[derive(Clone, Debug)]
pub struct ToolResultMsg {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// A neutral transcript message. The assistant variant carries the parts the
/// core interprets (text, tool calls) plus an optional provider-opaque `raw`
/// payload tagged with the backend that produced it — so that backend can
/// replay its own wire format faithfully (thinking blocks, signatures, …)
/// while any *other* backend falls back to reconstructing from the neutral
/// parts. The core never looks inside `raw`.
#[derive(Clone, Debug)]
pub enum Msg {
    User(String),
    Assistant {
        parts: Vec<Part>,
        raw: Option<(String, Value)>,
    },
    ToolResults(Vec<ToolResultMsg>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum StopKind {
    EndTurn,
    ToolUse,
    Other(String),
}

/// What a backend returns for one model turn.
pub struct TurnResult {
    pub parts: Vec<Part>,
    /// Provider-opaque payload for faithful replay (stored tagged with the
    /// backend name).
    pub raw: Value,
    pub stop: StopKind,
}

pub struct TurnRequest<'a> {
    pub system: &'a str,
    pub messages: &'a [Msg],
    pub tools: &'a [ToolDef],
}

/// Streaming output of a turn as it is generated.
pub trait TurnSink {
    fn text_delta(&mut self, s: &str);
}

/// A live egress response stream: the head (`{status, headers}`) plus broker
/// events (`{"chunk"}* → {"done"} | {"error"}`) as they arrive.
pub struct EgressStream {
    pub head: Value,
    pub rx: Receiver<Value>,
}

/// The network a backend is allowed to see: the kernel-mediated egress
/// chokepoint, nothing else. Credentials are injected broker-side; a backend
/// never holds a key.
pub trait Gateway {
    fn http(&self, args: Value) -> Result<Value, String>;
    fn http_stream(&self, args: Value) -> Result<EgressStream, String>;
}

/// One model provider. Stateless: the whole conversation rides in the
/// request, so backends stay swappable per session.
pub trait Backend {
    fn name(&self) -> &'static str;
    fn complete(
        &self,
        gw: &dyn Gateway,
        req: &TurnRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<TurnResult, String>;
}

pub struct Session {
    pub system: String,
    pub messages: Vec<Msg>,
}

struct EmitSink<'a> {
    emit: &'a dyn Fn(Value),
}
impl TurnSink for EmitSink<'_> {
    fn text_delta(&mut self, s: &str) {
        (self.emit)(json!({"kind": "delta", "text": s}));
    }
}

/// The agentic loop for one user message: model turn → (tool calls →
/// kernel invoke → results → next turn)* → final text. Tool failures feed
/// back to the model as `is_error` results rather than aborting the loop;
/// the kernel's capability gate on `invoke` is what actually bounds what the
/// model can do (enforcement below the model, never prompt discipline).
#[allow(clippy::too_many_arguments)]
pub fn run_send(
    backend: &dyn Backend,
    gw: &dyn Gateway,
    session: &mut Session,
    tools: &[ToolDef],
    user_text: String,
    max_turns: u32,
    emit: &dyn Fn(Value),
    invoke: &dyn Fn(&str, Value) -> Result<Value, String>,
) -> Result<String, String> {
    session.messages.push(Msg::User(user_text));
    for _ in 0..max_turns {
        let req = TurnRequest {
            system: &session.system,
            messages: &session.messages,
            tools,
        };
        let mut sink = EmitSink { emit };
        let turn = backend.complete(gw, &req, &mut sink)?;
        let calls: Vec<ToolCall> = turn
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        let text: String = turn
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        session.messages.push(Msg::Assistant {
            parts: turn.parts,
            raw: Some((backend.name().to_string(), turn.raw)),
        });

        if turn.stop != StopKind::ToolUse || calls.is_empty() {
            emit(json!({"kind": "done", "text": text}));
            return Ok(text);
        }

        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            emit(json!({"kind": "tool_call", "verb": call.verb, "args": call.args}));
            let (content, is_error) = match invoke(&call.verb, call.args.clone()) {
                Ok(v) => (serde_json::to_string(&v).unwrap_or_default(), false),
                Err(e) => (e, true),
            };
            emit(json!({"kind": "tool_result", "verb": call.verb, "ok": !is_error}));
            results.push(ToolResultMsg {
                call_id: call.id,
                content,
                is_error,
            });
        }
        session.messages.push(Msg::ToolResults(results));
    }
    Err(format!("max turns exceeded ({max_turns})"))
}
