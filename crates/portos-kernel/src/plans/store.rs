//! Persist and query run states, withheld effects and emission records.

use super::{BufferState, Buffered, Outcome, PlanService, RunRow, RunState, WithheldEffect};
use crate::{KernelError, consent::ConsentRecord};
use portos_rm::identity::{SubjectId, VerbId};
use rusqlite::params;
use serde_json::Value;

impl PlanService {
    pub(super) fn segment_of(&self, run_id: &str) -> Result<SubjectId, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.query_row(
            "SELECT subject FROM plan_segments WHERE run_id=?1",
            params![run_id],
            |r| r.get::<_, String>(0),
        )
        .map(SubjectId::new)
        .map_err(|e| KernelError::Corrupt(format!("plan segment {run_id}: {e}")))
    }
    pub(super) fn finish_row(&self, run_id: &str, outcome: &Outcome) {
        let conn = self.kernel.db.lock().unwrap();
        let _ = conn.execute(
            "UPDATE plan_runs SET state = 'done', finished_at = ?2, outcome = ?3 WHERE run_id = ?1",
            params![
                run_id,
                crate::db::now_unix() as i64,
                serde_json::to_string(outcome).unwrap()
            ],
        );
    }
    pub(super) fn load_run_row(&self, run_id: &str) -> Result<RunRow, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let (plan_hash, fiber, nonce, state) = conn
            .query_row(
                "SELECT plan_hash, subject, nonce, state FROM plan_runs WHERE run_id = ?1",
                params![run_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .map_err(|_| KernelError::NotFound(format!("plan run: {run_id}")))?;
        Ok(RunRow {
            plan_hash,
            fiber: SubjectId::new(fiber),
            nonce,
            state: state.parse()?,
        })
    }

    pub(super) fn consent_ttl_at(&self, nonce: &str) -> Result<u64, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let json: String = conn
            .query_row(
                "SELECT json FROM consents WHERE nonce = ?1",
                params![nonce],
                |r| r.get(0),
            )
            .map_err(|_| KernelError::NotFound(format!("consent: {nonce}")))?;
        let rec: ConsentRecord = serde_json::from_str(&json)
            .map_err(|e| KernelError::Corrupt(format!("consent json: {e}")))?;
        Ok(rec.issued_at + rec.ttl_secs)
    }

    pub(super) fn load_buffer(&self, run_id: &str) -> Result<Vec<Buffered>, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, verb, target, args, cost FROM suppression_buffer \
             WHERE run_id = ?1 AND state = 'held' ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(Buffered {
                    seq: r.get::<_, i64>(0)? as u64,
                    verb: VerbId::new(r.get::<_, String>(1)?),
                    target: r.get(2)?,
                    args: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
                    cost: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(super) fn buffer_seq(&self, run_id: &str) -> u64 {
        let conn = self.kernel.db.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM suppression_buffer WHERE run_id = ?1",
            params![run_id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as u64
    }

    pub(super) fn push_buffer(&self, run_id: &str, item: &Buffered) -> Result<(), KernelError> {
        let seq = self.buffer_seq(run_id) + 1;
        let conn = self.kernel.db.lock().unwrap();
        conn.execute(
            "INSERT INTO suppression_buffer (run_id, seq, verb, target, args, cost) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id,
                seq as i64,
                item.verb.as_str(),
                item.target,
                serde_json::to_string(&item.args).unwrap(),
                item.cost as i64
            ],
        )?;
        Ok(())
    }

    pub(super) fn emission_count(&self, run_id: &str) -> Result<u64, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM emission_log WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    pub(super) fn log_emission(
        &self,
        run_id: &str,
        seq: u64,
        verb: &VerbId,
        target: &str,
        nonce: &str,
    ) -> Result<(), KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.execute(
            "INSERT INTO emission_log (run_id, seq, verb, target, nonce, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id,
                seq as i64,
                verb.as_str(),
                target,
                nonce,
                crate::db::now_unix() as i64
            ],
        )?;
        Ok(())
    }

    pub(super) fn set_buffer_state(
        &self,
        run_id: &str,
        from: BufferState,
        to: BufferState,
    ) -> Result<(), KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.execute(
            "UPDATE suppression_buffer SET state = ?3 WHERE run_id = ?1 AND state = ?2",
            params![run_id, from.as_str(), to.as_str()],
        )?;
        Ok(())
    }

    pub(super) fn mark_buffer(
        &self,
        run_id: &str,
        seq: u64,
        state: BufferState,
    ) -> Result<(), KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.execute(
            "UPDATE suppression_buffer SET state = ?3 WHERE run_id = ?1 AND seq = ?2",
            params![run_id, seq as i64, state.as_str()],
        )?;
        Ok(())
    }

    // ---- read-only queries for the CLI ----

    /// The latest run still in `admitted` state for a plan (so `run-plan`
    /// starts the run `consent` registered rather than duplicating it).
    pub fn admitted_run_for(&self, plan_hash: &str) -> Result<Option<String>, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let id: Option<String> = conn
            .query_row(
                "SELECT run_id FROM plan_runs WHERE plan_hash = ?1 AND state = 'admitted' \
                 ORDER BY started_at DESC LIMIT 1",
                params![plan_hash],
                |r| r.get(0),
            )
            .ok();
        Ok(id)
    }

    /// The plan hash of a run (CLI approve).
    pub fn run_plan_hash(&self, run_id: &str) -> Result<String, KernelError> {
        Ok(self.load_run_row(run_id)?.plan_hash)
    }

    /// The state of a run ("admitted" | "running" | "awaiting_approval" |
    /// "paused" | "done"), from the durable row.
    pub fn run_state(&self, run_id: &str) -> Result<RunState, KernelError> {
        Ok(self.load_run_row(run_id)?.state)
    }

    /// The terminal outcome of a finished run, from the durable row.
    pub fn run_outcome(&self, run_id: &str) -> Result<Option<Outcome>, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let json: Option<String> = conn
            .query_row(
                "SELECT outcome FROM plan_runs WHERE run_id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        match json {
            Some(j) => Ok(serde_json::from_str(&j).ok()),
            None => Ok(None),
        }
    }

    /// The withheld batch still held, in release order (CLI approve renders
    /// this), with named verb, target and cost fields.
    pub fn withheld_batch(&self, run_id: &str) -> Result<Vec<WithheldEffect>, KernelError> {
        Ok(self
            .load_buffer(run_id)?
            .into_iter()
            .map(|b| WithheldEffect {
                verb: b.verb,
                target: b.target,
                cost: b.cost,
            })
            .collect())
    }
}
