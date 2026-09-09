//! Evaluate statements and apply the sink, withholding and budget pipeline.

mod eval;

use super::{
    BufferState, Buffered, Outcome, PlanService, RunControl, RunSignal, RunState,
    admission::split_verb,
};
use crate::{KernelError, consent::ConsentRecord, plancheck::VerbSchemas};
use portos_proto::{Expr, Label, Mode, Plan, Stmt, cap::Constraints};
use portos_rm::identity::{SubjectId, VerbId};
use portos_rm::time::Timestamp;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

impl PlanService {
    // ---- interpreter ----

    pub(super) fn exec_thread(self: &Arc<Self>, run_id: &str, plan: Plan, consent: ConsentRecord) {
        let (fiber, ctrl) = self
            .runs
            .lock()
            .unwrap()
            .get(run_id)
            .map(|r| (r.fiber.clone(), r.ctrl.clone()))
            .expect("run registered");
        let mut rt = Interpreter {
            svc: self,
            run_id,
            fiber: &fiber,
            ctrl,
            env: BTreeMap::new(),
            schemas: self.runtime.schemas(),
            modes: Vec::new(),
            withheld: 0,
            active_nonce: consent.nonce.clone(),
        };
        match rt.exec_stmts(&plan.stmts, &Label::public_trusted()) {
            Ok(()) => {
                if rt.withheld == 0 {
                    // [SEG-TX] walked to the end with nothing withheld = commit.
                    let seg = self.segment_of(run_id).expect("registered plan segment");
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
            Err(outcome) => {
                // Any non-commit terminal state: drop the buffer, roll the
                // segment back ([SEG-TX], the monitor does it itself).
                let _ = self.set_buffer_state(run_id, BufferState::Held, BufferState::Aborted);
                let seg = self.segment_of(run_id).expect("registered plan segment");
                let _ = self.runtime.teardown_segment(
                    &seg,
                    Timestamp::try_from(crate::db::now_unix()).expect("system timestamp in range"),
                );
                self.finish_run(run_id, &outcome, 0);
            }
        }
    }
}

/// The interpreter: recursive evaluation with the monitor pipeline per
/// effect. Owns its env; suspensions use the run's control channel.
struct Interpreter<'a> {
    svc: &'a PlanService,
    run_id: &'a str,
    fiber: &'a SubjectId,
    ctrl: Arc<RunControl>,
    env: BTreeMap<String, (Value, Label)>,
    schemas: VerbSchemas,
    modes: Vec<Mode>,
    withheld: usize,
    /// The consent nonce paying for emissions right now (the original
    /// consent's, then each incremental one after a resume).
    active_nonce: String,
}

impl Interpreter<'_> {
    fn current_mode(&self) -> Mode {
        *self.modes.last().unwrap_or(&Mode::Strict)
    }

    fn exec_stmts(&mut self, stmts: &[Stmt], pc: &Label) -> Result<(), Outcome> {
        for (i, s) in stmts.iter().enumerate() {
            match s {
                Stmt::Let { var, expr } => {
                    let vl = self.eval(expr)?;
                    self.env.insert(var.clone(), vl);
                }
                Stmt::Effect { verb, args } => self.exec_effect(verb, args, pc, stmts.len() - i)?,
                Stmt::If {
                    guard,
                    then_,
                    else_,
                } => {
                    let (b, gl) = self.eval_guard(guard)?;
                    let pc2 = pc.join(&gl);
                    self.exec_stmts(if b { then_ } else { else_ }, &pc2)?;
                }
                Stmt::Foreach {
                    var,
                    list,
                    bound,
                    mode,
                    body,
                } => {
                    let (lv, ll) = self.eval(list)?;
                    let items = lv.as_array().cloned().unwrap_or_default();
                    let n = items.len();
                    let mut take = n;
                    if n > *bound as usize {
                        match mode {
                            Mode::Strict => {
                                return Err(Outcome::FailStop {
                                    at: format!("foreach bound {bound} < {n}"),
                                });
                            }
                            Mode::Truncate => {
                                let dropped = n - *bound as usize;
                                self.svc.audit(
                                    self.run_id,
                                    "plan.truncated",
                                    json!({
                                        "bound": bound, "actual": n, "dropped": dropped,
                                    }),
                                );
                                self.svc.emit(
                                    self.run_id,
                                    json!({
                                        "kind": "truncated", "dropped": dropped,
                                    }),
                                );
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
    fn exec_effect(
        &mut self,
        verb: &str,
        args: &[Expr],
        pc: &Label,
        remaining: usize,
    ) -> Result<(), Outcome> {
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
            return Err(Outcome::FailStop {
                at: format!("sink denied: {verb}"),
            });
        }
        let verb_id = VerbId::new(verb);
        let policy = self.svc.runtime.effect_policy(&verb_id, &args_json);
        let target = policy.target;
        // Staged (the hard list: emitting ∧ non-amortizable): withhold fully
        // evaluated — an approval from a later process can release as-is.
        if policy.withhold {
            let item = Buffered {
                seq: 0, // assigned at insert
                verb: verb_id.clone(),
                target: target.clone(),
                args: args_json,
                cost: 1,
            };
            if self.svc.push_buffer(self.run_id, &item).is_err() {
                return Err(Outcome::FailStop {
                    at: format!("buffer write failed: {verb}"),
                });
            }
            self.withheld += 1;
            self.svc.audit(
                self.run_id,
                "plan.withheld",
                json!({
                    "verb": verb, "target": target,
                }),
            );
            self.svc.emit(
                self.run_id,
                json!({
                    "kind": "withheld", "verb": verb, "target": target,
                }),
            );
            return Ok(());
        }
        // Budget gate + execute (capability check against the consent-minted
        // caps; protocol enforced at the serving plugin). Escalate retries
        // the same effect against the fresh pool after each resume.
        loop {
            match self
                .svc
                .runtime
                .invoke(self.fiber, &verb_id, args_json.clone())
            {
                Ok(_) => {
                    let seq = self.svc.emission_count(self.run_id).unwrap_or(0) + 1;
                    let _ = self.svc.log_emission(
                        self.run_id,
                        seq,
                        &verb_id,
                        &target,
                        &self.active_nonce,
                    );
                    self.svc.emit(
                        self.run_id,
                        json!({
                            "kind": "effect", "verb": verb, "target": target, "ok": true,
                        }),
                    );
                    return Ok(());
                }
                Err(e) => {
                    let msg = e.to_string();
                    self.svc.emit(
                        self.run_id,
                        json!({
                            "kind": "effect", "verb": verb, "target": target, "ok": false,
                            "error": msg,
                        }),
                    );
                    if matches!(
                        e,
                        KernelError::BudgetExhausted(_) | KernelError::NoCapability { .. }
                    ) {
                        match self.current_mode() {
                            Mode::Strict => {
                                return Err(Outcome::FailStop {
                                    at: format!("budget exhausted: {verb}"),
                                });
                            }
                            Mode::Truncate => {
                                self.svc.audit(
                                    self.run_id,
                                    "plan.truncated",
                                    json!({
                                        "at": verb, "dropped": remaining,
                                    }),
                                );
                                self.svc.emit(
                                    self.run_id,
                                    json!({
                                        "kind": "truncated", "at": verb, "dropped": remaining,
                                    }),
                                );
                                return Err(Outcome::Truncated { dropped: remaining });
                            }
                            Mode::Escalate => {
                                self.park_for_resume()?;
                                continue;
                            }
                        }
                    } else {
                        return Err(Outcome::FailStop {
                            at: format!("{verb}: {msg}"),
                        });
                    }
                }
            }
        }
    }

    /// Escalate: park until a resume consent or an abort signal arrives
    /// ([TTL]: the sweeper expires the parked thread too).
    fn park_for_resume(&mut self) -> Result<(), Outcome> {
        self.svc.set_state(self.run_id, RunState::Paused);
        self.svc.audit(self.run_id, "plan.paused", json!({}));
        self.svc.emit(self.run_id, json!({"kind": "paused"}));
        let consent = match self.ctrl.wait() {
            RunSignal::Abort { expired } => return Err(Outcome::Aborted { expired }),
            RunSignal::Resume(consent) => consent,
        };
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
                    Constraints {
                        expires_at: Some(ttl_at),
                        counts,
                    },
                    None,
                )
                .is_err()
            {
                return Err(Outcome::FailStop {
                    at: "resume pool mint failed".into(),
                });
            }
        }
        self.active_nonce = consent.nonce.clone();
        self.svc.set_state(self.run_id, RunState::Running);
        Ok(())
    }
}
