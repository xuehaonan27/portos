//! CLI of portos. Currently links the kernel as a library, and daemonization
//! is deferred.
//!
//!   portos init <root>
//!   portos put <root> <file> [type]
//!   portos meta <root> <artifact-id>
//!   portos get <root> <artifact-id> <out-file>
//!   portos audit-verify <root>
//!   portos chat <root>

mod chat;

use portos_kernel::Kernel;
use portos_proto::Label;

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
            let ty = args
                .get(4)
                .cloned()
                .unwrap_or_else(|| "application/octet-stream".into());
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
        "chat" => {
            let root = need(args, 2, "root")?;
            chat::run(&root)?;
        }
        _ => {
            println!("portos — AgentOS M0 CLI");
            println!("  init | put | meta | get | audit-verify | chat");
        }
    }
    Ok(())
}

fn need(args: &[String], i: usize, what: &str) -> Result<String, String> {
    args.get(i)
        .cloned()
        .ok_or_else(|| format!("missing arg: {what}"))
}
