//! Host services required by plan interpretation and durable batch approval.

use crate::{KernelError, plancheck::VerbSchemas};
use portos_rm::identity::{SubjectId, VerbId};
use portos_rm::time::Timestamp;
use serde_json::Value;

/// Policy for one evaluated effect, read from its serving handler.
pub struct EffectPolicy {
    pub target: String,
    pub withhold: bool,
}

/// Implementations own routing, capability enforcement, plugin protocols and
/// physical cleanup. Plan execution never accesses the host's registry.
pub trait PlanRuntime: Send + Sync {
    fn schemas(&self) -> VerbSchemas;

    fn effect_policy(&self, verb: &VerbId, args: &Value) -> EffectPolicy;

    /// Enforce the caller's capability, budget and the handler's protocol.
    /// Budget exhaustion and missing grants use their typed `KernelError`
    /// variants; plugin failures must not be classified by their message text.
    fn invoke(&self, subject: &SubjectId, verb: &VerbId, args: Value)
    -> Result<Value, KernelError>;

    fn emit(&self, topic: &str, data: Value);

    /// Request and drive durable cleanup. Success does not imply that every
    /// physical resource is gone; unresolved cleanup obligations remain.
    fn teardown_segment(&self, subject: &SubjectId, now: Timestamp) -> Result<(), KernelError>;
}
