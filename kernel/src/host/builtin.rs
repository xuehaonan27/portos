//! The kernel's own verbs — spawn, stop, plugins — as rows in the route
//! table answered by the instance named `kernel`.

use super::HostInner;
use super::client::ok;
use super::launch::spawn_spec_on;
use super::ready::list_plugins;
use super::teardown::shutdown_on;
use crate::routes::{Answerer, RouteTable};
use crate::{Kernel, KernelError};
use portos_abi::ids::Verb;
use portos_abi::wire::{Payload, Reply};
use portos_kernel_api::{LaunchSpec, PluginsReply, SpawnReply, StopArgs, StopReply};
use portos_router::Router as _;
use std::sync::Arc;

/// The verbs the kernel answers itself. Reaching here means the capability
/// gate already passed, exactly as for a routed verb — the kernel is not a
/// special caller, it is a special *callee*.
/// The kernel answering one of its own verbs — reached through resolution
/// like any other answerer, so this is dispatch on a resolved target rather
/// than a case before dispatch begins.
pub(super) fn builtin_verb(
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
                .find(|p| p.name == name)
                .map(|p| (p.verbs, p.unmet))
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
            plugins: list_plugins(kernel, inner),
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
pub(super) fn reply_of(encoded: Option<Vec<u8>>) -> Result<Payload, KernelError> {
    let bytes = encoded.ok_or_else(|| KernelError::Corrupt("builtin wrote no reply".into()))?;
    let reply: Reply<Payload> = serde_json::from_slice(&bytes)
        .map_err(|e| KernelError::Corrupt(format!("builtin reply: {e}")))?;
    reply.into_result().map_err(KernelError::Denied)
}
/// The kernel's own verbs, as rows answered by the instance named `kernel`.
/// Nothing is reserved: a plugin may answer `kernel::spawn` as well — a
/// launcher for a form the kernel does not know — and callers then name
/// which of the two they mean. What each verb says about itself comes from
/// the interface, so a second implementation says the same thing.
pub(super) fn builtin_routes() -> RouteTable {
    let mut table = RouteTable::default();
    for (verb, meta) in portos_kernel_api::tools() {
        table
            .add(verb, Answerer::Kernel, meta)
            .expect("each kernel verb is registered once");
    }
    table
}
