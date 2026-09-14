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
//!
//! What the plugin declares is held to the driver documents this root has
//! (`portos driver`): every driver it answers must be registered, and its
//! description of each verb must be the document's. A plugin that departs
//! gets no manifest, and the manifest of one that does not names the
//! documents it was held to, by content id.

use portos_abi::Label;
use portos_abi::driver::Driver;
use portos_kernel::Kernel;
use portos_kernel::host::Host;
use portos_kernel_api::{LaunchSpec, MANIFEST_TYPE, Manifest, driver_ref};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
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

    let tools = hello.tools.clone().unwrap_or_default();
    let drivers: BTreeSet<&str> = hello.verbs.iter().map(|v| v.driver()).collect();
    let mut conforms = BTreeMap::new();
    for driver in drivers {
        let id = kernel.cas.get_ref(&driver_ref(driver))?.ok_or_else(|| {
            format!(
                "no driver document for `{driver}` in this root; register one with \
                     `portos driver {} <driver.json>`",
                root.display()
            )
        })?;
        let mut text = String::new();
        kernel.cas.open_read(&id)?.read_to_string(&mut text)?;
        let document = Driver::parse(&text)?;
        let problems = document.conformance(&hello.verbs, &tools);
        if !problems.is_empty() {
            return Err(format!(
                "{} does not conform to driver {driver} ({id}):\n  {}",
                hello.name,
                problems.join("\n  ")
            )
            .into());
        }
        conforms.insert(driver.to_string(), id);
    }
    let manifest = Manifest::of(&spec, &hello, conforms);
    let meta = kernel.cas.put_bytes(
        &serde_json::to_vec_pretty(&manifest)?,
        MANIFEST_TYPE,
        Label::public_trusted(),
        "cli",
    )?;
    println!("{}", serde_json::to_string_pretty(&meta)?);
    Ok(())
}
