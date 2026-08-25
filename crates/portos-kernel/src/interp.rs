//! The effect-plan interpreter — the "runtime monitor" half of the
//! two-level eBPF analogy (effect-plan-v0.md §3.4).
//!
//! Execution contract (§7): prefix execution + deterministic interpreter +
//! replayable audit. Any denial or precondition failure fail-stops and the
//! executed prefix is returned as data. No cross-effect atomicity is
//! promised (the world is not transactional).

use crate::consent::{ConsentKey, ConsentRecord};
use crate::plancheck::{VerbSchemas, admit};
use crate::{KernelError, audit::AuditLog};
use portos_proto::{CmpOp, Expr, Guard, Label, Mode, Plan, Stmt, artifact::id_for_bytes};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The boundary to the outside world. In M0 tests this is an in-process toy;
/// in M1 it dispatches to driver plugins over IPC.
pub trait EffectExec {
    fn observe(&mut self, verb: &str, args: &[Value]) -> Result<Value, String>;
    fn effect(&mut self, verb: &str, args: &[Value]) -> Result<(), String>;
    fn pure(&mut self, func: &str, args: &[Value]) -> Result<Value, String>;
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TraceEntry {
    pub kind: String, // "observe" | "effect" | "pure" | "note"
    pub verb: String,
    pub ok: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum StopReason {
    Completed,
    ConsentMismatch(String),
    AdmissionRejected(String),
    BudgetExhausted(String),
    BoundExceeded { bound: u32, actual: usize },
    EscalateRequested { bound: u32, actual: usize },
    ExecFailed(String),
    GuardError(String),
}

#[derive(Debug, serde::Serialize)]
pub struct RunOutcome {
    pub status: StopReason,
    pub trace: Vec<TraceEntry>,
    pub effects_executed: u64,
}

struct Rt<'a> {
    exec: &'a mut dyn EffectExec,
    schemas: &'a VerbSchemas,
    budget: BTreeMap<String, u64>,
    env: BTreeMap<String, (Value, Label)>,
    trace: Vec<TraceEntry>,
    effects_executed: u64,
}

enum Halt {
    Stop(StopReason),
}

pub fn run_plan(
    plan_bytes: &[u8],
    consent: &ConsentRecord,
    key: &ConsentKey,
    schemas: &VerbSchemas,
    exec: &mut dyn EffectExec,
    audit: &mut AuditLog,
) -> Result<RunOutcome, KernelError> {
    // 1. WYSIWYS: the consent must be authentic, unexpired, and bound to
    //    exactly these plan bytes (h_plan = CAS id of the bytes).
    let now = crate::db::now_unix();
    if let Err(e) = consent.verify(key, now) {
        return finish(
            audit,
            RunOutcome {
                status: StopReason::ConsentMismatch(e.to_string()),
                trace: vec![],
                effects_executed: 0,
            },
        );
    }
    let h = id_for_bytes(plan_bytes);
    if h != consent.plan_hash {
        return finish(
            audit,
            RunOutcome {
                status: StopReason::ConsentMismatch(format!(
                    "plan hash {h} != consented {}",
                    consent.plan_hash
                )),
                trace: vec![],
                effects_executed: 0,
            },
        );
    }

    // 2. Admission (verifier): structure + labels + budget, all static.
    let plan = match Plan::from_bytes(plan_bytes) {
        Ok(p) => p,
        Err(e) => {
            return finish(
                audit,
                RunOutcome {
                    status: StopReason::AdmissionRejected(format!("parse: {e}")),
                    trace: vec![],
                    effects_executed: 0,
                },
            );
        }
    };
    let adm = match admit(&plan, schemas) {
        Ok(a) => a,
        Err(e) => {
            return finish(
                audit,
                RunOutcome {
                    status: StopReason::AdmissionRejected(e.to_string()),
                    trace: vec![],
                    effects_executed: 0,
                },
            );
        }
    };
    // Derived worst case must fit inside what the user consented to.
    for (verb, n) in &adm.budget {
        if consent.budget.get(verb).copied().unwrap_or(0) < *n {
            return finish(
                audit,
                RunOutcome {
                    status: StopReason::AdmissionRejected(format!(
                        "derived budget {verb}={n} exceeds consent"
                    )),
                    trace: vec![],
                    effects_executed: 0,
                },
            );
        }
    }
    audit.append(json!({
        "event": "plan.admitted", "plan": h, "budget": adm.budget,
        "worst_case_steps": adm.worst_case_steps,
    }))?;

    // 3. Execute under the runtime monitor (defense in depth: the counting
    //    check re-fires per effect even though admission already bounded it).
    let mut rt = Rt {
        exec,
        schemas,
        budget: consent.budget.clone(),
        env: BTreeMap::new(),
        trace: Vec::new(),
        effects_executed: 0,
    };
    let status = match exec_stmts(&mut rt, &plan.stmts, &Label::public_trusted()) {
        Ok(()) => StopReason::Completed,
        Err(Halt::Stop(r)) => r,
    };
    let out = RunOutcome {
        status,
        trace: rt.trace,
        effects_executed: rt.effects_executed,
    };
    finish(audit, out)
}

fn finish(audit: &mut AuditLog, out: RunOutcome) -> Result<RunOutcome, KernelError> {
    audit.append(json!({
        "event": "plan.finished",
        "status": format!("{:?}", out.status),
        "effects": out.effects_executed,
    }))?;
    Ok(out)
}

fn exec_stmts(rt: &mut Rt, stmts: &[Stmt], pc: &Label) -> Result<(), Halt> {
    for s in stmts {
        match s {
            Stmt::Let { var, expr } => {
                let vl = eval(rt, expr)?;
                rt.env.insert(var.clone(), vl);
            }
            Stmt::Effect { verb, args } => {
                let mut eff_label = pc.clone();
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    let (v, l) = eval(rt, a)?;
                    eff_label = eff_label.join(&l);
                    vals.push(v);
                }
                // Monitor: sink policy (dynamic re-check mirrors admission).
                let external = rt
                    .schemas
                    .external_effects
                    .get(verb)
                    .copied()
                    .unwrap_or(true);
                if external && !eff_label.conf.is_empty() {
                    rt.trace.push(TraceEntry {
                        kind: "effect".into(),
                        verb: verb.clone(),
                        ok: false,
                        detail: Some("sink denied".into()),
                    });
                    return Err(Halt::Stop(StopReason::ExecFailed(format!(
                        "sink denied: {verb}"
                    ))));
                }
                // Monitor: counting budget, never overdraw.
                match rt.budget.get_mut(verb) {
                    Some(n) if *n > 0 => *n -= 1,
                    _ => {
                        rt.trace.push(TraceEntry {
                            kind: "effect".into(),
                            verb: verb.clone(),
                            ok: false,
                            detail: Some("budget exhausted".into()),
                        });
                        return Err(Halt::Stop(StopReason::BudgetExhausted(verb.clone())));
                    }
                }
                match rt.exec.effect(verb, &vals) {
                    Ok(()) => {
                        rt.effects_executed += 1;
                        rt.trace.push(TraceEntry {
                            kind: "effect".into(),
                            verb: verb.clone(),
                            ok: true,
                            detail: None,
                        });
                    }
                    Err(e) => {
                        rt.trace.push(TraceEntry {
                            kind: "effect".into(),
                            verb: verb.clone(),
                            ok: false,
                            detail: Some(e.clone()),
                        });
                        return Err(Halt::Stop(StopReason::ExecFailed(e)));
                    }
                }
            }
            Stmt::If {
                guard,
                then_,
                else_,
            } => {
                let (b, gl) = eval_guard(rt, guard)?;
                let pc2 = pc.join(&gl);
                exec_stmts(rt, if b { then_ } else { else_ }, &pc2)?;
            }
            Stmt::Foreach {
                var,
                list,
                bound,
                mode,
                body,
            } => {
                let (lv, ll) = eval(rt, list)?;
                let items = lv.as_array().cloned().unwrap_or_default();
                let n = items.len();
                let take = if n > *bound as usize {
                    match mode {
                        Mode::Strict => {
                            rt.trace.push(TraceEntry {
                                kind: "note".into(),
                                verb: "foreach".into(),
                                ok: false,
                                detail: Some(format!("strict bound {bound} < {n}")),
                            });
                            return Err(Halt::Stop(StopReason::BoundExceeded {
                                bound: *bound,
                                actual: n,
                            }));
                        }
                        Mode::Truncate => {
                            rt.trace.push(TraceEntry {
                                kind: "note".into(),
                                verb: "foreach".into(),
                                ok: true,
                                detail: Some(format!(
                                    "truncated to {bound} of {n} — reported, never silent"
                                )),
                            });
                            *bound as usize
                        }
                        Mode::Escalate => {
                            return Err(Halt::Stop(StopReason::EscalateRequested {
                                bound: *bound,
                                actual: n,
                            }));
                        }
                    }
                } else {
                    n
                };
                let pc2 = pc.join(&ll);
                for item in items.into_iter().take(take) {
                    rt.env.insert(var.clone(), (item, ll.clone()));
                    exec_stmts(rt, body, &pc2)?;
                }
            }
        }
    }
    Ok(())
}

fn eval(rt: &mut Rt, e: &Expr) -> Result<(Value, Label), Halt> {
    match e {
        Expr::Observe { verb, args } => {
            let base = rt.schemas.observe.get(verb).cloned().ok_or_else(|| {
                Halt::Stop(StopReason::ExecFailed(format!("unknown verb {verb}")))
            })?;
            let mut label = base;
            let mut vals = Vec::new();
            for a in args {
                let (v, l) = eval(rt, a)?;
                label = label.join(&l);
                vals.push(v);
            }
            let v = rt
                .exec
                .observe(verb, &vals)
                .map_err(|e| Halt::Stop(StopReason::ExecFailed(e)))?;
            rt.trace.push(TraceEntry {
                kind: "observe".into(),
                verb: verb.clone(),
                ok: true,
                detail: None,
            });
            Ok((v, label))
        }
        Expr::Pure { func, args } => {
            let mut label = Label::public_trusted();
            let mut vals = Vec::new();
            for a in args {
                let (v, l) = eval(rt, a)?;
                label = label.join(&l);
                vals.push(v);
            }
            let v = rt
                .exec
                .pure(func, &vals)
                .map_err(|e| Halt::Stop(StopReason::ExecFailed(e)))?;
            rt.trace.push(TraceEntry {
                kind: "pure".into(),
                verb: func.clone(),
                ok: true,
                detail: None,
            });
            Ok((v, label))
        }
        Expr::Const { value } => Ok((value.clone(), Label::public_trusted())),
        Expr::Var { name } => rt
            .env
            .get(name)
            .cloned()
            .ok_or_else(|| Halt::Stop(StopReason::ExecFailed(format!("unknown var {name}")))),
        Expr::Index { base, idx } => {
            let (v, l) = eval(rt, base)?;
            let item = v
                .as_array()
                .and_then(|a| a.get(*idx as usize))
                .cloned()
                .unwrap_or(Value::Null);
            Ok((item, l))
        }
    }
}

fn eval_guard(rt: &mut Rt, g: &Guard) -> Result<(bool, Label), Halt> {
    Ok(match g {
        Guard::Exists { expr } => {
            let (v, l) = eval(rt, expr)?;
            (
                v.as_array().map(|a| !a.is_empty()).unwrap_or(!v.is_null()),
                l,
            )
        }
        Guard::Matches { expr, regex } => {
            let (v, l) = eval(rt, expr)?;
            let re = regex::Regex::new(regex)
                .map_err(|e| Halt::Stop(StopReason::GuardError(e.to_string())))?;
            (v.as_str().map(|s| re.is_match(s)).unwrap_or(false), l)
        }
        Guard::Cmp { lhs, op, rhs } => {
            let (a, la) = eval(rt, lhs)?;
            let (b, lb) = eval(rt, rhs)?;
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
            let (a, la) = eval_guard(rt, l)?;
            let (b, lb) = eval_guard(rt, r)?;
            (a && b, la.join(&lb))
        }
        Guard::Or { l, r } => {
            let (a, la) = eval_guard(rt, l)?;
            let (b, lb) = eval_guard(rt, r)?;
            (a || b, la.join(&lb))
        }
        Guard::Not { g } => {
            let (a, l) = eval_guard(rt, g)?;
            (!a, l)
        }
    })
}

fn cmp_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}
