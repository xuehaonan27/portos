//! Readiness: whether each plugin can do its job, reconciled against the
//! route table after anything changes it, and reported as what a plugin is
//! still waiting for.

use super::{HostInner, PluginHandle};
use crate::Kernel;
use crate::routes::Answerer;
use portos_abi::ids::{PluginName, Verb};
use portos_kernel_api::PluginInfo;
use portos_router::Router as _;
use std::sync::Arc;

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
pub(super) fn settle(kernel: &Arc<Kernel>, inner: &Arc<HostInner>) {
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
/// Which plugins are up, what each was started from, what each answers,
/// and what each is waiting for.
pub(super) fn list_plugins(kernel: &Arc<Kernel>, inner: &Arc<HostInner>) -> Vec<PluginInfo> {
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
            let unmet = unmet(kernel, inner, &handle, &name);
            PluginInfo {
                artifact: handle.artifact.clone(),
                bin: handle.bin.clone(),
                name,
                verbs,
                unmet,
            }
        })
        .collect()
}
