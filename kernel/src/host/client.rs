//! The client channel: a plugin's requests to the kernel — invoke,
//! grants, emit, put, read, locate, claim — served one frame at a time.

use super::builtin::builtin_verb;
use super::events::dispatch_event;
use super::ready::settle;
use super::{HostInner, call_on, route};
use crate::routes::Answerer;
use crate::{Kernel, KernelError};
use portos_abi::ids::{PluginName, Verb};
use portos_abi::wire::{
    ClaimReply, ClientOp, EmitReply, GrantsReply, LocateReply, PutReply, ReadReply, Reply,
};
use portos_abi::{chunk, frame};
use portos_router::Router as _;
use serde::Serialize;
use serde_json::json;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

/// The client-channel loop: serve one plugin's kernel requests until EOF.
pub(super) fn spawn_client_loop(
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
pub(super) fn ok<T: Serialize>(body: &T) -> Result<Option<Vec<u8>>, KernelError> {
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
