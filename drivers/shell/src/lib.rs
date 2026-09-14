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
use portos_abi::driver::Driver;
use portos_abi::ids::Verb;
use portos_abi::wire::ToolMeta;
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
/// The interface itself; the crate's constants and types are a typed view.
pub const DRIVER_JSON: &str = include_str!("../driver.json");
pub static DRIVER: LazyLock<Driver> =
    LazyLock::new(|| Driver::parse(DRIVER_JSON).expect("drivers/shell/driver.json is well-formed"));

/// What each verb says about itself, as an implementation advertises it.
pub fn tools() -> BTreeMap<Verb, ToolMeta> {
    DRIVER.tools()
}

#[cfg(test)]
mod driver_document {
    use super::*;

    /// The document is this interface: exactly the verbs named here, all of
    /// this driver, described.
    #[test]
    fn names_exactly_these_verbs() {
        let named: Vec<&Verb> = vec![&RUN];
        assert_eq!(DRIVER.driver, "shell");
        assert_eq!(tools().len(), named.len());
        for v in named {
            assert!(DRIVER.spec(v).is_some(), "{v} is in driver.json");
        }
    }
}
