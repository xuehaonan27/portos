//! Decode resource requests and perform controlled holding transitions.

use super::HostInner;
use super::cleanup::HostWorld;
use super::lifecycle::rand_token;
use crate::ledger::{CLASS_FILE_LOCK, CLASS_PROCESS, ExclusiveRequest, proc_start_time};
use crate::{Kernel, KernelError};
use portos_proto::resource::{
    HoldRequest, HoldingRef, ReleaseRequest, ReleaseResponse, RenewRequest, RenewResponse,
};
use portos_rm::cleanup::*;
use portos_rm::identity::{
    ClassId, Generation, HoldingHandle, HoldingId, InstanceId, ResourceKey, SubjectId,
};
use portos_rm::time::{LeaseDuration, LeaseRequest, Timestamp};
use serde_json::{Value, json};
use std::sync::Arc;

// ---- substrate holdings (WP-02) ----

/// `hold {class, instance, substrate, lease_secs?}` → `{id, generation}`.
/// Restricted to the holdable built-in classes; the holding's parent is the
/// caller's `kernel/plugin` holding, so plugin death reclaims it children
/// first. For `kernel/process` the kernel derives the generation itself —
/// `<pid>:<start time>` witnesses the exact incarnation.
pub(super) fn op_hold(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    name: &str,
    req: &Value,
    now: u64,
) -> Result<Value, KernelError> {
    let request: HoldRequest = serde_json::from_value(req.clone())
        .map_err(|e| KernelError::Denied(format!("hold request: {e}")))?;
    let class = request.class.as_str();
    let instance = request.instance;
    let mut substrate = request.substrate;
    let lease = request
        .lease_secs
        .map(LeaseDuration::try_from)
        .transpose()
        .map_err(crate::ledger::map_err)?
        .map(LeaseRequest::For)
        .unwrap_or_default();
    let now = Timestamp::try_from(now).map_err(crate::ledger::map_err)?;
    if instance.is_empty() {
        return Err(KernelError::Denied("hold: instance required".into()));
    }
    let positive_pid = |v: &Value| {
        v.as_u64()
            .and_then(|n| i32::try_from(n).ok())
            .filter(|p| *p > 0)
            .map(|p| p as u32)
            .ok_or_else(|| {
                KernelError::Denied("substrate pid must be a positive process ID".into())
            })
    };
    let generation = match class {
        CLASS_PROCESS => {
            let pid = positive_pid(&substrate["pid"])?;
            let start = proc_start_time(pid)
                .ok_or_else(|| KernelError::Denied("hold: process not found".into()))?;
            substrate = json!({"pid":pid,"start":start});
            Generation::new(format!("{pid}:{start}"))
        }
        CLASS_FILE_LOCK => {
            if let Some(raw) = substrate.get("owner_pid") {
                let pid = positive_pid(raw)?;
                if let Some(start) = proc_start_time(pid) {
                    substrate["owner_start"] = json!(start);
                }
            }
            Generation::new(rand_token())
        }
        _ => Generation::new(rand_token()),
    };
    let parent = inner
        .plugins
        .lock()
        .unwrap()
        .get(name)
        .map(|h| h.holding.clone());
    let handle = kernel.ledger.hold_substrate(ExclusiveRequest {
        owner: SubjectId::new(format!("plugin:{name}")), resource: ResourceKey::new(ClassId::new(class), InstanceId::new(&instance)),
        generation, parent, lease,
    }, &substrate, now).map_err(|e| {
        let _ = kernel.audit.lock().unwrap().append(json!({"event":"hold.denied","from":name,"class":class,"instance":instance,"reason":e.to_string()}));
        e
    })?;
    let reference = HoldingRef {
        id: handle.id().get(),
        generation: handle.generation().to_string(),
    };
    let _ = kernel.audit.lock().unwrap().append(json!({"event":"substrate.held","plugin":name,"class":class,"instance":instance,"holding":reference.id,"generation":reference.generation,"lease_secs":request.lease_secs}));
    Ok(json!({"ok":reference}))
}

fn domain_handle(reference: &HoldingRef) -> Result<HoldingHandle, KernelError> {
    if reference.generation.is_empty() {
        return Err(KernelError::Denied("empty generation".into()));
    }
    Ok(HoldingHandle::new(
        HoldingId::try_from(reference.id).map_err(crate::ledger::map_err)?,
        Generation::new(&reference.generation),
    ))
}

pub(super) fn op_release(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    name: &str,
    req: &Value,
    now: u64,
) -> Result<Value, KernelError> {
    let request: ReleaseRequest = serde_json::from_value(req.clone())
        .map_err(|e| KernelError::Denied(format!("release request: {e}")))?;
    let handle = domain_handle(&request.holding)?;
    let now = Timestamp::try_from(now).map_err(crate::ledger::map_err)?;
    let subject = SubjectId::new(format!("plugin:{name}"));
    kernel.ledger.transaction(|tx| {
        let h = tx
            .holding(handle.id())
            .ok_or_else(|| KernelError::Denied("release: unknown holding".into()))?;
        if h.subject != subject {
            return Err(KernelError::Denied("release: not your holding".into()));
        }
        tx.request_retirement(&handle, now)
    })?;
    let report = kernel.ledger.release_with_world(
        &handle,
        &mut HostWorld {
            inner: inner.clone(),
        },
        now,
    )?;
    let response = match report.pending.first() {
        None => ReleaseResponse::Released,
        Some(t) => {
            let id = t.id.get();
            match &t.state {
                CleanupState::Retryable(reason) => ReleaseResponse::Retryable {
                    cleanup_id: id,
                    reason: reason.clone(),
                },
                CleanupState::Unknown(reason) => ReleaseResponse::Unknown {
                    cleanup_id: id,
                    reason: reason.clone(),
                },
                CleanupState::Blocked(reason) => ReleaseResponse::Blocked {
                    cleanup_id: id,
                    reason: reason.clone(),
                },
                _ => ReleaseResponse::Pending { cleanup_id: id },
            }
        }
    };
    let _=kernel.audit.lock().unwrap().append(json!({"event":"substrate.release","plugin":name,"holding":handle.id().get(),"result":response}));
    Ok(json!({"ok":response}))
}

pub(super) fn op_renew(
    kernel: &Arc<Kernel>,
    name: &str,
    req: &Value,
    now: u64,
) -> Result<Value, KernelError> {
    let request: RenewRequest = serde_json::from_value(req.clone())
        .map_err(|e| KernelError::Denied(format!("renew request: {e}")))?;
    let handle = domain_handle(&request.holding)?;
    let now = Timestamp::try_from(now).map_err(crate::ledger::map_err)?;
    let lease = request
        .lease_secs
        .map(LeaseDuration::try_from)
        .transpose()
        .map_err(crate::ledger::map_err)?
        .map(LeaseRequest::For)
        .unwrap_or_default();
    let expires = kernel
        .ledger
        .transaction(|tx| {
            let h = tx
                .holding(handle.id())
                .ok_or_else(|| KernelError::Denied("renew: unknown holding".into()))?;
            if h.subject.as_str() != format!("plugin:{name}") {
                return Err(KernelError::Denied("renew: not your holding".into()));
            }
            tx.renew(&handle, lease, now)
        })?
        .expires_at()
        .map(Timestamp::get);
    let _ = kernel.audit.lock().unwrap().append(json!({"event":"substrate.renewed","plugin":name,"holding":handle.id().get(),"lease_expires_at":expires}));
    Ok(json!({"ok":RenewResponse { lease_expires_at:expires }}))
}
