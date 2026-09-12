//! The `shell::*` driver interface: run a command.
//!
//! One verb. **The command is a whole string handed to `sh -c`**, on
//! purpose: pipes are context discipline — `cargo test 2>&1 | tail -40` has
//! the shell shrink the output before it ever reaches a driver. It is
//! arbitrary execution, which is the same thing `kernel::spawn` already
//! grants. Output is a data-plane problem in both directions: each stream
//! answers as a [`Bulk`], and a stored handle can come back *in* through
//! `artifacts` as a read-only file. `plugins/shell` is the first
//! implementation.

use portos_abi::bulk::Bulk;
use portos_abi::ids::Verb;
use portos_abi::wire::{Payload, ToolMeta};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

pub static RUN: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("shell::run").expect("constant verb"));

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunArgs {
    /// Passed to `sh -c`.
    pub cmd: String,
    /// Relative to the driver's default working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Artifacts to make visible to the command, as `NAME → handle`. Each
    /// becomes an environment variable holding the path of a read-only file.
    ///
    /// This is how a stored result stops being a dead end. Without it the
    /// only way to use a 700KB log was to read it back into the
    /// conversation, which is exactly what putting it in the store was meant
    /// to avoid: `grep -c error "$LOG"` costs one line of context instead.
    /// The path never reaches the model — it asks by handle, the driver
    /// resolves it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunReply {
    /// Exit code, or `null` when a signal ended it (including the driver's own).
    pub status: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub stdout: Bulk,
    pub stderr: Bulk,
    pub timed_out: bool,
    /// Output was still arriving when the driver stopped waiting for it. A
    /// caller that is told "this is all of it" deserves to know when it is
    /// not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub output_truncated: bool,
    pub duration_ms: u64,
}

/// What the verb says about itself: the tool the model is shown.
pub fn tools() -> BTreeMap<Verb, ToolMeta> {
    BTreeMap::from([(
        RUN.clone(),
        ToolMeta {
            description: "Run a shell command and return its exit status and output. The \
             command is passed to `sh -c`, so pipes and redirection work — use \
             them: `… 2>&1 | tail -40` keeps a long log out of the conversation. \
             Output over ~16KB comes back as {handle, size, preview} — and you can \
             feed such a handle straight back in through `artifacts` instead of \
             reading it, which is almost always the cheaper move. The call blocks \
             until the command ends or times out."
                .to_string(),
            schema: Payload::of(&serde_json::json!({
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "description": "passed to sh -c"},
                    "artifacts": {
                        "type": "object",
                        "description": "handles to expose to the command as environment \
                                        variables holding file paths, e.g. {\"LOG\": \"blake3:…\"} \
                                        then `grep error \"$LOG\"`",
                        "additionalProperties": {"type": "string"},
                    },
                    "cwd": {"type": "string", "description": "relative to the driver's working directory"},
                    "timeout_ms": {"type": "integer", "description": "default 120000"},
                },
                "required": ["cmd"],
            }))
            .ok(),
        },
    )])
}
