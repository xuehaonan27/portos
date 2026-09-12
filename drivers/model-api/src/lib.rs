//! The `model::*` family interface.
//!
//! The contract between a model driver and whoever drives it: how a session
//! is opened, fed and closed, and what a session publishes while a turn is
//! running. Provider-neutral by construction — nothing here names Anthropic
//! or any other vendor, because a provider is an implementation detail one
//! level further down.

use portos_proto::ids::{Topic, Verb};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

pub static START: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("model::start").expect("constant verb"));
pub static SEND: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("model::send").expect("constant verb"));
pub static END: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("model::end").expect("constant verb"));

/// A session's identity. Also the last segment of its event topic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(n: u64) -> SessionId {
        SessionId(format!("s{n}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The topic this session's progress is published on. Subscribing to
    /// `model::session::*` catches every session at once, which is what a
    /// renderer does.
    pub fn topic(&self) -> Topic {
        Topic::parse(&format!("model::session::{}", self.0)).expect("session ids are topic-safe")
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Every session topic, for a renderer that wants all of them.
/// Stop the turn running on a session. The session itself survives; it is
/// rewound to exactly where it was before the abandoned message.
pub static CANCEL: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("model::cancel").expect("constant verb"));

pub static ALL_SESSIONS: LazyLock<Topic> =
    LazyLock::new(|| Topic::parse("model::session::*").expect("constant topic"));

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StartArgs {
    /// Overrides the driver's configured system prompt for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartReply {
    pub session: SessionId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendArgs {
    pub session: SessionId,
    pub text: String,
}

/// `send` returns as soon as the turn is accepted, not when it finishes: a
/// turn can run for minutes, and a caller blocked for that long can neither
/// cancel it nor do anything else. Everything the turn produces — text,
/// tool activity, its ending — arrives on the session topic as
/// [`SessionEvent`]s.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SendReply {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CancelArgs {
    pub session: SessionId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CancelReply {
    /// Whether a turn was actually running to cancel.
    pub cancelled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EndArgs {
    pub session: SessionId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EndReply {
    pub ended: bool,
}

/// What a session publishes while a turn runs. Renderers subscribe to these
/// and present them; nothing else in the system interprets them.
///
/// A tool call's arguments are deliberately absent: no renderer has wanted
/// them, and leaving them out keeps this a plain tagged enum that both
/// halves of the contract can parse.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// A fragment of assistant text, as it is generated.
    Delta { text: String },
    /// A tool the model asked for, about to be invoked.
    ToolCall { verb: Verb },
    /// How that invocation ended.
    ToolResult { verb: Verb, ok: bool },
    /// The turn is over; `text` is everything the assistant said.
    Done { text: String },
    /// The turn was cancelled. The session is left exactly as it was before
    /// the cancelled message, so the next turn starts from clean ground.
    Cancelled,
    /// The turn ended badly. Since `send` returns before a turn runs, this is
    /// the only way a caller learns it failed — without it a front end waits
    /// forever for a `done` that is never coming.
    Failed { error: String },
    /// A kind this consumer does not know. Renderers ignore it rather than
    /// failing, so a driver can add events without breaking them.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_events_round_trip_and_tolerate_the_unknown() {
        let ev = SessionEvent::Delta {
            text: "hi".to_string(),
        };
        let text = serde_json::to_string(&ev).unwrap();
        assert_eq!(text, r#"{"kind":"delta","text":"hi"}"#);
        assert_eq!(serde_json::from_str::<SessionEvent>(&text).unwrap(), ev);

        let future: SessionEvent =
            serde_json::from_str(r#"{"kind":"thinking","text":"…"}"#).unwrap();
        assert_eq!(future, SessionEvent::Unknown);
    }

    #[test]
    fn a_turn_always_ends_with_exactly_one_terminal_event() {
        // A front end re-enables its input on these three and nothing else,
        // so their wire shapes are part of the contract.
        for (ev, text) in [
            (
                SessionEvent::Done {
                    text: "hi".to_string(),
                },
                r#"{"kind":"done","text":"hi"}"#,
            ),
            (SessionEvent::Cancelled, r#"{"kind":"cancelled"}"#),
            (
                SessionEvent::Failed {
                    error: "boom".to_string(),
                },
                r#"{"kind":"failed","error":"boom"}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&ev).unwrap(), text);
            assert_eq!(serde_json::from_str::<SessionEvent>(text).unwrap(), ev);
        }
    }

    #[test]
    fn a_session_id_names_its_own_topic() {
        let s = SessionId::new(3);
        assert_eq!(s.as_str(), "s3");
        assert_eq!(s.topic().as_str(), "model::session::s3");
        assert!(ALL_SESSIONS.matches(&s.topic()));
    }
}
