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
//!
//! `consent` renders the kernel-computed canonical budget (never model
//! prose) and signs the quadruple on approval. `run-plan` executes under a
//! toy executor wired to portos-compute.

use portos_kernel::consent::{render_budget, Budget, ConsentRecord};
use portos_kernel::interp::{run_plan, EffectExec};
use portos_kernel::plancheck::{admit, VerbSchemas};
use portos_kernel::Kernel;
use portos_proto::{Label, Plan};
use serde_json::{json, Value};
use std::io::Write;


fn toy_schemas() -> VerbSchemas {
    let mut s = VerbSchemas::default();
    s.observe.insert("echo::list".into(), Label::with_integ("toy:echo"));
    s.observe.insert("secret::read".into(), Label::with_conf("secret:demo"));
    s.external_effects.insert("echo::emit".into(), false);
    s.external_effects.insert("external::send".into(), true);
    s
}

struct ToyExec {
    compute: portos_compute::Registry,
}

impl EffectExec for ToyExec {
    fn observe(&mut self, verb: &str, _args: &[Value]) -> Result<Value, String> {
        match verb {
            "echo::list" => Ok(json!(["alpha", "beta", "gamma"])),
            "secret::read" => Ok(json!("A")),
            other => Err(format!("toy observe: unknown {other}")),
        }
    }
    fn effect(&mut self, verb: &str, args: &[Value]) -> Result<(), String> {
        println!("[effect] {verb} {}", serde_json::to_string(args).unwrap_or_default());
        Ok(())
    }
    fn pure(&mut self, func: &str, args: &[Value]) -> Result<Value, String> {
        let mut fuel = portos_compute::FuelMeter::new(1_000_000);
        self.compute.run(func, None, args, &mut fuel)
    }
}

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
            let k = Kernel::open(std::path::Path::new(&root))?;
            let bytes = std::fs::read(&plan_path)?;
            // The plan bytes ARE the artifact; h_plan is its CAS id.
            let meta = k.cas.put_bytes(&bytes, "portos/plan", Label::public_trusted(), "cli")?;
            let plan = Plan::from_bytes(&bytes)?;
            let adm = admit(&plan, &toy_schemas()).map_err(|e| e.to_string())?;
            let budget: Budget = adm.budget.clone();
            print!("{}", render_budget(&meta.id, &budget));
            let approved = auto_yes || {
                print!("Approve this plan? [y/N] ");
                std::io::stdout().flush()?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                matches!(line.trim(), "y" | "Y" | "yes")
            };
            if !approved {
                println!("Denied");
                return Ok(());
            }
            let rec = ConsentRecord::sign(&k.consent_key, &meta.id, budget, 3600);
            let out = format!("{plan_path}.consent.json");
            std::fs::write(&out, serde_json::to_string_pretty(&rec)?)?;
            k.audit.lock().unwrap().append(json!({
                "event": "consent.signed", "plan": meta.id, "nonce": rec.nonce,
            }))?;
            println!("consent → {out}");
        }
        "run-plan" => {
            let root = need(args, 2, "root")?;
            let plan_path = need(args, 3, "plan.json")?;
            let consent_path = need(args, 4, "consent.json")?;
            let k = Kernel::open(std::path::Path::new(&root))?;
            let bytes = std::fs::read(&plan_path)?;
            let rec: ConsentRecord = serde_json::from_str(&std::fs::read_to_string(&consent_path)?)?;
            let mut exec = ToyExec { compute: portos_compute::Registry::builtin() };
            let mut audit = k.audit.lock().unwrap();
            let out = run_plan(&bytes, &rec, &k.consent_key, &toy_schemas(), &mut exec, &mut audit)?;
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        _ => {
            println!("portos — AgentOS M0 CLI");
            println!("  init | put | meta | get | audit-verify | consent | run-plan");
        }
    }
    Ok(())
}

fn need(args: &[String], i: usize, what: &str) -> Result<String, String> {
    args.get(i).cloned().ok_or_else(|| format!("missing arg: {what}"))
}
