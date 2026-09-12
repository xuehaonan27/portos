//! modeld's implementation of the `router` driver: where a tool call goes.
//!
//! The model asks for a tool by name and something has to decide who answers
//! it. Almost always that is "out through the kernel to whoever serves it",
//! but not always — `artifact::read` is answered here, because dereferencing
//! a handle is this driver reading on the model's behalf and no plugin serves
//! that verb.
//!
//! It used to be an `if` in front of the invoke path, and that `if` was
//! stitching together two tables that were built separately: the tool list
//! the model is shown, and the decision about where each call goes. They
//! could disagree — a tool listed and not routable, or routable and not
//! listed — and nothing would have said so. Here they are one table, so the
//! question cannot come up.
//!
//! One consequence worth having: a miss is now an error here rather than a
//! call the kernel refuses. A family excluded from the tool surface used to
//! be merely unlisted — still callable, because this driver held the
//! capability — and now it is genuinely not offered.

use crate::core::ToolDef;
use portos_abi::ids::Verb;
use portos_abi::wire::{Payload, ToolMeta};
use portos_router::{Conflict, Resolved, Router};
use std::collections::BTreeMap;

/// Who answers a tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// This driver, on the model's behalf.
    Here(Local),
    /// Out through the kernel, to whoever serves it.
    Kernel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Local {
    ReadArtifact,
}

struct Entry {
    answer: Answer,
    meta: ToolMeta,
}

#[derive(Default)]
pub struct ToolTable {
    routes: BTreeMap<Verb, Entry>,
}

impl Router for ToolTable {
    type Target = Answer;

    fn resolve(&self, verb: &Verb) -> Option<Resolved<'_, Answer>> {
        self.routes.get(verb).map(|e| Resolved {
            target: &e.answer,
            name: verb.clone(),
        })
    }

    fn add(&mut self, verb: Verb, target: Answer, meta: ToolMeta) -> Result<(), Conflict> {
        if self.routes.contains_key(&verb) {
            return Err(Conflict::Verb(verb));
        }
        self.routes.insert(
            verb,
            Entry {
                answer: target,
                meta,
            },
        );
        Ok(())
    }

    fn meta(&self, verb: &Verb) -> Option<&ToolMeta> {
        self.routes.get(verb).map(|e| &e.meta)
    }

    fn remove_where(&mut self, f: &dyn Fn(&Verb, &Answer) -> bool) -> usize {
        let before = self.routes.len();
        self.routes.retain(|v, e| !f(v, &e.answer));
        before - self.routes.len()
    }

    fn verbs(&self) -> Vec<Verb> {
        self.routes.keys().cloned().collect()
    }
}

impl ToolTable {
    /// Replace an entry. Assembly overrides deliberately — config beats
    /// introspection — so it is not a conflict, and saying so here keeps
    /// `add`'s law about claiming a name intact.
    fn put(&mut self, verb: Verb, answer: Answer, meta: ToolMeta) {
        self.routes.insert(verb, Entry { answer, meta });
    }

    /// What the model is shown. Derived from the table rather than built
    /// beside it, which is the whole point.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.routes
            .iter()
            .map(|(verb, e)| ToolDef {
                verb: verb.clone(),
                description: e.meta.description.clone(),
                schema: e.meta.schema.clone().unwrap_or_else(|| {
                    Payload::of(&serde_json::json!({"type": "object"})).unwrap()
                }),
            })
            .collect()
    }
}

/// Build the table for one turn: what this driver may invoke right now,
/// whatever config adds or overrides, and the one verb it answers itself.
pub fn assemble(
    grants: Vec<portos_abi::wire::Grant>,
    exclude: &[String],
    config_tools: &[ToolDef],
) -> ToolTable {
    let mut table = ToolTable::default();
    for g in grants {
        if exclude.iter().any(|e| e == g.verb.family()) {
            continue;
        }
        table.put(
            g.verb,
            Answer::Kernel,
            ToolMeta {
                description: g.description,
                schema: Some(g.schema),
            },
        );
    }
    for t in config_tools {
        table.put(
            t.verb.clone(),
            Answer::Kernel,
            ToolMeta {
                description: t.description.clone(),
                schema: Some(t.schema.clone()),
            },
        );
    }
    if table.meta(&crate::ARTIFACT_READ).is_none() {
        table.put(
            crate::ARTIFACT_READ.clone(),
            Answer::Here(Local::ReadArtifact),
            crate::artifact_read_meta(),
        );
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use portos_abi::wire::Grant;

    fn grant(verb: &str) -> Grant {
        Grant {
            verb: Verb::parse(verb).unwrap(),
            description: "d".into(),
            schema: Payload::of(&serde_json::json!({})).unwrap(),
            counts_left: None,
        }
    }

    /// One table decides both what the model is shown and where each call
    /// goes, so the two can no longer disagree — and an excluded family is
    /// genuinely not offered rather than merely left off a list.
    #[test]
    fn the_table_is_both_the_tool_list_and_the_routing_decision() {
        let table = assemble(
            vec![grant("browser::open"), grant("egress::http")],
            &["egress".to_string()],
            &[],
        );

        let listed: Vec<String> = table
            .tool_defs()
            .iter()
            .map(|t| t.verb.to_string())
            .collect();
        assert!(listed.contains(&"browser::open".to_string()));
        assert!(listed.contains(&"artifact::read".to_string()), "{listed:?}");
        assert!(
            !listed.contains(&"egress::http".to_string()),
            "excluded: {listed:?}"
        );

        let open = Verb::parse("browser::open").unwrap();
        assert_eq!(
            table.resolve(&open).map(|r| *r.target),
            Some(Answer::Kernel)
        );

        // Answered here, because no plugin serves it.
        assert_eq!(
            table.resolve(&crate::ARTIFACT_READ).map(|r| *r.target),
            Some(Answer::Here(Local::ReadArtifact))
        );

        // The half that used to leak: excluded meant unlisted, not
        // unreachable — this driver holds the capability, so the call would
        // have gone through if the model asked for it anyway.
        let egress = Verb::parse("egress::http").unwrap();
        assert!(
            table.resolve(&egress).is_none(),
            "a miss is an error, not a fall-through to the kernel"
        );
    }
}
