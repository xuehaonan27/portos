//! Start, resume, expire and settle plan runs.

use super::{BufferState, Outcome, PlanService, RunControl, RunHandle, RunState};
use crate::{KernelError, consent::ConsentRecord, plancheck};
use portos_proto::Plan;
use portos_rm::time::Timestamp;
use rusqlite::params;
use serde_json::{Value, json};
use std::sync::Arc;

impl PlanService {
    /// Abort every run a previous process left *non-resumable*: the
    /// interpreter thread is gone, so `running` and `paused` rows are settled
    /// as crashed (buffer dropped, segment torn down). `admitted` runs are
    /// untouched (a consent may still start them) and `awaiting_approval`
    /// runs are untouched — their batch is durable precisely so a later
    /// process can approve it ([INSERT]).
    pub(super) fn recover(&self) {
        let now = crate::db::now_unix();
        let stale: Vec<String> = {
            let conn = self.kernel.db.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT run_id FROM plan_runs \
                     WHERE state IN ('running','paused')",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        for run_id in stale {
            let _ = self.set_buffer_state(&run_id, BufferState::Held, BufferState::Aborted);
            let seg = self.segment_of(&run_id).expect("registered plan segment");
            let _ = self.runtime.teardown_segment(
                &seg,
                Timestamp::try_from(now).expect("system timestamp in range"),
            );
            self.finish_row(&run_id, &Outcome::Aborted { expired: false });
            let _ = self.kernel.audit.lock().unwrap().append(json!({
                "event": "plan.aborted", "run": run_id, "crashed": true,
            }));
        }
    }
    // ---- run lifecycle ----

    /// Start an admitted run under a consent: verify the quadruple, mint the
    /// fiber's caps, and spawn the interpreter thread. The run then drives
    /// itself to a stable state (done / awaiting approval / paused).
    pub fn start(
        self: &Arc<Self>,
        run_id: &str,
        consent: &ConsentRecord,
    ) -> Result<(), KernelError> {
        // Hydrate a run admitted by an earlier process (e.g. `portos consent`)
        // from its durable row.
        if !self.runs.lock().unwrap().contains_key(run_id) {
            let row = self.load_run_row(run_id)?;
            if row.state != RunState::Admitted {
                return Err(KernelError::Denied(format!(
                    "plan run {run_id} not admitted (state {})",
                    row.state
                )));
            }
            self.runs.lock().unwrap().insert(
                run_id.to_string(),
                RunHandle {
                    plan_hash: row.plan_hash,
                    fiber: row.fiber,
                    nonce: String::new(),
                    original_ttl_at: 0,
                    state: RunState::Admitted,
                    ctrl: Arc::new(RunControl::default()),
                    thread: None,
                },
            );
        }
        let (plan_hash, fiber) = {
            let mut runs = self.runs.lock().unwrap();
            let run = runs
                .get_mut(run_id)
                .ok_or_else(|| KernelError::NotFound(format!("plan run: {run_id}")))?;
            if run.state != RunState::Admitted {
                return Err(KernelError::Denied(format!(
                    "plan run {run_id} not admitted (state {})",
                    run.state.as_str()
                )));
            }
            run.state = RunState::Running;
            run.nonce = consent.nonce.clone();
            run.original_ttl_at = consent.issued_at + consent.ttl_secs;
            (run.plan_hash.clone(), run.fiber.clone())
        };
        self.check_and_consume_consent(consent, &plan_hash, "signed")?;
        let bytes = self.plan_bytes(&plan_hash)?;
        let plan: Plan = Plan::from_bytes(&bytes)
            .map_err(|e| KernelError::Corrupt(format!("plan parse: {e}")))?;
        // Derived budget must fit inside what the user consented to.
        let adm = plancheck::admit(&plan, &self.runtime.schemas())
            .map_err(|e| KernelError::Denied(format!("admission: {e}")))?;
        for (verb, n) in &adm.budget {
            let allowed = consent.budget.get(verb).copied().unwrap_or(0);
            if allowed < *n {
                return Err(KernelError::Denied(format!(
                    "derived budget {verb}={n} exceeds consent {allowed}"
                )));
            }
        }
        self.mint_fiber_caps(&fiber, &plan, consent)?;
        {
            let conn = self.kernel.db.lock().unwrap();
            conn.execute(
                "UPDATE plan_runs SET state = 'running', nonce = ?2 WHERE run_id = ?1",
                params![run_id, consent.nonce],
            )?;
        }
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "plan.started", "run": run_id, "plan": plan_hash,
            "nonce": consent.nonce, "budget": consent.budget,
        }));
        self.emit(run_id, json!({"kind": "started", "budget": consent.budget}));

        let svc = self.clone();
        let id = run_id.to_string();
        let c = consent.clone();
        let handle = std::thread::spawn(move || svc.exec_thread(&id, plan, c));
        self.runs.lock().unwrap().get_mut(run_id).unwrap().thread = Some(handle);
        Ok(())
    }
    /// Resume an escalate-paused run with incremental consent ([ESC]): fresh
    /// quadruple, new pools, continue exactly from the pause point.
    /// In-process only — the interpreter thread holds the continuation.
    pub fn resume(&self, run_id: &str, consent: &ConsentRecord) -> Result<(), KernelError> {
        let ctrl = {
            let runs = self.runs.lock().unwrap();
            let run = runs
                .get(run_id)
                .ok_or_else(|| KernelError::NotFound(format!("plan run: {run_id}")))?;
            if run.state != RunState::Paused {
                return Err(KernelError::Denied(format!(
                    "plan run {run_id} is {}, not paused",
                    run.state.as_str()
                )));
            }
            if crate::db::now_unix() > run.original_ttl_at {
                return Err(KernelError::Denied(
                    "original consent expired: the segment can only be aborted".into(),
                ));
            }
            self.check_and_consume_consent(consent, &run.plan_hash, "signed")?;
            run.ctrl.clone()
        };
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "plan.resumed", "run": run_id, "nonce": consent.nonce,
        }));
        self.emit(run_id, json!({"kind": "resumed"}));
        ctrl.resume(consent.clone());
        Ok(())
    }

    /// Sweeper hook: expire every suspended run whose original consent is
    /// past ttl ([TTL]): drop the buffer, roll back the segment, the already
    /// emitted prefix stands.
    pub fn expire(&self, now: u64) {
        let expiring: Vec<(String, Arc<RunControl>, RunState)> = {
            let runs = self.runs.lock().unwrap();
            runs.iter()
                .filter(|(_, r)| {
                    matches!(r.state, RunState::AwaitingApproval | RunState::Paused)
                        && r.original_ttl_at > 0
                        && now > r.original_ttl_at
                })
                .map(|(id, r)| (id.clone(), r.ctrl.clone(), r.state))
                .collect()
        };
        for (run_id, ctrl, state) in expiring {
            ctrl.abort(true);
            // A Paused interpreter settles itself on wake; an
            // AwaitingApproval run has no thread — settle it here.
            if state == RunState::AwaitingApproval {
                self.settle_aborted(&run_id, true);
            }
        }
    }

    pub(super) fn settle_aborted(&self, run_id: &str, expired: bool) {
        let now = crate::db::now_unix();
        let _ = self.set_buffer_state(run_id, BufferState::Held, BufferState::Aborted);
        let seg = self.segment_of(run_id).expect("registered plan segment");
        let _ = self.runtime.teardown_segment(
            &seg,
            Timestamp::try_from(now).expect("system timestamp in range"),
        );
        self.finish_run(run_id, &Outcome::Aborted { expired }, 0);
    }

    /// Host drop: signal every interpreter to abort and join its thread.
    pub fn shutdown(&self) {
        let ctrls: Vec<Arc<RunControl>> = {
            let runs = self.runs.lock().unwrap();
            runs.values().map(|r| r.ctrl.clone()).collect()
        };
        for ctrl in &ctrls {
            ctrl.abort(false);
        }
        let threads: Vec<std::thread::JoinHandle<()>> = {
            let mut runs = self.runs.lock().unwrap();
            runs.values_mut().filter_map(|r| r.thread.take()).collect()
        };
        for t in threads {
            let _ = t.join();
        }
    }
    // ---- run helpers ----

    pub(super) fn set_state(&self, run_id: &str, state: RunState) {
        {
            let mut runs = self.runs.lock().unwrap();
            if let Some(r) = runs.get_mut(run_id) {
                r.state = state;
            }
        }
        let conn = self.kernel.db.lock().unwrap();
        let _ = conn.execute(
            "UPDATE plan_runs SET state = ?2 WHERE run_id = ?1",
            params![run_id, state.as_str()],
        );
    }

    pub(super) fn finish_run(&self, run_id: &str, outcome: &Outcome, seg_committed: usize) {
        {
            let mut runs = self.runs.lock().unwrap();
            if let Some(r) = runs.get_mut(run_id) {
                r.state = RunState::Done;
            }
        }
        self.finish_row(run_id, outcome);
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "plan.finished", "run": run_id,
            "status": outcome.status(),
            "outcome": serde_json::to_value(outcome).unwrap_or(Value::Null),
            "segment_committed": seg_committed,
        }));
        self.emit(
            run_id,
            json!({"kind": "finished", "status": outcome.status(), "outcome": outcome}),
        );
    }
}
