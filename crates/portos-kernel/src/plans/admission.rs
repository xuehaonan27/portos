//! Admit plans, verify consent and mint the run's capability budgets.

use super::{PlanService, RunControl, RunHandle, RunState, SubmitOut};
use crate::{
    Kernel, KernelError,
    consent::{ConsentRecord, render_budget},
    plancheck,
};
use portos_proto::{Expr, Guard, Label, Plan, Stmt, artifact::id_for_bytes, cap::Constraints};
use portos_rm::identity::SubjectId;
use rusqlite::params;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

impl PlanService {
    // ---- admission (submit) ----

    /// CAS the plan bytes, admit them (three passes + demand ⊆ the submitting
    /// subject's live grants for plugin subjects), and register the run in
    /// `admitted` state. A submission never executes anything.
    pub fn submit(&self, subject: &str, bytes: &[u8]) -> Result<SubmitOut, KernelError> {
        let plan_hash = id_for_bytes(bytes);
        let plan: Plan = Plan::from_bytes(bytes)
            .map_err(|e| KernelError::Corrupt(format!("plan parse: {e}")))?;
        let adm = plancheck::admit(&plan, &self.runtime.schemas())
            .map_err(|e| KernelError::Denied(format!("admission: {e}")))?;
        // F5: the plan's whole verb demand (effects and reads) must fit the
        // submitting subject's live grants. CLI runs submit as "user" — root
        // authority, no grants check.
        if subject != "user" {
            let demand = verb_demand(&plan);
            let grants = live_grant_verbs(&self.kernel, subject)?;
            let missing: Vec<String> = demand
                .iter()
                .filter(|v| !grants.contains(*v))
                .cloned()
                .collect();
            if !missing.is_empty() {
                let _ = self.kernel.audit.lock().unwrap().append(json!({
                    "event": "plan.submit_denied", "from": subject,
                    "reason": format!("demand exceeds grants: {missing:?}"),
                }));
                return Err(KernelError::Denied(format!(
                    "plan verbs outside the submitter's grants: {missing:?}"
                )));
            }
        }
        self.kernel
            .cas
            .put_bytes(bytes, "portos/plan", Label::public_trusted(), "plans")?;
        let run_id = format!("run_{}", hex_short());
        let fiber = SubjectId::new(format!("plan:{plan_hash}#{run_id}"));
        {
            let mut conn = self.kernel.db.lock().unwrap();
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO plan_runs (run_id, plan_hash, subject, nonce, state, started_at) \
                 VALUES (?1, ?2, ?3, '', 'admitted', ?4)",
                params![
                    run_id,
                    plan_hash,
                    fiber.as_str(),
                    crate::db::now_unix() as i64
                ],
            )?;
            tx.execute(
                "INSERT INTO plan_segments (run_id, subject) VALUES (?1, ?2)",
                params![run_id, format!("{fiber}:seg")],
            )?;
            tx.commit()?;
        }
        self.runs.lock().unwrap().insert(
            run_id.clone(),
            RunHandle {
                plan_hash: plan_hash.clone(),
                fiber,
                nonce: String::new(),
                original_ttl_at: 0,
                state: RunState::Admitted,
                ctrl: Arc::new(RunControl::default()),
                thread: None,
            },
        );
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "plan.admitted", "run": run_id, "plan": plan_hash,
            "subject": subject, "budget": adm.budget,
            "worst_case_steps": adm.worst_case_steps,
        }));
        Ok(SubmitOut {
            run_id: run_id.clone(),
            plan_hash: plan_hash.clone(),
            rendering: render_budget(&plan_hash, &adm.budget),
            budget: adm.budget,
        })
    }

    // ---- consent checks & fiber caps ----

    /// Verify a quadruple for this plan: MAC + ttl, plan-hash equality, nonce
    /// freshness — and consume the nonce (a replay is a primary-key conflict
    /// = StaleNonce). The original consent's ttl bounds both suspended states.
    pub(super) fn check_and_consume_consent(
        &self,
        consent: &ConsentRecord,
        plan_hash: &str,
        source: &str,
    ) -> Result<(), KernelError> {
        let now = crate::db::now_unix();
        consent.verify(&self.kernel.consent_key, now)?;
        if consent.plan_hash != plan_hash {
            return Err(KernelError::Denied(format!(
                "consent/plan mismatch: {} vs {plan_hash}",
                consent.plan_hash
            )));
        }
        let conn = self.kernel.db.lock().unwrap();
        let json = serde_json::to_string(consent).unwrap();
        match conn.execute(
            "INSERT INTO consents (nonce, json, created_at, source) VALUES (?1, ?2, ?3, ?4)",
            params![consent.nonce, json, now as i64, source],
        ) {
            Ok(_) => Ok(()),
            Err(_) => Err(KernelError::Denied(format!(
                "stale nonce: {}",
                consent.nonce
            ))),
        }
    }

    /// Mint the fiber's capabilities: one per family used by the plan,
    /// verbs = the plan's effect and read verbs, counts = the consent's
    /// per-class budgets, expiry = consent ttl. Reads mint uncounted (free
    /// per the truth table).
    pub(super) fn mint_fiber_caps(
        &self,
        fiber: &SubjectId,
        plan: &Plan,
        consent: &ConsentRecord,
    ) -> Result<(), KernelError> {
        let ttl_at = consent.issued_at + consent.ttl_secs;
        let mut by_family: BTreeMap<String, (Vec<String>, BTreeMap<String, u64>)> = BTreeMap::new();
        for verb in verb_demand(plan) {
            let (family, short) = split_verb(&verb);
            let (verbs, counts) = by_family.entry(family.to_string()).or_default();
            verbs.push(short.to_string());
            if let Some(n) = consent.budget.get(&verb) {
                counts.insert(short.to_string(), *n);
            }
        }
        for (family, (verbs, counts)) in by_family {
            self.kernel.caps.mint(
                fiber.as_str(),
                &format!("driver:{family}"),
                verbs.into_iter().collect(),
                Constraints {
                    expires_at: Some(ttl_at),
                    counts,
                },
                None,
            )?;
        }
        Ok(())
    }
    pub(super) fn plan_bytes(&self, plan_hash: &str) -> Result<Vec<u8>, KernelError> {
        let mut out = Vec::new();
        let mut reader = self.kernel.cas.open_read(&plan_hash.to_string())?;
        use std::io::Read;
        reader.read_to_end(&mut out)?;
        Ok(out)
    }
}

/// Every verb a plan may reach (effects and reads), as `family::verb` strings.
fn verb_demand(plan: &Plan) -> Vec<String> {
    fn of_stmts(stmts: &[Stmt], out: &mut Vec<String>) {
        for s in stmts {
            match s {
                Stmt::Effect { verb, .. } => out.push(verb.clone()),
                Stmt::If {
                    guard,
                    then_,
                    else_,
                } => {
                    of_guard(guard, out);
                    of_stmts(then_, out);
                    of_stmts(else_, out);
                }
                Stmt::Foreach { list, body, .. } => {
                    of_expr(list, out);
                    of_stmts(body, out);
                }
                Stmt::Let { expr, .. } => of_expr(expr, out),
            }
        }
    }
    fn of_expr(e: &Expr, out: &mut Vec<String>) {
        match e {
            Expr::Observe { verb, args } => {
                out.push(verb.clone());
                for a in args {
                    of_expr(a, out);
                }
            }
            Expr::Pure { args, .. } => {
                // Pure computation calls the compute plugin: the demand
                // includes it (admission's grants check and the fiber's caps).
                out.push("compute::run".to_string());
                for a in args {
                    of_expr(a, out);
                }
            }
            Expr::Index { base, .. } => of_expr(base, out),
            Expr::Const { .. } | Expr::Var { .. } => {}
        }
    }
    fn of_guard(g: &Guard, out: &mut Vec<String>) {
        match g {
            Guard::Exists { expr } => of_expr(expr, out),
            Guard::Matches { expr, .. } => of_expr(expr, out),
            Guard::Cmp { lhs, rhs, .. } => {
                of_expr(lhs, out);
                of_expr(rhs, out);
            }
            Guard::And { l, r } | Guard::Or { l, r } => {
                of_guard(l, out);
                of_guard(r, out);
            }
            Guard::Not { g } => of_guard(g, out),
        }
    }
    let mut out = Vec::new();
    of_stmts(&plan.stmts, &mut out);
    out.sort();
    out.dedup();
    out
}

/// The submitting subject's live grants as `family::verb` strings.
fn live_grant_verbs(
    kernel: &Kernel,
    subject: &str,
) -> Result<std::collections::BTreeSet<String>, KernelError> {
    let caps = kernel.caps.list_live(subject, crate::db::now_unix())?;
    let mut out = std::collections::BTreeSet::new();
    for cap in &caps {
        if let Some(family) = cap.resource.strip_prefix("driver:") {
            for short in &cap.verbs {
                out.insert(format!("{family}::{short}"));
            }
        }
    }
    Ok(out)
}

pub(super) fn split_verb(verb: &str) -> (&str, &str) {
    let family = verb.split("::").next().unwrap_or(verb);
    let short = verb.rsplit("::").next().unwrap_or(verb);
    (family, short)
}

fn hex_short() -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}
