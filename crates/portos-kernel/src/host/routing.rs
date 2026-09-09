//! Resolve calls and enforce per-plugin protocol order.

use super::{HostInner, PluginHandle};
use crate::plans::EffectPolicy;
use crate::{Kernel, KernelError};
use portos_proto::{Label, frame};
use portos_rm::protocol::Protocol;
use portos_rm::verbs::{CheckedVerb, Kind};
use serde_json::{Value, json};
use std::sync::Arc;

/// A routed verb: which plugin serves it, plus the model-facing metadata the
/// plugin advertised in its hello (opaque to the kernel — stored and joined,
/// never interpreted), and the verb character it declared (checked by the
/// F4 truth table at spawn).
pub(super) struct RouteEntry {
    pub(super) plugin: String,
    pub(super) description: String,
    pub(super) schema: Value,
    pub(super) character: Option<CheckedVerb>,
    /// Sink-target extraction (WP-06): which arg is the target, and how to
    /// read it ("origin" normalizes to scheme://host[:port]).
    pub(super) target: Option<TargetSpec>,
}

pub(super) enum TargetSpec {
    Literal(String),
    Origin(String),
}

pub(super) struct ProtocolSession {
    protocol: Protocol,
    state: String,
}

impl ProtocolSession {
    pub(super) fn new(protocol: Protocol) -> Self {
        Self {
            state: protocol.initial().to_string(),
            protocol,
        }
    }
}

/// Verb families currently routed (the services present for F5 `deps`).
pub(super) fn routed_families(inner: &HostInner) -> Vec<String> {
    let routes = inner.routes.lock().unwrap();
    let mut fams: Vec<String> = routes
        .keys()
        .map(|v| v.split("::").next().unwrap_or(v).to_string())
        .collect();
    fams.sort();
    fams.dedup();
    fams
}

/// One serve-channel request/response under the channel lock. Holding the
/// lock across write+read is what keeps the channel unmultiplexed — and it
/// is also what makes the F6 protocol check below precise: calls to one
/// plugin are serialized, so the automaton steps in call order, advancing
/// only when the call succeeded.
pub(super) fn call_on(
    handle: &PluginHandle,
    verb: &str,
    args: Value,
) -> Result<Value, KernelError> {
    let mut s = handle.serve.lock().unwrap();
    let mut session = handle.protocol.lock().unwrap();
    let next_state = session
        .as_ref()
        .map(|s| {
            s.protocol.step(&s.state, verb).map_err(|v| {
                KernelError::Denied(format!(
                    "protocol violation: {} in state {}",
                    v.verb, v.state
                ))
            })
        })
        .transpose()?;
    frame::write_frame(&mut *s, &json!({"op": "call", "verb": verb, "args": args}))
        .map_err(|e| KernelError::Corrupt(format!("call write: {e}")))?;
    let resp =
        frame::read_frame(&mut *s).map_err(|e| KernelError::Corrupt(format!("call read: {e}")))?;
    if let Some(err) = resp.get("err").and_then(|e| e.as_str()) {
        return Err(KernelError::Denied(format!("plugin error: {err}")));
    }
    if let (Some(session), Some(next)) = (session.as_mut(), next_state) {
        session.state = next;
    }
    Ok(resp.get("ok").cloned().unwrap_or(Value::Null))
}

/// The fiber-side invoke (WP-06): a plan run (or another kernel-side
/// subject) calls a verb through the same capability gate, budget spend and
/// protocol step the plugin-facing `invoke` op uses.
pub(super) fn invoke_as(
    kernel: &Arc<Kernel>,
    inner: &Arc<HostInner>,
    subject: &str,
    verb: &str,
    args: Value,
) -> Result<Value, KernelError> {
    let now = crate::db::now_unix();
    let family = verb.split("::").next().unwrap_or(verb);
    let short = verb.rsplit("::").next().unwrap_or(verb);
    let resource = format!("driver:{family}");
    if let Err(e) = kernel
        .caps
        .find_and_exercise(subject, &resource, short, now)
    {
        let _ = kernel.audit.lock().unwrap().append(json!({
            "event": "invoke.denied", "from": subject, "verb": verb,
            "reason": e.to_string(),
        }));
        return Err(e);
    }
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
        return Err(KernelError::Denied("plugin is retiring".into()));
    }
    call_on(&handle, verb, args)
}

/// The kernel's verb-label schemas for plan admission and the sink rule,
/// derived from the route table's declared verb characters. v0: reads carry
/// no confidentiality (the taint plane lands with WP-09); emitting verbs are
/// external sinks.
pub(super) fn kernel_schemas(inner: &HostInner) -> crate::plancheck::VerbSchemas {
    let routes = inner.routes.lock().unwrap();
    let mut s = crate::plancheck::VerbSchemas::default();
    for (verb, e) in routes.iter() {
        match e.character.as_ref().map(CheckedVerb::kind) {
            Some(Kind::Repeatable) => {
                s.observe.insert(verb.clone(), Label::public_trusted());
            }
            Some(Kind::Emitting { .. }) => {
                s.external_effects.insert(verb.clone(), true);
            }
            _ => {}
        }
    }
    s
}

/// Resolve sink targeting and withholding from the same route snapshot.
pub(super) fn effect_policy(inner: &HostInner, verb: &str, args: &Value) -> EffectPolicy {
    let routes = inner.routes.lock().unwrap();
    let route = routes.get(verb);
    let target = match route.and_then(|r| r.target.as_ref()) {
        Some(TargetSpec::Literal(arg)) => args[arg].as_str().unwrap_or("*").to_string(),
        Some(TargetSpec::Origin(arg)) => {
            let raw = args[arg].as_str().unwrap_or("*");
            origin_of(raw).unwrap_or_else(|| raw.to_string())
        }
        None => "*".to_string(),
    };
    EffectPolicy {
        target,
        withhold: route
            .and_then(|r| r.character.as_ref())
            .is_some_and(CheckedVerb::withhold),
    }
}

fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let hostport = rest.split('/').next().unwrap_or(rest);
    if hostport.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{hostport}"))
}
