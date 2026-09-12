//! What the launcher said this plugin should be.
//!
//! Every plugin used to find this out for itself: read an environment
//! variable, or a path to a directory, or a JSON file in it, then index into
//! a `serde_json::Value` with string keys and `unwrap_or` a default. Five
//! plugins, five versions of the same twenty lines, and every one of them a
//! place where a typo becomes a silently-wrong default rather than an error.
//!
//! The launcher already knows the answer — it is in the `LaunchSpec` — so it
//! is handed down instead, and parsed once into whatever type the plugin
//! actually wants. What a plugin keeps is naming that type, which is the
//! part that was ever its business.

use crate::PluginError;
use serde::de::DeserializeOwned;

/// This plugin's configuration, as the type it wants.
///
/// Absent config parses as `{}`, so a shape whose fields all have defaults
/// needs no config at all — and one with a required field says which field
/// is missing rather than quietly running as something else.
pub fn config<T: DeserializeOwned>() -> Result<T, PluginError> {
    let text = std::env::var("PORTOS_PLUGIN_CONFIG").unwrap_or_else(|_| "{}".to_string());
    Ok(serde_json::from_str(&text)?)
}
