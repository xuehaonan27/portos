//! The `model::*` driver interface.
//!
//! The contract between a model driver and whoever drives it: how a session
//! is opened, fed and closed, and what a session publishes while a turn is
//! running. Provider-neutral by construction — nothing here names Anthropic
//! or any other vendor, because a provider is an implementation detail one
//! level further down.

use portos_abi::ids::{IdError, Topic, Verb};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The directory a model driver is given under the runtime root, and the
/// variable it arrives in. Stated here rather than in one implementation
/// because the session index lives in it (`SessionIndex`): a driver that
/// replaces another finds the conversations where the last one left them.
pub const DIR: &str = "modeld";
pub const DIR_ENV: &str = "PORTOS_MODELD_DIR";

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

    /// Parse an id that came from outside — a command-line flag, a stored
    /// index. Validated against exactly what [`topic`] promises, so that
    /// method's `expect` stays true for ids this process did not mint.
    ///
    /// [`topic`]: SessionId::topic
    pub fn parse(s: &str) -> Result<SessionId, IdError> {
        let ok = !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if ok {
            Ok(SessionId(s.to_string()))
        } else {
            Err(IdError::Topic(s.to_string()))
        }
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

/// What has been stored, newest first. Answered from the driver's index —
/// handles and small facts, never a transcript — so a front end choosing a
/// conversation to continue never reads one, and never reads the driver's
/// files either.
pub static SESSIONS: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("model::sessions").expect("constant verb"));

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionsArgs {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionsReply {
    /// Most recently touched first.
    pub sessions: Vec<SessionEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: SessionId,
    #[serde(flatten)]
    pub record: SessionRecord,
}

pub static ALL_SESSIONS: LazyLock<Topic> =
    LazyLock::new(|| Topic::parse("model::session::*").expect("constant topic"));

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StartArgs {
    /// Overrides the driver's configured system prompt for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Continue a stored conversation instead of opening a new one. Starting
    /// is the same act either way — the driver either finds a transcript
    /// under this id or fails — so there is no separate `resume` verb to
    /// forget to call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<SessionId>,
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

impl SessionEvent {
    /// Whether this event ends the turn. Exactly one of these is owed to a
    /// session's subscribers — it is the cue a front end acts on, so by the
    /// time one goes out the session must already be free to take the next
    /// turn.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SessionEvent::Done { .. } | SessionEvent::Cancelled | SessionEvent::Failed { .. }
        )
    }
}

/// The stored-session index: the contract between the driver that writes
/// conversations down and whoever lists them.
///
/// It holds **handles and small facts only** — the transcripts themselves are
/// artifacts in the CAS. That is what keeps listing every session cheap no
/// matter how long they got, and it is the same split the model's own context
/// obeys.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionIndex {
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionRecord>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionRecord {
    /// CAS handle of the transcript.
    pub artifact: String,
    /// User messages, which is what a person means by "how long is it".
    pub turns: usize,
    /// Unix seconds.
    pub updated_at: u64,
    /// Enough of the opening line to recognise the conversation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
}

impl SessionIndex {
    /// Where it lives, stated once so a reader and the writer cannot disagree.
    pub fn path_in(modeld_dir: &Path) -> PathBuf {
        modeld_dir.join("sessions.json")
    }

    pub fn read(modeld_dir: &Path) -> SessionIndex {
        std::fs::read_to_string(SessionIndex::path_in(modeld_dir))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Most recently touched first — the order anyone listing them wants.
    pub fn by_recency(&self) -> Vec<(&String, &SessionRecord)> {
        let mut v: Vec<_> = self.sessions.iter().collect();
        v.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at).then(b.0.cmp(a.0)));
        v
    }

    /// The highest `sN` recorded. A restarted driver must not hand out an id
    /// that already names a stored conversation.
    pub fn highest_id(&self) -> u64 {
        self.sessions
            .keys()
            .filter_map(|k| k.strip_prefix('s')?.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
    }
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
