//! The peer node's HTTP surface, as seen from this side.
//!
//! `portos-bridge-http` did not invent a protocol. `/invoke` answers with the
//! same `{"ok":…}` / `{"err":…}` frame the ABI uses, and `/grants` with the
//! same `GrantsReply`. So this file parses the peer with `portos_abi::wire`
//! types rather than a private copy of them: the contract is written once,
//! and if either end ever changes shape it is a compile error rather than a
//! runtime surprise.
//!
//! Why this dials a socket directly instead of going through `egress::*`:
//! the broker is the chokepoint for calls a driver makes to the **outside
//! world**, which is where allowlists and credential injection belong. A
//! link to another node of the same workstation is not the outside world; it
//! is the fabric the workstation is made of, and the thing it connects to is
//! the peer of this driver, not a third party. Accounting does not go
//! missing either: a forwarded verb is audited twice — here as an ordinary
//! invoke on this node, and on the far node as an invoke by its bridge.

use portos_abi::ids::{Topic, Verb};
use portos_abi::wire::{Grant, GrantsReply, Payload, Reply};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A forwarded verb may legitimately take as long as the verb itself — a
/// page load, a shell command. What it may not do is never return: this
/// driver answers kernel calls on one thread, so one wedged link would wedge
/// every verb it serves.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// The far kernel's own refusal, forwarded in its own words. Kept
    /// distinct from a broken link on purpose: "denied" and "unreachable"
    /// call for different reactions, and one `String` never allowed telling
    /// them apart.
    #[error("{0}")]
    Refused(String),
    #[error("node unreachable: {0}")]
    Transport(Box<ureq::Error>),
    #[error("node sent an unreadable reply: {0}")]
    Body(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<ureq::Error> for LinkError {
    fn from(e: ureq::Error) -> LinkError {
        LinkError::Transport(Box::new(e))
    }
}

/// One event as the peer's bridge publishes it.
#[derive(Deserialize)]
pub struct PeerEvent {
    pub topic: Topic,
    pub data: Payload,
}

pub struct Link {
    base: String,
    calls: ureq::Agent,
    /// A second agent for the event stream. An idle stream is normal there,
    /// so it must not carry the read timeout a call needs.
    stream: ureq::Agent,
}

impl Link {
    pub fn new(base: &str) -> Link {
        Link {
            base: base.trim_end_matches('/').to_string(),
            calls: ureq::AgentBuilder::new()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout_read(CALL_TIMEOUT)
                .build(),
            stream: ureq::AgentBuilder::new()
                .timeout_connect(CONNECT_TIMEOUT)
                .build(),
        }
    }

    /// What the peer's bridge may invoke — which is the peer's own decision
    /// about what it exposes, written in its `chat.json` and read here
    /// rather than duplicated in ours.
    pub fn grants(&self) -> Result<Vec<Grant>, LinkError> {
        let body = self
            .calls
            .get(&format!("{}/grants", self.base))
            .call()?
            .into_string()?;
        Ok(serde_json::from_str::<GrantsReply>(&body)?.grants)
    }

    /// Forward one verb. Arguments cross as the bytes they already are; this
    /// driver has no more business reading them than the kernel does. The
    /// peer's kernel decides whether the call is allowed, and its refusal
    /// arrives as `err`.
    pub fn invoke(&self, verb: &Verb, args: &Payload) -> Result<Payload, LinkError> {
        let body = serde_json::to_string(&InvokeBody { verb, args })?;
        let text = self
            .calls
            .post(&format!("{}/invoke", self.base))
            .set("content-type", "application/json")
            .send_string(&body)?
            .into_string()?;
        match serde_json::from_str::<Reply<Payload>>(&text)? {
            Reply::Ok(p) => Ok(p),
            Reply::Err(e) => Err(LinkError::Refused(e)),
        }
    }

    /// Read the peer's event stream, handing each event to `on_event`, until
    /// the stream ends. `replay=0` because a reconnecting machine wants what
    /// happens next; the backlog is for a browser that reloaded.
    pub fn events(&self, mut on_event: impl FnMut(PeerEvent)) -> Result<(), LinkError> {
        let body = self
            .stream
            .get(&format!("{}/events?replay=0", self.base))
            .call()?
            .into_reader();
        // Server-sent events in the one shape the bridge emits: `data: <json>`
        // lines separated by blank ones. Not a general SSE client, and it
        // should not become one without a peer that needs it.
        for line in BufReader::new(body).lines() {
            let line = line?;
            let Some(json) = line.strip_prefix("data: ") else {
                continue;
            };
            match serde_json::from_str::<PeerEvent>(json) {
                Ok(ev) => on_event(ev),
                // One unreadable event is the peer's business, not a reason
                // to drop a working link.
                Err(e) => eprintln!("[remote] unreadable event: {e}"),
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct InvokeBody<'a> {
    verb: &'a Verb,
    args: &'a Payload,
}
