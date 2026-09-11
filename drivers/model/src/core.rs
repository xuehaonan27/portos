//! The neutral core of the model driver: session transcripts, tool
//! definitions, and the agentic loop. **Nothing in this module knows any
//! provider** — a provider is a [`Backend`] implementation chosen by config.
//! Nothing here knows the kernel either: the loop faces the `emit`/`invoke`
//! closures its caller wires up.
//!
//! One thing here is deliberately untyped and stays that way: a provider's
//! own JSON, carried as [`TurnResult::raw`]. It is an external, evolving
//! schema that we replay **verbatim** so multi-turn context (thinking
//! blocks, signatures) survives a round trip. Modelling it would mean
//! dropping whatever we failed to model.

use portos_egress_api::{EgressRequest, StreamEvent};
use portos_model_api::SessionEvent;
use portos_proto::ids::Verb;
use portos_proto::wire::Payload;
use serde_json::Value;
use std::sync::mpsc::Receiver;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("provider: {0}")]
    Provider(String),
    #[error("gateway: {0}")]
    Gateway(String),
    #[error("payload: {0}")]
    Payload(#[from] serde_json::Error),
    #[error("max turns exceeded ({0})")]
    MaxTurns(u32),
}

impl From<portos_proto::ids::IdError> for ModelError {
    fn from(e: portos_proto::ids::IdError) -> ModelError {
        ModelError::Provider(e.to_string())
    }
}

/// A tool the model may call: a kernel verb plus what the model needs to
/// understand it. The verb is the identity; the provider-facing name is
/// derived by [`Verb::tool_name`] at the provider boundary and never stored.
#[derive(Clone, Debug)]
pub struct ToolDef {
    pub verb: Verb,
    pub description: String,
    pub schema: Payload,
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
    pub verb: Verb,
    pub args: Payload,
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
/// replay its own wire format faithfully while any *other* backend falls
/// back to reconstructing from the neutral parts. The core never looks
/// inside `raw`.
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
    /// Provider-opaque payload for faithful replay.
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

/// A live egress response stream: the status the gateway reported plus the
/// body frames as they arrive.
pub struct EgressStream {
    pub status: u16,
    pub rx: Receiver<StreamEvent>,
}

/// The network a backend is allowed to see: the kernel-mediated egress
/// chokepoint, nothing else. Credentials are injected gateway-side; a
/// backend never holds a key — there is no field in [`EgressRequest`]
/// through which one could travel.
pub trait Gateway {
    fn http_stream(&self, req: EgressRequest) -> Result<EgressStream, ModelError>;
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
    ) -> Result<TurnResult, ModelError>;
}

pub struct Session {
    pub system: String,
    pub messages: Vec<Msg>,
}

struct EmitSink<'a> {
    emit: &'a dyn Fn(SessionEvent),
}

impl TurnSink for EmitSink<'_> {
    fn text_delta(&mut self, s: &str) {
        (self.emit)(SessionEvent::Delta {
            text: s.to_string(),
        });
    }
}

/// The agentic loop for one user message: model turn → (tool calls →
/// kernel invoke → results → next turn)* → final text. Tool failures feed
/// back to the model as `is_error` results rather than aborting the loop;
/// the kernel's capability gate on `invoke` is what actually bounds what the
/// model can do — enforcement below the model, never prompt discipline.
pub fn run_send(
    backend: &dyn Backend,
    gw: &dyn Gateway,
    session: &mut Session,
    tools: &[ToolDef],
    user_text: String,
    max_turns: u32,
    emit: &dyn Fn(SessionEvent),
    invoke: &dyn Fn(&Verb, Payload) -> Result<Payload, String>,
) -> Result<String, ModelError> {
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
            emit(SessionEvent::Done { text: text.clone() });
            return Ok(text);
        }

        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            emit(SessionEvent::ToolCall {
                verb: call.verb.clone(),
            });
            let (content, is_error) = match invoke(&call.verb, call.args) {
                Ok(v) => (v.as_raw().to_string(), false),
                Err(e) => (e, true),
            };
            emit(SessionEvent::ToolResult {
                verb: call.verb,
                ok: !is_error,
            });
            results.push(ToolResultMsg {
                call_id: call.id,
                content,
                is_error,
            });
        }
        session.messages.push(Msg::ToolResults(results));
    }
    Err(ModelError::MaxTurns(max_turns))
}
