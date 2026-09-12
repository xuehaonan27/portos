//! The kernel↔plugin wire protocol, as types.
//!
//! Two kinds of content cross this boundary and they are deliberately not
//! the same type.
//!
//! The **envelope** — which operation, which verb, which topic, which byte
//! range — is a closed set the kernel fully understands and must be able to
//! reject. It is an enum, so a malformed frame fails to parse instead of
//! becoming a valid-looking request: the old code read
//! `req["verb"].as_str().unwrap_or("")`, which turned a corrupt frame into
//! an invoke of the verb `""` and carried on.
//!
//! The **payload** — a plugin's arguments, results and event data — is an
//! open set the kernel must *not* understand. It is [`Payload`], which holds
//! unparsed JSON bytes and offers no way to look inside. The kernel forwards
//! it. That turns domain ignorance from a rule contributors have to remember
//! into a property the compiler keeps.
//!
//! The wire bytes are unchanged from the untyped version: a 4-byte
//! little-endian length prefix, then a JSON object, with `op` as an inline
//! tag. Serde's internally-tagged enums cannot carry a `RawValue` (they
//! buffer through an intermediate that raw JSON cannot be read from), so the
//! tag is peeked first and the body parsed into its own struct — and
//! [`Serialize`] is written out by hand to match. The round-trip tests at the
//! bottom are what keep the two halves from drifting.

use crate::ids::{IdError, PluginName, SubId, Topic, Verb};
use crate::{ArtifactMeta, Label};
use serde::de::DeserializeOwned;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("malformed frame: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("unknown op: {0:?}")]
    UnknownOp(String),
    #[error(transparent)]
    Id(#[from] IdError),
}

/// Opaque plugin JSON: arguments, results, event data, tool schemas.
///
/// The kernel stores and forwards these bytes and has no method here for
/// reading a field out of them. Plugins, which own the domain meaning, use
/// [`Payload::parse`] to get their own typed view.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Payload(Box<RawValue>);

impl Payload {
    pub fn null() -> Payload {
        Payload(RawValue::from_string("null".to_string()).expect("null is valid json"))
    }

    /// Wrap a serializable value as an opaque payload.
    pub fn of<T: Serialize>(value: &T) -> Result<Payload, serde_json::Error> {
        Ok(Payload(serde_json::value::to_raw_value(value)?))
    }

    /// The domain-side view. Only a plugin that owns this verb's meaning has
    /// a `T` worth naming here.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(self.0.get())
    }

    /// The raw JSON text, for forwarding and for measuring context bytes.
    pub fn as_raw(&self) -> &str {
        self.0.get()
    }

    pub fn is_null(&self) -> bool {
        self.0.get() == "null"
    }
}

impl Default for Payload {
    fn default() -> Payload {
        Payload::null()
    }
}

impl PartialEq for Payload {
    /// Textual equality of the raw JSON — enough for tests; this type is not
    /// meant to be compared semantically.
    fn eq(&self, other: &Payload) -> bool {
        self.0.get() == other.0.get()
    }
}

/// What a driver advertises about one of its verbs, so a caller holding the
/// capability gets a ready-made tool definition. Opaque to the kernel, which
/// stores it and joins it into `grants` responses.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolMeta {
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Payload>,
}

// ---------------------------------------------------------------- hello ----

/// Which channel a connection is. A plugin opens `serve` first (declaring
/// which of the others follow), then those.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelRole {
    Serve,
    Client,
    Events,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelloFrame {
    pub hello: Hello,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hello {
    pub name: PluginName,
    pub abi: String,
    pub role: ChannelRole,
    #[serde(default)]
    pub token: String,
    /// Serve hello only: the verbs this plugin answers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verbs: Vec<Verb>,
    /// Serve hello only: which further channels will be opened. Absent means
    /// `["client"]`, which is what a plugin that interleaves events on the
    /// serve channel sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<ChannelRole>>,
    /// Serve hello only: per-verb tool metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<BTreeMap<Verb, ToolMeta>>,
    /// Serve hello only: verbs this plugin needs somebody to answer.
    ///
    /// A plugin is the only thing that knows what it cannot work without, so
    /// it is the only thing that says so. Until every one of these resolves,
    /// its own verbs are not routed — it is running but not usable, which is
    /// the same state a plugin is in between being stopped and coming back.
    ///
    /// This is a *need*, not a permission: the operator still decides whether
    /// this plugin may call them. The two are separate questions and were
    /// being answered by one (grants), which is why nothing could tell the
    /// difference between "not allowed" and "nobody there".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<Verb>,
}

impl Hello {
    /// The channels this plugin promised beyond `serve`.
    pub fn declared_channels(&self) -> Vec<ChannelRole> {
        self.channels
            .clone()
            .unwrap_or_else(|| vec![ChannelRole::Client])
    }
}

// -------------------------------------------------------------- replies ----

/// Every response frame: `{"ok": …}` or `{"err": "…"}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply<T> {
    Ok(T),
    Err(String),
}

impl<T> Reply<T> {
    pub fn into_result(self) -> Result<T, String> {
        match self {
            Reply::Ok(v) => Ok(v),
            Reply::Err(e) => Err(e),
        }
    }
}

/// One entry of `grants` introspection: a verb this plugin may invoke,
/// joined with what its driver advertised.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub verb: Verb,
    #[serde(default)]
    pub description: String,
    pub schema: Payload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts_left: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantsReply {
    pub grants: Vec<Grant>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmitReply {
    pub delivered: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubscribeReply {
    pub sub: SubId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnsubscribeReply {
    pub removed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PutReply {
    pub meta: ArtifactMeta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadReply {
    pub len: u64,
}

// ------------------------------------------------- plugin → kernel ops ----

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Invoke {
    pub verb: Verb,
    #[serde(default)]
    pub args: Payload,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Emit {
    pub topic: Topic,
    #[serde(default)]
    pub data: Payload,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Subscribe {
    pub topic: Topic,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Unsubscribe {
    pub sub: SubId,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Put {
    #[serde(rename = "type")]
    pub content_type: String,
    /// Absent or `null` both mean public-and-trusted.
    #[serde(default)]
    pub labels: Option<Label>,
}

/// Verbs a plugin takes on after it has connected.
///
/// A plugin does not always know what it answers before it is running:
/// anything that mirrors somebody else has to ask them first, and asking
/// requires being up. Declaring in `hello` is the ordinary case, not the
/// only one.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Claim {
    pub tools: BTreeMap<Verb, ToolMeta>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaimReply {
    pub claimed: u64,
}

/// Where an artifact's bytes are, rather than the bytes themselves.
///
/// The point of the answer is that it can be handed to something that only
/// speaks in file paths — `grep`, `python`, a build tool. Without it the
/// store is reachable only through this ABI, and every use of an artifact
/// has to travel through the model's context, which is precisely what the
/// store exists to prevent.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Locate {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocateReply {
    /// Absolute, and read-only on disk. A plugin in a form that does not
    /// share this filesystem gets an error instead — the launcher has to
    /// make the store reachable, the same way it must for the socket.
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Read {
    pub id: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub len: Option<u64>,
}

/// A request a plugin sends the kernel on its client channel.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientOp {
    Invoke(Invoke),
    Grants,
    Emit(Emit),
    Subscribe(Subscribe),
    Unsubscribe(Unsubscribe),
    Put(Put),
    Read(Read),
    Locate(Locate),
    Claim(Claim),
}

impl ClientOp {
    pub fn from_slice(bytes: &[u8]) -> Result<ClientOp, WireError> {
        Ok(match peek_op(bytes)?.as_str() {
            "invoke" => ClientOp::Invoke(serde_json::from_slice(bytes)?),
            "grants" => ClientOp::Grants,
            "emit" => ClientOp::Emit(serde_json::from_slice(bytes)?),
            "subscribe" => ClientOp::Subscribe(serde_json::from_slice(bytes)?),
            "unsubscribe" => ClientOp::Unsubscribe(serde_json::from_slice(bytes)?),
            "put" => ClientOp::Put(serde_json::from_slice(bytes)?),
            "read" => ClientOp::Read(serde_json::from_slice(bytes)?),
            "locate" => ClientOp::Locate(serde_json::from_slice(bytes)?),
            "claim" => ClientOp::Claim(serde_json::from_slice(bytes)?),
            other => return Err(WireError::UnknownOp(other.to_string())),
        })
    }

    /// The op tag, for audit lines and errors.
    pub fn tag(&self) -> &'static str {
        match self {
            ClientOp::Invoke(_) => "invoke",
            ClientOp::Grants => "grants",
            ClientOp::Emit(_) => "emit",
            ClientOp::Subscribe(_) => "subscribe",
            ClientOp::Unsubscribe(_) => "unsubscribe",
            ClientOp::Put(_) => "put",
            ClientOp::Read(_) => "read",
            ClientOp::Locate(_) => "locate",
            ClientOp::Claim(_) => "claim",
        }
    }
}

impl Serialize for ClientOp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("op", self.tag())?;
        match self {
            ClientOp::Invoke(v) => {
                m.serialize_entry("verb", &v.verb)?;
                m.serialize_entry("args", &v.args)?;
            }
            ClientOp::Grants => {}
            ClientOp::Emit(v) => {
                m.serialize_entry("topic", &v.topic)?;
                m.serialize_entry("data", &v.data)?;
            }
            ClientOp::Subscribe(v) => m.serialize_entry("topic", &v.topic)?,
            ClientOp::Unsubscribe(v) => m.serialize_entry("sub", &v.sub)?,
            ClientOp::Put(v) => {
                m.serialize_entry("type", &v.content_type)?;
                m.serialize_entry("labels", &v.labels)?;
            }
            ClientOp::Read(v) => {
                m.serialize_entry("id", &v.id)?;
                m.serialize_entry("offset", &v.offset)?;
                if let Some(len) = v.len {
                    m.serialize_entry("len", &len)?;
                }
            }
            ClientOp::Locate(v) => m.serialize_entry("id", &v.id)?,
            ClientOp::Claim(v) => m.serialize_entry("tools", &v.tools)?,
        }
        m.end()
    }
}

// ------------------------------------------------- kernel → plugin msgs ----

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Call {
    pub verb: Verb,
    #[serde(default)]
    pub args: Payload,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Event {
    pub sub: SubId,
    pub topic: Topic,
    #[serde(default)]
    pub data: Payload,
}

/// A message the kernel sends a plugin on its serve or events channel.
#[derive(Clone, Debug, PartialEq)]
pub enum ServeMsg {
    Call(Call),
    Shutdown,
    Event(Event),
}

impl ServeMsg {
    pub fn from_slice(bytes: &[u8]) -> Result<ServeMsg, WireError> {
        Ok(match peek_op(bytes)?.as_str() {
            "call" => ServeMsg::Call(serde_json::from_slice(bytes)?),
            "shutdown" => ServeMsg::Shutdown,
            "event" => ServeMsg::Event(serde_json::from_slice(bytes)?),
            other => return Err(WireError::UnknownOp(other.to_string())),
        })
    }

    pub fn tag(&self) -> &'static str {
        match self {
            ServeMsg::Call(_) => "call",
            ServeMsg::Shutdown => "shutdown",
            ServeMsg::Event(_) => "event",
        }
    }
}

impl Serialize for ServeMsg {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("op", self.tag())?;
        match self {
            ServeMsg::Call(v) => {
                m.serialize_entry("verb", &v.verb)?;
                m.serialize_entry("args", &v.args)?;
            }
            ServeMsg::Shutdown => {}
            ServeMsg::Event(v) => {
                m.serialize_entry("sub", &v.sub)?;
                m.serialize_entry("topic", &v.topic)?;
                m.serialize_entry("data", &v.data)?;
            }
        }
        m.end()
    }
}

/// An event as delivered to an in-process subscriber. Kernel-side consumers
/// (the chat loop) get this rather than a plugin frame: same content, no op
/// tag, because nothing is being dispatched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalEvent {
    pub sub: SubId,
    pub topic: Topic,
    #[serde(default)]
    pub data: Payload,
}

/// Read just the `op` tag. Internally-tagged enums would do this for us if
/// they could hold raw JSON; they cannot, so it is done explicitly.
fn peek_op(bytes: &[u8]) -> Result<String, WireError> {
    #[derive(Deserialize)]
    struct OpTag {
        op: String,
    }
    let tag: OpTag = serde_json::from_slice(bytes)?;
    Ok(tag.op)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verb(s: &str) -> Verb {
        Verb::parse(s).unwrap()
    }
    fn topic(s: &str) -> Topic {
        Topic::parse(s).unwrap()
    }

    /// Serialize is hand-written; these prove it still agrees with the
    /// derived parsers on every op.
    #[test]
    fn client_ops_round_trip() {
        let ops = vec![
            ClientOp::Invoke(Invoke {
                verb: verb("browser::open"),
                args: Payload::of(&serde_json::json!({"url": "https://example.com"})).unwrap(),
            }),
            ClientOp::Grants,
            ClientOp::Emit(Emit {
                topic: topic("model::session::s1"),
                data: Payload::of(&serde_json::json!({"kind": "delta"})).unwrap(),
            }),
            ClientOp::Subscribe(Subscribe {
                topic: topic("model::session::*"),
            }),
            ClientOp::Unsubscribe(Unsubscribe { sub: SubId::new(7) }),
            ClientOp::Put(Put {
                content_type: "image/png".to_string(),
                labels: Some(Label::with_integ("web:https://example.com")),
            }),
            ClientOp::Read(Read {
                id: "blake3:ab".to_string(),
                offset: 16,
                len: Some(4096),
            }),
            ClientOp::Locate(Locate {
                id: "blake3:ab".to_string(),
            }),
            ClientOp::Claim(Claim {
                tools: BTreeMap::from([(verb("remote::open"), ToolMeta::default())]),
            }),
        ];
        for op in ops {
            let bytes = serde_json::to_vec(&op).unwrap();
            assert_eq!(ClientOp::from_slice(&bytes).unwrap(), op, "round trip");
        }
    }

    #[test]
    fn serve_msgs_round_trip() {
        let msgs = vec![
            ServeMsg::Call(Call {
                verb: verb("model::send"),
                args: Payload::of(&serde_json::json!({"text": "hi"})).unwrap(),
            }),
            ServeMsg::Shutdown,
            ServeMsg::Event(Event {
                sub: SubId::new(3),
                topic: topic("egress::log"),
                data: Payload::null(),
            }),
        ];
        for msg in msgs {
            let bytes = serde_json::to_vec(&msg).unwrap();
            assert_eq!(ServeMsg::from_slice(&bytes).unwrap(), msg, "round trip");
        }
    }

    /// The wire bytes must not have changed: these are frames as the
    /// untyped code wrote them, and the JS client still writes them.
    #[test]
    fn parses_frames_in_the_established_wire_format() {
        let op =
            ClientOp::from_slice(br#"{"op":"invoke","verb":"browser::open","args":{"url":"x"}}"#)
                .unwrap();
        let ClientOp::Invoke(inv) = op else {
            panic!("expected invoke")
        };
        assert_eq!(inv.verb.family(), "browser");
        assert_eq!(inv.args.as_raw(), r#"{"url":"x"}"#);

        // The JS driver sends `labels: null` when a page has no origin.
        let op = ClientOp::from_slice(br#"{"op":"put","type":"image/png","labels":null}"#).unwrap();
        assert_eq!(
            op,
            ClientOp::Put(Put {
                content_type: "image/png".into(),
                labels: None
            })
        );

        // `len` is omitted for a read-to-end.
        let op = ClientOp::from_slice(br#"{"op":"read","id":"blake3:ab","offset":0}"#).unwrap();
        assert_eq!(
            op,
            ClientOp::Read(Read {
                id: "blake3:ab".into(),
                offset: 0,
                len: None
            })
        );
    }

    #[test]
    fn malformed_frames_are_rejected_instead_of_defaulted() {
        // Each of these was previously accepted and turned into a
        // valid-looking request by an `unwrap_or`.
        assert!(
            ClientOp::from_slice(br#"{"op":"invoke"}"#).is_err(),
            "no verb"
        );
        assert!(
            ClientOp::from_slice(br#"{"op":"invoke","verb":"nocolons"}"#).is_err(),
            "unparseable verb"
        );
        assert!(
            ClientOp::from_slice(br#"{"op":"unsubscribe"}"#).is_err(),
            "no sub id"
        );
        assert!(
            ClientOp::from_slice(br#"{"op":"subscribe","topic":""}"#).is_err(),
            "empty topic"
        );
        assert!(ClientOp::from_slice(br#"{"op":"read"}"#).is_err(), "no id");
        assert!(
            ClientOp::from_slice(br#"{"op":"nope"}"#).is_err(),
            "unknown op"
        );
        assert!(ClientOp::from_slice(b"not json").is_err(), "not json");
    }

    #[test]
    fn replies_use_the_established_shape() {
        let ok: Reply<EmitReply> = serde_json::from_slice(br#"{"ok":{"delivered":2}}"#).unwrap();
        assert_eq!(ok, Reply::Ok(EmitReply { delivered: 2 }));
        let err: Reply<EmitReply> = serde_json::from_slice(br#"{"err":"denied"}"#).unwrap();
        assert_eq!(err, Reply::Err("denied".into()));
        assert_eq!(
            serde_json::to_string(&Reply::Ok(EmitReply { delivered: 2 })).unwrap(),
            r#"{"ok":{"delivered":2}}"#
        );
    }

    #[test]
    fn hello_defaults_channels_to_client_only() {
        let h: HelloFrame = serde_json::from_slice(
            br#"{"hello":{"name":"portos-browser","abi":"0.2","role":"serve","token":"t","verbs":["browser::open"]}}"#,
        )
        .unwrap();
        assert_eq!(h.hello.declared_channels(), vec![ChannelRole::Client]);
        assert_eq!(h.hello.verbs[0].short(), "open");
    }
}
