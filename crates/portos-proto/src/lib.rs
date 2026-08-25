//! protos-proto: shared types for PortOS.

pub mod artifact;
pub mod cap;
pub mod chunk;
pub mod frame;
pub mod label;
pub mod plan;

/// Wire ABI version, sent in every hello. ABI v2 (decisions-v1.md D23–D25,
/// D29): two connections per plugin (serve + client roles), plugin→kernel
/// `invoke`, event frames, chunked artifact streaming; fd passing removed.
pub const ABI_VERSION: &str = "0.2";

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
