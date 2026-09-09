//! A plan runtime can be supplied without creating a plugin host.

use portos_kernel::plans::{EffectPolicy, PlanRuntime, PlanService, RunState};
use portos_kernel::{Kernel, KernelError, plancheck::VerbSchemas};
use portos_rm::identity::{SubjectId, VerbId};
use portos_rm::time::Timestamp;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

enum Failure {
    PluginMessage,
    Budget,
}

struct Runtime {
    failure: Failure,
    calls: Mutex<Vec<(SubjectId, VerbId)>>,
}

impl PlanRuntime for Runtime {
    fn schemas(&self) -> VerbSchemas {
        VerbSchemas {
            observe: Default::default(),
            external_effects: [("fixture::emit".into(), true)].into(),
        }
    }

    fn effect_policy(&self, _: &VerbId, _: &Value) -> EffectPolicy {
        EffectPolicy {
            target: "fixture".into(),
            withhold: false,
        }
    }

    fn invoke(&self, subject: &SubjectId, verb: &VerbId, _: Value) -> Result<Value, KernelError> {
        self.calls
            .lock()
            .unwrap()
            .push((subject.clone(), verb.clone()));
        Err(match self.failure {
            Failure::PluginMessage => KernelError::Denied("plugin error: budget exhausted".into()),
            Failure::Budget => KernelError::BudgetExhausted(verb.clone()),
        })
    }

    fn emit(&self, _: &str, _: Value) {}

    fn teardown_segment(&self, _: &SubjectId, _: Timestamp) -> Result<(), KernelError> {
        Ok(()) // This fixture creates no physical resources.
    }
}

#[test]
fn runtime_boundary_distinguishes_budget_exhaustion_from_plugin_error_text() {
    for (tag, failure, expected) in [
        ("plugin", Failure::PluginMessage, RunState::Done),
        ("budget", Failure::Budget, RunState::Paused),
    ] {
        let root =
            std::env::temp_dir().join(format!("portos-runtime-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let kernel = Arc::new(Kernel::open(&root).unwrap());
        let runtime = Arc::new(Runtime {
            failure,
            calls: Mutex::new(Vec::new()),
        });
        let service = PlanService::new(kernel.clone(), runtime.clone());
        let plan = serde_json::to_vec(&json!({"stmts": [{
            "k": "foreach", "var": "x", "list": {"k": "const", "value": [1]},
            "bound": 1, "mode": "escalate",
            "body": [{"k": "effect", "verb": "fixture::emit", "args": []}],
        }]}))
        .unwrap();
        let run = service.submit("user", &plan).unwrap();
        let consent = portos_signer::Signer::load(&root).unwrap().sign(
            &run.plan_hash,
            [("fixture::emit".into(), 1)].into(),
            60,
        );
        service.start(&run.run_id, &consent).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let state = loop {
            let state = service.run_state(&run.run_id).unwrap();
            if matches!(state, RunState::Done | RunState::Paused)
                || std::time::Instant::now() > deadline
            {
                break state;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        service.shutdown();
        assert_eq!(state, expected);
        assert_eq!(
            *runtime.calls.lock().unwrap(),
            vec![(
                SubjectId::new(format!("plan:{}#{}", run.plan_hash, run.run_id)),
                VerbId::new("fixture::emit"),
            )]
        );
        drop(service);
        drop(kernel);
        std::fs::remove_dir_all(root).unwrap();
    }
}
