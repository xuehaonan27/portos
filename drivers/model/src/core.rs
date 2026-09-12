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
use serde::{Deserialize, Serialize};
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
    #[error("cancelled")]
    Cancelled,
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

/// The transcript types below are a **stored format**, not just an in-memory
/// one: a session is written to the CAS and read back after the driver has
/// been restarted. Two rules follow, and both are load-bearing.
///
/// They are **externally tagged** (serde's default) and must stay that way.
/// An internally-tagged enum deserializes through an intermediate buffer
/// that raw JSON cannot be read from, and [`ToolCall::args`] is exactly
/// that — the same trap the wire protocol hit. `transcripts_round_trip`
/// below is what keeps it from creeping back.
///
/// And the provider-opaque `raw` payload must survive **byte for byte**,
/// because replaying it is the only way a multi-turn conversation keeps its
/// thinking blocks and signatures.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    Text(String),
    ToolCall(ToolCall),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-issued call id, echoed back with the result.
    pub id: String,
    pub verb: Verb,
    pub args: Payload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// A turn's liveness channel, in both directions: output as it is generated,
/// and whether anyone still wants it. A backend must poll [`cancelled`] while
/// it waits on the network — that wait is where nearly all of a turn's time
/// goes, so a cancel that is only checked between turns is not a cancel.
///
/// [`cancelled`]: TurnSink::cancelled
pub trait TurnSink {
    fn text_delta(&mut self, s: &str);
    fn cancelled(&self) -> bool {
        false
    }
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

/// A conversation. This is what gets stored, so it is also the compatibility
/// surface: a field added here must be `#[serde(default)]` or an older
/// transcript stops loading.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub system: String,
    #[serde(default)]
    pub messages: Vec<Msg>,
}

impl Session {
    /// Enough of the opening line to recognise a conversation in a list.
    pub fn title(&self, chars: usize) -> String {
        let first = self.messages.iter().find_map(|m| match m {
            Msg::User(t) => Some(t.as_str()),
            _ => None,
        });
        let line = first.unwrap_or("").lines().next().unwrap_or("").trim();
        match line.char_indices().nth(chars) {
            Some((end, _)) => format!("{}…", &line[..end]),
            None => line.to_string(),
        }
    }

    /// How many user messages this conversation holds — the count a person
    /// means by "how long is it".
    pub fn turns(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| matches!(m, Msg::User(_)))
            .count()
    }
}

struct EmitSink<'a> {
    emit: &'a dyn Fn(SessionEvent),
    cancelled: &'a dyn Fn() -> bool,
}

impl TurnSink for EmitSink<'_> {
    fn text_delta(&mut self, s: &str) {
        (self.emit)(SessionEvent::Delta {
            text: s.to_string(),
        });
    }
    fn cancelled(&self) -> bool {
        (self.cancelled)()
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
    // Called with the finished transcript immediately before a terminal
    // event goes out. A terminal event is a promise that everything the turn
    // produced is durable — a front end acts on it by sending the next
    // message, reloading, or quitting — so saving afterwards would mean the
    // last turn of every conversation is the one at risk.
    checkpoint: &dyn Fn(&Session),
    // Recomputed every turn, not once: a tool call can change what the
    // caller may do — starting a plugin is exactly that — and a surface
    // fixed at the first turn would hide the thing just added until the
    // next message.
    tools: &dyn Fn() -> Vec<ToolDef>,
    user_text: String,
    max_turns: u32,
    emit: &dyn Fn(SessionEvent),
    invoke: &dyn Fn(&Verb, Payload) -> Result<Payload, String>,
    cancelled: &dyn Fn() -> bool,
) -> Result<String, ModelError> {
    // Where to rewind to if this turn is abandoned. A cancelled turn leaves
    // no trace: a half-streamed assistant message, or a tool call with no
    // result, would make the *next* turn malformed.
    let mark = session.messages.len();
    session.messages.push(Msg::User(user_text));

    for _ in 0..max_turns {
        if cancelled() {
            return abandon(session, mark, checkpoint, emit);
        }
        let available = tools();
        let req = TurnRequest {
            system: &session.system,
            messages: &session.messages,
            tools: &available,
        };
        let mut sink = EmitSink { emit, cancelled };
        let turn = match backend.complete(gw, &req, &mut sink) {
            Ok(t) => t,
            Err(ModelError::Cancelled) => return abandon(session, mark, checkpoint, emit),
            Err(e) => return Err(e),
        };
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
            checkpoint(session);
            emit(SessionEvent::Done { text: text.clone() });
            return Ok(text);
        }

        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            // Between a model asking for a tool and the tool running is the
            // other place a cancel has to land: the effect has not happened
            // yet, so not doing it is still free.
            if cancelled() {
                return abandon(session, mark, checkpoint, emit);
            }
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

/// Rewind the transcript and tell the session's subscribers. Whatever the
/// turn had already produced is dropped on purpose.
fn abandon(
    session: &mut Session,
    mark: usize,
    checkpoint: &dyn Fn(&Session),
    emit: &dyn Fn(SessionEvent),
) -> Result<String, ModelError> {
    session.messages.truncate(mark);
    // The rewound transcript is the one that actually happened, and it has to
    // be stored before anyone is told the turn ended.
    checkpoint(session);
    emit(SessionEvent::Cancelled);
    Err(ModelError::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A transcript is stored and read back after a restart, so this is a
    /// format test, not a smoke test. The two things it protects: raw JSON
    /// inside a tool call (which an internally-tagged enum would silently
    /// break), and the provider's opaque payload surviving byte for byte.
    #[test]
    fn transcripts_round_trip() {
        let session = Session {
            system: "be brief".into(),
            messages: vec![
                Msg::User("open the page".into()),
                Msg::Assistant {
                    parts: vec![
                        Part::Text("on it".into()),
                        Part::ToolCall(ToolCall {
                            id: "toolu_1".into(),
                            verb: Verb::parse("browser::open").unwrap(),
                            args: Payload::of(&json!({"url": "https://example.com"})).unwrap(),
                        }),
                    ],
                    raw: Some((
                        "anthropic".into(),
                        json!([{"type": "thinking", "thinking": "hmm", "signature": "sig123"}]),
                    )),
                },
                Msg::ToolResults(vec![ToolResultMsg {
                    call_id: "toolu_1".into(),
                    content: "{\"ok\":true}".into(),
                    is_error: false,
                }]),
            ],
        };

        let bytes = serde_json::to_vec(&session).unwrap();
        let back: Session = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, session, "a stored conversation comes back unchanged");

        // The provider payload in particular: replaying it is what keeps
        // thinking blocks and signatures across turns.
        let Msg::Assistant { raw, .. } = &back.messages[1] else {
            panic!("expected the assistant turn")
        };
        assert_eq!(raw.as_ref().unwrap().1[0]["signature"], "sig123");

        assert_eq!(back.turns(), 1);
        assert_eq!(back.title(80), "open the page");
    }

    /// An older transcript must still load when a field is added later.
    #[test]
    fn a_transcript_missing_newer_fields_still_loads() {
        let bare: Session = serde_json::from_str(r#"{"messages":[{"user":"hi"}]}"#).unwrap();
        assert_eq!(bare.system, "");
        assert_eq!(bare.turns(), 1);
    }
}
