//! portos-signer: the reference consent signer.
//!
//! Signing is NOT the kernel's business. The kernel defines the consent
//! quadruple and its verification path (the channel); a signer is a
//! replaceable implementation — a plugin in the end state. This crate is the
//! **reference implementation** used by the CLI and tests: a keyed-blake3 MAC
//! over the quadruple, with the key held in the root (`consent.key`, the same
//! shared secret the kernel verifies against). UI prompts, hardware tokens,
//! and LLM-authored signer plugins are other implementations of the same
//! channel — none of them is "standard".

use portos_kernel::consent::{Budget, ConsentRecord};
use std::path::Path;

/// The reference signer: signs quadruples with the root's consent key.
pub struct Signer([u8; 32]);

impl Signer {
    /// Load the shared consent key from `<root>/consent.key` (created by the
    /// kernel on first open — the channel's bootstrap).
    pub fn load(root: &Path) -> std::io::Result<Signer> {
        let bytes = std::fs::read(root.join("consent.key"))?;
        if bytes.len() != 32 {
            return Err(std::io::Error::other("consent.key: expected 32 bytes"));
        }
        let mut k = [0u8; 32];
        k.copy_from_slice(&bytes);
        Ok(Signer(k))
    }

    /// Sign a consent quadruple: (h_plan, budget, fresh nonce, ttl).
    pub fn sign(&self, plan_hash: &str, budget: Budget, ttl_secs: u64) -> ConsentRecord {
        use rand::RngCore;
        let mut nb = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nb);
        let nonce = hex::encode(nb);
        let issued_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mac = blake3::keyed_hash(
            &self.0,
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
}

/// The MAC payload: key-ordered JSON, hence deterministic.
fn mac_input(plan_hash: &str, budget: &Budget, nonce: &str, issued_at: u64, ttl: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "plan_hash": plan_hash,
        "budget": budget,
        "nonce": nonce,
        "issued_at": issued_at,
        "ttl_secs": ttl,
    }))
    .unwrap()
}
