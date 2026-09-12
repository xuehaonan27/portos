//! portos-remote: another PortOS node's capabilities, as local verbs.
//!
//! Multi-node with no multi-node protocol and no kernel change on either
//! side. The far node already knows how to put its two interaction shapes on
//! a socket — that is `portos-bridge-http`, written for a browser and
//! indifferent to who actually connects. This driver is the other end of
//! that socket: it asks the far node what it exposes and declares the answer
//! here as ordinary verbs. To this node's kernel it is one more plugin.
//!
//! Nothing is renamed. `browser::open` there is `browser::open` here,
//! because a verb names the driver's verb and not who answers it; what
//! tells the Mac's browser from this machine's is the *instance* — this
//! plugin, under its own name — and a caller with two browsers names the one
//! it means. An earlier version prefixed the node onto the driver
//! (`mac_browser::open`), which encoded the instance into a name nothing
//! could parse back; the router's laws now carry it instead.
//!
//! Authority is checked twice, and the two halves are owned in different
//! places, which is the property worth keeping: the far node decides **what
//! is exposed** (the grants on its bridge, read here at startup), this node
//! decides **who may use it** (grants on `driver:<driver>` here). Neither
//! side can overrule the other, and neither had to learn anything new.
//!
//! Config (from the launch spec):
//!   `{"url": "http://127.0.0.1:7777", "node": "mac"}`
//!
//! Known gaps, stated rather than hidden. An artifact handle in a reply
//! names bytes in the *peer's* CAS, and this driver does not fetch them, so a
//! screenshot taken on the far node cannot be dereferenced here; content
//! addressing means the fix is materialisation rather than translation. And
//! an event carries no instance: the far node's `model::session::s1` and
//! this node's arrive on one topic, indistinguishable — the trigger for
//! giving events an origin is a front end that needs to tell them apart,
//! and none does yet.

mod link;

use link::{Link, LinkError};
use portos_abi::ids::{PluginName, Verb};
use portos_abi::wire::ToolMeta;
use portos_sdk::{CallError, KernelClient, Plugin};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What this driver needs to know.
#[derive(Deserialize)]
struct Config {
    /// The peer bridge, e.g. `http://127.0.0.1:7777`.
    url: String,
    /// The far node's local nickname. It names this instance
    /// (`portos-remote-<node>`) unless the launcher names it otherwise, and
    /// it tells the model where a verb runs.
    node: String,
}

fn main() -> std::io::Result<()> {
    let cfg: Config = portos_sdk::config::config().map_err(std::io::Error::other)?;
    let (url, node) = (cfg.url, cfg.node);
    let default_name = format!("portos-remote-{node}");
    PluginName::parse(&default_name).map_err(|_| {
        std::io::Error::other(format!(
            "node must make a plugin name ([a-z0-9_-]): {node:?}"
        ))
    })?;

    let link = Arc::new(Link::new(&url));
    let event_link = link.clone();

    // Serving comes first and discovery second. Asking the far node what it
    // offers means being able to reach it, and a node that is not up yet is
    // not a reason to fail to start — it is a reason to answer nothing until
    // it is. The verbs are taken on once the answer arrives.
    portos_sdk::serve(
        Plugin::new(default_name).on_ready(move |registrar, client| {
            // On its own thread, and retrying: until the peer answers, this
            // driver runs and answers nothing — the same state as a plugin
            // waiting on a dependency, because that is what it is.
            let registrar = registrar.clone();
            let mirror_link = link.clone();
            let mirror_node = node.clone();
            std::thread::spawn(move || mirror(&registrar, &mirror_link, &mirror_node));

            // The link's other direction runs on its own thread for the life
            // of the plugin: nothing calls it, so it has no verb to hang off.
            let client = client.clone();
            std::thread::spawn(move || pump_events(&event_link, &client));
            Ok(())
        }),
        // Events travel the other way; this driver subscribes to nothing.
        |_topic, _data| {},
    )
}

/// Ask the peer what it offers and take those verbs on. Keeps asking: the
/// far node may simply not be up yet, and "not yet" is not "never".
fn mirror(registrar: &portos_sdk::Registrar<'static>, link: &Arc<Link>, node: &str) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match discover(link, node) {
            Ok(mirrored) => {
                eprintln!("[remote] node {node}: {} verb(s) mirrored", mirrored.len());
                for m in mirrored {
                    let link = link.clone();
                    let verb = m.verb.clone();
                    let schema = m
                        .meta
                        .schema
                        .as_ref()
                        .and_then(|p| p.parse().ok())
                        .unwrap_or_else(|| serde_json::json!({"type": "object"}));
                    if let Err(e) =
                        registrar.tool(&m.verb, &m.meta.description, schema, move |args, _c| {
                            link.invoke(&verb, args)
                                .map_err(|e| CallError::from(e.to_string()))
                        })
                    {
                        eprintln!("[remote] could not take on {}: {e}", m.verb);
                    }
                }
                return;
            }
            Err(e) => {
                eprintln!("[remote] node {node} not reachable yet ({e}); retrying");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// One verb this driver mirrors, and what the far node said about it.
struct Mirror {
    verb: Verb,
    meta: ToolMeta,
}

/// Ask the peer what it exposes.
///
/// The answer is already a tool surface — verb, description, schema — because
/// on that node the same join built its model's tools. Carrying it across
/// unchanged is what makes a remote verb *usable* by a model here rather than
/// merely reachable. The one addition is where it runs, because a model
/// choosing between two browsers needs to know which is which.
fn discover(link: &Link, node: &str) -> Result<Vec<Mirror>, LinkError> {
    let mut mirrored = Vec::new();
    for g in link.grants()? {
        let description = format!("{} Runs on node {node}.", g.description)
            .trim()
            .to_string();
        let schema = (!g.schema.is_null()).then_some(g.schema);
        mirrored.push(Mirror {
            verb: g.verb,
            meta: ToolMeta {
                description,
                schema,
            },
        });
    }
    Ok(mirrored)
}

/// Carry the peer's events onto this node's bus, under their own topics.
///
/// It reconnects, because an `ssh -L` tunnel is a normal thing to lose.
/// Events that happen while the link is down are lost — the honest cost of
/// not asking the peer to keep a queue for us. Verbs are unaffected: they are
/// request/reply and fail loudly.
fn pump_events(link: &Link, client: &KernelClient) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let opened = Instant::now();
        let outcome = link.events(|ev| {
            let _ = client.emit(&ev.topic, ev.data);
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
