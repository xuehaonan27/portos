//! Backend registry: config picks the wire protocol. The driver itself is
//! provider-neutral — a provider is one entry here, never the driver.
//!
//! An entry names a **protocol**, not a vendor: `anthropic-compatible`
//! speaks the Messages API to whatever endpoint the config points at, and a
//! future `openai-compatible` would do the same for that wire format. Which
//! company answers is a `base_url`, not a code path.

use crate::core::Backend;
use serde_json::Value;

pub fn make_backend(cfg: &Value) -> Result<std::sync::Arc<dyn Backend + Send + Sync>, String> {
    match cfg["backend"].as_str().unwrap_or("anthropic-compatible") {
        // `anthropic` is the name this had while it was still thought of as a
        // vendor binding; kept working, since a config file should not break
        // over a rename.
        "anthropic-compatible" | "anthropic" => Ok(std::sync::Arc::new(
            crate::backends::anthropic::AnthropicCompatible::from_config(cfg)?,
        )),
        other => Err(format!("unknown model backend: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// The old vendor-shaped name still selects the backend: a rename in the
    /// code is not a reason for someone's config file to stop working.
    #[test]
    fn the_previous_backend_name_is_still_accepted() {
        let cfg = json!({"backend": "anthropic", "base_url": "https://x.test", "model": "m"});
        assert!(super::make_backend(&cfg).is_ok());
        let canonical =
            json!({"backend": "anthropic-compatible", "base_url": "https://x.test", "model": "m"});
        assert!(super::make_backend(&canonical).is_ok());
        assert!(super::make_backend(&json!({"backend": "nope"})).is_err());
    }
}
