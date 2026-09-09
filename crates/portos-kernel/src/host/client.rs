//! Serve the plugin-to-kernel request channel and encode its responses.

use super::HostInner;
use super::cleanup::{HostWorld, reclaim_holding};
use super::declaration::{kind_label, str_array};
use super::events::{Sub, SubTarget, dispatch_event};
use super::lifecycle::spawn_plugin;
use super::resources::{op_hold, op_release, op_renew};
use super::routing::call_on;
use crate::ledger::{CLASS_SUBSCRIPTION, ExclusiveRequest};
use crate::{Kernel, KernelError};
use portos_proto::{Label, chunk, frame};
use portos_rm::cleanup::CleanupTarget;
use portos_rm::identity::{AccountId, ClassId, HoldingHandle, InstanceId, ResourceKey, SubjectId};
use portos_rm::time::{LeaseRequest, Timestamp};
use portos_rm::verbs::CheckedVerb;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The client-channel loop: serve one plugin's kernel requests until EOF.
pub(super) fn spawn_client_loop(
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
    plans: Arc<crate::plans::PlanService>,
    name: String,
    holding: HoldingHandle,
    mut stream: UnixStream,
) {
    std::thread::spawn(move || {
        loop {
            let req = match frame::read_frame(&mut stream) {
                Ok(r) => r,
                Err(_) => {
                    // The plugin went away (exit or crash): crash-only
                    // reclamation of everything it held. A graceful
                    // shutdown that raced us keeps its reason.
                    let reason = inner
                        .plugins
                        .lock()
                        .unwrap()
                        .get(&name)
                        .map(|h| {
                            if h.shutting_down.load(Ordering::SeqCst) {
                                "shutdown"
                            } else {
                                "exited"
                            }
                        })
                        .unwrap_or("exited");
                    reclaim_holding(&kernel, &inner, &name, &holding, reason);
                    return;
                }
            };
            count_context(&inner, &req);
            let resp = handle_client_op(&kernel, &inner, &plans, &name, &req, &mut stream);
            let resp = match resp {
                Ok(v) => v,
                Err(e) => json!({"err": e.to_string()}),
            };
            count_context(&inner, &resp);
            // `read` writes its own response before streaming chunks.
            if !resp.is_null() && frame::write_frame(&mut stream, &resp).is_err() {
                return;
            }
        }
    });
}

pub(super) fn count_context(inner: &Arc<HostInner>, v: &Value) {
    if v.is_null() {
        return;
    }
    let n = serde_json::to_vec(v).map(|b| b.len() as u64).unwrap_or(0);
    inner.meter.lock().unwrap().count_context(n);
}

pub(super) fn handle_client_op(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    plans: &Arc<crate::plans::PlanService>,
    name: &str,
    req: &Value,
    stream: &mut UnixStream,
) -> Result<Value, KernelError> {
    let now = crate::db::now_unix();
    let caller = inner
        .plugins
        .lock()
        .unwrap()
        .get(name)
        .map(|p| p.holding.clone());
    if let Some(caller) = caller {
        if !matches!(req["op"].as_str(), Some("release" | "unsubscribe"))
            && !kernel
                .ledger
                .holding(caller.id())?
                .is_some_and(|h| h.state.is_active() && h.handle() == caller)
        {
            return Err(KernelError::Denied("plugin is retiring".into()));
        }
    }
    match req["op"].as_str() {
        // ---- invoke: the capability-gated plugin→plugin path ----
        Some("invoke") => {
            let verb = req["verb"].as_str().unwrap_or("");
            let args = req.get("args").cloned().unwrap_or(Value::Null);
            let family = verb.split("::").next().unwrap_or(verb);
            let short = verb.rsplit("::").next().unwrap_or(verb);
            let subject = format!("plugin:{name}");
            let resource = format!("driver:{family}");
            // F5 effect row: the slot's position ceiling comes before the
            // subject's grants (actual = row ∩ grant) — and before any
            // budget is spent.
            let in_row = inner
                .plugins
                .lock()
                .unwrap()
                .get(name)
                .map(|h| {
                    h.offers
                        .as_ref()
                        .map(|o| o.0.contains(verb))
                        .unwrap_or(true)
                })
                .unwrap_or(true);
            if !in_row {
                let _ = kernel.audit.lock().unwrap().append(json!({
                    "event": "invoke.denied", "from": name, "verb": verb,
                    "reason": "verb outside the slot row",
                }));
                return Err(KernelError::Denied(format!(
                    "verb outside the slot row: {verb}"
                )));
            }
            let cap = match kernel
                .caps
                .find_and_exercise(&subject, &resource, short, now)
            {
                Ok(id) => id,
                Err(e) => {
                    let _ = kernel.audit.lock().unwrap().append(json!({
                        "event": "invoke.denied", "from": name, "verb": verb,
                        "reason": e.to_string(),
                    }));
                    return Err(e);
                }
            };
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "invoke.allowed", "from": name, "verb": verb, "cap": cap,
            }));
            let handle = {
                let target = inner
                    .routes
                    .lock()
                    .unwrap()
                    .get(verb)
                    .map(|e| e.plugin.clone())
                    .ok_or_else(|| KernelError::NotFound(format!("no route for verb: {verb}")))?;
                inner
                    .plugins
                    .lock()
                    .unwrap()
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| KernelError::NotFound(format!("plugin gone: {target}")))?
            };
            if !kernel
                .ledger
                .holding(handle.holding.id())?
                .is_some_and(|h| h.state.is_active() && h.handle() == handle.holding)
            {
                return Err(KernelError::Denied("target plugin is retiring".into()));
            }
            let out = call_on(&handle, verb, args)?;
            Ok(json!({"ok": out}))
        }

        // ---- grants introspection: what may *this* plugin invoke? ----
        // Joins the caller's live capabilities with the route table's
        // advertised verb metadata: the driver owns descriptions/schemas,
        // the user owns grants, the caller (e.g. the model driver) gets
        // ready-made tool definitions. The kernel joins strings; it never
        // interprets them.
        Some("grants") => {
            let subject = format!("plugin:{name}");
            let caps = kernel.caps.list_live(&subject, now)?;
            // Pass 1: merge per verb. Each entry tracks the strongest single
            // contributing grant: unlimited counts beat counted (then the
            // larger balance wins); no-expiry beats expiring (then the later
            // one wins). The winner's holding id is reported (WP-03).
            struct Best {
                counts: Option<u64>,
                expires_at: Option<u64>,
                holding: u64,
            }
            let key = |counts: Option<u64>, expires: Option<u64>| {
                (counts.unwrap_or(u64::MAX), expires.unwrap_or(u64::MAX))
            };
            let mut merged: BTreeMap<String, Best> = BTreeMap::new();
            for cap in &caps {
                let Some(family) = cap.resource.strip_prefix("driver:") else {
                    continue;
                };
                let Some(holding) = kernel
                    .ledger
                    .cap_holding(&AccountId::new(&cap.cap_id))?
                    .map(|h| h.id.get())
                else {
                    continue;
                };
                for short in &cap.verbs {
                    let verb = format!("{family}::{short}");
                    // Balance = capacity − fold of spend rows (F1), a snapshot.
                    let this = kernel.caps.counts_left(cap, short)?;
                    merged
                        .entry(verb)
                        .and_modify(|acc| {
                            if key(this, cap.constraints.expires_at)
                                > key(acc.counts, acc.expires_at)
                            {
                                *acc = Best {
                                    counts: this,
                                    expires_at: cap.constraints.expires_at,
                                    holding,
                                };
                            }
                        })
                        .or_insert(Best {
                            counts: this,
                            expires_at: cap.constraints.expires_at,
                            holding,
                        });
                }
            }
            // Pass 2: join with the route table's advertised metadata.
            let routes = inner.routes.lock().unwrap();
            let grants: Vec<Value> = merged
                .into_iter()
                .map(|(verb, best)| {
                    let (description, schema, kind, budgeted) = routes
                        .get(&verb)
                        .map(|e| {
                            (
                                e.description.clone(),
                                e.schema.clone(),
                                e.character.as_ref().map(kind_label),
                                e.character.as_ref().map(CheckedVerb::bears_budget),
                            )
                        })
                        .unwrap_or_else(|| (String::new(), json!({"type": "object"}), None, None));
                    let mut g = json!({
                        "verb": verb, "description": description, "schema": schema,
                        "holding": best.holding,
                    });
                    if let Some(n) = best.counts {
                        g["counts_left"] = json!(n);
                    }
                    if let Some(t) = best.expires_at {
                        g["expires_at"] = json!(t);
                    }
                    // F4: the verb character the serving plugin declared.
                    if let Some(k) = kind {
                        g["kind"] = json!(k);
                    }
                    if let Some(b) = budgeted {
                        g["budgeted"] = json!(b);
                    }
                    g
                })
                .collect();
            Ok(json!({"ok": {"grants": grants}}))
        }

        // ---- event bus ----
        Some("emit") => {
            let topic = req["topic"].as_str().unwrap_or("");
            let data = req.get("data").cloned().unwrap_or(Value::Null);
            let n = dispatch_event(kernel, inner, topic, data);
            Ok(json!({"ok": {"delivered": n}}))
        }
        Some("subscribe") => {
            let topic = req["topic"].as_str().unwrap_or("").to_string();
            let id = inner
                .next_sub
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
                .map_err(|_| KernelError::Denied("subscription IDs exhausted".into()))?;
            // A standing inbound channel is a holding, child of the plugin's
            // own holding: teardown unsubscribes before it kills.
            let parent = inner
                .plugins
                .lock()
                .unwrap()
                .get(name)
                .map(|h| h.holding.clone());
            let subject = format!("plugin:{name}");
            let mut subs = inner.subs.lock().unwrap();
            let holding = kernel.ledger.hold_managed(
                ExclusiveRequest {
                    owner: SubjectId::new(&subject),
                    resource: ResourceKey::new(
                        ClassId::new(CLASS_SUBSCRIPTION),
                        InstanceId::new(format!("{}/{}", inner.witness.session(), id)),
                    ),
                    generation: inner.witness.session().clone(),
                    parent: parent,
                    lease: LeaseRequest::UseClassDefault,
                },
                CleanupTarget::Subscription {
                    host: inner.witness.clone(),
                    subscription: id,
                },
                None,
                Timestamp::try_from(now).map_err(crate::ledger::map_err)?,
            )?;
            subs.push(Sub {
                id,
                topic: topic.clone(),
                target: SubTarget::Plugin(name.to_string()),
                holding: holding.clone(),
            });
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "events.subscribed", "plugin": name, "topic": topic, "sub": id,
                "holding": holding.id().get(),
            }));
            Ok(json!({"ok": {"sub": id}}))
        }
        Some("unsubscribe") => {
            // A plugin can only drop its own subscriptions. Find under the
            // subs lock, release in the ledger without it (lock order), then
            // remove.
            let id = req["sub"].as_u64().unwrap_or(0);
            let holding = {
                let subs = inner.subs.lock().unwrap();
                subs.iter()
                    .find(|s| s.id == id && matches!(&s.target, SubTarget::Plugin(n) if n == name))
                    .map(|s| s.holding.clone())
            };
            let removed = match holding {
                Some(h) => {
                    let report = kernel.ledger.release_with_world(
                        &h,
                        &mut HostWorld {
                            inner: inner.clone(),
                        },
                        Timestamp::try_from(now).expect("system timestamp in range"),
                    )?;
                    report.pending.is_empty()
                }
                None => false,
            };
            Ok(json!({"ok": {"removed": removed}}))
        }

        // ---- parent/child instantiation (WP-04) ----
        Some("spawn_child") => {
            // Capability gate: the caller must hold `kernel:spawn` /
            // `spawn_child`. The child's `kernel/plugin` holding becomes a
            // child of the caller's holding, so reclaiming the caller tears
            // the child down first (F2 closure).
            let subject = format!("plugin:{name}");
            let bin = req["bin"].as_str().unwrap_or("").to_string();
            if let Err(e) =
                kernel
                    .caps
                    .find_and_exercise(&subject, "kernel:spawn", "spawn_child", now)
            {
                let _ = kernel.audit.lock().unwrap().append(json!({
                    "event": "spawn_child.denied", "from": name, "bin": bin,
                    "reason": e.to_string(),
                }));
                return Err(e);
            }
            let parent_holding = inner
                .plugins
                .lock()
                .unwrap()
                .get(name)
                .map(|h| h.holding.clone())
                .ok_or_else(|| KernelError::NotFound(format!("parent plugin gone: {name}")))?;
            let args: Vec<String> = str_array(&req["args"]);
            let envs: Vec<(String, String)> = req["env"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let env_refs: Vec<(&str, &str)> =
                envs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            if bin.is_empty() {
                return Err(KernelError::Corrupt("spawn_child: missing bin".into()));
            }
            let child = spawn_plugin(
                kernel,
                inner,
                plans,
                Path::new(&bin),
                &arg_refs,
                &env_refs,
                None,
                Some((name, parent_holding)),
            )?;
            Ok(json!({"ok": {"name": child}}))
        }

        // ---- substrate holdings (WP-02): plugin-registered child processes,
        // ports, lock files — reclaimed with the plugin, children first;
        // leased ones expire via the sweeper.
        Some("hold") => op_hold(kernel, inner, name, req, now),
        Some("release") => op_release(kernel, inner, name, req, now),
        Some("renew") => op_renew(kernel, name, req, now),

        // ---- plan submission (WP-06): admit only — the model proposes,
        // never executes and never approves. Consent is the person's, given
        // through the CLI.
        Some("plan.submit") => {
            let subject = format!("plugin:{name}");
            let plan_val = req.get("plan").cloned().unwrap_or(Value::Null);
            let bytes = serde_json::to_vec(&plan_val)
                .map_err(|e| KernelError::Corrupt(format!("plan.submit encode: {e}")))?;
            match plans.submit(&subject, &bytes) {
                Ok(out) => {
                    // Put the plan where the CLI can sign it.
                    let dir = kernel.root.join("submitted-plans");
                    let fname = out.plan_hash.replace(':', "_");
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(dir.join(format!("{fname}.json")), &bytes);
                    Ok(json!({"ok": {
                        "run_id": out.run_id,
                        "plan_hash": out.plan_hash,
                        "rendering": out.rendering,
                        "budget": out.budget,
                        "needs": "consent",
                        "plan_path": dir.join(format!("{fname}.json")),
                    }}))
                }
                Err(e) => Err(e),
            }
        }

        // ---- artifact dereference: put (frame, then chunk stream) ----
        Some("put") => {
            let ty = req["type"].as_str().unwrap_or("application/octet-stream");
            let labels: Label = req
                .get("labels")
                .cloned()
                .map(|v| serde_json::from_value(v).unwrap_or_default())
                .unwrap_or_default();
            let origin = format!("plugin:{name}");
            let mut reader = chunk::ChunkReader::new(&mut *stream);
            let result = kernel.cas.put_stream(&mut reader, ty, labels, &origin);
            // Resync the stream even on a CAS error so the channel survives.
            reader
                .drain()
                .map_err(|e| KernelError::Corrupt(format!("put drain: {e}")))?;
            let meta = result?;
            inner.meter.lock().unwrap().count_data(meta.size);
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "artifact.put", "plugin": name, "id": meta.id, "size": meta.size,
            }));
            Ok(json!({"ok": {"meta": meta}}))
        }

        // ---- artifact dereference: read (response frame, then chunks) ----
        Some("read") => {
            let id = req["id"].as_str().unwrap_or("").to_string();
            let offset = req["offset"].as_u64().unwrap_or(0);
            let want = req["len"].as_u64();
            // Reads are free but accounted (读取记账): audit before bytes move.
            let meta = kernel.cas.meta(&id)?;
            let avail = meta.size.saturating_sub(offset);
            let n = want.map(|w| w.min(avail)).unwrap_or(avail);
            let mut f = kernel.cas.open_read(&id)?;
            f.seek(SeekFrom::Start(offset))?;
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "artifact.read", "plugin": name, "id": id,
                "offset": offset, "len": n,
            }));
            let head = json!({"ok": {"len": n}});
            count_context(inner, &head);
            frame::write_frame(stream, &head)
                .map_err(|e| KernelError::Corrupt(format!("read resp: {e}")))?;
            let mut taken = f.take(n);
            let moved = chunk::copy_into_chunks(&mut taken, stream)
                .map_err(|e| KernelError::Corrupt(format!("read stream: {e}")))?;
            inner.meter.lock().unwrap().count_data(moved);
            Ok(Value::Null) // response already sent
        }

        other => Err(KernelError::Corrupt(format!(
            "unknown client op: {other:?}"
        ))),
    }
}
