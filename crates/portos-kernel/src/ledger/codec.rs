use super::*;
use portos_rm::ledger::{HoldingRecord, LedgerSnapshot, PoolRecord};
use portos_rm::ra::{Frac, GSet, Ranges};
use portos_rm::time::LeaseDuration;
use serde_json::json;

pub(super) fn corrupt(detail: impl Into<String>) -> KernelError {
    KernelError::Corrupt(format!("resource storage: {}", detail.into()))
}

pub(super) fn frag_to_json(f: &Frag) -> String {
    match f {
        Frag::Ex(Ex::Token) => json!({"ex":"token"}),
        Frag::Ex(Ex::Bot) => json!({"ex":"bot"}),
        Frag::Count(Count::Value(n)) => json!({"count":n}),
        Frag::Count(Count::Invalid) => json!({"count":"invalid"}),
        Frag::Set(GSet(s)) => json!({"set":s}),
        Frag::Range(r) => json!({"range":r.spans(),"bot":r.is_bot()}),
        Frag::Frac(q) => {
            let (n, d) = q.parts();
            let part = |s: String| {
                s.parse::<u64>()
                    .map(|n| json!(n))
                    .unwrap_or_else(|_| json!(s))
            };
            json!({"frac":[part(n),part(d)]})
        }
    }
    .to_string()
}

pub(super) fn frag_from_json(s: &str) -> Result<Frag, KernelError> {
    let v: Value = serde_json::from_str(s).map_err(|e| corrupt(format!("fragment JSON: {e}")))?;
    let bad = || corrupt(format!("malformed fragment: {s}"));
    let obj = v.as_object().ok_or_else(bad)?;
    if obj.len() == 1 {
        if let Some(ex) = obj.get("ex") {
            return match ex.as_str() {
                Some("token") => Ok(Frag::Ex(Ex::Token)),
                Some("bot") => Ok(Frag::Ex(Ex::Bot)),
                _ => Err(bad()),
            };
        }
        if let Some(count) = obj.get("count") {
            return if count.as_str() == Some("invalid") {
                Ok(Frag::Count(Count::Invalid))
            } else {
                count
                    .as_u64()
                    .map(|n| Frag::Count(Count::Value(n)))
                    .ok_or_else(bad)
            };
        }
        if let Some(set) = obj.get("set") {
            let mut values = std::collections::BTreeSet::new();
            for value in set.as_array().ok_or_else(bad)? {
                values.insert(value.as_str().ok_or_else(bad)?.to_string());
            }
            return Ok(Frag::Set(GSet(values)));
        }
        if let Some(q) = obj.get("frac") {
            let q = q.as_array().filter(|q| q.len() == 2).ok_or_else(bad)?;
            let part = |v: &Value| {
                v.as_u64()
                    .map(|n| n.to_string())
                    .or_else(|| v.as_str().map(str::to_owned))
            };
            return Frac::from_parts(&part(&q[0]).ok_or_else(bad)?, &part(&q[1]).ok_or_else(bad)?)
                .map(Frag::Frac)
                .ok_or_else(bad);
        }
    }
    if (obj.len() == 1 || obj.len() == 2 && obj.contains_key("bot")) && obj.contains_key("range") {
        let mut spans = Vec::new();
        for span in obj["range"].as_array().ok_or_else(bad)? {
            let pair = span.as_array().filter(|p| p.len() == 2).ok_or_else(bad)?;
            spans.push((
                pair[0].as_u64().ok_or_else(bad)?,
                pair[1].as_u64().ok_or_else(bad)?,
            ));
        }
        let bot = match obj.get("bot") {
            None => false,
            Some(v) => v.as_bool().ok_or_else(bad)?,
        };
        if bot {
            if !spans.is_empty() {
                return Err(bad());
            }
            return Ok(Frag::Range(Ranges::bot()));
        }
        return Ranges::try_of(&spans)
            .map(Frag::Range)
            .map_err(|e| corrupt(format!("raw range: {e:?}")));
    }
    Err(bad())
}

pub(super) fn algebra_name(tag: AlgebraTag) -> &'static str {
    match tag {
        AlgebraTag::Exclusive => "exclusive",
        AlgebraTag::Counted => "counted",
        AlgebraTag::Set => "set",
        AlgebraTag::Range => "range",
        AlgebraTag::Frac => "frac",
    }
}
fn algebra(s: &str) -> Result<AlgebraTag, KernelError> {
    match s {
        "exclusive" => Ok(AlgebraTag::Exclusive),
        "counted" => Ok(AlgebraTag::Counted),
        "set" => Ok(AlgebraTag::Set),
        "range" => Ok(AlgebraTag::Range),
        "frac" => Ok(AlgebraTag::Frac),
        _ => Err(corrupt(format!("unknown algebra {s}"))),
    }
}
pub(super) fn grade_name(g: RevertGrade) -> &'static str {
    match g {
        RevertGrade::Inverse => "inverse",
        RevertGrade::Compensable => "compensable",
        RevertGrade::External => "external",
    }
}
pub(super) fn grade(s: &str) -> Result<RevertGrade, KernelError> {
    match s {
        "inverse" => Ok(RevertGrade::Inverse),
        "compensable" => Ok(RevertGrade::Compensable),
        "external" => Ok(RevertGrade::External),
        _ => Err(corrupt(format!("unknown grade {s}"))),
    }
}
pub(super) fn persist(conn: &Connection, snapshot: &LedgerSnapshot) -> Result<(), KernelError> {
    for d in &snapshot.classes {
        conn.execute("INSERT INTO resource_classes(class_id,algebra,release_idempotent,lease_secs,revert_grade) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(class_id) DO NOTHING", params![d.class_id.as_str(), algebra_name(d.algebra), i64::from(d.release_idempotent), d.lease_duration.map(|d| i64::try_from(d.get()).expect("duration validated")), grade_name(d.revert_grade)])?;
    }
    for p in &snapshot.pools {
        conn.execute("INSERT INTO resource_pools(class_id,instance,capacity) VALUES(?1,?2,?3) ON CONFLICT(class_id,instance) DO UPDATE SET capacity=excluded.capacity", params![p.key.class().as_str(), p.key.instance().as_str(), frag_to_json(&p.capacity)])?;
    }
    for (child, parent) in &snapshot.instantiations {
        conn.execute("INSERT INTO resource_instantiations(child,parent) VALUES(?1,?2) ON CONFLICT(child) DO UPDATE SET parent=excluded.parent", params![child.as_str(),parent.as_str()])?;
    }
    for h in &snapshot.holdings {
        let kind = match h.lease {
            Lease::Until(_) => "until",
            Lease::ParentBound => "parent",
            Lease::Unbounded => "unbounded",
        };
        conn.execute("INSERT INTO holdings(id,subject,class_id,instance,frag,generation,parent,lease_expires_at,acquired_at,released_at,lease_kind) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(id) DO UPDATE SET subject=excluded.subject,lease_expires_at=excluded.lease_expires_at,released_at=excluded.released_at,lease_kind=excluded.lease_kind", params![h.id.to_sql(),h.subject.as_str(),h.class_id.as_str(),h.instance.as_str(),frag_to_json(&h.frag),h.generation.as_str(),h.parent.map(HoldingId::to_sql),h.lease.expires_at().map(Timestamp::to_sql),h.acquired_at.to_sql(),h.released_at.map(Timestamp::to_sql),kind])?;
    }
    Ok(())
}

pub(super) fn decode(conn: &Connection, legacy: bool) -> Result<LedgerSnapshot, KernelError> {
    let mut snapshot = LedgerSnapshot::default();
    let mut stmt = conn.prepare("SELECT class_id,algebra,release_idempotent,lease_secs,revert_grade FROM resource_classes ORDER BY class_id")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (id, a, idem, lease, g) = row?;
        if idem != 0 && idem != 1 {
            return Err(corrupt("invalid release declaration"));
        }
        let duration = lease
            .map(|n| {
                u64::try_from(n)
                    .map_err(|_| corrupt("negative duration"))
                    .and_then(|n| {
                        LeaseDuration::try_from(n).map_err(|e| corrupt(format!("duration: {e:?}")))
                    })
            })
            .transpose()?;
        snapshot.classes.push(ClassDecl {
            class_id: id.into(),
            algebra: algebra(&a)?,
            release_idempotent: idem == 1,
            lease_duration: duration,
            revert_grade: grade(&g)?,
        });
    }
    let mut stmt = conn.prepare(
        "SELECT class_id,instance,capacity FROM resource_pools ORDER BY class_id,instance",
    )?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (c, i, f) = row?;
        snapshot.pools.push(PoolRecord {
            key: ResourceKey::new(c.into(), i.into()),
            capacity: frag_from_json(&f)?,
        });
    }
    let mut stmt =
        conn.prepare("SELECT child,parent FROM resource_instantiations ORDER BY child")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (c, p) = row?;
        snapshot.instantiations.push((c.into(), p.into()));
    }
    let mut stmt = conn.prepare("SELECT id,subject,class_id,instance,frag,generation,parent,lease_expires_at,acquired_at,released_at,lease_kind FROM holdings ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, Option<i64>>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, Option<i64>>(9)?,
            r.get::<_, Option<String>>(10)?,
        ))
    })?;
    let boundary = |e| corrupt(format!("holding identity/time: {e:?}"));
    for row in rows {
        let (id, subject, c, i, f, g, p, exp, acq, rel, kind) = row?;
        let id = HoldingId::try_from(id).map_err(boundary)?;
        let parent = p.map(HoldingId::try_from).transpose().map_err(boundary)?;
        let expires = exp.map(Timestamp::try_from).transpose().map_err(boundary)?;
        let lease = match (kind.as_deref(), expires) {
            (Some("until"), Some(t)) => Lease::Until(t),
            (Some("parent"), None) if parent.is_some() => Lease::ParentBound,
            (Some("unbounded"), None) => Lease::Unbounded,
            (None, Some(t)) if legacy => Lease::Until(t),
            (None, None) if legacy && parent.is_some() => Lease::ParentBound,
            (None, None) if legacy => Lease::Unbounded,
            _ => {
                return Err(corrupt(format!(
                    "holding {id}: invalid lease representation"
                )));
            }
        };
        snapshot.holdings.push(HoldingRecord {
            id,
            subject: subject.into(),
            class_id: c.into(),
            instance: i.into(),
            frag: frag_from_json(&f)?,
            generation: g.into(),
            parent,
            lease,
            acquired_at: Timestamp::try_from(acq).map_err(boundary)?,
            released_at: rel.map(Timestamp::try_from).transpose().map_err(boundary)?,
        });
    }
    Ok(snapshot)
}
