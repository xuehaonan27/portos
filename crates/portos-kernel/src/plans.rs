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
//!     fully evaluated, so an approval from a *later process* can still
//!     release the batch ([INSERT]).
//!   - **Three modes** on bound/budget excess: strict fail-stops, truncate
//!     processes the first N and reports (never silent), escalate pauses and
//!     waits for incremental consent. Suspended states (AwaitingApproval,
//!     Paused) are bounded by the *original* consent's ttl ([TTL]); the
//!     sweeper expires them.
//!   - **Segment = transaction** ([SEG-TX]): holdings the run acquires are
//!     booked under `<fiber>:seg`; commit transfers them to the fiber, any
//!     other terminal state tears the segment down (the F2 path). `promote`
//!     is not exposed yet (no plan verb acquires today).
//!
//! Escalate resume is in-process (the interpreter thread parks on a
//! condvar); a crashed kernel's suspended runs are aborted at open
//! (`recover`), matching the drill's crash model (buffer loss = degraded
//! abort).

use crate::consent::{ConsentRecord, render_budget};
use crate::host::{HostInner, invoke_as, subscribe_with_subject};
use crate::plancheck::{self, VerbSchemas};
use crate::{Kernel, KernelError};
use portos_proto::{CmpOp, Expr, Guard, Label, Mode, Plan, Stmt, artifact::id_for_bytes};
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
    fn status(&self) -> String {
        match self {
            Outcome::Completed => "Completed".into(),
            Outcome::FailStop { .. } => "FailStop".into(),
            Outcome::Truncated { .. } => "Truncated".into(),
            Outcome::Aborted { .. } => "Aborted".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Admitted,
    Running,
    AwaitingApproval,
    Paused,
    Done,
}

impl State {
    fn as_str(&self) -> &'static str {
        match self {
            State::Admitted => "admitted",
            State::Running => "running",
            State::AwaitingApproval => "awaiting_approval",
            State::Paused => "paused",
            State::Done => "done",
        }
    }
}

/// Coordination for a parked (Paused) interpreter thread.
#[derive(Default)]
struct Ctrl {
    resume: Option<ConsentRecord>,
    abort: bool,
}

struct RunHandle {
    plan_hash: String,
    fiber: String,
    nonce: String,
    original_ttl_at: u64,
    state: State,
    ctrl: Arc<(Mutex<Ctrl>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The plan-run registry. Self-contained: it drives effects through
/// `invoke_as` (kernel + host inner), so the sweeper and any CLI process can
/// operate it without a `Host`.
pub struct PlanService {
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
    runs: Mutex<BTreeMap<String, RunHandle>>,
}

impl PlanService {
    pub fn new(kernel: Arc<Kernel>, inner: Arc<HostInner>) -> PlanService {
        let svc = PlanService {
            kernel,
            inner,
            runs: Mutex::new(BTreeMap::new()),
        };
        svc.recover();
        svc
    }

    fn seg_of(fiber: &str) -> String {
        format!("{fiber}:seg")
    }

    /// Abort every run a previous process left non-terminal: the interpreter
    /// is gone, so the buffer is dropped and the segment torn down — the
    /// degraded-abort shape ([TTL]; buffer loss is on the safe side).
    fn recover(&self) {
        let now = crate::db::now_unix();
        let stale: Vec<(String, String)> = {
            let conn = self.kernel.db.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT run_id, subject FROM plan_runs \
                     WHERE state IN ('admitted','running','awaiting_approval','paused')",
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
            let mut world = crate::host::HostWorld { inner: self.inner.clone() };
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
    /// `admitted` state. The consent step comes after — a submission never
    /// executes anything.
    pub fn submit(&self, subject: &str, bytes: &[u8]) -> Result<SubmitOut, KernelError> {
        let plan_hash = id_for_bytes(bytes);
        let plan: Plan = Plan::from_bytes(bytes)
            .map_err(|e| KernelError::Corrupt(format!("plan parse: {e}")))?;
        let schemas = self.kernel_schemas();
        let adm = plancheck::admit(&plan, &schemas)
            .map_err(|e| KernelError::Denied(format!("admission: {e}")))?;
        // F5: the plan's whole verb demand (effects and reads) must fit the
        // submitting subject's live grants. CLI runs submit as "user" — root
        // authority, no grants check.
        if subject != "user" {
            let demand = verb_demand(&plan);
            let grants = live_grant_verbs(&self.kernel, subject)?;
            let missing: Vec<&String> = demand.iter().filter(|v| !grants.contains(*v)).collect();
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
        let _ = self
            .kernel
            .cas
            .put_bytes(bytes, "portos/plan", portos_proto::Label::public_trusted(), "plans")?;
        let run_id = format!("run_{}", hex_short());
        let fiber = format!("plan:{plan_hash}#{run_id}");
        {
            let conn = self.kernel.db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_runs (run_id, plan_hash, subject, nonce, state, started_at) \
                 VALUES (?1, ?2, ?3, '', 'admitted', ?4)",
                rusqlite::params![run_id, plan_hash, fiber, crate::db::now_unix() as i64],
            )?;
        }
        self.runs.lock().unwrap().insert(
            run_id.clone(),
            RunHandle {
                plan_hash: plan_hash.clone(),
                fiber,
                nonce: String::new(),
                original_ttl_at: 0,
                state: State::Admitted,
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
            run_id,
            plan_hash: plan_hash.clone(),
            rendering: render_budget(&plan_hash, &adm.budget),
            budget: adm.budget,
        })
    }

    // ---- consent checks & fiber caps ----

    /// Verify a quadruple for this plan: MAC + ttl (the signer stub), plan
    /// hash equality, nonce freshness — and consume the nonce (insert into
    /// `consents`; a replay is a primary-key conflict = StaleNonce).
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
            rusqlite::params![consent.nonce, json, now as i64, source],
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
    /// per the truth table); any verb outside the plan is simply absent.
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
                portos_proto::cap::Constraints {
                    expires_at: Some(ttl_at),
                    counts,
                },
                None,
            )?;
        }
        Ok(())
    }

    // ---- run lifecycle ----

    /// Start an admitted run under a consent: verify the quadruple, mint the
    /// fiber's caps, and spawn the interpreter thread. Returns when the run
    /// reaches a stable state (done / awaiting approval / paused).
    pub fn start(&self, run_id: &str, consent: &ConsentRecord) -> Result<State, KernelError> {
        let (plan_hash, fiber, ctrl) = {
            let mut runs = self.runs.lock().unwrap();
            let run = runs
                .get_mut(run_id)
                .ok_or_else(|| KernelError::NotFound(format!("plan run: {run_id}")))?;
            if run.state != State::Admitted {
                return Err(KernelError::Denied(format!(
                    "plan run {run_id} not admitted (state {})",
                    run.state.as_str()
                )));
            }
            run.state = State::Running;
            run.nonce = consent.nonce.clone();
            run.original_ttl_at = consent.issued_at + consent.ttl_secs;
            (
                run.plan_hash.clone(),
                run.fiber.clone(),
                run.ctrl.clone(),
            )
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
                rusqlite::params![run_id, consent.nonce],
            )?;
        }
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "plan.started", "run": run_id, "plan": plan_hash,
            "nonce": consent.nonce, "budget": consent.budget,
        }));
        self.emit(run_id, json!({"kind": "started", "budget": consent.budget}));

        let svc = self.this();
        let id = run_id.to_string();
        let c = consent.clone();
        let handle = std::thread::spawn(move || svc.exec_thread(&id, plan, c));
        self.runs.lock().unwrap().get_mut(run_id).unwrap().thread = Some(handle);
        Ok(State::Running)
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
        let now = crate::db::now_unix();
        if now > original_ttl_at {
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
                portos_proto::cap::Constraints { expires_at: Some(ttl_at), counts },
                None,
            )?;
        }
        // Release in original order, exactly once (emission log per effect).
        let mut seq = self.emission_count(run_id)?;
        for item in &batch {
            invoke_as(&self.kernel, &self.inner, &row.subject, &item.verb, item.args.clone())
                .map_err(|e| KernelError::Denied(format!("approve release failed: {e}")))?;
            seq += 1;
            self.log_emission(run_id, seq, &item.verb, &item.target, &consent.nonce)?;
            self.emit(
                run_id,
                json!({"kind": "effect", "verb": item.verb, "target": item.target, "ok": true}),
            );
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
        self.finish_run(run_id, &Outcome::Completed, moved)?;
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
            if run.state != State::Paused {
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
            let _ = self.kernel.audit.lock().unwrap().append(json!({
                "event": "plan.resumed", "run": run_id, "nonce": consent.nonce,
            }));
            self.emit(run_id, json!({"kind": "resumed"}));
            run.ctrl.clone()
        };
        {
            let (lock, cv) = &*ctrl;
            let mut c = lock.lock().unwrap();
            c.resume = Some(consent.clone());
            cv.notify_all();
        }
        Ok(())
    }

    /// Sweeper hook: expire every suspended run whose original consent is
    /// past ttl ([TTL]): drop the buffer, roll back the segment, deliver the
    /// prefix (already-emitted effects stand).
    pub fn expire(&self, now: u64) {
        let expiring: Vec<(String, Arc<(Mutex<Ctrl>, Condvar)>)> = {
            let runs = self.runs.lock().unwrap();
            runs.values()
                .filter(|r| {
                    matches!(r.state, State::AwaitingApproval | State::Paused)
                        && r.original_ttl_at > 0
                        && now > r.original_ttl_at
                })
                .map(|r| {
                    let id = self
                        .runs
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(_, v)| std::ptr::eq(*v, r))
                        .map(|(k, _)| k.clone())
                        .unwrap_or_default();
                    (id, r.ctrl.clone())
                })
                .collect()
        };
        for (run_id, ctrl) in expiring {
            self.abort_run(&run_id, true, &ctrl);
        }
    }

    /// Abort a run: signal a parked interpreter (if any) and settle the
    /// terminal state (buffer dropped, segment rolled back).
    fn abort_run(&self, run_id: &str, expired: bool, ctrl: &Arc<(Mutex<Ctrl>, Condvar)>) {
        {
            let (lock, cv) = &**ctrl;
            let mut c = lock.lock().unwrap();
            c.abort = true;
            cv.notify_all();
        }
        // The parked thread settles itself on wake; if the run was only
        // AwaitingApproval (no thread), settle here.
        let settle_here = {
            let runs = self.runs.lock().unwrap();
            runs.get(run_id).map(|r| r.state == State::AwaitingApproval).unwrap_or(false)
        };
        if settle_here {
            let fiber = self
                .runs
                .lock()
                .unwrap()
                .get(run_id)
                .map(|r| r.fiber.clone())
                .unwrap_or_default();
            self.settle_aborted(run_id, &fiber, expired);
        }
    }

    fn settle_aborted(&self, run_id: &str, fiber: &str, expired: bool) {
        let now = crate::db::now_unix();
        let _ = self.set_buffer_state(run_id, "held", "aborted");
        let seg = Self::seg_of(fiber);
        let mut world = crate::host::HostWorld { inner: self.inner.clone() };
        let _ = self.kernel.ledger.teardown(&seg, &mut world, now);
        self.finish_row(run_id, &Outcome::Aborted { expired });
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "plan.aborted", "run": run_id, "expired": expired,
        }));
        self.emit(run_id, json!({"kind": "aborted", "expired": expired}));
    }

    /// Host drop: park every interpreter with an abort signal and join.
    pub fn shutdown(&self) {
        let ctrls: Vec<Arc<(Mutex<Ctrl>, Condvar)>> = {
            let runs = self.runs.lock().unwrap();
            runs.values().map(|r| r.ctrl.clone()).collect()
        };
        for (lock, cv) in ctrls.iter().map(|c| ((), c)) {
            let _ = lock;
        }
        for ctrl in &ctrls {
            let (lock, cv) = &**ctrl;
            let mut c = lock.lock().unwrap();
            c.abort = true;
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

    // ---- interpreter thread ----

    fn exec_thread(&self, run_id: &str, plan: Plan, consent: ConsentRecord) {
        let fiber = format!("plan:{}#{}", self.run_plan_hash(run_id), run_id);
        let mut rt = Rt {
            svc: self,
            run_id,
            fiber: &fiber,
            plan_hash: self.run_plan_hash(run_id),
            consent: &consent,
            env: BTreeMap::new(),
            schemas: self.kernel_schemas(),
            effects: 0,
        };
        let outcome = match rt.exec_stmts(&plan.stmts, &Label::public_trusted()) {
            Ok(()) => {
                // Plan walked to the end: withheld batch pending → suspend for
                // approval; nothing pending → [SEG-TX] commit.
                if rt.withheld == 0 {
                    let seg = Self::seg_of(&fiber);
                    let moved = self.kernel.ledger.transfer_all(&seg, &fiber).unwrap_or(0);
                    self.finish_run(run_id, &Outcome::Completed, moved)
                } else {
                    self.suspend_for_approval(run_id, &fiber, rt.withheld)
                }
            }
            Err(halt) => match halt {
                Halt::Paused => {} // the thread settled itself (or still parked → handled below)
                Halt::Stop(outcome) => {
                    let _ = self.set_buffer_state(run_id, "held", "aborted");
                    let seg = Self::seg_of(&fiber);
                    let mut world = crate::host::HostWorld { inner: self.inner.clone() };
                    let _ = self.kernel.ledger.teardown(&seg, &mut world, crate::db::now_unix());
                    self.finish_run(run_id, &outcome, 0)
                }
            },
        };
        let _ = outcome;
    }

    fn suspend_for_approval(&self, run_id: &str, _fiber: &str, withheld: usize) {
        {
            let mut runs = self.runs.lock().unwrap();
            if let Some(r) = runs.get_mut(run_id) {
                r.state = State::AwaitingApproval;
            }
        }
        {
            let conn = self.kernel.db.lock().unwrap();
            let _ = conn.execute(
                "UPDATE plan_runs SET state = 'awaiting_approval' WHERE run_id = ?1",
                rusqlite::params![run_id],
            );
        }
        let _ = self.kernel.audit.lock().unwrap().append(json!({
            "event": "plan.awaiting_approval", "run": run_id, "withheld": withheld,
        }));
        self.emit(run_id, json!({"kind": "awaiting_approval", "withheld": withheld}));
    }

    // ---- run helpers ----

    fn this(&self) -> PlanServiceRef {
        PlanServiceRef {
            kernel: self.kernel.clone(),
            inner: self.inner.clone(),
            runs: unsafe { std::mem::transmute::<&Mutex<BTreeMap<String, RunHandle>>, &'static Mutex<BTreeMap<String, RunHandle>>>(self.runs_ref()) },
        }
    }

    fn runs_ref(&self) -> &Mutex<BTreeMap<String, RunHandle>> {
        &self.runs
    }

    fn run_plan_hash(&self, run_id: &str) -> String {
        self.runs
            .lock()
            .unwrap()
            .get(run_id)
            .map(|r| r.plan_hash.clone())
            .unwrap_or_default()
    }

    fn plan_bytes(&self, plan_hash: &str) -> Result<Vec<u8>, KernelError> {
        let mut out = Vec::new();
        let mut reader = self.kernel.cas.open_read(plan_hash)?;
        use std::io::Read;
        reader.read_to_end(&mut out)?;
        Ok(out)
    }

    fn emit(&self, run_id: &str, data: Value) {
        crate::host::dispatch_event(
            &self.kernel,
            &self.inner,
            &format!("plan::run::{run_id}"),
            data,
        );
    }

    fn finish_run(&self, run_id: &str, outcome: &Outcome, seg_committed: usize) {
        {
            let mut runs = self.runs.lock().unwrap();
            if let Some(r) = runs.get_mut(run_id) {
                r.state = State::Done;
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
            rusqlite::params![
                run_id,
                crate::db::now_unix() as i64,
                serde_json::to_string(outcome).unwrap()
            ],
        );
    }

    fn load_run_row(&self, run_id: &str) -> Result<RunRow, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.query_row(
            "SELECT plan_hash, subject, nonce, state FROM plan_runs WHERE run_id = ?1",
            rusqlite::params![run_id],
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
            .query_row(
                "SELECT json FROM consents WHERE nonce = ?1",
                rusqlite::params![nonce],
                |r| r.get(0),
            )
            .map_err(|_| KernelError::NotFound(format!("consent: {nonce}")))?;
        let rec: ConsentRecord = serde_json::from_str(&json)
            .map_err(|e| KernelError::Corrupt(format!("consent json: {e}")))?;
        Ok(rec.issued_at + rec.ttl_secs)
    }

    fn load_buffer(&self, run_id: &str) -> Result<Vec<Buffered>, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, verb, target, args, cost FROM suppression_buffer \
             WHERE run_id = ?1 AND state = 'held' ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![run_id], |r| {
                Ok(Buffered {
                    verb: r.get(1)?,
                    target: r.get(2)?,
                    args: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
                    cost: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn emission_count(&self, run_id: &str) -> Result<u64, KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM emission_log WHERE run_id = ?1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    fn log_emission(&self, run_id: &str, seq: u64, verb: &str, target: &str, nonce: &str) -> Result<(), KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.execute(
            "INSERT INTO emission_log (run_id, seq, verb, target, nonce, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![run_id, seq as i64, verb, target, nonce, crate::db::now_unix() as i64],
        )?;
        Ok(())
    }

    fn set_buffer_state(&self, run_id: &str, from: &str, to: &str) -> Result<(), KernelError> {
        let conn = self.kernel.db.lock().unwrap();
        conn.execute(
            "UPDATE suppression_buffer SET state = ?3 WHERE run_id = ?1 AND state = ?2",
            rusqlite::params![run_id, from, to],
        )?;
        Ok(())
    }

    fn kernel_schemas(&self) -> VerbSchemas {
        crate::host::kernel_schemas(&self.inner)
    }
}

struct RunRow {
    plan_hash: String,
    subject: String,
    nonce: String,
    state: String,
}

struct Buffered {
    verb: String,
    target: String,
    args: Value,
    cost: u64,
}

/// An `Arc`-free clone of the service's internals for the interpreter thread.
struct PlanServiceRef {
    kernel: Arc<Kernel>,
    inner: Arc<HostInner>,
    runs: &'static Mutex<BTreeMap<String, RunHandle>>,
}

enum Halt {
    Stop(Outcome),
    Paused,
}

/// The interpreter: recursive evaluation with the monitor pipeline per
/// effect. Owns its env; suspensions coordinate through the run's `Ctrl`.
struct Rt<'a> {
    svc: &'a PlanService,
    run_id: &'a str,
    fiber: &'a str,
    plan_hash: String,
    consent: &'a ConsentRecord,
    env: BTreeMap<String, (Value, Label)>,
    schemas: VerbSchemas,
    effects: usize,
}

struct WithheldCount(usize);

impl Rt<'_> {}
