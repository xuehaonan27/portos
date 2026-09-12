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
//!     `emit`/`subscribe`/`unsubscribe` (event bus; topic patterns may end
//!     in `*` for prefix matching), and `put`/`read` (artifact dereference
//!     as chunked byte streams; see `portos_abi::chunk`).
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

use crate::routes::{Answerer, RouteTable};
use crate::{Kernel, KernelError};
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use portos_abi::boundary::{Cgroup, CgroupRoot};
use portos_abi::ids::{PluginName, SubId, Topic, Verb};
use portos_abi::wire::{
    Call, ChannelRole, ClaimReply, ClientOp, EmitReply, Event, GrantsReply, Hello, HelloFrame,
    LocalEvent, LocateReply, Payload, PutReply, ReadReply, Reply, ServeMsg, SubscribeReply,
    ToolMeta, UnsubscribeReply,
};
use portos_abi::{ABI_VERSION, chunk, frame};
use portos_router::{Miss, Router as _};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

/// How to start a plugin, and what it may do once it is up.
///
/// The same shape whether it comes from `chat.json` at boot or from a
/// `kernel::spawn` call mid-session: starting a plugin is one operation with
/// one description, not two code paths that drift.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LaunchSpec {
    /// An executable in the CAS.
    ///
    /// A plugin named this way is a **thing** rather than a location: the
    /// same bytes get the same id on every machine, the spec carries the
    /// name and never the bytes, and what ran is checkable afterwards. It
    /// is also what lets a spec cross a boundary at all — a path means
    /// nothing inside a container, on another node, or to the next run of
    /// this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// A path on this host: the escape hatch, for things already installed
    /// (`node`, a system tool) and for the bootstrap.
    ///
    /// **Not reproducible and not portable.** The file can change under a
    /// spec that names it, and nothing that names one can be shipped
    /// anywhere. Kept because pretending otherwise would be worse, and
    /// labelled so it does not read as equivalent to the line above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    /// A tar archive in the CAS: everything the plugin needs beyond one
    /// executable.
    ///
    /// A second axis from `artifact`/`bin`, not an alternative to them. Those
    /// say **what runs**; this says **what files it has**. A JS plugin needs
    /// both and they come from different places — the script and its
    /// dependencies from here, the interpreter from the host — which is why
    /// folding them into one field would not have worked.
    ///
    /// It is unpacked once and becomes the process's working directory, so
    /// relative paths inside it mean what they meant when it was built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// What this plugin should be, in its own vocabulary. Opaque here — the
    /// kernel hands it over and cannot read it, like any other payload.
    ///
    /// It lives in the spec rather than in a file the plugin finds for
    /// itself, so that changing it changes the spec: a reload then re-plugs
    /// that one driver without anyone having to declare which files matter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Payload>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The identifier this instance runs under.
    ///
    /// Two instances of one driver — two browsers — need two, and the
    /// launcher is the one who knows there are two, so it names them; what a
    /// plugin calls itself is only the name it gets when nobody says
    /// otherwise. A plugin that was named and answers to something else is
    /// refused: a launcher that cannot trust the name it gave cannot address
    /// what it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Minted once the plugin is up and has declared its name.
    #[serde(default)]
    pub grants: Vec<GrantSpec>,
    /// How it runs. What a plugin *is* and how it runs are two axes; this is
    /// the second one, and the kernel knows only the two it can do without
    /// learning a domain — a child process, optionally inside a cgroup.
    /// Containers and VMs belong to drivers.
    #[serde(default)]
    pub form: Form,
}

/// The runtime forms the kernel implements itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Form {
    /// A child process leading its own process group. Always available, and
    /// the bootstrap that must never depend on anything else being present.
    Bare,
    /// The same child, inside a cgroup of its own — a boundary it cannot
    /// leave with `setsid`, and one that leaves a findable directory if this
    /// runtime dies without teardown. Falls back to [`Form::Bare`] where
    /// cgroup v2 is not available or not writable.
    #[default]
    Cgroup,
}

/// A capability to mint. `subject` defaults to the plugin being started,
/// which is the common case: a driver being granted what it needs.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GrantSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub resource: String,
    #[serde(default)]
    pub verbs: BTreeSet<String>,
}

/// Where a plugin's executable comes from. Two fields rather than one enum
/// so that a spec reads as `{"artifact": …}` or `{"bin": …}` and a typo gets
/// a message naming the field; "exactly one of them" is then checked here,
/// once.
enum Source<'a> {
    Artifact(&'a str),
    Path(&'a str),
}

impl LaunchSpec {
    pub fn from_path(bin: impl Into<String>) -> LaunchSpec {
        LaunchSpec {
            bin: Some(bin.into()),
            ..Default::default()
        }
    }

    pub fn from_artifact(id: impl Into<String>) -> LaunchSpec {
        LaunchSpec {
            artifact: Some(id.into()),
            ..Default::default()
        }
    }

    /// A bundled plugin: its files from `bundle`, run by `bin` — which is
    /// either a name to find on PATH (`node`) or, if it contains a
    /// separator, a file inside the bundle.
    pub fn from_bundle(bundle: impl Into<String>, bin: impl Into<String>) -> LaunchSpec {
        LaunchSpec {
            bundle: Some(bundle.into()),
            bin: Some(bin.into()),
            ..Default::default()
        }
    }

    fn source(&self) -> Result<Source<'_>, KernelError> {
        match (&self.artifact, &self.bin) {
            (Some(id), None) => Ok(Source::Artifact(id)),
            (None, Some(path)) => Ok(Source::Path(path)),
            (Some(_), Some(_)) => Err(KernelError::Denied(
                "launch spec names both `artifact` and `bin`; it must name exactly one".into(),
            )),
            (None, None) => Err(KernelError::Denied(
                "launch spec names neither `artifact` nor `bin`".into(),
            )),
        }
    }
}

/// Bounded event queue per subscriber. A subscriber that falls this far
/// behind is cut off, because a slow consumer must never stall the kernel:
/// plugin subscribers are disconnected, local subscribers dropped.
pub const EVENT_QUEUE: usize = 256;

const SPAWN_DEADLINE_MS: u64 = 10_000;

/// How long each teardown step waits before escalating.
const GRACE_MS: u64 = 1_000;

/// What a plugin declared about itself, kept so readiness can be judged
/// again whenever the route table changes under it.
struct Declared {
    verbs: Vec<Verb>,
    tools: BTreeMap<Verb, ToolMeta>,
    needs: Vec<Verb>,
}

struct PluginHandle {
    declared: Mutex<Declared>,
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

    /// Which plugins are up, what each answers, and what each is waiting for.
    pub fn plugins(&self) -> Vec<(PluginName, Vec<Verb>, Vec<Verb>)> {
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

/// Which plugins are up, what each answers, and what each is waiting for.
fn list_plugins(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
) -> Vec<(PluginName, Vec<Verb>, Vec<Verb>)> {
    let entries: Vec<(PluginName, Arc<PluginHandle>)> = inner
        .plugins
        .lock()
        .unwrap()
        .iter()
        .map(|(n, h)| (n.clone(), h.clone()))
        .collect();
    entries
        .into_iter()
        .map(|(name, handle)| {
            let verbs = inner.routes.lock().unwrap().verbs_of(&name);
            let waiting = unmet(kernel, inner, &handle, &name);
            (name, verbs, waiting)
        })
        .collect()
}

fn spawn_spec_on(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    spec: &LaunchSpec,
) -> Result<PluginName, KernelError> {
    let name = spawn_process(kernel, inner, spec)?;
    for g in &spec.grants {
        let subject = g.subject.clone().unwrap_or_else(|| name.subject());
        kernel.caps.mint(
            &subject,
            &g.resource,
            g.verbs.clone(),
            Default::default(),
            None,
        )?;
    }
    // Granting is the other half of readiness, so it is also a reason to
    // look again — a plugin can be waiting on a permission as easily as on
    // a provider.
    settle(kernel, inner);
    Ok(name)
}

/// `spawn`, retrying while the file we just wrote is still "busy".
///
/// Not our race and not one we can close: writing an executable in one
/// thread while another forks leaks the write descriptor into that fork, and
/// `execve` refuses a file anybody has open for writing. The descriptor goes
/// when the other child execs, which is microseconds away — so this is the
/// OS telling us to look again in a moment, not a condition to design
/// around.
fn spawn_retrying_busy(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(26) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

/// Put an artifact's bytes somewhere they can be executed, once.
///
/// Content addressing makes the cache trivially correct: the same id is the
/// same bytes, forever, so a materialised copy never goes stale and there is
/// nothing to invalidate. It is written under a temporary name and renamed,
/// because two spawns of the same plugin can race and neither may ever see a
/// half-written file.
///
/// The copy is deliberate rather than an exec straight out of the CAS: an
/// artifact is not an executable, and making every stored object executable
/// to suit the few that are would be the wrong trade.
fn materialise(kernel: &Kernel, id: &str) -> Result<PathBuf, KernelError> {
    use std::os::unix::fs::PermissionsExt;

    let dir = kernel.root.join("exec");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(id.replace(':', "-"));
    if path.exists() {
        return Ok(path);
    }
    let meta = kernel.cas.meta(&id.to_string())?;
    let tmp = path.with_extension(format!("tmp-{}", rand_token()));
    {
        let mut src = kernel.cas.open_read(&id.to_string())?;
        let mut dst = std::fs::File::create(&tmp)?;
        std::io::copy(&mut src, &mut dst)?;
        dst.set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &path)?;
    let _ = kernel.audit.lock().unwrap().append(json!({
        "event": "plugin.materialised", "id": id, "size": meta.size,
        "path": path.to_string_lossy(),
    }));
    Ok(path)
}

/// Unpack a bundle into a directory, once.
///
/// Same cache argument as [`materialise`], and the same reason it needs no
/// invalidation: the id *is* the bytes. Unpacked under a temporary name and
/// renamed, so a concurrent spawn never sees a half-written tree — and if it
/// loses that race, the winner's tree is just as good, because they are the
/// same tree.
///
/// `tar`'s `unpack` refuses entries that would write outside the
/// destination, which is the property that matters for an archive that may
/// have come from anywhere.
fn unpack(kernel: &Kernel, id: &str) -> Result<PathBuf, KernelError> {
    let dir = kernel
        .root
        .join("exec")
        .join(format!("{}.d", id.replace(':', "-")));
    if dir.exists() {
        return Ok(dir);
    }
    std::fs::create_dir_all(dir.parent().expect("exec dir"))?;
    let tmp = dir.with_extension(format!("d-tmp-{}", rand_token()));
    let file = kernel.cas.open_read(&id.to_string())?;
    tar::Archive::new(file)
        .unpack(&tmp)
        .map_err(|e| KernelError::Corrupt(format!("bundle {id}: {e}")))?;
    if std::fs::rename(&tmp, &dir).is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        if !dir.exists() {
            return Err(KernelError::Corrupt(format!(
                "bundle {id}: could not unpack"
            )));
        }
    }
    let _ = kernel.audit.lock().unwrap().append(json!({
        "event": "plugin.unpacked", "id": id, "path": dir.to_string_lossy(),
    }));
    Ok(dir)
}

/// Start the process, complete every declared handshake, register its verbs.
/// Free-standing because both the embedder (`Host::spawn_spec`) and the
/// `kernel::spawn` verb reach it, and a second way to start a plugin is a
/// second set of bugs.
fn spawn_process(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    spec: &LaunchSpec,
) -> Result<PluginName, KernelError> {
    let idx = inner.next_spawn.fetch_add(1, Ordering::SeqCst);
    let sock_path = inner
        .sock_dir
        .join(format!("plugin-{}-{idx}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;
    listener.set_nonblocking(true)?;
    let token = rand_token();

    let cgroup = match spec.form {
        Form::Cgroup => inner
            .cgroups
            .as_ref()
            .and_then(|r| r.create(&format!("{}-{idx}", inner.cgroup_tag))),
        Form::Bare => None,
    };

    let bundle_dir = match &spec.bundle {
        Some(id) => Some(unpack(kernel, id)?),
        None => None,
    };
    let bin = match spec.source()? {
        Source::Artifact(id) => materialise(kernel, id)?,
        // A program name with no separator is for PATH to find — `node` is
        // not something a bundle carries. One with a separator names a file,
        // and inside a bundle that means the bundle's file, resolved here
        // rather than left to whatever a relative program path means after a
        // chdir.
        Source::Path(path) => match &bundle_dir {
            Some(dir) if path.contains('/') => dir.join(path),
            _ => PathBuf::from(path),
        },
    };

    let mut cmd = std::process::Command::new(&bin);
    if let Some(dir) = &bundle_dir {
        cmd.current_dir(dir);
    }
    cmd.args(&spec.args)
        .env("PORTOS_PLUGIN_SOCK", &sock_path)
        .env("PORTOS_PLUGIN_TOKEN", &token)
        // Each plugin leads its own process group, so teardown can reach
        // whatever it started. A driver's real cost is usually its
        // grandchildren — the browser driver's chromium, a shell driver's
        // pipeline — and `Child::kill` never sees those. The process group
        // stays even in cgroup form: it is what makes a *polite* SIGTERM
        // possible, and a cgroup only offers the final, unanswerable one.
        .process_group(0);
    if let Some(cg) = &cgroup {
        // A plugin that runs children of its own needs the same boundary the
        // kernel just gave it, or it reinvents signal escalation — which is
        // what the shell driver did, badly, twice.
        cmd.env("PORTOS_PLUGIN_CGROUP", cg.path());
        match cg.procs_file() {
            Ok(file) => {
                // Between fork and exec, the child puts itself in the cgroup.
                // It has to be here rather than after spawning: anything the
                // plugin forks inherits the cgroup, and a process moved in
                // later leaves its existing children outside it.
                //
                // Safety: the closure runs in the forked child before exec
                // and does one `write` on an already-open descriptor — no
                // allocation, no locks, nothing that a fork could have left
                // inconsistent.
                unsafe {
                    cmd.pre_exec(move || {
                        use std::io::Write;
                        (&file).write_all(b"0")
                    });
                }
            }
            Err(e) => eprintln!("[kernel] cgroup unusable, falling back to process group: {e}"),
        }
    }
    if let Some(config) = &spec.config {
        cmd.env("PORTOS_PLUGIN_CONFIG", config.as_raw());
    }
    if let Some(name) = &spec.name {
        cmd.env("PORTOS_PLUGIN_NAME", name);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    let mut child = spawn_retrying_busy(&mut cmd)?;

    // The serve connection comes first and declares which extra channels
    // follow ("client" always; "events" optionally). A frame that is not
    // a well-formed hello, or a bad token, is fatal for the spawn.
    let accept_hello =
        |child: &mut std::process::Child| -> Result<(UnixStream, Hello), KernelError> {
            let mut stream = accept_with_deadline(&listener, child, SPAWN_DEADLINE_MS)?;
            let frame: HelloFrame = frame::read_frame(&mut stream)
                .map_err(|e| KernelError::Corrupt(format!("hello: {e}")))?;
            if frame.hello.token != token {
                let _ = frame::write_frame(&mut stream, &Reply::<()>::Err("bad token".into()));
                return Err(KernelError::Denied("plugin hello: bad token".into()));
            }
            frame::write_frame(&mut stream, &Reply::Ok(Payload::null()))
                .map_err(|e| KernelError::Corrupt(format!("hello ack: {e}")))?;
            Ok((stream, frame.hello))
        };

    let (serve_stream, hello) = match accept_hello(&mut child) {
        Ok((stream, h)) if h.role == ChannelRole::Serve => (stream, h),
        Ok(_) => {
            let _ = child.kill();
            return Err(KernelError::Corrupt(
                "plugin hello: first connection must be role serve".into(),
            ));
        }
        Err(e) => {
            let _ = child.kill();
            return Err(e);
        }
    };
    if hello.abi != ABI_VERSION {
        let _ = child.kill();
        return Err(KernelError::Denied(format!(
            "plugin abi {} != kernel abi {ABI_VERSION}",
            hello.abi
        )));
    }
    let name = hello.name.clone();
    let verbs = hello.verbs.clone();
    let tools_meta = hello.tools.clone().unwrap_or_default();
    let mut expected = hello.declared_channels();

    if !expected.contains(&ChannelRole::Client) {
        let _ = child.kill();
        return Err(KernelError::Corrupt(
            "plugin hello: a client channel is required".into(),
        ));
    }
    let mut client: Option<UnixStream> = None;
    let mut events: Option<UnixStream> = None;
    while !expected.is_empty() {
        let (stream, h) = match accept_hello(&mut child) {
            Ok(x) => x,
            Err(e) => {
                let _ = child.kill();
                return Err(e);
            }
        };
        match expected.iter().position(|c| *c == h.role) {
            Some(i) => {
                expected.remove(i);
                match h.role {
                    ChannelRole::Client => client = Some(stream),
                    ChannelRole::Events => events = Some(stream),
                    ChannelRole::Serve => {
                        let _ = child.kill();
                        return Err(KernelError::Corrupt(
                            "plugin hello: duplicate serve channel".into(),
                        ));
                    }
                }
            }
            None => {
                let _ = child.kill();
                return Err(KernelError::Corrupt(format!(
                    "plugin hello: undeclared or duplicate role {:?}",
                    h.role
                )));
            }
        }
    }
    let client_stream = client.expect("client channel present");

    // Register verbs; a route conflict aborts the spawn.
    {
        let mut plugins = inner.plugins.lock().unwrap();
        // The kernel is not in this map, but it took its name first like
        // anything else does.
        if plugins.contains_key(&name) || name.as_str() == crate::routes::KERNEL {
            let _ = child.kill();
            return Err(KernelError::Denied(format!("plugin name taken: {name}")));
        }
        // A launcher that named this instance must get the name it gave, or
        // it cannot address what it started.
        if let Some(wanted) = &spec.name {
            if wanted != name.as_str() {
                let _ = child.kill();
                return Err(KernelError::Denied(format!(
                    "launched as {wanted} but it calls itself {name}"
                )));
            }
        }

        let (events_tx, events_rx) = sync_channel::<ServeMsg>(EVENT_QUEUE);
        let handle = Arc::new(PluginHandle {
            declared: Mutex::new(Declared {
                verbs: verbs.clone(),
                tools: tools_meta.clone(),
                needs: hello.needs.clone(),
            }),
            cgroup,
            child: Mutex::new(child),
            serve: Mutex::new(serve_stream),
            events: events.map(Mutex::new),
            events_tx,
            sock_path: sock_path.clone(),
        });
        plugins.insert(name.clone(), handle.clone());
        spawn_event_pump(handle.clone(), events_rx);
        spawn_client_loop(kernel.clone(), inner.clone(), name.clone(), client_stream);
    }
    // Whether this one is usable, and whether it just made somebody else so.
    settle(kernel, inner);

    let _ = kernel.audit.lock().unwrap().append(json!({
        "event": "plugin.spawned",
        "plugin": name.as_str(),
        // What ran, by the name it is knowable under. For an artifact that
        // is a claim anyone can check later; for a path it is only a claim
        // about a file that may since have changed, which is the difference
        // the two fields exist to record.
        "artifact": spec.artifact,
        "bin": spec.bin,
        "bundle": spec.bundle,
        "form": spec.form,
        "verbs": verbs.iter().map(Verb::as_str).collect::<Vec<_>>(),
    }));
    Ok(name)
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

/// Stop a plugin and collect what the kernel was holding for it.
///
/// The residue is a short, enumerable list, which is the whole reason
/// hot-unplug needs no theory here: the plugin is a process, so its own
/// state goes with it, and what the kernel keeps is routes, subscriptions,
/// a socket file, a process group, and capabilities. The first four were
/// always collected; the fifth is collected here, because **a capability is
/// held by a running plugin, not by a name**. What survives on purpose is
/// only the immutable record: artifacts already in the CAS, and the audit
/// log.
fn shutdown_on(kernel: &Arc<Kernel>, inner: &Arc<HostInner>, plugin: &PluginName) -> bool {
    let handle = { inner.plugins.lock().unwrap().remove(plugin) };
    let Some(h) = handle else { return false };
    cleanup_plugin(inner, plugin);
    let revoked = kernel
        .caps
        .revoke_subject(&plugin.subject(), crate::db::now_unix())
        .unwrap_or(0);
    let _ = kernel.audit.lock().unwrap().append(json!({
        "event": "plugin.stopped", "plugin": plugin.as_str(), "caps_revoked": revoked,
    }));
    if let Ok(mut s) = h.serve.lock() {
        let _ = frame::write_frame(&mut *s, &ServeMsg::Shutdown);
    }
    let mut child = h.child.lock().unwrap();
    let pgid = child.id();
    if !wait_for_exit(&mut child, GRACE_MS) {
        signal_group(pgid, Signal::SIGTERM);
        if !wait_for_exit(&mut child, GRACE_MS) {
            signal_group(pgid, Signal::SIGKILL);
            let _ = wait_for_exit(&mut child, GRACE_MS);
        }
    }
    // The leader is reaped; anything left in its group is not a child of
    // ours and cannot be waited on, so the final KILL is unconditional.
    let _ = child.kill();
    let _ = child.wait();
    signal_group(pgid, Signal::SIGKILL);
    // And then the part a process group cannot do: a child that called
    // `setsid` is no longer in the group and has been ignoring every signal
    // above. `cgroup.kill` has no such gap. Removing the directory is the
    // proof it worked — `rmdir` refuses a cgroup that still holds anything.
    // Its routes are gone; whoever needed them is no longer usable either.
    settle(kernel, inner);
    if let Some(cg) = &h.cgroup {
        cg.kill();
        if !cg.remove() {
            eprintln!(
                "[kernel] {plugin}: cgroup {} would not empty",
                cg.path().display()
            );
        }
    }
    let _ = std::fs::remove_file(&h.sock_path);
    true
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

/// Per-plugin thread draining the bounded event queue onto the plugin's
/// events channel (or the serve channel, when it declared none).
fn spawn_event_pump(handle: Arc<PluginHandle>, rx: Receiver<ServeMsg>) {
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
fn dispatch_event(
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

fn cleanup_plugin(inner: &Arc<HostInner>, name: &PluginName) {
    inner
        .routes
        .lock()
        .unwrap()
        .remove_where(&|_, a| a == &Answerer::Plugin(name.clone()));
    inner.subs.lock().unwrap().retain(|s| match &s.target {
        SubTarget::Plugin(n) => n != name,
        _ => true,
    });
}

/// Bring every plugin's routes in line with whether its needs are met.
///
/// Called after anything that changes what the table answers — a spawn, a
/// shutdown — and repeated until nothing moves, because a plugin becoming
/// usable can be the thing another one was waiting for. That loop is the
/// whole of dependency ordering: nothing declares an order, and the order
/// falls out of who is waiting for whom.
///
/// A plugin whose needs are unmet keeps running and keeps its names; what it
/// loses is being answered. That is deliberately the same state a stopped
/// plugin leaves behind — callers get "no route" either way — because "not
/// there yet" and "not there any more" are the same thing to a caller, and
/// inventing a second way to be absent would be a second rule to learn.
fn settle(kernel: &Arc<Kernel>, inner: &Arc<HostInner>) {
    loop {
        let mut changed = false;
        let plugins: Vec<(PluginName, Arc<PluginHandle>)> = inner
            .plugins
            .lock()
            .unwrap()
            .iter()
            .map(|(n, h)| (n.clone(), h.clone()))
            .collect();

        for (name, handle) in plugins {
            // A plugin with no verbs answers nothing by design; it has no
            // route state to move, and asking whether it is "routed" would
            // be asking a question with no meaning.
            let declared = handle.declared.lock().unwrap();
            if declared.verbs.is_empty() {
                continue;
            }
            let mut routes = inner.routes.lock().unwrap();
            let who = Answerer::Plugin(name.clone());
            // Both halves of "can this plugin do its job": somebody answers
            // what it needs, *and* it is allowed to ask. Those were being
            // answered by one thing, which is why nothing could tell "not
            // allowed" from "nobody there" — and why a missing grant used to
            // surface mid-turn instead of at startup.
            let subject = name.subject();
            let ready = declared.needs.iter().all(|need| {
                !routes.answerers(need).is_empty()
                    && kernel.caps.allows(
                        &subject,
                        &need.resource(),
                        need.short(),
                        crate::db::now_unix(),
                    )
            });
            // Reconciled against what the plugin declares, not toggled on a
            // transition: a plugin can learn a new verb while already
            // answering others, and a rule that only fires on "was not
            // routed, now is" leaves every verb after the first unrouted.
            let before = routes.verbs_of(&name);
            if ready {
                for v in &declared.verbs {
                    if before.contains(v) {
                        continue;
                    }
                    let meta = declared.tools.get(v).cloned().unwrap_or_default();
                    if let Err(conflict) = routes.add(v.clone(), who.clone(), meta) {
                        // Only a verb this plugin already answers is refused,
                        // and the SDK never declares one twice; if it does,
                        // the declaration and the table have drifted.
                        eprintln!("[kernel] {name}: {conflict}");
                    }
                }
            } else {
                routes.remove_where(&|_, a| a == &who);
            }

            // Progress is what the table *did*, not what we meant it to do.
            // Deciding otherwise is how a change that cannot happen becomes
            // a loop that never ends.
            if routes.verbs_of(&name) != before {
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
}

/// What a plugin is still waiting for, if anything.
fn unmet(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    handle: &PluginHandle,
    name: &PluginName,
) -> Vec<Verb> {
    let routes = inner.routes.lock().unwrap();
    let subject = name.subject();
    let declared = handle.declared.lock().unwrap();
    declared
        .needs
        .iter()
        .filter(|need| {
            routes.answerers(need).is_empty()
                || !kernel.caps.allows(
                    &subject,
                    &need.resource(),
                    need.short(),
                    crate::db::now_unix(),
                )
        })
        .cloned()
        .collect()
}

/// The client-channel loop: serve one plugin's kernel requests until EOF.
fn spawn_client_loop(
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
    name: PluginName,
    mut stream: UnixStream,
) {
    std::thread::spawn(move || {
        loop {
            let bytes = match frame::read_bytes(&mut stream) {
                Ok(b) => b,
                Err(_) => return, // plugin went away; spawn/shutdown owns cleanup
            };
            inner
                .meter
                .lock()
                .unwrap()
                .count_context(bytes.len() as u64);
            let outcome = match ClientOp::from_slice(&bytes) {
                Ok(op) => handle_client_op(&kernel, &inner, &name, op, &mut stream),
                Err(e) => Err(KernelError::Corrupt(e.to_string())),
            };
            let resp = match outcome {
                Ok(Some(body)) => body,
                Ok(None) => continue, // the op wrote its own response
                Err(e) => serde_json::to_vec(&Reply::<()>::Err(e.to_string()))
                    .expect("error reply serializes"),
            };
            inner.meter.lock().unwrap().count_context(resp.len() as u64);
            if frame::write_bytes(&mut stream, &resp).is_err() {
                return;
            }
        }
    });
}

/// Encode a successful reply body. `Ok(None)` from an op means it already
/// wrote its own response frame, as the streaming read does.
fn ok<T: Serialize>(body: &T) -> Result<Option<Vec<u8>>, KernelError> {
    serde_json::to_vec(&Reply::Ok(body))
        .map(Some)
        .map_err(|e| KernelError::Corrupt(format!("reply encode: {e}")))
}

fn handle_client_op(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    name: &PluginName,
    op: ClientOp,
    stream: &mut UnixStream,
) -> Result<Option<Vec<u8>>, KernelError> {
    let now = crate::db::now_unix();
    let subject = name.subject();
    match op {
        // ---- invoke: the capability-gated plugin→plugin path ----
        ClientOp::Invoke(req) => {
            let verb = req.verb;
            let cap =
                match kernel
                    .caps
                    .find_and_exercise(&subject, &verb.resource(), verb.short(), now)
                {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = kernel.audit.lock().unwrap().append(json!({
                            "event": "invoke.denied", "from": name.as_str(),
                            "verb": verb.as_str(), "reason": e.to_string(),
                        }));
                        return Err(e);
                    }
                };
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "invoke.allowed", "from": name.as_str(),
                "verb": verb.as_str(), "cap": cap,
            }));
            // No branch for the kernel's own verbs: they are rows, and
            // resolution finds them the way it finds anything else.
            let (target, name_there) = route(inner, &verb, req.at.as_ref())?;
            match target {
                Answerer::Kernel => builtin_verb(kernel, inner, &name_there, req.args),
                Answerer::Plugin(plugin) => {
                    let handle = inner
                        .plugins
                        .lock()
                        .unwrap()
                        .get(&plugin)
                        .cloned()
                        .ok_or_else(|| KernelError::NotFound(format!("plugin gone: {plugin}")))?;
                    // The name the *target* knows, not the one looked up.
                    ok(&call_on(&handle, &name_there, req.args)?)
                }
            }
        }

        // ---- grants introspection: what may *this* plugin invoke? ----
        // The capability table knows the verbs and budgets; the route table
        // knows what each driver said about them. Joining the two hands the
        // caller a ready-made tool definition — the driver owns descriptions
        // and schemas, the user owns grants, the kernel owns neither.
        ClientOp::Grants => {
            // One table. What the kernel answers itself is in it like
            // anything else, so there is nothing to look in first.
            let grants = kernel.caps.grants_for(&subject, now, |verb| {
                inner
                    .routes
                    .lock()
                    .unwrap()
                    .answerers(verb)
                    .into_iter()
                    .map(|(a, m)| (a.id(), m.clone()))
                    .collect()
            })?;
            ok(&GrantsReply { grants })
        }

        // ---- event bus ----
        ClientOp::Emit(req) => {
            let delivered = dispatch_event(kernel, inner, &req.topic, &req.data);
            ok(&EmitReply { delivered })
        }
        ClientOp::Subscribe(req) => {
            let id = SubId::new(inner.next_sub.fetch_add(1, Ordering::SeqCst));
            inner.subs.lock().unwrap().push(Sub {
                id,
                pattern: req.topic.clone(),
                target: SubTarget::Plugin(name.clone()),
            });
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "events.subscribed", "plugin": name.as_str(),
                "topic": req.topic.as_str(), "sub": id.get(),
            }));
            ok(&SubscribeReply { sub: id })
        }
        ClientOp::Unsubscribe(req) => {
            // A plugin can only drop its own subscriptions.
            let removed = {
                let mut subs = inner.subs.lock().unwrap();
                let before = subs.len();
                subs.retain(|s| {
                    !(s.id == req.sub && matches!(&s.target, SubTarget::Plugin(n) if n == name))
                });
                before != subs.len()
            };
            ok(&UnsubscribeReply { removed })
        }

        // ---- artifact ingest: frame, then chunk stream ----
        ClientOp::Put(req) => {
            let labels = req.labels.unwrap_or_default();
            let mut reader = chunk::ChunkReader::new(&mut *stream);
            let result = kernel
                .cas
                .put_stream(&mut reader, &req.content_type, labels, &subject);
            // Resync the stream even on a CAS error so the channel survives.
            reader
                .drain()
                .map_err(|e| KernelError::Corrupt(format!("put drain: {e}")))?;
            let meta = result?;
            inner.meter.lock().unwrap().count_data(meta.size);
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "artifact.put", "plugin": name.as_str(),
                "id": meta.id, "size": meta.size,
            }));
            ok(&PutReply { meta })
        }

        // ---- taking on verbs after the fact ----
        //
        // A plugin that mirrors somebody else cannot know what it answers
        // until it has asked them, and asking requires being up. So the
        // hello is the ordinary way to declare verbs, not the only one.
        ClientOp::Claim(req) => {
            let verbs: Vec<Verb> = req.tools.keys().cloned().collect();
            if let Some(handle) = inner.plugins.lock().unwrap().get(name) {
                let mut declared = handle.declared.lock().unwrap();
                for (v, meta) in req.tools {
                    if !declared.verbs.contains(&v) {
                        declared.verbs.push(v.clone());
                    }
                    declared.tools.insert(v, meta);
                }
            }
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "plugin.claimed", "plugin": name.as_str(),
                "verbs": verbs.iter().map(Verb::as_str).collect::<Vec<_>>(),
            }));
            settle(kernel, inner);
            ok(&ClaimReply {
                claimed: verbs.len() as u64,
            })
        }

        // ---- artifact by path: the data plane without moving the data ----
        //
        // Audited like a read, because it *is* a read — of everything, by
        // whatever the caller hands the path to.
        ClientOp::Locate(req) => {
            let path = kernel.cas.path_of(&req.id)?;
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "artifact.located", "plugin": name.as_str(), "id": req.id,
            }));
            ok(&LocateReply {
                path: path.to_string_lossy().into_owned(),
            })
        }

        // ---- artifact dereference: response frame, then chunk stream ----
        ClientOp::Read(req) => {
            // Reads are free but accounted: audit before bytes move.
            let meta = kernel.cas.meta(&req.id)?;
            let avail = meta.size.saturating_sub(req.offset);
            let n = req.len.map(|w| w.min(avail)).unwrap_or(avail);
            let mut f = kernel.cas.open_read(&req.id)?;
            f.seek(SeekFrom::Start(req.offset))?;
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "artifact.read", "plugin": name.as_str(), "id": req.id,
                "offset": req.offset, "len": n,
            }));
            let written = frame::write_frame(stream, &Reply::Ok(ReadReply { len: n }))
                .map_err(|e| KernelError::Corrupt(format!("read resp: {e}")))?;
            inner.meter.lock().unwrap().count_context(written);
            let mut taken = f.take(n);
            let moved = chunk::copy_into_chunks(&mut taken, stream)
                .map_err(|e| KernelError::Corrupt(format!("read stream: {e}")))?;
            inner.meter.lock().unwrap().count_data(moved);
            Ok(None) // response already sent
        }
    }
}

#[derive(Deserialize)]
struct StopArgs {
    name: PluginName,
}

#[derive(Serialize)]
struct PluginInfo {
    name: PluginName,
    verbs: Vec<Verb>,
    /// What it is still waiting for. Empty means it is answering.
    ///
    /// The diagnosis lives here rather than in the route table because the
    /// table has one job — does this name resolve — and a third state in it
    /// would be a special case for every reader of it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unmet: Vec<Verb>,
}

#[derive(Serialize)]
struct SpawnReply {
    name: PluginName,
    verbs: Vec<Verb>,
    /// Present when it started but is waiting on something. An agent that
    /// spawned a plugin and got this back knows to start what it needs
    /// rather than wondering why the new verbs are not there.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unmet: Vec<Verb>,
}

#[derive(Serialize)]
struct StopReply {
    stopped: bool,
}

#[derive(Serialize)]
struct PluginsReply {
    plugins: Vec<PluginInfo>,
}

/// What the kernel advertises about its own verbs. A driver describes its
/// verbs in its hello; the kernel has no hello, so it says so here — and
/// `grants` then hands these to a caller exactly like any driver's, which is
/// how a model comes to see `kernel__spawn` as an ordinary tool.

/// The verbs the kernel answers itself. Reaching here means the capability
/// gate already passed, exactly as for a routed verb — the kernel is not a
/// special caller, it is a special *callee*.
/// The kernel answering one of its own verbs — reached through resolution
/// like any other answerer, so this is dispatch on a resolved target rather
/// than a case before dispatch begins.
fn builtin_verb(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    verb: &Verb,
    args: Payload,
) -> Result<Option<Vec<u8>>, KernelError> {
    match verb.short() {
        "spawn" => {
            let spec: LaunchSpec = args
                .parse()
                .map_err(|e| KernelError::Corrupt(format!("kernel::spawn args: {e}")))?;
            let name = spawn_spec_on(kernel, inner, &spec)?;
            let (verbs, unmet) = list_plugins(kernel, inner)
                .into_iter()
                .find(|(n, _, _)| n == &name)
                .map(|(_, v, u)| (v, u))
                .unwrap_or_default();
            ok(&SpawnReply { name, verbs, unmet })
        }
        "stop" => {
            let a: StopArgs = args
                .parse()
                .map_err(|e| KernelError::Corrupt(format!("kernel::stop args: {e}")))?;
            ok(&StopReply {
                stopped: shutdown_on(kernel, inner, &a.name),
            })
        }
        "plugins" => ok(&PluginsReply {
            plugins: list_plugins(kernel, inner)
                .into_iter()
                .map(|(name, verbs, unmet)| PluginInfo { name, verbs, unmet })
                .collect(),
        }),
        // Routed here, so the table says the kernel answers it; a verb it
        // does not know is the table and this list having drifted.
        _ => Err(KernelError::Corrupt(format!(
            "the kernel does not answer {verb}"
        ))),
    }
}

/// Unwrap what a builtin produced for a kernel-side caller, which wants the
/// payload rather than an encoded reply frame.
fn reply_of(encoded: Option<Vec<u8>>) -> Result<Payload, KernelError> {
    let bytes = encoded.ok_or_else(|| KernelError::Corrupt("builtin wrote no reply".into()))?;
    let reply: Reply<Payload> = serde_json::from_slice(&bytes)
        .map_err(|e| KernelError::Corrupt(format!("builtin reply: {e}")))?;
    reply.into_result().map_err(KernelError::Denied)
}

/// The kernel's own verbs, as rows answered by the instance named `kernel`.
/// Nothing is reserved: a plugin may answer `kernel::spawn` as well — a
/// launcher for a form the kernel does not know — and callers then name
/// which of the two they mean. The descriptions live here rather than in a
/// second table, because a second table keyed by verb is the mistake this
/// whole change is about.
fn builtin_routes() -> RouteTable {
    let tool = |verb: &str, description: &str, schema: serde_json::Value| {
        (
            Verb::parse(verb).expect("constant verb"),
            ToolMeta {
                description: description.to_string(),
                schema: Payload::of(&schema).ok(),
            },
        )
    };
    let described: BTreeMap<Verb, ToolMeta> = BTreeMap::from([
        tool(
            "kernel::spawn",
            "Start a new plugin and grant it what it needs. Name it with \
             either `artifact` (an executable in the CAS) or `bin` (a path \
             on this host) — exactly one. The plugin's verbs \
             become available to anyone granted them — including, if the grants \
             say so, you — from your next turn onward. Use this to add a \
             capability the system does not currently have.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact": {"type": "string", "description":
                        "id of an executable stored in the CAS — the portable \
                         way to name a plugin"},
                    "bin": {"type": "string", "description":
                        "path to an executable on this host; use it for things \
                         already installed, such as `node`"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "env": {"type": "object"},
                    "grants": {
                        "type": "array",
                        "description": "capabilities to mint once it is up; \
                                        `subject` defaults to the new plugin",
                        "items": {
                            "type": "object",
                            "properties": {
                                "subject": {"type": "string"},
                                "resource": {"type": "string"},
                                "verbs": {"type": "array", "items": {"type": "string"}},
                            },
                            "required": ["resource", "verbs"],
                        },
                    },
                },
                // Exactly one of `artifact` and `bin`, which a JSON
                // schema cannot say and the kernel checks instead.
                "required": [],
            }),
        ),
        tool(
            "kernel::stop",
            "Stop a running plugin. Its verbs stop being routed and everything \
             it was granted is revoked; anything it started is collected too.",
            serde_json::json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
            }),
        ),
        tool(
            "kernel::plugins",
            "List the plugins currently running and the verbs each answers.",
            serde_json::json!({"type": "object", "properties": {}}),
        ),
    ]);

    let mut table = RouteTable::default();
    for verb in ["kernel::spawn", "kernel::stop", "kernel::plugins"] {
        let verb = Verb::parse(verb).expect("constant verb");
        let meta = described.get(&verb).cloned().unwrap_or_default();
        table
            .add(verb, Answerer::Kernel, meta)
            .expect("each kernel verb is registered once");
    }
    table
}

fn accept_with_deadline(
    listener: &UnixListener,
    child: &mut std::process::Child,
    ms: u64,
) -> Result<UnixStream, KernelError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false)?;
                return Ok(s);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(KernelError::Corrupt(format!(
                        "plugin exited before connecting: {status}"
                    )));
                }
                if std::time::Instant::now() > deadline {
                    return Err(KernelError::Corrupt("plugin connect timeout".into()));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Signal a plugin's whole process group. The group id equals the plugin's
/// own pid because it was spawned as a group leader; an error here means the
/// group is already gone, which is the outcome we wanted.
fn signal_group(pgid: u32, sig: Signal) {
    let _ = nix::sys::signal::killpg(Pid::from_raw(pgid as i32), sig);
}

/// Poll for exit up to `ms`. Returns whether the child is gone.
fn wait_for_exit(child: &mut std::process::Child, ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn rand_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}
