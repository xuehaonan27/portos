//! The event plane: delivery to every matching subscriber, bounded so a
//! slow consumer can never stall the kernel, and the per-plugin pump that
//! carries events onto its channel.

use super::teardown::cleanup_plugin;
use super::{HostInner, PluginHandle, SubTarget};
use crate::Kernel;
use portos_abi::frame;
use portos_abi::ids::{PluginName, SubId, Topic};
use portos_abi::wire::{Event, LocalEvent, Payload, ServeMsg};
use serde_json::json;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

/// Per-plugin thread draining the bounded event queue onto the plugin's
/// events channel (or the serve channel, when it declared none).
pub(super) fn spawn_event_pump(handle: Arc<PluginHandle>, rx: Receiver<ServeMsg>) {
    std::thread::spawn(move || {
        for ev in rx {
            let target = handle.events.as_ref().unwrap_or(&handle.serve);
            let mut s = target.lock().unwrap();
            if frame::write_frame(&mut *s, &ev).is_err() {
                return;
            }
        }
    });
}
/// Why a delivery failed: the queue is full (and, for a plugin, whose), or
/// the subscriber is gone.
enum Overflow {
    Full(Option<PluginName>),
    Gone,
}
/// Deliver an event to every matching subscriber. Overflow policy: a local
/// subscriber is dropped; a plugin subscriber's whole connection is cut —
/// never let a slow consumer wedge the kernel.
pub(super) fn dispatch_event(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    topic: &Topic,
    data: &Payload,
) -> u64 {
    enum Target {
        Local(SubId, SyncSender<LocalEvent>),
        Plugin(SubId, PluginName),
    }
    let targets: Vec<Target> = {
        let subs = inner.subs.lock().unwrap();
        subs.iter()
            .filter(|s| s.pattern.matches(topic))
            .map(|s| match &s.target {
                SubTarget::Local(tx) => Target::Local(s.id, tx.clone()),
                SubTarget::Plugin(name) => Target::Plugin(s.id, name.clone()),
            })
            .collect()
    };
    let mut delivered = 0u64;
    let mut drop_subs: Vec<SubId> = Vec::new();
    let mut kill_plugins: Vec<PluginName> = Vec::new();
    for t in targets {
        let (id, sent) = match t {
            Target::Local(id, tx) => {
                let ev = LocalEvent {
                    sub: id,
                    topic: topic.clone(),
                    data: data.clone(),
                };
                let sent = tx.try_send(ev).map_err(|e| match e {
                    TrySendError::Full(_) => Overflow::Full(None),
                    TrySendError::Disconnected(_) => Overflow::Gone,
                });
                (id, sent)
            }
            Target::Plugin(id, name) => {
                let tx = inner
                    .plugins
                    .lock()
                    .unwrap()
                    .get(&name)
                    .map(|h| h.events_tx.clone());
                let Some(tx) = tx else {
                    drop_subs.push(id);
                    continue;
                };
                let ev = ServeMsg::Event(Event {
                    sub: id,
                    topic: topic.clone(),
                    data: data.clone(),
                });
                let sent = tx.try_send(ev).map_err(|e| match e {
                    TrySendError::Full(_) => Overflow::Full(Some(name)),
                    TrySendError::Disconnected(_) => Overflow::Gone,
                });
                (id, sent)
            }
        };
        match sent {
            Ok(()) => delivered += 1,
            Err(Overflow::Gone) => drop_subs.push(id),
            Err(Overflow::Full(plugin)) => {
                drop_subs.push(id);
                let _ = kernel.audit.lock().unwrap().append(json!({
                    "event": "events.overflow",
                    "sub": id.get(),
                    "topic": topic.as_str(),
                    "plugin": plugin.as_ref().map(PluginName::as_str),
                }));
                if let Some(name) = plugin {
                    kill_plugins.push(name);
                }
            }
        }
    }
    if !drop_subs.is_empty() {
        inner
            .subs
            .lock()
            .unwrap()
            .retain(|s| !drop_subs.contains(&s.id));
    }
    for name in kill_plugins {
        if let Some(h) = inner.plugins.lock().unwrap().remove(&name) {
            cleanup_plugin(inner, &name);
            let _ = h.child.lock().unwrap().kill();
        }
    }
    delivered
}
