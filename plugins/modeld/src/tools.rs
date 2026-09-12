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
//! call the kernel refuses. A driver excluded from the tool surface used to
//! be merely unlisted — still callable, because this driver held the
//! capability — and now it is genuinely not offered.
//!
//! When more than one instance answers a verb — two browsers — the kernel
//! refuses to choose, so the model is asked to: the tool gains an `instance`
//! argument listing them, and the call names the one chosen. The argument
//! exists exactly when the choice does; a choice with one option is not a
//! choice, and would only cost context.

use crate::core::ToolDef;
use portos_abi::ids::{PluginName, Verb};
use portos_abi::wire::{Payload, ToolMeta};
use portos_router::{Conflict, Miss, Resolved, Router};
use std::collections::BTreeMap;

/// Who answers a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// This driver, on the model's behalf.
    Here(Local),
    /// Out through the kernel, to one of these instances — or to the only
    /// one, unnamed, when the list is shorter than two.
    Kernel { instances: Vec<PluginName> },
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

    fn resolve(&self, verb: &Verb, at: Option<&Answer>) -> Result<Resolved<'_, Answer>, Miss> {
        let e = self.routes.get(verb).ok_or(Miss::NoRoute)?;
        if at.is_some_and(|a| a != &e.answer) {
            return Err(Miss::NoRoute);
        }
        Ok(Resolved {
            target: &e.answer,
            name: verb.clone(),
        })
    }

    fn answerers(&self, verb: &Verb) -> Vec<(&Answer, &ToolMeta)> {
        self.routes
            .get(verb)
            .map(|e| vec![(&e.answer, &e.meta)])
            .unwrap_or_default()
    }

    fn add(&mut self, verb: Verb, target: Answer, meta: ToolMeta) -> Result<(), Conflict> {
        // One entry per verb: the model names a tool and nothing else, so a
        // second entry could never be reached.
        if self.routes.contains_key(&verb) {
            return Err(Conflict(verb));
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
            .map(|(verb, e)| {
                let schema = e.meta.schema.clone().unwrap_or_else(|| {
                    Payload::of(&serde_json::json!({"type": "object"})).unwrap()
                });
                let schema = match &e.answer {
                    Answer::Kernel { instances } if instances.len() > 1 => {
                        with_instance_choice(&schema, verb, instances)
                    }
                    _ => schema,
                };
                ToolDef {
                    verb: verb.clone(),
                    description: e.meta.description.clone(),
                    schema,
                }
            })
            .collect()
    }
}

/// The argument that names the instance, added to a tool's schema.
fn with_instance_choice(schema: &Payload, verb: &Verb, instances: &[PluginName]) -> Payload {
    let mut s: serde_json::Value = schema
        .parse()
        .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
    if !s.is_object() {
        s = serde_json::json!({"type": "object"});
    }
    let names: Vec<&str> = instances.iter().map(PluginName::as_str).collect();
    s["properties"]["instance"] = serde_json::json!({
        "type": "string",
        "enum": names,
        "description": format!(
            "Which {} instance answers. Several are running, so this is required.",
            verb.driver()
        ),
    });
    let required = s["required"].as_array().cloned().unwrap_or_default();
    let mut required: Vec<serde_json::Value> = required;
    if !required.iter().any(|r| r == "instance") {
        required.push(serde_json::Value::from("instance"));
    }
    s["required"] = serde_json::Value::Array(required);
    Payload::of(&s).expect("a schema is json")
}

/// Take the instance the model named out of the arguments, when there was a
/// choice to make. With one instance or none listed the arguments pass
/// untouched and the call goes unnamed.
pub fn pick_instance(
    instances: &[PluginName],
    args: Payload,
) -> Result<(Option<PluginName>, Payload), String> {
    if instances.len() < 2 {
        return Ok((None, args));
    }
    let mut a: serde_json::Value = args
        .parse()
        .map_err(|e| format!("tool arguments are not json: {e}"))?;
    let Some(chosen) = a
        .as_object_mut()
        .and_then(|o| o.remove("instance"))
        .and_then(|v| v.as_str().map(String::from))
    else {
        return Err(format!(
            "several instances answer this; `instance` must be one of: {}",
            instances
                .iter()
                .map(PluginName::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    let at = PluginName::parse(&chosen).map_err(|e| e.to_string())?;
    if !instances.contains(&at) {
        return Err(format!("no such instance: {chosen}"));
    }
    let args = Payload::of(&a).map_err(|e| e.to_string())?;
    Ok((Some(at), args))
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
        if exclude.iter().any(|e| e == g.verb.driver()) {
            continue;
        }
        table.put(
            g.verb,
            Answer::Kernel {
                instances: g.instances,
            },
            ToolMeta {
                description: g.description,
                schema: Some(g.schema),
            },
        );
    }
    for t in config_tools {
        table.put(
            t.verb.clone(),
            Answer::Kernel {
                instances: Vec::new(),
            },
            ToolMeta {
                description: t.description.clone(),
                schema: Some(t.schema.clone()),
            },
        );
    }
    if table.answerers(&crate::ARTIFACT_READ).is_empty() {
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

    fn grant(verb: &str, instances: &[&str]) -> Grant {
        Grant {
            verb: Verb::parse(verb).unwrap(),
            description: "d".into(),
            schema: Payload::of(&serde_json::json!({"type": "object", "properties": {}})).unwrap(),
            counts_left: None,
            instances: instances
                .iter()
                .map(|n| PluginName::parse(n).unwrap())
                .collect(),
        }
    }

    /// One table decides both what the model is shown and where each call
    /// goes, so the two can no longer disagree — and an excluded driver is
    /// genuinely not offered rather than merely left off a list.
    #[test]
    fn the_table_is_both_the_tool_list_and_the_routing_decision() {
        let table = assemble(
            vec![
                grant("browser::open", &["portos-browser"]),
                grant("egress::http", &["portos-broker"]),
            ],
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
        assert!(matches!(
            table.resolve(&open, None).map(|r| r.target.clone()),
            Ok(Answer::Kernel { .. })
        ));

        // Answered here, because no plugin serves it.
        assert_eq!(
            table
                .resolve(&crate::ARTIFACT_READ, None)
                .map(|r| r.target.clone()),
            Ok(Answer::Here(Local::ReadArtifact))
        );

        // The half that used to leak: excluded meant unlisted, not
        // unreachable — this driver holds the capability, so the call would
        // have gone through if the model asked for it anyway.
        let egress = Verb::parse("egress::http").unwrap();
        assert!(
            table.resolve(&egress, None).is_err(),
            "a miss is an error, not a fall-through to the kernel"
        );
    }

    /// The model is asked to choose exactly when there is a choice: two
    /// browsers put an `instance` argument on the tool, one browser does
    /// not, and the chosen name leaves the arguments before they travel.
    #[test]
    fn a_choice_of_instances_becomes_an_argument_and_only_then() {
        let table = assemble(
            vec![
                grant("browser::open", &["portos-browser", "portos-remote-mac"]),
                grant("fs::read", &["portos-fs"]),
            ],
            &[],
            &[],
        );
        let defs = table.tool_defs();
        let schema_of = |v: &str| -> serde_json::Value {
            defs.iter()
                .find(|t| t.verb.as_str() == v)
                .unwrap()
                .schema
                .parse()
                .unwrap()
        };
        let open = schema_of("browser::open");
        assert_eq!(
            open["properties"]["instance"]["enum"],
            serde_json::json!(["portos-browser", "portos-remote-mac"])
        );
        assert!(
            open["required"]
                .as_array()
                .unwrap()
                .contains(&"instance".into())
        );
        assert!(
            schema_of("fs::read")["properties"]
                .get("instance")
                .is_none(),
            "one instance is not a choice"
        );

        let two = [
            PluginName::parse("portos-browser").unwrap(),
            PluginName::parse("portos-remote-mac").unwrap(),
        ];
        let args =
            Payload::of(&serde_json::json!({"instance": "portos-remote-mac", "url": "x"})).unwrap();
        let (at, rest) = pick_instance(&two, args).unwrap();
        assert_eq!(at.unwrap().as_str(), "portos-remote-mac");
        let rest: serde_json::Value = rest.parse().unwrap();
        assert_eq!(
            rest,
            serde_json::json!({"url": "x"}),
            "the name does not travel"
        );

        let unnamed = Payload::of(&serde_json::json!({"url": "x"})).unwrap();
        assert!(
            pick_instance(&two, unnamed).is_err(),
            "no choice made is an error"
        );

        let one = [PluginName::parse("portos-fs").unwrap()];
        let args = Payload::of(&serde_json::json!({"path": "p"})).unwrap();
        let (at, _) = pick_instance(&one, args).unwrap();
        assert!(at.is_none(), "one instance needs no name");
    }
}
