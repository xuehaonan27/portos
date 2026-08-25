//! Consent quadruple.
//! Currently a local keyed-MAC (blake3 keyed mode) stands in for the
//! companion's Touch ID signature. The data flow is identical to the
//! real thing, only the signer changes later. The plan hash is the
//! plan artifacts's CAS id, so consent, audit and pinning all reference
//! one hash.

use crate::KernelError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub struct ConsentKey([u8; 32]);

impl ConsentKey {
    pub fn load_or_create(root: &Path) -> Result<ConsentKey, KernelError> {
        let path = root.join("consent.key");

        // There's already one
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() == 32 {
                let mut k = [0u8; 32];
                k.copy_from_slice(&bytes);
                return Ok(ConsentKey(k));
            }
        }

        // Create new one and store it
        use rand::RngCore;
        let mut k = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut k);
        std::fs::write(&path, k)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(ConsentKey(k))
    }
}

/// Budget as the user confirms it: map verb-class to count bound.
/// TODO: Protocol templates and provenance constraints join later.
/// The counting multiset adds later.
pub type Budget = BTreeMap<String, u64>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsentRecord {
    /// CAS id of the plan bytes. None would mean a standing consent,
    /// which is not implemented for now.
    pub plan_hash: String,
    pub budget: Budget,
    pub nonce: String,
    pub issued_at: u64,
    pub ttl_secs: u64,
    pub mac: String,
}

/// TODO: currently supports MacOS, but cross-platform should be supported
/// TODO: currently mimicking real input.
fn mac_input(plan_hash: &str, budget: &Budget, nonce: &str, issued_at: u64, ttl: u64) -> Vec<u8> {
    // BTreeMap serialization is key-ordered, hence deterministic
    serde_json::to_vec(&serde_json::json!({
        "plan_hash": plan_hash,
        "budget": budget,
        "nonce": nonce,
        "issued_at": issued_at,
        "ttl_secs": ttl,
    }))
    .unwrap()
}

impl ConsentRecord {
    pub fn sign(key: &ConsentKey, plan_hash: &str, budget: Budget, ttl_secs: u64) -> ConsentRecord {
        use rand::RngCore;
        let mut nb = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nb);
        let nonce = hex::encode(nb);
        let issued_at = crate::db::now_unix();
        let mac = blake3::keyed_hash(
            &key.0,
            &mac_input(plan_hash, &budget, &nonce, issued_at, ttl_secs),
        );
        ConsentRecord {
            plan_hash: plan_hash.to_string(),
            budget,
            nonce,
            issued_at,
            ttl_secs,
            mac: mac.to_hex().to_string(),
        }
    }

    pub fn verify(&self, key: &ConsentKey, now: u64) -> Result<(), KernelError> {
        let expect = blake3::keyed_hash(
            &key.0,
            &mac_input(
                &self.plan_hash,
                &self.budget,
                &self.nonce,
                self.issued_at,
                self.ttl_secs,
            ),
        );
        if expect.to_hex().to_string() != self.mac {
            return Err(KernelError::Denied("consent MAC mismatch".into()));
        }
        if now > self.issued_at + self.ttl_secs {
            return Err(KernelError::Denied("consent expired".into()));
        }
        Ok(())
    }
}

/// Deterministic canonical rendering of a budget for the consent surface.
///
/// # Documentation
/// docs/effect-plan-v0.md §5.6: fixed wording, fixed (sorted) order, plain text.
/// Model-authored prose must NEVER be interpolated here.
pub fn render_budget(plan_hash: &str, budget: &Budget) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Plan {}\nwill get these effect budgets:\n",
        &plan_hash[..21.min(plan_hash.len())]
    ));
    for (verb, n) in budget {
        s.push_str(&format!("  - {verb} <= {n}\n"));
    }
    s.push_str("Operation that exceeds budget will be stopped. Reading and pure computation are not limited though\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_and_tamper() {
        let root = std::env::temp_dir().join(format!("portos-consent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        std::fs::create_dir_all(&root).unwrap();
        let key = ConsentKey::load_or_create(&root).unwrap();
        let mut b = Budget::new();
        b.insert("echo::emit".into(), 3);
        let rec = ConsentRecord::sign(&key, "blake3:abc", b, 3600);
        assert!(rec.verify(&key, crate::db::now_unix()).is_ok());

        let mut tampered = rec.clone();
        tampered.budget.insert("echo::emit".into(), 3000);
        assert!(tampered.verify(&key, crate::db::now_unix()).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
