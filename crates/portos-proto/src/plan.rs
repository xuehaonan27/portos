//! Effect-plan AST. The total plan language L.
//!
//! Plans are submitted as JSON. The submitted bytes go into the CAS verbatim,
//! so `h_plan` is the plan artifact's CAS id. No JSON canonicalization is
//! needed anywhere (consent, audit, and pinning all reference the same hash),
//!
//! No `while`, general recursion, function definitions (pure computation
//! lives in the compute plugin), guards are a tiny total predicate language.
//!
//! TODO: this is only a prototype. Whether it should be built on top of
//! current programming languages?

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub stmts: Vec<Stmt>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Stmt {
    Let {
        var: String,
        expr: Expr,
    },
    Effect {
        verb: String,
        #[serde(default)]
        args: Vec<Expr>,
    },
    If {
        guard: Guard, // cannot be `Expr`.
        then_: Vec<Stmt>,
        #[serde(default)]
        else_: Vec<Stmt>,
    },
    Foreach {
        var: String,
        list: Expr,
        bound: u32,
        #[serde(default)]
        mode: Mode,
        body: Vec<Stmt>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Expr {
    /// Observation verb (read): capability-checked, label-recorded,
    /// never budget-consuming.
    Observe {
        verb: String,
        #[serde(default)]
        args: Vec<Expr>,
    },
    /// Pure computation delegated to the zero-capability compute plugin.
    Pure {
        func: String,
        #[serde(default)]
        args: Vec<Expr>,
    },
    Const {
        value: Value,
    },
    Var {
        name: String,
    },
    /// Index into a list value produced by an observation.
    Index {
        base: Box<Expr>,
        idx: u32,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Effect-loop default. If cardinality > bound, then fail-stop.
    #[default]
    Strict,
    /// Read-loop default. Process first N, report truncation as data.
    Truncate,
    /// Pause and request incremental consent.
    Escalate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Guard {
    /// Does a list-valued expression have at least one element?
    Exists {
        expr: Box<Expr>,
    },
    /// Regex match on the string value of an expression.
    Matches {
        expr: Box<Expr>,
        regex: String,
    },
    Cmp {
        lhs: Box<Expr>,
        op: CmpOp,
        rhs: Box<Expr>,
    },
    And {
        l: Box<Guard>,
        r: Box<Guard>,
    },
    Or {
        l: Box<Guard>,
        r: Box<Guard>,
    },
    Not {
        g: Box<Guard>,
    },
}

impl Plan {
    pub fn from_bytes(bytes: &[u8]) -> Result<Plan, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ast_roundtrips_through_json() {
        let p = Plan {
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
        let s = serde_json::to_vec(&p).unwrap();
        let p2 = Plan::from_bytes(&s).unwrap();
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::to_value(&p2).unwrap()
        );
    }
}
