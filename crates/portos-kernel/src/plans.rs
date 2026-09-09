//! Plan admission, consent and execution through an explicit runtime interface.
//!
//! The service owns run coordination and durable state. `PlanRuntime` owns
//! plugin invocation, effect policy, event delivery and physical cleanup.
//! Interpreter threads resume in-process; after a restart, running and paused
//! runs abort while durable withheld batches remain available for approval.
//!
//! Segment identity comes from storage. Plugin-created holdings and resource
//! checkpoints are not yet attached to these segments by the interpreter.

mod admission;
mod approval;
mod control;
mod interpreter;
mod lifecycle;
mod runtime;
mod store;
mod types;

use crate::Kernel;
use control::{RunControl, RunSignal};
pub use runtime::{EffectPolicy, PlanRuntime};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use types::{BufferState, Buffered, RunHandle, RunRow};
pub use types::{Outcome, RunState, SubmitOut, WithheldEffect};

/// The plan-run registry. Self-contained (kernel + runtime interface), so the
/// sweeper, a CLI process, or a `Host` can all operate it.
pub struct PlanService {
    kernel: Arc<Kernel>,
    runtime: Arc<dyn PlanRuntime>,
    runs: Mutex<BTreeMap<String, RunHandle>>,
}

impl PlanService {
    pub fn new(kernel: Arc<Kernel>, runtime: Arc<dyn PlanRuntime>) -> Arc<PlanService> {
        let svc = PlanService {
            kernel,
            runtime,
            runs: Mutex::new(BTreeMap::new()),
        };
        svc.recover();
        Arc::new(svc)
    }

    fn emit(&self, run_id: &str, data: Value) {
        self.runtime.emit(&format!("plan::run::{run_id}"), data);
    }
    fn audit(&self, run_id: &str, event: &str, extra: Value) {
        let mut body = json!({ "event": event, "run": run_id });
        if let (Some(a), Some(b)) = (body.as_object_mut(), extra.as_object()) {
            a.extend(b.clone());
        }
        let _ = self.kernel.audit.lock().unwrap().append(body);
    }
}

#[cfg(test)]
mod m3_tests;
