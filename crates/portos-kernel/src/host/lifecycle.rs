//! Spawn and authenticate plugin processes, then publish their checked routes.

use super::cleanup::HostWorld;
use super::client::spawn_client_loop;
use super::declaration::{
    check_class_declaration, declaration_identity, manifest_from_meta, str_array, target_from_meta,
};
use super::events::spawn_event_pump;
use super::routing::{ProtocolSession, RouteEntry, routed_families};
use super::{EVENT_QUEUE, HostInner, PluginHandle, SPAWN_DEADLINE_MS, Slot};
use crate::ledger::{CLASS_PLUGIN, ExclusiveRequest, capture_process};
use crate::{Kernel, KernelError};
use portos_proto::frame;
use portos_rm::cleanup::CleanupTarget;
use portos_rm::coeffect::{Flat, Mount, admit_mount};
use portos_rm::identity::{
    ClassId, Generation, HoldingHandle, InstanceId, ResourceKey, SubjectId, VerbId,
};
use portos_rm::time::{LeaseRequest, Timestamp};
use serde_json::{Value, json};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};

/// Spawn a plugin process: `bin args…` with `envs` added, wait for the
/// hellos, register the plugin's verbs, and start its service threads.
/// Returns the plugin name (from its hello).
///
/// With `parent = Some((parent_name, parent_holding))` the new plugin's
/// `kernel/plugin` holding becomes a child of the parent's holding (decision
/// 2: the instantiation edge is declared here, at spawn), so reclaiming the
/// parent tears the child down first (F2 closure).
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_plugin(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    plans: &Arc<crate::plans::PlanService>,
    bin: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    slot: Option<&Slot>,
    parent: Option<(&str, HoldingHandle)>,
) -> Result<String, KernelError> {
    let idx = inner.next_spawn.fetch_add(1, Ordering::SeqCst);
    let sock_path = inner
        .sock_dir
        .join(format!("plugin-{}-{idx}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;
    listener.set_nonblocking(true)?;
    let token = rand_token();

    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .env("PORTOS_PLUGIN_SOCK", &sock_path)
        .env("PORTOS_PLUGIN_TOKEN", &token);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;

    // The serve connection comes first and declares which extra channels
    // follow ("client" always; "events" optionally). Bad token or an
    // undeclared/duplicate role is fatal for the spawn.
    let accept_hello =
        |child: &mut std::process::Child| -> Result<(UnixStream, Value), KernelError> {
            let mut stream = accept_with_deadline(&listener, child, SPAWN_DEADLINE_MS)?;
            let hello = frame::read_frame(&mut stream)
                .map_err(|e| KernelError::Corrupt(format!("hello: {e}")))?;
            if hello["hello"]["token"].as_str() != Some(token.as_str()) {
                let _ = frame::write_frame(&mut stream, &json!({"err": "bad token"}));
                return Err(KernelError::Denied("plugin hello: bad token".into()));
            }
            frame::write_frame(&mut stream, &json!({"ok": {}}))
                .map_err(|e| KernelError::Corrupt(format!("hello ack: {e}")))?;
            Ok((stream, hello["hello"].clone()))
        };

    let (serve_stream, hello) = match accept_hello(&mut child) {
        Ok((stream, h)) if h["role"] == "serve" => (stream, h),
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
    let (name, verbs) = match declaration_identity(&hello) {
        Ok(identity) => identity,
        Err(e) => {
            let _ = child.kill();
            return Err(KernelError::Denied(format!("plugin hello: {e}")));
        }
    };
    // Optional per-verb tool metadata (description + schema + kind +
    // requires): joined into `grants`, checked by the F4/F5 laws below.
    let tools_meta = hello["tools"].clone();
    let mut expected: Vec<String> = hello["channels"]
        .as_array()
        .map(|_| str_array(&hello["channels"]))
        .unwrap_or_else(|| vec!["client".to_string()]);
    if !expected.iter().any(|c| c == "client") {
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
        let role = h["role"].as_str().unwrap_or("?").to_string();
        match expected.iter().position(|c| *c == role) {
            Some(i) => {
                expected.remove(i);
                match role.as_str() {
                    "client" => client = Some(stream),
                    "events" => events = Some(stream),
                    _ => {
                        let _ = child.kill();
                        return Err(KernelError::Corrupt(format!(
                            "plugin hello: unknown channel role {role}"
                        )));
                    }
                }
            }
            None => {
                let _ = child.kill();
                return Err(KernelError::Corrupt(format!(
                    "plugin hello: undeclared or duplicate role {role}"
                )));
            }
        }
    }
    let client_stream = client.expect("client channel present");

    // ---- F4: the verb character the plugin declared, checked at the door.
    let class = match check_class_declaration(&name, &verbs, &tools_meta, &hello) {
        Ok(class) => class,
        Err(e) => {
            let _ = child.kill();
            return Err(KernelError::Denied(format!(
                "plugin {name}: verb metadata rejected: {e}"
            )));
        }
    };
    let protocol = class.protocol().cloned();

    // ---- F5: slot admission — every verb's requires must fit the row.
    if let Some(slot) = slot {
        let manifest = manifest_from_meta(&name, &verbs, &tools_meta);
        let mut provides: Vec<String> = slot.provides.clone();
        provides.extend(routed_families(inner));
        let mount = Mount {
            name: "slot".into(),
            offers: Flat(slot.offers.iter().cloned().collect()),
            provides: Flat(provides.into_iter().collect()),
        };
        if let Err(e) = admit_mount(&manifest, &mount) {
            let _ = child.kill();
            return Err(KernelError::Denied(format!(
                "plugin {name}: slot admission failed: {e:?}"
            )));
        }
    }
    let offers: Option<Flat> = slot.map(|s| Flat(s.offers.iter().cloned().collect()));

    // Serialize publication with cleanup of this host registry. The transaction
    // performs no world actions, so holding the host lock cannot form a cycle.
    let now = crate::db::now_unix();
    let subject = format!("plugin:{name}");
    let process = capture_process(child.id()).map_err(|e| {
        let _ = child.kill();
        e
    })?;
    let mut plugins = inner.plugins.lock().unwrap();
    let holding = kernel
        .ledger
        .hold_managed(
            ExclusiveRequest {
                owner: SubjectId::new(&subject),
                resource: ResourceKey::new(ClassId::new(CLASS_PLUGIN), InstanceId::new(&name)),
                generation: Generation::new(&token),
                parent: parent.as_ref().map(|(_, h)| h.clone()),
                lease: LeaseRequest::UseClassDefault,
            },
            CleanupTarget::Plugin {
                host: inner.witness.clone(),
                process,
            },
            parent
                .as_ref()
                .map(|(name, _)| SubjectId::new(format!("plugin:{name}"))),
            Timestamp::try_from(now).map_err(crate::ledger::map_err)?,
        )
        .map_err(|e| {
            let _ = child.kill();
            e
        })?;
    let pid = child.id();

    // Register verbs; a route conflict aborts the spawn (and releases the
    // holding again — nothing in the ledger outlives a failed spawn).
    {
        let mut routes = inner.routes.lock().unwrap();
        if let Some(v) = verbs.iter().find(|v| routes.contains_key(*v)) {
            let conflict = v.clone();
            drop(routes);
            drop(plugins);
            let _ = kernel.ledger.release_with_world(
                &holding,
                &mut HostWorld {
                    inner: inner.clone(),
                },
                Timestamp::try_from(now).expect("system timestamp in range"),
            );
            let _ = child.try_wait();
            return Err(KernelError::Denied(format!(
                "verb already routed: {conflict}"
            )));
        }
        let (events_tx, events_rx) = sync_channel::<Value>(EVENT_QUEUE);
        let handle = Arc::new(PluginHandle {
            child: Mutex::new(child),
            serve: Mutex::new(serve_stream),
            events: events.map(Mutex::new),
            events_tx,
            sock_path: sock_path.clone(),
            holding: holding.clone(),
            pid,
            offers,
            protocol: Mutex::new(protocol.map(ProtocolSession::new)),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
        });
        for v in &verbs {
            let meta = &tools_meta[v.as_str()];
            let entry = class.lookup(&VerbId::new(v)).ok();
            let target = target_from_meta(meta.get("target"));
            routes.insert(
                v.clone(),
                RouteEntry {
                    plugin: name.clone(),
                    description: meta["description"].as_str().unwrap_or("").to_string(),
                    schema: if meta["schema"].is_object() {
                        meta["schema"].clone()
                    } else {
                        json!({"type": "object"})
                    },
                    character: entry.cloned(),
                    target,
                },
            );
        }
        plugins.insert(name.clone(), handle.clone());
        spawn_event_pump(handle.clone(), events_rx);
        spawn_client_loop(
            kernel.clone(),
            inner.clone(),
            plans.clone(),
            name.clone(),
            holding.clone(),
            client_stream,
        );
    }

    let _ = kernel.audit.lock().unwrap().append(json!({
        "event": "plugin.spawned", "plugin": name, "verbs": verbs,
        "holding": holding.id().get(), "pid": pid, "parent": parent.as_ref().map(|(p, _)| p),
    }));
    Ok(name)
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
