//! portos-shell: run a command, and do not drown the model in its output.
//!
//! One verb, and after the runtime plumbing moved into the SDK, almost all of
//! what is left is the two decisions that are actually this driver's:
//!
//! **The command is a whole string handed to `sh -c`, deliberately.** Pipes
//! are context discipline: `cargo test 2>&1 | tail -40` has the shell shrink
//! the output before it ever reaches us, which is cheaper than paging
//! through an artifact afterwards. It also means this verb is arbitrary
//! execution, which is the same thing `kernel::spawn` already grants.
//!
//! **Output is a data-plane problem**, in both directions. A big log goes to
//! the store with a preview left behind; a stored handle can come back in
//! through `artifacts` as a read-only file, so `grep` works where the bytes
//! already are instead of dragging them through the conversation.
//!
//! What is *not* here any more, and was: draining two pipes without
//! deadlocking, timing out, escalating signals, and making sure nothing was
//! left behind. `portos_sdk::scope` owns those, because they are the same
//! four questions for every plugin that runs a child — and this driver got
//! the last one wrong twice while it owned them alone.
//!
//! Known shape, stated: the call blocks until the command ends. A turn
//! cancelled mid-command is only noticed once it returns, because
//! cancellation is checked between tool calls.
//!
//! Config (from the launch spec): `{"cwd": "/path"}`, the default working
//! directory — the process's own if unset.

use portos_abi::Label;
use portos_abi::wire::Payload;
use portos_sdk::bulk::{Bulk, Sink};
use portos_sdk::scope::Scope;
use portos_sdk::{CallError, CallResult, KernelClient, Plugin};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Provenance labels carry the command; a whole script would not be a label.
const LABEL_CMD_CHARS: usize = 80;

/// What this driver needs to know, which is one thing — and it is a default,
/// not a fence: `cd /` leaves it.
#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    cwd: Option<String>,
}

fn main() -> std::io::Result<()> {
    let cfg: Config = portos_sdk::config::config().map_err(std::io::Error::other)?;
    let cwd = match cfg.cwd {
        Some(p) => std::fs::canonicalize(p)?,
        None => std::env::current_dir()?,
    };
    eprintln!("[shell] cwd {}", cwd.display());
    let sink = Sink::default();

    portos_sdk::serve(
        Plugin::new("portos-shell").tool(
            "shell::run",
            "Run a shell command and return its exit status and output. The \
             command is passed to `sh -c`, so pipes and redirection work — use \
             them: `… 2>&1 | tail -40` keeps a long log out of the conversation. \
             Output over ~16KB comes back as {handle, size, preview} — and you can \
             feed such a handle straight back in through `artifacts` instead of \
             reading it, which is almost always the cheaper move. The call blocks \
             until the command ends or times out.",
            json!({
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
            }),
            move |args, client| run(&cwd, &sink, client, args.parse()?),
        ),
        |_topic, _data| {},
    )
}

#[derive(Deserialize)]
struct RunArgs {
    cmd: String,
    /// Relative to the driver's default working directory.
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Artifacts to make visible to the command, as `NAME → handle`. Each
    /// becomes an environment variable holding the path of a read-only file.
    ///
    /// This is how a stored result stops being a dead end. Without it the
    /// only way to use a 700KB log was to read it back into the
    /// conversation, which is exactly what putting it in the store was meant
    /// to avoid: `grep -c error "$LOG"` costs one line of context instead.
    /// The path never reaches the model — it asks by handle, the driver
    /// resolves it here.
    #[serde(default)]
    artifacts: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct RunReply {
    /// Exit code, or `null` when a signal ended it (including our own).
    status: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    stdout: Bulk,
    stderr: Bulk,
    timed_out: bool,
    /// Output was still arriving when we stopped waiting for it. Only
    /// reachable if something in the group survived a SIGKILL, but a caller
    /// that is told "this is all of it" deserves to know when it is not.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    output_truncated: bool,
    duration_ms: u64,
}

fn run(base: &PathBuf, sink: &Sink, client: &KernelClient, a: RunArgs) -> CallResult {
    let cwd = match &a.cwd {
        Some(rel) => base.join(rel),
        None => base.clone(),
    };

    // Resolved to paths, not copied: the store's objects are already files,
    // and moving bytes through a socket so a program can read them would
    // defeat the point of having stored them.
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&a.cmd).current_dir(&cwd);
    for (name, id) in &a.artifacts {
        let path = client
            .locate(id)
            .map_err(|e| CallError::from(format!("artifact {name}={id}: {e}")))?;
        cmd.env(name, path);
    }

    let scope = Scope::new("run");
    let done = scope
        .run(
            cmd,
            Duration::from_millis(a.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
        )
        .map_err(|e| CallError::from(format!("spawn: {e}")))?;

    let label = Label::with_integ(&format!("shell:{}", truncate(&a.cmd, LABEL_CMD_CHARS)));
    Ok(Payload::of(&RunReply {
        status: done.status,
        signal: done.signal,
        stdout: sink.deliver(client, &done.stdout, "text/plain", Some(label.clone()))?,
        stderr: sink.deliver(client, &done.stderr, "text/plain", Some(label))?,
        timed_out: done.timed_out,
        output_truncated: done.output_truncated,
        duration_ms: done.duration.as_millis() as u64,
    })?)
}

fn truncate(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}
