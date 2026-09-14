//! Starting a plugin: what it is (an artifact or a path, plus a bundle),
//! how it runs (its form), the handshake over the spawn socket, and the
//! registration that makes it part of the host before the spawn returns.

use super::client::spawn_client_loop;
use super::events::spawn_event_pump;
use super::ready::settle;
use super::{Declared, EVENT_QUEUE, Form, HostInner, LaunchSpec, PluginHandle, Sub, SubTarget};
use crate::{Kernel, KernelError};
use portos_abi::artifact::id_for_bytes;
use portos_abi::ids::{PluginName, SubId, Verb};
use portos_abi::wire::{ChannelRole, Hello, HelloFrame, Payload, Reply, ServeMsg};
use portos_abi::{ABI_VERSION, frame};
use portos_kernel_api::{MANIFEST_TYPE, Manifest};
use serde_json::json;
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};

/// Where a plugin's executable comes from. Two fields rather than one enum
/// so that a spec reads as `{"artifact": …}` or `{"bin": …}` and a typo gets
/// a message naming the field; "exactly one of them" is then checked here,
/// once.
enum Source<'a> {
    Artifact(&'a str),
    Path(&'a str),
}
/// What runs, once the spec has been read through its manifest if it names
/// one. A spec naming a plugin takes all four from the manifest; naming any
/// of them as well is refused, because two answers to "what runs" is the
/// mistake a manifest exists to remove.
struct WhatRuns {
    artifact: Option<String>,
    bin: Option<String>,
    bundle: Option<String>,
    args: Vec<String>,
    manifest: Option<Manifest>,
}

fn what_runs(kernel: &Kernel, spec: &LaunchSpec) -> Result<WhatRuns, KernelError> {
    let Some(id) = &spec.plugin else {
        return Ok(WhatRuns {
            artifact: spec.artifact.clone(),
            bin: spec.bin.clone(),
            bundle: spec.bundle.clone(),
            args: spec.args.clone(),
            manifest: None,
        });
    };
    if spec.artifact.is_some()
        || spec.bin.is_some()
        || spec.bundle.is_some()
        || !spec.args.is_empty()
    {
        return Err(KernelError::Denied(
            "launch spec names a plugin and also what runs; with `plugin`, artifact, bin, bundle \
             and args come from the manifest"
                .into(),
        ));
    }
    let meta = kernel.cas.meta(id)?;
    if meta.r#type != MANIFEST_TYPE {
        return Err(KernelError::Denied(format!(
            "{id} is {} rather than a {MANIFEST_TYPE}",
            meta.r#type
        )));
    }
    let mut bytes = Vec::new();
    kernel.cas.open_read(id)?.read_to_end(&mut bytes)?;
    let m: Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| KernelError::Corrupt(format!("manifest {id}: {e}")))?;
    Ok(WhatRuns {
        artifact: m.artifact.clone(),
        bin: m.bin.clone(),
        bundle: m.bundle.clone(),
        args: m.args.clone(),
        manifest: Some(m),
    })
}

/// Exactly one of `artifact` and `bin`, checked here, once.
fn source(runs: &WhatRuns) -> Result<Source<'_>, KernelError> {
    match (&runs.artifact, &runs.bin) {
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
const SPAWN_DEADLINE_MS: u64 = 10_000;
pub(super) fn spawn_spec_on(
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

    let runs = what_runs(kernel, spec)?;
    let bundle_dir = match &runs.bundle {
        Some(id) => Some(unpack(kernel, id)?),
        None => None,
    };
    let bin = match source(&runs)? {
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

    // Hashed before it runs, so that what ran is a fact about content
    // whatever the spec named it by. For an artifact this is its own id.
    let ran = hash_executable(&bin);

    let mut cmd = std::process::Command::new(&bin);
    if let Some(dir) = &bundle_dir {
        cmd.current_dir(dir);
    }
    cmd.args(&runs.args)
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

    // The manifest was a claim about this; the hello is the fact. A
    // difference is reported, not obeyed: env and config can legitimately
    // change what a plugin declares, and over a path the content can too.
    if let Some(drift) = runs.manifest.as_ref().and_then(|m| m.drift(&hello)) {
        eprintln!("[kernel] {name}: differs from its manifest — {drift}");
        let _ = kernel.audit.lock().unwrap().append(json!({
            "event": "plugin.drift", "plugin": name.as_str(),
            "manifest": spec.plugin, "drift": drift,
        }));
    }

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

        // What it listens to, in place before this spawn returns: whoever
        // started it may publish the moment it has. This is the only way a
        // plugin subscribes — there is no runtime op — so a subscription
        // cannot land after the event it was for.
        {
            let mut subs = inner.subs.lock().unwrap();
            for pattern in &hello.subscribes {
                let id = SubId::new(inner.next_sub.fetch_add(1, Ordering::SeqCst));
                subs.push(Sub {
                    id,
                    pattern: pattern.clone(),
                    target: SubTarget::Plugin(name.clone()),
                });
            }
        }
        let (events_tx, events_rx) = sync_channel::<ServeMsg>(EVENT_QUEUE);
        let handle = Arc::new(PluginHandle {
            declared: Mutex::new(Declared {
                verbs: verbs.clone(),
                tools: tools_meta.clone(),
                needs: hello.needs.clone(),
            }),
            plugin: spec.plugin.clone(),
            artifact: runs.artifact.clone(),
            bin: runs.bin.clone(),
            ran: ran.clone(),
            hello: Hello {
                token: String::new(),
                ..hello.clone()
            },
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
        // What ran, by the name it was asked for under, and by content:
        // `ran` is checkable later whichever of the three named it.
        "plugin": spec.plugin,
        "artifact": runs.artifact,
        "bin": runs.bin,
        "bundle": runs.bundle,
        "ran": ran,
        "form": spec.form,
        "verbs": verbs.iter().map(Verb::as_str).collect::<Vec<_>>(),
    }));
    Ok(name)
}
/// The content id of the file about to be executed. `None` only when it
/// cannot be read, in which case the exec is about to fail too.
fn hash_executable(bin: &Path) -> Option<String> {
    let path = if bin.components().count() > 1 {
        bin.to_path_buf()
    } else {
        // A bare name is what `Command` looks up on PATH; look the same way.
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(bin))
            .find(|p| p.is_file())?
    };
    std::fs::read(path).ok().map(|bytes| id_for_bytes(&bytes))
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
pub(super) fn rand_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}
