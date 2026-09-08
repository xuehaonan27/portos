//! portos-compute: the zero-capability pure-computation plugin.
//!
//! Pure evaluation lives here, NOT in the kernel (methodology audit 2026-09-07:
//! the kernel used to link this crate and run `pure` in-process — the kernel
//! must not carry computation). This plugin is a reference implementation of
//! the compute family: it serves `compute::run` — pinned pure functions with
//! fuel, holding no capabilities of its own. LLM-registered functions (the
//! generator path) arrive as a later iteration of THIS plugin, not of the kernel.

use serde_json::{Value, json};

fn main() -> std::io::Result<()> {
    let registry = portos_compute::Registry::builtin();
    portos_sdk::serve_hello(
        "portos-compute",
        &["compute::run"],
        json!({
            "tools": {
                "compute::run": {
                    "description": "Run a pinned pure function over JSON args (fuel-capped, zero capabilities).",
                    "schema": {
                        "type": "object",
                        "required": ["func"],
                        "properties": {
                            "func": {"type": "string"},
                            "args": {"type": "array"},
                            "fuel": {"type": "integer"},
                        },
                    },
                    "kind": "repeatable",
                },
            },
        }),
        move |verb, args, _client| {
            if verb != "compute::run" {
                return Err(format!("unknown verb: {verb}"));
            }
            let func = args["func"].as_str().ok_or("compute::run: missing func")?;
            let fn_args: Vec<Value> = args["args"].as_array().cloned().unwrap_or_default();
            let fuel_cap = args["fuel"].as_u64().unwrap_or(100_000);
            let mut fuel = portos_compute::FuelMeter::new(fuel_cap);
            registry.run(func, None, &fn_args, &mut fuel)
        },
        |_, _| {},
    )
}
