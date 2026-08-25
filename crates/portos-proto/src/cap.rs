//! Capabilities with counting constraints.
//!
//! A budget minted by user consent IS a counting capability. `counts` maps
//! verb-class to remaining balance; excercise decrementstransactionally
//! and never overdraws.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraints {
    /// Unix seconds; None = no expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// `counts` maps verb-class to remaining count.
    /// Absent verb class has unlimited budget, but currently only for
    /// observation verbs. Effect verbs should always be counted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counts: BTreeMap<String, u64>,
}

/// An example of how a capability object would look like:
/// ```json
/// {
///   "cap_id": "cap_1c00a2...",
///   "subject": "plugin:browser-driver@0.1/instance:7",
///   "resource": "web-origin:https://example.com",
///   "verbs": ["navigate", "dom.read", "input", "screenshot"],
///   "constraints": {
///     "expires_at": "2026-08-19T18:00:00+08:00",
///     "rate": "60/min",
///     "confirm_required": ["form.submit"]
///   },
///   "parent": "cap_0087f1...",
///   "revoked": false
/// }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capability {
    pub cap_id: String, // TODO: not use string
    /// e.g. "portos:portos-echo/1" or "session:cli".
    /// TODO: not use string
    pub subject: String,
    /// e.g. "toy:echo" or "web-origin:https://example.com".
    /// TODO: not use string
    pub resource: String,
    pub verbs: BTreeSet<String>,
    pub constraints: Constraints,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default)]
    pub revoked: bool,
}

impl Capability {
    /// Attenuation legality. Child must narrow the parent in every field.
    /// Child verbs should be subset of parent ones.
    /// Child counts should less than or equal to parent ones pointwise.
    /// Child expiry should not later than parent's.
    pub fn is_valid_attenuation_of(&self, parent: &Capability) -> bool {
        // 1. Child verbs should be subset of parent ones.
        if !self.verbs.is_subset(&parent.verbs) {
            return false;
        }

        // 2. Child counts should less than or equal to parent ones pointwise.
        for (v, n) in &self.constraints.counts {
            match parent.constraints.counts.get(v) {
                Some(pn) if n <= pn => {}
                // parent unlimited on this verb, so any child bound is fine.
                // But the verb must at least be granted by the parent.
                None if parent.verbs.contains(v) => {}
                _ => return false,
            }
        }

        // 3. Child expiry should not later than parent's.
        match (self.constraints.expires_at, parent.constraints.expires_at) {
            (_, None) => true,
            (Some(c), Some(p)) => c <= p,
            (None, Some(_)) => false,
        }
    }
}
