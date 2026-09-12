//! Backend registry: config picks the provider implementation. The driver
//! itself is provider-neutral — a provider is one entry here, never the
//! driver (the model-family instance of the seam discipline in
//! decisions-v1.md D26/D27). Future entries: local models, OpenAI-compatible
//! endpoints, harness adapters.

use crate::core::Backend;
use serde_json::Value;

pub fn make_backend(cfg: &Value) -> Result<std::sync::Arc<dyn Backend + Send + Sync>, String> {
    match cfg["backend"].as_str().unwrap_or("anthropic") {
        "anthropic" => Ok(std::sync::Arc::new(
            crate::backends::anthropic::Anthropic::from_config(cfg),
        )),
        other => Err(format!("unknown model backend: {other}")),
    }
}
