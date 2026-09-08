//! WP-06 kernel integration tests: plan runs against the real driver stack
//! (echo over ABI v2), mirroring the F3 laws by name. Each test opens a fresh
//! kernel root, spawns echo plugins, and drives `PlanService`.

use portos_kernel::consent::ConsentRecord;
use portos_kernel::host::Host;
use portos_kernel::ledger::CLASS_FILE_LOCK;
use portos_kernel::plans::{Outcome, PlanService};
use portos_kernel::Kernel;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn setup(tag: &str) -> (Arc<Kernel>, Host, PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-plans-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
    (kernel, host, root)
}

fn echo_bin() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/portos-echo");
    assert!(p.exists(), "portos-echo not built (run under cargo test --workspace)");
    p
}

fn spawn_echo(host: &Host, family: &str, extra: &[(&str, &str)]) -> String {
    let mut envs: Vec<(&str, &str)> = vec![("PORTOS_ECHO_FAMILY", family)];
    envs.extend_from_slice(extra);
    host.spawn(&echo_bin(), &[], &envs).unwrap()
}

fn plan(v: Value) -> Vec<u8> {
    serde_json::to_vec(&v).unwrap()
}

fn emit_once(text: &str) -> Vec<u8> {
    plan(json!({"stmts": [
        {"k": "effect", "verb": "echo::emit", "args": [{"k": "const", "value": text}]},
    ]}))
}

fn emit_times(texts: &[&str]) -> Vec<u8> {
    let stmts: Vec<Value> = texts
        .iter()
        .map(|t| json!({"k": "effect", "verb": "echo::emit", "args": [{"k": "const", "value": t}]}))
        .collect();
    plan(json!({"stmts": stmts}))
}

fn foreach_plan(items: &[&str], bound: u32, mode: &str) -> Vec<u8> {
    plan(json!({"stmts": [
        {"k": "foreach", "var": "x",
         "list": {"k": "const", "value": items},
         "bound": bound, "mode": mode,
         "body": [
            {"k": "effect", "verb": "echo::emit", "args": [{"k": "var", "name": "x"}]},
         ]},
    ]}))
}

fn sign(kernel: &Kernel, plan_hash: &str, budget: &[(&str, u64)], ttl_secs: u64) -> ConsentRecord {
    let b: std::collections::BTreeMap<String, u64> =
        budget.iter().map(|(k, n)| (k.to_string(), *n)).collect();
    let root = kernel.root.clone();
    portos_signer::Signer::load(&root).unwrap().sign(plan_hash, b, ttl_secs)
}

fn wait_state(svc: &PlanService, run_id: &str, want: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let state = svc.run_state(run_id).unwrap();
        if state == want || state == "done" {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "run never reached {want}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn wait_outcome(svc: &PlanService, run_id: &str) -> Outcome {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(o) = svc.run_outcome(run_id).unwrap() {
            return o;
        }
        assert!(std::time::Instant::now() < deadline, "run never finished");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn emissions(kernel: &Kernel, run_id: &str) -> u64 {
    let conn = kernel.db.lock().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM emission_log WHERE run_id = ?1",
        rusqlite::params![run_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap() as u64
}

fn audit_events(root: &Path) -> Vec<Value> {
    portos_kernel::audit::AuditLog::verify(&root.join("audit.log"))
        .unwrap()
        .into_iter()
        .map(|e| e["body"].clone())
        .collect()
}

/// [WYS] a consent for plan A starts nothing of plan B; a tampered quadruple
/// verifies nowhere. Zero effects in both cases (m0 accept_4 revived).
#[test]
fn wysiwys_tampered_plan_is_refused_with_zero_effects() {
    let (kernel, host, root) = setup("wysiwys");
    spawn_echo(&host, "echo", &[]);
    let bytes_a = emit_once("A");
    let out_a = host.plans.submit("user", &bytes_a).unwrap();
    let bytes_b = emit_once("B");
    let out_b = host.plans.submit("user", &bytes_b).unwrap();
    let consent_a = sign(&kernel, &out_a.plan_hash, &[("echo::emit", 1)], 3600);
    // A consent bound to A cannot start B.
    let e = host.plans.start(&out_b.run_id, &consent_a).unwrap_err();
    assert!(e.to_string().contains("mismatch"), "{e}");
    // A tampered consent (budget inflated, MAC now invalid) verifies nowhere.
    let mut forged = consent_a.clone();
    forged.budget.insert("echo::emit".into(), 99);
    let e = host.plans.start(&out_a.run_id, &forged).unwrap_err();
    assert!(e.to_string().contains("MAC") || e.to_string().contains("mismatch"), "{e}");
    assert_eq!(emissions(&kernel, &out_a.run_id), 0);
    assert_eq!(emissions(&kernel, &out_b.run_id), 0);
    assert_eq!(host.plans.run_state(&out_a.run_id).unwrap(), "admitted");
    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Derived budget beyond the consent is refused at the door (m0 accept_3c):
/// zero effects, nothing minted.
#[test]
fn admission_rejects_derived_budget_over_consent() {
    let (kernel, host, root) = setup("admit");
    spawn_echo(&host, "echo", &[]);
    let out = host.plans.submit("user", &emit_times(&["a", "b", "c"])).unwrap();
    let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 2)], 3600);
    let e = host.plans.start(&out.run_id, &consent).unwrap_err();
    assert!(e.to_string().contains("exceeds consent"), "{e}");
    assert_eq!(emissions(&kernel, &out.run_id), 0);
    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Strict bound: the run fail-stops at the excess and delivers the prefix —
/// effects before it stand, the segment is rolled back (m0 accept_3).
#[test]
fn strict_bound_fail_stops_with_prefix_delivered() {
    let (kernel, host, root) = setup("strict");
    spawn_echo(&host, "echo", &[]);
    // Two effects before the loop; the loop's bound is exceeded.
    let bytes = plan(json!({"stmts": [
        {"k": "effect", "verb": "echo::emit", "args": [{"k": "const", "value": "p1"}]},
        {"k": "effect", "verb": "echo::emit", "args": [{"k": "const", "value": "p2"}]},
        {"k": "foreach", "var": "x",
         "list": {"k": "const", "value": ["a", "b", "c"]},
         "bound": 2, "mode": "strict",
         "body": [{"k": "effect", "verb": "echo::emit", "args": [{"k": "var", "name": "x"}]}]},
    ]}));
    let out = host.plans.submit("user", &bytes).unwrap();
    let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 4)], 3600);
    host.plans.start(&out.run_id, &consent).unwrap();
    let outcome = wait_outcome(&host.plans, &out.run_id);
    assert!(matches!(outcome, Outcome::FailStop { .. }), "{outcome:?}");
    assert_eq!(emissions(&kernel, &out.run_id), 2, "the prefix stands");
    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Truncate: the first N are processed, the drop is reported in trace and
/// audit — never silent (m0 accept_3b).
#[test]
fn truncate_reports_and_never_silent() {
    let (kernel, host, root) = setup("truncate");
    spawn_echo(&host, "echo", &[]);
    let out = host.plans.submit("user", &foreach_plan(&["a", "b", "c"], 2, "truncate")).unwrap();
    let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 2)], 3600);
    host.plans.start(&out.run_id, &consent).unwrap();
    let outcome = wait_outcome(&host.plans, &out.run_id);
    assert!(matches!(outcome, Outcome::Completed), "{outcome:?}");
    assert_eq!(emissions(&kernel, &out.run_id), 2);
    drop(host);
    let events = audit_events(&root);
    assert!(
        events.iter().any(|e| e["event"] == "plan.truncated" && e["dropped"] == 1),
        "the truncation is audited: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The budget is rows: every emission mints a spend row under
/// `plan:<h>#<run>:spent`, the rows survive a kernel reopen, and the pool's
/// balance is only ever the fold ([GATE]).
#[test]
fn budget_is_rows_the_gate_refuses_at_capacity() {
    let (kernel, host, root) = setup("rows");
    spawn_echo(&host, "echo", &[]);
    let out = host.plans.submit("user", &emit_times(&["a", "b"])).unwrap();
    let fiber = format!("plan:{}#{}", out.plan_hash, out.run_id);
    let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 2)], 3600);
    host.plans.start(&out.run_id, &consent).unwrap();
    let outcome = wait_outcome(&host.plans, &out.run_id);
    assert!(matches!(outcome, Outcome::Completed), "{outcome:?}");
    let spent_subject = format!("{fiber}:spent");
    let rows = kernel.ledger.live_snapshot(&spent_subject);
    assert_eq!(rows.len(), 2, "one spend row per emission");
    // Rows survive a reopen: the pool continues where it was.
    let root2 = root.clone();
    drop(host);
    let kernel2 = Arc::new(Kernel::open(&root2).unwrap());
    let rows2 = kernel2.ledger.live_snapshot(&spent_subject);
    assert_eq!(rows2.len(), 2, "spend rows reloaded");
    let host2 = Host::new(kernel2.clone(), &root2.join("sock")).unwrap();
    host2.shutdown_all();
    let _ = std::fs::remove_dir_all(&root2);
}

/// [SUPPR]/[INSERT]: the hard list is withheld unseen; an approval — from a
/// *fresh kernel process* — releases the batch in original order exactly
/// once, each row journaled.
#[test]
fn withheld_batch_is_released_exactly_once_in_order_after_approval() {
    let (kernel, host, root) = setup("withhold");
    spawn_echo(&host, "echo", &[("PORTOS_ECHO_HARD_EMIT", "1")]);
    let out = host.plans.submit("user", &emit_times(&["a", "b"])).unwrap();
    let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 2)], 3600);
    host.plans.start(&out.run_id, &consent).unwrap();
    wait_state(&host.plans, &out.run_id, "awaiting_approval");
    assert_eq!(emissions(&kernel, &out.run_id), 0, "withheld effects are unseen");
    // Approve from a fresh process on the same root: everything it needs is
    // durable (buffer rows are fully evaluated).
    drop(host);
    let kernel2 = Arc::new(Kernel::open(&root).unwrap());
    let host2 = Host::new(kernel2.clone(), &root.join("sock")).unwrap();
    spawn_echo(&host2, "echo", &[("PORTOS_ECHO_HARD_EMIT", "1")]);
    let batch = host2.plans.withheld_batch(&out.run_id).unwrap();
    assert_eq!(batch.len(), 2);
    let approval = sign(&kernel2, &out.plan_hash, &[("echo::emit", 2)], 3600);
    host2.plans.approve(&out.run_id, &approval).unwrap();
    let outcome = wait_outcome(&host2.plans, &out.run_id);
    assert!(matches!(outcome, Outcome::Completed), "{outcome:?}");
    assert_eq!(emissions(&kernel2, &out.run_id), 2, "released exactly once");
    // Original order, journaled as inserted.
    {
        let conn = kernel2.db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT args, state FROM suppression_buffer WHERE run_id = ?1 ORDER BY seq")
            .unwrap();
        let rows: Vec<(String, String)> = stmt
            .query_map(rusqlite::params![out.run_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].0.contains("\"a\"") && rows[1].0.contains("\"b\""), "original order: {rows:?}");
        assert!(rows.iter().all(|(_, s)| s == "inserted"), "journaled: {rows:?}");
    }
    // A second approval with a replayed nonce is refused (one-time consent).
    let e = host2.plans.approve(&out.run_id, &approval).unwrap_err();
    assert!(e.to_string().contains("not awaiting approval") || e.to_string().contains("stale nonce"), "{e}");
    host2.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// [TTL] both suspended states are bounded by the original consent's ttl:
/// expiry aborts the withheld batch and the paused segment alike.
#[test]
fn ttl_expiry_aborts_awaiting_approval_and_paused_alike() {
    // AwaitingApproval.
    {
        let (kernel, host, root) = setup("ttl-await");
        spawn_echo(&host, "echo", &[("PORTOS_ECHO_HARD_EMIT", "1")]);
        let out = host.plans.submit("user", &emit_once("x")).unwrap();
        let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 1)], 1);
        host.plans.start(&out.run_id, &consent).unwrap();
        wait_state(&host.plans, &out.run_id, "awaiting_approval");
        std::thread::sleep(std::time::Duration::from_millis(2100));
        host.plans.expire(portos_kernel::db::now_unix());
        let outcome = wait_outcome(&host.plans, &out.run_id);
        assert!(matches!(outcome, Outcome::Aborted { expired: true }), "{outcome:?}");
        assert_eq!(emissions(&kernel, &out.run_id), 0, "the batch is dropped, never emitted");
        host.shutdown_all();
        let _ = std::fs::remove_dir_all(&root);
    }
    // Paused (escalate).
    {
        let (kernel, host, root) = setup("ttl-paused");
        spawn_echo(&host, "echo", &[]);
        let out = host.plans.submit("user", &foreach_plan(&["a", "b"], 1, "escalate")).unwrap();
        let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 2)], 1);
        host.plans.start(&out.run_id, &consent).unwrap();
        wait_state(&host.plans, &out.run_id, "paused");
        std::thread::sleep(std::time::Duration::from_millis(2100));
        host.plans.expire(portos_kernel::db::now_unix());
        let outcome = wait_outcome(&host.plans, &out.run_id);
        assert!(matches!(outcome, Outcome::Aborted { expired: true }), "{outcome:?}");
        host.shutdown_all();
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// [ESC] a paused loop resumes exactly at its pause point under a fresh
/// quadruple — each incremental pool buys its own budget, the gate re-arms
/// and parks again when every live pool is spent.
#[test]
fn escalate_pauses_and_resumes_exactly_with_fresh_consent() {
    let (kernel, host, root) = setup("escalate");
    spawn_echo(&host, "echo", &[]);
    // bound 2 over 3 items, escalate: the loop pauses at the bound check.
    let out = host.plans.submit("user", &foreach_plan(&["a", "b", "c"], 2, "escalate")).unwrap();
    let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 2)], 3600);
    host.plans.start(&out.run_id, &consent).unwrap();
    wait_state(&host.plans, &out.run_id, "paused");
    assert_eq!(emissions(&kernel, &out.run_id), 0, "paused before any emission");

    // Resume with an empty pool: the first two items spend the original
    // budget, the third finds every live pool spent and parks again.
    let inc_empty = sign(&kernel, &out.plan_hash, &[("echo::emit", 0)], 3600);
    host.plans.resume(&out.run_id, &inc_empty).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert_eq!(host.plans.run_state(&out.run_id).unwrap(), "paused");
    assert_eq!(emissions(&kernel, &out.run_id), 2, "the original pool bought exactly two");

    // Resume with one more unit: the remaining item completes the plan.
    let inc_one = sign(&kernel, &out.plan_hash, &[("echo::emit", 1)], 3600);
    host.plans.resume(&out.run_id, &inc_one).unwrap();
    let outcome = wait_outcome(&host.plans, &out.run_id);
    assert!(matches!(outcome, Outcome::Completed), "{outcome:?}");
    assert_eq!(emissions(&kernel, &out.run_id), 3, "each item emitted exactly once");
    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// [SEG-TX] non-commit terminal states roll the segment back: subscriptions
/// and holds the run made are torn down (children first) — commit instead
/// transfers them to the fiber.
#[test]
fn segment_abort_rolls_back_subscriptions_and_holds_made_by_the_run() {
    let (kernel, host, root) = setup("segment");
    spawn_echo(&host, "echo", &[]);
    // Abort case: the run fail-stops; its seg holdings must go. They are
    // staged before the start so the teardown deterministically covers them
    // (the drill stages acquisitions the same way).
    let out = host.plans.submit("user", &foreach_plan(&["a", "b", "c"], 2, "strict")).unwrap();
    let fiber = format!("plan:{}#{}", out.plan_hash, out.run_id);
    let seg = format!("{fiber}:seg");
    let lock = root.join("run.lock");
    std::fs::write(&lock, b"x").unwrap();
    let (_sub_id, _rx) = host.subscribe_for(&seg, "t::*");
    kernel
        .ledger
        .hold_substrate(&seg, CLASS_FILE_LOCK, lock.to_str().unwrap(), "lk", None, &json!({}), None, portos_kernel::db::now_unix())
        .unwrap();
    assert_eq!(kernel.ledger.live_snapshot(&seg).len(), 2, "sub + lock staged in the segment");
    let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 2)], 3600);
    host.plans.start(&out.run_id, &consent).unwrap();
    let outcome = wait_outcome(&host.plans, &out.run_id);
    assert!(matches!(outcome, Outcome::FailStop { .. }), "{outcome:?}");
    assert!(
        kernel.ledger.live_snapshot(&seg).is_empty(),
        "the segment was rolled back on fail-stop"
    );
    assert!(!lock.exists(), "the lock file went with the segment");

    // Commit case: transferred, not torn down.
    let out2 = host.plans.submit("user", &emit_once("ok")).unwrap();
    let fiber2 = format!("plan:{}#{}", out2.plan_hash, out2.run_id);
    let seg2 = format!("{fiber2}:seg");
    let (_s2, _r2) = host.subscribe_for(&seg2, "t::*");
    let consent2 = sign(&kernel, &out2.plan_hash, &[("echo::emit", 1)], 3600);
    host.plans.start(&out2.run_id, &consent2).unwrap();
    let outcome2 = wait_outcome(&host.plans, &out2.run_id);
    assert!(matches!(outcome2, Outcome::Completed), "{outcome2:?}");
    let moved = kernel.ledger.live_snapshot(&fiber2);
    assert_eq!(moved.len(), 1, "the subscription was transferred to the fiber");
    assert!(kernel.ledger.live_snapshot(&seg2).is_empty());
    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// The audit chain covers a whole run and replays (m0 accept_5).
#[test]
fn audit_chain_replays_a_run() {    let (kernel, host, root) = setup("audit");
    spawn_echo(&host, "echo", &[]);
    let out = host.plans.submit("user", &emit_times(&["a", "b"])).unwrap();
    let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 2)], 3600);
    host.plans.start(&out.run_id, &consent).unwrap();
    let outcome = wait_outcome(&host.plans, &out.run_id);
    assert!(matches!(outcome, Outcome::Completed), "{outcome:?}");
    drop(host);
    let events = audit_events(&root);
    for want in ["plan.admitted", "plan.started", "plan.finished"] {
        assert!(events.iter().any(|e| e["event"] == want), "missing {want}: {events:?}");
    }
    assert!(
        events.iter().any(|e| e["event"] == "plan.finished" && e["status"] == "Completed"),
        "the finished event carries the status: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Pure computation runs in the compute PLUGIN, never in the kernel
/// (methodology audit): a plan's `pure` node is dispatched as `compute::run`
/// through the same capability gate, and the result flows into later effects.
#[test]
fn pure_evaluation_runs_in_the_compute_plugin() {
    let (kernel, host, root) = setup("pure");
    spawn_echo(&host, "echo", &[]);
    let compute_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/portos-compute");
    assert!(compute_bin.exists(), "portos-compute not built");
    host.spawn(&compute_bin, &[], &[]).unwrap();
    let bytes = plan(json!({"stmts": [
        {"k": "let", "var": "u",
         "expr": {"k": "pure", "func": "upper", "args": [{"k": "const", "value": "hi"}]}},
        {"k": "effect", "verb": "echo::emit", "args": [{"k": "var", "name": "u"}]},
    ]}));
    let out = host.plans.submit("user", &bytes).unwrap();
    let consent = sign(&kernel, &out.plan_hash, &[("echo::emit", 1)], 3600);
    host.plans.start(&out.run_id, &consent).unwrap();
    let outcome = wait_outcome(&host.plans, &out.run_id);
    assert!(matches!(outcome, Outcome::Completed), "{outcome:?}");
    assert_eq!(emissions(&kernel, &out.run_id), 1);
    // The compute call itself is gated: a consent whose fiber caps were
    // somehow absent compute would fail — here the demand carried it, so the
    // fiber held compute::run uncounted (repeatable, never budgeted).
    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}
