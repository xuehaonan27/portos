//! Artifact metadata. Content-addressed by blake3.

use crate::label::Label;
use serde::{Deserialize, Serialize};

pub type ArtifactId = String; // "blake3:<hex>"

/// TODO: blake3 is good known for its parallel computation ability.
pub fn id_for_bytes(bytes: &[u8]) -> ArtifactId {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub id: ArtifactId,
    /// (Currently) MIME-ish type, e.g. "text/plain", "portos/plan".
    /// TODO: but we could have a better representation using type
    /// instead of plain strings.
    pub r#type: String,
    pub size: u64,
    pub labels: Label,
    /// Producer, e.g. "plugin:portos-echo/1".
    /// TODO: using plain string is ugly.
    pub origin: String,
    /// Unix seconds.
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}
