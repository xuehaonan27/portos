//! A driver interface, as data.
//!
//! `drivers/<name>/driver.json` *is* the interface: its verbs, what each
//! says about itself, and the shape of its arguments — and of its reply,
//! where an implementation with no types of its own has to be held to one.
//! Every reader parses this one document: the Rust interface crate for its
//! `tools()`, each SDK to hold an implementation to it, a conformance test to
//! hold a hello to it. So the wording of a verb exists once, and an
//! implementation cannot describe itself.

use crate::ids::{IdError, Topic, Verb};
use crate::wire::{Payload, ToolMeta};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Driver {
    /// The first segment of every verb here.
    pub driver: String,
    /// By short name.
    pub verbs: BTreeMap<String, VerbSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerbSpec {
    pub description: String,
    /// JSON Schema of the arguments: what a caller is shown, and what an
    /// SDK checks a call against.
    pub args: Payload,
    /// JSON Schema of the reply, for an implementation that has no types to
    /// be held to instead. Absent where every implementation is typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<Payload>,
    /// The reply, or these fields of it, may be large: an SDK stores what
    /// is over the line and leaves `Bulk::Stored` behind, so a handler
    /// returns its text or its document and never sees the line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bulk: Option<BulkSpec>,
    /// The verb is accepted, not awaited: the call returns once the work is
    /// admitted, the work reports on a topic, and exactly one terminal
    /// event ends it. An SDK owes that event when the work does not pay it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<AcceptedSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BulkSpec {
    /// The content type of what is stored.
    pub r#type: String,
    /// Which top-level fields of the reply are bulky. Empty means the reply
    /// itself is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AcceptedSpec {
    /// Where the work reports, with `{field}` filled from the arguments.
    pub topic: String,
    /// Which events end the work.
    pub terminal: Terminal,
    /// What to publish when the work ends without ending it: a `"{error}"`
    /// string value is replaced by the reason.
    pub failed: Payload,
}

/// An event ends the work when this field of it has one of these values.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Terminal {
    pub field: String,
    pub values: Vec<String>,
}

impl AcceptedSpec {
    /// The topic this call's work reports on.
    pub fn topic_for(&self, args: &Payload) -> Result<Topic, String> {
        let fields: BTreeMap<String, Payload> = match self.topic.contains('{') {
            true => args
                .parse()
                .map_err(|_| "the arguments must be an object to name the topic".to_string())?,
            false => BTreeMap::new(),
        };
        let mut out = String::new();
        let mut rest = self.topic.as_str();
        while let Some(open) = rest.find('{') {
            let close = rest[open..]
                .find('}')
                .ok_or_else(|| format!("unclosed placeholder in topic {}", self.topic))?;
            let name = &rest[open + 1..open + close];
            let value: String = fields
                .get(name)
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| format!("the topic needs a string argument `{name}`"))?;
            out.push_str(&rest[..open]);
            out.push_str(&value);
            rest = &rest[open + close + 1..];
        }
        out.push_str(rest);
        Topic::parse(&out).map_err(|e| e.to_string())
    }

    /// Whether this event ends the work.
    pub fn is_terminal(&self, event: &Payload) -> bool {
        let Ok(fields) = event.parse::<BTreeMap<String, Payload>>() else {
            return false;
        };
        fields
            .get(&self.terminal.field)
            .and_then(|p| p.parse::<String>().ok())
            .is_some_and(|v| self.terminal.values.contains(&v))
    }

    /// The failure event, with the reason filled in.
    pub fn failed_event(&self, error: &str) -> Payload {
        let mut fields: BTreeMap<String, Payload> = self.failed.parse().unwrap_or_default();
        for value in fields.values_mut() {
            if value.parse::<String>().ok().as_deref() == Some("{error}") {
                *value = Payload::of(&error).expect("a string serialises");
            }
        }
        Payload::of(&fields).expect("a map serialises")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("driver document: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error(transparent)]
    Id(#[from] IdError),
}

impl Driver {
    /// Parse a document, refusing one whose names could not be verbs.
    pub fn parse(json: &str) -> Result<Driver, DriverError> {
        let d: Driver = serde_json::from_str(json)?;
        for short in d.verbs.keys() {
            Verb::new(&d.driver, short)?;
        }
        Ok(d)
    }

    /// The verb this document calls `short`.
    pub fn verb(&self, short: &str) -> Result<Verb, IdError> {
        Verb::new(&self.driver, short)
    }

    /// What this document says about a verb, if it is one of this driver's.
    pub fn spec(&self, verb: &Verb) -> Option<&VerbSpec> {
        if verb.driver() != self.driver {
            return None;
        }
        self.verbs.get(verb.short())
    }

    /// Where a plugin's declaration departs from this document, for the
    /// verbs of this driver it declares: a verb the document does not have,
    /// or one described in other words or with another schema. Verbs of the
    /// document it does not declare are not a departure — an implementation
    /// may answer part of an interface — and verbs of other drivers are
    /// another document's business. Empty means it conforms.
    pub fn conformance(&self, verbs: &[Verb], tools: &BTreeMap<Verb, ToolMeta>) -> Vec<String> {
        let mut problems = Vec::new();
        for verb in verbs.iter().filter(|v| v.driver() == self.driver) {
            let Some(spec) = self.verbs.get(verb.short()) else {
                problems.push(format!("{verb}: not a verb of driver {}", self.driver));
                continue;
            };
            let Some(meta) = tools.get(verb) else {
                problems.push(format!("{verb}: declared with nothing said about it"));
                continue;
            };
            if meta.description != spec.description {
                problems.push(format!("{verb}: description differs from the document's"));
            }
            if !meta
                .schema
                .as_ref()
                .is_some_and(|s| same_json(s, &spec.args))
            {
                problems.push(format!(
                    "{verb}: argument schema differs from the document's"
                ));
            }
        }
        problems
    }

    /// The hello shape: what an implementation advertises, verbatim.
    pub fn tools(&self) -> BTreeMap<Verb, ToolMeta> {
        self.verbs
            .iter()
            .map(|(short, spec)| {
                (
                    Verb::new(&self.driver, short).expect("checked at parse"),
                    ToolMeta {
                        description: spec.description.clone(),
                        schema: Some(spec.args.clone()),
                    },
                )
            })
            .collect()
    }
}

/// Two JSON texts that mean the same thing — a document is pretty-printed
/// and a hello is not.
fn same_json(a: &Payload, b: &Payload) -> bool {
    match (
        a.parse::<serde_json::Value>(),
        b.parse::<serde_json::Value>(),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{"driver": "x", "verbs": {
        "ping": {"description": "Ping.", "args": {"type": "object", "properties": {}}},
        "big": {"description": "Big.", "args": {"type": "object"}, "reply": {"type": "string"}}
    }}"#;

    #[test]
    fn a_document_is_verbs_with_what_they_say() {
        let d = Driver::parse(DOC).unwrap();
        let ping = d.verb("ping").unwrap();
        assert_eq!(ping.as_str(), "x::ping");
        assert_eq!(d.spec(&ping).unwrap().description, "Ping.");
        assert!(d.spec(&ping).unwrap().reply.is_none());
        assert!(d.spec(&d.verb("big").unwrap()).unwrap().reply.is_some());
        let tools = d.tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[&ping].description, "Ping.");
        assert_eq!(
            tools[&ping].schema.as_ref().unwrap().as_raw(),
            r#"{"type": "object", "properties": {}}"#
        );
    }

    #[test]
    fn a_verb_of_another_driver_is_not_in_it() {
        let d = Driver::parse(DOC).unwrap();
        assert!(d.spec(&Verb::parse("y::ping").unwrap()).is_none());
        assert!(d.spec(&Verb::parse("x::pong").unwrap()).is_none());
    }

    /// What a plugin declares is held to the document: the document's words
    /// and schema, whatever the whitespace; a verb it does not have, other
    /// words, or another schema is a departure; leaving verbs out is not.
    #[test]
    fn a_declaration_is_held_to_the_document() {
        let d = Driver::parse(DOC).unwrap();
        let ping = d.verb("ping").unwrap();
        let mut tools = BTreeMap::new();
        tools.insert(
            ping.clone(),
            ToolMeta {
                description: "Ping.".into(),
                schema: Some(
                    Payload::of(&serde_json::json!({"properties": {}, "type": "object"})).unwrap(),
                ),
            },
        );
        assert!(
            d.conformance(&[ping.clone()], &tools).is_empty(),
            "same meaning, other text"
        );

        tools.get_mut(&ping).unwrap().description = "Pong.".into();
        assert_eq!(
            d.conformance(&[ping.clone()], &tools),
            vec!["x::ping: description differs from the document's"]
        );

        let pong = Verb::parse("x::pong").unwrap();
        let other = Verb::parse("y::ping").unwrap();
        assert_eq!(
            d.conformance(&[pong, other], &BTreeMap::new()),
            vec!["x::pong: not a verb of driver x"],
            "another driver's verb is not this document's business"
        );
    }

    #[test]
    fn accepted_work_has_a_topic_and_an_ending() {
        let spec = AcceptedSpec {
            topic: "model::session::{session}".into(),
            terminal: Terminal {
                field: "kind".into(),
                values: vec!["done".into(), "failed".into()],
            },
            failed: Payload::of(&serde_json::json!({"kind": "failed", "error": "{error}"}))
                .unwrap(),
        };
        let args = Payload::of(&serde_json::json!({"session": "s7", "text": "hi"})).unwrap();
        assert_eq!(
            spec.topic_for(&args).unwrap().as_str(),
            "model::session::s7"
        );
        assert!(
            spec.topic_for(&Payload::of(&serde_json::json!({"text": "hi"})).unwrap())
                .unwrap_err()
                .contains("`session`")
        );
        assert!(spec.is_terminal(&Payload::of(&serde_json::json!({"kind": "done"})).unwrap()));
        assert!(!spec.is_terminal(&Payload::of(&serde_json::json!({"kind": "delta"})).unwrap()));
        assert_eq!(
            spec.failed_event("it broke").as_raw(),
            r#"{"error":"it broke","kind":"failed"}"#
        );
    }

    #[test]
    fn a_name_that_cannot_be_a_verb_is_refused() {
        let bad = r#"{"driver": "x", "verbs": {"no::pe": {"description": "", "args": {}}}}"#;
        assert!(Driver::parse(bad).is_err());
    }
}
