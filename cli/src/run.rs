//! `portos run <root>` — the launcher.
//!
//! The one thing that cannot be a plugin, because a plugin needs it in order
//! to be one: open the kernel, start what `<root>/portos.json` lists, mint
//! what each entry grants, and bring the set back in line on `SIGHUP`. It
//! knows no driver. A chat is a model driver, a front end and whatever else
//! the operator listed — `portos init` writes that list — and this process
//! starts the list and parks. The kernel stays in this process
//! (library-linked); daemonization is still deferred.
//!
//! portos.json shape:
//!   { "plugins": [ { "bin" | "artifact": …, "bundle"?, "name"?, "config"?,
//!                    "args"?, "env"?, "watch"?, "form"?,
//!                    "grants": [ {"subject"?, "resource": "driver:<driver>",
//!                                 "verbs": […]} ] } ],
//!     "audit_topics": ["egress::log"] }
//! A grant's subject defaults to the plugin it is listed under. Relative
//! paths in `args` resolve against `<root>` when they exist there; a `bin`
//! with no directory is looked for beside this binary, then on PATH; `name`
//! gives an entry its instance identifier, for running two of one driver.
//!
//! **Reload is re-plug.** `SIGHUP` re-reads the file and brings the running
//! set back in line: whatever changed is stopped and started again, whatever
//! did not is left alone. A plugin's grants and the files it lists under
//! `watch` count as part of what it *is* — filling in an API key changes the
//! broker without changing its command line, and that is the case reload
//! exists for. Starting a plugin mints its grants, so re-plugging owes it
//! nothing further. `audit_topics` are read once, at startup.

use nix::sys::signal::{SigSet, Signal};
use portos_abi::ids::{PluginName, Topic};
use portos_kernel::Kernel;
use portos_kernel::host::{Form, GrantSpec, Host, LaunchSpec};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const CONFIG: &str = "portos.json";

/// `<root>/portos.json`: what to run.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RunConfig {
    plugins: Vec<PluginSpec>,
    /// Event topics copied into the audit log as they happen. Policy, so it
    /// lives in the file: the broker's `egress::log` is there because
    /// `portos init` put it there, not because this launcher knows what a
    /// broker is.
    audit_topics: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PluginSpec {
    /// An executable stored in the CAS — `portos put <root> <file>` gives
    /// you the id. A plugin named this way is a thing rather than a
    /// location.
    #[serde(default)]
    artifact: Option<String>,
    /// A path on this host. Fine for `node` and other things already
    /// installed; not reproducible and not portable.
    #[serde(default)]
    bin: Option<String>,
    /// A tar archive in the CAS holding everything else the plugin needs —
    /// `portos bundle <root> <base> [paths…]` builds one. It becomes the
    /// working directory, so `args` are relative to it.
    #[serde(default)]
    bundle: Option<String>,
    /// The instance identifier. Needed only to run two instances of one
    /// driver; otherwise the plugin's own name is used.
    #[serde(default)]
    name: Option<String>,
    /// What this plugin should be, in its own vocabulary. Handed down rather
    /// than left for the plugin to find, so editing it re-plugs that driver
    /// on the next reload without anyone declaring which files matter.
    #[serde(default)]
    config: Option<serde_json::Value>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Files this plugin reads when it starts. Listing one here makes its
    /// contents part of what the plugin *is*, so editing it and reloading
    /// re-plugs this driver and nothing else.
    #[serde(default)]
    watch: Vec<String>,
    /// How it runs: `cgroup` (the default) or `bare`. Part of the spec, so a
    /// change to it re-plugs the driver like any other.
    #[serde(default)]
    form: Form,
    /// What it may invoke, minted once it is up. A grant is declared, not
    /// negotiated: whatever is listed here is what the subject may call. The
    /// subject defaults to this plugin.
    #[serde(default)]
    grants: Vec<GrantSpec>,
}

pub fn run(root: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Block the interrupt signals before anything else starts a thread, so
    // every thread inherits the mask and only the handler thread below ever
    // sees them. Without this, Ctrl-C tears the process down without running
    // teardown, and every plugin — and everything it started — is orphaned.
    let signals = block_interrupts()?;

    // Canonical for the same reason the kernel canonicalises its socket
    // directory: every path handed to a plugin has to mean the same thing
    // whatever working directory that plugin ends up with.
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
    let cfg = load_config(&root)?;
    let kernel = Arc::new(Kernel::open(&root)?);
    let host = Arc::new(Host::new(kernel.clone(), &root.join("sock"))?);
    for topic in &cfg.audit_topics {
        host.audit_topic(&Topic::parse(topic)?);
    }
    let exe = std::env::current_exe()?;

    // What this process started. Startup and reload run the same code over
    // it; at startup everything is new.
    let state = Arc::new(Mutex::new(RuntimeState::default()));
    let failures = converge(&host, &root, &exe, &mut state.lock().unwrap());
    if let Some(first) = failures.first() {
        host.shutdown_all();
        return Err(first.clone().into());
    }
    println!("[run] up — SIGHUP reloads {CONFIG}, Ctrl-C stops");

    spawn_signal_handler(
        signals,
        Reloadable {
            host: host.clone(),
            root,
            exe,
            state,
        },
    );
    // Parked: the plugins are the runtime, and a front end among them owns
    // whatever interaction there is.
    let (_keep, never) = std::sync::mpsc::channel::<()>();
    let _ = never.recv();
    Ok(())
}

/// Take the interrupt signals away from the default disposition, which is to
/// kill the process without running any teardown. Called before any thread
/// exists so the mask is inherited everywhere.
fn block_interrupts() -> Result<SigSet, Box<dyn std::error::Error>> {
    let mut set = SigSet::empty();
    set.add(Signal::SIGINT);
    set.add(Signal::SIGTERM);
    // SIGHUP joins them because it is handled on the same thread, for the
    // same reason: reloading means stopping and starting plugins, which is
    // real work and must not happen inside a signal handler.
    set.add(Signal::SIGHUP);
    set.thread_block()?;
    Ok(set)
}

/// One thread that does nothing but wait for a signal. Waiting rather than
/// handling means it may do real work — tear plugins down, reload — none of
/// which is safe inside an actual signal handler.
fn spawn_signal_handler(signals: SigSet, rt: Reloadable) {
    std::thread::spawn(move || {
        loop {
            let Ok(sig) = signals.wait() else {
                return;
            };
            match sig {
                Signal::SIGHUP => rt.reload(),
                // A stop that was asked for — a front end's `/exit` arrives
                // this way — is not a failure; an interrupt is reported as
                // one, the way the shell expects.
                Signal::SIGTERM => {
                    eprintln!("[run] shutting down");
                    rt.host.shutdown_all();
                    std::process::exit(0);
                }
                _ => {
                    eprintln!("\n[run] interrupted, shutting down");
                    rt.host.shutdown_all();
                    std::process::exit(130);
                }
            }
        }
    });
}

/// What the signal thread needs in order to reload.
struct Reloadable {
    host: Arc<Host>,
    root: PathBuf,
    exe: PathBuf,
    state: Arc<Mutex<RuntimeState>>,
}

impl Reloadable {
    fn reload(&self) {
        let mut state = self.state.lock().unwrap();
        eprintln!("[run] reloading");
        // A bad edit must not leave the runtime emptier than it found it,
        // so a failed start is reported and the rest still converges.
        for f in converge(&self.host, &self.root, &self.exe, &mut state) {
            eprintln!("[run] reload: {f}");
        }
    }
}

/// What this process started. Nothing else: a plugin an agent started with
/// `kernel::spawn` is not in here, and a reload leaves it alone.
#[derive(Default)]
struct RuntimeState {
    /// fingerprint → the name the plugin declared. Keyed by fingerprint
    /// rather than by name because a name is something a plugin declares
    /// *after* it starts, while the decision to start it is made before.
    running: BTreeMap<String, PluginName>,
}

/// One plugin the runtime wants running.
struct Desired {
    spec: LaunchSpec,
    /// Files it reads when it starts. Their contents are part of what it
    /// *is* — filling in an API key changes the broker without changing its
    /// command line, and a reload that compared only command lines would
    /// miss the one case it exists for.
    watch: Vec<PathBuf>,
}

impl Desired {
    /// How to name it in a message: by what the spec actually said.
    fn label(&self) -> String {
        match (&self.spec.artifact, &self.spec.bin) {
            (Some(id), _) => id.clone(),
            (_, Some(bin)) => bin.clone(),
            _ => "<no bin or artifact>".to_string(),
        }
    }

    /// What "the same plugin" means here.
    ///
    /// Hashed rather than kept: the secrets file goes through this, and the
    /// launcher has no business holding an API key in memory to compare it
    /// later.
    fn fingerprint(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(self.spec.artifact.as_deref().unwrap_or("").as_bytes());
        h.update(b"\0b");
        h.update(self.spec.bin.as_deref().unwrap_or("").as_bytes());
        h.update(b"\0u");
        h.update(self.spec.bundle.as_deref().unwrap_or("").as_bytes());
        h.update(b"\0n");
        h.update(self.spec.name.as_deref().unwrap_or("").as_bytes());
        // Config is part of what the plugin *is*: change it and the next
        // reload re-plugs this one and nothing else.
        h.update(b"\0c");
        h.update(
            self.spec
                .config
                .as_ref()
                .map(|c| c.as_raw())
                .unwrap_or("")
                .as_bytes(),
        );
        for a in &self.spec.args {
            h.update(b"\0a");
            h.update(a.as_bytes());
        }
        for (k, v) in &self.spec.env {
            h.update(b"\0e");
            h.update(k.as_bytes());
            h.update(b"=");
            h.update(v.as_bytes());
        }
        // So are its grants: they are minted when it starts, so changing
        // them is a reason to start it again.
        for g in &self.spec.grants {
            h.update(b"\0g");
            h.update(g.subject.as_deref().unwrap_or("").as_bytes());
            h.update(b"@");
            h.update(g.resource.as_bytes());
            for v in &g.verbs {
                h.update(b",");
                h.update(v.as_bytes());
            }
        }
        h.update(b"\0f");
        h.update(format!("{:?}", self.spec.form).as_bytes());
        for p in &self.watch {
            h.update(b"\0w");
            h.update(p.as_os_str().as_encoded_bytes());
            // A file that is not there yet is a state like any other: create
            // it later and the fingerprint changes, which is the point.
            if let Ok(bytes) = std::fs::read(p) {
                h.update(&bytes);
            }
        }
        h.finalize().to_hex().to_string()
    }
}

fn load_config(root: &Path) -> Result<RunConfig, String> {
    let path = root.join(CONFIG);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: {e} — `portos init {}` writes one",
            path.display(),
            root.display()
        )
    })?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// What the file lists.
///
/// Not in any order: a plugin declares what it needs and the kernel answers
/// it when that arrives, so the order these are started in stops mattering.
/// It used to be a hand-written list with the broker first — and that order
/// was silently thrown away when this began keying by fingerprint, which
/// nothing noticed, because nothing was checking. An ordering nobody can
/// check is an ordering nobody has.
fn desired(cfg: &RunConfig, root: &Path, exe: &Path) -> Vec<Desired> {
    cfg.plugins
        .iter()
        .map(|p| Desired {
            spec: LaunchSpec {
                artifact: p.artifact.clone(),
                bin: p.bin.as_deref().map(|b| resolve_bin(exe, b)),
                bundle: p.bundle.clone(),
                name: p.name.clone(),
                config: p
                    .config
                    .as_ref()
                    .and_then(|c| portos_abi::wire::Payload::of(c).ok()),
                // A bundle brings its own working directory, so its args are
                // already relative to something real and must not be turned
                // into paths on this host.
                args: match p.bundle {
                    Some(_) => p.args.clone(),
                    None => p.args.iter().map(|a| resolve_arg(root, a)).collect(),
                },
                env: p.env.clone(),
                grants: p.grants.clone(),
                form: p.form,
            },
            watch: p.watch.iter().map(|w| root.join(w)).collect(),
        })
        .collect()
}

/// A `bin` with no directory in it is looked for beside this binary first,
/// which is where the standard plugins are installed, and otherwise left to
/// PATH the way `node` is.
fn resolve_bin(exe: &Path, bin: &str) -> String {
    if bin.contains('/') {
        return bin.to_string();
    }
    let sibling = exe.with_file_name(bin);
    if sibling.exists() {
        sibling.to_string_lossy().into_owned()
    } else {
        bin.to_string()
    }
}

fn resolve_arg(root: &Path, arg: &str) -> String {
    let candidate = root.join(arg);
    if !Path::new(arg).is_absolute() && candidate.exists() {
        candidate.to_string_lossy().into_owned()
    } else {
        arg.to_string()
    }
}

/// Bring the running set in line with the file. Returns the failures rather
/// than stopping at the first, so one bad entry does not leave the runtime
/// emptier than it found it.
///
/// Reload is nothing more than this function running a second time. There is
/// no per-setting "is this live?" question to answer, because the unit of
/// change is a plugin: `kernel::stop` already collects its whole residue and
/// starting it again puts it back, grants included.
fn converge(host: &Host, root: &Path, exe: &Path, state: &mut RuntimeState) -> Vec<String> {
    let cfg = match load_config(root) {
        Ok(cfg) => cfg,
        Err(e) => return vec![e],
    };
    let wanted: BTreeMap<String, Desired> = desired(&cfg, root, exe)
        .into_iter()
        .map(|d| (d.fingerprint(), d))
        .collect();
    let mut failures = Vec::new();

    // Stopping comes first: a plugin whose config changed comes back under
    // the same name, and a name is held by one process at a time.
    let stale: Vec<String> = state
        .running
        .keys()
        .filter(|fp| !wanted.contains_key(*fp))
        .cloned()
        .collect();
    for fp in stale {
        if let Some(name) = state.running.remove(&fp) {
            host.shutdown(&name);
            println!("[run] stopped {name}");
        }
    }
    for (fp, d) in &wanted {
        if state.running.contains_key(fp) {
            continue;
        }
        match host.spawn_spec(&d.spec) {
            Ok(name) => {
                println!("[run] started {name}");
                state.running.insert(fp.clone(), name);
            }
            Err(e) => failures.push(format!("{}: {e}", d.label())),
        }
    }

    // Reported once everything is up and granted, because until then
    // "waiting" is just "not yet" and saying so would be noise.
    for (name, _, unmet) in host.plugins() {
        if !unmet.is_empty() {
            let what: Vec<String> = unmet.iter().map(|v| v.to_string()).collect();
            println!(
                "[run] {name} is not answering — it needs {}",
                what.join(", ")
            );
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(watch: Vec<PathBuf>) -> Desired {
        Desired {
            spec: LaunchSpec::from_path("/bin/true"),
            watch,
        }
    }

    /// The whole reason reload keys on a fingerprint rather than a command
    /// line: filling in an API key changes nothing a process listing would
    /// show, and it is the change that matters most.
    #[test]
    fn a_watched_files_contents_are_part_of_what_the_plugin_is() {
        let dir = std::env::temp_dir().join(format!("portos-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let secrets = dir.join("secrets.json");

        // Absent is a state like any other.
        let missing = desired(vec![secrets.clone()]).fingerprint();

        std::fs::write(&secrets, r#"{"key":""}"#).unwrap();
        let empty = desired(vec![secrets.clone()]).fingerprint();
        assert_ne!(missing, empty, "the file appearing is a change");

        std::fs::write(&secrets, r#"{"key":"filled-in"}"#).unwrap();
        let filled = desired(vec![secrets.clone()]).fingerprint();
        assert_ne!(empty, filled, "filling in the key must re-plug the broker");

        assert_eq!(
            filled,
            desired(vec![secrets.clone()]).fingerprint(),
            "and an unchanged plugin must be left alone"
        );

        // A plugin that watches nothing is decided by its spec alone.
        assert_eq!(desired(vec![]).fingerprint(), desired(vec![]).fingerprint());
        assert_ne!(desired(vec![]).fingerprint(), filled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two plugins differing only in one environment variable, or in one
    /// grant, are two plugins — which is what makes editing `portos.json`
    /// re-plug just the driver that changed.
    #[test]
    fn the_spec_decides_identity_when_nothing_is_watched() {
        let base = desired(vec![]);
        let mut other = desired(vec![]);
        other
            .spec
            .env
            .insert("PORTOS_FS_ROOT".into(), "/srv".into());
        assert_ne!(base.fingerprint(), other.fingerprint());

        let mut granted = desired(vec![]);
        granted.spec.grants.push(GrantSpec {
            subject: None,
            resource: "driver:fs".into(),
            verbs: ["read".to_string()].into_iter().collect(),
        });
        assert_ne!(base.fingerprint(), granted.fingerprint());
    }
}
