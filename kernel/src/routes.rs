//! The kernel's implementation of the `router` driver.
//!
//! Its targets are the two things that can answer a verb on this node: a
//! plugin process, or the kernel itself. Nothing else — reaching another node
//! is reaching the transport plugin that faces it, which is a `Plugin` like
//! any other.
//!
//! A verb may have several answerers: two browsers, or this node's and
//! another's. The table keeps them all and resolution picks one — the one
//! named, or the only one. The kernel is an answerer like any other, under
//! the name `kernel`: a plugin may answer `kernel::spawn` too (a launcher for
//! a form the kernel does not know), and then a caller names which.

use portos_abi::ids::{PluginName, Verb};
use portos_abi::wire::ToolMeta;
use portos_router::{Conflict, Miss, Resolved, Router};
use std::collections::BTreeMap;

/// The kernel's own instance name. Taken before any plugin exists, the way
/// any name is taken: by being there first.
pub const KERNEL: &str = "kernel";

/// Who answers a verb here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answerer {
    /// A plugin process on this node.
    Plugin(PluginName),
    /// The kernel, which is an answerer like any other and not a case before
    /// the answering starts.
    Kernel,
}

impl Answerer {
    /// The instance name a caller uses to pick this answerer.
    pub fn id(&self) -> PluginName {
        match self {
            Answerer::Plugin(name) => name.clone(),
            Answerer::Kernel => PluginName::parse(KERNEL).expect("constant name"),
        }
    }

    /// The answerer a caller means by an instance name.
    pub fn named(name: &PluginName) -> Answerer {
        if name.as_str() == KERNEL {
            Answerer::Kernel
        } else {
            Answerer::Plugin(name.clone())
        }
    }
}

struct Entry {
    answerer: Answerer,
    meta: ToolMeta,
}

#[derive(Default)]
pub struct RouteTable {
    routes: BTreeMap<Verb, Vec<Entry>>,
}

impl Router for RouteTable {
    type Target = Answerer;

    fn resolve(&self, verb: &Verb, at: Option<&Answerer>) -> Result<Resolved<'_, Answerer>, Miss> {
        let entries = self.routes.get(verb).ok_or(Miss::NoRoute)?;
        let entry = match at {
            Some(who) => entries
                .iter()
                .find(|e| &e.answerer == who)
                .ok_or(Miss::NoRoute)?,
            None => match entries.as_slice() {
                [only] => only,
                _ => return Err(Miss::Ambiguous),
            },
        };
        Ok(Resolved {
            target: &entry.answerer,
            // No route here renames anything yet. When one does — a verb
            // whose answerer is a transport, and the far side calls it
            // something else — this is where that name comes from, and
            // callers already use it rather than what they looked up.
            name: verb.clone(),
        })
    }

    fn answerers(&self, verb: &Verb) -> Vec<(&Answerer, &ToolMeta)> {
        self.routes
            .get(verb)
            .map(|entries| entries.iter().map(|e| (&e.answerer, &e.meta)).collect())
            .unwrap_or_default()
    }

    fn add(&mut self, verb: Verb, target: Answerer, meta: ToolMeta) -> Result<(), Conflict> {
        let entries = self.routes.entry(verb.clone()).or_default();
        if entries.iter().any(|e| e.answerer == target) {
            return Err(Conflict(verb));
        }
        entries.push(Entry {
            answerer: target,
            meta,
        });
        Ok(())
    }

    fn remove_where(&mut self, f: &dyn Fn(&Verb, &Answerer) -> bool) -> usize {
        let mut removed = 0;
        for (verb, entries) in self.routes.iter_mut() {
            let before = entries.len();
            entries.retain(|e| !f(verb, &e.answerer));
            removed += before - entries.len();
        }
        self.routes.retain(|_, entries| !entries.is_empty());
        removed
    }

    fn verbs(&self) -> Vec<Verb> {
        self.routes.keys().cloned().collect()
    }
}

impl RouteTable {
    /// Every verb answered by one plugin. Used to report what a spawn gained.
    pub fn verbs_of(&self, plugin: &PluginName) -> Vec<Verb> {
        let who = Answerer::Plugin(plugin.clone());
        self.routes
            .iter()
            .filter(|(_, entries)| entries.iter().any(|e| e.answerer == who))
            .map(|(v, _)| v.clone())
            .collect()
    }
}
