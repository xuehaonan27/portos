//! portos-remote: another PortOS node's capabilities, as local verbs.
//!
//! Multi-node with no multi-node protocol and no kernel change on either
//! side. The far node already knows how to put its two interaction shapes on
//! a socket — that is `portos-bridge-http`, written for a browser and
//! indifferent to who actually connects. This driver is the other end of
//! that socket: it asks the far node what it exposes and declares the answer
//! here as ordinary verbs. To this node's kernel it is one more plugin.
//!
//! The whole translation is one rule: **the family gains the node's name.**
//! `browser::open` on node `mac` is `mac_browser::open` here, and an event
//! on `model::session::s1` arrives as `mac_model::session::s1`. That is what
//! keeps two nodes' browsers from colliding in a flat route table, and it
//! makes a grant read honestly — `driver:mac_browser` is the Mac's browser,
//! not this machine's.
//!
//! Authority is checked twice, and the two halves are owned in different
//! places, which is the property worth keeping: the far node decides **what
//! is exposed** (the grants on its bridge, read here at startup), this node
//! decides **who may use it** (grants on `driver:<node>_<family>`). Neither
//! side can overrule the other, and neither had to learn anything new.
//!
//! Config (environment):
//!   PORTOS_REMOTE_URL    required — the peer bridge, e.g. http://127.0.0.1:7777
//!   PORTOS_REMOTE_NODE   required — the far node's local nickname. It becomes
//!                        a family prefix, so it must be a verb segment.
//!
//! Known gap, stated rather than hidden: an artifact handle in a reply names
//! bytes in the *peer's* CAS, and this driver does not fetch them, so a
//! screenshot taken on the far node cannot be dereferenced here. Content
//! addressing means the fix is materialisation rather than translation — the
//! same bytes get the same id on both nodes — but nothing needs it yet.

mod link;

use link::{Link, LinkError};
use portos_proto::ids::{Topic, Verb};
use portos_proto::wire::ToolMeta;
use portos_sdk::{CallError, CallResult, KernelClient, Plugin};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let url = require("PORTOS_REMOTE_URL")?;
    let node = require("PORTOS_REMOTE_NODE")?;
    // The prefix has to survive `Verb::parse` on every verb it will form.
    // Failing here beats failing once per verb with a less obvious message.
    Verb::new(&format!("{node}_family"), "verb").map_err(|_| {
        std::io::Error::other(format!(
            "PORTOS_REMOTE_NODE must be a verb segment ([a-z][a-z0-9_]*, no `__`): {node:?}"
        ))
    })?;

    let link = Arc::new(Link::new(&url));
    let mirrored = discover(&link, &node).map_err(std::io::Error::other)?;
    eprintln!(
        "[remote] node {node} at {url}: {} verb(s) mirrored",
        mirrored.len()
    );

    let name = format!("portos-remote-{node}");
    let verbs: Vec<String> = mirrored
        .iter()
        .map(|m| m.local.as_str().to_string())
        .collect();
    let verb_refs: Vec<&str> = verbs.iter().map(String::as_str).collect();
    let tools: BTreeMap<&str, ToolMeta> = mirrored
        .iter()
        .map(|m| (m.local.as_str(), m.meta.clone()))
        .collect();
    let routes: BTreeMap<Verb, Verb> = mirrored
        .iter()
        .map(|m| (m.local.clone(), m.peer.clone()))
        .collect();

    let event_link = link.clone();
    let event_node = node.clone();
    portos_sdk::serve(
        Plugin::new(&name, &verb_refs)
            .with_tools(tools)
            .on_ready(move |client| {
                // The link's other direction runs on its own thread for the
                // life of the plugin: nothing calls it, so it has no verb to
                // hang off.
                let client = client.clone();
                std::thread::spawn(move || pump_events(&event_link, &event_node, &client));
                Ok(())
            }),
        move |verb, args, _client| -> CallResult {
            let peer = routes
                .get(verb)
                .ok_or_else(|| CallError::from(format!("node {node} does not serve {verb}")))?;
            link.invoke(peer, args)
                .map_err(|e| CallError::from(e.to_string()))
        },
        // Events travel the other way; this driver subscribes to nothing.
        |_topic, _data| {},
    )
}

fn require(key: &str) -> std::io::Result<String> {
    std::env::var(key).map_err(|_| std::io::Error::other(format!("{key} unset")))
}

/// One verb this driver mirrors: what it is called here, what it is called
/// there, and what the far node said about it.
struct Mirror {
    local: Verb,
    peer: Verb,
    meta: ToolMeta,
}

/// Ask the peer what it exposes and give each verb its local name.
///
/// The answer is already a tool surface — verb, description, schema — because
/// on that node the same join built its model's tools. Carrying it across
/// unchanged is what makes a remote verb *usable* by a model here rather than
/// merely reachable.
fn discover(link: &Link, node: &str) -> Result<Vec<Mirror>, LinkError> {
    let mut mirrored = Vec::new();
    for g in link.grants()? {
        let local = match Verb::new(&format!("{node}_{}", g.verb.family()), g.verb.short()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[remote] skipping {}: {e}", g.verb);
                continue;
            }
        };
        let description = format!("{} Runs on node {node}.", g.description)
            .trim()
            .to_string();
        let schema = (!g.schema.is_null()).then_some(g.schema);
        mirrored.push(Mirror {
            local,
            peer: g.verb,
            meta: ToolMeta {
                description,
                schema,
            },
        });
    }
    Ok(mirrored)
}

/// Carry the peer's events onto this node's bus, renamed the way its verbs
/// are renamed.
///
/// It reconnects, because an `ssh -L` tunnel is a normal thing to lose.
/// Events that happen while the link is down are lost — the honest cost of
/// not asking the peer to keep a queue for us. Verbs are unaffected: they are
/// request/reply and fail loudly.
fn pump_events(link: &Link, node: &str, client: &KernelClient) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let opened = Instant::now();
        let outcome = link.events(|ev| {
            // A topic's text begins with its leading segment, so prefixing
            // the string is exactly the family rename the verbs get.
            match Topic::parse(&format!("{node}_{}", ev.topic)) {
                Ok(topic) => {
                    let _ = client.emit(&topic, ev.data);
                }
                Err(e) => eprintln!("[remote] undeliverable topic {}: {e}", ev.topic),
            }
        });
        if let Err(e) = outcome {
            eprintln!("[remote] event stream: {e}");
        }
        // A link that stayed up for a while and then dropped is a blip, not
        // a dead node; only repeated quick failures earn a longer wait.
        if opened.elapsed() > Duration::from_secs(30) {
            backoff = Duration::from_secs(1);
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}
