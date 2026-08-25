//! protos-proto: shared types for PortOS.

pub mod artifact;
pub mod cap;
pub mod fdpass;
pub mod frame;
pub mod label;
pub mod plan;

pub use artifact::{ArtifactId, ArtifactMeta};
pub use cap::{Capability, Constraints};
pub use label::Label;
pub use plan::{CmpOp, Expr, Guard, Mode, Plan, Stmt};

/// An unforgeable reference minted by the kernel. Bound to a subject, so
/// leaking the string to another plugin is useless. The kernel-side table
/// maps (handle, subject ) => rights. Transfer requires re-minting.
///
/// TODO: the internal layout of handle might change.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Handle(pub String);

impl Handle {
    /// Mint a handle to resource
    pub fn mint() -> Self {
        use rand::RngCore;
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        Handle(format!("h:{}", hex::encode(b)))
    }
}
