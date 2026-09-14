//! Stopping a plugin: the escalation that makes it actually go, and the
//! collection of everything the kernel was holding for it.

use super::ready::settle;
use super::{HostInner, SubTarget};
use crate::Kernel;
use crate::routes::Answerer;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use portos_abi::frame;
use portos_abi::ids::PluginName;
use portos_abi::wire::ServeMsg;
use portos_router::Router as _;
use serde_json::json;
use std::sync::Arc;

/// How long each teardown step waits before escalating.
const GRACE_MS: u64 = 1_000;
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
pub(super) fn shutdown_on(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    plugin: &PluginName,
) -> bool {
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
pub(super) fn cleanup_plugin(inner: &Arc<HostInner>, name: &PluginName) {
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
