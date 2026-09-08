//! CLI of portos. Currently links the kernel as a library, and daemonization
//! is deferred.
//!
//!   portos init <root>
//!   portos put <root> <file> [type]
//!   portos meta <root> <artifact-id>
//!   portos get <root> <artifact-id> <out-file>
//!   portos audit-verify <root>
//!   portos consent <root> <plan.json> [--yes]
//!   portos run-plan <root> <plan.json> <consent.json>
//!   portos approve <root> <run_id>
//!   portos resume <root> <run_id>
//!   portos chat <root>
//!
//! `consent` renders the kernel-computed canonical budget (never model
//! prose) and signs the quadruple on approval. `run-plan` executes the plan
//! against the live driver stack under the F3 monitor (WP-06); `approve`
//! releases a withheld batch; `resume` reports on paused runs (escalate
//! resume is in-process, from the run-plan session).

mod chat;

use portos_kernel::consent::{ConsentRecord, render_budget};
use portos_kernel::host::Host;
use portos_kernel::Kernel;
use portos_proto::Label;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Err(e) = dispatch(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn dispatch(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");
    match cmd {
        "init" => {
            let root = need(args, 2, "root")?;
            Kernel::open(std::path::Path::new(&root))?;
            println!("initialized kernel state at {root}");
        }
        "put" => {
            let root = need(args, 2, "root")?;
            let file = need(args, 3, "file")?;
            let ty = args.get(4).cloned().unwrap_or_else(|| "application/octet-stream".into());
            let k = Kernel::open(std::path::Path::new(&root))?;
            let f = std::fs::File::open(&file)?;
            let meta = k.cas.put_stream(f, &ty, Label::public_trusted(), "cli")?;
            println!("{}", serde_json::to_string_pretty(&meta)?);
        }
        "meta" => {
            let root = need(args, 2, "root")?;
            let id = need(args, 3, "artifact-id")?;
            let k = Kernel::open(std::path::Path::new(&root))?;
            println!("{}", serde_json::to_string_pretty(&k.cas.meta(&id)?)?);
        }
        "get" => {
            let root = need(args, 2, "root")?;
            let id = need(args, 3, "artifact-id")?;
            let out = need(args, 4, "out-file")?;
            let k = Kernel::open(std::path::Path::new(&root))?;
            let mut f = k.cas.open_read(&id)?;
            let mut o = std::fs::File::create(&out)?;
            std::io::copy(&mut f, &mut o)?;
            println!("wrote {out}");
        }
        "audit-verify" => {
            let root = need(args, 2, "root")?;
            let path = std::path::Path::new(&root).join("audit.log");
            let entries = portos_kernel::audit::AuditLog::verify(&path)?;
            println!("audit chain OK: {} entries", entries.len());
        }
        "consent" => {
            let root = need(args, 2, "root")?;
            let plan_path = need(args, 3, "plan.json")?;
            let auto_yes = args.iter().any(|a| a == "--yes");
            let root_path = std::path::PathBuf::from(&root);
            let k = Arc::new(Kernel::open(&root_path)?);
            let host = Host::new(k.clone(), &root_path.join("sock"))?;
            // Admission runs against the real route tables (verbs' declared
            // characters), so the driver stack comes up — plugins only, no
            // browser window (Chromium opens lazily on first verb).
            chat::ensure_templates(&root_path)?;
            if let Some(cfg) = chat::load_chat_json(&root_path) {
                chat::spawn_chat_plugins(&host, &root_path, &cfg)?;
            }
            let bytes = std::fs::read(&plan_path)?;
            let out = host.plans.submit("user", &bytes)?;
            print!("{}", out.rendering);
            let signer = portos_signer::Signer::load(&root_path)?;
            let approved = auto_yes || {
                print!("Approve this plan? [y/N] ");
                std::io::stdout().flush()?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                matches!(line.trim(), "y" | "Y" | "yes")
            };
            if !approved {
                println!("Denied");
                host.shutdown_all();
                return Ok(());
            }
            let rec = signer.sign(&out.plan_hash, out.budget.clone(), 3600);
            let out_path = format!("{plan_path}.consent.json");
            std::fs::write(&out_path, serde_json::to_string_pretty(&rec)?)?;
            k.audit.lock().unwrap().append(json!({
                "event": "consent.signed", "plan": out.plan_hash, "nonce": rec.nonce,
            }))?;
            println!("consent → {out_path}");
            println!("run it with: portos run-plan {root} {plan_path} {out_path}");
            host.shutdown_all();
        }
        "run-plan" => {
            let root = need(args, 2, "root")?;
            let plan_path = need(args, 3, "plan.json")?;
            let consent_path = need(args, 4, "consent.json")?;
            let root_path = std::path::PathBuf::from(&root);
            let k = Arc::new(Kernel::open(&root_path)?);
            let host = Host::new(k.clone(), &root_path.join("sock"))?;
            host.start_sweeper(std::time::Duration::from_secs(1));
            chat::ensure_templates(&root_path)?;
            if let Some(cfg) = chat::load_chat_json(&root_path) {
                chat::spawn_chat_plugins(&host, &root_path, &cfg)?;
            }
            let bytes = std::fs::read(&plan_path)?;
            let rec: ConsentRecord = serde_json::from_str(&std::fs::read_to_string(&consent_path)?)?;
            let signer = portos_signer::Signer::load(&root_path)?;
            let plan_hash = portos_proto::artifact::id_for_bytes(&bytes);
            // Prefer the run `consent` already admitted for this plan.
            let run_id = match host.plans.admitted_run_for(&plan_hash)? {
                Some(id) => id,
                None => host.plans.submit("user", &bytes)?.run_id,
            };
            println!("[run] admitted {run_id} ({plan_hash})");
            let (_sub, rx) = host.subscribe_local(&format!("plan::run::{run_id}"));
            host.plans.start(&run_id, &rec)?;
            let stdin = std::io::stdin();
            loop {
                let ev = match rx.recv() {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let kind = ev["data"]["kind"].as_str().unwrap_or("").to_string();
                println!("[run] {}", serde_json::to_string(&ev["data"]).unwrap_or_default());
                match kind.as_str() {
                    "finished" => break,
                    "awaiting_approval" => {
                        let batch = host.plans.withheld_batch(&run_id).unwrap_or_default();
                        for (verb, target, cost) in &batch {
                            println!("[run] withheld: {verb} @ {target} (cost {cost})");
                        }
                        print!("Approve and release this batch? [y/N] ");
                        std::io::stdout().flush()?;
                        let mut line = String::new();
                        stdin.read_line(&mut line)?;
                        if matches!(line.trim(), "y" | "Y" | "yes") {
                            let mut budget = std::collections::BTreeMap::new();
                            for (verb, _, cost) in &batch {
                                *budget.entry(verb.clone()).or_insert(0u64) += cost;
                            }
                            let approval = signer.sign(&plan_hash, budget, 3600);
                            k.audit.lock().unwrap().append(json!({
                                "event": "consent.signed", "plan": plan_hash, "nonce": approval.nonce,
                            }))?;
                            if let Err(e) = host.plans.approve(&run_id, &approval) {
                                println!("[run] approve failed: {e}");
                                break;
                            }
                            // keep watching for finished
                        } else {
                            println!("[run] left awaiting approval; it will be aborted at consent expiry");
                            break;
                        }
                    }
                    "paused" => {
                        print!("Plan paused (escalate). Grant another batch with the same budget? [y/N] ");
                        std::io::stdout().flush()?;
                        let mut line = String::new();
                        stdin.read_line(&mut line)?;
                        if matches!(line.trim(), "y" | "Y" | "yes") {
                            let inc = signer.sign(&plan_hash, rec.budget.clone(), 3600);
                            host.plans.resume(&run_id, &inc)?;
                        } else {
                            println!("[run] left paused; it will be aborted at consent expiry");
                            break;
                        }
                    }
                    _ => {}
                }
            }
            host.shutdown_all();
        }
        "approve" => {
            let root = need(args, 2, "root")?;
            let run_id = need(args, 3, "run_id")?;
            let root_path = std::path::PathBuf::from(&root);
            let k = Arc::new(Kernel::open(&root_path)?);
            let host = Host::new(k.clone(), &root_path.join("sock"))?;
            chat::ensure_templates(&root_path)?;
            if let Some(cfg) = chat::load_chat_json(&root_path) {
                chat::spawn_chat_plugins(&host, &root_path, &cfg)?;
            }
            let plan_hash = host.plans.run_plan_hash(&run_id)?;
            let batch = host.plans.withheld_batch(&run_id)?;
            let signer = portos_signer::Signer::load(&root_path)?;
            if batch.is_empty() {
                println!("nothing to approve for {run_id}");
                return Ok(());
            }
            let mut budget = std::collections::BTreeMap::new();
            for (verb, _, cost) in &batch {
                *budget.entry(verb.clone()).or_insert(0u64) += cost;
            }
            print!("{}", render_budget(&plan_hash, &budget));
            println!("Withheld effects to release, in order:");
            for (verb, target, cost) in &batch {
                println!("  - {verb} @ {target} (cost {cost})");
            }
            print!("Approve and release this batch? [y/N] ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !matches!(line.trim(), "y" | "Y" | "yes") {
                println!("Denied");
                return Ok(());
            }
            let rec = signer.sign(&plan_hash, budget, 3600);
            host.plans.approve(&run_id, &rec)?;
            k.audit.lock().unwrap().append(json!({
                "event": "consent.signed", "plan": plan_hash, "nonce": rec.nonce,
            }))?;
            println!("[approve] batch released");
            host.shutdown_all();
        }
        "resume" => {
            // A paused run's continuation lives in the interpreter thread of
            // the process that started it — a fresh CLI cannot resume it.
            // (Opening the kernel aborts dead processes' runs as crashed.)
            let root = need(args, 2, "root")?;
            let run_id = need(args, 3, "run_id")?;
            let root_path = std::path::PathBuf::from(&root);
            let k = Arc::new(Kernel::open(&root_path)?);
            let host = Host::new(k, &root_path.join("sock"))?;
            let state = host.plans.run_state(&run_id)?;
            return Err(format!(
                "plan run {run_id} is {state}; escalate resume is in-process \
                 (answer the prompt in the run-plan session)"
            )
            .into());
        }
        "chat" => {
            let root = need(args, 2, "root")?;
            chat::run(&root)?;
        }
        _ => {
            println!("portos — AgentOS M0 CLI");
            println!("  init | put | meta | get | audit-verify | consent | run-plan | chat");
        }
    }
    Ok(())
}

fn need(args: &[String], i: usize, what: &str) -> Result<String, String> {
    args.get(i).cloned().ok_or_else(|| format!("missing arg: {what}"))
}
