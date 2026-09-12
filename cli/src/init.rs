//! `portos init <root>` — a fresh workstation.
//!
//! The one place that knows what the standard set is: the egress broker,
//! the model driver, and a terminal front end, each with the grants it needs
//! and the files it reads. All of it is *written down* — `<root>/portos.json`
//! and two config directories — so nothing about it is built into anything
//! that runs. Change the file and `portos run` runs something else; list a
//! different model driver, or two browsers, or a front end for a browser
//! instead of a terminal, and the launcher neither knows nor cares.
//!
//! Nothing that already exists is overwritten.

use portos_egress_api as egress;
use portos_kernel::Kernel;
use portos_model_api as model;
use serde_json::json;
use std::path::Path;

pub fn init(root: &str) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(root)?;
    let root = std::fs::canonicalize(root)?;
    Kernel::open(&root)?;
    println!("initialized kernel state at {}", root.display());

    let broker_dir = root.join(egress::DIR);
    let model_dir = root.join(model::DIR);
    write_if_absent(
        &root.join(crate::run::CONFIG),
        &json!({
            "audit_topics": [egress::LOG.as_str()],
            "plugins": [
                {
                    "bin": "portos-broker",
                    "env": {egress::DIR_ENV: broker_dir},
                    // The reason reload exists: the API key lands in
                    // secrets.json, and only the broker ever reads it.
                    "watch": [
                        format!("{}/config.json", egress::DIR),
                        format!("{}/secrets.json", egress::DIR),
                    ],
                },
                {
                    "bin": "portos-modeld",
                    "env": {model::DIR_ENV: model_dir},
                    "watch": [format!("{}/config.json", model::DIR)],
                    // Its LLM calls go through the broker; it holds no key
                    // and no network of its own.
                    "grants": [{
                        "resource": egress::HTTP.resource(),
                        "verbs": [egress::HTTP.short(), egress::HTTP_STREAM.short()],
                    }],
                },
                {
                    "bin": "portos-tty",
                    "grants": [{
                        "resource": model::START.resource(),
                        "verbs": [
                            model::START.short(), model::SEND.short(),
                            model::CANCEL.short(), model::END.short(),
                            model::SESSIONS.short(),
                        ],
                    }],
                },
            ],
        }),
        "the standard set: broker, model driver, terminal front end — edit to taste",
    )?;

    write_if_absent(
        &broker_dir.join("config.json"),
        &json!({
            "allow": [{
                "host": "api.anthropic.com",
                "inject": {"x-api-key": "anthropic_api_key"},
            }],
        }),
        "",
    )?;
    write_if_absent(
        &broker_dir.join("secrets.json"),
        &json!({"anthropic_api_key": ""}),
        "put your API key there (it stays in the broker; the model driver never sees it)",
    )?;
    write_if_absent(
        &model_dir.join("config.json"),
        &json!({
            // A protocol, not a vendor: point base_url at any endpoint
            // that speaks the Anthropic Messages API. Whichever one it
            // is, its host and auth header belong in broker/config.json.
            "backend": "anthropic-compatible",
            "base_url": "https://api.anthropic.com",
            "model": "claude-opus-5",
            "max_tokens": 64000,
            "system": "You are the PortOS assistant. Use the available tools when they help.",
            "tools": [],
        }),
        "base_url and model are required; change them and broker/config.json together to use another endpoint",
    )?;
    Ok(())
}

fn write_if_absent(path: &Path, v: &serde_json::Value, note: &str) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, serde_json::to_string_pretty(v).unwrap())?;
    if note.is_empty() {
        println!("[init] wrote {}", path.display());
    } else {
        println!("[init] wrote {} — {note}", path.display());
    }
    Ok(())
}
