//! Plugin host — ABI v2.
//!
//! A plugin is a plain child process (no sandbox yet) that connects back to a
//! per-spawn UDS **twice**, authenticating both connections with a spawn
//! token from the environment:
//!
//!   - the **serve** channel: kernel→plugin `call` requests and `shutdown`.
//!     The plugin declares its verb list (and which extra channels it will
//!     open) in the serve hello; the kernel registers those verbs in its
//!     route table.
//!   - the **client** channel: plugin→kernel requests — `invoke` (call
//!     another plugin's verb through the kernel: capability-checked against
//!     the calling plugin, audited, routed), `grants` (introspect what this
//!     plugin may invoke, joined with the verbs' advertised metadata),
//!     `emit` (event bus; a plugin's subscriptions are declared in its
//!     hello, and topic patterns may end in `*` for prefix matching), and
//!     `put`/`read` (artifact dereference as chunked byte streams; see
//!     `portos_abi::chunk`).
//!   - an optional **events** channel: one-way kernel→plugin event
//!     deliveries. A plugin that declares it can receive subscribed events
//!     *while one of its own verbs is mid-call* — the serve channel is busy
//!     then, and without a separate channel a plugin awaiting an event
//!     stream inside a call handler would deadlock (the model driver's SSE
//!     consumption is exactly that shape). Plugins that don't declare it get
//!     events interleaved on the serve channel as before.
//!
//! Each channel is strict in a single direction, which keeps the sync-thread
//! model trivial: no frame multiplexing anywhere.
//!
//! Every frame crossing these channels is one of the types in
//! `portos_abi::wire`, so a malformed frame is refused at the boundary
//! instead of defaulting into a plausible-looking request. Verb *payloads*
//! stay opaque: this module moves [`Payload`] bytes and has no way to read a
//! field out of them, which makes domain ignorance structural rather than a
//! rule to remember. The capability convention for invoke is subject
//! `plugin:<name>`, resource `driver:<driver>`, verb the short name.
//!
//! Known limitation: the invoke graph must be acyclic. A cycle (A invokes B
//! while B's serve loop is blocked invoking A) deadlocks; current flows
//! (cli → model driver → {broker, browser}) are acyclic by construction.

mod builtin;
mod client;
mod events;
mod launch;
mod ready;
mod teardown;

use crate::routes::{Answerer, RouteTable};
use crate::{Kernel, KernelError};
use builtin::{builtin_routes, builtin_verb, reply_of};
use events::dispatch_event;
use launch::{rand_token, spawn_spec_on};
use portos_abi::boundary::{Cgroup, CgroupRoot};
use portos_abi::frame;
use portos_abi::ids::{PluginName, SubId, Topic, Verb};
use portos_abi::wire::{Call, LocalEvent, Payload, Reply, ServeMsg, ToolMeta};
use portos_router::{Miss, Router as _};
use ready::{list_plugins, settle};
use serde_json::json;
use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use teardown::shutdown_on;

use portos_kernel_api::PluginInfo;
pub use portos_kernel_api::{Form, GrantSpec, LaunchSpec};

/// Bounded event queue per subscriber. A subscriber that falls this far
/// behind is cut off, because a slow consumer must never stall the kernel:
/// plugin subscribers are disconnected, local subscribers dropped.
pub const EVENT_QUEUE: usize = 256;

/// What a plugin declared about itself, kept so readiness can be judged
/// again whenever the route table changes under it.
struct Declared {
    verbs: Vec<Verb>,
    tools: BTreeMap<Verb, ToolMeta>,
    needs: Vec<Verb>,
}

struct PluginHandle {
    declared: Mutex<Declared>,
    /// What it was started from, as the spec named it; one of the two.
    artifact: Option<String>,
    bin: Option<String>,
    /// Present when this plugin was started in [`Form::Cgroup`]. Teardown
    /// ends with emptying it, and removing it is the proof that it worked —
    /// `rmdir` only succeeds on an empty cgroup.
    cgroup: Option<Cgroup>,
    child: Mutex<std::process::Child>,
    serve: Mutex<UnixStream>,
    /// Dedicated event-delivery stream, when the plugin declared one.
    /// Without it, events interleave on the serve channel.
    events: Option<Mutex<UnixStream>>,
    events_tx: SyncSender<ServeMsg>,
    sock_path: PathBuf,
}

enum SubTarget {
    Local(SyncSender<LocalEvent>),
    Plugin(PluginName),
}

struct Sub {
    id: SubId,
    pattern: Topic,
    target: SubTarget,
}

struct HostInner {
    cgroups: Option<CgroupRoot>,
    /// Distinguishes this host's cgroups from another host's in the same
    /// process. Without it two hosts number their plugins from one and
    /// collide — and a shared cgroup means stopping one plugin kills the
    /// other's.
    cgroup_tag: String,
    plugins: Mutex<BTreeMap<PluginName, Arc<PluginHandle>>>,
    routes: Mutex<RouteTable>,
    subs: Mutex<Vec<Sub>>,
    next_sub: AtomicU64,
    next_spawn: AtomicU64,
    sock_dir: PathBuf,
    meter: Mutex<crate::metrics::ContextMeter>,
}

/// The plugin host: spawn, route, event bus, artifact channel.
pub struct Host {
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
}

impl Host {
    pub fn new(kernel: Arc<Kernel>, sock_dir: &Path) -> Result<Host, KernelError> {
        std::fs::create_dir_all(sock_dir)?;
        // Absolute, always. `PORTOS_PLUGIN_SOCK` is the whole of how a plugin
        // reaches the kernel, and a relative path makes that depend on the
        // working directory it happens to be started in — which worked only
        // because every plugin inherited this process's. A bundle sets its
        // own working directory and the first one to do so could not connect
        // at all; a container would have failed the same way.
        let sock_dir = std::fs::canonicalize(sock_dir)?;
        let cgroups = CgroupRoot::detect();
        match &cgroups {
            Some(root) => {
                // Before anything starts: whatever a previous run left when
                // it died without teardown. This is the half of reclamation
                // a process group cannot do at all, because it leaves
                // nothing behind to come back to.
                let reaped = root.reap_orphans();
                if reaped > 0 {
                    eprintln!("[kernel] collected {reaped} cgroup(s) left by an earlier run");
                }
            }
            None => eprintln!(
                "[kernel] no writable cgroup v2: plugins run as process groups, \
                 which a child can leave with setsid"
            ),
        }
        Ok(Host {
            kernel,
            inner: Arc::new(HostInner {
                cgroups,
                cgroup_tag: rand_token()[..8].to_string(),
                plugins: Mutex::new(BTreeMap::new()),
                // The kernel's own verbs go in first, as rows. Nothing else
                // reserves the name: whoever registers first holds it, and a
                // plugin that later claims `kernel::spawn` gets the same
                // conflict any double claim gets.
                routes: Mutex::new(builtin_routes()),
                subs: Mutex::new(Vec::new()),
                next_sub: AtomicU64::new(1),
                next_spawn: AtomicU64::new(1),
                sock_dir,
                meter: Mutex::new(crate::metrics::ContextMeter::default()),
            }),
        })
    }

    /// Spawn `bin args…` with `envs` added, wait for every declared hello,
    /// register the plugin's verbs, and start its service threads. Returns
    /// the name the plugin declared.
    pub fn spawn(
        &self,
        bin: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<PluginName, KernelError> {
        self.spawn_spec(&LaunchSpec {
            args: args.iter().map(|s| s.to_string()).collect(),
            env: envs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..LaunchSpec::from_path(bin.to_string_lossy())
        })
    }

    /// Start a plugin and mint what its spec says it may do.
    pub fn spawn_spec(&self, spec: &LaunchSpec) -> Result<PluginName, KernelError> {
        spawn_spec_on(&self.kernel, &self.inner, spec)
    }

    /// Judge again whether each plugin can do its job.
    ///
    /// Needed by whoever mints capabilities outside a spec — the kernel sees
    /// routes change on its own, but not grants, and a permission arriving
    /// is as much a reason to re-judge as a provider arriving.
    pub fn refresh(&self) {
        settle(&self.kernel, &self.inner);
    }

    /// Which plugins are up, what each was started from, what each answers,
    /// and what each is waiting for.
    pub fn plugins(&self) -> Vec<PluginInfo> {
        list_plugins(&self.kernel, &self.inner)
    }

    /// Every running instance that answers this verb — routed, or still
    /// waiting on a grant or a dependency, because a plugin that is waiting
    /// is exactly the one a front end is about to grant something to.
    ///
    /// This is how anything outside the kernel finds "the model driver": by
    /// what it answers, never by what it is called. That is what lets one be
    /// replaced, and what lets there be two.
    pub fn answerers_of(&self, verb: &Verb) -> Vec<PluginName> {
        self.inner
            .plugins
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, h)| h.declared.lock().unwrap().verbs.contains(verb))
            .map(|(n, _)| n.clone())
            .collect()
    }
}

/// Resolve a verb to who answers it and what they call it, turning a miss
/// into the error a caller can act on: nobody, or several and none named.
fn route(
    inner: &Arc<HostInner>,
    verb: &Verb,
    at: Option<&PluginName>,
) -> Result<(Answerer, Verb), KernelError> {
    let routes = inner.routes.lock().unwrap();
    let who = at.map(Answerer::named);
    match routes.resolve(verb, who.as_ref()) {
        Ok(r) => Ok((r.target.clone(), r.name)),
        Err(Miss::NoRoute) => Err(KernelError::NotFound(match at {
            Some(at) => format!("no route for verb: {verb} at {at}"),
            None => format!("no route for verb: {verb}"),
        })),
        Err(Miss::Ambiguous) => {
            let names: Vec<String> = routes
                .answerers(verb)
                .into_iter()
                .map(|(a, _)| a.id().to_string())
                .collect();
            Err(KernelError::Ambiguous(format!(
                "{verb} is answered by {}; name one",
                names.join(", ")
            )))
        }
    }
}

impl Host {
    /// Kernel-initiated verb call on a named plugin. No capability check:
    /// kernel-side callers act with root authority.
    pub fn call(
        &self,
        plugin: &PluginName,
        verb: &Verb,
        args: Payload,
    ) -> Result<Payload, KernelError> {
        let handle = self
            .inner
            .plugins
            .lock()
            .unwrap()
            .get(plugin)
            .cloned()
            .ok_or_else(|| KernelError::NotFound(format!("plugin: {plugin}")))?;
        call_on(&handle, verb, args)
    }

    /// Kernel-initiated call routed by verb. `at` names the instance when
    /// the caller knows which one it wants; with one instance it need not.
    pub fn call_verb(
        &self,
        verb: &Verb,
        at: Option<&PluginName>,
        args: Payload,
    ) -> Result<Payload, KernelError> {
        let (target, name_there) = route(&self.inner, verb, at)?;
        match target {
            Answerer::Plugin(name) => self.call(&name, &name_there, args),
            Answerer::Kernel => {
                reply_of(builtin_verb(&self.kernel, &self.inner, &name_there, args)?)
            }
        }
    }

    /// Subscribe an in-process consumer (the chat loop) to a topic pattern.
    pub fn subscribe_local(&self, pattern: &Topic) -> (SubId, Receiver<LocalEvent>) {
        let (tx, rx) = sync_channel::<LocalEvent>(EVENT_QUEUE);
        let id = SubId::new(self.inner.next_sub.fetch_add(1, Ordering::SeqCst));
        self.inner.subs.lock().unwrap().push(Sub {
            id,
            pattern: pattern.clone(),
            target: SubTarget::Local(tx),
        });
        (id, rx)
    }

    /// Kernel-side event publish. Returns how many subscribers it reached.
    pub fn emit(&self, topic: &Topic, data: Payload) -> u64 {
        dispatch_event(&self.kernel, &self.inner, topic, &data)
    }

    /// Persist every event matching `pattern` to the audit chain. This is how
    /// a trusted plugin's self-reported log (e.g. the egress broker's
    /// `egress::log`) becomes tamper-evident: the plugin emits, the kernel
    /// subscribes and appends.
    pub fn audit_topic(&self, pattern: &Topic) {
        let (_id, rx) = self.subscribe_local(pattern);
        let kernel = self.kernel.clone();
        std::thread::spawn(move || {
            for ev in rx {
                let data: serde_json::Value = ev.data.parse().unwrap_or(serde_json::Value::Null);
                let _ = kernel.audit.lock().unwrap().append(json!({
                    "event": "topic.audit",
                    "topic": ev.topic.as_str(),
                    "data": data,
                }));
            }
        });
    }

    /// (context_bytes, data_bytes) moved through plugin channels so far.
    pub fn meter(&self) -> (u64, u64) {
        let m = self.inner.meter.lock().unwrap();
        (m.context_bytes, m.data_bytes)
    }

    /// Shut a plugin down, escalating until it is actually gone.
    ///
    /// Asking politely is the first step, not the mechanism: a plugin that
    /// ignores `shutdown`, or that leaves a listening socket holding its
    /// runtime alive, still goes — and so does everything it started, because
    /// the signals go to its process group rather than to it alone.
    pub fn shutdown(&self, plugin: &PluginName) {
        shutdown_on(&self.kernel, &self.inner, plugin);
    }

    pub fn shutdown_all(&self) {
        let names: Vec<PluginName> = self.inner.plugins.lock().unwrap().keys().cloned().collect();
        for n in &names {
            shutdown_on(&self.kernel, &self.inner, n);
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

/// One serve-channel request/response under the channel lock. Holding the
/// lock across write+read is what keeps the channel unmultiplexed.
fn call_on(handle: &PluginHandle, verb: &Verb, args: Payload) -> Result<Payload, KernelError> {
    let mut s = handle.serve.lock().unwrap();
    let msg = ServeMsg::Call(Call {
        verb: verb.clone(),
        args,
    });
    frame::write_frame(&mut *s, &msg)
        .map_err(|e| KernelError::Corrupt(format!("call write: {e}")))?;
    let reply: Reply<Payload> =
        frame::read_frame(&mut *s).map_err(|e| KernelError::Corrupt(format!("call read: {e}")))?;
    reply
        .into_result()
        .map_err(|e| KernelError::Denied(format!("plugin error: {e}")))
}
