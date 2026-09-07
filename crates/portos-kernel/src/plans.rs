//! Plan runs in the kernel (WP-06): admission → consent (WYSIWYS) → run
//! under the F3 monitor → prefix delivery.
//!
//! The semantics are the law crate's F3 monitor (`portos_rm::monitor`,
//! unchanged) rebuilt on the real kernel:
//!
//!   - **Budget is rows** ([GATE]): a consent mints one capability per family
//!     for the fiber `plan:<h_plan>#<run>` (counts = the consented per-class
//!     budgets, expiry = consent ttl); every effect spends through the issuer
//!     gate (`find_and_exercise` → pool row under `<fiber>:spent`). There is
//!     no subtraction anywhere.
//!   - **The monitor pipeline** per effect: sink re-check → staged (withhold
//!     the hard list: emitting ∧ non-amortizable) → protocol (enforced at the
//!     serving plugin) → budget. Withheld effects land in `suppression_buffer`
//!     fully evaluated, so an approval from a *later process* can release the
//!     batch ([INSERT]).
//!   - **Three modes** on bound/budget excess: strict fail-stops, truncate
//!     processes the first N and reports (never silent), escalate pauses and
//!     waits for incremental consent. Suspended states (AwaitingApproval,
//!     Paused) are bounded by the *original* consent's ttl ([TTL]); the
//!     sweeper expires them.
//!   - **Segment = transaction** ([SEG-TX]): holdings the run acquires are
//!     booked under `<fiber>:seg`; commit transfers them to the fiber, any
//!     other terminal state tears the segment down (the F2 path).
//!
//! Escalate resume is in-process (the interpreter thread parks on a condvar).
//! A crashed kernel's suspended runs are aborted at open (`recover`), which
//! matches the drill's crash model (buffer loss = degraded abort).

use crate::consent::{ConsentRecord, render_budget};
use crate::host::{HostInner, HostWorld, dispatch_event, invoke_as, kernel_schemas};
use crate::plancheck::{self, VerbSchemas};
use crate::{Kernel, KernelError};
use portos_proto::cap::Constraints;
use portos_proto::{CmpOp, Expr, Guard, Label, Mode, Plan, Stmt, artifact::id_for_bytes};
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};

/// What a submission returns: the admitted run's id, the plan's CAS id, the
/// deterministic rendering (WYSIWYS), and the derived budget it renders.
pub struct SubmitOut {
    pub run_id: String,
    pub plan_hash: String,
    pub rendering: String,
    pub budget: BTreeMap<String, u64>,
}

/// Terminal outcome of a run (persisted as JSON).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Outcome {
    Completed,
    FailStop { at: String },
    Truncated { dropped: usize },
    Aborted { expired: bool },
}

impl Outcome {
    pub fn status(&self) -> String {
        match self {
            Outcome::Completed => "Completed".into(),
            Outcome::FailStop { .. } => "FailStop".into(),
            Outcome::Truncated { .. } => "Truncated".into(),
            Outcome::Aborted { .. } => "Aborted".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunState {
    Admitted,
    Running,
    AwaitingApproval,
    Paused,
    Done,
}

impl RunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Admitted => "admitted",
            RunState::Running => "running",
            RunState::AwaitingApproval => "awaiting_approval",
            RunState::Paused => "paused",
            RunState::Done => "done",
        }
    }
}

/// Coordination for a parked (Paused) interpreter thread.
#[derive(Default)]
struct Ctrl {
    resume: Option<ConsentRecord>,
    abort: Option<bool>, // Some(expired) — settle as aborted
}

struct RunHandle {
    plan_hash: String,
    fiber: String,
    nonce: String,
    original_ttl_at: u64,
    state: RunState,
    ctrl: Arc<(Mutex<Ctrl>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The plan-run registry. Self-contained (kernel + host inner), so the
/// sweeper, a CLI process, or a `Host` can all operate it.
pub struct PlanService {
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
    runs: Arc<Mutex<BTreeMap<String, RunHandle>>>,
}

struct RunRow {
    plan_hash: String,
    subject: String,
    nonce: String,
    state: String,
}

#[derive(Clone)]
struct Buffered {
    seq: u64,
    verb: String,
    target: String,
    args: Value,
    cost: u64,
}

impl PlanService {
    pub(crate) fn new(kernel: Arc<Kernel>, inner: Arc<HostInner>) -> Arc<PlanService> {
        let svc = PlanService {
            kernel,
            inner,
            runs: Arc::new(Mutex::new(BTreeMap::new())),
        };
        svc.recover();
        Arc::new(svc)
    }

    fn seg_of(fiber: &str) -> String {
        format!("{fiber}:seg")
    }

    /// Abort every run a previous process left *non-resumable*: the
    /// interpreter thread is gone, so `running` and `paused` rows are settled
    /// as crashed (buffer dropped, segment torn down). `admitted` runs are
    /// untouched (a consent may still start them) and `awaiting_approval`
    /// runs are untouched — their batch is durable precisely so a later
    /// process can approve it ([INSERT]).
    fn recover(&self) {
        let now = crate::db::now_unix();
        let stale: Vec<(String, String)> = {
            let conn = self.kernel.db.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT run_id, subject FROM plan_runs \
                     WHERE state IN ('running','paused')",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        for (run_id, fiber) in stale {
            let _ = self.set_buffer_state(&run_id, "held", "aborted");
            let seg = Self::seg_of(&fiber);
            let mut world = HostWorld { inner: self.inner.clone() };
            let _ = self.kernel.ledger.teardown(&seg, &mut world, now);
            self.finish_row(&run_id, &Outcome::Aborted { expired: false });
            let _ = self.kernel.audit.lock().unwrap().append(json!({
                "event": "plan.aborted", "run": run_id, "crashed": true,
            }));
        }
    }

    // ---- admission (submit) ----

    /// CAS the plan bytes, admit them (three passes + demand ⊆ the submitting
    /// subject's live grants for plugin subjects), and register the run in
    /// `admitted` state. A submission never executes anything.
    pub fn submit(&self, subject: &str, bytes: &[u8]) -> Result<SubmitOut, KernelError> {
        let plan_hash = id_for_bytes(bytes);
        let plan: Plan = Plan::from_bytes(bytes)
            .map_err(|e| KernelError::Corrupt(format!("plan parse: {e}")))?;
        let adm = plancheck::admit(&plan, &self.kernel_schemas())
            .map_err(|e| KernelError::Denied(format!("admission: {e}")))?;
        // F5: the plan's whole verb demand (effects and reads) must fit the
        // submitting subject's live grants. CLI runs submit as "user" — root
        // authority, no grants check.
        if subject != "user" {
            let demand = verb_demand(&plan);
            let grants = live_grant_verbs(&self.kernel, subject)?;
            let missing: Vec<String> =
                demand.iter().filter(|v| !grants.contains(*v)).cloned().collect();
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
        let fiber = format!("plan:{plan_hash}#{run_id}");
        {
            let conn = self.kernel.db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_runs (run_id, plan_hash, subject, nonce, state, started_at) \
                 VALUES (?1, ?2, ?3, '', 'admitted', ?4)",
                params![run_id, plan_hash, fiber, crate::db::now_unix() as i64],
            )?;
        }
        self.runs.lock().unwrap().insert(
            run_id.clone(),
            RunHandle {
                plan_hash: plan_hash.clone(),
                fiber,
                nonce: String::new(),
                original_ttl_at: 0,
                state: RunState::Admitted,
                ctrl: Arc::new((Mutex::new(Ctrl::default()), Condvar::new())),
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
    fn check_and_consume_consent(
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
            Err(_) => Err(KernelError::Denied(format!("stale nonce: {}", consent.nonce))),
        }
    }

    /// Mint the fiber's capabilities: one per family used by the plan,
    /// verbs = the plan's effect and read verbs, counts = the consent's
    /// per-class budgets, expiry = consent ttl. Reads mint uncounted (free
    /// per the truth table).
    fn mint_fiber_caps(
        &self,
        fiber: &str,
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
                fiber,
                &format!("driver:{family}"),
                verbs.into_iter().collect(),
                Constraints { expires_at: Some(ttl_at), counts },
                None,
            )?;
        }
        Ok(())
    }

    // ---- run lifecycle ----

    /// Start an admitted run under a consent: verify the quadruple, mint the
    /// fiber's caps, and spawn the interpreter thread. The run then drives
    /// itself to a stable state (done / awaiting approval / paused).
    pub fn start(self: &Arc<Self>, run_id: &str, consent: &ConsentRecord) -> Result<(), KernelError> {
        // Hydrate a run admitted by an earlier process (e.g. `portos consent`)
        // from its durable row.
        if !self.runs.lock().unwrap().contains_key(run_id) {
            let row = self.load_run_row(run_id)?;
            if row.state != "admitted" {
                return Err(KernelError::Denied(format!(
                    "plan run {run_id} not admitted (state {})",
                    row.state
                )));
            }
            self.runs.lock().unwrap().insert(
                run_id.to_string(),
                RunHandle {
                    plan_hash: row.plan_hash,
                    fiber: row.subject,
                    nonce: String::new(),
                    original_ttl_at: 0,
                    state: RunState::Admitted,
                    ctrl: Arc::new((Mutex::new(Ctrl::default()), Condvar::new())),
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
        let adm = plancheck::admit(&plan, &self.kernel_schemas())
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

    /// Approve a withheld batch with a fresh quadruple ([INSERT]): verify,
    /// check the approval budget covers the whole batch (transaction shape),
    /// release in original order exactly once, commit the segment. Works
    /// cross-process: everything needed is durable.
    pub fn approve(&self, run_id: &str, consent: &ConsentRecord) -> Result<(), KernelError> {
        let row = self.load_run_row(run_id)?;
        if row.state != "awaiting_approval" {
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
            *need.entry(item.verb.clone()).or_insert(0) += item.cost;
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
                &row.subject,
                &format!("driver:{family}"),
                verbs.into_iter().collect(),
                Constraints { expires_at: Some(ttl_at), counts },
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
            match invoke_as(&self.kernel, &self.inner, &row.subject, &item.verb, item.args.clone()) {
                Ok(_) => {
                    seq += 1;
                    self.log_emission(run_id, seq, &item.verb, &item.target, &consent.nonce)?;
                    self.mark_buffer(run_id, item.seq, "inserted")?;
                    self.emit(
                        run_id,
                        json!({"kind": "effect", "verb": item.verb, "target": item.target, "ok": true}),
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
            let _ = self.set_buffer_state(run_id, "held", "aborted");
            let seg = Self::seg_of(&row.subject);
            let mut world = HostWorld { inner: self.inner.clone() };
            let _ = self.kernel.ledger.teardown(&seg, &mut world, crate::db::now_unix());
            self.finish_run(run_id, &Outcome::FailStop { at: why.clone() }, 0);
            return Err(KernelError::Denied(format!("approve release failed: {why}")));
        }
        self.set_buffer_state(run_id, "held", "inserted")?;
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "plan.approved", "run": run_id, "nonce": consent.nonce,
            "released": batch.len(),
        }));
        self.emit(run_id, json!({"kind": "approved", "released": batch.len()}));
        // [SEG-TX] approval = commit.
        let seg = Self::seg_of(&row.subject);
        let moved = self.kernel.ledger.transfer_all(&seg, &row.subject)?;
        self.finish_run(run_id, &Outcome::Completed, moved);
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
        let (lock, cv) = &*ctrl;
        {
            let mut c = lock.lock().unwrap();
            c.resume = Some(consent.clone());
        }
        cv.notify_all();
        Ok(())
    }

    /// Sweeper hook: expire every suspended run whose original consent is
    /// past ttl ([TTL]): drop the buffer, roll back the segment, the already
    /// emitted prefix stands.
    pub fn expire(&self, now: u64) {
        let expiring: Vec<(String, Arc<(Mutex<Ctrl>, Condvar)>, String, RunState)> = {
            let runs = self.runs.lock().unwrap();
            runs.iter()
                .filter(|(_, r)| {
                    matches!(r.state, RunState::AwaitingApproval | RunState::Paused)
                        && r.original_ttl_at > 0
                        && now > r.original_ttl_at
                })
                .map(|(id, r)| (id.clone(), r.ctrl.clone(), r.fiber.clone(), r.state))
                .collect()
        };
        for (run_id, ctrl, fiber, state) in expiring {
            {
                let (lock, cv) = &*ctrl;
                let mut c = lock.lock().unwrap();
                c.abort = Some(true);
                cv.notify_all();
            }
            // A Paused interpreter settles itself on wake; an
            // AwaitingApproval run has no thread — settle it here.
            if state == RunState::AwaitingApproval {
                self.settle_aborted(&run_id, &fiber, true);
            }
        }
    }

    fn settle_aborted(&self, run_id: &str, fiber: &str, expired: bool) {
        let now = crate::db::now_unix();
        let _ = self.set_buffer_state(run_id, "held", "aborted");
        let seg = Self::seg_of(fiber);
        let mut world = HostWorld { inner: self.inner.clone() };
        let _ = self.kernel.ledger.teardown(&seg, &mut world, now);
        self.finish_run(run_id, &Outcome::Aborted { expired }, 0);
    }

    /// Host drop: signal every interpreter to abort and join its thread.
    pub fn shutdown(&self) {
        let ctrls: Vec<Arc<(Mutex<Ctrl>, Condvar)>> = {
            let runs = self.runs.lock().unwrap();
            runs.values().map(|r| r.ctrl.clone()).collect()
        };
        for ctrl in &ctrls {
            let (lock, cv) = &**ctrl;
            let mut c = lock.lock().unwrap();
            c.abort = Some(false);
            cv.notify_all();
        }
        let threads: Vec<std::thread::JoinHandle<()>> = {
            let mut runs = self.runs.lock().unwrap();
            runs.values_mut().filter_map(|r| r.thread.take()).collect()
        };
        for t in threads {
            let _ = t.join();
        }
    }

    // ---- interpreter ----

    fn exec_thread(self: &Arc<Self>, run_id: &str, plan: Plan, consent: ConsentRecord) {
        let fiber = format!("plan:{}#{}", self.plan_hash_of(run_id), run_id);
        let ctrl = self
            .runs
            .lock()
            .unwrap()
            .get(run_id)
            .map(|r| r.ctrl.clone())
            .expect("run registered");
        let mut rt = Rt {
            svc: self,
            run_id,
            fiber: &fiber,
            ctrl,
            env: BTreeMap::new(),
            schemas: self.kernel_schemas(),
            modes: Vec::new(),
            withheld: 0,
            active_nonce: consent.nonce.clone(),
        };
        match rt.exec_stmts(&plan.stmts, &Label::public_trusted()) {
            Ok(()) => {
                if rt.withheld == 0 {
                    // [SEG-TX] walked to the end with nothing withheld = commit.
                    let seg = Self::seg_of(&fiber);
                    let moved = self.kernel.ledger.transfer_all(&seg, &fiber).unwrap_or(0);
                    self.finish_run(run_id, &Outcome::Completed, moved);
                } else {
                    self.set_state(run_id, RunState::AwaitingApproval);
                    let _ = self.kernel.audit.lock().unwrap().append(json!({
                        "event": "plan.awaiting_approval", "run": run_id, "withheld": rt.withheld,
                    }));
                    self.emit(
                        run_id,
                        json!({"kind": "awaiting_approval", "withheld": rt.withheld}),
                    );
                }
            }
            Err(Halt::Stop(outcome)) => {
                // Any non-commit terminal state: drop the buffer, roll the
                // segment back ([SEG-TX], the monitor does it itself).
                let _ = self.set_buffer_state(run_id, "held", "aborted");
                let seg = Self::seg_of(&fiber);
                let mut world = HostWorld { inner: self.inner.clone() };
                let _ = self.kernel.ledger.teardown(&seg, &mut world, crate::db::now_unix());
                self.finish_run(run_id, &outcome, 0);
            }
        }
    }

    // ---- run helpers ----

    fn set_state(&self, run_id: &str, state: RunState) {
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

    fn finish_run(&self, run_id: &str, outcome: &Outcome, seg_committed: usize) {
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

    fn finish_row(&self, run_id: &str, outcome: &Outcome) {
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

    fn plan_hash_of(&self, run_id: &str) -> String {
        self.runs
            .lock()
            .unwrap()
            .get(run_id)
            .map(|r| r.plan_hash.clone())
            .unwrap_or_default()
    }

    fn plan_bytes(&self, plan_hash: &str) -> Result<Vec<u8>, KernelError> {
        let mut out = Vec::new();
        let mut reader = self.kernel.cas.open_read(&plan_hash.to_string())?;
        use std::io::Read;
        reader.read_to_end(&mut out)?;
        Ok(out)
    }

    fn emit(&self, run_id: &str, data: Value) {
        dispatch_event(&self.kernel, &self.inner, &format!("plan::run::{run_id}"), data);
    }

    fn load_run_row(&self, run_id: &str) -> Result<RunRow, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.query_row(
            "SELECT plan_hash, subject, nonce, state FROM plan_runs WHERE run_id = ?1",
            params![run_id],
            |r| {
                Ok(RunRow {
                    plan_hash: r.get(0)?,
                    subject: r.get(1)?,
                    nonce: r.get(2)?,
                    state: r.get(3)?,
                })
            },
        )
        .map_err(|_| KernelError::NotFound(format!("plan run: {run_id}")))
    }

    fn consent_ttl_at(&self, nonce: &str) -> Result<u64, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let json: String = conn
            .query_row("SELECT json FROM consents WHERE nonce = ?1", params![nonce], |r| r.get(0))
            .map_err(|_| KernelError::NotFound(format!("consent: {nonce}")))?;
        let rec: ConsentRecord =
            serde_json::from_str(&json).map_err(|e| KernelError::Corrupt(format!("consent json: {e}")))?;
        Ok(rec.issued_at + rec.ttl_secs)
    }

    fn load_buffer(&self, run_id: &str) -> Result<Vec<Buffered>, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, verb, target, args, cost FROM suppression_buffer \
             WHERE run_id = ?1 AND state = 'held' ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(Buffered {
                    seq: r.get::<_, i64>(0)? as u64,
                    verb: r.get(1)?,
                    target: r.get(2)?,
                    args: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
                    cost: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn buffer_seq(&self, run_id: &str) -> u64 {
        let conn = self.kernel.db.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM suppression_buffer WHERE run_id = ?1",
            params![run_id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as u64
    }

    fn push_buffer(&self, run_id: &str, item: &Buffered) -> Result<(), KernelError> {
        let seq = self.buffer_seq(run_id) + 1;
        let conn = self.kernel.db.lock().unwrap();
        conn.execute(
            "INSERT INTO suppression_buffer (run_id, seq, verb, target, args, cost) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id,
                seq as i64,
                item.verb,
                item.target,
                serde_json::to_string(&item.args).unwrap(),
                item.cost as i64
            ],
        )?;
        Ok(())
    }

    fn emission_count(&self, run_id: &str) -> Result<u64, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM emission_log WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    fn log_emission(&self, run_id: &str, seq: u64, verb: &str, target: &str, nonce: &str) -> Result<(), KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.execute(
            "INSERT INTO emission_log (run_id, seq, verb, target, nonce, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![run_id, seq as i64, verb, target, nonce, crate::db::now_unix() as i64],
        )?;
        Ok(())
    }

    fn set_buffer_state(&self, run_id: &str, from: &str, to: &str) -> Result<(), KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.execute(
            "UPDATE suppression_buffer SET state = ?3 WHERE run_id = ?1 AND state = ?2",
            params![run_id, from, to],
        )?;
        Ok(())
    }

    fn mark_buffer(&self, run_id: &str, seq: u64, state: &str) -> Result<(), KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.execute(
            "UPDATE suppression_buffer SET state = ?3 WHERE run_id = ?1 AND seq = ?2",
            params![run_id, seq as i64, state],
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
    pub fn run_state(&self, run_id: &str) -> Result<String, KernelError> {
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
    /// this): (verb, target, cost).
    pub fn withheld_batch(&self, run_id: &str) -> Result<Vec<(String, String, u64)>, KernelError> {
        Ok(self
            .load_buffer(run_id)?
            .into_iter()
            .map(|b| (b.verb, b.target, b.cost))
            .collect())
    }

    fn kernel_schemas(&self) -> VerbSchemas {
        kernel_schemas(&self.inner)
    }
}

enum Halt {
    Stop(Outcome),
}

/// The interpreter: recursive evaluation with the monitor pipeline per
/// effect. Owns its env; suspensions coordinate through the run's `Ctrl`.
struct Rt<'a> {
    svc: &'a PlanService,
    run_id: &'a str,
    fiber: &'a str,
    ctrl: Arc<(Mutex<Ctrl>, Condvar)>,
    env: BTreeMap<String, (Value, Label)>,
    schemas: VerbSchemas,
    modes: Vec<Mode>,
    withheld: usize,
    /// The consent nonce paying for emissions right now (the original
    /// consent's, then each incremental one after a resume).
    active_nonce: String,
}

impl Rt<'_> {
    fn current_mode(&self) -> Mode {
        *self.modes.last().unwrap_or(&Mode::Strict)
    }

    fn exec_stmts(&mut self, stmts: &[Stmt], pc: &Label) -> Result<(), Halt> {
        for (i, s) in stmts.iter().enumerate() {
            match s {
                Stmt::Let { var, expr } => {
                    let vl = self.eval(expr)?;
                    self.env.insert(var.clone(), vl);
                }
                Stmt::Effect { verb, args } => {
                    self.exec_effect(verb, args, pc, stmts.len() - i)?
                }
                Stmt::If { guard, then_, else_ } => {
                    let (b, gl) = self.eval_guard(guard)?;
                    let pc2 = pc.join(&gl);
                    self.exec_stmts(if b { then_ } else { else_ }, &pc2)?;
                }
                Stmt::Foreach { var, list, bound, mode, body } => {
                    let (lv, ll) = self.eval(list)?;
                    let items = lv.as_array().cloned().unwrap_or_default();
                    let n = items.len();
                    let mut take = n;
                    if n > *bound as usize {
                        match mode {
                            Mode::Strict => {
                                return Err(Halt::Stop(Outcome::FailStop {
                                    at: format!("foreach bound {bound} < {n}"),
                                }));
                            }
                            Mode::Truncate => {
                                let dropped = n - *bound as usize;
                                self.svc.audit(self.run_id, "plan.truncated", json!({
                                    "bound": bound, "actual": n, "dropped": dropped,
                                }));
                                self.svc.emit(self.run_id, json!({
                                    "kind": "truncated", "dropped": dropped,
                                }));
                                take = *bound as usize;
                            }
                            Mode::Escalate => {
                                // Pause; on resume the loop continues with the
                                // whole list, ceiling = the incremental budget.
                                self.park_for_resume()?;
                                take = n;
                            }
                        }
                    }
                    let pc2 = pc.join(&ll);
                    self.modes.push(*mode);
                    for item in items.into_iter().take(take) {
                        self.env.insert(var.clone(), (item, ll.clone()));
                        if let Err(h) = self.exec_stmts(body, &pc2) {
                            self.modes.pop();
                            return Err(h);
                        }
                    }
                    self.modes.pop();
                }
            }
        }
        Ok(())
    }

    /// The monitor pipeline per effect: sink re-check → staged → budget →
    /// execute. (Confine has no declared stand-ins today; protocol is
    /// enforced at the serving plugin inside `call_on`.)
    fn exec_effect(&mut self, verb: &str, args: &[Expr], pc: &Label, remaining: usize) -> Result<(), Halt> {
        let mut eff_label = pc.clone();
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            let (v, l) = self.eval(a)?;
            eff_label = eff_label.join(&l);
            vals.push(v);
        }
        // Args convention: a single object-valued argument is passed bare
        // (browser-style named args); anything else rides as the array
        // (echo-style positional args).
        let args_json = match vals.len() {
            1 if vals[0].is_object() => vals.remove(0),
            _ => Value::Array(vals),
        };
        // Sink re-check (the label rule, mirroring admission): confidential
        // must not reach an external effect.
        let external = self
            .schemas
            .external_effects
            .get(verb)
            .copied()
            .unwrap_or(true);
        if external && !eff_label.conf.is_empty() {
            return Err(Halt::Stop(Outcome::FailStop { at: format!("sink denied: {verb}") }));
        }
        let target = crate::host::target_of(&self.svc.inner, verb, &args_json);
        // Staged (the hard list: emitting ∧ non-amortizable): withhold fully
        // evaluated — an approval from a later process can release as-is.
        if crate::host::withholds(&self.svc.inner, verb) {
            let item = Buffered {
                seq: 0, // assigned at insert
                verb: verb.to_string(),
                target: target.clone(),
                args: args_json,
                cost: 1,
            };
            if self.svc.push_buffer(self.run_id, &item).is_err() {
                return Err(Halt::Stop(Outcome::FailStop { at: format!("buffer write failed: {verb}") }));
            }
            self.withheld += 1;
            self.svc.audit(self.run_id, "plan.withheld", json!({
                "verb": verb, "target": target,
            }));
            self.svc.emit(self.run_id, json!({
                "kind": "withheld", "verb": verb, "target": target,
            }));
            return Ok(());
        }
        // Budget gate + execute (capability check against the consent-minted
        // caps; protocol enforced at the serving plugin). Escalate retries
        // the same effect against the fresh pool after each resume.
        loop {
            match invoke_as(&self.svc.kernel, &self.svc.inner, self.fiber, verb, args_json.clone()) {
                Ok(_) => {
                    let seq = self.svc.emission_count(self.run_id).unwrap_or(0) + 1;
                    let _ = self.svc.log_emission(self.run_id, seq, verb, &target, &self.active_nonce);
                    self.svc.emit(self.run_id, json!({
                        "kind": "effect", "verb": verb, "target": target, "ok": true,
                    }));
                    return Ok(());
                }
                Err(e) => {
                    let msg = e.to_string();
                    self.svc.emit(self.run_id, json!({
                        "kind": "effect", "verb": verb, "target": target, "ok": false,
                        "error": msg,
                    }));
                    if msg.contains("budget exhausted") || msg.contains("no capability") {
                        match self.current_mode() {
                            Mode::Strict => {
                                return Err(Halt::Stop(Outcome::FailStop {
                                    at: format!("budget exhausted: {verb}"),
                                }));
                            }
                            Mode::Truncate => {
                                self.svc.audit(self.run_id, "plan.truncated", json!({
                                    "at": verb, "dropped": remaining,
                                }));
                                self.svc.emit(self.run_id, json!({
                                    "kind": "truncated", "at": verb, "dropped": remaining,
                                }));
                                return Err(Halt::Stop(Outcome::Truncated { dropped: remaining }));
                            }
                            Mode::Escalate => {
                                self.park_for_resume()?;
                                continue;
                            }
                        }
                    } else {
                        return Err(Halt::Stop(Outcome::FailStop { at: format!("{verb}: {msg}") }));
                    }
                }
            }
        }
    }

    /// Escalate: park until a resume consent or an abort signal arrives
    /// ([TTL]: the sweeper expires the parked thread too).
    fn park_for_resume(&mut self) -> Result<(), Halt> {
        self.svc.set_state(self.run_id, RunState::Paused);
        self.svc.audit(self.run_id, "plan.paused", json!({}));
        self.svc.emit(self.run_id, json!({"kind": "paused"}));
        let (lock, cv) = &*self.ctrl;
        let mut c = lock.lock().unwrap();
        let consent = loop {
            if let Some(expired) = c.abort {
                return Err(Halt::Stop(Outcome::Aborted { expired }));
            }
            if let Some(consent) = c.resume.take() {
                break consent;
            }
            c = cv.wait(c).unwrap();
        };
        drop(c);
        // The fresh quadruple was already verified by `resume`; mint its
        // pools for the fiber and continue ([ESC]: mint, then exact resume).
        let fiber = self.fiber.to_string();
        let mut by_family: BTreeMap<String, (Vec<String>, BTreeMap<String, u64>)> = BTreeMap::new();
        for (verb, n) in &consent.budget {
            let (family, short) = split_verb(verb);
            let (verbs, counts) = by_family.entry(family.to_string()).or_default();
            verbs.push(short.to_string());
            counts.insert(short.to_string(), *n);
        }
        let ttl_at = consent.issued_at + consent.ttl_secs;
        for (family, (verbs, counts)) in by_family {
            if self
                .svc
                .kernel
                .caps
                .mint(
                    &fiber,
                    &format!("driver:{family}"),
                    verbs.into_iter().collect(),
                    Constraints { expires_at: Some(ttl_at), counts },
                    None,
                )
                .is_err()
            {
                return Err(Halt::Stop(Outcome::FailStop { at: "resume pool mint failed".into() }));
            }
        }
        self.active_nonce = consent.nonce.clone();
        self.svc.set_state(self.run_id, RunState::Running);
        Ok(())
    }

    fn eval(&mut self, e: &Expr) -> Result<(Value, Label), Halt> {
        match e {
            Expr::Observe { verb, args } => {
                let base = self
                    .schemas
                    .observe
                    .get(verb)
                    .cloned()
                    .ok_or_else(|| Halt::Stop(Outcome::FailStop { at: format!("unknown verb {verb}") }))?;
                let mut label = base;
                let mut vals = Vec::new();
                for a in args {
                    let (v, l) = self.eval(a)?;
                    label = label.join(&l);
                    vals.push(v);
                }
                let args_json = if vals.len() == 1 { vals.remove(0) } else { Value::Array(vals) };
                // Reads go through the same capability gate (minted uncounted)
                // and are metered, never budgeted (truth table).
                let v = invoke_as(&self.svc.kernel, &self.svc.inner, self.fiber, verb, args_json)
                    .map_err(|e| Halt::Stop(Outcome::FailStop { at: format!("{verb}: {e}") }))?;
                Ok((v, label))
            }
            Expr::Pure { func, args } => {
                let mut label = Label::public_trusted();
                let mut vals = Vec::new();
                for a in args {
                    let (v, l) = self.eval(a)?;
                    label = label.join(&l);
                    vals.push(v);
                }
                let mut fuel = portos_compute::FuelMeter::new(100_000);
                let registry = portos_compute::Registry::builtin();
                let v = registry
                    .run(func, None, &vals, &mut fuel)
                    .map_err(|e| Halt::Stop(Outcome::FailStop { at: format!("pure {func}: {e}") }))?;
                Ok((v, label))
            }
            Expr::Const { value } => Ok((value.clone(), Label::public_trusted())),
            Expr::Var { name } => self
                .env
                .get(name)
                .cloned()
                .ok_or_else(|| Halt::Stop(Outcome::FailStop { at: format!("unknown var {name}") })),
            Expr::Index { base, idx } => {
                let (v, l) = self.eval(base)?;
                let item = v
                    .as_array()
                    .and_then(|a| a.get(*idx as usize))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok((item, l))
            }
        }
    }

    fn eval_guard(&mut self, g: &Guard) -> Result<(bool, Label), Halt> {
        Ok(match g {
            Guard::Exists { expr } => {
                let (v, l) = self.eval(expr)?;
                (v.as_array().map(|a| !a.is_empty()).unwrap_or(!v.is_null()), l)
            }
            Guard::Matches { expr, regex } => {
                let (v, l) = self.eval(expr)?;
                let re = regex::Regex::new(regex)
                    .map_err(|e| Halt::Stop(Outcome::FailStop { at: format!("guard regex: {e}") }))?;
                (v.as_str().map(|s| re.is_match(s)).unwrap_or(false), l)
            }
            Guard::Cmp { lhs, op, rhs } => {
                let (a, la) = self.eval(lhs)?;
                let (b, lb) = self.eval(rhs)?;
                let ord = cmp_values(&a, &b);
                let res = match (op, ord) {
                    (CmpOp::Eq, Some(o)) => o == std::cmp::Ordering::Equal,
                    (CmpOp::Ne, Some(o)) => o != std::cmp::Ordering::Equal,
                    (CmpOp::Lt, Some(o)) => o == std::cmp::Ordering::Less,
                    (CmpOp::Le, Some(o)) => o != std::cmp::Ordering::Greater,
                    (CmpOp::Gt, Some(o)) => o == std::cmp::Ordering::Greater,
                    (CmpOp::Ge, Some(o)) => o != std::cmp::Ordering::Less,
                    (_, None) => false,
                };
                (res, la.join(&lb))
            }
            Guard::And { l, r } => {
                let (a, la) = self.eval_guard(l)?;
                let (b, lb) = self.eval_guard(r)?;
                (a && b, la.join(&lb))
            }
            Guard::Or { l, r } => {
                let (a, la) = self.eval_guard(l)?;
                let (b, lb) = self.eval_guard(r)?;
                (a || b, la.join(&lb))
            }
            Guard::Not { g } => {
                let (a, l) = self.eval_guard(g)?;
                (!a, l)
            }
        })
    }
}

fn cmp_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Every verb a plan may reach (effects and reads), as `family::verb` strings.
fn verb_demand(plan: &Plan) -> Vec<String> {
    fn of_stmts(stmts: &[Stmt], out: &mut Vec<String>) {
        for s in stmts {
            match s {
                Stmt::Effect { verb, .. } => out.push(verb.clone()),
                Stmt::If { guard, then_, else_ } => {
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

fn split_verb(verb: &str) -> (&str, &str) {
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

impl PlanService {
    fn audit(&self, run_id: &str, event: &str, extra: Value) {
        let mut body = json!({ "event": event, "run": run_id });
        if let (Some(a), Some(b)) = (body.as_object_mut(), extra.as_object()) {
            a.extend(b.clone());
        }
        let _ = self.kernel.audit.lock().unwrap().append(body);
    }
}
