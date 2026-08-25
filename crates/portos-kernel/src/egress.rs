//! Egress proxy
//! 
//! ## Documentation
//! ### docs/architecture-v0.md §5.7
//! The plugin default contract is NO direct network. All egress flows
//! through this chokepoint, which is simultaneously the credential
//! injection point, the taint sink check, and the outbound audit point.
//! M0 has no networked plugins, so the trait exists to keep the shape and
//! the default is deny-all. M1 implements it for real.

use portos_proto::Label;

pub enum EgressDecision {
    Deny(String),
    #[allow(dead_code)]
    Allow,
}

pub trait EgressPolicy: Send {
    fn check(&self, destination: &str, payload_label: &Label) -> EgressDecision;
}

/// M0 default: nothing leaves.
pub struct DenyAll;

impl EgressPolicy for DenyAll {
    fn check(&self, destination: &str, _payload_label: &Label) -> EgressDecision {
        EgressDecision::Deny(format!("M0 egress is deny-all (dest: {destination})"))
    }
}
