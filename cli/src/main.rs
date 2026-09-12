//! CLI of portos. Currently links the kernel as a library, and daemonization
//! is deferred.
//!
//!   portos init <root>
//!   portos put <root> <file> [type]
//!   portos meta <root> <artifact-id>
//!   portos get <root> <artifact-id> <out-file>
//!   portos audit-verify <root>
//!   portos bundle <root> <base-dir> [paths…]
//!   portos sessions <root>
//!   portos run <root>

mod init;
mod run;
mod sessions;

use portos_abi::Label;
use portos_kernel::Kernel;

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
        "init" => init::init(&need(args, 2, "root")?)?,
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
        // `portos bundle <root> <base> [paths…]` — a plugin's files as one
        // artifact. Paths are kept relative to `base`, so a script that
        // imports `../../sdk/js/client.js` still finds it once unpacked:
        // bundle from the repository root and name both directories, rather
        // than rearranging the plugin to suit the packaging.
        "bundle" => {
            let root = need(args, 2, "root")?;
            let base = std::path::PathBuf::from(need(args, 3, "base-dir")?);
            let rel: Vec<String> = args[4.min(args.len())..].to_vec();
            let k = Kernel::open(std::path::Path::new(&root))?;

            // Built to a file rather than to memory: a plugin with its
            // dependencies is data, and data does not go through RAM just to
            // be handed to a store that streams.
            let tmp = std::path::Path::new(&root).join("bundle.tmp");
            {
                let mut tar = tar::Builder::new(std::fs::File::create(&tmp)?);
                tar.follow_symlinks(false);
                if rel.is_empty() {
                    tar.append_dir_all(".", &base)?;
                } else {
                    for r in &rel {
                        let full = base.join(r);
                        if full.is_dir() {
                            tar.append_dir_all(r, &full)?;
                        } else {
                            tar.append_path_with_name(&full, r)?;
                        }
                    }
                }
                tar.finish()?;
            }
            let meta = k.cas.put_stream(
                std::fs::File::open(&tmp)?,
                "application/x-tar",
                Label::public_trusted(),
                "cli",
            )?;
            std::fs::remove_file(&tmp)?;
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
        "sessions" => sessions::sessions(&need(args, 2, "root")?)?,
        "run" => run::run(&need(args, 2, "root")?)?,
        _ => {
            println!("portos — AgentOS M0 CLI");
            println!("  init | put | bundle | meta | get | audit-verify | sessions | run");
        }
    }
    Ok(())
}

fn need(args: &[String], i: usize, what: &str) -> Result<String, String> {
    args.get(i)
        .cloned()
        .ok_or_else(|| format!("missing arg: {what}"))
}
