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
//! The interface is `drivers/shell`; this is its first implementation.
//!
//! Config (from the launch spec): `{"cwd": "/path"}`, the default working
//! directory — the process's own if unset.

use portos_abi::Label;
use portos_abi::wire::Payload;
use portos_sdk::bulk::Sink;
use portos_sdk::scope::Scope;
use portos_sdk::{CallError, CallResult, KernelClient, Plugin};
use portos_shell_api::{self as shell, DEFAULT_TIMEOUT_MS, RunArgs, RunReply};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

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

    let tools = shell::tools();
    portos_sdk::serve(
        Plugin::new("portos-shell").implement(
            &shell::RUN,
            &tools[&shell::RUN],
            move |args, client| run(&cwd, &sink, client, args.parse()?),
        ),
        |_topic, _data| {},
    )
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
