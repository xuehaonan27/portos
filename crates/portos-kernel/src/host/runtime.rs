//! Connect the plan runtime interface to this host's routing and cleanup.

use super::{HostInner, cleanup::HostWorld, events::dispatch_event, routing};
use crate::{
    Kernel, KernelError,
    plancheck::VerbSchemas,
    plans::{EffectPolicy, PlanRuntime},
};
use portos_rm::identity::{SubjectId, VerbId};
use portos_rm::time::Timestamp;
use serde_json::Value;
use std::sync::Arc;

pub(super) struct HostRuntime {
    pub(super) kernel: Arc<Kernel>,
    pub(super) inner: Arc<HostInner>,
}

impl PlanRuntime for HostRuntime {
    fn schemas(&self) -> VerbSchemas {
        routing::kernel_schemas(&self.inner)
    }

    fn effect_policy(&self, verb: &VerbId, args: &Value) -> EffectPolicy {
        routing::effect_policy(&self.inner, verb.as_str(), args)
    }

    fn invoke(
        &self,
        subject: &SubjectId,
        verb: &VerbId,
        args: Value,
    ) -> Result<Value, KernelError> {
        routing::invoke_as(
            &self.kernel,
            &self.inner,
            subject.as_str(),
            verb.as_str(),
            args,
        )
    }

    fn emit(&self, topic: &str, data: Value) {
        dispatch_event(&self.kernel, &self.inner, topic, data);
    }

    fn teardown_segment(&self, subject: &SubjectId, now: Timestamp) -> Result<(), KernelError> {
        self.kernel
            .ledger
            .teardown(
                subject,
                &mut HostWorld {
                    inner: self.inner.clone(),
                },
                now,
            )
            .map(|_| ())
    }
}
