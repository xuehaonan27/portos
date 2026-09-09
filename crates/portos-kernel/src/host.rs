//! Plugin host: lifecycle, calls, subscriptions and plan services.
//!
//! Each plugin connects over separate serve and client channels, plus an
//! optional event channel. Calls to one plugin are serialized. Capability
//! and protocol checks happen before dispatch; cleanup runs after durable
//! claims and outside storage transactions.
//!
//! Declaration decoding, process startup, routing, events, resource requests
//! and cleanup are private modules. Plans use the `PlanRuntime` interface.

mod cleanup;
mod client;
mod declaration;
mod events;
mod lifecycle;
mod resources;
mod routing;
mod runtime;

use crate::ledger::capture_process;
use crate::{Kernel, KernelError};
use cleanup::{HostWorld, host_registry, reclaim, reclaim_holding};
pub(crate) use cleanup::{cleanup_known_host, local_host_gone};
use events::{Sub, SubTarget, dispatch_event, subscribe_with_subject};
use lifecycle::{rand_token, spawn_plugin};
use portos_rm::cleanup::HostWitness;
use portos_rm::coeffect::Flat;
use portos_rm::identity::{Generation, HoldingHandle};
use portos_rm::time::Timestamp;
use routing::{ProtocolSession, RouteEntry, call_on};
use runtime::HostRuntime;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

/// Bounded event queue per subscriber. A subscriber that falls this far
/// behind is cut off (m0-kernel-v0.md §3: slow consumers must not stall the
/// kernel): plugin subscribers are disconnected, local subscribers dropped.
pub const EVENT_QUEUE: usize = 256;

const SPAWN_DEADLINE_MS: u64 = 10_000;

struct PluginHandle {
    child: Mutex<std::process::Child>,
    serve: Mutex<UnixStream>,
    /// Dedicated event-delivery stream, when the plugin declared one.
    /// Without it, events interleave on the serve channel.
    events: Option<Mutex<UnixStream>>,
    events_tx: SyncSender<Value>,
    sock_path: PathBuf,
    /// The plugin's `kernel/plugin` holding (F1) and its generation (the
    /// spawn token): the stable denotation of this incarnation.
    holding: HoldingHandle,
    pid: u32,
    /// Position ceiling (F5 effect row) when spawned into a slot: the verbs
    /// this plugin may invoke, before any capability is consulted.
    offers: Option<Flat>,
    /// Declared verb-order protocol (F6) and its current state.
    protocol: Mutex<Option<ProtocolSession>>,
    /// Set by a graceful shutdown before the polite frame, so the exit path
    /// that races it reports the true reason.
    shutting_down: std::sync::atomic::AtomicBool,
}

/// A slot a plugin is spawned into (F5 `Mount`): `offers` is the row — the
/// verbs a plugin here may invoke at most; `provides` names services (verb
/// families) the slot guarantees present besides those already routed.
#[derive(Clone, Debug, Default)]
pub struct Slot {
    pub offers: Vec<String>,
    pub provides: Vec<String>,
}

struct HostInner {
    plugins: Mutex<BTreeMap<String, Arc<PluginHandle>>>,
    routes: Mutex<BTreeMap<String, RouteEntry>>, // verb -> route
    subs: Mutex<Vec<Sub>>,
    next_sub: AtomicU64,
    witness: HostWitness,
    next_spawn: AtomicU64,
    sock_dir: PathBuf,
    meter: Mutex<crate::metrics::ContextMeter>,
    /// Lease sweeper (WP-02): stop flag and thread handle; stopped on drop.
    sweeper_stop: Arc<AtomicBool>,
    sweeper_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// The plugin host: spawn, route, event bus, artifact channel.
pub struct Host {
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
    /// Plan runs (WP-06): admission → consent → run under the F3 monitor.
    pub plans: Arc<crate::plans::PlanService>,
}

impl Host {
    pub fn new(kernel: Arc<Kernel>, sock_dir: &Path) -> Result<Host, KernelError> {
        std::fs::create_dir_all(sock_dir)?;
        let inner = Arc::new(HostInner {
            plugins: Mutex::new(BTreeMap::new()),
            routes: Mutex::new(BTreeMap::new()),
            subs: Mutex::new(Vec::new()),
            next_sub: AtomicU64::new(1),
            witness: HostWitness::new(
                capture_process(std::process::id())?,
                Generation::new(rand_token()),
            )
            .map_err(crate::ledger::map_err)?,
            next_spawn: AtomicU64::new(1),
            sock_dir: sock_dir.to_path_buf(),
            meter: Mutex::new(crate::metrics::ContextMeter::default()),
            sweeper_stop: Arc::new(AtomicBool::new(false)),
            sweeper_handle: Mutex::new(None),
        });
        host_registry()
            .lock()
            .unwrap()
            .insert(inner.witness.session().to_string(), Arc::downgrade(&inner));
        let runtime = Arc::new(HostRuntime {
            kernel: kernel.clone(),
            inner: inner.clone(),
        });
        let plans = crate::plans::PlanService::new(kernel.clone(), runtime);
        Ok(Host {
            kernel,
            inner,
            plans,
        })
    }

    /// Spawn `bin args…` with `envs` added, wait for both hellos, register
    /// the plugin's verbs, and start its service threads. Returns the plugin
    /// name (from its hello). No slot: no position ceiling.
    pub fn spawn(
        &self,
        bin: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<String, KernelError> {
        self.spawn_in(bin, args, envs, None)
    }

    /// [`spawn`](Self::spawn) into a slot (F5): the plugin's declared
    /// `requires` must fit the slot's row at the door, and later invokes are
    /// bounded by it.
    pub fn spawn_in(
        &self,
        bin: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
        slot: Option<&Slot>,
    ) -> Result<String, KernelError> {
        spawn_plugin(
            &self.kernel,
            &self.inner,
            &self.plans,
            bin,
            args,
            envs,
            slot,
            None,
        )
    }
}

impl Host {
    /// OS pid of a running plugin (tests kill plugins with it).
    pub fn pid(&self, plugin: &str) -> Option<u32> {
        self.inner
            .plugins
            .lock()
            .unwrap()
            .get(plugin)
            .map(|h| h.pid)
    }

    /// The plugin serving a verb, if any — used by the CLI to find a driver
    /// by its family rather than by name (stack entries are data).
    pub fn verb_provider(&self, verb: &str) -> Option<String> {
        self.inner
            .routes
            .lock()
            .unwrap()
            .get(verb)
            .map(|e| e.plugin.clone())
    }

    /// Crash-only reclamation of a plugin's holdings (children first) with
    /// the physical inverses — the one teardown path. Returns rows released.
    pub fn reclaim(&self, plugin: &str, reason: &str) -> usize {
        reclaim(&self.kernel, &self.inner, plugin, reason)
    }

    /// Revoke a capability (WP-03): the derivation-tree cascade, each revoked
    /// cap's holding subtree torn down through the one teardown path. Returns
    /// how many caps the cascade newly revoked.
    pub fn revoke_capability(&self, cap_id: &str) -> Result<u64, KernelError> {
        let now = crate::db::now_unix();
        let mut world = HostWorld {
            inner: self.inner.clone(),
        };
        let n = self.kernel.caps.revoke(cap_id, &mut world, now)?;
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "cap.revoked", "cap": cap_id, "cascade": n,
        }));
        Ok(n)
    }

    /// Kernel-initiated verb call on a named plugin (no capability check:
    /// kernel-side callers act with root authority; user-session grants come
    /// later via consent).
    pub fn call(&self, plugin: &str, verb: &str, args: Value) -> Result<Value, KernelError> {
        let handle = self
            .inner
            .plugins
            .lock()
            .unwrap()
            .get(plugin)
            .cloned()
            .ok_or_else(|| KernelError::NotFound(format!("plugin: {plugin}")))?;
        if !self
            .kernel
            .ledger
            .holding(handle.holding.id())?
            .is_some_and(|h| h.state.is_active() && h.handle() == handle.holding)
        {
            return Err(KernelError::Denied("plugin is retiring".into()));
        }
        call_on(&handle, verb, args)
    }

    /// Kernel-initiated call routed by verb.
    pub fn call_verb(&self, verb: &str, args: Value) -> Result<Value, KernelError> {
        let target = self
            .inner
            .routes
            .lock()
            .unwrap()
            .get(verb)
            .map(|e| e.plugin.clone())
            .ok_or_else(|| KernelError::NotFound(format!("no route for verb: {verb}")))?;
        self.call(&target, verb, args)
    }

    /// Subscribe an in-process consumer (the CLI / model-facing loop) to a
    /// topic (exact or trailing-`*` prefix pattern). The subscription is a
    /// `kernel/subscription` holding of the kernel itself.
    pub fn subscribe_local(&self, topic: &str) -> Result<(u64, Receiver<Value>), KernelError> {
        subscribe_with_subject(&self.kernel, &self.inner, "kernel", topic)
    }

    /// Subscribe an arbitrary kernel-side subject (e.g. a plan run's
    /// segment) to a topic: the holding lands under that subject.
    pub fn subscribe_for(
        &self,
        subject: &str,
        topic: &str,
    ) -> Result<(u64, Receiver<Value>), KernelError> {
        subscribe_with_subject(&self.kernel, &self.inner, subject, topic)
    }

    /// Drop one of the kernel's own local subscriptions. Mirror of the client
    /// `unsubscribe` op: find under the subs lock, release its holding in the
    /// ledger without it, then let the cleanup executor remove the subscription.
    pub fn unsubscribe_local(&self, sub_id: u64) -> bool {
        let holding = {
            let subs = self.inner.subs.lock().unwrap();
            subs.iter()
                .find(|s| s.id == sub_id && matches!(s.target, SubTarget::Local(_)))
                .map(|s| s.holding.clone())
        };
        match holding {
            Some(h) => {
                let result = self.kernel.ledger.release_with_world(
                    &h,
                    &mut HostWorld {
                        inner: self.inner.clone(),
                    },
                    Timestamp::try_from(crate::db::now_unix()).expect("system timestamp in range"),
                );
                result.is_ok_and(|report| report.pending.is_empty())
            }
            None => false,
        }
    }

    /// Kernel-side event publish. Returns the number of subscribers the event
    /// was delivered to.
    pub fn emit(&self, topic: &str, data: Value) -> usize {
        dispatch_event(&self.kernel, &self.inner, topic, data)
    }

    /// Persist every event on `topic` to the audit chain. This is how a
    /// trusted plugin's self-reported log (e.g. the egress broker's
    /// `egress::log`) becomes tamper-evident: the plugin emits, the kernel
    /// subscribes and appends.
    pub fn audit_topic(&self, topic: &str) -> Result<(), KernelError> {
        let (_id, rx) = self.subscribe_local(topic)?;
        let kernel = self.kernel.clone();
        std::thread::spawn(move || {
            for ev in rx {
                let _ = kernel.audit.lock().unwrap().append(json!({
                    "event": "topic.audit",
                    "topic": ev["topic"],
                    "data": ev["data"],
                }));
            }
        });
        Ok(())
    }

    /// (context_bytes, data_bytes) moved through plugin channels so far.
    pub fn meter(&self) -> (u64, u64) {
        let m = self.inner.meter.lock().unwrap();
        (m.context_bytes, m.data_bytes)
    }

    /// Request the same durable, children-first path used on process exit.
    /// The executor may send a polite frame once all children are confirmed.
    pub fn shutdown(&self, plugin: &str) {
        let handle = self.inner.plugins.lock().unwrap().get(plugin).cloned();
        if let Some(h) = handle {
            h.shutting_down.store(true, Ordering::SeqCst);
            reclaim_holding(&self.kernel, &self.inner, plugin, &h.holding, "shutdown");
        }
    }

    pub fn shutdown_all(&self) {
        let names: Vec<String> = self.inner.plugins.lock().unwrap().keys().cloned().collect();
        for n in names {
            self.shutdown(&n);
        }
    }

    /// Start the lease sweeper (WP-02): every `interval`, due holdings expire
    /// through `Ledger::sweep` (children first; a parent waits on a child
    /// holding its own live lease) and the world side of each release runs
    /// through `HostWorld`. Quiet ticks audit nothing; a tick that released
    /// or failed anything audits `ledger.swept`. The thread stops on drop.
    pub fn start_sweeper(&self, interval: std::time::Duration) {
        let kernel = self.kernel.clone();
        let inner = self.inner.clone();
        let plans = self.plans.clone();
        let stop = inner.sweeper_stop.clone();
        let handle = std::thread::spawn(move || {
            loop {
                // Sleep in slices so Host::drop stops the thread promptly.
                let deadline = std::time::Instant::now() + interval;
                while std::time::Instant::now() < deadline {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let now = crate::db::now_unix();
                let mut world = HostWorld {
                    inner: inner.clone(),
                };
                match kernel.ledger.sweep_with_world(
                    &mut world,
                    Timestamp::try_from(now).expect("system timestamp in range"),
                ) {
                    Ok(report) if !report.released.is_empty() || !report.pending.is_empty() => {
                        let mut classes: BTreeMap<String, usize> = BTreeMap::new();
                        for (_, class, _) in &report.released {
                            *classes.entry(class.clone()).or_insert(0) += 1;
                        }
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "ledger.swept",
                            "released": report.released.len(),
                            "classes": classes,
                            "pending": report.pending.iter().map(|t|json!({"cleanup_id":t.id.get(),"holding":t.holding.id().get(),"state":format!("{:?}",t.state)})).collect::<Vec<_>>(),
                        }));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "ledger.sweep_error", "error": e.to_string(),
                        }));
                    }
                }
                // [TTL] suspended plan runs expire on the same tick (WP-06).
                plans.expire(now);
            }
        });
        *self.inner.sweeper_handle.lock().unwrap() = Some(handle);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.inner.sweeper_stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.inner.sweeper_handle.lock().unwrap().take() {
            let _ = h.join();
        }
        self.plans.shutdown();
        self.shutdown_all();
        let subscriptions = self
            .inner
            .subs
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.holding.clone())
            .collect::<Vec<_>>();
        let now = Timestamp::try_from(crate::db::now_unix()).expect("system timestamp in range");
        for holding in subscriptions {
            let _ = self.kernel.ledger.release_with_world(
                &holding,
                &mut HostWorld {
                    inner: self.inner.clone(),
                },
                now,
            );
        }
    }
}

#[cfg(test)]
mod tests;
