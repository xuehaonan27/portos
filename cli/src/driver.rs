//! `portos driver <root> <driver.json>`: a driver document, as data in this
//! root.
//!
//! Stored under its content id and named by its driver (`driver:<name>`),
//! so that `portos plugin` can hold a plugin's declaration to it. Registering
//! another document under the same name moves the name; a manifest made
//! against the old one still names it by id, which is the point of the id.

use portos_abi::Label;
use portos_abi::driver::Driver;
use portos_kernel::Kernel;
use portos_kernel_api::{DRIVER_TYPE, driver_ref};
use std::path::Path;

pub fn driver(root: &str, file: &str) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(file)?;
    let doc = Driver::parse(&text)?;
    let kernel = Kernel::open(Path::new(root))?;
    let meta =
        kernel
            .cas
            .put_bytes(text.as_bytes(), DRIVER_TYPE, Label::public_trusted(), "cli")?;
    kernel.cas.set_ref(&driver_ref(&doc.driver), &meta.id)?;
    println!("{}", serde_json::to_string_pretty(&meta)?);
    Ok(())
}
