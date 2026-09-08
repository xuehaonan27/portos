//! Backend registry: config picks the provider implementation. The driver
//! itself is provider-neutral — a provider is one entry here, never the
//! driver (the model-family instance of the seam discipline in
//! decisions-v1.md D26/D27). The Anthropic implementation is its own crate
//! (`portos-model-anthropic`); future entries (local models,
//! OpenAI-compatible endpoints, harness adapters) are likewise crates,
//! swapped at this binary's manifest.

use portos_model_core::Backend;
use serde_json::Value;

pub fn make_backend(cfg: &Value) -> Result<Box<dyn Backend>, String> {
    match cfg["backend"].as_str().unwrap_or("anthropic") {
        "anthropic" => Ok(Box::new(portos_model_anthropic::Anthropic::from_config(cfg))),
        other => Err(format!("unknown model backend: {other}")),
    }
}
