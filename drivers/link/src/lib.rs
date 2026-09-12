//! The `link` interface: one PortOS node's two interaction shapes, on a
//! socket, for another process to drive.
//!
//! Not a verb family — nothing inside a node calls it. It is what a
//! presenter (a browser console) or another node (`plugins/remote`) speaks
//! to a bridge (`plugins/bridge-http`), and it invents nothing: `/invoke`
//! answers with the ABI's own `Reply<Payload>`, `/grants` with its
//! `GrantsReply`, and an event is `{topic, data}`. Stated once here so the
//! server and its clients cannot disagree about the shape; the JS server
//! follows this file.
//!
//!   GET  /grants            → `GrantsReply`: what the bridge may invoke,
//!                             which is the node's own statement of what it
//!                             exposes
//!   POST /invoke            ← [`InvokeRequest`] → `Reply<Payload>`
//!   GET  /events?replay=N   → server-sent events, one `data: <Event>` line
//!                             per event; `replay=0` skips the backlog
//!   GET  /artifact/<id>     → the bytes (the data plane never rides /events)
//!
//! Whatever connects acts with the **bridge's** grants: what the far node is
//! willing to serve is decided there, in its `portos.json`, and read here.

use portos_abi::ids::{PluginName, Topic, Verb};
use portos_abi::wire::Payload;
use serde::{Deserialize, Serialize};

pub const GRANTS: &str = "/grants";
pub const INVOKE: &str = "/invoke";
pub const EVENTS: &str = "/events";
pub const ARTIFACT: &str = "/artifact/";

/// One forwarded verb. Arguments cross as the bytes they already are.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvokeRequest {
    pub verb: Verb,
    #[serde(default)]
    pub args: Payload,
    /// Which instance on the far node, when the caller knows; the far kernel
    /// refuses to choose between several, the way this one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<PluginName>,
}

/// One event, as the bridge publishes it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub topic: Topic,
    #[serde(default)]
    pub data: Payload,
}
