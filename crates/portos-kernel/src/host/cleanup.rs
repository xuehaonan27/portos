//! Resolve exact host incarnations and execute committed cleanup work.

use super::HostInner;
use crate::{
    Kernel,
    ledger::{capture_process, cleanup_process, execute_target},
};
use portos_proto::frame;
use portos_rm::cleanup::{
    CleanupExecutor, CleanupOutcome, CleanupTarget, CleanupWork, HostWitness,
};
use portos_rm::identity::HoldingHandle;
use portos_rm::time::Timestamp;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

// Weak registration lets recovery find an existing host by its exact session,
// including during retry from another Host in the same kernel process.
static HOSTS: std::sync::OnceLock<Mutex<BTreeMap<String, std::sync::Weak<HostInner>>>> =
    std::sync::OnceLock::new();
pub(super) fn host_registry() -> &'static Mutex<BTreeMap<String, std::sync::Weak<HostInner>>> {
    HOSTS.get_or_init(Mutex::default)
}
fn known_host(w: &HostWitness) -> Option<Arc<HostInner>> {
    host_registry()
        .lock()
        .unwrap()
        .get(w.session().as_str())
        .and_then(std::sync::Weak::upgrade)
        .filter(|h| &h.witness == w)
}
pub(crate) fn local_host_gone(w: &HostWitness) -> bool {
    capture_process(std::process::id()).is_ok_and(|p| &p == w.process()) && known_host(w).is_none()
}
pub(crate) fn cleanup_known_host(work: &CleanupWork) -> Option<CleanupOutcome> {
    let host = match work.target() {
        CleanupTarget::Subscription { host, .. } | CleanupTarget::Plugin { host, .. } => host,
        _ => return None,
    };
    known_host(host).map(|inner| HostWorld { inner }.execute(work))
}

/// The host cleanup executor: physical cleanup of the kernel's
/// built-in holdings. Called after a durable claim, without storage locks.
pub(super) struct HostWorld {
    pub(super) inner: Arc<HostInner>,
}

impl CleanupExecutor for HostWorld {
    fn execute(&mut self, work: &CleanupWork) -> CleanupOutcome {
        match work.target() {
            CleanupTarget::Subscription { host, subscription } if host == &self.inner.witness => {
                let mut subs = self.inner.subs.lock().unwrap();
                let before = subs.len();
                subs.retain(|s| !(s.id == *subscription && s.holding == work.task().holding));
                if subs.len() != before {
                    CleanupOutcome::Confirmed
                } else {
                    CleanupOutcome::AlreadyAbsent
                }
            }
            CleanupTarget::Plugin { host, process } if host == &self.inner.witness => {
                let name = work.resource().instance().as_str();
                let mut plugins = self.inner.plugins.lock().unwrap();
                let handle = plugins
                    .get(name)
                    .filter(|h| h.holding == work.task().holding)
                    .cloned();
                if let Some(h) = &handle {
                    if h.shutting_down.load(Ordering::SeqCst) {
                        if let Ok(mut stream) = h.serve.try_lock() {
                            let previous = stream.write_timeout().ok().flatten();
                            if stream
                                .set_write_timeout(Some(std::time::Duration::from_millis(50)))
                                .is_ok()
                            {
                                let _ = frame::write_frame(&mut *stream, &json!({"op":"shutdown"}));
                                let _ = stream.set_write_timeout(previous);
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
                let result = cleanup_process(process);
                if matches!(
                    result,
                    CleanupOutcome::Confirmed | CleanupOutcome::AlreadyAbsent
                ) {
                    if let Some(h) = handle {
                        if let Err(e) = h.child.lock().unwrap().try_wait() {
                            return CleanupOutcome::Retryable(e.to_string());
                        }
                        if let Err(e) = std::fs::remove_file(&h.sock_path) {
                            if e.kind() != std::io::ErrorKind::NotFound {
                                return CleanupOutcome::Retryable(e.to_string());
                            }
                        }
                        // Keep the name locked until its old routes are removed.
                        cleanup_plugin(&self.inner, name);
                        plugins.remove(name);
                    }
                }
                result
            }
            _ => execute_target(work),
        }
    }
}

/// The one teardown path (crash-only): release the plugin's ownership
/// closure children first through `HostWorld`, then audit. Idempotent — a
/// second call for the same plugin finds nothing to release.
pub(super) fn reclaim(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    name: &str,
    reason: &str,
) -> usize {
    let holding = inner
        .plugins
        .lock()
        .unwrap()
        .get(name)
        .map(|h| h.holding.clone());
    holding
        .map(|h| reclaim_holding(kernel, inner, name, &h, reason))
        .unwrap_or(0)
}
pub(super) fn reclaim_holding(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    name: &str,
    holding: &HoldingHandle,
    reason: &str,
) -> usize {
    let now = crate::db::now_unix();
    let mut world = HostWorld {
        inner: inner.clone(),
    };
    let report = match kernel.ledger.teardown_owner_incarnation(
        holding,
        &mut world,
        Timestamp::try_from(now).expect("system timestamp in range"),
    ) {
        Ok(x) => x,
        Err(e) => {
            let _ = kernel.audit.lock().unwrap().append(json!({
                "event": "plugin.reclaim_error", "plugin": name, "reason": reason,
                "error": e.to_string(),
            }));
            crate::ledger::CleanupReport::default()
        }
    };
    let released = report.completed.len();
    let pending = report
        .pending
        .iter()
        .map(|t| t.holding.id().get())
        .collect::<Vec<_>>();
    if released > 0 || !pending.is_empty() {
        let _ = kernel.audit.lock().unwrap().append(json!({
            "event": "plugin.reclaimed", "plugin": name, "reason": reason,
            "released": released, "pending": pending,
        }));
    }
    released
}

fn cleanup_plugin(inner: &Arc<HostInner>, name: &str) {
    inner
        .routes
        .lock()
        .unwrap()
        .retain(|_, entry| entry.plugin != name);
}
