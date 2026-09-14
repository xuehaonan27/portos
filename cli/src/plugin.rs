//! `portos plugin <root> <spec.json>`: a plugin, as data.
//!
//! Starts the spec once under a throwaway host, takes what it declared in
//! its hello, and stores that together with what runs as one manifest in
//! the CAS. What comes back is an id: list it in `portos.json` as
//! `{"plugin": "<id>", …}` and the deployment half — name, config, env,
//! grants — stays in the file, because it is not the plugin's.
//!
//! The spec is a launch spec: `artifact` or `bin`, optionally `bundle` and
//! `args`. A bare `bin` is looked for beside this binary first, the way
//! `portos run` does, so the standard plugins can be captured by name.

use portos_abi::Label;
use portos_kernel::Kernel;
use portos_kernel::host::Host;
use portos_kernel_api::{LaunchSpec, MANIFEST_TYPE, Manifest};
use std::sync::Arc;

pub fn plugin(root: &str, spec_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = std::fs::canonicalize(root)?;
    let mut spec: LaunchSpec = serde_json::from_str(&std::fs::read_to_string(spec_path)?)?;
    let exe = std::env::current_exe()?;
    if let Some(bin) = spec.bin.as_deref() {
        spec.bin = Some(crate::run::resolve_bin(&exe, bin));
    }
    let kernel = Arc::new(Kernel::open(&root)?);
    let host = Host::new(kernel.clone(), &root.join("sock"))?;
    let name = host.spawn_spec(&spec)?;
    let hello = host.hello(&name).ok_or("the plugin went away")?;
    host.shutdown(&name);
    let manifest = Manifest::of(&spec, &hello);
    let meta = kernel.cas.put_bytes(
        &serde_json::to_vec_pretty(&manifest)?,
        MANIFEST_TYPE,
        Label::public_trusted(),
        "cli",
    )?;
    println!("{}", serde_json::to_string_pretty(&meta)?);
    Ok(())
}
