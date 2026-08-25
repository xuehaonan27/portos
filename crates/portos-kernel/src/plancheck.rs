//! Admission: the "verifier" half of the two-level eBPF analogy.
//! Three passes over the finite plan tree (effect-plan-v0.md §3.3/§4.3/§5.2):
//!   1. structure  — sizes, depths, bounds, worst-case step ceiling
//!   2. labels     — static pc/label derivation, sink obligations
//!   3. budget     — occurrence-sum over-approximation (sound)
//! All three are O(|plan|)-ish and run before any consent is requested.

use portos_proto::{Expr, Guard, Label, Plan, Stmt};
use std::collections::BTreeMap;

pub const MAX_NODES: usize = 4096;
pub const MAX_DEPTH: usize = 16;
pub const S_MAX: u64 = 100_000;

/// Static label schemas for verbs, provided by driver manifests at
/// registration time. M0: an in-memory map.
#[derive(Clone, Default)]
pub struct VerbSchemas {
    /// observe verb → label its results carry.
    pub observe: BTreeMap<String, Label>,
    /// effect verb → is it an "external" sink (confidentiality-gated)?
    pub external_effects: BTreeMap<String, bool>,
}

#[derive(Debug)]
pub enum AdmitError {
    Parse(String),
    TooLarge {
        nodes: usize,
    },
    TooDeep {
        depth: usize,
    },
    StepCeiling {
        worst_case: u64,
    },
    UnknownVerb(String),
    UnknownVar(String),
    /// Static sink violation: an effect with a confidential effective label
    /// targets an external sink (effect-plan-v0.md §4.4).
    SinkDenied {
        verb: String,
        label: Label,
    },
}

impl std::fmt::Display for AdmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmitError::Parse(e) => write!(f, "parse: {e}"),
            AdmitError::TooLarge { nodes } => write!(f, "plan too large: {nodes} nodes"),
            AdmitError::TooDeep { depth } => write!(f, "plan too deep: {depth}"),
            AdmitError::StepCeiling { worst_case } => {
                write!(f, "worst-case steps {worst_case} exceed S_MAX {S_MAX}")
            }
            AdmitError::UnknownVerb(v) => write!(f, "unknown verb: {v}"),
            AdmitError::UnknownVar(v) => write!(f, "unknown var: {v}"),
            AdmitError::SinkDenied { verb, label } => {
                write!(f, "sink denied: {verb} under label {:?}", label)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct EffectAnn {
    pub verb: String,
    /// join(argument labels, pc) as derived statically.
    pub effective: Label,
    /// multiplier = product of enclosing loop bounds (for budget rendering).
    pub multiplier: u64,
}

#[derive(Debug, Default)]
pub struct Admission {
    pub budget: BTreeMap<String, u64>,
    pub effects: Vec<EffectAnn>,
    pub worst_case_steps: u64,
    pub node_count: usize,
}

struct Walk<'a> {
    schemas: &'a VerbSchemas,
    env: BTreeMap<String, Label>,
    out: Admission,
}

pub fn admit(plan: &Plan, schemas: &VerbSchemas) -> Result<Admission, AdmitError> {
    let mut w = Walk {
        schemas,
        env: BTreeMap::new(),
        out: Admission::default(),
    };
    walk_stmts(&mut w, &plan.stmts, &Label::public_trusted(), 1, 1)?;
    if w.out.node_count > MAX_NODES {
        return Err(AdmitError::TooLarge {
            nodes: w.out.node_count,
        });
    }
    if w.out.worst_case_steps > S_MAX {
        return Err(AdmitError::StepCeiling {
            worst_case: w.out.worst_case_steps,
        });
    }
    Ok(w.out)
}

fn walk_stmts(
    w: &mut Walk,
    stmts: &[Stmt],
    pc: &Label,
    mult: u64,
    depth: usize,
) -> Result<(), AdmitError> {
    if depth > MAX_DEPTH {
        return Err(AdmitError::TooDeep { depth });
    }
    for s in stmts {
        w.out.node_count += 1;
        w.out.worst_case_steps = w.out.worst_case_steps.saturating_add(mult);
        match s {
            Stmt::Let { var, expr } => {
                let l = expr_label(w, expr)?;
                w.env.insert(var.clone(), l);
            }
            Stmt::Effect { verb, args } => {
                let external = *w
                    .schemas
                    .external_effects
                    .get(verb)
                    .ok_or_else(|| AdmitError::UnknownVerb(verb.clone()))?;
                let mut eff = pc.clone();
                for a in args {
                    eff = eff.join(&expr_label(w, a)?);
                }
                // M0 sink policy: confidential data may not reach an
                // external effect. (Origin-scoped policies arrive with M1.)
                if external && !eff.conf.is_empty() {
                    return Err(AdmitError::SinkDenied {
                        verb: verb.clone(),
                        label: eff,
                    });
                }
                *w.out.budget.entry(verb.clone()).or_insert(0) += mult;
                w.out.effects.push(EffectAnn {
                    verb: verb.clone(),
                    effective: eff,
                    multiplier: mult,
                });
            }
            Stmt::If {
                guard,
                then_,
                else_,
            } => {
                let gl = guard_label(w, guard)?;
                let pc2 = pc.join(&gl);
                walk_stmts(w, then_, &pc2, mult, depth + 1)?;
                walk_stmts(w, else_, &pc2, mult, depth + 1)?;
            }
            Stmt::Foreach {
                var,
                list,
                bound,
                body,
                ..
            } => {
                let ll = expr_label(w, list)?;
                // Loop variable carries the list's label; the iteration count
                // itself also informs pc (how many rounds ran is observable),
                // conservatively joined here.
                w.env.insert(var.clone(), ll.clone());
                let pc2 = pc.join(&ll);
                let m2 = mult.saturating_mul(*bound as u64);
                walk_stmts(w, body, &pc2, m2, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn expr_label(w: &mut Walk, e: &Expr) -> Result<Label, AdmitError> {
    w.out.node_count += 1;
    Ok(match e {
        Expr::Observe { verb, args } => {
            let base = w
                .schemas
                .observe
                .get(verb)
                .ok_or_else(|| AdmitError::UnknownVerb(verb.clone()))?
                .clone();
            let mut l = base;
            for a in args {
                l = l.join(&expr_label(w, a)?);
            }
            l
        }
        Expr::Pure { args, .. } => {
            let mut l = Label::public_trusted();
            for a in args {
                l = l.join(&expr_label(w, a)?);
            }
            l
        }
        Expr::Const { .. } => Label::public_trusted(),
        Expr::Var { name } => w
            .env
            .get(name)
            .cloned()
            .ok_or_else(|| AdmitError::UnknownVar(name.clone()))?,
        Expr::Index { base, .. } => expr_label(w, base)?,
    })
}

fn guard_label(w: &mut Walk, g: &Guard) -> Result<Label, AdmitError> {
    Ok(match g {
        Guard::Exists { expr } => expr_label(w, expr)?,
        Guard::Matches { expr, .. } => expr_label(w, expr)?,
        Guard::Cmp { lhs, rhs, .. } => expr_label(w, lhs)?.join(&expr_label(w, rhs)?),
        Guard::And { l, r } | Guard::Or { l, r } => guard_label(w, l)?.join(&guard_label(w, r)?),
        Guard::Not { g } => guard_label(w, g)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use portos_proto::Mode;

    fn toy_schemas() -> VerbSchemas {
        let mut s = VerbSchemas::default();
        s.observe
            .insert("echo.list".into(), Label::with_integ("toy:echo"));
        s.observe
            .insert("secret.read".into(), Label::with_conf("secret:demo"));
        s.external_effects.insert("echo.emit".into(), false);
        s.external_effects.insert("external.send".into(), true);
        s
    }

    #[test]
    fn budget_multiplies_through_loops() {
        let plan = Plan {
            stmts: vec![
                Stmt::Let {
                    var: "xs".into(),
                    expr: Expr::Observe {
                        verb: "echo.list".into(),
                        args: vec![],
                    },
                },
                Stmt::Foreach {
                    var: "x".into(),
                    list: Expr::Var { name: "xs".into() },
                    bound: 3,
                    mode: Mode::Strict,
                    body: vec![Stmt::Effect {
                        verb: "echo.emit".into(),
                        args: vec![Expr::Var { name: "x".into() }],
                    }],
                },
            ],
        };
        let adm = admit(&plan, &toy_schemas()).unwrap();
        assert_eq!(adm.budget.get("echo.emit"), Some(&3));
        // the effect's effective label carries the list's taint (pc + arg)
        assert!(adm.effects[0].effective.integ.contains("toy:echo"));
    }

    #[test]
    fn pc_label_blocks_confidential_branch_to_external_sink() {
        // if secret.read() == "A" { external.send("clean-constant") }
        let plan = Plan {
            stmts: vec![Stmt::If {
                guard: Guard::Cmp {
                    lhs: Box::new(Expr::Observe {
                        verb: "secret.read".into(),
                        args: vec![],
                    }),
                    op: portos_proto::CmpOp::Eq,
                    rhs: Box::new(Expr::Const {
                        value: serde_json::json!("A"),
                    }),
                },
                then_: vec![Stmt::Effect {
                    verb: "external.send".into(),
                    args: vec![Expr::Const {
                        value: serde_json::json!("hi"),
                    }],
                }],
                else_: vec![],
            }],
        };
        let err = admit(&plan, &toy_schemas()).unwrap_err();
        assert!(
            matches!(err, AdmitError::SinkDenied { .. }),
            "implicit flow via pc must be caught statically, got: {err}"
        );
    }

    #[test]
    fn same_effect_outside_secret_branch_is_fine() {
        let plan = Plan {
            stmts: vec![Stmt::Effect {
                verb: "external.send".into(),
                args: vec![Expr::Const {
                    value: serde_json::json!("hi"),
                }],
            }],
        };
        assert!(admit(&plan, &toy_schemas()).is_ok());
    }
}
