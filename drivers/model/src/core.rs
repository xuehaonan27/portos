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
    /// The verb's character as its driver declared it and the kernel checked
    /// it into the truth table (F4): `repeatable` | `transforming` |
    /// `consuming` | `emitting`. `None` when the route carries no
    /// declaration.
    pub kind: Option<String>,
    /// Whether the kernel budgets the verb (F4: everything but repeatable).
    pub budgeted: Option<bool>,
}

impl ToolDef {
    pub fn new(verb: impl Into<String>, description: impl Into<String>, schema: Value) -> Self {
        Self {
            verb: verb.into(),
            description: description.into(),
            schema,
            kind: None,
            budgeted: None,
        }
    }

    /// What the provider sees as the tool description: the driver's text plus
    /// one line stating the verb character, so the model can tell a free,
    /// retry-safe read from a budgeted effect before choosing. This informs
    /// the model; it never enforces anything (the kernel gates every invoke
    /// regardless of what the model believed).
    pub fn provider_description(&self) -> String {
        let Some(kind) = self.kind.as_deref() else {
            return self.description.clone();
        };
        let what = match kind {
            "repeatable" => "read-only, safe to repeat",
            "transforming" => "changes state its driver holds",
            "consuming" => "consumes or acquires a resource",
            "emitting" => "external effect",
            other => other,
        };
        let budget = match self.budgeted {
            Some(true) => "; budgeted",
            Some(false) => "; not budgeted",
            None => "",
        };
        let note = format!("[kind: {kind}; {what}{budget}]");
        if self.description.is_empty() {
            note
        } else {
            format!("{}\n{note}", self.description)
        }
    }
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
/// never holds a key. (`http` is the buffered variant for backends without
/// streaming responses; current backends stream.)
pub trait Gateway {
    #[allow(dead_code)]
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
            // Renderers get the verb character with the activity, so a read
            // and an effect can look different on screen (`kind` is the
            // event type; the verb's character rides as `verb_kind`).
            let character = tools.iter().find(|t| t.verb == call.verb);
            let with_character = |mut ev: Value| {
                if let Some(t) = character {
                    if let Some(k) = &t.kind {
                        ev["verb_kind"] = json!(k);
                    }
                    if let Some(b) = t.budgeted {
                        ev["budgeted"] = json!(b);
                    }
                }
                ev
            };
            emit(with_character(json!({"kind": "tool_call", "verb": call.verb, "args": call.args})));
            let (content, is_error) = match invoke(&call.verb, call.args.clone()) {
                Ok(v) => (serde_json::to_string(&v).unwrap_or_default(), false),
                Err(e) => (e, true),
            };
            emit(with_character(json!({"kind": "tool_result", "verb": call.verb, "ok": !is_error})));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_description_carries_the_verb_character() {
        let mut t = ToolDef::new("browser::click", "Click the element.", json!({}));
        assert_eq!(t.provider_description(), "Click the element.");
        t.kind = Some("emitting".into());
        t.budgeted = Some(true);
        assert_eq!(
            t.provider_description(),
            "Click the element.\n[kind: emitting; external effect; budgeted]"
        );
        let mut r = ToolDef::new("browser::snapshot", "", json!({}));
        r.kind = Some("repeatable".into());
        r.budgeted = Some(false);
        assert_eq!(r.provider_description(), "[kind: repeatable; read-only, safe to repeat; not budgeted]");
    }

    #[test]
    fn tool_activity_events_carry_the_verb_character() {
        struct Scripted;
        impl Backend for Scripted {
            fn name(&self) -> &'static str {
                "scripted"
            }
            fn complete(
                &self,
                _gw: &dyn Gateway,
                req: &TurnRequest,
                _sink: &mut dyn TurnSink,
            ) -> Result<TurnResult, String> {
                // Turn 1: call the tool; turn 2: finish.
                if req.messages.len() == 1 {
                    Ok(TurnResult {
                        parts: vec![Part::ToolCall(ToolCall {
                            id: "c1".into(),
                            verb: "browser::click".into(),
                            args: json!({"ref": "e1"}),
                        })],
                        raw: Value::Null,
                        stop: StopKind::ToolUse,
                    })
                } else {
                    Ok(TurnResult { parts: vec![Part::Text("done".into())], raw: Value::Null, stop: StopKind::EndTurn })
                }
            }
        }
        struct NoNet;
        impl Gateway for NoNet {
            fn http(&self, _: Value) -> Result<Value, String> {
                Err("no network".into())
            }
            fn http_stream(&self, _: Value) -> Result<EgressStream, String> {
                Err("no network".into())
            }
        }
        let mut click = ToolDef::new("browser::click", "Click.", json!({}));
        click.kind = Some("emitting".into());
        click.budgeted = Some(true);
        let mut session = Session { system: String::new(), messages: Vec::new() };
        let events = std::cell::RefCell::new(Vec::new());
        let emit = |v: Value| events.borrow_mut().push(v);
        let invoke = |_: &str, _: Value| Ok(json!({"clicked": true}));
        let out = run_send(&Scripted, &NoNet, &mut session, &[click], "go".into(), 4, &emit, &invoke).unwrap();
        assert_eq!(out, "done");
        let evs = events.borrow();
        let call = evs.iter().find(|e| e["kind"] == "tool_call").unwrap();
        assert_eq!(call["verb_kind"], "emitting");
        assert_eq!(call["budgeted"], true);
        let result = evs.iter().find(|e| e["kind"] == "tool_result").unwrap();
        assert_eq!(result["verb_kind"], "emitting");
        assert_eq!(result["ok"], true);
    }
}
