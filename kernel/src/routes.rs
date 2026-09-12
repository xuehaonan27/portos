//! The kernel's implementation of the `router` driver.
//!
//! Its targets are the two things that can answer a verb on this node: a
//! plugin process, or the kernel itself. Nothing else — reaching another node
//! is reaching the transport plugin that faces it, which is a `Plugin` like
//! any other.
//!
//! What this removed, and why it matters more than the code: the kernel used
//! to check `verb.family() == "kernel"` *before* looking at the table, and
//! `grants` used to look in a separate `BUILTIN_TOOLS` map before looking at
//! the table. Two special cases for one idea — "the kernel answers some
//! verbs itself". As rows they are not special at all, and the reservation
//! comes for free: the kernel registers first, so a plugin claiming
//! `kernel::spawn` gets the ordinary conflict every other double claim gets.

use portos_abi::ids::{PluginName, Verb};
use portos_abi::wire::ToolMeta;
use portos_router::{Conflict, Resolved, Router};
use std::collections::BTreeMap;

/// Who answers a verb here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answerer {
    /// A plugin process on this node.
    Plugin(PluginName),
    /// The kernel, which is an answerer like any other and not a case before
    /// the answering starts.
    Builtin(Builtin),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Spawn,
    Stop,
    Plugins,
}

impl Answerer {
    /// Whether two targets name the same answerer.
    ///
    /// Not the same as being equal: `kernel::spawn` and `kernel::stop` are
    /// different targets and the same answerer — the kernel. Only an
    /// implementation can know this, because only it knows what its targets
    /// name, which is why the interface states the law and leaves this here.
    pub(crate) fn same_as(&self, other: &Answerer) -> bool {
        match (self, other) {
            (Answerer::Plugin(a), Answerer::Plugin(b)) => a == b,
            (Answerer::Builtin(_), Answerer::Builtin(_)) => true,
            _ => false,
        }
    }
}

struct Entry {
    answerer: Answerer,
    meta: ToolMeta,
}

#[derive(Default)]
pub struct RouteTable {
    routes: BTreeMap<Verb, Entry>,
    /// Names spoken for, including by plugins that are not usable yet.
    claims: BTreeMap<Verb, Answerer>,
}

impl Router for RouteTable {
    type Target = Answerer;

    fn resolve(&self, verb: &Verb) -> Option<Resolved<'_, Answerer>> {
        self.routes.get(verb).map(|e| Resolved {
            target: &e.answerer,
            // No route here renames anything yet. When one does — a verb
            // whose answerer is a transport, and the far side calls it
            // something else — this is where that name comes from, and
            // callers already use it rather than what they looked up.
            name: verb.clone(),
        })
    }

    fn add(&mut self, verb: Verb, target: Answerer, meta: ToolMeta) -> Result<(), Conflict> {
        if self.routes.contains_key(&verb) {
            return Err(Conflict::Verb(verb));
        }
        // Through `claim` rather than repeating its checks, so there is one
        // law site. A route that was never claimed would be a name the table
        // answers and nobody owns — which is exactly how the kernel's own
        // verbs came to be routed and claimable by somebody else at once.
        self.claim(std::slice::from_ref(&verb), &target)?;
        self.routes.insert(
            verb,
            Entry {
                answerer: target,
                meta,
            },
        );
        Ok(())
    }

    fn meta(&self, verb: &Verb) -> Option<&ToolMeta> {
        self.routes.get(verb).map(|e| &e.meta)
    }

    fn remove_where(&mut self, f: &dyn Fn(&Verb, &Answerer) -> bool) -> usize {
        let before = self.routes.len();
        self.routes.retain(|v, e| !f(v, &e.answerer));
        before - self.routes.len()
    }

    fn verbs(&self) -> Vec<Verb> {
        self.routes.keys().cloned().collect()
    }
}

impl RouteTable {
    /// Who has claimed this name, whether or not it currently resolves.
    ///
    /// A plugin waiting on a dependency is not usable but its names are
    /// still its own — otherwise a second plugin could take them while it
    /// waits, and the first would never get to become usable.
    pub fn claimed_by(&self, verb: &Verb) -> Option<&Answerer> {
        self.claims.get(verb)
    }

    /// Claim names without routing them yet.
    ///
    /// The laws are enforced *here*, against claims, because a claim is the
    /// authoritative "this name is spoken for" — a plugin that is waiting on
    /// a dependency has claimed its names and not yet been routed, and if
    /// the laws only looked at routes it could be refused later, at a point
    /// where there is nobody left to refuse it to.
    pub fn claim(&mut self, verbs: &[Verb], who: &Answerer) -> Result<(), Conflict> {
        for v in verbs {
            if let Some(other) = self.claims.get(v) {
                if !other.same_as(who) {
                    return Err(Conflict::Verb(v.clone()));
                }
            }
            if let Some(other) = self.claimant_of_family(v.family()) {
                if !other.same_as(who) {
                    return Err(Conflict::Family {
                        family: v.family().to_string(),
                        verb: v.clone(),
                    });
                }
            }
        }
        for v in verbs {
            self.claims.insert(v.clone(), who.clone());
        }
        Ok(())
    }

    fn claimant_of_family(&self, family: &str) -> Option<&Answerer> {
        self.claims
            .iter()
            .find(|(v, _)| v.family() == family)
            .map(|(_, a)| a)
    }

    /// Whether this answerer's names are currently answered.
    pub fn is_routed(&self, who: &Answerer) -> bool {
        self.routes.values().any(|e| e.answerer.same_as(who))
    }

    pub fn release(&mut self, who: &Answerer) {
        self.claims.retain(|_, a| !a.same_as(who));
    }

    /// Every verb answered by one plugin. Used to report what a spawn gained.
    pub fn verbs_of(&self, plugin: &PluginName) -> Vec<Verb> {
        self.routes
            .iter()
            .filter(|(_, e)| e.answerer == Answerer::Plugin(plugin.clone()))
            .map(|(v, _)| v.clone())
            .collect()
    }
}
