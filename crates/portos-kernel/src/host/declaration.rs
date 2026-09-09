//! Decode and check plugin declarations at the hello boundary.

use super::routing::TargetSpec;
use portos_rm::coeffect::{Flat, Manifest, Requires};
use portos_rm::identity::{ClassId, VerbId};
use portos_rm::ledger::RevertGrade;
use portos_rm::protocol::{Protocol, ProtocolDraft};
use portos_rm::verbs::{
    CheckedClass, CheckedVerb, ClassDeclarationDraft, ConsumeGrade, EmitGrade, Kind, VerbEntry,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn str_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Decode declaration names before constructing domain values.
pub(super) fn declaration_identity(hello: &Value) -> Result<(String, Vec<String>), String> {
    let name = hello
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("name must be a nonempty string")?;
    let verbs = hello
        .get("verbs")
        .and_then(Value::as_array)
        .ok_or("verbs must be an array")?;
    let mut names = std::collections::BTreeSet::new();
    let verbs = verbs
        .iter()
        .map(|v| {
            let v = v
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("verbs must contain nonempty strings")?;
            if !names.insert(v) {
                return Err("duplicate advertised verb");
            }
            Ok(v.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((name.to_string(), verbs))
}

/// Check the plugin's F4 class using the names decoded by `declaration_identity`.
/// Verbs use their full `family::verb` name. Verbs without a declared `kind`
/// are absent from the class and retain their legacy routing behavior.
pub(super) fn check_class_declaration(
    name: &str,
    verbs: &[String],
    tools_meta: &Value,
    hello: &Value,
) -> Result<CheckedClass, String> {
    if !tools_meta.is_null() && !tools_meta.is_object() {
        return Err("tools must be an object".into());
    }
    let mut draft = ClassDeclarationDraft::new(ClassId::new(name));
    if let Some(rho) = optional_str(hello, "holding_rho")? {
        draft.holding_grade = Some(match rho {
            "inverse" => RevertGrade::Inverse,
            "compensable" => RevertGrade::Compensable,
            "external" => RevertGrade::External,
            _ => return Err(format!("unknown holding_rho: {rho}")),
        });
    }
    for v in verbs {
        if let Some(entry) = kind_from_meta(&tools_meta[v.as_str()])? {
            draft.verbs.push((VerbId::new(v), entry));
        }
    }
    draft.protocol = protocol_from_json(hello.get("protocol"))?;
    draft.check().map_err(|e| format!("{e:?}"))
}

fn optional_str<'a>(v: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    v.get(key)
        .map(|x| x.as_str().ok_or_else(|| format!("{key} must be a string")))
        .transpose()
}
fn optional_bool(v: &Value, key: &str) -> Result<Option<bool>, String> {
    v.get(key)
        .map(|x| {
            x.as_bool()
                .ok_or_else(|| format!("{key} must be a boolean"))
        })
        .transpose()
}

fn kind_from_meta(meta: &Value) -> Result<Option<VerbEntry>, String> {
    if !meta.is_null() && !meta.is_object() {
        return Err("verb metadata must be an object".into());
    }
    let Some(kind) = optional_str(meta, "kind")? else {
        if [
            "world",
            "compensate_with",
            "amortizable",
            "idempotent",
            "commutes",
            "degrade",
        ]
        .iter()
        .any(|k| meta.get(k).is_some())
        {
            return Err("verb character fields require kind".into());
        }
        return Ok(None);
    };
    let compensate = optional_str(meta, "compensate_with")?.map(VerbId::new);
    let world = optional_str(meta, "world")?;
    let amortizable = optional_bool(meta, "amortizable")?.unwrap_or(true);
    let mut entry = match kind {
        "repeatable" => VerbEntry::repeatable(),
        "repeatable_shared" => VerbEntry::repeatable_shared(),
        "transforming" => VerbEntry::transforming(),
        "consuming" => {
            let world = match world {
                None | Some("held") => ConsumeGrade::Held,
                Some("compensable") => ConsumeGrade::Compensable {
                    compensate_with: compensate
                        .ok_or("consuming/compensable needs compensate_with")?,
                },
                Some("external") => ConsumeGrade::External,
                Some(other) => return Err(format!("unknown consuming world {other}")),
            };
            VerbEntry::consuming(world)
        }
        "emitting" => {
            let world = match world {
                None | Some("external") => EmitGrade::External,
                Some("compensable") => EmitGrade::Compensable {
                    compensate_with: compensate
                        .ok_or("emitting/compensable needs compensate_with")?,
                },
                Some(other) => return Err(format!("unknown emitting world {other}")),
            };
            VerbEntry::emitting(world, amortizable)
        }
        other => return Err(format!("unknown verb kind {other}")),
    };
    if let Some(b) = optional_bool(meta, "idempotent")? {
        entry.idempotent = b;
    }
    if let Some(b) = optional_bool(meta, "commutes")? {
        entry.commutes = b;
    }
    if let Some(d) = optional_str(meta, "degrade")? {
        entry = entry.degrades_to(d);
    }
    Ok(Some(entry))
}

pub(super) fn kind_label(e: &CheckedVerb) -> &'static str {
    match e.kind() {
        Kind::Repeatable => "repeatable",
        Kind::Transforming => "transforming",
        Kind::Consuming { .. } => "consuming",
        Kind::Emitting { .. } => "emitting",
    }
}

/// `{"initial": "s0", "transitions": [["s0", "family::verb", "s1"], …]}`.
fn protocol_from_json(v: Option<&Value>) -> Result<Option<Protocol>, String> {
    let Some(v) = v else { return Ok(None) };
    if v.is_null() {
        return Ok(None);
    }
    let initial = v
        .get("initial")
        .and_then(|i| i.as_str())
        .ok_or("protocol needs an initial state")?;
    let mut p = ProtocolDraft::new(initial);
    for t in v
        .get("transitions")
        .and_then(|t| t.as_array())
        .ok_or("protocol transitions must be an array")?
    {
        if t.as_array().is_none_or(|a| a.len() != 3) {
            return Err("protocol transition must have exactly three names".into());
        }
        let (Some(from), Some(verb), Some(to)) = (
            t.get(0).and_then(|x| x.as_str()),
            t.get(1).and_then(|x| x.as_str()),
            t.get(2).and_then(|x| x.as_str()),
        ) else {
            return Err("protocol transition must be [from, verb, to]".into());
        };
        p = p.transition(from, verb, to);
    }
    Ok(Some(p.check().map_err(|e| format!("protocol: {e:?}"))?))
}

/// The plugin's F5 manifest from its hello: per verb, the caps it needs to
/// invoke and the services it depends on (`tools[verb].requires`).
pub(super) fn manifest_from_meta(name: &str, verbs: &[String], tools_meta: &Value) -> Manifest {
    let mut m = Manifest {
        driver: name.to_string(),
        verbs: BTreeMap::new(),
    };
    for v in verbs {
        let req = &tools_meta[v.as_str()]["requires"];
        m.verbs.insert(
            v.clone(),
            Requires {
                caps: Flat(str_array(&req["caps"]).into_iter().collect()),
                deps: Flat(str_array(&req["deps"]).into_iter().collect()),
                uses: Default::default(),
            },
        );
    }
    m
}

/// The existing wire convention treats unspecified kinds as literal targets.
pub(super) fn target_from_meta(meta: Option<&Value>) -> Option<TargetSpec> {
    let meta = meta?;
    let arg = meta["arg"].as_str()?.to_string();
    Some(match meta["kind"].as_str() {
        Some("origin") => TargetSpec::Origin(arg),
        _ => TargetSpec::Literal(arg),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_character_fields_are_not_defaulted() {
        for meta in [
            json!({"kind": 1}),
            json!({"kind": "emitting", "amortizable": "false"}),
            json!({"kind": "consuming", "world": false}),
            json!({"kind": "repeatable", "idempotent": "true"}),
            json!({"kind": "repeatable", "commutes": 1}),
            json!({"kind": "repeatable", "degrade": 0}),
            json!({"kind": "emitting", "compensate_with": false}),
            json!({"commutes": true}),
        ] {
            assert!(kind_from_meta(&meta).is_err(), "{meta}");
        }
        for hello in [
            json!({"name": "p", "verbs": ["v", 1]}),
            json!({"name": "p", "verbs": ["v", "v"]}),
            json!({"name": "p"}),
        ] {
            assert!(declaration_identity(&hello).is_err());
        }
    }

    #[test]
    fn checked_character_preserves_legacy_defaults_and_hard_list() {
        let verbs = vec!["emit".into(), "send".into(), "legacy".into()];
        let class = check_class_declaration(
            "p",
            &verbs,
            &json!({
                "emit": {"kind": "emitting"}, "send": {"kind": "emitting", "amortizable": false},
            }),
            &json!({}),
        )
        .unwrap();
        assert!(!class.lookup(&VerbId::new("emit")).unwrap().withhold());
        assert!(class.lookup(&VerbId::new("send")).unwrap().withhold());
        assert!(class.lookup(&VerbId::new("legacy")).is_err());
        assert!(
            check_class_declaration("p", &verbs, &json!({}), &json!({"holding_rho": 1})).is_err()
        );
    }

    #[test]
    fn invalid_protocols_and_dangling_relations_cannot_be_published() {
        for protocol in [
            json!({"initial": "s", "transitions": false}),
            json!({"initial": "s", "transitions": [["s", "read", "s", "extra"]]}),
            json!({"initial": "s", "transitions": [["s", "read", "s"], ["s", "read", "other"]]}),
        ] {
            assert!(protocol_from_json(Some(&protocol)).is_err());
        }
        assert!(
            check_class_declaration(
                "p",
                &["read".into()],
                &json!({"read": {"kind": "repeatable", "degrade": "unknown"}}),
                &json!({})
            )
            .is_err()
        );
    }
}
