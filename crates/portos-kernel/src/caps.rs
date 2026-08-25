//! Capability table. Currently using SQLite to store it.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use portos_proto::{Capability, Constraints};
use rusqlite::{Connection, OptionalExtension, params};

use crate::KernelError;

pub struct CapStore {
    // Currently we use a database to store capabilities.
    db: Arc<Mutex<Connection>>,
}

impl CapStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> CapStore {
        CapStore { db }
    }

    fn store(&self, cap: &Capability) -> Result<(), KernelError> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR REPLACE INTO caps (cap_id, json, parent, revoked) VALUES (?1,?2,?3,?4)",
            params![
                cap.cap_id,
                serde_json::to_string(cap).unwrap(),
                cap.parent,
                cap.revoked as i64
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, cap_id: &str) -> Result<Capability, KernelError> {
        let db = self.db.lock().unwrap();
        let json = db
            .query_row(
                "SELECT json FROM caps WHERE cap_id=?1",
                params![cap_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| KernelError::NotFound(cap_id.to_string()))?;
        serde_json::from_str(&json).map_err(|e| KernelError::Corrupt(format!("cap json: {e}")))
    }

    pub fn mint(
        &self,
        subject: &str,
        resource: &str,
        verbs: BTreeSet<String>,
        constraints: Constraints,
        parent: Option<String>,
    ) -> Result<Capability, KernelError> {
        let cap = Capability {
            cap_id: format!("cap_{}", rand_id()),
            subject: subject.to_string(),
            resource: resource.to_string(),
            verbs,
            constraints,
            parent,
            revoked: false,
        };
        self.store(&cap)?;
        Ok(cap)
    }

    /// Attenuation: mint a child that must narrow the parent in every field.
    pub fn attenuate(
        &self,
        parent_id: &str,
        subject: &str,
        verbs: BTreeSet<String>,
        constraints: Constraints,
    ) -> Result<Capability, KernelError> {
        let parent = self.get(parent_id)?;
        if parent.revoked {
            return Err(KernelError::Denied("parent revoked".into()));
        }
        let child = Capability {
            cap_id: format!("cap_{}", rand_id()),
            subject: subject.to_string(),
            resource: parent.resource.clone(),
            verbs,
            constraints,
            parent: Some(parent.cap_id.clone()),
            revoked: false,
        };
        if !child.is_valid_attenuation_of(&parent) {
            return Err(KernelError::Denied("attenuation must narrow".into()));
        }
        self.store(&child)?;
        Ok(child)
    }

    pub fn exercise(&self, cap_id: &str, verb: &str, now: u64) -> Result<(), KernelError> {
        let mut cap = self.get(cap_id)?;
        if cap.revoked {
            return Err(KernelError::Denied("cap revoked".into()));
        }
        if let Some(exp) = cap.constraints.expires_at {
            if now > exp {
                return Err(KernelError::Denied("cap expired".into()));
            }
        }
        if !cap.verbs.contains(verb) {
            return Err(KernelError::Denied(format!("verb not granted: {verb}")));
        }
        if let Some(n) = cap.constraints.counts.get_mut(verb) {
            if *n == 0 {
                return Err(KernelError::Denied(format!("budget exhausted: {verb}")));
            }
            *n -= 1;
            self.store(&cap)?;
        }
        Ok(())
    }

    /// Revoke a capability and everything attenuated from it.
    pub fn revoke(&self, cap_id: &str) -> Result<u64, KernelError> {
        let mut frontier = vec![cap_id.to_string()];
        let mut n = 0u64;
        while let Some(id) = frontier.pop() {
            let mut cap = self.get(&id)?;
            if !cap.revoked {
                cap.revoked = true;
                self.store(&cap)?;
                n += 1;
            }
            let db = self.db.lock().unwrap();
            let mut stmt = db.prepare("SELECT cap_id FROM caps WHERE parent=?1")?;
            let kids: Vec<String> = stmt
                .query_map(params![id], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            drop(db);
            frontier.extend(kids);
        }
        Ok(n)
    }
}

fn rand_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn store(tag: &str) -> (CapStore, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("portos-caps-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = Arc::new(Mutex::new(crate::db::open(&root).unwrap()));
        (CapStore::new(db), root)
    }

    fn verbs(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn counting_exercise_never_overdraws() {
        let (caps, root) = store("count");
        let mut counts = BTreeMap::new();
        counts.insert("emit".to_string(), 2u64);
        let cap = caps
            .mint(
                "session:test",
                "toy:echo",
                verbs(&["emit"]),
                Constraints {
                    expires_at: None,
                    counts,
                },
                None,
            )
            .unwrap();
        assert!(caps.exercise(&cap.cap_id, "emit", 0).is_ok());
        assert!(caps.exercise(&cap.cap_id, "emit", 0).is_ok());
        let e = caps.exercise(&cap.cap_id, "emit", 0);
        assert!(
            matches!(e, Err(KernelError::Denied(_))),
            "third emit must be denied"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn attenuation_must_narrow_and_revocation_cascades() {
        let (caps, root) = store("att");
        let parent = caps
            .mint(
                "session:test",
                "toy:echo",
                verbs(&["emit", "list"]),
                Constraints::default(),
                None,
            )
            .unwrap();
        // widening is rejected
        let bad = caps.attenuate(
            &parent.cap_id,
            "plugin:kid",
            verbs(&["emit", "delete"]),
            Constraints::default(),
        );
        assert!(bad.is_err());
        // narrowing is fine
        let kid = caps
            .attenuate(
                &parent.cap_id,
                "plugin:kid",
                verbs(&["emit"]),
                Constraints::default(),
            )
            .unwrap();
        assert!(caps.exercise(&kid.cap_id, "emit", 0).is_ok());
        // revoking the parent kills the child too
        let n = caps.revoke(&parent.cap_id).unwrap();
        assert_eq!(n, 2);
        assert!(caps.exercise(&kid.cap_id, "emit", 0).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
