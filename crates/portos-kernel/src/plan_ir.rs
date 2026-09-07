//! WP-05: the bridge from the M0 plan AST (`portos-proto::plan`) to the law
//! crate's metering shapes (`portos_rm::coeffect::Plan`), via the local mirror
//! `coeffect::AstNode`. The projection keeps exactly the four metering shapes
//! (verb, sequence, bounded loop, branch); statements that never bear a budget
//! (`Let`, and reads — `Observe`/`Pure` are repeatable, hence free via the
//! truth table) collapse to transparent nodes.
//!
//! Compiled in since D41 (G1 lifted D31).

use portos_proto::{Plan, Stmt};
use portos_rm::coeffect::AstNode;

/// Project a proto plan into the metering-shape mirror (one node per
/// statement; nesting preserved).
pub fn to_ast_nodes(plan: &Plan) -> Vec<AstNode> {
    plan.stmts.iter().map(node_of).collect()
}

fn node_of(s: &Stmt) -> AstNode {
    match s {
        Stmt::Effect { verb, .. } => AstNode::Effect { verb: verb.clone() },
        Stmt::Foreach { bound, body, .. } => AstNode::Loop {
            bound: *bound as u64,
            body: body.iter().map(node_of).collect(),
        },
        Stmt::If { then_, else_, .. } => AstNode::Branch {
            then: then_.iter().map(node_of).collect(),
            else_: else_.iter().map(node_of).collect(),
        },
        Stmt::Let { .. } => AstNode::Opaque,
    }
}
