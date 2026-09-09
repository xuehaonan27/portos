//! Evaluate expressions and guards while carrying information-flow labels.

use super::Interpreter;
use crate::plans::Outcome;
use portos_proto::{CmpOp, Expr, Guard, Label};
use portos_rm::identity::VerbId;
use serde_json::{Value, json};

impl Interpreter<'_> {
    pub(super) fn eval(&mut self, e: &Expr) -> Result<(Value, Label), Outcome> {
        match e {
            Expr::Observe { verb, args } => {
                let base =
                    self.schemas
                        .observe
                        .get(verb)
                        .cloned()
                        .ok_or_else(|| Outcome::FailStop {
                            at: format!("unknown verb {verb}"),
                        })?;
                let mut label = base;
                let mut vals = Vec::new();
                for a in args {
                    let (v, l) = self.eval(a)?;
                    label = label.join(&l);
                    vals.push(v);
                }
                let args_json = if vals.len() == 1 {
                    vals.remove(0)
                } else {
                    Value::Array(vals)
                };
                // Reads go through the same capability gate (minted uncounted)
                // and are metered, never budgeted (truth table).
                let v = self
                    .svc
                    .runtime
                    .invoke(self.fiber, &VerbId::new(verb), args_json)
                    .map_err(|e| Outcome::FailStop {
                        at: format!("{verb}: {e}"),
                    })?;
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
                // Pure computation runs in the zero-capability compute PLUGIN
                // (methodology audit: never in the kernel). It rides the same
                // capability gate as any verb; repeatable, so never budgeted.
                let v = self
                    .svc
                    .runtime
                    .invoke(
                        self.fiber,
                        &VerbId::new("compute::run"),
                        json!({"func": func, "args": vals}),
                    )
                    .map_err(|e| Outcome::FailStop {
                        at: format!("pure {func}: {e}"),
                    })?;
                Ok((v, label))
            }
            Expr::Const { value } => Ok((value.clone(), Label::public_trusted())),
            Expr::Var { name } => self
                .env
                .get(name)
                .cloned()
                .ok_or_else(|| Outcome::FailStop {
                    at: format!("unknown var {name}"),
                }),
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

    pub(super) fn eval_guard(&mut self, g: &Guard) -> Result<(bool, Label), Outcome> {
        Ok(match g {
            Guard::Exists { expr } => {
                let (v, l) = self.eval(expr)?;
                (
                    v.as_array().map(|a| !a.is_empty()).unwrap_or(!v.is_null()),
                    l,
                )
            }
            Guard::Matches { expr, regex } => {
                let (v, l) = self.eval(expr)?;
                let re = regex::Regex::new(regex).map_err(|e| Outcome::FailStop {
                    at: format!("guard regex: {e}"),
                })?;
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
