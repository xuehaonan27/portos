//! Subscriptions backed by holdings and bounded event delivery.

use super::cleanup::{HostWorld, reclaim};
use super::{EVENT_QUEUE, HostInner, PluginHandle};
use crate::ledger::{CLASS_SUBSCRIPTION, ExclusiveRequest};
use crate::{Kernel, KernelError};
use portos_proto::frame;
use portos_rm::cleanup::CleanupTarget;
use portos_rm::identity::{ClassId, HoldingHandle, InstanceId, ResourceKey, SubjectId};
use portos_rm::time::{LeaseRequest, Timestamp};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

pub(super) enum SubTarget {
    Local(SyncSender<Value>),
    Plugin(String),
}

pub(super) struct Sub {
    pub(super) id: u64,
    pub(super) topic: String,
    pub(super) target: SubTarget,
    /// The `kernel/subscription` holding backing this subscription.
    pub(super) holding: HoldingHandle,
}

/// Per-plugin thread draining the bounded event queue onto the plugin's
/// events channel (or the serve channel, when it declared none).
pub(super) fn spawn_event_pump(handle: Arc<PluginHandle>, rx: Receiver<Value>) {
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

/// Subscribe an arbitrary kernel-side subject (e.g. a plan run's segment) to
/// a topic. `Host::subscribe_local` is this with the `kernel` subject.
pub(super) fn subscribe_with_subject(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    subject: &str,
    topic: &str,
) -> Result<(u64, Receiver<Value>), KernelError> {
    let (tx, rx) = sync_channel::<Value>(EVENT_QUEUE);
    let id = inner
        .next_sub
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
        .map_err(|_| KernelError::Denied("subscription IDs exhausted".into()))?;
    let mut subs = inner.subs.lock().unwrap();
    let holding = kernel.ledger.hold_managed(
        ExclusiveRequest {
            owner: SubjectId::new(subject),
            resource: ResourceKey::new(
                ClassId::new(CLASS_SUBSCRIPTION),
                InstanceId::new(format!("{}/{}", inner.witness.session(), id)),
            ),
            generation: inner.witness.session().clone(),
            parent: None,
            lease: LeaseRequest::UseClassDefault,
        },
        CleanupTarget::Subscription {
            host: inner.witness.clone(),
            subscription: id,
        },
        None,
        Timestamp::try_from(crate::db::now_unix()).map_err(crate::ledger::map_err)?,
    )?;
    subs.push(Sub {
        id,
        topic: topic.to_string(),
        target: SubTarget::Local(tx),
        holding,
    });
    Ok((id, rx))
}

/// Deliver an event to every matching subscriber. Overflow policy: a local
/// subscriber is dropped; a plugin subscriber's whole connection is cut
/// (m0 §3 — never let a slow consumer wedge the kernel).
pub(super) fn dispatch_event(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    topic: &str,
    data: Value,
) -> usize {
    enum Target {
        Local(u64, HoldingHandle, SyncSender<Value>),
        Plugin(u64, HoldingHandle, String),
    }
    let targets: Vec<Target> = {
        let subs = inner.subs.lock().unwrap();
        subs.iter()
            .filter(|s| topic_matches(&s.topic, topic))
            .map(|s| match &s.target {
                SubTarget::Local(tx) => Target::Local(s.id, s.holding.clone(), tx.clone()),
                SubTarget::Plugin(name) => Target::Plugin(s.id, s.holding.clone(), name.clone()),
            })
            .collect()
    };
    let mut delivered = 0usize;
    let mut drop_subs: Vec<(u64, HoldingHandle)> = Vec::new();
    let mut kill_plugins: Vec<String> = Vec::new();
    for t in targets {
        let holding = match &t {
            Target::Local(_, h, _) | Target::Plugin(_, h, _) => h,
        };
        let Some(current) = kernel
            .ledger
            .holding(holding.id())
            .ok()
            .flatten()
            .filter(|h| h.state.is_active() && h.handle() == *holding)
        else {
            continue;
        };
        match t {
            Target::Local(id, holding, tx) => {
                let ev = json!({"topic": topic, "data": data, "sub": id});
                match tx.try_send(ev) {
                    Ok(()) => delivered += 1,
                    Err(TrySendError::Full(_)) => {
                        drop_subs.push((id, holding));
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "events.overflow", "sub": id, "topic": topic,
                        }));
                    }
                    Err(TrySendError::Disconnected(_)) => drop_subs.push((id, holding)),
                }
            }
            Target::Plugin(id, holding, name) => {
                let tx = inner
                    .plugins
                    .lock()
                    .unwrap()
                    .get(&name)
                    .filter(|h| Some(h.holding.id()) == current.parent)
                    .map(|h| h.events_tx.clone());
                let Some(tx) = tx else {
                    drop_subs.push((id, holding));
                    continue;
                };
                let ev = json!({"op": "event", "sub": id, "topic": topic, "data": data});
                match tx.try_send(ev) {
                    Ok(()) => delivered += 1,
                    Err(TrySendError::Full(_)) => {
                        // A plugin that cannot keep up is reclaimed on the
                        // one teardown path (its subscriptions go with it).
                        kill_plugins.push(name.clone());
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "events.overflow", "sub": id, "topic": topic,
                            "plugin": name,
                        }));
                    }
                    Err(TrySendError::Disconnected(_)) => drop_subs.push((id, holding)),
                }
            }
        }
    }
    if !drop_subs.is_empty() {
        let now = crate::db::now_unix();
        for (_, holding) in drop_subs {
            let _ = kernel.ledger.release_with_world(
                &holding,
                &mut HostWorld {
                    inner: inner.clone(),
                },
                Timestamp::try_from(now).expect("system timestamp in range"),
            );
        }
    }
    for name in kill_plugins {
        reclaim(kernel, inner, &name, "events.overflow");
    }
    delivered
}

/// Topic patterns: a trailing `*` matches any topic with that prefix
/// (`model::session::*`); otherwise exact match.
fn topic_matches(pattern: &str, topic: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => topic.starts_with(prefix),
        None => pattern == topic,
    }
}
