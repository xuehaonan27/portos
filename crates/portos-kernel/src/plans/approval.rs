//! Release a durable withheld batch under fresh consent.

use super::{BufferState, Outcome, PlanService, RunState, admission::split_verb};
use crate::{KernelError, consent::ConsentRecord};
use portos_proto::cap::Constraints;
use portos_rm::time::Timestamp;
use serde_json::json;
use std::collections::BTreeMap;

impl PlanService {
    /// Approve a withheld batch with a fresh quadruple ([INSERT]): verify,
    /// check the approval budget covers the whole batch (transaction shape),
    /// release in original order exactly once, commit the segment. Works
    /// cross-process: everything needed is durable.
    pub fn approve(&self, run_id: &str, consent: &ConsentRecord) -> Result<(), KernelError> {
        let row = self.load_run_row(run_id)?;
        if row.state != RunState::AwaitingApproval {
            return Err(KernelError::Denied(format!(
                "plan run {run_id} is {}, not awaiting approval",
                row.state
            )));
        }
        let original_ttl_at = self.consent_ttl_at(&row.nonce)?;
        if crate::db::now_unix() > original_ttl_at {
            return Err(KernelError::Denied(
                "original consent expired: the segment can only be aborted".into(),
            ));
        }
        self.check_and_consume_consent(consent, &row.plan_hash, "signed")?;
        let batch = self.load_buffer(run_id)?;
        // Transaction shape: the approval budget must cover the whole batch.
        let mut need: BTreeMap<String, u64> = BTreeMap::new();
        for item in &batch {
            *need.entry(item.verb.to_string()).or_insert(0) += item.cost;
        }
        for (verb, n) in &need {
            let allowed = consent.budget.get(verb).copied().unwrap_or(0);
            if allowed < *n {
                return Err(KernelError::Denied(format!(
                    "approval budget short: {verb} needs {n}, got {allowed}"
                )));
            }
        }
        // Fresh caps for the batch under the fiber (counts from the approval).
        let mut by_family: BTreeMap<String, (Vec<String>, BTreeMap<String, u64>)> = BTreeMap::new();
        for (verb, n) in &need {
            let (family, short) = split_verb(verb);
            let (verbs, counts) = by_family.entry(family.to_string()).or_default();
            verbs.push(short.to_string());
            counts.insert(short.to_string(), *n);
        }
        let ttl_at = consent.issued_at + consent.ttl_secs;
        for (family, (verbs, counts)) in by_family {
            self.kernel.caps.mint(
                row.fiber.as_str(),
                &format!("driver:{family}"),
                verbs.into_iter().collect(),
                Constraints {
                    expires_at: Some(ttl_at),
                    counts,
                },
                None,
            )?;
        }
        // Release in original order, each item marked as it goes — a crash
        // mid-batch leaves the rest held for a later approval, never a
        // duplicate emission (exactly-once = at-least-once + per-item journal).
        let mut seq = self.emission_count(run_id)?;
        let mut released = 0usize;
        let mut failure: Option<String> = None;
        for item in &batch {
            match self
                .runtime
                .invoke(&row.fiber, &item.verb, item.args.clone())
            {
                Ok(_) => {
                    seq += 1;
                    self.log_emission(run_id, seq, &item.verb, &item.target, &consent.nonce)?;
                    self.mark_buffer(run_id, item.seq, BufferState::Inserted)?;
                    self.emit(
                        run_id,
                        json!({"kind": "effect", "verb": item.verb.as_str(), "target": item.target, "ok": true}),
                    );
                    released += 1;
                }
                Err(e) => {
                    failure = Some(format!("{}: {e}", item.verb));
                    break;
                }
            }
        }
        if let Some(why) = failure {
            let _ = self.kernel.audit.lock().unwrap().append(json!({
                "event": "plan.approve_failed", "run": run_id, "released": released, "error": why,
            }));
            let _ = self.set_buffer_state(run_id, BufferState::Held, BufferState::Aborted);
            let seg = self.segment_of(run_id)?;
            let _ = self.runtime.teardown_segment(
                &seg,
                Timestamp::try_from(crate::db::now_unix()).expect("system timestamp in range"),
            );
            self.finish_run(run_id, &Outcome::FailStop { at: why.clone() }, 0);
            return Err(KernelError::Denied(format!(
                "approve release failed: {why}"
            )));
        }
        self.set_buffer_state(run_id, BufferState::Held, BufferState::Inserted)?;
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "plan.approved", "run": run_id, "nonce": consent.nonce,
            "released": batch.len(),
        }));
        self.emit(run_id, json!({"kind": "approved", "released": batch.len()}));
        // [SEG-TX] approval = commit.
        let seg = self.segment_of(run_id)?;
        let moved = self.kernel.ledger.transfer_all(&seg, &row.fiber)?;
        self.finish_run(run_id, &Outcome::Completed, moved);
        Ok(())
    }
}
